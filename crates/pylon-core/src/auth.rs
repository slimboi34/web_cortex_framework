//! Identity and authorization.
//!
//! Two rules govern everything in this module, and they are what make the agent
//! story safe rather than merely convenient:
//!
//! 1. **A principal is established once, at the edge.** Every downstream
//!    consumer — an HTTP route, an MCP tool call, an agent step — reads the same
//!    [`Principal`]. There is no second authentication path for agents to slip
//!    through.
//!
//! 2. **An agent can never hold more authority than the caller who started it.**
//!    Agent scopes are *intersected* with the caller's, never unioned. This is
//!    the confused-deputy defence, and it is enforced in code rather than left
//!    to the application author to remember.

use crate::manifest::{AuthConfig, JwtConfig};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use subtle::ConstantTimeEq;

/// Who is making a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    /// Stable identifier. `anonymous` when no credential was presented.
    pub id: String,
    /// How the identity was established, for the audit log.
    pub kind: PrincipalKind,
    pub scopes: Vec<String>,
    /// Additional claims, surfaced to handlers and templates.
    #[serde(default)]
    pub claims: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    Anonymous,
    ApiKey,
    Jwt,
    /// An agent acting on behalf of the principal that started its run.
    Agent,
}

impl Principal {
    pub fn anonymous() -> Self {
        Self {
            id: "anonymous".into(),
            kind: PrincipalKind::Anonymous,
            scopes: Vec::new(),
            claims: BTreeMap::new(),
        }
    }

    pub fn is_anonymous(&self) -> bool {
        self.kind == PrincipalKind::Anonymous
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        // A wildcard is spelled exactly `*`; no prefix matching, because
        // `admin:*` style globs invite mistakes that are invisible in review.
        self.scopes.iter().any(|s| s == scope || s == "*")
    }

    pub fn missing_scopes(&self, required: &[String]) -> Vec<String> {
        required
            .iter()
            .filter(|s| !self.has_scope(s))
            .cloned()
            .collect()
    }

