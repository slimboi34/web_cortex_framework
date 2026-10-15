//! Model Context Protocol server, served over Streamable HTTP JSON-RPC.
//!
//! This is the feature that earns the "for AI agents" framing. Any route marked
//! `tool=True` is callable by any MCP client the moment the server boots — no
//! separate tool server, no hand-maintained schema, no drift. Tool calls land on
//! `App::dispatch` in-process, so an agent pays a function call rather than a
//! loopback HTTP round trip.

use crate::app::App;
use crate::auth::Principal;
use crate::http::WebCortexResponse;
use serde_json::{Value, json};

/// The MCP revision this server implements.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Most messages one JSON-RPC batch may carry. Each one can be a tool call, and
/// the rate limiter sees a batch as a single request, so an unbounded batch
/// turned one admitted request into any number of tool runs. (MCP 2025-06-18
/// dropped batching; it is still accepted for older clients, within this cap.)
pub const MAX_BATCH: usize = 32;

/// Built-in tools every app's MCP endpoint offers to callers who may read the
/// control plane, so a coding agent connected with `webcortex mcp-config` can
/// ask what the running app is doing. An app tool of the same name wins.
pub const STATUS_TOOL: &str = "webcortex_status";
pub const EXPLAIN_TOOL: &str = "webcortex_explain";
pub const STATUS_URI: &str = "webcortex://status";
pub const ERRORS_URI: &str = "webcortex://errors";

pub async fn handle(app: &App, body: &[u8], caller: &Principal) -> WebCortexResponse {
    handle_request(app, body, caller, "").await
}

/// As [`handle`], with the HTTP request id so a failing tool call can be found
/// in the status view's recent errors.
pub async fn handle_request(app: &App, body: &[u8], caller: &Principal, request_id: &str) -> WebCortexResponse {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return jsonrpc_response(error_obj(Value::Null, -32700, format!("parse error: {e}"), "WC205"));
        }
    };

    match parsed {
        // JSON-RPC batch.
        Value::Array(items) => {
            if items.len() > MAX_BATCH {
                return jsonrpc_response(error_obj(
                    Value::Null,
                    -32600,
                    format!("a batch may hold at most {MAX_BATCH} messages ({} sent)", items.len()),
                    "WC405",
                ));
            }
            let mut out = Vec::new();
            for item in items {
                if let Some(res) = handle_one(app, item, caller, request_id).await {
                    out.push(res);
                }
            }
            if out.is_empty() {
                return accepted();
            }
            jsonrpc_response(Value::Array(out))
        }
        single => match handle_one(app, single, caller, request_id).await {
            Some(res) => jsonrpc_response(res),
            // Notifications get 202 with no body, per the spec.
            None => accepted(),
        },
    }
}

/// Returns `None` for notifications, which must not receive a response.
async fn handle_one(app: &App, req: Value, caller: &Principal, request_id: &str) -> Option<Value> {
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = req.get("id").cloned();
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    if id.is_none() {
        // A notification. Nothing we currently track needs to react.
        tracing::debug!(method, "mcp notification");
        return None;
    }
    let id = id.unwrap();

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "listChanged": false, "subscribe": false },
            },
            "serverInfo": {
                "name": app.manifest.name,
                "version": app.manifest.version,
            },
            "instructions": if app.manifest.description.is_empty() {
                format!("Tools exposed by the {} WebCortex application.", app.manifest.name)
            } else {
                app.manifest.description.clone()
            },
        })),

        "ping" => Ok(json!({})),

        // Only tools this caller could actually invoke are advertised. Listing
        // a tool the caller cannot use invites the model to plan around it and
        // then fail, and it discloses the shape of the privileged surface.
        "tools/list" => Ok(json!({ "tools": tool_descriptors(app, caller) })),

        "tools/call" => call_tool(app, &params, caller, request_id).await,

        "resources/list" => Ok(json!({
            "resources": if may_observe(app, caller) { builtin_resources() } else { vec![] }
        })),

        "resources/read" => read_resource(app, &params, caller),

        other => Err((-32601, format!("method not found: {other}"))),
    };

    Some(match result {
        Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
        Err((code, message)) => {
            let wc = if code == -32601 { "WC212" } else { "WC205" };
            error_obj(id, code, message, wc)
        }
    })
}

/// The live view is control-plane data: visible to whoever may read the control
/// plane — anyone when no auth is configured, otherwise `webcortex:admin`.
/// (Reaching `/mcp` at all already needs that; this keeps the rule true for
/// any other caller of [`handle`].)
fn may_observe(app: &App, caller: &Principal) -> bool {
    let auth = &app.manifest.auth;
    (auth.api_keys.is_empty() && auth.jwt.is_none()) || caller.has_scope("webcortex:admin")
}

