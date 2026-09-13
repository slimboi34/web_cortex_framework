//! The application: manifest + router + op executor.
//!
//! Everything funnels through `dispatch`, whether it arrived over HTTP or was
//! invoked in-process by an agent. That single path is what makes "every route
//! is automatically a tool" true rather than aspirational — there is no second
//! implementation for agents to drift away from, and in particular no second
//! place where an authorization check could be forgotten.

use crate::agent::{AgentRuntime, ProviderRegistry, RunOptions, RunResult, SessionStore, SharedBudget, SuspendedRun};
use crate::audit::{AuditSink, MemoryAudit};
use crate::auth::{Authenticator, Principal};
use crate::bridge::{NoBridge, PyBridge};
use crate::files::FileServer;
use crate::http::{WebCortexRequest, WebCortexResponse};
use crate::ledger::Ledger;
use crate::manifest::{Manifest, Op, PageData, Route};
use crate::middleware::{Cors, RateLimiter};
use crate::router::{MatchError, Router};
use crate::templates::Templates;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(feature = "sqlite")]
use crate::db::Db;

pub struct App {
    pub manifest: Manifest,
    router: Router,
    routes_by_id: HashMap<u32, Route>,
    tools_by_name: HashMap<String, u32>,
    #[cfg(feature = "sqlite")]
    db: Option<Db>,
    bridge: Arc<dyn PyBridge>,
    client: reqwest::Client,
    upstream_tokens: HashMap<String, String>,

    pub authenticator: Authenticator,
    pub cors: Cors,
    pub rate_limiter: Option<RateLimiter>,
    templates: Option<Templates>,
    file_servers: HashMap<u32, FileServer>,
    pub audit: Arc<dyn AuditSink>,
    agent_runtime: Option<AgentRuntime>,
    registry: Arc<ProviderRegistry>,
    provider: Arc<dyn crate::agent::ModelProvider>,
    behaviours: HashMap<String, crate::manifest::BehaviourDef>,
    ledger: Arc<Ledger>,
    sessions: SessionStore,
    /// Runs waiting on a human. Bounded and expired by `approval_ttl_secs`.
    approvals: Mutex<HashMap<String, SuspendedRun>>,
    /// Set once the App is wrapped in an Arc. A Behaviour needs a handle to the
    /// application in order to call tools, and that reference is necessarily
    /// cyclic; a Weak keeps it from leaking.
    self_ref: std::sync::OnceLock<std::sync::Weak<App>>,
}

