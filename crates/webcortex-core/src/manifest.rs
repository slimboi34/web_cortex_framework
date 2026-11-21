//! The manifest is the contract between the Python declaration layer and the
//! Rust runtime.
//!
//! Python code is *not* the server. Python runs once at startup, describes the
//! application as data, and hands that data to Rust. Rust compiles it into a
//! router plus a set of executable ops. Only ops of kind `Python` ever re-enter
//! the interpreter; everything else is served without touching the GIL.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

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
    /// Cameras and sensors whose frames and telemetry live in the Rust device hub.
    #[serde(default)]
    pub devices: Vec<DeviceDef>,
    /// Loops that hand a device's newest frame to an agent and publish what it says.
    #[serde(default)]
    pub watchers: Vec<WatcherDef>,
    /// Span traces of agent, flow and behaviour runs. See [`crate::trace`].
    #[serde(default)]
    pub tracing: TracingConfig,
}

// ---------------------------------------------------------------------------
// Traces
// ---------------------------------------------------------------------------

/// How runs are traced and where the spans go. See [`crate::trace`].
///
/// Tracing is on by default and costs nothing on routes that run no model:
/// only agent, flow and behaviour runs open spans. Spans never hold a prompt,
/// an argument or a result, only names, timings, token counts and error codes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracingConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// How many recent traces `GET /_webcortex/traces` keeps in memory.
    #[serde(default = "default_trace_keep")]
    pub keep: usize,
    /// Spans kept per trace; further ones are counted as dropped.
    #[serde(default = "default_trace_spans")]
    pub max_spans: usize,
    /// OTLP/HTTP endpoint to export finished spans to, e.g.
    /// `http://localhost:4318`. When unset, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`
    /// and then `OTEL_EXPORTER_OTLP_ENDPOINT` are read from the environment.
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    /// Environment variable holding the export headers as `k=v,k2=v2` (the
    /// `OTEL_EXPORTER_OTLP_HEADERS` format). Named, never embedded.
    #[serde(default)]
    pub otlp_headers_env: Option<String>,
    /// `service.name` on exported spans. Defaults to the app's name.
    #[serde(default)]
    pub service_name: Option<String>,
}

fn default_trace_keep() -> usize {
    200
}
fn default_trace_spans() -> usize {
    500
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep: default_trace_keep(),
            max_spans: default_trace_spans(),
            otlp_endpoint: None,
            otlp_headers_env: None,
            service_name: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Devices: cameras and sensors, and the agents that watch them
// ---------------------------------------------------------------------------

/// A camera or sensor. Frames and telemetry are held in Rust; no Python runs
/// per frame. See [`crate::devices`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub kind: DeviceKind,
    /// Where frames come from when nobody pushes them: an HTTP snapshot URL,
    /// fetched when a frame is asked for. Most IP cameras expose one.
    #[serde(default)]
    pub source: Option<PullSource>,
    /// Frames kept in the ring buffer.
    #[serde(default = "default_keep_frames")]
    pub keep: usize,
    /// Ingest ceiling. Frames arriving faster are dropped (WebSocket) or
    /// refused with 429 (HTTP), so a misbehaving device cannot flood the hub.
    #[serde(default = "default_max_fps")]
    pub max_fps: f64,
    #[serde(default = "default_max_frame_bytes")]
    pub max_frame_bytes: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    #[default]
    Camera,
    /// Telemetry only.
    Sensor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullSource {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Environment variable holding a bearer token for the camera.
    #[serde(default)]
    pub bearer_env: Option<String>,
    #[serde(default = "default_pull_timeout")]
    pub timeout_ms: u64,
}

fn default_keep_frames() -> usize {
    8
}
fn default_max_fps() -> f64 {
    30.0
}
fn default_max_frame_bytes() -> usize {
    5 * 1024 * 1024
}
fn default_pull_timeout() -> u64 {
    5000
}

/// What a device route does.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceAction {
    /// JSON with the newest frame as an image marker, plus telemetry. The tool.
    Snapshot,
    /// The newest frame's raw bytes, for an `<img>` tag.
    Image,
    Telemetry,
    Insights,
    /// POST a frame: the body is the encoded image.
    IngestFrame,
    /// POST telemetry: the body is JSON.
    IngestTelemetry,
    /// WebSocket: binary messages are frames, text messages are telemetry.
    SocketIngest,
    /// WebSocket: frames, telemetry and insights as they happen.
    SocketStream,
    /// An HTML page that turns a browser's camera into this device.
    ConnectPage,
    /// An HTML page that shows the live stream and insights.
    ViewPage,
}