fn builtin_resources() -> Vec<Value> {
    vec![
        json!({
            "uri": STATUS_URI,
            "name": "status",
            "title": "Live status",
            "description": "What the running app is doing: requests per route, recent errors with codes, \
                            agent runs in flight and recent, spend, budgets, e-stop, devices.",
            "mimeType": "application/json",
        }),
        json!({
            "uri": ERRORS_URI,
            "name": "errors",
            "title": "Error catalogue",
            "description": "Every WCnnn error code with its meaning and what to do about it.",
            "mimeType": "application/json",
        }),
    ]
}

fn read_resource(app: &App, params: &Value, caller: &Principal) -> Result<Value, (i64, String)> {
    let uri = params.get("uri").and_then(|u| u.as_str()).unwrap_or("");
    if !may_observe(app, caller) {
        return Err((-32002, format!("resource not found: {uri}")));
    }
    let value = match uri {
        STATUS_URI => app.status(),
        ERRORS_URI => json!({"errors": crate::errors::catalogue_json()}),
        _ => return Err((-32002, format!("resource not found: {uri}"))),
    };
    Ok(json!({"contents": [{
        "uri": uri,
        "mimeType": "application/json",
        "text": serde_json::to_string_pretty(&value).unwrap_or_default(),
    }]}))
}

fn builtin_tools(app: &App) -> Vec<Value> {
    let mut out = Vec::new();
    if app.route_for_tool(STATUS_TOOL).is_none() {
        out.push(json!({
            "name": STATUS_TOOL,
            "description": "What is this WebCortex app doing right now? Uptime, versions, requests per route \
                            and status class, latency, recent errors (code, route, request id), agent runs \
                            in flight and recent (steps, tokens, cost, outcome), budgets, e-stop, devices \
                            and watchers. Pass section to get one part only.",
            "inputSchema": {"type": "object", "properties": {
                "section": {"type": "string", "enum": ["requests", "errors", "agents", "usage", "budgets", "estop", "devices"]}
            }},
            "annotations": {"readOnlyHint": true, "idempotentHint": true, "destructiveHint": false,
                            "openWorldHint": false, "requiresApproval": false, "actuator": false},
        }));
    }
    if app.route_for_tool(EXPLAIN_TOOL).is_none() {
        out.push(json!({
            "name": EXPLAIN_TOOL,
            "description": "Explain a WebCortex error code (WCnnn) from an error body, a run result or the \
                            status view: what it means and what to do about it.",
            "inputSchema": {"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]},
            "annotations": {"readOnlyHint": true, "idempotentHint": true, "destructiveHint": false,
                            "openWorldHint": false, "requiresApproval": false, "actuator": false},
        }));
    }
    out
}

/// A built-in tool call, or `None` when `name` is not one (or an app tool shadows it).
fn call_builtin(app: &App, name: &str, args: &Value) -> Option<Value> {
    if app.route_for_tool(name).is_some() {
        return None;
    }
    let value = match name {
        STATUS_TOOL => {
            let all = app.status();
            match args.get("section").and_then(|s| s.as_str()) {
                Some(section) if all.get(section).is_some() => json!({ section: all[section] }),
                _ => all,
            }
        }
        EXPLAIN_TOOL => {
            let code = args.get("code").and_then(|c| c.as_str()).unwrap_or("").trim().to_ascii_uppercase();
            match crate::errors::lookup(&code) {
                Some(e) => json!({"code": e.code, "title": e.title, "category": e.category,
                                  "status": e.status, "hint": e.hint}),
                None => {
                    return Some(json!({
                        "content": [{"type": "text", "text": format!("{code:?} is not a WebCortex error code")}],
                        "structuredContent": {"error": {"code": "WC205", "message": "unknown error code",
                                                        "hint": "Codes look like WC201; read webcortex://errors for the list."}},
                        "isError": true,
                    }));
                }
            }
        }
        _ => return None,
    };
    Some(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default()}],
        "structuredContent": value,
        "isError": false,
    }))
}

fn tool_descriptors(app: &App, caller: &Principal) -> Vec<Value> {
    let mut tools = app_tool_descriptors(app, caller);
    if may_observe(app, caller) {
        tools.extend(builtin_tools(app));
    }
    tools
}

