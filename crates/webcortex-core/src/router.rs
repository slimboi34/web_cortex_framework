//! Method-partitioned radix routing over the manifest.
//!
//! One `matchit::Router` per method keeps matching to a single radix descent and
//! lets us distinguish 404 from 405 without a second data structure.

use crate::manifest::{Op, Route};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

pub struct Router {
    by_method: HashMap<String, matchit::Router<u32>>,
    /// Every declared path, regardless of method, for 405 detection.
    all_paths: matchit::Router<()>,
}

pub struct Matched {
    pub route_id: u32,
    pub path_params: BTreeMap<String, String>,
}

#[derive(Debug)]
pub enum MatchError {
    NotFound,
    MethodNotAllowed,
}

impl Router {
    pub fn build(routes: &[Route]) -> Result<Self, String> {
        let mut by_method: HashMap<String, matchit::Router<u32>> = HashMap::new();
        let mut all_paths = matchit::Router::new();

        for r in routes {
            let method = r.method.to_ascii_uppercase();
            let path = normalize_route(&r.path);
            by_method
                .entry(method.clone())
                .or_default()
                .insert(path.as_str(), r.id)
                .map_err(|e| format!("invalid route path {:?}: {e}", r.path))?;
            // Duplicate paths across methods are expected; ignore the conflict.
            let _ = all_paths.insert(path.as_str(), ());
        }

        // A static directory with an index is served at its mount point too.
        // A catch-all segment never matches the empty string, so
        // `static_files("/docs", ..., index="index.html")` answered 404 at
        // `/docs/` and `/docs`. Added last, and only where nothing else is
        // declared, so an explicit route at the mount point still wins.
        for r in routes {
            let Op::Files { index: Some(_), .. } = &r.op else { continue };
            let path = normalize_route(&r.path);
            let Some(mount) = mount_point(&path) else { continue };
            let method = r.method.to_ascii_uppercase();
            let router = by_method.entry(method).or_default();
            for alias in [format!("{mount}/"), mount.to_string()] {
                if !alias.is_empty() && router.insert(alias.as_str(), r.id).is_ok() {
                    let _ = all_paths.insert(alias.as_str(), ());
                }
            }
        }

        Ok(Self { by_method, all_paths })
    }

    pub fn find(&self, method: &str, path: &str) -> Result<Matched, MatchError> {
        let method = method.to_ascii_uppercase();
        let path = &*normalize_path(path);
        if let Some(m) = self.lookup(&method, path) {
            return Ok(m);
        }
        // HEAD is GET without the body (RFC 9110 §9.3.2), and hyper drops the
        // body of a HEAD response. A route declared for GET answers it, as
        // every load balancer health check and `curl -I` expects.
        if method == "HEAD"
            && let Some(m) = self.lookup("GET", path)
        {
            return Ok(m);
        }
        if self.all_paths.at(path).is_ok() {
            return Err(MatchError::MethodNotAllowed);
        }
        Err(MatchError::NotFound)
    }

    fn lookup(&self, method: &str, path: &str) -> Option<Matched> {
        let m = self.by_method.get(method)?.at(path).ok()?;
        let path_params = m
            .params
            .iter()
            .map(|(k, v)| (k.to_string(), decode_param(v)))
            .collect();
        Some(Matched { route_id: *m.value, path_params })
    }

    /// The methods `path` answers, for an `Allow` header: the declared ones,
    /// HEAD wherever GET is declared, and OPTIONS, which is always answered.
    pub fn allowed_methods(&self, path: &str) -> Vec<String> {
        let path = &*normalize_path(path);
        let mut out: Vec<String> = self
            .by_method
            .iter()
            .filter(|(_, r)| r.at(path).is_ok())
            .map(|(m, _)| m.clone())
            .collect();
        if out.is_empty() {
            return out;
        }
        if out.iter().any(|m| m == "GET") {
            out.push("HEAD".into());
        }
        out.push("OPTIONS".into());
        out.sort();
        out.dedup();
        out
    }
}

/// The mount point of a route whose last segment is a catch-all: `/docs` for
/// `/docs/{*file}`, `""` for `/{*file}`.
fn mount_point(path: &str) -> Option<&str> {
    let (mount, last) = path.rsplit_once('/')?;
    (last.starts_with("{*") && last.ends_with('}')).then_some(mount)
}

/// The one spelling of a request path that routes match against.
///
/// A route is declared with its literal text (`/café/{n}`), but a client may
/// send the same path as raw UTF-8, as `%C3%A9` or as `%c3%a9`, and may
/// percent-encode a letter that needed no encoding (`/%70ing`). All of these
/// are the same path (RFC 3986 §6.2.2), so both the declared routes and every
/// request path are brought to one form before matching: bytes outside ASCII
/// are percent-encoded, hex digits are upper case, and an encoded unreserved
/// character (a letter, a digit, `-`, `.`, `_`, `~`) is decoded. Everything
/// else that was encoded stays encoded — `%2F`, `%5C` and `%25` above all —
/// so an encoded slash still never becomes a separator, and a parameter is
/// still decoded exactly once, after the match ([`decode_param`]).
fn normalize_path(path: &str) -> Cow<'_, str> {
    if path.is_ascii() && !path.contains('%') {
        return Cow::Borrowed(path);
    }
    let mut out = String::with_capacity(path.len() + 8);
    normalize_into(&mut out, path);
    Cow::Owned(out)
}