/// Hand a device's newest frame to an agent on a timer, and publish the answer
/// to the device's subscribers and, optionally, a webhook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatcherDef {
    pub name: String,
    pub device: String,
    pub agent: String,
    pub input: String,
    #[serde(default = "default_watch_secs")]
    pub every_secs: f64,
    /// Skip a tick when no new frame or telemetry has arrived since the last run.
    #[serde(default = "default_true")]
    pub only_on_change: bool,
    /// The authority the agent runs with. A watcher has no caller to inherit from.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Hard ceiling on runs, and therefore on spend: a timer is a loop that
    /// nobody is watching.
    #[serde(default = "default_runs_per_hour")]
    pub max_runs_per_hour: u32,
    #[serde(default)]
    pub webhook: Option<WebhookDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDef {
    pub url: String,
    #[serde(default)]
    pub bearer_env: Option<String>,
    #[serde(default = "default_pull_timeout")]
    pub timeout_ms: u64,
}

fn default_watch_secs() -> f64 {
    10.0
}
fn default_true() -> bool {
    true
}
fn default_runs_per_hour() -> u32 {
    60
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
    /// `provider name -> request options` merged into every request body sent
    /// to that provider, beneath the agent's own `options`. The name is a
    /// declared provider or a built-in one (`anthropic`, `openai`, `ollama`).
    #[serde(default)]
    pub options: BTreeMap<String, RequestOptions>,
}

/// Extra top-level fields for a provider's request body: the pass-through for
/// whatever a provider supports that WebCortex has no keyword for (`seed`,
/// `top_p`, `keep_alive`, `response_format`, `thinking`, …). A `null` value
/// removes a field the runtime would otherwise send. See
/// [`crate::agent::provider::apply_options`].
pub type RequestOptions = serde_json::Map<String, serde_json::Value>;

/// Body fields the runtime owns. Setting or removing one through `options`
/// would break the tool loop (or, for `stream`, the response parser), so it is
/// a boot error instead.
pub const RESERVED_OPTIONS: [&str; 6] = ["model", "messages", "tools", "tool_choice", "system", "stream"];

/// Why `options` cannot be used, if they cannot.
pub fn options_problem(options: &RequestOptions) -> Option<String> {
    options.keys().find(|k| RESERVED_OPTIONS.contains(&k.as_str())).map(|k| {
        format!(
            "option {k:?} is managed by the runtime and cannot be set through options              (reserved: {})",
            RESERVED_OPTIONS.join(", ")
        )
    })
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
    /// How many images stay in the conversation as pixels. Older ones become
    /// a line of text: a camera tool called in a loop would otherwise resend
    /// every frame it ever took on every step.
    #[serde(default = "default_max_images")]
    pub max_images: usize,
}

