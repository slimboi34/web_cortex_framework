//! Runtime-internal request/response types.
//!
//! Deliberately not `hyper::Request` — an op may be invoked by the HTTP server,
//! by an agent calling a tool in-process, or by a test, and none of those should
//! have to fabricate a socket to do it. This is the type every entry point
//! funnels into.

use bytes::Bytes;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct WebCortexRequest {
    pub method: String,
    pub path: String,
    pub path_params: BTreeMap<String, String>,
    /// One value per key: the last, when a key repeats.
    pub query: BTreeMap<String, String>,
    /// Every query pair in order, repeats included (`?tag=a&tag=b`). Empty for
    /// synthetic requests that only set [`Self::query`].
    pub query_all: Vec<(String, String)>,
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
    /// The token ceiling shared by every model-backed op in this request tree.
    /// `None` at the edge; set once an agent, behaviour or flow starts one.
    pub budget: Option<std::sync::Arc<crate::agent::SharedBudget>>,
}

impl WebCortexRequest {
    pub fn synthetic(method: &str, path: &str) -> Self {
        Self {
            method: method.to_string(),
            path: path.to_string(),
            path_params: BTreeMap::new(),
            query: BTreeMap::new(),
            query_all: Vec::new(),
            headers: BTreeMap::new(),
            body: Bytes::new(),
            route_id: None,
            principal: crate::auth::Principal::anonymous(),
            depth: 0,
            budget: None,
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
        // The caller's identity is bindable by name, so a declared query can be
        // scoped to whoever is asking without a Python handler in the way. It
        // resolves to the *root* principal — the human or service behind any
        // chain of agent delegation — so memory an agent writes on someone's
        // behalf is that someone's memory.
        if name == "@principal" {
            return Some(serde_json::Value::String(self.principal.root_id().to_string()));
        }
        if let Some(v) = self.path_params.get(name) {

            return Some(coerce_scalar(v));
        }
        if let Some(v) = self.query.get(name) {
            return Some(coerce_scalar(v));
        }
        if let Ok(serde_json::Value::Object(map)) = self.json_body()
            && let Some(v) = map.get(name) {
                return Some(v.clone());
            }
        None
    }

    /// Resolve each of `names` as [`Self::lookup`] does, for a declared query.
    ///
    /// The body is parsed once, not once per name (a 30 MB body bound to five
    /// parameters was parsed five times). And a body a name has to be read
    /// from must be a JSON object: 2.5.3 treated a malformed, too deeply
    /// nested or non-object body as absent, so `POST /items` with `{bad json`
    /// bound every column to NULL and inserted an empty row. That is a 400
    /// now, with the reason as the `Err`.
    pub fn bind(&self, names: &[String]) -> Result<Vec<serde_json::Value>, String> {
        let mut body: Option<Result<serde_json::Map<String, serde_json::Value>, String>> = None;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            if name == "@principal" || self.path_params.contains_key(name) || self.query.contains_key(name) {
                out.push(self.lookup(name).unwrap_or(serde_json::Value::Null));
                continue;
            }
            if self.body.is_empty() {
                out.push(serde_json::Value::Null);
                continue;
            }
            let parsed = body.get_or_insert_with(|| match serde_json::from_slice(&self.body) {
                Ok(serde_json::Value::Object(map)) => Ok(map),
                Ok(_) => Err("the request body must be a JSON object naming the route's parameters".into()),
                Err(e) => Err(format!("the request body is not valid JSON: {e}")),
            });
            match parsed {
                Ok(map) => out.push(map.get(name).cloned().unwrap_or(serde_json::Value::Null)),
                Err(e) => return Err(e.clone()),
            }
        }
        Ok(out)
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
///
/// Only text that *is* a number in its plain spelling is converted; anything
/// else stays a string, which SQLite still compares numerically against a
/// numeric column. 2.5.3 handed every string Rust could parse to `f64`, so
/// `?q=nan`, `inf` or `1e999` bound as NULL (JSON has no NaN), the ZIP code
/// `02134` lost its zero, and `99999999999999999999` lost its last digits.
fn coerce_scalar(s: &str) -> serde_json::Value {
    if let Ok(i) = s.parse::<i64>()
        && plain_number(s, false)
    {
        return serde_json::Value::from(i);
    }
    if plain_number(s, true)
        && s.contains(['.', 'e', 'E'])
        && let Ok(f) = s.parse::<f64>()
        && f.is_finite()
    {
        return serde_json::Value::from(f);
    }
    match s {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        _ => serde_json::Value::String(s.to_string()),
    }
}

/// Whether `s` is spelled as JSON spells a number: an optional `-`, digits
/// with no superfluous leading zero, and (if `fraction`) a fraction and an
/// exponent. No `+`, no whitespace, no `inf`/`nan`, no `0x`.
fn plain_number(s: &str, fraction: bool) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    let (mantissa, exponent) = match digits.split_once(['e', 'E']) {
        Some((m, e)) if fraction => (m, Some(e)),
        Some(_) => return false,
        None => (digits, None),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) if fraction => (i, Some(f)),
        Some(_) => return false,
        None => (mantissa, None),
    };
    let all_digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    all_digits(int)
        && (int == "0" || !int.starts_with('0'))
        && frac.is_none_or(all_digits)
        && exponent.is_none_or(|e| all_digits(e.strip_prefix(['-', '+']).unwrap_or(e)))
}