/// [`normalize_path`] for a declared route: the same, except inside a
/// `{param}` or `{*rest}`, whose name is kept as written. `{{` and `}}` are
/// matchit's escaped braces and are literal text.
fn normalize_route(route: &str) -> String {
    let mut out = String::with_capacity(route.len() + 8);
    let mut rest = route;
    while let Some(open) = rest.find('{') {
        normalize_into(&mut out, &rest[..open]);
        rest = &rest[open..];
        if rest.starts_with("{{") {
            out.push_str("{{");
            rest = &rest[2..];
            continue;
        }
        let close = rest.find('}').map(|i| i + 1).unwrap_or(rest.len());
        out.push_str(&rest[..close]);
        rest = &rest[close..];
    }
    normalize_into(&mut out, rest);
    out
}

fn normalize_into(out: &mut String, text: &str) {
    use std::fmt::Write;
    let bytes = text.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16);
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%'
            && let (Some(hi), Some(lo)) = (bytes.get(i + 1).and_then(|&h| hex(h)), bytes.get(i + 2).and_then(|&l| hex(l)))
        {
            let byte = (hi * 16 + lo) as u8;
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                out.push(byte as char);
            } else {
                let _ = write!(out, "%{byte:02X}");
            }
            i += 3;
        } else if b >= 0x80 {
            let _ = write!(out, "%{b:02X}");
            i += 1;
        } else {
            out.push(b as char);
            i += 1;
        }
    }
}

