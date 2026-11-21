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

    /// True when the root of the delegation chain presented no credential.
    /// Every such caller shares the one id `anonymous`, so nothing keyed by
    /// identity — a session, an `@principal` binding — may be handed to one.
    ///
    /// A bearer token without a `sub` counts as well: every such token is the
    /// one principal `jwt-subject-unset`, so keying a session or an
    /// `@principal` query by it would pool every subject-less caller's data,
    /// exactly as it would for anonymous ones.
    pub fn root_is_anonymous(&self) -> bool {
        self.is_anonymous()
            || self.claims.get("root_anonymous").and_then(|v| v.as_bool()).unwrap_or(false)
            || self.root_id() == JWT_SUBJECT_UNSET
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        // A wildcard is spelled exactly `*`; no prefix matching, because
        // `admin:*` style globs invite mistakes that are invisible in review.
        self.scopes.iter().any(|s| s == scope || s == "*")
    }

    /// The principal at the root of a delegation chain: the human or service
    /// that started things, however many agents deep this one is.
    pub fn root_id(&self) -> &str {
        self.claims
            .get("root")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.id)
    }

    /// When the bearer token this principal was authenticated with expires
    /// (its `exp`, in Unix seconds). `None` for an API key or an anonymous
    /// caller. Written by the runtime under a claim name whose value in the
    /// token itself is discarded, and carried through delegation, so a
    /// WebSocket opened with a token can stop honouring it once it lapses.
    pub fn token_expires_at(&self) -> Option<u64> {
        self.claims.get(TOKEN_EXPIRY_CLAIM).and_then(|v| v.as_u64())
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
        claims
            .entry("root".into())
            .or_insert_with(|| serde_json::Value::String(self.id.clone()));
        claims
            .entry("root_anonymous".into())
            .or_insert_with(|| serde_json::Value::Bool(self.is_anonymous()));
        Self {
            id: format!("agent:{agent_name}#{}", self.id),
            kind: PrincipalKind::Agent,
            scopes,
            claims,
        }
    }
}

/// The id of a bearer token that names no subject.
pub const JWT_SUBJECT_UNSET: &str = "jwt-subject-unset";

/// Where the runtime records a bearer token's expiry on its principal.
const TOKEN_EXPIRY_CLAIM: &str = "token_expires_at";

/// Claim names the runtime writes into a principal itself, to track delegation.
/// A token's own claims of these names are dropped at the edge: a signed
/// `"root": "alice"` claim otherwise became the caller's root identity, which
/// is what an `@principal` query binds, so it read and wrote alice's rows.
const RESERVED_CLAIMS: &[&str] = &["root", "root_anonymous", "agent", "on_behalf_of"];

/// Principal ids the runtime gives its own background work. A watcher runs as
/// `watcher:<name>`; a bearer token naming such a subject would share its
/// memory, sessions and rate-limit bucket.
const RESERVED_ID_PREFIXES: &[&str] = &["watcher:"];

/// The value `webcortex new` writes into `.env.example` for an API key. A key
/// that still holds it is publicly documented, so it is never registered.
const PLACEHOLDER_KEY_PREFIX: &str = "replace-me";

/// The JWT algorithms an app may configure: HMAC, RSA PKCS#1 v1.5, ECDSA on
/// P-256 and P-384, and Ed25519.
pub const JWT_ALGORITHMS: &[&str] =
    &["HS256", "HS384", "HS512", "RS256", "RS384", "RS512", "ES256", "ES384", "EdDSA"];

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
    /// Every API-key principal id the app declares, set or not. A bearer
    /// token may not claim one as its subject: identity-keyed state (memory
    /// rows, sessions, the rate-limit bucket) would otherwise be shared.
    key_ids: std::collections::HashSet<String>,
}

struct JwtDecoder {
    key: jsonwebtoken::DecodingKey,
    validation: jsonwebtoken::Validation,
}