#[derive(Debug, Clone)]
pub struct WebCortexResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl WebCortexResponse {
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

    /// An error body: `{"error": {"status", "message", "code", "hint"}}`.
    ///
    /// The code is the generic one for `status` ([`crate::errors::code_for_status`]);
    /// prefer [`Self::coded`] wherever a more specific code applies.
    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::coded(status, crate::errors::code_for_status(status), message)
    }

    /// An error body carrying a catalogued code and that code's hint.
    pub fn coded(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self::coded_with_hint(status, code, message, crate::errors::hint(code))
    }

    /// As [`Self::coded`], with a hint specific to this occurrence.
    pub fn coded_with_hint(status: u16, code: &str, message: impl Into<String>, hint: &str) -> Self {
        let message = message.into();
        let mut res = Self::json(
            status,
            &serde_json::json!({
                "error": { "status": status, "message": message, "code": code, "hint": hint }
            }),
        );
        res.headers.push(("x-webcortex-error".into(), code.to_string()));
        res
    }

    /// The `WCnnn` code of an error response, from the header [`Self::coded`]
    /// sets, or from a JSON body a Python handler wrote.
    pub fn error_code(&self) -> Option<String> {
        if let Some((_, v)) = self.headers.iter().find(|(k, _)| k == "x-webcortex-error") {
            return Some(v.clone());
        }
        if self.status < 400 || self.body.len() > 64 * 1024 {
            return None;
        }
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v.pointer("/error/code").and_then(|c| c.as_str()).map(str::to_string))
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

/// The query string as a map: a repeated key keeps its last value.
pub fn parse_query(raw: &str) -> BTreeMap<String, String> {
    parse_query_pairs(raw).into_iter().collect()
}

/// Every pair in the query string, in order, repeats included.
pub fn parse_query_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k, true), percent_decode(v, true))
        })
        .collect()
}

impl WebCortexRequest {
    /// Every value given for a query key, in order. Falls back to the single
    /// value in [`Self::query`] for a request built without the pairs.
    pub fn query_values(&self, name: &str) -> Vec<&str> {
        if self.query_all.is_empty() {
            return self.query.get(name).map(String::as_str).into_iter().collect();
        }
        self.query_all.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect()
    }

    /// The pairs to forward: all of them when known, else the map.
    pub fn query_pairs(&self) -> Vec<(&str, &str)> {
        if self.query_all.is_empty() {
            self.query.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
        } else {
            self.query_all.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
        }
    }
}

