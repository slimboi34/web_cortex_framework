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
    /// For each tool that would reach an approval-gated tool without an
    /// agent asking a human first, that gated tool (see
    /// [`Manifest::gated_tool_behind`]).
    gated_behind: HashMap<String, String>,
    #[cfg(feature = "sqlite")]
    db: Option<Db>,
    bridge: Arc<dyn PyBridge>,
    client: reqwest::Client,
    /// The client proxy routes use: it never follows a redirect and never
    /// sends a Referer, so an upstream's 3xx reaches the caller instead of
    /// being fetched — with the upstream's credentials — from wherever it points.
    proxy_client: reqwest::Client,
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
    /// The emergency stop. While set, every route declared `actuator` refuses
    /// to run, whoever asks and however they arrive.
    halt: Arc<Mutex<Option<serde_json::Value>>>,
    /// Cameras and sensors: frames, telemetry, insights and their subscribers.
    devices: crate::devices::DeviceHub,
    /// Request counters, recent failures and agent runs: the live view.
    recorder: crate::status::Recorder,
    /// Spans of recent agent, flow and behaviour runs.
    tracer: Arc<crate::trace::Tracer>,
    /// One permit per open connection or WebSocket; `None` when uncapped.
    connections: Option<Arc<tokio::sync::Semaphore>>,
    /// Flipped once at shutdown. Every WebSocket holds a receiver, so "no
    /// receivers left" is "every socket has said goodbye".
    shutdown: tokio::sync::watch::Sender<bool>,
}

/// Load the app's templates and check every page's template parses — what a
/// boot does, and what `webcortex check` does too, so a typo or a missing
/// file is reported there rather than first by `webcortex run`.
pub fn load_templates(manifest: &Manifest) -> Result<Option<Templates>, String> {
    let Some(cfg) = &manifest.templates else { return Ok(None) };
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
    Ok(Some(t))
}

/// What an HTTP request's dispatch produced: a whole response, or a static
/// file's response head and the open file to stream after it.
pub enum Reply {
    Full(WebCortexResponse),
    File { head: WebCortexResponse, file: std::fs::File, len: u64 },
}

/// Most runs waiting on a human at once, across every caller.
const MAX_APPROVALS: usize = 1000;

/// Most of those one root principal (the human or service behind a chain of
/// agents) may hold. Past it the principal's own oldest is evicted, never
/// another principal's.
pub(crate) const MAX_APPROVALS_PER_PRINCIPAL: usize = 100;

