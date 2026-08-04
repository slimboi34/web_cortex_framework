//! Runtime-internal request/response types.
//!
//! Deliberately not `hyper::Request` — an op may be invoked by the HTTP server,
//! by an agent calling a tool in-process, or by a test, and none of those should
//! have to fabricate a socket to do it. This is the type every entry point
//! funnels into.

use bytes::Bytes;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct PylonRequest {
    pub method: String,
    pub path: String,
    pub path_params: BTreeMap<String, String>,
    pub query: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub body: Bytes,
    /// Set once the router has matched. `None` for synthetic in-process calls.
    pub route_id: Option<u32>,
    /// Established once at the edge and carried unchanged from there. An agent
    /// tool call arrives with a *delegated* principal whose scopes are a subset
    /// of the caller's — see [`crate::auth::Principal::delegate_to_agent`].
    pub principal: crate::auth::Principal,
    /// How many in-process invocations deep this request is.
    ///
    /// Zero for anything arriving over HTTP. Incremented every time a behaviour
    /// or agent calls a tool, so a cycle is bounded even though each nested
    /// invocation gets its own step budget.
    pub depth: u32,
}

impl PylonRequest {
    pub fn synthetic(method: &str, path: &str) -> Self {
        Self {
            method: method.to_string(),
            path: path.to_string(),
            path_params: BTreeMap::new(),
            query: BTreeMap::new(),
            headers: BTreeMap::new(),
            body: Bytes::new(),
            route_id: None,
            principal: crate::auth::Principal::anonymous(),
            depth: 0,
        }
    }

    /// A synthetic request carrying the given scopes. Test and tooling helper.
    pub fn synthetic_with_scopes(method: &str, path: &str, scopes: &[&str]) -> Self {
        let mut req = Self::synthetic(method, path);
        req.principal = crate::auth::Principal {
            id: "local".into(),
            kind: crate::auth::PrincipalKind::ApiKey,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            claims: Default::default(),
        };
        req
    }

    /// Look a name up across path params, then query string, then a top-level
    /// key of a JSON body. Agents supply arguments as one flat object and
    /// should not have to know which of the three a given route used.
    pub fn lookup(&self, name: &str) -> Option<serde_json::Value> {
        if let Some(v) = self.path_params.get(name) {
            return Some(coerce_scalar(v));
        }
        if let Some(v) = self.query.get(name) {
            return Some(coerce_scalar(v));
        }
        if let Ok(serde_json::Value::Object(map)) = self.json_body() {
            if let Some(v) = map.get(name) {
                return Some(v.clone());
            }
        }
        None
    }

    pub fn json_body(&self) -> Result<serde_json::Value, serde_json::Error> {
        if self.body.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&self.body)
    }
}

/// Path and query values arrive as strings but are semantically typed. Coercing
/// here means a `Query` op can bind `/users/42` to an integer column without the
/// declaration having to restate the type.
fn coerce_scalar(s: &str) -> serde_json::Value {
    if let Ok(i) = s.parse::<i64>() {
        return serde_json::Value::from(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return serde_json::Value::from(f);
    }
    match s {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        _ => serde_json::Value::String(s.to_string()),
    }
}

#[derive(Debug, Clone)]
pub struct PylonResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl PylonResponse {
    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|e| {
            serde_json::to_vec(&serde_json::json!({"error": e.to_string()})).unwrap()
        });
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: Bytes::from(body),
        }
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        let message = message.into();
        Self::json(
            status,
            &serde_json::json!({ "error": { "status": status, "message": message } }),
        )
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
            body: Bytes::from(body.into()),
        }
    }

    /// Interpret the body as JSON so an in-process caller (an agent invoking a
    /// tool) gets structured data instead of bytes it has to re-parse.
    pub fn json_value(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(&self.body).into_owned())
        })
    }
}

pub fn parse_query(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in raw.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.insert(percent_decode(k), percent_decode(v));
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
