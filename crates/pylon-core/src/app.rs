//! The application: manifest + router + op executor.
//!
//! Everything funnels through `dispatch`, whether it arrived over HTTP or was
//! invoked in-process by an agent. That single path is what makes "every route
//! is automatically a tool" true rather than aspirational — there is no second
//! implementation for agents to drift away from.

use crate::bridge::{NoBridge, PyBridge};
use crate::db::Db;
use crate::http::{PylonRequest, PylonResponse};
use crate::manifest::{Manifest, Op, Route};
use crate::router::{MatchError, Router};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub struct App {
    pub manifest: Manifest,
    router: Router,
    routes_by_id: HashMap<u32, Route>,
    tools_by_name: HashMap<String, u32>,
    db: Option<Db>,
    bridge: Arc<dyn PyBridge>,
    client: reqwest::Client,
    /// Upstream bearer tokens resolved from the environment at boot, so the
    /// manifest can be logged or shipped without leaking credentials.
    upstream_tokens: HashMap<String, String>,
}

impl App {
    pub async fn build(manifest: Manifest, bridge: Arc<dyn PyBridge>) -> Result<Self, String> {
        manifest.validate()?;

        let router = Router::build(&manifest.routes)?;
        let routes_by_id: HashMap<u32, Route> =
            manifest.routes.iter().map(|r| (r.id, r.clone())).collect();

        // `validate` has already rejected duplicate tool names.
        let tools_by_name: HashMap<String, u32> = manifest
            .routes
            .iter()
            .filter(|r| r.tool.expose)
            .map(|r| (r.tool_name(), r.id))
            .collect();

        let db = match &manifest.database {
            Some(cfg) => Some(Db::connect(cfg).await?),
            None => None,
        };

        let mut upstream_tokens = HashMap::new();
        for (name, up) in &manifest.upstreams {
            if let Some(var) = &up.bearer_env {
                match std::env::var(var) {
                    Ok(tok) => {
                        upstream_tokens.insert(name.clone(), tok);
                    }
                    Err(_) => {
                        tracing::warn!(
                            upstream = %name,
                            env = %var,
                            "bearer env var not set; upstream will be called unauthenticated"
                        );
                    }
                }
            }
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("http client build failed: {e}"))?;

        Ok(Self {
            manifest,
            router,
            routes_by_id,
            tools_by_name,
            db,
            bridge,
            client,
            upstream_tokens,
        })
    }

    pub async fn build_without_python(manifest: Manifest) -> Result<Self, String> {
        Self::build(manifest, Arc::new(NoBridge)).await
    }

    pub fn db(&self) -> Option<&Db> {
        self.db.as_ref()
    }

    pub fn bridge(&self) -> &Arc<dyn PyBridge> {
        &self.bridge
    }

    pub fn route(&self, id: u32) -> Option<&Route> {
        self.routes_by_id.get(&id)
    }

    /// Routes an agent may call, in declaration order.
    pub fn exposed_tools(&self) -> Vec<&Route> {
        self.manifest.routes.iter().filter(|r| r.tool.expose).collect()
    }

    pub async fn dispatch(&self, mut req: PylonRequest) -> PylonResponse {
        let matched = match self.router.find(&req.method, &req.path) {
            Ok(m) => m,
            Err(MatchError::NotFound) => {
                return PylonResponse::error(404, format!("no route for {} {}", req.method, req.path));
            }
            Err(MatchError::MethodNotAllowed) => {
                let allowed = self.router.allowed_methods(&req.path).join(", ");
                let mut res = PylonResponse::error(
                    405,
                    format!("{} not allowed on {}; try: {}", req.method, req.path, allowed),
                );
                res.headers.push(("allow".into(), allowed));
                return res;
            }
        };

        req.path_params = matched.path_params;
        req.route_id = Some(matched.route_id);

        let Some(route) = self.routes_by_id.get(&matched.route_id) else {
            return PylonResponse::error(500, "router matched an unknown route id");
        };

        if !route.tool.scopes.is_empty() {
            let missing: Vec<&String> = route
                .tool
                .scopes
                .iter()
                .filter(|s| !req.scopes.contains(s))
                .collect();
            if !missing.is_empty() {
                return PylonResponse::error(
                    403,
                    format!(
                        "missing required scope(s): {}",
                        missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                    ),
                );
            }
        }

        match self.execute(&route.op, req).await {
            Ok(res) => res,
            Err(e) => PylonResponse::error(500, e),
        }
    }