/// A connection's place under `max_connections`, released on drop.
pub struct ConnectionSlot(#[allow(dead_code)] Option<tokio::sync::OwnedSemaphorePermit>);

/// Where the trace id of the run an HTTP request started is written, so the
/// server can still name it when the request times out and the run's own
/// response is never built.
pub type EdgeTrace = Arc<Mutex<Option<String>>>;

tokio::task_local! {
    static EDGE_TRACE: EdgeTrace;
}

/// Run `f` (a request's dispatch) with `slot` receiving the trace id of the
/// agent, flow or behaviour run it starts at the edge.
pub async fn with_edge_trace<F: std::future::Future>(slot: EdgeTrace, f: F) -> F::Output {
    EDGE_TRACE.scope(slot, f).await
}

/// Record the trace an edge run (depth 0) started. The first one wins; outside
/// [`with_edge_trace`] this does nothing.
pub(crate) fn note_edge_trace(depth: u32, span: Option<&crate::trace::TraceCtx>) {
    if depth != 0 {
        return;
    }
    let Some(span) = span else { return };
    let _ = EDGE_TRACE.try_with(|slot| {
        let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(span.trace_id.to_string());
        }
    });
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
        let gated_behind: HashMap<String, String> = tools_by_name
            .keys()
            .filter_map(|name| manifest.gated_tool_behind(name).map(|gated| (name.clone(), gated)))
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

        let templates = load_templates(&manifest)?;

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
        let proxy_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .build()
            .map_err(|e| format!("http client build failed: {e}"))?;
        let manifest_start_halted = manifest.server.start_halted;
        let manifest_max_connections = manifest.server.max_connections;
        let devices = crate::devices::DeviceHub::new(&manifest.devices, client.clone());
        let recorder = crate::status::Recorder::new(&manifest);
        let tracer = Arc::new(crate::trace::Tracer::new(&manifest.tracing));

        Ok(Self {
            manifest,
            router,
            routes_by_id,
            tools_by_name,
            gated_behind,
            #[cfg(feature = "sqlite")]
            db,
            bridge,
            client,
            proxy_client,
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
            halt: Arc::new(Mutex::new(manifest_start_halted.then(|| {
                // The boot time, not 0: a reader computing "halted for" from
                // since_unix would otherwise see fifty years.
                let since = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                serde_json::json!({"reason": "started halted; release to enable actuators",
                                   "by": "boot", "since_unix": since})
            }))),
            connections: (manifest_max_connections > 0)
                .then(|| Arc::new(tokio::sync::Semaphore::new(manifest_max_connections))),
            shutdown: tokio::sync::watch::Sender::new(false),
            devices,
            recorder,
            tracer,
        })
    }

    /// Claim a place for a new connection, or `None` when `max_connections`
    /// are already open.
    pub fn admit_connection(&self) -> Option<ConnectionSlot> {
        match &self.connections {
            None => Some(ConnectionSlot(None)),
            Some(sem) => sem.clone().try_acquire_owned().ok().map(|p| ConnectionSlot(Some(p))),
        }
    }

    /// Resolves once shutdown has begun. Held by each WebSocket.
    pub fn shutdown_watch(&self) -> tokio::sync::watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    /// Tell every WebSocket to close (1001), then wait until they all have.
    pub async fn close_sockets(&self) {
        self.shutdown.send_replace(true);
        self.shutdown.closed().await;
    }

    /// How many WebSockets are open.
    pub fn open_sockets(&self) -> usize {
        self.shutdown.receiver_count()
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
        // One caller's runs make room among themselves. Through 2.8.0 the
        // store evicted the oldest approval of anyone's, so one principal
        // that suspended a thousand runs threw away every other principal's
        // pending approvals (and their runs) and filled the approver's queue.
        let owner = run.root().to_string();
        let mine = map.values().filter(|r| r.root() == owner).count();
        let evict = if mine >= MAX_APPROVALS_PER_PRINCIPAL {
            map.iter().filter(|(_, r)| r.root() == owner).min_by_key(|(_, r)| r.created).map(|(k, _)| k.clone())
        } else if map.len() >= MAX_APPROVALS {
            map.iter().min_by_key(|(_, r)| r.created).map(|(k, _)| k.clone())
        } else {
            None
        };
        if let Some(oldest) = evict {
            tracing::warn!(approval = %oldest, principal = %owner, "pending approval evicted to make room");
            map.remove(&oldest);
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
        // An expired approval is only swept when another run suspends or the
        // list is read, so the age is checked here too: without it a decision
        // could still resume a run hours past `approval_ttl_secs`.
        let ttl = Duration::from_secs(self.manifest.server.approval_ttl_secs);
        let run = {
            let mut map = self.approvals.lock().unwrap_or_else(|p| p.into_inner());
            map.remove(approval_id)
        }
        .filter(|r| r.created.elapsed() <= ttl)
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

    pub fn devices(&self) -> &crate::devices::DeviceHub {
        &self.devices
    }

    /// The live counters behind `GET {control_prefix}/status`.
    pub fn recorder(&self) -> &crate::status::Recorder {
        &self.recorder
    }

    /// The trace store: spans of recent runs, and the door to the exporter.
    pub fn tracer(&self) -> &Arc<crate::trace::Tracer> {
        &self.tracer
    }

    /// Where a model-backed op's span attaches: under the span of the call
    /// that reached it, or, at the edge, under the caller's `traceparent`.
    fn span_for(&self, req: &WebCortexRequest, kind: &'static str, name: &str) -> Option<crate::trace::TraceCtx> {
        let span = self.tracer.begin(req.trace.as_ref(), req.headers.get("traceparent").map(String::as_str), kind, name);
        note_edge_trace(req.depth, span.as_ref());
        span
    }

    /// The live system view: requests, failures, runs, spend, e-stop, devices.
    pub fn status(&self) -> serde_json::Value {
        crate::status::snapshot(self)
    }

    /// Which route a request would reach, for counting it. `None` when none does.
    pub fn route_id_for(&self, method: &str, path: &str) -> Option<u32> {
        self.router.find(method, path).ok().map(|m| m.route_id)
    }

    /// The shared outbound HTTP client: upstreams, pulled cameras, webhooks.
    pub fn http_client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Match a request to its route and check the caller may reach it, without
    /// running it. The WebSocket upgrade uses this, so a socket is held to the
    /// same scopes as the route it is mounted on.
    pub fn authorize(&self, method: &str, path: &str, principal: &Principal) -> Result<&Route, WebCortexResponse> {
        let matched = self.router.find(method, path).map_err(|e| match e {
            MatchError::NotFound => self.no_route(method, path),
            MatchError::MethodNotAllowed => WebCortexResponse::coded(405, "WC202", format!("{method} not allowed on {path}")),
        })?;
        let route = self
            .routes_by_id
            .get(&matched.route_id)
            .ok_or_else(|| WebCortexResponse::error(500, "router matched an unknown route id"))?;
        let missing = principal.missing_scopes(&Self::required_scopes(route));
        if !missing.is_empty() {
            return Err(scope_refusal_for(principal, &missing));
        }
        Ok(route)
    }

    /// Engage the emergency stop. Idempotent; a second halt updates the reason.
    pub fn halt(&self, reason: &str, by: &Principal) {
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let state = serde_json::json!({"reason": reason, "by": by.id, "since_unix": since});
        *self.halt.lock().unwrap_or_else(|p| p.into_inner()) = Some(state.clone());
        tracing::warn!(reason = %reason, by = %by.id, "actuators halted");
        self.audit.record(crate::audit::AuditEvent {
            kind: "halted".into(),
            run_id: String::new(),
            actor: Some(by.id.clone()),
            tool: None,
            detail: state,
        });
    }

    /// Release the emergency stop. Returns whether it was engaged.
    pub fn release(&self, by: &Principal) -> bool {
        let was = self.halt.lock().unwrap_or_else(|p| p.into_inner()).take();
        if was.is_some() {
            tracing::warn!(by = %by.id, "actuators released");
            self.audit.record(crate::audit::AuditEvent {
                kind: "released".into(),
                run_id: String::new(),
                actor: Some(by.id.clone()),
                tool: None,
                detail: serde_json::json!({"was": was}),
            });
        }
        was.is_some()
    }

    /// The halt state, when the emergency stop is engaged.
    pub fn halted(&self) -> Option<serde_json::Value> {
        self.halt.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The emergency stop, for a handler to check as it starts.
    pub fn halt_gate(&self) -> crate::bridge::HaltGate {
        crate::bridge::HaltGate::new(self.halt.clone(), self.manifest.server.control_prefix.clone())
    }

    pub fn behaviour(&self, name: &str) -> Option<&crate::manifest::BehaviourDef> {
        self.behaviours.get(name)
    }

    pub async fn build_without_python(manifest: Manifest) -> Result<Self, String> {
        Self::build(manifest, Arc::new(NoBridge)).await
    }

    /// Answer every model call in this app from `provider` instead of the
    /// registry: a cassette being replayed or recorded (`webcortex eval`), or
    /// a scripted provider in a test. Agents, behaviours, flow classifiers
    /// and compaction all go through it.
    pub fn with_provider(mut self, provider: Arc<dyn crate::agent::ModelProvider>) -> Self {
        self.agent_runtime = Some(AgentRuntime::new(provider.clone(), self.audit.clone()));
        self.provider = provider;
        self
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

    /// True when `method path` runs a model loop — an agent, a flow or a
    /// behaviour — and so is bounded by `agent_timeout_secs`.
    pub fn runs_a_model_loop(&self, method: &str, path: &str) -> bool {
        self.router
            .find(method, path)
            .ok()
            .and_then(|m| self.routes_by_id.get(&m.route_id))
            .is_some_and(|r| matches!(r.op, Op::Agent { .. } | Op::Flow { .. } | Op::Behaviour { .. }))
    }

    pub fn route_for_tool(&self, name: &str) -> Option<&Route> {
        self.tools_by_name.get(name).and_then(|id| self.routes_by_id.get(id))
    }

    /// The approval-gated tool calling `name` would run with no human asked:
    /// `name` itself when gated, or a gated step inside it when it is a flow.
    /// Every entry point that dispatches a tool by name without suspending
    /// for approval (MCP, behaviours, flow steps) refuses when this is `Some`.
    pub fn gated_tool_behind(&self, name: &str) -> Option<&str> {
        self.gated_behind.get(name).map(String::as_str)
    }

    pub fn exposed_tools(&self) -> Vec<&Route> {
        self.manifest.routes.iter().filter(|r| r.tool.expose).collect()
    }

    pub fn agent(&self, name: &str) -> Option<&crate::manifest::AgentDef> {
        self.manifest.agents.iter().find(|a| a.name == name)
    }

    /// All scopes a route demands, from either declaration site.
    pub fn scopes_of(route: &Route) -> Vec<String> {
        Self::required_scopes(route)
    }

    /// All scopes a route demands, from either declaration site.
    fn required_scopes(route: &Route) -> Vec<String> {
        let mut all = route.scopes.clone();
        all.extend(route.tool.scopes.iter().cloned());
        all.sort();
        all.dedup();
        all
    }

    /// The 404 for a path no route matches. Under a device prefix, naming a
    /// device that was never declared, it is WC501 "unknown device" rather
    /// than WC201, whose hint sends the reader looking for a route.
    fn no_route(&self, method: &str, path: &str) -> WebCortexResponse {
        if let Some(name) = self.undeclared_device_in(path) {
            return WebCortexResponse::coded(
                404,
                "WC501",
                format!("no device named {name:?} (no route for {method} {path})"),
            );
        }
        WebCortexResponse::coded(404, "WC201", format!("no route for {method} {path}"))
    }

    /// The device segment of `path`, when `path` sits under the prefix of a
    /// declared device's routes and names a device that does not exist.
    fn undeclared_device_in<'p>(&self, path: &'p str) -> Option<&'p str> {
        if self.devices().is_empty() {
            return None;
        }
        for route in &self.manifest.routes {
            let Op::Device { device, .. } = &route.op else { continue };
            let Some(at) = route.path.find(&format!("/{device}/")) else { continue };
            let prefix = &route.path[..at];
            if prefix.is_empty() {
                continue;
            }
            let Some(rest) = path.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')) else { continue };
            let Some((name, _)) = rest.split_once('/') else { continue };
            if !name.is_empty() && name.len() <= 64 && self.devices().get(name).is_none() {
                return Some(name);
            }
            return None;
        }
        None
    }

    /// Everything dispatch decides before an op runs: the route (404, 405,
    /// or OPTIONS's `Allow`), that a tool call's arguments address the route
    /// it named, the caller's scopes (401/403), and the emergency stop (423).
    fn admit(
        &self,
        method: &str,
        path: &str,
        principal: &Principal,
        expected: Option<u32>,
    ) -> Result<(&Route, std::collections::BTreeMap<String, String>), WebCortexResponse> {
        let matched = match self.router.find(method, path) {
            Ok(m) => m,
            Err(MatchError::NotFound) => return Err(self.no_route(method, path)),
            Err(MatchError::MethodNotAllowed) => {
                let allowed = self.router.allowed_methods(path).join(", ");
                // OPTIONS asks what a path supports; the answer is the Allow
                // header, not a 405 for having asked.
                if method.eq_ignore_ascii_case("OPTIONS") {
                    return Err(WebCortexResponse {
                        status: 204,
                        headers: vec![("allow".into(), allowed)],
                        body: bytes::Bytes::new(),
                    });
                }
                let mut res = WebCortexResponse::coded(
                    405,
                    "WC202",
                    format!("{method} not allowed on {path}; try: {allowed}"),
                );
                res.headers.push(("allow".into(), allowed));
                return Err(res);
            }
        };

        // A tool call arrives already naming its route, and its path was built
        // from the caller's (or a model's) arguments. If those arguments steer
        // the path onto a different route — `id = "purge"` landing on a static
        // `/notes/purge`, say — refuse rather than run it: approval gates are
        // checked against the tool that was named, so this would be a way round them.
        if expected.is_some_and(|expected| expected != matched.route_id) {
            return Err(WebCortexResponse::coded(
                400,
                "WC208",
                "the arguments do not address this tool's route; a path argument may not \
                 select a different route",
            ));
        }

        let Some(route) = self.routes_by_id.get(&matched.route_id) else {
            return Err(WebCortexResponse::error(500, "router matched an unknown route id"));
        };

        let required = Self::required_scopes(route);
        if !required.is_empty() {
            let missing = principal.missing_scopes(&required);
            if !missing.is_empty() {
                // 401 when nobody is authenticated (the client can fix it by
                // logging in); 403 when a known principal simply lacks the
                // scope. Collapsing both to 403 makes auth bugs hard to debug.
                return Err(scope_refusal_for(principal, &missing));
            }
        }

        // The emergency stop sits here, after authentication and before the
        // op, so it covers HTTP, agent tool calls, behaviours, flows, MCP and
        // approvals resumed after the halt — they all come through dispatch.
        if route.actuator
            && let Some(refusal) = self.halt_gate().refusal() {
                return Err(WebCortexResponse::coded(423, "WC502", refusal));
            }
        Ok((route, matched.path_params))
    }

    /// The answer dispatch would give this request without running anything
    /// — no route, a method it does not take, a scope the caller lacks, the
    /// e-stop — or `None` when it would go on to run the op. The server asks
    /// before reading a request body, so a body for a route the caller may
    /// not use is never read: through 2.8.0 up to 32 MB of it was buffered
    /// first, from anyone.
    pub fn refusal(&self, method: &str, path: &str, principal: &Principal) -> Option<WebCortexResponse> {
        self.admit(method, path, principal, None).err()
    }

    pub async fn dispatch(&self, mut req: WebCortexRequest) -> WebCortexResponse {
        let (route, path_params) = match self.admit(&req.method, &req.path, &req.principal, req.route_id) {
            Ok(v) => v,
            Err(res) => return res,
        };
        req.path_params = path_params;
        req.route_id = Some(route.id);
        match self.execute(&route.op, req).await {
            Ok(res) => res,
            Err(e) => WebCortexResponse::coded(500, op_failure_code(&route.op, &e), e),
        }
    }

    /// [`Self::dispatch`] for the HTTP server: the same, except that a static
    /// file is opened rather than read, so it can be streamed from disk.
    pub async fn dispatch_reply(&self, mut req: WebCortexRequest) -> Reply {
        let (route, path_params) = match self.admit(&req.method, &req.path, &req.principal, req.route_id) {
            Ok(v) => v,
            Err(res) => return Reply::Full(res),
        };
        req.path_params = path_params;
        req.route_id = Some(route.id);
        if let Op::Files { .. } = route.op
            && let Some(server) = self.file_servers.get(&route.id)
        {
            let relative = req.path_params.values().next().cloned().unwrap_or_else(|| "/".into());
            return match server.open(&relative, &req.path, req.headers.get("if-none-match").map(|s| s.as_str())) {
                crate::files::FileReply::Done(res) => Reply::Full(res),
                crate::files::FileReply::Stream { head, file, len } => Reply::File { head, file, len },
            };
        }
        Reply::Full(match self.execute(&route.op, req).await {
            Ok(res) => res,
            Err(e) => WebCortexResponse::coded(500, op_failure_code(&route.op, &e), e),
        })
    }

    async fn execute(&self, op: &Op, req: WebCortexRequest) -> Result<WebCortexResponse, String> {
        match op {
            Op::Static { status, body } => Ok(WebCortexResponse::json(*status, body)),

            // An actuator checks the stop again as its handler starts: it may
            // have queued behind other handlers since dispatch let it through.
            Op::Python { handler }
                if req.route_id.and_then(|id| self.routes_by_id.get(&id)).is_some_and(|r| r.actuator) =>
            {
                self.bridge.call_actuator(*handler, req, self.halt_gate()).await
            }
            Op::Python { handler } => self.bridge.call(*handler, req).await,

            Op::Query { sql, params, returns } => {
                if binds_principal(params) && req.principal.root_is_anonymous() {
                    return Ok(anonymous_principal_refused());
                }
                #[cfg(feature = "sqlite")]
                {
                    let db = self.db.as_ref().ok_or("no database configured")?;
                    let route = req.route_id.and_then(|id| self.routes_by_id.get(&id));
                    if let Some(refused) = route.and_then(|r| body_refusal(r, &req)) {
                        return Ok(refused);
                    }
                    let bindings = match req.bind(params) {
                        Ok(b) => b,
                        Err(e) => return Ok(WebCortexResponse::coded(400, "WC205", e)),
                    };
                    let mut value = match db.run(sql, &bindings, *returns).await {
                        Ok(v) => v,
                        Err(e) => return Ok(WebCortexResponse::coded(e.status(), e.code(), e.message())),
                    };
                    if value.is_null() && *returns == crate::manifest::QueryReturns::One {
                        return Ok(WebCortexResponse::coded(404, "WC207", "not found"));
                    }
                    if let Some(schema) = route.and_then(|r| r.output_schema.as_ref()) {
                        restore_booleans(&mut value, schema);
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
                Ok(server.serve(&relative, &req.path, req.headers.get("if-none-match").map(|s| s.as_str())))
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

            Op::Device { device, action } => self.serve_device(device, *action, req).await,
        }
    }

    async fn serve_device(
        &self,
        name: &str,
        action: crate::manifest::DeviceAction,
        req: WebCortexRequest,
    ) -> Result<WebCortexResponse, String> {
        use crate::manifest::DeviceAction as A;
        let device = self
            .devices
            .get(name)
            .ok_or_else(|| format!("unknown device {name:?}"))?;
        let html = |page: &str| WebCortexResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
            body: bytes::Bytes::from(page.replace("__DEVICE__", &crate::devices::escape_html(name))),
        };
        Ok(match action {
            A::Snapshot => match self.devices.snapshot(device).await {
                Ok(v) => WebCortexResponse::json(200, &v),
                Err(e) if device.def.source.is_some() => WebCortexResponse::coded(502, "WC506", e),
                Err(e) => WebCortexResponse::coded(404, "WC503", e),
            },
            A::Image => match self.devices.current_frame(device).await {
                Ok(Some(frame)) => WebCortexResponse {
                    status: 200,
                    headers: vec![
                        ("content-type".into(), frame.media_type.into()),
                        ("cache-control".into(), "no-store".into()),
                        ("x-webcortex-seq".into(), frame.seq.to_string()),
                    ],
                    body: frame.data.clone(),
                },
                Ok(None) => WebCortexResponse::coded(404, "WC503", format!("device {name:?} has not sent a frame yet")),
                Err(e) => WebCortexResponse::coded(502, "WC506", e),
            },
            A::Telemetry => {
                let t = device.telemetry();
                WebCortexResponse::json(200, &serde_json::json!({
                    "device": name,
                    "telemetry": t.as_ref().map(|(_, v)| (**v).clone()),
                    "age_ms": t.map(|(at, _)| crate::devices::now_ms().saturating_sub(at)),
                }))
            }
            A::Insights => WebCortexResponse::json(
                200,
                &serde_json::json!({"device": name, "insights": device.insights()}),
            ),
            A::IngestFrame => match device.push_frame(req.body.clone()) {
                Ok(seq) => WebCortexResponse::json(200, &serde_json::json!({"device": name, "seq": seq})),
                Err(e) => WebCortexResponse::coded(e.status(), e.code(), e.message()),
            },
            A::IngestTelemetry => {
                let Ok(value) = req.json_body() else {
                    return Ok(WebCortexResponse::coded(400, "WC505", "telemetry must be a JSON body"));
                };
                match device.push_telemetry(value) {
                    Ok(()) => WebCortexResponse::json(200, &serde_json::json!({"device": name, "ok": true})),
                    Err(e) => WebCortexResponse::coded(e.status(), e.code(), e.message()),
                }
            }
            A::SocketIngest | A::SocketStream => {
                let mut res = WebCortexResponse::coded(426, "WC213", "this route is a WebSocket; connect with Upgrade: websocket");
                res.headers.push(("upgrade".into(), "websocket".into()));
                res
            }
            A::ConnectPage => html(crate::devices::CONNECT_PAGE),
            A::ViewPage => html(crate::devices::VIEW_PAGE),
        })
    }

    async fn run_flow(&self, name: &str, req: WebCortexRequest) -> Result<WebCortexResponse, String> {
        let def = self
            .manifest
            .flow(name)
            .ok_or_else(|| format!("unknown flow {name:?}"))?
            .clone();
        let input = req.json_body().unwrap_or(serde_json::Value::Null);
        let span = self.span_for(&req, "flow", &format!("flow {}", def.name));
        // A request timeout or a client that leaves drops this future before
        // the flow returns; the guard then ends the span as cancelled. 2.8.0
        // left it open for good, and it was never exported.
        let mut guard = SpanGuard { tracer: &self.tracer, span: span.clone(), outcome: None };
        let outcome = crate::flow::run(self, &def, input, &req.principal, req.depth, req.budget.clone(), span.clone()).await;
        // Finished: ended below, with the flow's own attributes.
        guard.span = None;
        let mut res = match outcome {
            Ok(mut value) => {
                if let Some(span) = &span {
                    self.tracer.end(span, crate::trace::SpanStatus::Ok, None, vec![
                        ("webcortex.flow.name", serde_json::json!(def.name)),
                        ("webcortex.run.status", value["status"].clone()),
                        ("webcortex.run.tokens", value["usage"]["tree_tokens"].clone()),
                    ]);
                    value["trace_id"] = serde_json::json!(&*span.trace_id);
                }
                WebCortexResponse::json(200, &value)
            }
            Err(e) => {
                let code = if e.contains("token budget") { "WC402" } else { "WC315" };
                if let Some(span) = &span {
                    self.tracer.end(span, crate::trace::SpanStatus::Error, Some(code), vec![
                        ("webcortex.flow.name", serde_json::json!(def.name)),
                    ]);
                }
                WebCortexResponse::coded(500, code, e)
            }
        };
        trace_header(&mut res, span.as_ref(), req.depth);
        Ok(res)
    }

    /// Refuse an invocation nested deeper than the configured ceiling.
    ///
    /// Checked for agent, behaviour and flow ops, because those are the ones
    /// that can re-enter the dispatcher and form a cycle.
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
        Some(WebCortexResponse::coded(
            508,
            "WC403",
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
        let span = self.span_for(&req, "behaviour", &format!("behaviour {}", def.name));
        // Ended here whichever way the behaviour ends, including a request
        // timeout that drops this future mid-run.
        let mut guard = SpanGuard { tracer: &self.tracer, span: span.clone(), outcome: None };
        let outcome = self
            .bridge
            .call_behaviour(app, def.clone(), input, actor.clone(), req.depth + 1, budget, span.clone())
            .await;
        match outcome {
            Ok(value) => {
                guard.outcome = Some(None);
                drop(guard);
                let mut res = WebCortexResponse::json(200, &value);
                trace_header(&mut res, span.as_ref(), req.depth);
                Ok(res)
            }
            Err(e) => {
                self.audit.record(crate::audit::AuditEvent {
                    kind: "behaviour_failed".into(),
                    run_id: String::new(),
                    actor: Some(actor.id),
                    tool: Some(name.to_string()),
                    detail: serde_json::json!({"error": &e}),
                });
                // A failure the bridge already coded ("WC406: …") keeps its code.
                let code = crate::errors::code_in(e.get(..6).unwrap_or_default())
                    .filter(|_| e.as_bytes().get(5) == Some(&b':'))
                    .unwrap_or("WC003");
                guard.outcome = Some(Some(code));
                drop(guard);
                let mut res = WebCortexResponse::coded(500, code, e);
                trace_header(&mut res, span.as_ref(), req.depth);
                Ok(res)
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
                if binds_principal(params) && req.principal.root_is_anonymous() {
                    return Ok(anonymous_principal_refused());
                }
                #[cfg(feature = "sqlite")]
                {
                    let db = self.db.as_ref().ok_or("no database configured")?;
                    let bindings = match req.bind(params) {
                        Ok(b) => b,
                        Err(e) => return Ok(WebCortexResponse::coded(400, "WC205", e)),
                    };
                    let value = match db.run(sql, &bindings, *returns).await {
                        Ok(v) => v,
                        Err(e) => return Ok(WebCortexResponse::coded(e.status(), e.code(), e.message())),
                    };
                    if value.is_null() && *returns == crate::manifest::QueryReturns::One {
                        return Ok(WebCortexResponse::coded(404, "WC207", "not found"));
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
            return Ok(WebCortexResponse::coded(503, "WC215", "agent runtime is unavailable"));
        };
        let def = self
            .agent(name)
            .ok_or_else(|| format!("unknown agent {name:?}"))?;

        let Some(input) = req.lookup("input").and_then(|v| v.as_str().map(str::to_string)) else {
            return Ok(WebCortexResponse::coded(
                400,
                "WC306",
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
        // Every anonymous caller is the same `anonymous` principal, so a session
        // keyed by it would be one conversation shared by strangers.
        if session_id.is_some() && req.principal.root_is_anonymous() {
            return Ok(WebCortexResponse::coded(
                401,
                "WC607",
                "sessions need an authenticated caller: anonymous callers share one identity, \
                 so they would share one conversation",
            ));
        }

        let images = match crate::agent::vision::input_images(req.lookup("images").as_ref()) {
            Ok(images) => images,
            Err(e) => return Ok(WebCortexResponse::coded(400, "WC313", e)),
        };

        let opts = RunOptions {
            depth: req.depth,
            budget: req.budget.clone(),
            session_id,
            reset_session,
            images,
            events: None,
            traceparent: if req.trace.is_none() { req.headers.get("traceparent").cloned() } else { None },
            trace: req.trace.clone(),
        };
        let result = runtime.run(self, def, &req.principal, &input, opts).await;
        let mut res = run_result_response(&result);
        if req.depth == 0
            && let Some(id) = &result.trace_id
        {
            res.headers.push(("x-webcortex-trace".into(), id.clone()));
        }
        Ok(res)
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
                return Ok(WebCortexResponse::coded(400, "WC209", "invalid path parameter"));
            }
        }

        let tail = match rewrite {
            Some(t) => substitute_path(t, &req.path_params),
            None => req.path.clone(),
        };

        // Belt and braces: even with clean parameters, the assembled tail must
        // not contain a traversal segment.
        if tail.split('/').any(|seg| seg == ".." || seg == ".") {
            return Ok(WebCortexResponse::coded(400, "WC209", "invalid upstream path"));
        }

        let url = format!("{}{}", up.base_url.trim_end_matches('/'), tail);

        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| format!("bad method: {e}"))?;
        let mut builder = self
            .proxy_client
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
        let pairs = req.query_pairs();
        if !pairs.is_empty() {
            builder = builder.query(&pairs);
        }
        if !req.body.is_empty() {
            builder = builder.body(req.body.clone());
        }

        let res = builder
            .send()
            .await
            .map_err(|e| {
                // The error names the upstream URL; that stays in the log.
                tracing::warn!(upstream = %upstream_name, error = %e, "upstream request failed");
                format!("upstream {upstream_name} failed: {}", e.without_url())
            })?;

        let status = res.status().as_u16();
        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        // A redirect is the caller's to follow, not ours: 2.8.0 followed it to
        // any host — loopback, a metadata address — sending the upstream's
        // static headers and a Referer there, and relayed what it found. One
        // pointing back into the upstream is rewritten to the path a caller
        // would use here, so the upstream's own address is not handed out.
        let location = res.status().is_redirection().then(|| {
            res.headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|loc| relay_location(&up.base_url, res.url(), loc, &self.proxy_mounts(upstream_name, req.route_id)))
        }).flatten();
        // Buffered, so bounded: a declared length over the cap is refused
        // before reading, and an undeclared one is cut off once it passes it.
        let cap = up.max_response_bytes;
        let too_large = || {
            tracing::warn!(upstream = %upstream_name, max_response_bytes = cap,
                           "upstream response over max_response_bytes; not relayed");
            Ok(WebCortexResponse::coded(
                502,
                "WC217",
                format!("upstream {upstream_name} answered with more than {cap} bytes"),
            ))
        };
        if res.content_length().is_some_and(|n| n > cap as u64) {
            return too_large();
        }
        let mut res = res;
        let mut buf = bytes::BytesMut::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|e| format!("upstream {upstream_name} body read failed: {}", e.without_url()))?
        {
            if buf.len() + chunk.len() > cap {
                return too_large();
            }
            buf.extend_from_slice(&chunk);
        }
        let body = buf.freeze();

        let mut headers = vec![("content-type".into(), content_type)];
        if let Some(location) = location {
            headers.push(("location".into(), location));
        }
        Ok(WebCortexResponse { status, headers, body })
    }

    /// How this app's proxy routes map onto `upstream`, as (route path,
    /// upstream path template) pairs: the route that made the request first,
    /// then the upstream's other GET routes. Used to map a redirect back.
    fn proxy_mounts(&self, upstream: &str, current: Option<u32>) -> Vec<(&str, &str)> {
        fn mount<'r>(r: &'r Route, upstream: &str) -> Option<(&'r str, &'r str)> {
            match &r.op {
                Op::Proxy { upstream: u, rewrite } if u == upstream => {
                    Some((r.path.as_str(), rewrite.as_deref().unwrap_or(r.path.as_str())))
                }
                _ => None,
            }
        }
        let mut mounts: Vec<(&str, &str)> =
            current.and_then(|id| self.routes_by_id.get(&id)).and_then(|r| mount(r, upstream)).into_iter().collect();
        for r in &self.manifest.routes {
            if Some(r.id) != current && r.method.eq_ignore_ascii_case("GET")
                && let Some(m) = mount(r, upstream)
            {
                mounts.push(m);
            }
        }
        mounts
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
        self.call_tool_traced(tool_name, args, principal, depth, budget, None)
    }

    /// As [`Self::call_tool_in_tree`], naming the span the call is made under
    /// so a run it starts joins the caller's trace.
    pub fn call_tool_traced<'a>(
        &'a self,
        tool_name: &'a str,
        args: &'a serde_json::Value,
        principal: &'a Principal,
        depth: u32,
        budget: Option<Arc<SharedBudget>>,
        trace: Option<crate::trace::TraceCtx>,
    ) -> futures::future::BoxFuture<'a, Result<serde_json::Value, String>> {
        Box::pin(async move {
            let res = self.tool_response(tool_name, args, principal, depth, budget, trace).await?;
            if res.status >= 400 {
                return Err(format!(
                    "tool {tool_name} failed with {}: {}",
                    res.status,
                    String::from_utf8_lossy(&res.body)
                ));
            }
            Ok(res.json_value())
        })
    }

    /// Call a tool in-process and return the response it produced, whatever
    /// its status: what [`Self::call_tool_traced`] turns into a value or an
    /// error string, kept whole for a caller that needs the body of a failure
    /// (an eval reading the envelope of a failed run). `Err` is only for a
    /// call that could not be made: an unknown tool, a missing path argument.
    pub async fn tool_response(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        principal: &Principal,
        depth: u32,
        budget: Option<Arc<SharedBudget>>,
        trace: Option<crate::trace::TraceCtx>,
    ) -> Result<WebCortexResponse, String> {
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
            let Some(name) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
                continue;
            };
            let value = obj.get(name).filter(|v| !v.is_null()).map(json_to_path_string);
            match value {
                Some(v) if !v.is_empty() => {
                    path_params.insert(name.to_string(), v);
                }
                // Without this, `/items/{id}` became `/items/` and the call
                // failed as "no route" (WC201), which sends a model looking
                // for a route that exists instead of the argument it left out.
                _ => {
                    let res = WebCortexResponse::coded(
                        422,
                        "WC206",
                        format!("missing required argument {name:?} (a path parameter of {} {})", route.method, route.path),
                    );
                    return Err(format!(
                        "tool {tool_name} failed with {}: {}",
                        res.status,
                        String::from_utf8_lossy(&res.body)
                    ));
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
        let mut query_all = Vec::new();
        if body_less {
            for (k, v) in &obj {
                if !path_params.contains_key(k) {
                    query.insert(k.clone(), json_to_path_string(v));
                    // A list argument is the repeated key an HTTP caller would send.
                    match v {
                        serde_json::Value::Array(items) => {
                            query_all.extend(items.iter().map(|i| (k.clone(), json_to_path_string(i))))
                        }
                        _ => query_all.push((k.clone(), json_to_path_string(v))),
                    }
                }
            }
        }

        let req = WebCortexRequest {
            method: route.method.clone(),
            path: concrete_path,
            path_params,
            query,
            query_all,
            headers: Default::default(),
            body,
            route_id: Some(route_id),
            principal: principal.clone(),
            depth,
            budget,
            trace,
        };

        Ok(self.dispatch(req).await)
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

/// Ends a span when dropped: as given by `outcome` (`Some(None)` ok,
/// `Some(Some(code))` failed), or as cancelled when the future holding it was
/// dropped first.
struct SpanGuard<'a> {
    tracer: &'a crate::trace::Tracer,
    span: Option<crate::trace::TraceCtx>,
    outcome: Option<Option<&'static str>>,
}

impl Drop for SpanGuard<'_> {
    fn drop(&mut self) {
        if let Some(span) = &self.span {
            let (status, code) = match self.outcome {
                Some(None) => (crate::trace::SpanStatus::Ok, None),
                Some(Some(code)) => (crate::trace::SpanStatus::Error, Some(code)),
                None => (crate::trace::SpanStatus::Cancelled, None),
            };
            self.tracer.end(span, status, code, vec![]);
        }
    }
}

/// Tell an HTTP caller which trace its run produced. Only at the edge: a
/// nested call's response is a tool result, not something a client reads.
fn trace_header(res: &mut WebCortexResponse, span: Option<&crate::trace::TraceCtx>, depth: u32) {
    if depth == 0
        && let Some(span) = span
    {
        res.headers.push(("x-webcortex-trace".into(), span.trace_id.to_string()));
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
        Ok(v) => {
            let mut res = WebCortexResponse::json(status, &v);
            // A failed run is an error response like any other: the header
            // carries its code, so the request log and the status view record
            // the run's own code (WC304, …) rather than the generic one for 502.
            if status >= 400
                && let Some(code) = result.code.as_deref()
            {
                res.headers.push(("x-webcortex-error".into(), code.to_string()));
            }
            res
        }
        Err(e) => WebCortexResponse::coded(500, "WC001", e.to_string()),
    }
}

/// 401 (nobody authenticated) or 403 (a known caller without the scope), with
/// a hint naming the scopes to hold — and, when the caller is an agent or a
/// behaviour acting for someone, which delegate lacks them and where to add
/// them, rather than blaming a key that may well hold them.
pub fn scope_refusal_for(principal: &Principal, missing: &[String]) -> WebCortexResponse {
    let status = if principal.is_anonymous() { 401 } else { 403 };
    let code = if status == 401 { "WC601" } else { "WC602" };
    let hint = scope_hint(principal, missing);
    WebCortexResponse::coded_with_hint(status, code, format!("missing required scope(s): {}", missing.join(", ")), &hint)
}

/// What to do about `missing` scopes, for this principal.
pub fn scope_hint(principal: &Principal, missing: &[String]) -> String {
    let quoted = missing.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(", ");
    if principal.is_anonymous() {
        return format!(
            "Authenticate: send an API key (x-api-key) or a bearer token whose scopes include {}.",
            missing.join(", ")
        );
    }
    if principal.kind == crate::auth::PrincipalKind::Agent {
        let agent = principal.claims.get("agent").and_then(|v| v.as_str()).unwrap_or("?");
        let held = if principal.scopes.is_empty() { "none".to_string() } else { principal.scopes.join(", ") };
        return format!(
            "{agent:?} acts with scopes [{held}]: its own scopes= intersected with its caller's. \
             Add {quoted} to {agent:?}'s scopes= (the caller must hold them too)."
        );
    }
    format!(
        "This caller lacks {}. Use a key registered with app.api_key(..., scopes=[{quoted}]).",
        missing.join(", ")
    )
}

/// The code for an op that failed with an `Err` string rather than a response.
fn op_failure_code(op: &Op, message: &str) -> &'static str {
    if let Some(code) = crate::errors::code_in(message) {
        return code;
    }
    match op {
        Op::Python { .. } => "WC002",
        Op::Behaviour { .. } => "WC003",
        Op::Page { .. } if message.contains("database") => "WC105",
        Op::Page { .. } => "WC004",
        Op::Query { .. } if message.contains("database") => "WC105",
        Op::Proxy { .. } => "WC210",
        Op::Flow { .. } => "WC315",
        Op::Agent { .. } => "WC300",
        Op::Device { .. } if message.contains("unknown device") => "WC501",
        _ => "WC001",
    }
}

/// The `Location` to relay for an upstream redirect. One that resolves under
/// the upstream's `base_url` is mapped back through the app's proxy routes
/// (`mounts`, see [`App::proxy_mounts`]) to the path a caller uses here; one
/// under the base that no route maps becomes the path beneath the base; any
/// other is relayed as the upstream sent it, for the caller to judge.
fn relay_location(base_url: &str, requested: &reqwest::Url, location: &str, mounts: &[(&str, &str)]) -> String {
    let base = base_url.trim_end_matches('/');
    let Ok(resolved) = requested.join(location) else { return location.to_string() };
    let resolved = resolved.as_str();
    let rest = match resolved.strip_prefix(base) {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '?', '#']) => rest,
        _ => return location.to_string(),
    };
    let (path, suffix) = rest.split_at(rest.find(['?', '#']).unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    for (route_path, upstream_template) in mounts {
        // A rewrite may spell the route's catch-all `{rest}` rather than
        // `{*rest}`; it carries the rest of the path all the same.
        let template: String = upstream_template
            .split('/')
            .map(|seg| match seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Some(n) if !n.starts_with('*') && route_path.contains(&format!("{{*{n}}}")) => format!("{{*{n}}}"),
                _ => seg.to_string(),
            })
            .collect::<Vec<_>>()
            .join("/");
        if let Some(params) = match_template(&template, path) {
            return format!("{}{suffix}", fill_template(route_path, &params));
        }
    }
    format!("{path}{suffix}")
}