impl App {
    pub async fn build(manifest: Manifest, bridge: Arc<dyn PyBridge>) -> Result<Self, String> {
        manifest.validate()?;

        let router = Router::build(&manifest.routes)?;
        let routes_by_id: HashMap<u32, Route> =
            manifest.routes.iter().map(|r| (r.id, r.clone())).collect();
        let tools_by_name: HashMap<String, u32> = manifest
            .routes
            .iter()
            .filter(|r| r.tool.expose)
            .map(|r| (r.tool_name(), r.id))
            .collect();

        #[cfg(feature = "sqlite")]
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
                    Err(_) => tracing::warn!(
                        upstream = %name, env = %var,
                        "bearer env var not set; upstream will be called unauthenticated"
                    ),
                }
            }
        }

        let templates = match &manifest.templates {
            Some(cfg) => {
                let t = Templates::load(cfg)?;
                // Verify every declared template parses now, so a typo is a boot
                // failure rather than a 500 for whoever visits that page first.
                let names: Vec<String> = manifest
                    .routes
                    .iter()
                    .filter_map(|r| match &r.op {
                        Op::Page { template, .. } => Some(template.clone()),
                        _ => None,
                    })
                    .collect();
                t.verify(&names)?;
                Some(t)
            }
            None => None,
        };

        let mut file_servers = HashMap::new();
        for r in &manifest.routes {
            if let Op::Files { dir, index, cache_secs } = &r.op {
                file_servers.insert(r.id, FileServer::new(dir, index.clone(), *cache_secs)?);
            }
        }

        let authenticator = Authenticator::build(&manifest.auth)?;
        let cors = Cors::new(manifest.cors.clone());
        let rate_limiter = manifest
            .rate_limit
            .enabled
            .then(|| RateLimiter::new(&manifest.rate_limit));

        let audit: Arc<dyn AuditSink> = Arc::new(MemoryAudit::default());

        // Model access is resolved per call, by name, so an app with declared
        // agents and no key still boots; a run that needs a missing key fails
        // with a message naming the variable, and everything else keeps
        // serving. Ollama needs no key, so local models always resolve.
        let registry = Arc::new(ProviderRegistry::from_env(&manifest.models));
        let needs_model = !manifest.agents.is_empty()
            || !manifest.behaviours.is_empty()
            || !manifest.flows.is_empty();
        if needs_model {
            let d = registry.describe();
            let hosted = d["anthropic"] == true || d["openai"] == true || d["fake"] == true;
            if !hosted {
                tracing::warn!(
                    "agents, behaviours or flows are declared but neither ANTHROPIC_API_KEY \
                     nor OPENAI_API_KEY is set; only prefixed local models (ollama/…) and \
                     declared providers will resolve"
                );
            }
        }
        let provider: Arc<dyn crate::agent::ModelProvider> = registry.clone();
        let agent_runtime = Some(AgentRuntime::new(provider.clone(), audit.clone()));
        let ledger = Arc::new(Ledger::default());
        let sessions = SessionStore::new(manifest.server.session_capacity, manifest.server.session_ttl_secs);

        let behaviours: HashMap<String, crate::manifest::BehaviourDef> = manifest
            .behaviours
            .iter()
            .map(|b| (b.name.clone(), b.clone()))
            .collect();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("http client build failed: {e}"))?;

        Ok(Self {
            manifest,
            router,
            routes_by_id,
            tools_by_name,
            #[cfg(feature = "sqlite")]
            db,
            bridge,
            client,
            upstream_tokens,
            authenticator,
            cors,
            rate_limiter,
            templates,
            file_servers,
            audit,
            agent_runtime,
            registry,
            provider,
            behaviours,
            ledger,
            sessions,
            approvals: Mutex::new(HashMap::new()),
            self_ref: std::sync::OnceLock::new(),
        })
    }

    /// Wrap in an `Arc` and record the self-reference behaviours need.
    ///
    /// Always use this rather than `Arc::new(app)`: a behaviour dispatched from
    /// an App that never learned its own `Arc` cannot call tools.
    pub fn into_arc(self) -> Arc<Self> {
        let arc = Arc::new(self);
        let _ = arc.self_ref.set(Arc::downgrade(&arc));
        arc
    }

    fn arc_self(&self) -> Result<Arc<App>, String> {
        self.self_ref
            .get()
            .and_then(|w| w.upgrade())
            .ok_or_else(|| "app was not created with App::into_arc()".to_string())
    }

    pub fn provider(&self) -> &Arc<dyn crate::agent::ModelProvider> {
        &self.provider
    }

    pub fn registry(&self) -> &ProviderRegistry {
        &self.registry
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn sessions(&self) -> &SessionStore {
        &self.sessions
    }

    pub fn agent_runtime(&self) -> Option<&AgentRuntime> {
        self.agent_runtime.as_ref()
    }

    /// Park a run that is waiting on a human.
    pub fn suspend(&self, run: SuspendedRun) {
        let ttl = Duration::from_secs(self.manifest.server.approval_ttl_secs);
        let mut map = self.approvals.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, r| r.created.elapsed() <= ttl);
        if map.len() >= 1000 {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, r)| r.created)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(run.approval_id.clone(), run);
    }

    /// Everything currently waiting on a human, oldest first.
    pub fn pending_approvals(&self) -> Vec<serde_json::Value> {
        let ttl = Duration::from_secs(self.manifest.server.approval_ttl_secs);
        let mut map = self.approvals.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, r| r.created.elapsed() <= ttl);
        let mut runs: Vec<&SuspendedRun> = map.values().collect();
        runs.sort_by_key(|r| r.created);
        runs.iter().map(|r| r.summary()).collect()
    }

    /// Approve or deny a suspended run and continue it. The decision is
    /// consumed: a second call with the same id is an error.
    pub async fn resolve_approval(
        &self,
        approval_id: &str,
        approve: bool,
        note: &str,
        approver: &Principal,
    ) -> Result<RunResult, String> {
        let run = {
            let mut map = self.approvals.lock().unwrap_or_else(|p| p.into_inner());
            map.remove(approval_id)
        }
        .ok_or_else(|| format!("no pending approval {approval_id:?}; it may have expired or already been decided"))?;
        let runtime = self
            .agent_runtime
            .as_ref()
            .ok_or("agent runtime is unavailable")?;
        self.audit.record(crate::audit::AuditEvent {
            kind: "approval_resolved".into(),
            run_id: String::new(),
            actor: Some(approver.id.clone()),
            tool: None,
            detail: serde_json::json!({
                "approval_id": approval_id,
                "approved": approve,
                "on_behalf_of": run.caller().id,
            }),
        });
        Ok(runtime.resume_approval(self, run, approve, note).await)
    }

    pub fn behaviour(&self, name: &str) -> Option<&crate::manifest::BehaviourDef> {
        self.behaviours.get(name)
    }

    pub async fn build_without_python(manifest: Manifest) -> Result<Self, String> {
        Self::build(manifest, Arc::new(NoBridge)).await
    }

    /// Swap in a deterministic provider. Used by tests.
    pub fn with_agent_runtime(mut self, rt: AgentRuntime) -> Self {
        self.provider = rt.provider().clone();
        self.agent_runtime = Some(rt);
        self
    }

    #[cfg(feature = "sqlite")]
    pub fn db(&self) -> Option<&Db> {
        self.db.as_ref()
    }

    pub fn bridge(&self) -> &Arc<dyn PyBridge> {
        &self.bridge
    }

    pub fn route(&self, id: u32) -> Option<&Route> {
        self.routes_by_id.get(&id)
    }

    pub fn route_for_tool(&self, name: &str) -> Option<&Route> {
        self.tools_by_name.get(name).and_then(|id| self.routes_by_id.get(id))
    }

    pub fn exposed_tools(&self) -> Vec<&Route> {
        self.manifest.routes.iter().filter(|r| r.tool.expose).collect()
    }

    pub fn agent(&self, name: &str) -> Option<&crate::manifest::AgentDef> {
        self.manifest.agents.iter().find(|a| a.name == name)
    }

    /// All scopes a route demands, from either declaration site.
    fn required_scopes(route: &Route) -> Vec<String> {
        let mut all = route.scopes.clone();
        all.extend(route.tool.scopes.iter().cloned());
        all.sort();
        all.dedup();
        all
    }

    pub async fn dispatch(&self, mut req: WebCortexRequest) -> WebCortexResponse {
        let matched = match self.router.find(&req.method, &req.path) {
            Ok(m) => m,
            Err(MatchError::NotFound) => {
                return WebCortexResponse::error(404, format!("no route for {} {}", req.method, req.path));
            }
            Err(MatchError::MethodNotAllowed) => {
                let allowed = self.router.allowed_methods(&req.path).join(", ");
                let mut res = WebCortexResponse::error(
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
            return WebCortexResponse::error(500, "router matched an unknown route id");
        };

        let required = Self::required_scopes(route);
        if !required.is_empty() {
            let missing = req.principal.missing_scopes(&required);
            if !missing.is_empty() {
                // 401 when nobody is authenticated (the client can fix it by
                // logging in); 403 when a known principal simply lacks the
                // scope. Collapsing both to 403 makes auth bugs hard to debug.
                let status = if req.principal.is_anonymous() { 401 } else { 403 };
                return WebCortexResponse::error(
                    status,
                    format!("missing required scope(s): {}", missing.join(", ")),
                );
            }
        }

        match self.execute(&route.op, req).await {
            Ok(res) => res,
            Err(e) => WebCortexResponse::error(500, e),
        }
    }

    async fn execute(&self, op: &Op, req: WebCortexRequest) -> Result<WebCortexResponse, String> {
        match op {
            Op::Static { status, body } => Ok(WebCortexResponse::json(*status, body)),

            Op::Python { handler } => self.bridge.call(*handler, req).await,

            Op::Query { sql, params, returns } => {
                #[cfg(feature = "sqlite")]
                {
                    let db = self.db.as_ref().ok_or("no database configured")?;
                    let bindings: Vec<serde_json::Value> = params
                        .iter()
                        .map(|name| req.lookup(name).unwrap_or(serde_json::Value::Null))
                        .collect();
                    let value = match db.run(sql, &bindings, *returns).await {
                        Ok(v) => v,
                        Err(e) => return Ok(WebCortexResponse::error(e.status(), e.message())),
                    };
                    if value.is_null() && *returns == crate::manifest::QueryReturns::One {
                        return Ok(WebCortexResponse::error(404, "not found"));
                    }
                    Ok(WebCortexResponse::json(200, &value))
                }
                #[cfg(not(feature = "sqlite"))]
                {
                    let _ = (sql, params, returns, &req);
                    Err("this build has no database support".into())
                }
            }

            Op::Proxy { upstream, rewrite } => self.proxy(upstream, rewrite.as_deref(), req).await,

            Op::Page { template, data, status } => self.render_page(template, data, *status, req).await,

            Op::Files { .. } => {
                let id = req.route_id.ok_or("file route dispatched without a route id")?;
                let server = self.file_servers.get(&id).ok_or("file server not initialised")?;
                // The wildcard segment carries the path beneath the mount point.
                let relative = req
                    .path_params
                    .values()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "/".into());
                Ok(server.serve(&relative, req.headers.get("if-none-match").map(|s| s.as_str())))
            }

            Op::Agent { agent, .. } => {
                if let Some(res) = self.depth_exceeded(&req) {
                    return Ok(res);
                }
                self.run_agent(agent, req).await
            }

            Op::Behaviour { behaviour, .. } => {
                if let Some(res) = self.depth_exceeded(&req) {
                    return Ok(res);
                }
                self.run_behaviour(behaviour, req).await
            }

            Op::Flow { flow } => {
                if let Some(res) = self.depth_exceeded(&req) {
                    return Ok(res);
                }
                self.run_flow(flow, req).await
            }
        }
    }

    async fn run_flow(&self, name: &str, req: WebCortexRequest) -> Result<WebCortexResponse, String> {
        let def = self
            .manifest
            .flow(name)
            .ok_or_else(|| format!("unknown flow {name:?}"))?
            .clone();
        let input = req.json_body().unwrap_or(serde_json::Value::Null);
        match crate::flow::run(self, &def, input, &req.principal, req.depth, req.budget.clone()).await {
            Ok(value) => Ok(WebCortexResponse::json(200, &value)),
            Err(e) => Ok(WebCortexResponse::error(500, e)),
        }
    }

    /// Refuse an invocation nested deeper than the configured ceiling.
    ///
    /// Checked only for behaviour and agent ops, because those are the ones that
    /// can re-enter the dispatcher and form a cycle.
    fn depth_exceeded(&self, req: &WebCortexRequest) -> Option<WebCortexResponse> {
        let max = self.manifest.server.max_invocation_depth;
        if req.depth < max {
            return None;
        }
        tracing::warn!(
            depth = req.depth,
            max,
            path = %req.path,
            principal = %req.principal.id,
            "refused an invocation past the nesting ceiling; likely a recursive behaviour"
        );
        // 508 Loop Detected says precisely what happened.
        Some(WebCortexResponse::error(
            508,
            format!(
                "invocation nested {} deep, exceeding the ceiling of {max}; \
                 a behaviour or agent is calling itself",
                req.depth
            ),
        ))
    }

    async fn run_behaviour(
        &self,
        name: &str,
        req: WebCortexRequest,
    ) -> Result<WebCortexResponse, String> {
        let def = self
            .behaviours
            .get(name)
            .ok_or_else(|| format!("unknown behaviour {name:?}"))?
            .clone();

        // Same delegation rule as agents: a behaviour holds a subset of its
        // caller's authority, never a superset.
        let actor = req.principal.delegate_to_agent(&def.name, &def.scopes);
        let input = req.json_body().unwrap_or(serde_json::Value::Null);

        self.audit.record(crate::audit::AuditEvent {
            kind: "behaviour_started".into(),
            run_id: String::new(),
            actor: Some(actor.id.clone()),
            tool: Some(def.name.clone()),
            detail: serde_json::json!({"granted_scopes": actor.scopes}),
        });

        let app = self.arc_self()?;
        let budget = req
            .budget
            .clone()
            .unwrap_or_else(|| SharedBudget::new(format!("behaviour:{}", def.name), def.token_budget));
        match self
            .bridge
            .call_behaviour(app, def.clone(), input, actor.clone(), req.depth + 1, Some(budget))
            .await
        {
            Ok(value) => Ok(WebCortexResponse::json(200, &value)),
            Err(e) => {
                self.audit.record(crate::audit::AuditEvent {
                    kind: "behaviour_failed".into(),
                    run_id: String::new(),
                    actor: Some(actor.id),
                    tool: Some(name.to_string()),
                    detail: serde_json::json!({"error": &e}),
                });
                Ok(WebCortexResponse::error(500, e))
            }
        }
    }

    async fn render_page(
        &self,
        template: &str,
        data: &PageData,
        status: u16,
        req: WebCortexRequest,
    ) -> Result<WebCortexResponse, String> {
        let templates = self.templates.as_ref().ok_or("no templates configured")?;

        // Resolve data *before* rendering. The template never gets a handle to
        // anything it could use to fetch more.
        let mut context = serde_json::Map::new();
        match data {
            PageData::None => {}
            PageData::Static { value } => {
                context.insert("data".into(), value.clone());
            }
            PageData::Query { sql, params, returns, bind } => {
                #[cfg(feature = "sqlite")]
                {
                    let db = self.db.as_ref().ok_or("no database configured")?;
                    let bindings: Vec<serde_json::Value> = params
                        .iter()
                        .map(|n| req.lookup(n).unwrap_or(serde_json::Value::Null))
                        .collect();
                    let value = match db.run(sql, &bindings, *returns).await {
                        Ok(v) => v,
                        Err(e) => return Ok(WebCortexResponse::error(e.status(), e.message())),
                    };
                    if value.is_null() && *returns == crate::manifest::QueryReturns::One {
                        return Ok(WebCortexResponse::error(404, "not found"));
                    }
                    context.insert(bind.clone(), value);
                }
                #[cfg(not(feature = "sqlite"))]
                {
                    let _ = (sql, params, returns, bind);
                    return Err("this build has no database support".into());
                }
            }
            PageData::Python { handler } => {
                let res = self.bridge.call(*handler, req.clone()).await?;
                if res.status >= 400 {
                    return Ok(res);
                }
                context.insert("data".into(), res.json_value());
            }
        }

        // Request context every page can rely on, namespaced so it cannot
        // collide with the page's own data.
        context.insert(
            "request".into(),
            serde_json::json!({
                "path": req.path,
                "params": req.path_params,
                "query": req.query,
            }),
        );
        context.insert(
            "user".into(),
            serde_json::json!({
                "id": req.principal.id,
                "authenticated": !req.principal.is_anonymous(),
                "scopes": req.principal.scopes,
            }),
        );

        Ok(templates.render(template, &serde_json::Value::Object(context), status))
    }

    async fn run_agent(&self, name: &str, req: WebCortexRequest) -> Result<WebCortexResponse, String> {
        let Some(runtime) = &self.agent_runtime else {
            return Ok(WebCortexResponse::error(503, "agent runtime is unavailable"));
        };
        let def = self
            .agent(name)
            .ok_or_else(|| format!("unknown agent {name:?}"))?;

        let Some(input) = req.lookup("input").and_then(|v| v.as_str().map(str::to_string)) else {
            return Ok(WebCortexResponse::error(
                400,
                "agent invocation requires an 'input' string",
            ));
        };
        let session_id = req
            .lookup("session_id")
            .and_then(|v| v.as_str().map(str::to_string))
            .filter(|s| !s.is_empty());
        let reset_session = req
            .lookup("reset")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let opts = RunOptions {
            depth: req.depth,
            budget: req.budget.clone(),
            session_id,
            reset_session,
        };
        let result = runtime.run(self, def, &req.principal, &input, opts).await;
        Ok(run_result_response(&result))
    }

    /// Run an agent directly. Used by the control plane and by tests.
    pub async fn invoke_agent(
        &self,
        name: &str,
        input: &str,
        caller: &Principal,
        opts: RunOptions,
    ) -> Result<RunResult, String> {
        let runtime = self
            .agent_runtime
            .as_ref()
            .ok_or("agent runtime is unavailable")?;
        let def = self.agent(name).ok_or_else(|| format!("unknown agent {name:?}"))?;
        Ok(runtime.run(self, def, caller, input, opts).await)
    }

    async fn proxy(
        &self,
        upstream_name: &str,
        rewrite: Option<&str>,
        req: WebCortexRequest,
    ) -> Result<WebCortexResponse, String> {
        let up = self
            .manifest
            .upstreams
            .get(upstream_name)
            .ok_or_else(|| format!("unknown upstream {upstream_name:?}"))?;

        // Path parameters are attacker-controlled. Substituted naively into a
        // rewrite template they can climb out of the intended upstream prefix —
        // `/proxy/..` reaching `/` on the upstream was a confirmed escape — which
        // turns a narrow proxy into a general SSRF primitive against whatever the
        // upstream happens to be.
        for (name, value) in &req.path_params {
            if is_traversal(value) {
                tracing::warn!(
                    upstream = %upstream_name, param = %name,
                    "rejected proxy request whose path parameter contained traversal"
                );
                return Ok(WebCortexResponse::error(400, "invalid path parameter"));
            }
        }

        let tail = match rewrite {
            Some(t) => substitute_path(t, &req.path_params),
            None => req.path.clone(),
        };

        // Belt and braces: even with clean parameters, the assembled tail must
        // not contain a traversal segment.
        if tail.split('/').any(|seg| seg == ".." || seg == ".") {
            return Ok(WebCortexResponse::error(400, "invalid upstream path"));
        }

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
        // Forward only content negotiation. Never hop-by-hop headers, and never
        // the caller's Authorization — the upstream sees our credentials, not
        // whatever the client happened to send us.
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

        Ok(WebCortexResponse {
            status,
            headers: vec![("content-type".into(), content_type)],
            body,
        })
    }

    /// Invoke an exposed route by tool name, as a given principal.
    ///
    /// This is the in-process path an agent uses. It goes through `dispatch`,
    /// so scope enforcement is the same code that guards the HTTP path — there
    /// is no way to reach a route as an agent that you could not reach as a
    /// request.
    ///
    /// Returns a boxed future deliberately: an agent route can invoke a tool
    /// that reaches another agent, so this call graph is genuinely cyclic and
    /// needs an indirection to have a finite type.
    pub fn call_tool_as<'a>(
        &'a self,
        tool_name: &'a str,
        args: &'a serde_json::Value,
        principal: &'a Principal,
    ) -> futures::future::BoxFuture<'a, Result<serde_json::Value, String>> {
        self.call_tool_at_depth(tool_name, args, principal, 0)
    }

    /// As [`Self::call_tool_as`], carrying the caller's nesting depth so a cycle
    /// through tools is bounded.
    pub fn call_tool_at_depth<'a>(
        &'a self,
        tool_name: &'a str,
        args: &'a serde_json::Value,
        principal: &'a Principal,
        depth: u32,
    ) -> futures::future::BoxFuture<'a, Result<serde_json::Value, String>> {
        self.call_tool_in_tree(tool_name, args, principal, depth, None)
    }

    /// The full in-process call: depth *and* the request tree's shared budget,
    /// so a nested agent or behaviour spends against the same ceiling as the
    /// thing that called it.
    pub fn call_tool_in_tree<'a>(
        &'a self,
        tool_name: &'a str,
        args: &'a serde_json::Value,
        principal: &'a Principal,
        depth: u32,
        budget: Option<Arc<SharedBudget>>,
    ) -> futures::future::BoxFuture<'a, Result<serde_json::Value, String>> {
        Box::pin(async move { self.call_tool_inner(tool_name, args, principal, depth, budget).await })
    }

    async fn call_tool_inner(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        principal: &Principal,
        depth: u32,
        budget: Option<Arc<SharedBudget>>,
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

        let mut path_params = std::collections::BTreeMap::new();
        for seg in route.path.split('/') {
            if let Some(name) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                if let Some(v) = obj.get(name) {
                    path_params.insert(name.to_string(), json_to_path_string(v));
                }
            }
        }

        let concrete_path = substitute_path(&route.path, &path_params);
        let body_less = route.method == "GET" || route.method == "DELETE";
        let body = if body_less {
            bytes::Bytes::new()
        } else {
            bytes::Bytes::from(serde_json::to_vec(&obj).map_err(|e| e.to_string())?)
        };

        let mut query = std::collections::BTreeMap::new();
        if body_less {
            for (k, v) in &obj {
                if !path_params.contains_key(k) {
                    query.insert(k.clone(), json_to_path_string(v));
                }
            }
        }

        let req = WebCortexRequest {
            method: route.method.clone(),
            path: concrete_path,
            path_params,
            query,
            headers: Default::default(),
            body,
            route_id: Some(route_id),
            principal: principal.clone(),
            depth,
            budget,
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

    /// Convenience wrapper granting exactly the scopes a route declares.
    /// Used only where no real principal exists (tests, local tooling).
    pub async fn call_tool(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        scopes: Vec<String>,
    ) -> Result<serde_json::Value, String> {
        let principal = Principal {
            id: "local".into(),
            kind: crate::auth::PrincipalKind::ApiKey,
            scopes,
            claims: Default::default(),
        };
        self.call_tool_as(tool_name, args, &principal).await
    }
}

