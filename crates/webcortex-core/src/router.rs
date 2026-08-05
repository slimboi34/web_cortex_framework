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
                    .map(|(k, v)| (k.to_string(), v.to_string()))
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
