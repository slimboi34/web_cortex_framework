//! Method-partitioned radix routing over the manifest.
//!
//! One `matchit::Router` per method keeps matching to a single radix descent and
//! lets us distinguish 404 from 405 without a second data structure.

use crate::manifest::Route;
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
            by_method
                .entry(method.clone())
                .or_default()
                .insert(r.path.as_str(), r.id)
                .map_err(|e| format!("invalid route path {:?}: {e}", r.path))?;
            // Duplicate paths across methods are expected; ignore the conflict.
            let _ = all_paths.insert(r.path.as_str(), ());
        }

        Ok(Self { by_method, all_paths })
    }

    pub fn find(&self, method: &str, path: &str) -> Result<Matched, MatchError> {
        if let Some(router) = self.by_method.get(&method.to_ascii_uppercase()) {
            if let Ok(m) = router.at(path) {
                let path_params = m
                    .params
                    .iter()
                    .map(|(k, v)| (k.to_string(), decode_param(v)))
                    .collect();
                return Ok(Matched { route_id: *m.value, path_params });
            }
        }
        if self.all_paths.at(path).is_ok() {
            return Err(MatchError::MethodNotAllowed);
        }
        Err(MatchError::NotFound)
    }

    pub fn allowed_methods(&self, path: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .by_method
            .iter()
            .filter(|(_, r)| r.at(path).is_ok())
            .map(|(m, _)| m.clone())
            .collect();
        out.sort();
        out
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
    use super::decode_param;

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