impl Authenticator {
    pub fn build(cfg: &AuthConfig) -> Result<Self, String> {
        let mut api_keys = HashMap::new();
        let mut key_ids = std::collections::HashSet::new();
        for (env_var, spec) in &cfg.api_keys {
            key_ids.insert(spec.id.clone());
            match std::env::var(env_var) {
                // The starter's `.env.example` value. Copied to `.env` as the
                // file says, it booted as a working key — usually an admin one
                // — that anyone who has read the starter knows.
                Ok(secret) if secret.trim().starts_with(PLACEHOLDER_KEY_PREFIX) => {
                    tracing::warn!(
                        env = %env_var,
                        principal = %spec.id,
                        "api key env var still holds the starter placeholder; this principal cannot \
                         authenticate until it is set to a real key (webcortex keygen)"
                    );
                }
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
            key_ids,
        })
    }

    /// True when bearer tokens (`app.jwt(...)`) are configured.
    pub fn accepts_bearer_tokens(&self) -> bool {
        self.jwt.is_some()
    }

    /// True when the credential a principal was authenticated with has
    /// expired, allowing the configured leeway. Always false for credentials
    /// that do not expire (API keys, anonymous callers).
    pub fn expired(&self, principal: &Principal) -> bool {
        self.expires_in(principal).is_some_and(|left| left.is_zero())
    }

    /// How long until the principal's bearer token stops being honoured
    /// (`exp` plus the leeway); zero once it has. `None` when it never expires.
    pub fn expires_in(&self, principal: &Principal) -> Option<std::time::Duration> {
        let exp = principal.token_expires_at()?;
        let leeway = self.jwt.as_ref().map_or(0, |j| j.validation.leeway);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let left = exp.saturating_add(leeway) as f64 - now;
        Some(std::time::Duration::from_secs_f64(left.max(0.0)))
    }

    /// Establish identity from request headers.
    ///
    /// A malformed or expired credential is an error, not a silent downgrade to
    /// anonymous: a client that sent a token deserves to be told it was
    /// rejected rather than mysteriously receiving 403s later.
    /// The header an API key is read from (lower-case).
    pub fn api_key_header(&self) -> &str {
        &self.api_key_header
    }

    pub fn authenticate(
        &self,
        headers: &BTreeMap<String, String>,
    ) -> Result<Principal, AuthError> {
        if let Some(raw) = headers.get(&self.api_key_header) {
            return self.authenticate_api_key(raw.trim());
        }

        if let Some(auth) = headers.get("authorization") {
            if let Some(token) = bearer_token(auth) {
                return self.authenticate_jwt(token);
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

        let mut claims = c.extra;
        for name in RESERVED_CLAIMS {
            claims.remove(*name);
        }
        // `exp` is a required claim, so a token that got this far has a
        // validated one. Recorded where the token itself cannot write: a
        // claim of the same name in the token is replaced, never trusted.
        claims.remove(TOKEN_EXPIRY_CLAIM);
        let exp = claims.get("exp").and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)));
        if let Some(exp) = exp {
            claims.insert(TOKEN_EXPIRY_CLAIM.into(), serde_json::json!(exp));
        }
        // An empty `sub` names nobody, the same as a missing one.
        let id = c.sub.filter(|s| !s.is_empty()).unwrap_or_else(|| JWT_SUBJECT_UNSET.into());
        // API-key and bearer principals share one id space, and memory,
        // sessions and the rate limiter are keyed by it: a token whose
        // subject was an API key's id read and wrote that service's state.
        if self.key_ids.contains(&id) || RESERVED_ID_PREFIXES.iter().any(|p| id.starts_with(p)) {
            return Err(AuthError::Invalid(
                "token rejected: its subject is an identity reserved by this application \
                 (an API key's id or a watcher's); issue the token for another subject"
                    .into(),
            ));
        }
        Ok(Principal {
            id,
            kind: PrincipalKind::Jwt,
            scopes,
            claims,
        })
    }
}

