//! The manifest is the contract between the Python declaration layer and the
//! Rust runtime.
//!
//! Python code is *not* the server. Python runs once at startup, describes the
//! application as data, and hands that data to Rust. Rust compiles it into a
//! router plus a set of executable ops. Only ops of kind `Python` ever re-enter
//! the interpreter; everything else is served without touching the GIL.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub database: Option<DatabaseConfig>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub upstreams: BTreeMap<String, Upstream>,
    #[serde(default)]
    pub agents: Vec<AgentDef>,
    #[serde(default)]
    pub behaviours: Vec<BehaviourDef>,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub cors: CorsConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub security_headers: SecurityHeaders,
    #[serde(default)]
    pub templates: Option<TemplateConfig>,
    /// Model aliases, provider endpoints and pricing. See [`ModelsConfig`].
    #[serde(default)]
    pub models: ModelsConfig,
    /// Named context providers agents and behaviours can be given at run start.
    #[serde(default)]
    pub contexts: Vec<ContextDef>,
    /// Declarative orchestrations executed entirely in Rust.
    #[serde(default)]
    pub flows: Vec<FlowDef>,
}

// ---------------------------------------------------------------------------
// Models: aliases, providers, pricing
// ---------------------------------------------------------------------------

/// How a model name resolves to a wire protocol and an endpoint.
///
/// Deliberately not a universal LLM abstraction. There are two wire formats the
/// runtime speaks — Anthropic Messages and OpenAI Chat Completions — and a model
/// name selects one by prefix: `ollama/…`, `openai/…`, or the name of a
/// declared provider. Aliases (`fast`, `default`, `local`) let an application
/// name a *tier* once and change the model behind it in one place.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelsConfig {
    /// `alias -> model name`. `default` and `fast` have built-in values that an
    /// application may override.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// `prefix -> provider`. `ollama` and `openai` are built in.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderDef>,
    /// USD per million tokens, by model name. Absent means "unknown", and the
    /// usage ledger reports tokens only. No prices are built in: they change,
    /// and a stale number is worse than none.
    #[serde(default)]
    pub pricing: BTreeMap<String, Price>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDef {
    /// `"openai"` for any Chat-Completions-compatible server (Ollama, vLLM,
    /// LM Studio, OpenAI itself, Groq, OpenRouter); `"anthropic"` for the
    /// Messages API.
    #[serde(default = "default_provider_kind")]
    pub kind: String,
    pub base_url: String,
    /// Environment variable holding the API key, if the endpoint needs one.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

fn default_provider_kind() -> String {
    "openai".into()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    #[serde(default)]
    pub cache_read_per_mtok: f64,
    #[serde(default)]
    pub cache_write_per_mtok: f64,
}

// ---------------------------------------------------------------------------
// Context: what a run is allowed to carry, and what it is given to start with
// ---------------------------------------------------------------------------

/// Limits on how much conversation a run drags along.
///
/// Tokens are the cost of an agent, and most of them are *re-sent* context:
/// every step replays the whole conversation. These three knobs are where the
/// bulk of a bill is decided.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPolicy {
    /// A tool result larger than this is truncated before the model sees it,
    /// with a marker saying how much was cut. A `list_*` call returning five
    /// hundred rows is the classic way a context window fills in one step.
    #[serde(default = "default_tool_result_bytes")]
    pub max_tool_result_bytes: usize,
    /// When the *measured* input size of the last provider call exceeds this,
    /// older turns are summarised into one message before the next call.
    #[serde(default)]
    pub max_context_tokens: Option<u64>,
    /// Model used for the summary. Defaults to the `fast` alias.
    #[serde(default)]
    pub compact_with: Option<String>,
    /// How many recent messages survive a compaction untouched.
    #[serde(default = "default_keep_recent")]
    pub keep_recent: usize,
}

fn default_tool_result_bytes() -> usize {
    16 * 1024
}
fn default_keep_recent() -> usize {
    6
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            max_tool_result_bytes: default_tool_result_bytes(),
            max_context_tokens: None,
            compact_with: None,
            keep_recent: default_keep_recent(),
        }
    }
}

/// A named source of context, resolved at run start and injected into the
/// system prompt as a delimited block. Declared once, reused by any agent or
/// behaviour that names it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub source: ContextSource,
    /// Upper bound on the rendered size. Context is re-sent on every step, so
    /// this multiplies.
    #[serde(default = "default_context_chars")]
    pub max_chars: usize,
}