/// Percent-decode one captured path parameter.
///
/// Matching runs on the raw request path, so `%2F` can never become a segment
/// separator and change which route is chosen; only the *captured value* is
/// decoded, once, after the match. `/books/by-author/Frank%20Herbert` therefore
/// binds `author = "Frank Herbert"` and `/books/%31/blurb` binds `id = "1"`,
/// which is what the query, the Python handler and the tool schema all expect.
/// A parameter that is not valid UTF-8 once decoded is kept as sent, so a
/// handler sees the original bytes rather than a lossy rewrite.
fn decode_param(raw: &str) -> String {
    if !raw.contains('%') {
        return raw.to_string();
    }
    match percent_encoding::percent_decode_str(raw).decode_utf8() {
        Ok(decoded) => decoded.into_owned(),
        Err(_) => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Route;

    fn route(id: u32, method: &str, path: &str, op: serde_json::Value) -> Route {
        serde_json::from_value(serde_json::json!({
            "id": id, "method": method, "path": path, "op": op,
        }))
        .expect("route")
    }

    fn files(index: Option<&str>) -> serde_json::Value {
        serde_json::json!({"kind": "files", "dir": ".", "index": index})
    }

    fn fixed() -> serde_json::Value {
        serde_json::json!({"kind": "static", "status": 200, "body": {}})
    }

    #[test]
    fn head_is_answered_by_the_get_route() {
        let r = Router::build(&[route(0, "GET", "/ping", fixed()), route(1, "POST", "/ping", fixed())]).unwrap();
        assert_eq!(r.find("HEAD", "/ping").unwrap().route_id, 0);
        assert_eq!(r.find("head", "/ping").unwrap().route_id, 0);
        assert!(matches!(r.find("PUT", "/ping"), Err(MatchError::MethodNotAllowed)));
        assert_eq!(r.allowed_methods("/ping"), ["GET", "HEAD", "OPTIONS", "POST"]);
        assert!(r.allowed_methods("/nope").is_empty());
    }

    #[test]
    fn an_explicit_head_route_wins_over_get() {
        let r = Router::build(&[route(0, "GET", "/x", fixed()), route(1, "HEAD", "/x", fixed())]).unwrap();
        assert_eq!(r.find("HEAD", "/x").unwrap().route_id, 1);
    }

    #[test]
    fn a_static_directory_with_an_index_answers_at_its_mount_point() {
        let r = Router::build(&[route(0, "GET", "/docs/{*file}", files(Some("index.html")))]).unwrap();
        for path in ["/docs", "/docs/", "/docs/a.txt"] {
            assert_eq!(r.find("GET", path).unwrap().route_id, 0, "{path}");
        }
        assert!(r.find("GET", "/docs/").unwrap().path_params.is_empty());
        let root = Router::build(&[route(0, "GET", "/{*file}", files(Some("index.html")))]).unwrap();
        assert_eq!(root.find("GET", "/").unwrap().route_id, 0);
    }

    #[test]
    fn the_mount_point_alias_yields_to_a_declared_route() {
        let r = Router::build(&[
            route(0, "GET", "/docs/{*file}", files(Some("index.html"))),
            route(1, "GET", "/docs", fixed()),
            route(2, "GET", "/", fixed()),
            route(3, "GET", "/{*file}", files(Some("index.html"))),
        ])
        .unwrap();
        assert_eq!(r.find("GET", "/docs").unwrap().route_id, 1);
        assert_eq!(r.find("GET", "/docs/").unwrap().route_id, 0);
        assert_eq!(r.find("GET", "/").unwrap().route_id, 2);
    }

    #[test]
    fn without_an_index_the_mount_point_is_not_aliased() {
        let r = Router::build(&[route(0, "GET", "/docs/{*file}", files(None))]).unwrap();
        assert!(r.find("GET", "/docs/").is_err());
    }

    fn get(path: &str) -> serde_json::Value {
        serde_json::json!({"kind": "static", "status": 200, "body": {"path": path}})
    }

    #[test]
    fn a_non_ascii_route_answers_however_the_client_spells_it() {
        let r = Router::build(&[
            route(0, "GET", "/café/{n}", get("café")),
            route(1, "GET", "/caf%c3%a9-enc/{n}", get("encoded")),
            route(2, "GET", "/ping", fixed()),
            route(3, "GET", "/%70ong", fixed()),
            route(4, "GET", "/files/{*rest}", fixed()),
            route(5, "GET", "/a/b/c", fixed()),
            route(6, "GET", "/a/{x}", fixed()),
            route(7, "GET", "/naïve/{número}", fixed()),
        ])
        .unwrap();
        for path in ["/caf%C3%A9/x", "/caf%c3%a9/x", "/café/x", "/caf%C3%a9/x"] {
            let m = r.find("GET", path).unwrap_or_else(|e| panic!("{path}: {e:?}"));
            assert_eq!(m.route_id, 0, "{path}");
            assert_eq!(m.path_params["n"], "x");
        }
        for path in ["/caf%C3%A9-enc/x", "/café-enc/x"] {
            assert_eq!(r.find("GET", path).unwrap().route_id, 1, "{path}");
        }
        assert_eq!(r.find("GET", "/%70ing").unwrap().route_id, 2);
        assert_eq!(r.find("GET", "/%70%69%6E%67").unwrap().route_id, 2);
        assert_eq!(r.find("GET", "/pong").unwrap().route_id, 3);
        assert_eq!(r.find("GET", "/naïve/uno").unwrap().path_params["número"], "uno");
        assert!(matches!(r.find("POST", "/caf%c3%a9/x"), Err(MatchError::MethodNotAllowed)));
        assert_eq!(r.allowed_methods("/caf%c3%a9/x"), ["GET", "HEAD", "OPTIONS"]);
        // Parameters are still decoded exactly once.
        assert_eq!(r.find("GET", "/café/caf%c3%a9").unwrap().path_params["n"], "café");
        assert_eq!(r.find("GET", "/café/%2541").unwrap().path_params["n"], "%41");
        assert_eq!(r.find("GET", "/café/Frank%20Herbert").unwrap().path_params["n"], "Frank Herbert");
        assert_eq!(r.find("GET", "/café/100%").unwrap().path_params["n"], "100%");
        assert_eq!(r.find("GET", "/files/a/b%2fc").unwrap().path_params["rest"], "a/b/c");
    }

    #[test]
    fn an_encoded_separator_still_never_splits_a_segment() {
        let r = Router::build(&[route(0, "GET", "/a/b/c", fixed()), route(1, "GET", "/a/{x}", fixed())]).unwrap();
        for path in ["/a/b%2Fc", "/a/b%2fc", "/a/b%5Cc", "/a/b%5cc"] {
            let m = r.find("GET", path).unwrap();
            assert_eq!(m.route_id, 1, "{path} must not reach /a/b/c");
        }
        assert_eq!(r.find("GET", "/a/b%2fc").unwrap().path_params["x"], "b/c");
        assert_eq!(normalize_path("/a/%2f%5c%25%41%7e%2E"), "/a/%2F%5C%25A~.");
        assert!(matches!(normalize_path("/plain/ascii"), Cow::Borrowed(_)));
        assert_eq!(normalize_route("/é/{é}/{*ü}/{{x}}"), "/%C3%A9/{é}/{*ü}/{{x}}");
    }

    #[test]
    fn decodes_percent_sequences_in_a_captured_parameter() {
        assert_eq!(decode_param("Frank%20Herbert"), "Frank Herbert");
        assert_eq!(decode_param("%31"), "1");
        assert_eq!(decode_param("caf%C3%A9"), "café");
    }

    #[test]
    fn leaves_plain_and_plus_alone() {
        // `+` is a space only in form bodies, never in a path segment.
        assert_eq!(decode_param("Frank+Herbert"), "Frank+Herbert");
        assert_eq!(decode_param("plain"), "plain");
    }

    #[test]
    fn keeps_invalid_utf8_as_sent() {
        assert_eq!(decode_param("%FF%FE"), "%FF%FE");
    }
}