/// The token of a `Bearer` credential. The scheme name is case-insensitive
/// (RFC 9110 §11.1), so `BEARER x` is a bearer token, not a malformed header,
/// and any run of spaces or tabs may separate it from the token.
fn bearer_token(value: &str) -> Option<&str> {
    let value = value.trim();
    let (scheme, token) = value.split_once([' ', '\t'])?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim()).filter(|t| !t.is_empty())
}

/// A token that looks like a compact JWS (`header.payload.signature`, each
/// part base64url), as opposed to an opaque API key. Used to decide where a
/// WebSocket's `?access_token=` belongs.
pub fn looks_like_jwt(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

fn build_jwt_decoder(cfg: &JwtConfig) -> Result<JwtDecoder, String> {
    let secret = std::env::var(&cfg.secret_env).map_err(|_| {
        format!(
            "JWT is configured but {} is not set; refusing to start with an unverifiable token path",
            cfg.secret_env
        )
    })?;

    let unsupported = || {
        format!(
            "unsupported JWT algorithm {:?}; use one of {}",
            cfg.algorithm,
            JWT_ALGORITHMS.join(", ")
        )
    };
    if !JWT_ALGORITHMS.contains(&cfg.algorithm.as_str()) {
        return Err(unsupported());
    }
    let algorithm: jsonwebtoken::Algorithm = cfg.algorithm.parse().map_err(|_| unsupported())?;

    use jsonwebtoken::Algorithm as A;
    let key = match algorithm {
        A::HS256 | A::HS384 | A::HS512 => {
            // Only a shared secret can be guessed; a public key is public.
            if secret.len() < 32 {
                return Err(format!(
                    "{} must be at least 32 bytes; short HMAC secrets are brute-forceable",
                    cfg.secret_env
                ));
            }
            jsonwebtoken::DecodingKey::from_secret(secret.as_bytes())
        }
        A::RS256 | A::RS384 | A::RS512 => jsonwebtoken::DecodingKey::from_rsa_pem(secret.as_bytes())
            .map_err(|e| format!("invalid RSA public key in {}: {e}", cfg.secret_env))?,
        A::ES256 | A::ES384 => jsonwebtoken::DecodingKey::from_ec_pem(secret.as_bytes())
            .map_err(|e| format!("invalid EC public key (PEM) in {}: {e}", cfg.secret_env))?,
        A::EdDSA => jsonwebtoken::DecodingKey::from_ed_pem(secret.as_bytes())
            .map_err(|e| format!("invalid Ed25519 public key (PEM) in {}: {e}", cfg.secret_env))?,
        _ => return Err(unsupported()),
    };

    let mut validation = jsonwebtoken::Validation::new(algorithm);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    // Set explicitly rather than inherited: the library default is 60s, which
    // is a longer grace window than most operators would knowingly choose.
    validation.leeway = cfg.leeway_secs;
    // `exp` is required by default; state it so a library default change cannot
    // silently start accepting non-expiring tokens. A configured audience or
    // issuer is required too: the library checks `aud` and `iss` only when a
    // token carries them, so a token minted for another service that simply
    // left them out was accepted.
    let mut required = vec!["exp"];
    if let Some(aud) = &cfg.audience {
        validation.set_audience(&[aud]);
        required.push("aud");
    }
    if let Some(iss) = &cfg.issuer {
        validation.set_issuer(&[iss]);
        required.push("iss");
    }
    validation.set_required_spec_claims(&required);

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

/// Generate an API key suitable for handing to a client. Used by `webcortex keygen`.
pub fn generate_api_key() -> String {
    let raw = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(raw.as_bytes());
    bytes.extend_from_slice(second.as_bytes());
    format!(
        "wcx_{}",
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
    fn an_anonymous_root_is_remembered_through_delegation() {
        let a = Principal::anonymous().delegate_to_agent("a", &[]);
        let b = a.delegate_to_agent("b", &[]);
        assert!(a.root_is_anonymous() && b.root_is_anonymous());
        assert!(!principal_with(&["read"]).delegate_to_agent("a", &[]).root_is_anonymous());
    }

    #[test]
    fn the_root_principal_survives_a_chain_of_delegation() {
        let human = principal_with(&["read"]);
        let a = human.delegate_to_agent("a", &[]);
        let b = a.delegate_to_agent("b", &[]);
        assert_eq!(human.root_id(), "u1");
        assert_eq!(a.root_id(), "u1");
        assert_eq!(b.root_id(), "u1");
        assert_ne!(b.id, "u1");
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("BEARER abc"), Some("abc"));
        assert_eq!(bearer_token("bEaReR \t abc "), Some("abc"));
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Bearer   "), None);
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearerabc"), None);
    }

    fn jwt_authenticator() -> Authenticator {
        // A unique variable name, read only by this authenticator.
        let var = format!("WC_TEST_JWT_{}", uuid::Uuid::new_v4().simple());
        unsafe { std::env::set_var(&var, "a-test-secret-that-is-long-enough-123456") };
        Authenticator::build(&AuthConfig {
            jwt: Some(JwtConfig {
                secret_env: var,
                algorithm: "HS256".into(),
                audience: None,
                issuer: None,
                leeway_secs: 0,
            }),
            ..Default::default()
        })
        .expect("authenticator")
    }

    fn sign(claims: serde_json::Value) -> String {
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"a-test-secret-that-is-long-enough-123456"),
        )
        .expect("sign")
    }

    fn exp() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 600
    }

    fn auth_with(a: &Authenticator, token: &str) -> Principal {
        let mut headers = BTreeMap::new();
        headers.insert("authorization".to_string(), format!("bearer {token}"));
        a.authenticate(&headers).expect("token accepted")
    }

    #[test]
    fn a_token_cannot_claim_a_root_identity() {
        let a = jwt_authenticator();
        let p = auth_with(&a, &sign(serde_json::json!({
            "sub": "mallory", "root": "alice", "root_anonymous": false,
            "agent": "x", "on_behalf_of": "alice", "team": "red", "exp": exp(),
        })));
        assert_eq!(p.root_id(), "mallory");
        assert_eq!(p.claims.get("team"), Some(&serde_json::json!("red")), "other claims are kept");
        for name in RESERVED_CLAIMS {
            assert!(!p.claims.contains_key(*name), "{name} survived");
        }
        let delegated = p.delegate_to_agent("helper", &[]);
        assert_eq!(delegated.root_id(), "mallory");
    }

    #[test]
    fn a_token_without_a_subject_has_no_identity_to_key_on() {
        let a = jwt_authenticator();
        for claims in [serde_json::json!({"exp": exp()}), serde_json::json!({"sub": "", "exp": exp()})] {
            let p = auth_with(&a, &sign(claims));
            assert_eq!(p.id, JWT_SUBJECT_UNSET);
            assert!(p.root_is_anonymous());
            assert!(p.delegate_to_agent("a", &[]).root_is_anonymous());
        }
        let named = auth_with(&a, &sign(serde_json::json!({"sub": "u1", "exp": exp()})));
        assert!(!named.root_is_anonymous());
    }

    /// An authenticator for `cfg`, with `secret` in a variable of its own.
    fn jwt_with(algorithm: &str, secret: &str, audience: Option<&str>, issuer: Option<&str>) -> Result<Authenticator, String> {
        let var = format!("WC_TEST_JWT_{}", uuid::Uuid::new_v4().simple());
        unsafe { std::env::set_var(&var, secret) };
        Authenticator::build(&AuthConfig {
            jwt: Some(JwtConfig {
                secret_env: var,
                algorithm: algorithm.into(),
                audience: audience.map(str::to_string),
                issuer: issuer.map(str::to_string),
                leeway_secs: 0,
            }),
            ..Default::default()
        })
    }

    fn bearer(a: &Authenticator, token: &str) -> Result<Principal, AuthError> {
        let mut headers = BTreeMap::new();
        headers.insert("authorization".to_string(), format!("Bearer {token}"));
        a.authenticate(&headers)
    }

    #[test]
    fn a_configured_audience_and_issuer_are_required_not_only_checked() {
        let a = jwt_with("HS256", "a-test-secret-that-is-long-enough-123456", Some("api"), Some("https://idp"))
            .expect("authenticator");
        let good = serde_json::json!({"sub": "u", "exp": exp(), "aud": "api", "iss": "https://idp"});
        assert!(bearer(&a, &sign(good.clone())).is_ok());
        for missing in ["aud", "iss"] {
            let mut claims = good.clone();
            claims.as_object_mut().unwrap().remove(missing);
            assert!(bearer(&a, &sign(claims)).is_err(), "a token without {missing} was accepted");
        }
        // Without an audience or issuer configured, neither claim is needed.
        let plain = jwt_authenticator();
        assert!(bearer(&plain, &sign(serde_json::json!({"sub": "u", "exp": exp()}))).is_ok());
    }

    #[test]
    fn a_token_may_not_claim_an_api_keys_id_or_a_watchers() {
        let var = format!("WC_TEST_JWT_{}", uuid::Uuid::new_v4().simple());
        unsafe { std::env::set_var(&var, "a-test-secret-that-is-long-enough-123456") };
        let mut api_keys = BTreeMap::new();
        // Declared but unset: the id is still the service's.
        api_keys.insert("WC_TEST_UNSET_KEY_FOR_IDS".to_string(), crate::manifest::ApiKeySpec { id: "ops".into(), scopes: vec![] });
        let a = Authenticator::build(&AuthConfig {
            api_keys,
            jwt: Some(JwtConfig { secret_env: var, algorithm: "HS256".into(), audience: None, issuer: None, leeway_secs: 0 }),
            ..Default::default()
        })
        .unwrap();
        for sub in ["ops", "watcher:door"] {
            let err = bearer(&a, &sign(serde_json::json!({"sub": sub, "exp": exp()}))).unwrap_err();
            assert_eq!(err.status(), 401);
            assert!(err.message().contains("reserved"), "{}", err.message());
        }
        assert!(bearer(&a, &sign(serde_json::json!({"sub": "opsy", "exp": exp()}))).is_ok());
    }

    #[test]
    fn the_starter_placeholder_is_never_a_working_key() {
        let var = format!("WC_TEST_KEY_{}", uuid::Uuid::new_v4().simple());
        unsafe { std::env::set_var(&var, "replace-me-run-webcortex-keygen") };
        let mut api_keys = BTreeMap::new();
        api_keys.insert(var, crate::manifest::ApiKeySpec { id: "admin".into(), scopes: vec!["webcortex:admin".into()] });
        let a = Authenticator::build(&AuthConfig { api_keys, api_key_header: "x-api-key".into(), ..Default::default() })
            .unwrap();
        let mut headers = BTreeMap::new();
        headers.insert("x-api-key".to_string(), "replace-me-run-webcortex-keygen".to_string());
        assert!(a.authenticate(&headers).is_err());
    }

    #[test]
    fn a_tokens_expiry_is_recorded_where_the_token_cannot_write_it() {
        let a = jwt_authenticator();
        let soon = exp();
        let p = bearer(&a, &sign(serde_json::json!({"sub": "u", "exp": soon, "token_expires_at": soon + 99999}))).unwrap();
        assert_eq!(p.token_expires_at(), Some(soon));
        assert!(!a.expired(&p));
        assert!(a.expires_in(&p).unwrap() > std::time::Duration::from_secs(500));
        // Delegation carries it.
        assert_eq!(p.delegate_to_agent("x", &[]).token_expires_at(), Some(soon));
        // A principal past its expiry is reported as expired.
        let mut lapsed = p.clone();
        lapsed.claims.insert(TOKEN_EXPIRY_CLAIM.into(), serde_json::json!(1));
        assert!(a.expired(&lapsed));
        // Keys never expire.
        let key = Principal { id: "k".into(), kind: PrincipalKind::ApiKey, scopes: vec![], claims: BTreeMap::new() };
        assert!(!a.expired(&key) && a.expires_in(&key).is_none());
    }

    #[test]
    fn only_a_compact_jws_looks_like_a_jwt() {
        assert!(looks_like_jwt(&sign(serde_json::json!({"sub": "u", "exp": exp()}))));
        assert!(!looks_like_jwt(&generate_api_key()));
        assert!(!looks_like_jwt("a.b"));
        assert!(!looks_like_jwt("a..c"));
        assert!(!looks_like_jwt("a.b.c d"));
        assert!(!looks_like_jwt("a.b.c.d"));
    }

    const EC_PRIVATE: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg/7/uCESEjQMj06w2