fn default_context_chars() -> usize {
    4000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextSource {
    /// A constant.
    Static { value: serde_json::Value },
    /// SQL executed in Rust. Parameters resolve from the run's input.
    Query {
        sql: String,
        #[serde(default)]
        params: Vec<String>,
        #[serde(default)]
        returns: QueryReturns,
    },
    /// A Python function returning any JSON-serialisable value.
    Python { handler: u32 },
}

// ---------------------------------------------------------------------------
// Flows: orchestration as data
// ---------------------------------------------------------------------------

/// A declarative orchestration. Every step is a tool — an agent, a behaviour,
/// another flow, or a plain route — so composition is uniform and every step
/// runs under the same delegated principal, depth ceiling and shared budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub kind: FlowKind,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Budget for the whole flow, including every nested agent and behaviour.
    #[serde(default)]
    pub token_budget: Option<u64>,
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowKind {
    /// Steps run in order; each receives the previous step's output.
    Pipeline { steps: Vec<FlowStep> },
    /// Every branch receives the same input and runs concurrently.
    Parallel {
        branches: Vec<FlowStep>,
        #[serde(default)]
        merge: MergeStrategy,
    },
    /// A model classifies the input into one of the labels; that branch runs.
    Route {
        routes: BTreeMap<String, FlowStep>,
        #[serde(default)]
        default: Option<FlowStep>,
        #[serde(default)]
        classify_with: Option<String>,
        #[serde(default)]
        classify_prompt: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowStep {
    pub tool: String,
    /// Optional argument template. String values of the form `$.a.b` are
    /// resolved from the incoming value; `$` is the whole value; `$input` is
    /// the flow's original input. Absent means "pass the incoming value
    /// through", wrapped as `{"input": …}` when the target is an agent.
    #[serde(default)]
    pub input: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    /// A list of branch outputs, in declaration order.
    #[default]
    Collect,
    /// Branch outputs that are objects are merged into one object; later
    /// branches win on key collisions. Non-object outputs are kept under the
    /// branch's tool name.
    Merge,
}

// ---------------------------------------------------------------------------
// Security configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    /// Maps an environment variable holding a secret to the principal it grants.
    /// Keys live in the environment; the manifest only ever names them.
    #[serde(default)]
    pub api_keys: BTreeMap<String, ApiKeySpec>,
    #[serde(default = "default_api_key_header")]
    pub api_key_header: String,
    #[serde(default)]
    pub jwt: Option<JwtConfig>,
    /// Scopes granted to callers presenting no credential at all. Empty by
    /// default: unauthenticated means unprivileged.
    #[serde(default)]
    pub anonymous_scopes: Vec<String>,
}

fn default_api_key_header() -> String {
    "x-api-key".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeySpec {
    pub id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtConfig {
    /// Environment variable holding the HMAC secret or PEM public key.
    pub secret_env: String,
    #[serde(default = "default_jwt_alg")]
    pub algorithm: String,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default)]
    pub issuer: Option<String>,
    /// Seconds of clock skew tolerated on `exp` and `nbf`.
    ///
    /// Explicit because the underlying library defaults to 60, which silently
    /// keeps a revoked-by-expiry token usable for a full minute. Thirty seconds
    /// still absorbs realistic NTP drift.
    #[serde(default = "default_jwt_leeway")]
    pub leeway_secs: u64,
}

fn default_jwt_leeway() -> u64 {
    30
}

fn default_jwt_alg() -> String {
    "HS256".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allow_origins: Vec<String>,
    #[serde(default = "default_cors_methods")]
    pub allow_methods: Vec<String>,
    #[serde(default = "default_cors_headers")]
    pub allow_headers: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
    #[serde(default = "default_cors_max_age")]
    pub max_age_secs: u64,
}

fn default_cors_methods() -> Vec<String> {
    ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}
fn default_cors_headers() -> Vec<String> {
    ["content-type", "authorization", "x-api-key", "x-request-id"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}
fn default_cors_max_age() -> u64 {
    600
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_origins: Vec::new(),
            allow_methods: default_cors_methods(),
            allow_headers: default_cors_headers(),
            expose_headers: Vec::new(),
            allow_credentials: false,
            max_age_secs: default_cors_max_age(),
        }
    }
}

impl CorsConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.allow_origins.is_empty() {
            return Err("cors is enabled but allow_origins is empty".into());
        }
        // The spec forbids this combination, and browsers reject it — but the
        // failure is silent and confusing, so refuse at boot instead.
        if self.allow_credentials && self.allow_origins.iter().any(|o| o == "*") {
            return Err(
                "cors: allow_credentials with a '*' origin is forbidden; list explicit origins"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_rate_per_second")]
    pub per_second: f64,
    #[serde(default = "default_rate_burst")]
    pub burst: u32,
    #[serde(default = "default_idle_eviction")]
    pub idle_eviction_secs: u64,
}

fn default_rate_per_second() -> f64 {
    50.0
}
fn default_rate_burst() -> u32 {
    100
}
fn default_idle_eviction() -> u64 {
    300
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            per_second: default_rate_per_second(),
            burst: default_rate_burst(),
            idle_eviction_secs: default_idle_eviction(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityHeaders {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_frame_options")]
    pub frame_options: String,
    #[serde(default = "default_referrer_policy")]
    pub referrer_policy: String,
    #[serde(default)]
    pub content_security_policy: Option<String>,
    #[serde(default = "default_hsts")]
    pub hsts_max_age_secs: u64,
}

fn yes() -> bool {
    true
}
fn default_frame_options() -> String {
    "DENY".into()
}
fn default_referrer_policy() -> String {
    "strict-origin-when-cross-origin".into()
}
fn default_hsts() -> u64 {
    31_536_000
}

impl Default for SecurityHeaders {
    fn default() -> Self {
        Self {
            enabled: true,
            frame_options: default_frame_options(),
            referrer_policy: default_referrer_policy(),
            content_security_policy: None,
            hsts_max_age_secs: default_hsts(),
        }
    }
}

/// A Behaviour: a named, versioned procedure written in Python.
///
/// The framework's answer to "skills". A skill is a prompt the model may
/// ignore; a Behaviour is code whose control flow always runs, with model calls
/// only at the leaves. It is exposed as a tool like any route, so agents can
/// invoke behaviours and behaviours can invoke each other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviourDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Index into the Python handler registry.
    pub handler: u32,
    /// Tools this behaviour may call. Empty means "whatever its caller can".
    #[serde(default)]
    pub tools: Vec<String>,
    /// Scopes the run executes with, intersected with the caller's.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Total leaf operations (tool calls plus model calls) permitted.
    #[serde(default = "default_behaviour_steps")]
    pub max_steps: u32,
    #[serde(default)]
    pub token_budget: Option<u64>,
    #[serde(default = "default_behaviour_model")]
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
    /// Context providers resolved on demand via `ctx.context(name)`.
    #[serde(default)]
    pub context: Vec<String>,
}

fn default_behaviour_steps() -> u32 {
    50
}
fn default_behaviour_model() -> String {
    "claude-opus-5".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateConfig {
    /// Directory containing `.html` templates, resolved relative to the app.
    pub dir: String,
    #[serde(default = "yes")]
    pub autoescape: bool,
}

fn default_version() -> String {
    "0.1.0".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Number of free-threaded Python worker interpreters. `None` = auto
    /// (cpu count). Ignored entirely if no route uses a Python op.
    #[serde(default)]
    pub python_workers: Option<usize>,
    /// Mount point for the introspection surfaces (OpenAPI, MCP, health).
    #[serde(default = "default_control_prefix")]
    pub control_prefix: String,
    /// Ceiling on a single request's handling time. Prevents a hung handler
    /// from pinning a connection indefinitely.
    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u64,
    /// How long to drain in-flight connections on shutdown.
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_secs: u64,
    /// Maximum nesting for in-process invocations (behaviour → tool → behaviour).
    ///
    /// Without this a self-recursive behaviour recurses forever: each nested
    /// invocation receives a fresh step budget, so the per-behaviour cap never
    /// bounds the total, and every level holds a worker thread while it waits.
    /// One request could permanently exhaust the interpreter pool.
    #[serde(default = "default_max_depth")]
    pub max_invocation_depth: u32,
    /// How many multi-turn agent sessions to keep in memory at once.
    #[serde(default = "default_session_capacity")]
    pub session_capacity: usize,
    /// Idle time after which a session is forgotten.
    #[serde(default = "default_session_ttl")]
    pub session_ttl_secs: u64,
    /// How long a suspended run waits for a human before it is discarded.
    #[serde(default = "default_approval_ttl")]
    pub approval_ttl_secs: u64,
}

fn default_max_depth() -> u32 {
    8
}
fn default_session_capacity() -> usize {
    1000
}
fn default_session_ttl() -> u64 {
    3600
}
fn default_approval_ttl() -> u64 {
    3600
}

fn default_request_timeout() -> u64 {
    30
}
fn default_shutdown_timeout() -> u64 {
    25
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    8000
}
fn default_control_prefix() -> String {
    "/_webcortex".into()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            python_workers: None,
            control_prefix: default_control_prefix(),
            request_timeout_secs: default_request_timeout(),
            shutdown_timeout_secs: default_shutdown_timeout(),
            max_invocation_depth: default_max_depth(),
            session_capacity: default_session_capacity(),
            session_ttl_secs: default_session_ttl(),
            approval_ttl_secs: default_approval_ttl(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    pub url: String,
    #[serde(default = "default_pool_size")]
    pub max_connections: u32,
}

fn default_pool_size() -> u32 {
    16
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upstream {
    pub base_url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Name of an environment variable holding a bearer token. Resolved at
    /// startup so secrets never live in the manifest itself.
    #[serde(default)]
    pub bearer_env: Option<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub id: u32,
    pub method: String,
    pub path: String,
    pub op: Op,

    // ---- Description carried for humans, OpenAPI, *and* agents alike. ----
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema describing path/query params merged with the body, i.e.
    /// exactly the shape an agent tool call takes.
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub tool: ToolExposure,
    /// Scopes required to reach this route at all, over HTTP or as a tool.
    /// Distinct from `tool.scopes`, which historically served both purposes.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Whether an agent may invoke this without a human in the loop.
    #[serde(default)]
    pub approval: Approval,
    /// Reject requests whose body fails this JSON Schema before the op runs.
    #[serde(default)]
    pub validate_body: bool,
}

/// Controls whether a route is visible to agents as a callable tool.
///
/// This is the crux of the "agent-native" claim: tool exposure is a property of
/// the route itself, declared next to the handler, not a separate hand-written
/// MCP server that drifts out of sync.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolExposure {
    #[serde(default)]
    pub expose: bool,
    #[serde(default)]
    pub name: Option<String>,
    /// Free-form hints an agent runtime can use for planning: whether the call
    /// mutates state, is safe to retry, or is expensive.
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub idempotent: bool,
    /// Scopes the caller must hold. Enforced before the op runs.
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// The executable core. Each variant is a different answer to "who serves this
/// request", and only `Python` pays interpreter cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Op {
    /// A constant response. Compiled to bytes at startup.
    Static {
        #[serde(default = "default_status")]
        status: u16,
        body: serde_json::Value,
    },
    /// Call a Python handler by index into the handler registry.
    Python { handler: u32 },
    /// Run SQL and stream the rows straight out as JSON. Never enters Python.
    Query {
        sql: String,
        /// Names pulled from path/query params, bound positionally in order.
        #[serde(default)]
        params: Vec<String>,
        #[serde(default)]
        returns: QueryReturns,
    },
    /// Forward to a declared upstream. The gateway layer.
    Proxy {
        upstream: String,
        #[serde(default)]
        rewrite: Option<String>,
    },
    /// Invoke a declared agent, optionally streaming tokens back over SSE.
    Agent {
        agent: String,
        #[serde(default)]
        stream: bool,
    },
    /// Render a server-side template.
    ///
    /// The template receives a data dictionary and nothing else — no database
    /// handle, no ability to call back into Python. That restriction is the
    /// whole reason this layer stays clean: a template cannot grow logic,
    /// because it has nothing to call.
    Page {
        template: String,
        /// Where the template's data comes from. Both options produce a plain
        /// object; neither is reachable from inside the template itself.
        #[serde(default)]
        data: PageData,
        #[serde(default = "default_page_status")]
        status: u16,
    },
    /// Run a Behaviour: Python control flow whose leaves are tool and model
    /// calls. Unlike an agent, the *structure* is deterministic — the loops and
    /// branches are code, and only the leaves are probabilistic.
    Behaviour {
        behaviour: String,
        handler: u32,
    },
    /// Serve files from a directory.
    Files {
        dir: String,
        #[serde(default)]
        index: Option<String>,
        #[serde(default = "default_cache_secs")]
        cache_secs: u64,
    },
    /// Run a declared flow: a pipeline, a parallel fan-out, or a router.
    Flow { flow: String },
}

fn default_page_status() -> u16 {
    200
}
fn default_cache_secs() -> u64 {
    3600
}

/// How a [`Op::Page`] obtains its rendering context.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PageData {
    /// No data beyond the request context.
    #[default]
    None,
    /// Constant data baked in at boot.
    Static { value: serde_json::Value },
    /// A SQL query, executed in Rust. A fully dynamic page with zero Python.
    Query {
        sql: String,
        #[serde(default)]
        params: Vec<String>,
        #[serde(default)]
        returns: QueryReturns,
        /// Name to bind the result under in the template context.
        #[serde(default = "default_bind")]
        bind: String,
    },
    /// A Python handler returning a dict.
    Python { handler: u32 },
}

fn default_bind() -> String {
    "data".into()
}

/// Whether a tool call may proceed unattended.
///
/// The runtime — not the model, and not the application author's diligence —
/// decides. A tool marked `Required` suspends the agent run and waits for a
/// human, which is the difference between an agent you can point at production
/// and a demo.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    /// Runs immediately.
    #[default]
    Never,
    /// Suspends the run and records an approval request.
    Required,
}

fn default_status() -> u16 {
    200
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryReturns {
    /// A JSON array of row objects.
    #[default]
    Many,
    /// A single row object, or 404 when the query yields nothing.
    One,
    /// Row count only, as `{"affected": n}`.
    Affected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub model: String,
    #[serde(default)]
    pub system: String,
    /// Route ids this agent may call as tools. Resolved against the route table
    /// at startup, so a typo is a boot error rather than a runtime surprise.
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub max_steps: Option<u32>,
    /// Hard ceiling on tokens per run. Enforced by the runtime, not the model.
    #[serde(default)]
    pub token_budget: Option<u64>,
    /// Scopes this agent may exercise. Always intersected with the scopes of
    /// whoever started the run — an agent is a delegate, never an escalation.
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Agents this one may hand the conversation to. Each becomes a
    /// `transfer_to_<name>` tool; calling it swaps the active agent while the
    /// conversation, the budget and the caller's authority carry over.
    #[serde(default)]
    pub handoffs: Vec<String>,
    /// Context providers resolved at run start and appended to the system
    /// prompt.
    #[serde(default)]
    pub context: Vec<String>,
    /// Ask the provider to cache the system prompt and tool definitions across
    /// steps. On Anthropic this is prompt caching; it is what makes a
    /// twelve-step run cost a little more than a one-step run instead of twelve
    /// times as much.
    #[serde(default = "yes")]
    pub cache: bool,
    #[serde(default)]
    pub policy: ContextPolicy,
}

fn default_temperature() -> f32 {
    1.0
}
fn default_max_tokens() -> u32 {
    4096
}

impl Manifest {
    /// Validate cross-references that the type system can't catch, so that a
    /// misconfigured app fails at boot instead of on a request.
    pub fn validate(&self) -> Result<(), String> {
        // Name collisions among agents are reported before route collisions:
        // every agent owns a route, so a duplicate agent also duplicates a
        // route, and the generic message would point at the wrong fix.
        let mut agent_seen = std::collections::HashSet::new();
        for a in &self.agents {
            if !agent_seen.insert(a.name.as_str()) {
                return Err(format!("duplicate agent name {:?}", a.name));
            }
        }

        let mut seen = std::collections::HashSet::new();
        for r in &self.routes {

            if !seen.insert((r.method.as_str(), r.path.as_str())) {
                return Err(format!("duplicate route {} {}", r.method, r.path));
            }
            match &r.op {
                Op::Proxy { upstream, .. } if !self.upstreams.contains_key(upstream) => {
                    return Err(format!(
                        "route {} {} proxies to undeclared upstream {:?}",
                        r.method, r.path, upstream
                    ));
                }
                Op::Agent { agent, .. } if !self.agents.iter().any(|a| &a.name == agent) => {
                    return Err(format!(
                        "route {} {} invokes undeclared agent {:?}",
                        r.method, r.path, agent
                    ));
                }
                Op::Flow { flow } if !self.flows.iter().any(|f| &f.name == flow) => {
                    return Err(format!(
                        "route {} {} invokes undeclared flow {:?}",
                        r.method, r.path, flow
                    ));
                }
                Op::Query { .. } if self.database.is_none() => {
                    return Err(format!(
                        "route {} {} runs a query but no database is configured",
                        r.method, r.path
                    ));
                }
                Op::Page { data: PageData::Query { .. }, .. } if self.database.is_none() => {
                    return Err(format!(
                        "page {} {} sources data from a query but no database is configured",
                        r.method, r.path
                    ));
                }
                Op::Page { .. } if self.templates.is_none() => {
                    return Err(format!(
                        "route {} {} renders a template but no template directory is configured; \
                         pass templates=... to WebCortex()",
                        r.method, r.path
                    ));
                }
                _ => {}
            }

            // An approval gate on a route no agent can call is almost always a
            // mistake — either the author meant to expose it, or the gate is
            // dead configuration giving false confidence.
            if r.approval == Approval::Required && !r.tool.expose {
                return Err(format!(
                    "route {} {} requires approval but is not exposed as a tool; \
                     approval gates only apply to agent tool calls",
                    r.method, r.path
                ));
            }
        }

        self.cors.validate()?;

        if self.rate_limit.enabled && self.rate_limit.per_second <= 0.0 {
            return Err("rate_limit.per_second must be greater than zero".into());
        }

        // Checked before route tool-name collisions, because a duplicate
        // behaviour causes one of those too and the generic message would
        // point at the wrong fix.
        let mut behaviour_names = std::collections::HashSet::new();
        for b in &self.behaviours {
            if !behaviour_names.insert(b.name.as_str()) {
                return Err(format!("duplicate behaviour name {:?}", b.name));
            }
            if b.max_steps == 0 {
                return Err(format!(
                    "behaviour {:?} has max_steps=0 and could never do anything",
                    b.name
                ));
            }
        }

        // Two routes answering to one tool name would make a model's tool call
        // ambiguous. Caught here rather than at bind time so `webcortex check`
        // reports it.
        let mut tool_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in self.routes.iter().filter(|r| r.tool.expose) {
            let name = r.tool_name();
            if !tool_names.insert(name.clone()) {
                return Err(format!(
                    "two routes both expose the tool name {name:?}; set tool_name= on one of them"
                ));
            }
        }

        let context_names: std::collections::HashSet<&str> =
            self.contexts.iter().map(|c| c.name.as_str()).collect();
        if context_names.len() != self.contexts.len() {
            return Err("duplicate context provider name".into());
        }
        let check_contexts = |owner: &str, kind: &str, names: &[String]| -> Result<(), String> {
            for c in names {
                if !context_names.contains(c.as_str()) {
                    return Err(format!(
                        "{kind} {owner:?} names context {c:?}, which is not declared; \
                         declare it with app.context(...)"
                    ));
                }
            }
            Ok(())
        };

        for b in &self.behaviours {
            check_contexts(&b.name, "behaviour", &b.context)?;
            for t in &b.tools {
                // A behaviour may call another behaviour, so both namespaces
                // are valid targets.
                if tool_names.contains(t.as_str())
                    || self.behaviours.iter().any(|o| &o.name == t)
                {
                    continue;
                }
                let hint = nearest(t, &tool_names);
                return Err(match hint {
                    Some(h) => format!(
                        "behaviour {:?} declares tool {:?}, which is not an exposed route \
                         or behaviour. Did you mean {:?}?",
                        b.name, t, h
                    ),
                    None => format!(
                        "behaviour {:?} declares tool {:?}, which is not an exposed route \
                         or behaviour",
                        b.name, t
                    ),
                });
            }
        }

        let mut agent_names = std::collections::HashSet::new();
        for a in &self.agents {
            if !agent_names.insert(a.name.as_str()) {
                return Err(format!("duplicate agent name {:?}", a.name));
            }
            for t in &a.tools {
                if !tool_names.contains(t.as_str())
                    && !self.behaviours.iter().any(|b| &b.name == t)
                {
                    let hint = nearest(t, &tool_names);
                    return Err(match hint {
                        Some(h) => format!(
                            "agent {:?} references tool {:?}, which is not an exposed route. Did you mean {:?}?",
                            a.name, t, h
                        ),
                        None => format!(
                            "agent {:?} references tool {:?}, which is not an exposed route",
                            a.name, t
                        ),
                    });
                }
            }
            if a.max_steps == Some(0) {
                return Err(format!("agent {:?} has max_steps=0 and could never act", a.name));
            }
            check_contexts(&a.name, "agent", &a.context)?;
            for h in &a.handoffs {
                if h == &a.name {
                    return Err(format!("agent {:?} lists itself as a handoff target", a.name));
                }
                if !agent_names_all(self).contains(h.as_str()) {
                    let hint = nearest(h, &agent_names_all(self).into_iter().map(String::from).collect());
                    return Err(match hint {
                        Some(x) => format!(
                            "agent {:?} hands off to {:?}, which is not a declared agent. Did you mean {:?}?",
                            a.name, h, x
                        ),
                        None => format!(
                            "agent {:?} hands off to {:?}, which is not a declared agent",
                            a.name, h
                        ),
                    });
                }
            }
            if a.policy.keep_recent == 0 {
                return Err(format!(
                    "agent {:?} has keep_recent=0; a compaction must keep at least one recent turn",
                    a.name
                ));
            }
        }

        let mut flow_names = std::collections::HashSet::new();
        for f in &self.flows {
            if !flow_names.insert(f.name.as_str()) {
                return Err(format!("duplicate flow name {:?}", f.name));
            }
            let steps: Vec<&FlowStep> = match &f.kind {
                FlowKind::Pipeline { steps } => {
                    if steps.is_empty() {
                        return Err(format!("flow {:?} is a pipeline with no steps", f.name));
                    }
                    steps.iter().collect()
                }
                FlowKind::Parallel { branches, .. } => {
                    if branches.is_empty() {
                        return Err(format!("flow {:?} is a parallel with no branches", f.name));
                    }
                    branches.iter().collect()
                }
                FlowKind::Route { routes, default, .. } => {
                    if routes.is_empty() {
                        return Err(format!("flow {:?} is a router with no routes", f.name));
                    }
                    routes.values().chain(default.iter()).collect()
                }
            };
            for s in steps {
                if s.tool == f.name {
                    return Err(format!("flow {:?} contains itself as a step", f.name));
                }
                if !tool_names.contains(s.tool.as_str()) {
                    let hint = nearest(&s.tool, &tool_names);
                    return Err(match hint {
                        Some(h) => format!(
                            "flow {:?} references tool {:?}, which is not an exposed route. Did you mean {:?}?",
                            f.name, s.tool, h
                        ),
                        None => format!(
                            "flow {:?} references tool {:?}, which is not an exposed route",
                            f.name, s.tool
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn flow(&self, name: &str) -> Option<&FlowDef> {
        self.flows.iter().find(|f| f.name == name)
    }

    pub fn context(&self, name: &str) -> Option<&ContextDef> {
        self.contexts.iter().find(|c| c.name == name)
    }

    pub fn agent_def(&self, name: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|a| a.name == name)
    }


    /// Routes reachable without any credential. Surfaced by `webcortex check` so an
    /// operator can see their public attack surface on one screen.
    pub fn public_routes(&self) -> Vec<&Route> {
        self.routes
            .iter()
            .filter(|r| r.scopes.is_empty() && r.tool.scopes.is_empty())
            .collect()
    }
}

fn agent_names_all(m: &Manifest) -> std::collections::HashSet<&str> {
    m.agents.iter().map(|a| a.name.as_str()).collect()
}

/// Closest match by edit distance, for "did you mean" hints on typos.
fn nearest(needle: &str, haystack: &std::collections::HashSet<String>) -> Option<String> {
    haystack
        .iter()
        .map(|c| (edit_distance(needle, c), c))
        .filter(|(d, _)| *d <= 3)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c.clone())
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

impl Route {
    /// The name an agent sees. Derived from method+path when not given, so that
    /// exposing a tool is a one-word change rather than a naming exercise.
    pub fn tool_name(&self) -> String {
        match &self.tool.name {
            Some(n) => n.clone(),
            None => derive_tool_name(&self.method, &self.path),
        }
    }
}

pub fn derive_tool_name(method: &str, path: &str) -> String {
    let verb = match method {
        "GET" => "get",
        "POST" => "create",
        "PUT" | "PATCH" => "update",
        "DELETE" => "delete",
        other => &other.to_ascii_lowercase(),
    };
    let mut parts = vec![verb.to_string()];
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if let Some(inner) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            parts.push("by".into());
            parts.push(inner.replace('-', "_"));
        } else {
            parts.push(seg.replace('-', "_"));
        }
    }
    parts.join("_")
}