fn default_tool_result_bytes() -> usize {
    16 * 1024
}
fn default_keep_recent() -> usize {
    6
}
fn default_max_images() -> usize {
    4
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            max_tool_result_bytes: default_tool_result_bytes(),
            max_context_tokens: None,
            compact_with: None,
            keep_recent: default_keep_recent(),
            max_images: default_max_images(),
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

impl FlowKind {
    /// Every step the flow can run: a pipeline's steps, a parallel's
    /// branches, a router's routes and its default.
    pub fn steps(&self) -> Vec<&FlowStep> {
        match self {
            FlowKind::Pipeline { steps } => steps.iter().collect(),
            FlowKind::Parallel { branches, .. } => branches.iter().collect(),
            FlowKind::Route { routes, default, .. } => routes.values().chain(default.iter()).collect(),
        }
    }
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
    /// Sent only when set: recent Anthropic models reject sampling parameters.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// How hard the model may think before answering: `none`, `low`, `medium`
    /// or `high`. Sent as `reasoning_effort` on the OpenAI wire format, where
    /// `none` is what makes a local thinking model answer in a second rather
    /// than a minute. Not sent to Anthropic.
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
    /// Context providers resolved on demand via `ctx.context(name)`.
    #[serde(default)]
    pub context: Vec<String>,
    /// Extra request-body fields for this behaviour's `ctx.ask` calls.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub options: RequestOptions,
}

fn default_behaviour_steps() -> u32 {
    50
}
fn default_behaviour_model() -> String {
    // The alias, as for agents, so `app.models(default=...)` also applies to a
    // manifest that leaves the model out.
    "default".into()
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
    /// Mount point for the introspection surfaces (OpenAPI, MCP, health).
    #[serde(default = "default_control_prefix")]
    pub control_prefix: String,
    /// Ceiling on a single request's handling time. Prevents a hung handler
    /// from pinning a connection indefinitely.
    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u64,
    /// Ceiling for routes that run a model loop (agents, flows, behaviours),
    /// which routinely outlast `request_timeout_secs`.
    #[serde(default = "default_agent_timeout")]
    pub agent_timeout_secs: u64,
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
    /// Boot with the emergency stop engaged. The halt lives in memory, so a
    /// crash-and-restart would otherwise come back with every actuator live;
    /// a machine that moves should wait for an operator to release it.
    #[serde(default)]
    pub start_halted: bool,
    /// Ceiling on open connections, WebSockets included. Past it a new
    /// connection is answered `503` and closed at once, so a flood of idle
    /// sockets cannot exhaust file descriptors or memory. `0` means no cap.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
}

fn default_max_connections() -> usize {
    10_000
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
fn default_agent_timeout() -> u64 {
    600
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
            control_prefix: default_control_prefix(),
            request_timeout_secs: default_request_timeout(),
            agent_timeout_secs: default_agent_timeout(),
            shutdown_timeout_secs: default_shutdown_timeout(),
            max_invocation_depth: default_max_depth(),
            session_capacity: default_session_capacity(),
            session_ttl_secs: default_session_ttl(),
            approval_ttl_secs: default_approval_ttl(),
            start_halted: false,
            max_connections: default_max_connections(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    pub url: String,
    #[serde(default = "default_pool_size")]
    pub max_connections: u32,
    /// `CREATE TABLE IF NOT EXISTS` statements for declared resources, applied
    /// through the pool at connect. Python used to apply them on a connection
    /// of its own, which a `:memory:` database never saw.
    #[serde(default)]
    pub schema: String,
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
    /// The largest upstream response body a proxy route will relay. The body
    /// is buffered, so without a ceiling one huge (or endless) upstream
    /// response held that much memory per request.
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
}

fn default_timeout_ms() -> u64 {
    30_000
}
fn default_max_response_bytes() -> usize {
    64 * 1024 * 1024
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
    /// The route acts on the physical world — a motor, a valve, a relay. It
    /// refuses to run while the emergency stop is engaged.
    #[serde(default)]
    pub actuator: bool,
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
    /// Invoke a declared agent. The result is returned whole; streaming is on
    /// the roadmap (DESIGN.md §6).
    Agent { agent: String },
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
    /// The handler index lives on the [`BehaviourDef`].
    Behaviour { behaviour: String },
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
    /// A camera or sensor in the device hub. Never enters Python.
    Device { device: String, action: DeviceAction },
}

impl Op {
    /// The engine name, as `x-webcortex-op` and the control plane report it.
    pub fn kind(&self) -> &'static str {
        match self {
            Op::Static { .. } => "static",
            Op::Python { .. } => "python",
            Op::Query { .. } => "query",
            Op::Proxy { .. } => "proxy",
            Op::Agent { .. } => "agent",
            Op::Page { .. } => "page",
            Op::Files { .. } => "files",
            Op::Behaviour { .. } => "behaviour",
            Op::Flow { .. } => "flow",
            Op::Device { .. } => "device",
        }
    }
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
    /// Tool names this agent may call. Resolved against the route table
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
    /// Sent only when set: recent Anthropic models reject sampling parameters.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// How hard the model may think before answering: `none`, `low`, `medium`
    /// or `high`. Sent as `reasoning_effort` on the OpenAI wire format (Ollama,
    /// vLLM, OpenAI); `none` is what makes a local thinking model answer in a
    /// second rather than a minute. Not sent to Anthropic.
    #[serde(default)]
    pub reasoning: Option<String>,
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
    /// Extra top-level fields for the provider's request body, merged over the
    /// provider's own (`models.options`). See [`RequestOptions`].
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub options: RequestOptions,
    /// The JSON Schema the run's answer must match. With one set, the agent
    /// finishes by calling a `final_answer` tool, the runtime validates what
    /// it sent (see [`crate::schema`]), and the result carries the value as
    /// `data`. An answer that still does not match after
    /// [`crate::agent::MAX_OUTPUT_REPAIRS`] corrections fails the run (WC318).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
}

fn default_max_tokens() -> u32 {
    // Room for adaptive thinking as well as the answer: 4096 could leave a
    // thinking model with little or no text.
    16_000
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
            if let Some(problem) = options_problem(&a.options) {
                return Err(format!("agent {:?}: {problem}", a.name));
            }
            if let Some(schema) = &a.output_schema {
                if let Some(problem) = crate::schema::output_schema_problem(schema) {
                    return Err(format!("agent {:?} has an output schema that cannot be used: {problem}", a.name));
                }
                if a.tools.iter().any(|t| t == crate::agent::FINAL_TOOL) {
                    return Err(format!(
                        "agent {:?} has an output schema, so the tool name {:?} is taken by its answer; \
                         rename the route's tool",
                        a.name,
                        crate::agent::FINAL_TOOL
                    ));
                }
                // The answer tool survives a handoff (the entry agent's
                // contract holds for the whole run), so it is taken for every
                // agent the run can be handed to as well. Through 2.8.0 a
                // specialist listing a route named final_answer booted, and
                // its request then carried two tools of that name.
                if let Some(target) = self.handoff_closure(&a.name).into_iter().find(|t| {
                    self.agents.iter().any(|o| &o.name == t && o.tools.iter().any(|x| x == crate::agent::FINAL_TOOL))
                }) {
                    return Err(format!(
                        "agent {:?} has an output schema, so the tool name {:?} is taken by its answer \
                         for the whole run, but agent {target:?}, which the run can be handed to, lists a \
                         tool of that name; rename the route's tool",
                        a.name,
                        crate::agent::FINAL_TOOL
                    ));
                }
            }
        }
        for b in &self.behaviours {
            if let Some(problem) = options_problem(&b.options) {
                return Err(format!("behaviour {:?}: {problem}", b.name));
            }
        }
        for (provider, options) in &self.models.options {
            if let Some(problem) = options_problem(options) {
                return Err(format!("provider {provider:?}: {problem}"));
            }
        }
        if self.tracing.enabled && (self.tracing.keep == 0 || self.tracing.max_spans == 0) {
            return Err("tracing.keep and tracing.max_spans must be at least 1 (or disable tracing)".into());
        }
        if let Some(url) = &self.tracing.otlp_endpoint
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            return Err(format!("tracing.otlp_endpoint must start with http:// or https://, got {url:?}"));
        }

        let mut device_seen = std::collections::HashSet::new();
        for d in &self.devices {
            if !device_seen.insert(d.name.as_str()) {
                return Err(format!("duplicate device name {:?}", d.name));
            }
            if !d.max_fps.is_finite() || d.max_fps <= 0.0 || d.keep == 0 || d.max_frame_bytes == 0 {
                return Err(format!("device {:?}: max_fps, keep and max_frame_bytes must be positive", d.name));
            }
            if let Some(src) = &d.source
                && !(src.url.starts_with("http://") || src.url.starts_with("https://")) {
                    return Err(format!("device {:?}: source must be an http(s) URL", d.name));
                }
        }
        let mut watcher_seen = std::collections::HashSet::new();
        for w in &self.watchers {
            if !watcher_seen.insert(w.name.as_str()) {
                return Err(format!("duplicate watcher name {:?}", w.name));
            }
            if !self.devices.iter().any(|d| d.name == w.device) {
                return Err(format!("watcher {:?} watches undeclared device {:?}", w.name, w.device));
            }
            if !self.agents.iter().any(|a| a.name == w.agent) {
                return Err(format!("watcher {:?} runs undeclared agent {:?}", w.name, w.agent));
            }
            if !w.every_secs.is_finite() || w.every_secs < 1.0 {
                return Err(format!("watcher {:?}: every_secs must be at least 1", w.name));
            }
            // A finite but enormous period overflowed a Duration and panicked
            // the watcher's task at boot, silently.
            if std::time::Duration::try_from_secs_f64(w.every_secs).is_err() {
                return Err(format!("watcher {:?}: every_secs {} is too large", w.name, w.every_secs));
            }
            if w.max_runs_per_hour == 0 {
                return Err(format!("watcher {:?}: max_runs_per_hour must be at least 1", w.name));
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
                Op::Device { device, .. } if !self.devices.iter().any(|d| &d.name == device) => {
                    return Err(format!(
                        "route {} {} serves undeclared device {:?}",
                        r.method, r.path, device
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
            // Every provider refuses a request for zero tokens, so this used to
            // surface only as a provider error on the first ctx.ask.
            if b.max_tokens == 0 {
                return Err(format!("behaviour {:?} has max_tokens=0 and could never get an answer", b.name));
            }
        }

        // Two routes answering to one tool name would make a model's tool call
        // ambiguous. Caught here rather than at bind time so `webcortex check`
        // reports it.
        let mut tool_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in self.routes.iter().filter(|r| r.tool.expose) {
            let name = r.tool_name();
            if let Some(problem) = tool_name_problem(&name) {
                return Err(format!(
                    "route {} {} exposes the tool name {name:?}, which {problem}: model \
                     providers refuse a request whose tools include it",
                    r.method, r.path
                ));
            }
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
                // An exposed behaviour is a tool like any route. One declared
                // with tool=False has no tool, so naming it could never work.
                if tool_names.contains(t.as_str()) {
                    continue;
                }
                if self.behaviours.iter().any(|o| &o.name == t) {
                    return Err(format!(
                        "behaviour {:?} declares tool {:?}, a behaviour declared with tool=False, \
                         which cannot be called as a tool",
                        b.name, t
                    ));
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

        // Duplicate agent names were already rejected, before the routes.
        for a in &self.agents {
            for t in &a.tools {
                if !tool_names.contains(t.as_str()) {
                    if self.behaviours.iter().any(|b| &b.name == t) {
                        return Err(format!(
                            "agent {:?} references tool {:?}, a behaviour declared with tool=False, \
                             which cannot be called as a tool",
                            a.name, t
                        ));
                    }
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
            if a.max_tokens == 0 {
                return Err(format!("agent {:?} has max_tokens=0 and could never get an answer", a.name));
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
                // The handoff is itself a tool. A declared tool of the same
                // name would reach the model twice and never be dispatched.
                if a.tools.iter().any(|t| t.strip_prefix("transfer_to_") == Some(h.as_str())) {
                    return Err(format!(
                        "agent {:?} lists tool \"transfer_to_{h}\", which collides with the tool \
                         its handoff to {h:?} creates",
                        a.name
                    ));
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
        // A flow step is dispatched straight to its tool, with nobody to ask:
        // a flow cannot wait for a human. Through 2.8.0 a gated tool named as
        // a step simply ran, so wrapping it in a flow — then calling the flow
        // from an agent, over MCP or from a behaviour — skipped its approval.
        for f in &self.flows {
            if let Some((via, tool)) = self.gated_tool_in_flow(&f.name) {
                let path = if via.len() > 1 { format!(" (through {})", via.join(" -> ")) } else { String::new() };
                return Err(format!(
                    "flow {:?} runs the approval-gated tool {tool:?} as a step{path}; a flow cannot \
                     wait for a human, so its gate would never be asked. Let an agent call {tool:?} \
                     (the run suspends for approval) or remove approval=\"required\"",
                    f.name
                ));
            }
        }
        Ok(())
    }

    /// Every agent a run started at `agent` can be handed to, directly or
    /// through other handoffs. Excludes `agent` itself.
    pub fn handoff_closure(&self, agent: &str) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        let mut queue: Vec<&str> = vec![agent];
        while let Some(name) = queue.pop() {
            let Some(def) = self.agents.iter().find(|a| a.name == name) else { continue };
            for h in &def.handoffs {
                if h != agent && !seen.iter().any(|s| s == h) {
                    seen.push(h.clone());
                    queue.push(h);
                }
            }
        }
        seen
    }

    /// The approval-gated tool that calling `tool` would reach without an
    /// agent asking a human first: `tool` itself when it is gated, or a gated
    /// step anywhere inside it when it is a flow (nested flows, parallel
    /// branches and router routes included). `None` when there is none.
    pub fn gated_tool_behind(&self, tool: &str) -> Option<String> {
        let route = self.routes.iter().find(|r| r.tool.expose && r.tool_name() == tool)?;
        if route.approval == Approval::Required {
            return Some(tool.to_string());
        }
        match &route.op {
            Op::Flow { flow } => self.gated_tool_in_flow(flow).map(|(_, gated)| gated),
            _ => None,
        }
    }

    /// The first approval-gated tool a flow runs as a step, directly or
    /// through flows nested in it, with the chain of flows that reaches it.
    pub fn gated_tool_in_flow(&self, flow: &str) -> Option<(Vec<String>, String)> {
        let by_tool: HashMap<String, &Route> =
            self.routes.iter().filter(|r| r.tool.expose).map(|r| (r.tool_name(), r)).collect();
        let mut seen = std::collections::HashSet::new();
        let mut via = Vec::new();
        self.walk_flow_for_gates(flow, &by_tool, &mut seen, &mut via)
    }

    fn walk_flow_for_gates(
        &self,
        flow: &str,
        by_tool: &HashMap<String, &Route>,
        seen: &mut std::collections::HashSet<String>,
        via: &mut Vec<String>,
    ) -> Option<(Vec<String>, String)> {
        // Flows may name each other in a cycle (bounded at run time by the
        // nesting ceiling); each is walked once.
        if !seen.insert(flow.to_string()) {
            return None;
        }
        let def = self.flow(flow)?;
        via.push(flow.to_string());
        for step in def.kind.steps() {
            let Some(route) = by_tool.get(&step.tool) else { continue };
            if route.approval == Approval::Required {
                return Some((via.clone(), step.tool.clone()));
            }
            if let Op::Flow { flow: inner } = &route.op
                && let Some(found) = self.walk_flow_for_gates(inner, by_tool, seen, via)
            {
                return Some(found);
            }
        }
        via.pop();
        None
    }

    pub fn flow(&self, name: &str) -> Option<&FlowDef> {
        self.flows.iter().find(|f| f.name == name)
    }

    pub fn context(&self, name: &str) -> Option<&ContextDef> {
        self.contexts.iter().find(|c| c.name == name)
    }

    /// Routes reachable without any credential. Surfaced by `webcortex check` so an
    /// operator can see their public attack surface on one screen.
    ///
    /// A route is public when no authentication is configured at all, or when
    /// every scope it requires (its own and its tool's; for an agent, a
    /// behaviour or a flow that is its `expose_scopes`) is one that
    /// `anonymous_scopes` grants. Through 2.8.0 only routes declaring no scope
    /// were listed, so a write route opened by `anonymous_scopes("write")`
    /// was missing from the very report meant to show it.
    pub fn public_routes(&self) -> Vec<&Route> {
        let auth_off = self.auth.api_keys.is_empty() && self.auth.jwt.is_none();
        let anonymous = &self.auth.anonymous_scopes;
        let granted = |scope: &String| anonymous.iter().any(|a| a == scope || a == "*");
        self.routes
            .iter()
            .filter(|r| auth_off || r.scopes.iter().chain(&r.tool.scopes).all(granted))
            .collect()
    }
}

fn agent_names_all(m: &Manifest) -> std::collections::HashSet<&str> {
    m.agents.iter().map(|a| a.name.as_str()).collect()
}

/// Closest match by edit distance, for "did you mean" hints on typos.
fn nearest(needle: &str, haystack: &std::collections::HashSet<String>) -> Option<String> {
    // A handler's function name (`move`) is the usual mistake for its derived
    // tool name (`create_arm_move`): prefer a name containing the needle as
    // whole `_`-separated words over one that is merely a few edits away.
    let words = |c: &str| {
        let needle = needle.trim_matches('_');
        !needle.is_empty()
            && c != needle
            && (c.starts_with(&format!("{needle}_"))
                || c.ends_with(&format!("_{needle}"))
                || c.contains(&format!("_{needle}_")))
    };
    if let Some(c) = haystack.iter().filter(|c| words(c)).min_by_key(|c| (c.len(), c.as_str())) {
        return Some(c.clone());
    }
    haystack
        .iter()
        .map(|c| (edit_distance(needle, c), c))
        .filter(|(d, _)| *d <= 3)
        .min_by_key(|(d, c)| (*d, c.as_str()))
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
            // `{*rest}` is a wildcard named `rest`.
            parts.push(tool_word(inner.trim_start_matches('*')));
        } else {
            parts.push(tool_word(seg));
        }
    }
    parts.join("_")
}

/// A path segment as part of a tool name. Model providers and MCP clients
/// accept `[A-Za-z0-9_-]` in a tool name and refuse the whole tool list over
/// anything else, so `/v1.0/{*rest}` must not derive `get_v1.0_by_*rest`.
fn tool_word(seg: &str) -> String {
    seg.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

/// Why an explicit `tool_name=` cannot be offered to a model, if it cannot.
fn tool_name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("is empty");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')) {
        return Some("may contain only ASCII letters, digits, '_' and '-'");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::nearest;

    fn set(names: &[&str]) -> std::collections::HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn derived_tool_names_only_use_characters_providers_accept() {
        use super::derive_tool_name;
        assert_eq!(derive_tool_name("GET", "/files/{*rest}"), "get_files_by_rest");
        assert_eq!(derive_tool_name("GET", "/v1.0/items"), "get_v1_0_items");
        assert_eq!(derive_tool_name("GET", "/a~b/{user-id}"), "get_a_b_by_user_id");
        assert_eq!(derive_tool_name("POST", "/my-items"), "create_my_items");
    }

    #[test]
    fn explicit_tool_names_are_checked() {
        use super::tool_name_problem;
        assert!(tool_name_problem("search_books").is_none());
        assert!(tool_name_problem("books-search_v2").is_none());
        assert!(tool_name_problem("books.search").is_some());
        assert!(tool_name_problem("").is_some());
        assert!(tool_name_problem("search books").is_some());
        assert!(tool_name_problem("search/books").is_some());
    }

    #[test]
    fn a_handler_name_suggests_its_derived_tool_name() {
        // 2.5.0 suggested the agent "mover" for tools=["move"], one edit away,
        // when the route's tool was create_arm_move.
        let tools = set(&["mover", "create_arm_move", "list_things", "get_typed_by_n"]);
        assert_eq!(nearest("move", &tools).as_deref(), Some("create_arm_move"));
        assert_eq!(nearest("typed", &tools).as_deref(), Some("get_typed_by_n"));
        // A typo still finds its near neighbour.
        assert_eq!(nearest("list_thing", &tools).as_deref(), Some("list_things"));
        assert_eq!(nearest("zzzzzzzzzz", &tools), None);
    }

    #[test]
    fn routes_opened_by_anonymous_scopes_are_public() {
        let m = |auth: serde_json::Value| -> super::Manifest {
            serde_json::from_value(serde_json::json!({
                "name": "p",
                "auth": auth,
                "routes": [
                    {"id": 0, "method": "GET", "path": "/open", "op": {"kind": "static", "body": {}}},
                    {"id": 1, "method": "POST", "path": "/items", "op": {"kind": "static", "body": {}}, "scopes": ["write"]},
                    {"id": 2, "method": "GET", "path": "/items", "op": {"kind": "static", "body": {}},
                     "tool": {"expose": true, "name": "list_items", "scopes": ["read"]}},
                    {"id": 3, "method": "DELETE", "path": "/all", "op": {"kind": "static", "body": {}}, "scopes": ["admin"]}
                ]
            }))
            .unwrap()
        };
        let public = |m: &super::Manifest| -> Vec<String> {
            m.public_routes().iter().map(|r| format!("{} {}", r.method, r.path)).collect()
        };
        let keys = serde_json::json!({"api_keys": {"K": {"id": "svc", "scopes": ["*"]}}, "api_key_header": "x-api-key"});
        assert_eq!(public(&m(keys.clone())), vec!["GET /open"]);
        let mut anon = keys.clone();
        anon["anonymous_scopes"] = serde_json::json!(["read", "write"]);
        assert_eq!(public(&m(anon)), vec!["GET /open", "POST /items", "GET /items"]);
        let mut star = keys;
        star["anonymous_scopes"] = serde_json::json!(["*"]);
        assert_eq!(public(&m(star)).len(), 4);
        // With no authentication configured, every route is open to everyone.
        assert_eq!(public(&m(serde_json::json!({}))).len(), 4);
    }

    #[test]
    fn the_answer_tool_name_is_taken_for_every_agent_a_typed_run_reaches() {
        let m = |specialist_tools: serde_json::Value| -> super::Manifest {
            serde_json::from_value(serde_json::json!({
                "name": "h",
                "routes": [
                    {"id": 0, "method": "POST", "path": "/final_answer", "op": {"kind": "static", "body": {}},
                     "tool": {"expose": true, "name": "final_answer"}},
                    {"id": 1, "method": "POST", "path": "/ok", "op": {"kind": "static", "body": {}},
                     "tool": {"expose": true, "name": "ok"}}
                ],
                "agents": [
                    {"name": "desk", "model": "m", "handoffs": ["triage"],
                     "output_schema": {"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]}},
                    {"name": "triage", "model": "m", "handoffs": ["specialist", "desk"]},
                    {"name": "specialist", "model": "m", "tools": specialist_tools}
                ]
            }))
            .unwrap()
        };
        let err = m(serde_json::json!(["final_answer"])).validate().unwrap_err();
        assert!(err.contains("\"desk\"") && err.contains("\"specialist\"") && err.contains("final_answer"), "{err}");
        m(serde_json::json!(["ok"])).validate().expect("no clash");
        assert_eq!(m(serde_json::json!([])).handoff_closure("desk"), vec!["triage", "specialist"]);
    }

    fn gate_manifest(flows: serde_json::Value) -> super::Manifest {
        serde_json::from_value(serde_json::json!({
            "name": "g",
            "routes": [
                {"id": 0, "method": "POST", "path": "/purge", "op": {"kind": "static", "body": {}},
                 "tool": {"expose": true, "name": "purge"}, "approval": "required"},
                {"id": 1, "method": "POST", "path": "/ok", "op": {"kind": "static", "body": {}},
                 "tool": {"expose": true, "name": "ok"}},
                {"id": 2, "method": "POST", "path": "/agents/asker", "op": {"kind": "agent", "agent": "asker"},
                 "tool": {"expose": true, "name": "asker"}},
                {"id": 3, "method": "POST", "path": "/flows/inner", "op": {"kind": "flow", "flow": "inner"},
                 "tool": {"expose": true, "name": "inner"}},
                {"id": 4, "method": "POST", "path": "/flows/outer", "op": {"kind": "flow", "flow": "outer"},
                 "tool": {"expose": true, "name": "outer"}}
            ],
            "agents": [{"name": "asker", "model": "m", "tools": ["purge"]}],
            "flows": flows,
        }))
        .unwrap()
    }

    #[test]
    fn a_flow_may_not_run_an_approval_gated_tool_as_a_step() {
        use serde_json::json;
        // Through 2.8.0 each of these booted, and the gated tool ran unattended
        // whenever the flow was called — by an agent, over MCP, from a behaviour.
        let cases = [
            json!([{"name": "inner", "kind": {"kind": "pipeline", "steps": [{"tool": "ok"}, {"tool": "purge"}]}},
                   {"name": "outer", "kind": {"kind": "pipeline", "steps": [{"tool": "ok"}]}}]),
            json!([{"name": "inner", "kind": {"kind": "parallel", "branches": [{"tool": "ok"}, {"tool": "purge"}]}},
                   {"name": "outer", "kind": {"kind": "pipeline", "steps": [{"tool": "ok"}]}}]),
            json!([{"name": "inner", "kind": {"kind": "route", "routes": {"a": {"tool": "ok"}}, "default": {"tool": "purge"}}},
                   {"name": "outer", "kind": {"kind": "pipeline", "steps": [{"tool": "ok"}]}}]),
        ];
        for flows in cases {
            let err = gate_manifest(flows.clone()).validate().unwrap_err();
            assert!(err.contains("\"inner\"") && err.contains("\"purge\""), "{err}");
        }
        // Nested: the outer flow reaches the gate through the inner one, and
        // the message names the chain.
        let nested = json!([
            {"name": "inner", "kind": {"kind": "pipeline", "steps": [{"tool": "purge"}]}},
            {"name": "outer", "kind": {"kind": "pipeline", "steps": [{"tool": "ok"}, {"tool": "inner"}]}}]);
        let m = gate_manifest(nested);
        assert_eq!(m.gated_tool_behind("outer").as_deref(), Some("purge"));
        assert_eq!(m.gated_tool_in_flow("outer").unwrap().0, vec!["outer", "inner"]);
        assert!(m.validate().is_err());

        // An agent step that calls the gated tool is fine: the agent suspends
        // and asks. So are flows naming each other in a cycle.
        let fine = json!([
            {"name": "inner", "kind": {"kind": "pipeline", "steps": [{"tool": "asker"}, {"tool": "outer"}]}},
            {"name": "outer", "kind": {"kind": "pipeline", "steps": [{"tool": "inner"}]}}]);
        let m = gate_manifest(fine);
        m.validate().expect("an agent step asks a human; a flow cycle has no gate");
        assert_eq!(m.gated_tool_behind("purge").as_deref(), Some("purge"));
        assert_eq!(m.gated_tool_behind("outer"), None);
        assert_eq!(m.gated_tool_behind("asker"), None);
        assert_eq!(m.gated_tool_behind("nope"), None);
    }
}