fn app_tool_descriptors(app: &App, caller: &Principal) -> Vec<Value> {
    app.exposed_tools()
        .into_iter()
        .filter(|r| {
            let mut required = r.scopes.clone();
            required.extend(r.tool.scopes.iter().cloned());
            caller.missing_scopes(&required).is_empty()
        })
        .map(|r| {
            let description = if r.description.is_empty() {
                if r.summary.is_empty() {
                    format!("{} {}", r.method, r.path)
                } else {
                    r.summary.clone()
                }
            } else {
                r.description.clone()
            };
            json!({
                "name": r.tool_name(),
                "description": description,
                "inputSchema": r.input_schema.clone().unwrap_or_else(|| json!({
                    "type": "object", "properties": {}
                })),
                "annotations": {
                    "readOnlyHint": r.tool.read_only,
                    "idempotentHint": r.tool.idempotent,
                    // Surfaced so a client can warn a user before a call that
                    // will block on human approval.
                    "requiresApproval": r.approval == crate::manifest::Approval::Required,
                    // A tool that moves hardware acts on the world outside the
                    // app; clients use these hints to decide when to confirm.
                    "destructiveHint": r.actuator,
                    "openWorldHint": r.actuator,
                    "actuator": r.actuator,
                },
            })
        })
        .collect()
}

async fn call_tool(app: &App, params: &Value, caller: &Principal, request_id: &str) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or((-32602, "tools/call requires a 'name'".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    if may_observe(app, caller) {
        if let Some(result) = call_builtin(app, name, &args) {
            return Ok(result);
        }
    }

    // A gated tool is not callable straight off the MCP surface: approval gates
    // exist so a human sees the call first, and honouring them only inside the
    // agent loop would leave an obvious way around them.
    if let Some(route) = app.route_for_tool(name) {
        if route.approval == crate::manifest::Approval::Required {
            let message = format!("tool {name:?} requires human approval and cannot be invoked directly over MCP");
            app.recorder().record_failure("mcp", &format!("tools/call {name}"), "WC309", request_id);
            return Ok(tool_error(&message, "WC309"));
        }
    }

    // The MCP caller's own principal is used — no scope substitution. This is
    // the same authority the caller would have over plain HTTP.
    match app.call_tool_as(name, &args, caller).await {
        Ok(value) => {
            // An image a tool returned becomes an MCP image item beside the text, so
            // the client's model sees the frame; the text and the structured copy
            // keep a placeholder and the size, not megabytes of base64.
            let (shown, images) = crate::agent::vision::extract(&value, crate::agent::vision::MAX_IMAGES);
            let text = serde_json::to_string_pretty(&shown).unwrap_or_else(|_| shown.to_string());
            let value = crate::agent::vision::redact(&value);
            // MCP requires `structuredContent` to be a JSON object. A tool whose result is a
            // list or a scalar — every `list_*` resource route, for one — is wrapped as
            // `{"result": …}`; an object passes through unchanged. Strict clients (Claude
            // Code among them) reject the bare value as malformed, which turned a working
            // list route into a tool the model could not use.
            let structured = if value.is_object() { value.clone() } else { json!({"result": value}) };
            let mut content = vec![json!({"type": "text", "text": text})];
            content.extend(images.iter().filter_map(mcp_image));
            Ok(json!({
                "content": content,
                "structuredContent": structured,
                "isError": false,
            }))
        }
        // A failing tool is a successful protocol exchange reporting an error,
        // so the model can read it and adapt rather than seeing a transport fault.
        Err(e) => {
            let code = crate::errors::code_in(&e).unwrap_or(if e.starts_with("unknown tool") { "WC314" } else { "WC308" });
            app.recorder().record_failure("mcp", &format!("tools/call {name}"), code, request_id);
            Ok(tool_error(&e, code))
        }
    }
}

/// An MCP tool error: the message as text for the model, and the code and hint
/// as structured content for a client that branches on them.
fn tool_error(message: &str, code: &str) -> Value {
    let hint = crate::errors::hint(code);
    json!({
        "content": [{"type": "text", "text": format!("{message}\n\n[{code}] {hint}")}],
        "structuredContent": {"error": {"code": code, "message": message, "hint": hint}},
        "isError": true,
    })
}

/// A canonical base64 image block as an MCP image content item. URL images
/// have no MCP form; their placeholder text already carries the URL.
fn mcp_image(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    if source.get("type")?.as_str()? != "base64" {
        return None;
    }
    Some(json!({
        "type": "image",
        "data": source.get("data")?,
        "mimeType": source.get("media_type")?,
    }))
}

fn error_obj(id: Value, code: i64, message: String, wc: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {
        "code": code,
        "message": message,
        "data": {"code": wc, "hint": crate::errors::hint(wc)},
    }})
}

fn jsonrpc_response(value: Value) -> WebCortexResponse {
    WebCortexResponse::json(200, &value)
}

fn accepted() -> WebCortexResponse {
    WebCortexResponse {
        status: 202,
        headers: vec![],
        body: bytes::Bytes::new(),
    }
}