/// The parameters `path` binds when it matches a route-style `template`
/// (`{name}` takes one segment, `{*name}` the rest), or `None`. Values are
/// kept as they appear in the path, still percent-encoded.
fn match_template(template: &str, path: &str) -> Option<std::collections::BTreeMap<String, String>> {
    let mut want = template.split('/').filter(|s| !s.is_empty());
    let got: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut params = std::collections::BTreeMap::new();
    let mut i = 0;
    while let Some(seg) = want.next() {
        let name = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}'));
        match name {
            Some(n) if n.starts_with('*') => {
                if want.next().is_some() {
                    return None;
                }
                params.insert(n.trim_start_matches('*').to_string(), got.get(i..).unwrap_or_default().join("/"));
                return Some(params);
            }
            Some(n) => {
                params.insert(n.to_string(), (*got.get(i)?).to_string());
            }
            None if got.get(i) == Some(&seg) => {}
            None => return None,
        }
        i += 1;
    }
    (i == got.len()).then_some(params)
}

/// `template` with each `{name}` / `{*name}` replaced by its value as given.
fn fill_template(template: &str, params: &std::collections::BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for seg in template.split('/').filter(|s| !s.is_empty()) {
        let value = match seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(n) => params.get(n.trim_start_matches('*')).map(String::as_str).unwrap_or_default(),
            None => seg,
        };
        if !value.is_empty() {
            out.push('/');
            out.push_str(value);
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// The 422 for a native query route's body that does not match the route's
/// `input_schema`, or `None` when it matches (or there is nothing to check).
///
/// Only routes that take a body (POST, PUT, PATCH) are checked, and only the
/// body: a parameter the path or query string supplies counts as present and
/// is not checked here (binding takes that value, and a path value is text
/// until it is bound), even if the body repeats it. Through 2.8.0 the
/// advertised schema was never applied: `{}` inserted a row of NULLs, a
/// misspelt key on PUT wiped every field, and `"lol"` was stored in a REAL
/// column.
fn body_refusal(route: &Route, req: &WebCortexRequest) -> Option<WebCortexResponse> {
    let schema = route.input_schema.as_ref()?;
    if !matches!(req.method.to_ascii_uppercase().as_str(), "POST" | "PUT" | "PATCH") {
        return None;
    }
    let from_url = |name: &str| req.path_params.contains_key(name) || req.query.contains_key(name);
    let body = if req.body.is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        match serde_json::from_slice::<serde_json::Value>(&req.body) {
            Ok(serde_json::Value::Object(mut map)) => {
                map.retain(|k, _| !from_url(k));
                serde_json::Value::Object(map)
            }
            // Not an object, or not JSON: binding refuses it with WC205.
            _ => return None,
        }
    };
    let mut schema = schema.clone();
    if let Some(serde_json::Value::Array(required)) = schema.get_mut("required") {
        required.retain(|name| name.as_str().is_none_or(|n| !from_url(n)));
    }
    let problems = crate::schema::validate(&schema, &body);
    if problems.is_empty() {
        return None;
    }
    let message = format!(
        "the request body does not match the input schema of {} {}: {}",
        route.method,
        route.path,
        problems.join("; ")
    );
    let mut res = WebCortexResponse::json(
        422,
        &serde_json::json!({"error": {
            "status": 422, "message": message, "code": "WC206",
            "hint": crate::errors::hint("WC206"), "problems": problems,
        }}),
    );
    res.headers.push(("x-webcortex-error".into(), "WC206".into()));
    Some(res)
}

/// Turn the 0 and 1 SQLite stores for a boolean back into `false` and `true`
/// wherever `schema` (a route's `output_schema`) declares a boolean: a row,
/// or each row of a list. SQLite has no boolean type, so a resource's `bool`
/// field read back as an integer, contradicting its own schema.
fn restore_booleans(value: &mut serde_json::Value, schema: &serde_json::Value) {
    let declares = |s: &serde_json::Value, t: &str| match s.get("type") {
        Some(serde_json::Value::String(x)) => x == t,
        Some(serde_json::Value::Array(xs)) => xs.iter().any(|x| x == t),
        _ => false,
    };
    match value {
        serde_json::Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for item in items {
                    restore_booleans(item, item_schema);
                }
            }
        }
        serde_json::Value::Object(map) => {
            let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else { return };
            for (key, field) in map.iter_mut() {
                let Some(sub) = props.get(key) else { continue };
                if declares(sub, "boolean") {
                    match field.as_i64() {
                        Some(0) => *field = serde_json::Value::Bool(false),
                        Some(1) => *field = serde_json::Value::Bool(true),
                        _ => {}
                    }
                } else {
                    restore_booleans(field, sub);
                }
            }
        }
        _ => {}
    }
}

