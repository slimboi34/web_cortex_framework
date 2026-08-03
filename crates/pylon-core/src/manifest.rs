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
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    8000
}
fn default_control_prefix() -> String {
    "/_pylon".into()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            python_workers: None,
            control_prefix: default_control_prefix(),
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
}

impl Manifest {
    /// Validate cross-references that the type system can't catch, so that a
    /// misconfigured app fails at boot instead of on a request.
    pub fn validate(&self) -> Result<(), String> {
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
                Op::Query { .. } if self.database.is_none() => {
                    return Err(format!(
                        "route {} {} runs a query but no database is configured",
                        r.method, r.path
                    ));
                }
                _ => {}
            }
        }

        // Two routes answering to one tool name would make a model's tool call
        // ambiguous. Caught here rather than at bind time so `pylon check`
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

        for a in &self.agents {
            for t in &a.tools {
                if !tool_names.contains(t.as_str()) {
                    return Err(format!(
                        "agent {:?} references tool {:?}, which is not an exposed route",
                        a.name, t
                    ));
                }
            }
        }
        Ok(())
    }
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