    async fn execute(&self, op: &Op, req: PylonRequest) -> Result<PylonResponse, String> {
        match op {
            Op::Static { status, body } => Ok(PylonResponse::json(*status, body)),

            Op::Python { handler } => self.bridge.call(*handler, req).await,

            Op::Query { sql, params, returns } => {
                let db = self.db.as_ref().ok_or("no database configured")?;
                let bindings: Vec<serde_json::Value> = params
                    .iter()
                    .map(|name| req.lookup(name).unwrap_or(serde_json::Value::Null))
                    .collect();
                let value = db.run(sql, &bindings, *returns).await?;
                if value.is_null() && *returns == crate::manifest::QueryReturns::One {
                    return Ok(PylonResponse::error(404, "not found"));
                }
                Ok(PylonResponse::json(200, &value))
            }

            Op::Proxy { upstream, rewrite } => self.proxy(upstream, rewrite.as_deref(), req).await,

            Op::Agent { agent, .. } => {
                // The agent runtime lands in the next milestone; until then this
                // reports precisely what is missing rather than pretending.
                Ok(PylonResponse::error(
                    501,
                    format!("agent {agent:?} is declared but the agent runtime is not enabled in this build"),
                ))
            }
        }
    }

    async fn proxy(
        &self,
        upstream_name: &str,
        rewrite: Option<&str>,
        req: PylonRequest,
    ) -> Result<PylonResponse, String> {
        let up = self
            .manifest
            .upstreams
            .get(upstream_name)
            .ok_or_else(|| format!("unknown upstream {upstream_name:?}"))?;

        let tail = match rewrite {
            Some(t) => substitute_path(t, &req.path_params),
            None => req.path.clone(),
        };
        let url = format!("{}{}", up.base_url.trim_end_matches('/'), tail);

        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| format!("bad method: {e}"))?;
        let mut builder = self
            .client
            .request(method, &url)
            .timeout(Duration::from_millis(up.timeout_ms));

        for (k, v) in &up.headers {
            builder = builder.header(k, v);
        }
        if let Some(tok) = self.upstream_tokens.get(upstream_name) {
            builder = builder.bearer_auth(tok);
        }
        // Forward content-type and accept, but never hop-by-hop headers or the
        // caller's own Authorization — the upstream gets our credentials only.
        for k in ["content-type", "accept"] {
            if let Some(v) = req.headers.get(k) {
                builder = builder.header(k, v);
            }
        }
        if !req.query.is_empty() {
            builder = builder.query(&req.query.iter().collect::<Vec<_>>());
        }
        if !req.body.is_empty() {
            builder = builder.body(req.body.clone());
        }

        let res = builder
            .send()
            .await
            .map_err(|e| format!("upstream {upstream_name} failed: {e}"))?;

        let status = res.status().as_u16();
        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        let body = res
            .bytes()
            .await
            .map_err(|e| format!("upstream {upstream_name} body read failed: {e}"))?;

        Ok(PylonResponse {
            status,
            headers: vec![("content-type".into(), content_type)],
            body,
        })
    }

    /// Invoke an exposed route by tool name with a flat argument object.
    ///
    /// This is the in-process path an agent uses. It never opens a socket, so a
    /// local agent calling ten tools pays ten function calls, not ten round
    /// trips through the loopback interface.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        scopes: Vec<String>,
    ) -> Result<serde_json::Value, String> {
        let route_id = *self
            .tools_by_name
            .get(tool_name)
            .ok_or_else(|| format!("unknown tool {tool_name:?}"))?;
        let route = self
            .routes_by_id
            .get(&route_id)
            .ok_or("tool points at an unknown route")?;

        let obj = args.as_object().cloned().unwrap_or_default();

        // Path params come out of the same flat object the model produced.
        let mut path_params = std::collections::BTreeMap::new();
        for seg in route.path.split('/') {
            if let Some(name) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                if let Some(v) = obj.get(name) {
                    path_params.insert(name.to_string(), json_to_path_string(v));
                }
            }
        }

        let concrete_path = substitute_path(&route.path, &path_params);
        let body = if route.method == "GET" || route.method == "DELETE" {
            bytes::Bytes::new()
        } else {
            bytes::Bytes::from(serde_json::to_vec(&obj).map_err(|e| e.to_string())?)
        };

        let mut query = std::collections::BTreeMap::new();
        if route.method == "GET" || route.method == "DELETE" {
            for (k, v) in &obj {
                if !path_params.contains_key(k) {
                    query.insert(k.clone(), json_to_path_string(v));
                }
            }
        }

        let req = PylonRequest {
            method: route.method.clone(),
            path: concrete_path,
            path_params,
            query,
            headers: Default::default(),
            body,
            route_id: Some(route_id),
            scopes,
        };

        let res = self.dispatch(req).await;
        if res.status >= 400 {
            return Err(format!(
                "tool {tool_name} failed with {}: {}",
                res.status,
                String::from_utf8_lossy(&res.body)
            ));
        }
        Ok(res.json_value())
    }
}

fn json_to_path_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn substitute_path(
    template: &str,
    params: &std::collections::BTreeMap<String, String>,
) -> String {
    let mut out = String::with_capacity(template.len());
    for seg in template.split('/') {
        if seg.is_empty() {
            continue;
        }
        out.push('/');
        match seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(name) => out.push_str(params.get(name).map(|s| s.as_str()).unwrap_or("")),
            None => out.push_str(seg),
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}