/// HTTP status for an agent run: 202 while a human is being waited on, 502
/// when the model side failed, 200 otherwise (including limits, which are
/// outcomes rather than errors).
pub fn run_result_response(result: &RunResult) -> WebCortexResponse {
    let status = match result.status {
        crate::agent::RunStatus::Failed => 502,
        crate::agent::RunStatus::AwaitingApproval => 202,
        _ => 200,
    };
    match serde_json::to_value(result) {
        Ok(v) => WebCortexResponse::json(status, &v),
        Err(e) => WebCortexResponse::error(500, e.to_string()),
    }
}

/// True when a value could alter the structure of a URL path it is spliced into.
///
/// Deliberately blunt: a path *parameter* is a single segment, so a slash or a
/// dot-dot in one is always either an attack or a bug. Checked after percent
/// decoding, and again on the raw text, so a doubly-encoded payload cannot slip
/// through whichever layer decoded only once.
fn is_traversal(value: &str) -> bool {
    let decoded = percent_decode_twice(value);
    for candidate in [value, decoded.as_str()] {
        if candidate.contains("..")
            || candidate.contains('/')
            || candidate.contains('\\')
            || candidate.contains('\0')
        {
            return true;
        }
    }
    false
}

fn percent_decode_twice(s: &str) -> String {
    fn once(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
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
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    once(&once(s))
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
        match seg
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .map(|n| n.trim_start_matches('*'))
        {
            Some(name) => out.push_str(params.get(name).map(|s| s.as_str()).unwrap_or("")),
            None => out.push_str(seg),
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}