/// Whether a declared query binds the caller's identity.
fn binds_principal(params: &[String]) -> bool {
    params.iter().any(|p| p == "@principal")
}

/// Every anonymous caller shares the id `anonymous`, so a route scoped to
/// `@principal` would pool their data. It needs a real caller instead.
fn anonymous_principal_refused() -> WebCortexResponse {
    WebCortexResponse::coded(
        401,
        "WC607",
        "this route is scoped to the caller (@principal) and needs an authenticated caller",
    )
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
    use crate::http::percent_decode;
    percent_decode(&percent_decode(s, false), false)
}

fn json_to_path_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Characters percent-encoded when a value is spliced into one path segment:
/// everything outside RFC 3986 `pchar`, plus `;` (a path-parameter delimiter
/// to some servers). `?`, `#` and `/` above all — a decoded `%3F` substituted
/// raw would start a query on the upstream, and a `/` would add a segment.
const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b';')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Fill a route template's `{name}` segments with values, each encoded so it
/// stays exactly one path segment.
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
            Some(name) => {
                let value = params.get(name).map(|s| s.as_str()).unwrap_or("");
                out.extend(percent_encoding::utf8_percent_encode(value, PATH_SEGMENT));
            }
            None => out.push_str(seg),
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_substituted_value_stays_one_path_segment() {
        let mut params = std::collections::BTreeMap::new();
        params.insert("slug".to_string(), "x?admin=1#frag".to_string());
        assert_eq!(substitute_path("/echo/{slug}", &params), "/echo/x%3Fadmin=1%23frag");
        params.insert("slug".to_string(), "a/b c;d%".to_string());
        assert_eq!(substitute_path("/echo/{slug}", &params), "/echo/a%2Fb%20c%3Bd%25");
        params.insert("slug".to_string(), "Frank-Herbert_1.0~:@".to_string());
        assert_eq!(substitute_path("/echo/{slug}", &params), "/echo/Frank-Herbert_1.0~:@");
    }

    #[test]
    fn an_upstream_redirect_into_the_upstream_is_rewritten_and_any_other_relayed() {
        let at = reqwest::Url::parse("http://10.0.0.5:8080/v2/login?x=1").unwrap();
        let base = "http://10.0.0.5:8080/v2/";
        // Mapped back through the app's proxy routes where one matches.
        let mounts = [("/billing/login", "/login"), ("/billing/{*rest}", "/{rest}"), ("/inv/{id}", "/invoices/{id}")];
        assert_eq!(relay_location(base, &at, "http://10.0.0.5:8080/v2/home?a=b", &mounts), "/billing/home?a=b");
        assert_eq!(relay_location(base, &at, "dashboard/x%20y", &mounts), "/billing/dashboard/x%20y");
        assert_eq!(relay_location(base, &at, "/v2/login", &mounts), "/billing/login");
        let only_invoices = [("/inv/{id}", "/invoices/{id}")];
        assert_eq!(relay_location(base, &at, "/v2/invoices/42#top", &only_invoices), "/inv/42#top");
        // Under the base but no route maps it: the path beneath the base.
        assert_eq!(relay_location(base, &at, "/v2/invoices/42/pdf", &only_invoices), "/invoices/42/pdf");
        assert_eq!(relay_location(base, &at, "/v2", &[]), "/");
        // Not under the base path: a sibling prefix, another host, metadata.
        assert_eq!(relay_location(base, &at, "/v20/x", &mounts), "/v20/x");
        assert_eq!(relay_location(base, &at, "http://169.254.169.254/latest", &mounts), "http://169.254.169.254/latest");
        assert_eq!(relay_location(base, &at, "https://example.com/", &mounts), "https://example.com/");
    }

    #[test]
    fn templates_match_and_fill_like_routes() {
        let m = |t: &str, p: &str| match_template(t, p).map(|m| m.into_iter().collect::<Vec<_>>());
        assert_eq!(m("/a/{id}", "/a/7"), Some(vec![("id".into(), "7".into())]));
        assert_eq!(m("/a/{id}", "/a/7/8"), None);
        assert_eq!(m("/a/{id}", "/b/7"), None);
        assert_eq!(m("/a/{*rest}", "/a/x/y"), Some(vec![("rest".into(), "x/y".into())]));
        assert_eq!(m("/a/{*rest}", "/a"), Some(vec![("rest".into(), "".into())]));
        assert_eq!(m("/", "/"), Some(vec![]));
        let p: std::collections::BTreeMap<String, String> = [("rest".to_string(), "x/y".to_string())].into();
        assert_eq!(fill_template("/b/{*rest}", &p), "/b/x/y");
        assert_eq!(fill_template("/b/{*rest}", &Default::default()), "/b");
    }

    async fn resource_app() -> App {
        let manifest: Manifest = serde_json::from_value(json!({
            "name": "r",
            "database": {"url": "sqlite://:memory:", "schema": "CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, name TEXT, price REAL, ok INTEGER)"},
            "routes": [
                {"id": 0, "method": "POST", "path": "/items",
                 "op": {"kind": "query", "sql": "INSERT INTO items (name, price, ok) VALUES (?, ?, ?) RETURNING *",
                        "params": ["name", "price", "ok"], "returns": "one"},
                 "input_schema": {"type": "object", "additionalProperties": false, "required": ["name", "price", "ok"],
                                  "properties": {"name": {"type": "string"}, "price": {"type": "number"}, "ok": {"type": "boolean"}}},
                 "output_schema": {"type": "object", "properties": {"id": {"type": "integer"}, "name": {"type": "string"},
                                   "price": {"type": "number"}, "ok": {"type": "boolean"}}}},
                {"id": 1, "method": "PUT", "path": "/items/{id}",
                 "op": {"kind": "query", "sql": "UPDATE items SET name = ?, price = ?, ok = ? WHERE id = ? RETURNING *",
                        "params": ["name", "price", "ok", "id"], "returns": "one"},
                 "input_schema": {"type": "object", "additionalProperties": false, "required": ["id", "name", "price", "ok"],
                                  "properties": {"id": {"type": "integer"}, "name": {"type": "string"}, "price": {"type": "number"},
                                                 "ok": {"type": "boolean"}}},
                 "output_schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}}},
                {"id": 2, "method": "GET", "path": "/items",
                 "op": {"kind": "query", "sql": "SELECT * FROM items", "params": [], "returns": "many"},
                 "output_schema": {"type": "array", "items": {"type": "object", "properties": {"ok": {"type": "boolean"}}}}}
            ]
        }))
        .unwrap();
        App::build_without_python(manifest).await.expect("app")
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_query_routes_body_is_held_to_its_input_schema_and_booleans_read_back() {
        let app = resource_app().await;
        let send = |method: &str, path: &str, body: serde_json::Value| WebCortexRequest {
            method: method.into(),
            path: path.into(),
            path_params: Default::default(),
            query: Default::default(),
            query_all: Vec::new(),
            headers: Default::default(),
            body: if body.is_null() { bytes::Bytes::new() } else { bytes::Bytes::from(serde_json::to_vec(&body).unwrap()) },
            route_id: None,
            principal: Principal::anonymous(),
            depth: 0,
            budget: None,
            trace: None,
        };
        // Through 2.8.0 each of these was stored: NULLs, text in a REAL
        // column, a misspelt key silently ignored.
        for (body, needle) in [
            (json!({}), "missing required property \"name\""),
            (json!({"name": "a", "price": "abc", "ok": true}), "$.price: expected number"),
            (json!({"name": "a", "price": 1, "ok": true, "colour": "red"}), "colour"),
            (json!({"name": "a", "price": 1, "ok": "yes"}), "$.ok: expected boolean"),
        ] {
            let res = app.dispatch(send("POST", "/items", body)).await;
            assert_eq!(res.status, 422, "{}", String::from_utf8_lossy(&res.body));
            let v = res.json_value();
            assert_eq!(v["error"]["code"], "WC206");
            assert!(v["error"]["message"].as_str().unwrap().contains(needle), "{v}");
            assert!(!v["error"]["problems"].as_array().unwrap().is_empty());
        }
        let res = app.dispatch(send("POST", "/items", json!({"name": "a", "price": 1.5, "ok": true}))).await;
        assert_eq!(res.status, 200);
        assert_eq!(res.json_value()["ok"], json!(true), "a stored 1 reads back as true");
        // The path supplies the id; the body need not repeat it, and a typo is refused.
        let res = app.dispatch(send("PUT", "/items/1", json!({"name": "b", "price": 2, "ok": false}))).await;
        assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
        assert_eq!(res.json_value()["ok"], json!(false));
        let res = app.dispatch(send("PUT", "/items/1", json!({"titel": "b", "price": 2, "ok": false}))).await;
        assert_eq!(res.status, 422);
        // A body that repeats what the path supplies is not held to it: the
        // path's value is the one bound.
        let res = app.dispatch(send("PUT", "/items/1", json!({"id": "1", "name": "b", "price": 2, "ok": false}))).await;
        assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
        let res = app.dispatch(send("GET", "/items", serde_json::Value::Null)).await;
        assert_eq!(res.json_value(), json!([{"id": 1, "name": "b", "price": 2.0, "ok": false}]));
    }

    async fn notes_app() -> App {
        let manifest: Manifest = serde_json::from_value(json!({
            "name": "t",
            "routes": [
                {"id": 0, "method": "POST", "path": "/notes/{id}",
                 "op": {"kind": "static", "body": {"touched": true}},
                 "tool": {"expose": true, "name": "touch_note"}},
                {"id": 1, "method": "POST", "path": "/notes/purge",
                 "op": {"kind": "static", "body": {"purged": true}},
                 "tool": {"expose": true, "name": "purge"},
                 "approval": "required"},
                {"id": 2, "method": "POST", "path": "/notes/{id}/archive",
                 "op": {"kind": "static", "body": {"archived": true}},
                 "tool": {"expose": true, "name": "archive"},
                 "approval": "required"}
            ]
        }))
        .expect("manifest");
        App::build_without_python(manifest).await.expect("app builds")
    }

    #[tokio::test]
    async fn a_tool_argument_cannot_steer_the_call_onto_a_gated_route() {
        let app = notes_app().await;
        let who = Principal::anonymous();
        // `purge` is gated; reaching it through the ungated tool's argument
        // would skip the human the gate exists for.
        let err = app.call_tool_as("touch_note", &json!({"id": "purge"}), &who).await.unwrap_err();
        assert!(err.contains("do not address this tool's route"), "{err}");
        // A slash cannot add a segment and land on `/notes/{id}/archive` either.
        let out = app.call_tool_as("touch_note", &json!({"id": "1/archive"}), &who).await.unwrap();
        assert_eq!(out["touched"], true);
        let out = app.call_tool_as("touch_note", &json!({"id": "a b?c#d"}), &who).await.unwrap();
        assert_eq!(out["touched"], true);
    }
}