dKMv5Sf1LvfBtI/pu/b1/aFYy5ehRANCAATyPRnTk088Qf6huQu0OY0jfaZLnQmp
GhCIDZgAGGw1bo2uWmUsKn87MZ/uEUeYm0LrP+i28Ui6VX25YpjXkg5M
-----END PRIVATE KEY-----
";
    const EC_PUBLIC: &str = "-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE8j0Z05NPPEH+obkLtDmNI32mS50J
qRoQiA2YABhsNW6NrlplLCp/OzGf7hFHmJtC6z/otvFIulV9uWKY15IOTA==
-----END PUBLIC KEY-----
";
    const ED_PRIVATE: &str = "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIF6+YQBVv7Bmk1W1ekG5Kp5MIFuQ8YlVq5Ms9eyu81Qj
-----END PRIVATE KEY-----
";
    const ED_PUBLIC: &str = "-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAECCoQLe8PqRFGNjFFN5jwxHmGmu3Nxanm+aC3ff3X9k=
-----END PUBLIC KEY-----
";

    #[test]
    fn ecdsa_and_eddsa_tokens_are_verified() {
        let claims = serde_json::json!({"sub": "u", "exp": exp(), "scope": "read"});
        for (alg, algorithm, private, public, key) in [
            ("ES256", jsonwebtoken::Algorithm::ES256, EC_PRIVATE, EC_PUBLIC,
             jsonwebtoken::EncodingKey::from_ec_pem(EC_PRIVATE.as_bytes()).unwrap()),
            ("EdDSA", jsonwebtoken::Algorithm::EdDSA, ED_PRIVATE, ED_PUBLIC,
             jsonwebtoken::EncodingKey::from_ed_pem(ED_PRIVATE.as_bytes()).unwrap()),
        ] {
            let _ = private;
            let a = jwt_with(alg, public, None, None).unwrap_or_else(|e| panic!("{alg} refused to build: {e}"));
            let token = jsonwebtoken::encode(&jsonwebtoken::Header::new(algorithm), &claims, &key).unwrap();
            let p = bearer(&a, &token).unwrap_or_else(|e| panic!("{alg}: {}", e.message()));
            assert_eq!(p.scopes, vec!["read"]);
            // A token signed for another algorithm is refused.
            assert!(bearer(&a, &sign(claims.clone())).is_err(), "{alg} accepted an HS256 token");
        }
        // A public key is not held to the HMAC secret length, and a key of the
        // wrong family is a boot error rather than a panic later.
        assert!(jwt_with("ES256", ED_PUBLIC, None, None).is_err());
        assert!(jwt_with("PS256", EC_PUBLIC, None, None).err().unwrap().contains("ES256"));
        assert!(jwt_with("HS256", "short", None, None).err().unwrap().contains("32 bytes"));
    }

    #[test]
    fn generated_keys_are_prefixed_and_unique() {

        let a = generate_api_key();
        let b = generate_api_key();
        assert!(a.starts_with("wcx_") && a.len() > 40);
        assert_ne!(a, b);
    }
}