/// Decode `%XX` escapes, and `+` as a space when `plus_as_space` (query
/// strings). A `%` not followed by two hex digits is kept as it is.
///
/// Works on bytes throughout. Slicing the `&str` at `i + 3` panicked when a
/// multi-byte character followed a `%`, and path parameters can carry any text
/// a model put in a tool call's arguments.
pub(crate) fn percent_decode(s: &str, plus_as_space: bool) -> String {
    fn hex(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|d| d as u8)
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push((hi << 4) | lo);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' if plus_as_space => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_plainly_spelled_numbers_are_coerced() {
        assert_eq!(coerce_scalar("42"), json!(42));
        assert_eq!(coerce_scalar("-7"), json!(-7));
        assert_eq!(coerce_scalar("0"), json!(0));
        assert_eq!(coerce_scalar("1.5"), json!(1.5));
        assert_eq!(coerce_scalar("-0.25e2"), json!(-25.0));
        assert_eq!(coerce_scalar("true"), json!(true));
        for kept in ["nan", "NaN", "inf", "-Infinity", "1e999", "007", "02134", "+5", " 5", "5 ",
                     "0x10", "1_000", "99999999999999999999", "1.", ".5", "1e", "-", ""] {
            assert_eq!(coerce_scalar(kept), json!(kept), "{kept:?} must stay a string");
        }
    }

    fn with_body(body: &str) -> WebCortexRequest {
        let mut req = WebCortexRequest::synthetic("POST", "/x");
        req.body = Bytes::from(body.to_string());
        req
    }

    #[test]
    fn bind_reads_path_query_then_body() {
        let mut req = with_body(r#"{"a": 1, "b": "two"}"#);
        req.query.insert("b".into(), "q".into());
        req.path_params.insert("c".into(), "9".into());
        let names: Vec<String> = ["a", "b", "c", "missing"].map(String::from).to_vec();
        assert_eq!(req.bind(&names).unwrap(), vec![json!(1), json!("q"), json!(9), json!(null)]);
        assert_eq!(WebCortexRequest::synthetic("GET", "/").bind(&names).unwrap()[0], json!(null));
    }

    #[test]
    fn bind_refuses_a_body_it_cannot_read() {
        let names = vec!["a".to_string()];
        for bad in ["{bad json", "[1, 2]", "\"text\"", &format!("{{\"a\": {}1{}}}", "[".repeat(200), "]".repeat(200))] {
            assert!(with_body(bad).bind(&names).is_err(), "{bad:.40}");
        }
        // A body no name is read from is not inspected.
        let mut req = with_body("{bad json");
        req.query.insert("a".into(), "1".into());
        assert_eq!(req.bind(&names).unwrap(), vec![json!(1)]);
    }

    #[test]
    fn percent_decoding_handles_escapes_plus_and_junk() {
        assert_eq!(percent_decode("a%2Fb%2e", false), "a/b.");
        assert_eq!(percent_decode("a+b", true), "a b");
        assert_eq!(percent_decode("a+b", false), "a+b");
        assert_eq!(percent_decode("100%", false), "100%");
        assert_eq!(percent_decode("%zz%4", false), "%zz%4");
    }

    #[test]
    fn a_percent_before_a_multibyte_character_does_not_panic() {
        assert_eq!(percent_decode("%aé", false), "%aé");
        assert_eq!(percent_decode("x%é%2F", true), "x%é/");
    }

    #[test]
    fn repeated_query_keys_are_all_kept() {
        let pairs = parse_query_pairs("tag=a&x=1&tag=b+c&tag");
        assert_eq!(pairs.iter().filter(|(k, _)| k == "tag").count(), 3);
        // The map keeps the last value, as it always has.
        assert_eq!(parse_query("tag=a&tag=b")["tag"], "b");
        let mut req = WebCortexRequest::synthetic("GET", "/");
        req.query_all = pairs;
        req.query = req.query_all.iter().cloned().collect();
        assert_eq!(req.query_values("tag"), vec!["a", "b c", ""]);
        assert_eq!(req.query_values("nope"), Vec::<&str>::new());
        // A request built with only the map still answers.
        let mut bare = WebCortexRequest::synthetic("GET", "/");
        bare.query.insert("x".into(), "1".into());
        assert_eq!(bare.query_values("x"), vec!["1"]);
        assert_eq!(bare.query_pairs(), vec![("x", "1")]);
    }
}