    /// Derive the principal an agent run executes as.
    ///
    /// The result can only ever be a *subset* of this principal's scopes. If an
    /// agent declares a scope its caller does not hold, that scope is dropped
    /// rather than granted — an agent is a delegate, never an escalation.
    pub fn delegate_to_agent(&self, agent_name: &str, agent_scopes: &[String]) -> Self {
        let scopes = if agent_scopes.is_empty() {
            self.scopes.clone()
        } else {
            agent_scopes
                .iter()
                .filter(|s| self.has_scope(s))
                .cloned()
                .collect()
        };
        let mut claims = self.claims.clone();
        claims.insert("agent".into(), serde_json::Value::String(agent_name.into()));
        claims.insert(
            "on_behalf_of".into(),
            serde_json::Value::String(self.id.clone()),
        );
        Self {
            id: format!("agent:{agent_name}#{}", self.id),
            kind: PrincipalKind::Agent,
            scopes,
            claims,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Claims {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

/// Resolves credentials into principals. Built once at boot.
pub struct Authenticator {
    /// SHA-256 of each API key, mapped to (principal id, scopes). Hashed so a
    /// heap dump or core file does not hand over live credentials.
    api_keys: HashMap<[u8; 32], (String, Vec<String>)>,
    api_key_header: String,
    jwt: Option<JwtDecoder>,
    anonymous_scopes: Vec<String>,
}

struct JwtDecoder {
    key: jsonwebtoken::DecodingKey,
    validation: jsonwebtoken::Validation,
}

impl Authenticator {
    pub fn build(cfg: &AuthConfig) -> Result<Self, String> {
        let mut api_keys = HashMap::new();
        for (env_var, spec) in &cfg.api_keys {
            match std::env::var(env_var) {
                Ok(secret) if !secret.is_empty() => {
                    api_keys.insert(sha256(secret.as_bytes()), (spec.id.clone(), spec.scopes.clone()));
                }
                _ => {
                    // Loud, because a silently-missing key means an endpoint the
                    // operator believes is reachable is not.
                    tracing::warn!(
                        env = %env_var,
                        principal = %spec.id,
                        "api key env var is unset or empty; this principal cannot authenticate"
                    );
                }
            }
        }

        let jwt = match &cfg.jwt {
            Some(j) => Some(build_jwt_decoder(j)?),
            None => None,
        };

        Ok(Self {
            api_keys,
            api_key_header: cfg.api_key_header.to_ascii_lowercase(),
            jwt,
            anonymous_scopes: cfg.anonymous_scopes.clone(),
        })
    }

    /// Establish identity from request headers.
    ///
    /// A malformed or expired credential is an error, not a silent downgrade to
    /// anonymous: a client that sent a token deserves to be told it was
    /// rejected rather than mysteriously receiving 403s later.
    pub fn authenticate(
        &self,
        headers: &BTreeMap<String, String>,
    ) -> Result<Principal, AuthError> {
        if let Some(raw) = headers.get(&self.api_key_header) {
            return self.authenticate_api_key(raw.trim());
        }

        if let Some(auth) = headers.get("authorization") {
            let value = auth.trim();
            if let Some(token) = value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
            {
                return self.authenticate_jwt(token.trim());
            }
            return Err(AuthError::Malformed(
                "Authorization header must use the Bearer scheme".into(),
            ));
        }

        Ok(Principal {
            scopes: self.anonymous_scopes.clone(),
            ..Principal::anonymous()
        })
    }

    fn authenticate_api_key(&self, presented: &str) -> Result<Principal, AuthError> {
        let digest = sha256(presented.as_bytes());
        // Constant-time compare against every configured key. Iterating the map
        // and comparing digests in constant time keeps lookup from leaking which
        // prefix matched via timing.
        let mut found: Option<&(String, Vec<String>)> = None;
        for (known, value) in &self.api_keys {
            if known.ct_eq(&digest).into() {
                found = Some(value);
            }
        }
        match found {
            Some((id, scopes)) => Ok(Principal {
                id: id.clone(),
                kind: PrincipalKind::ApiKey,
                scopes: scopes.clone(),
                claims: BTreeMap::new(),
            }),
            None => Err(AuthError::Invalid("unrecognised API key".into())),
        }
    }

    fn authenticate_jwt(&self, token: &str) -> Result<Principal, AuthError> {
        let decoder = self
            .jwt
            .as_ref()
            .ok_or_else(|| AuthError::Invalid("bearer tokens are not accepted by this app".into()))?;

        let data = jsonwebtoken::decode::<Claims>(token, &decoder.key, &decoder.validation)
            .map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => {
                    AuthError::Expired("token has expired".into())
                }
                _ => AuthError::Invalid(format!("token rejected: {e}")),
            })?;

        let c = data.claims;
        // Accept both conventions: RFC 8693 space-delimited `scope`, and the
        // array-valued `scopes` many issuers emit.
        let mut scopes: Vec<String> = c
            .scope
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();
        if let Some(list) = c.scopes {
            scopes.extend(list);
        }
        scopes.sort();
        scopes.dedup();

        Ok(Principal {
            id: c.sub.unwrap_or_else(|| "jwt-subject-unset".into()),
            kind: PrincipalKind::Jwt,
            scopes,
            claims: c.extra,
        })
    }
}

fn build_jwt_decoder(cfg: &JwtConfig) -> Result<JwtDecoder, String> {
    let secret = std::env::var(&cfg.secret_env).map_err(|_| {
        format!(
            "JWT is configured but {} is not set; refusing to start with an unverifiable token path",
            cfg.secret_env
        )
    })?;
    if secret.len() < 32 {
        return Err(format!(
            "{} must be at least 32 bytes; short HMAC secrets are brute-forceable",
            cfg.secret_env
        ));
    }

    let algorithm: jsonwebtoken::Algorithm = cfg
        .algorithm
        .parse()
        .map_err(|_| format!("unsupported JWT algorithm {:?}", cfg.algorithm))?;

    let key = match algorithm {
        jsonwebtoken::Algorithm::HS256
        | jsonwebtoken::Algorithm::HS384
        | jsonwebtoken::Algorithm::HS512 => jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        jsonwebtoken::Algorithm::RS256
        | jsonwebtoken::Algorithm::RS384
        | jsonwebtoken::Algorithm::RS512 => jsonwebtoken::DecodingKey::from_rsa_pem(secret.as_bytes())
            .map_err(|e| format!("invalid RSA public key in {}: {e}", cfg.secret_env))?,
        other => return Err(format!("unsupported JWT algorithm {other:?}")),
    };

    let mut validation = jsonwebtoken::Validation::new(algorithm);
    validation.validate_exp = true;
    if let Some(aud) = &cfg.audience {
        validation.set_audience(&[aud]);
    }
    if let Some(iss) = &cfg.issuer {
        validation.set_issuer(&[iss]);
    }

    Ok(JwtDecoder { key, validation })
}

#[derive(Debug, Clone)]
pub enum AuthError {
    Invalid(String),
    Expired(String),
    Malformed(String),
}

impl AuthError {
    pub fn status(&self) -> u16 {
        401
    }
    pub fn message(&self) -> &str {
        match self {
            AuthError::Invalid(m) | AuthError::Expired(m) | AuthError::Malformed(m) => m,
        }
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// Generate an API key suitable for handing to a client. Used by `pylon keygen`.
pub fn generate_api_key() -> String {
    let raw = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(raw.as_bytes());
    bytes.extend_from_slice(second.as_bytes());
    format!(
        "pyl_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal_with(scopes: &[&str]) -> Principal {
        Principal {
            id: "u1".into(),
            kind: PrincipalKind::Jwt,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            claims: BTreeMap::new(),
        }
    }

    #[test]
    fn wildcard_grants_everything_but_prefixes_do_not() {
        assert!(principal_with(&["*"]).has_scope("anything"));
        assert!(!principal_with(&["admin:*"]).has_scope("admin:write"));
    }

    #[test]
    fn agent_scopes_are_intersected_never_unioned() {
        let caller = principal_with(&["read"]);
        let agent = caller.delegate_to_agent("a", &["read".into(), "write".into()]);
        assert_eq!(agent.scopes, vec!["read"], "agent must not gain 'write'");
        assert_eq!(agent.kind, PrincipalKind::Agent);
    }

    #[test]
    fn agent_without_declared_scopes_inherits_the_caller_exactly() {
        let caller = principal_with(&["read", "write"]);
        let agent = caller.delegate_to_agent("a", &[]);
        assert_eq!(agent.scopes, vec!["read", "write"]);
    }

    #[test]
    fn an_anonymous_caller_cannot_launch_a_privileged_agent() {
        let anon = Principal::anonymous();
        let agent = anon.delegate_to_agent("a", &["admin".into()]);
        assert!(agent.scopes.is_empty());
    }

    #[test]
    fn delegation_records_the_original_principal_for_audit() {
        let agent = principal_with(&["read"]).delegate_to_agent("librarian", &[]);
        assert_eq!(agent.claims["on_behalf_of"], serde_json::json!("u1"));
        assert_eq!(agent.claims["agent"], serde_json::json!("librarian"));
    }

    #[test]
    fn generated_keys_are_prefixed_and_unique() {
        let a = generate_api_key();
        let b = generate_api_key();
        assert!(a.starts_with("pyl_") && a.len() > 40);
        assert_ne!(a, b);
    }
}
