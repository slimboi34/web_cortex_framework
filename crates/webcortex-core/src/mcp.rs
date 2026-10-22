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

/// Revisions a client may ask for and get back unchanged. Everything this
/// server does — tools, resources, `structuredContent` aside, which older
/// clients ignore — reads the same in each. Answering every client with the
/// newest revision told a 2024-11-05 client the server spoke something it did
/// not, and the spec has a client disconnect when that happens.
pub const SUPPORTED_VERSIONS: [&str; 3] = [PROTOCOL_VERSION, "2025-03-26", "2024-11-05"];

/// The revision to answer `initialize` with: the client's, when this server
/// supports it, otherwise the newest this server has.
pub fn negotiate_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|r| SUPPORTED_VERSIONS.iter().find(|v| **v == r))
        .copied()
        .unwrap_or(PROTOCOL_VERSION)
}

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
            // An empty batch is itself an invalid request (JSON-RPC 2.0 §6).
            if items.is_empty() {
                return jsonrpc_response(error_obj(Value::Null, -32600, "an empty batch is not a request".into(), "WC205"));
            }
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
    // Something that is not a request object at all — `[1, 2]`, `"x"` — was
    // read as a notification and dropped without a word. JSON-RPC answers it
    // with an Invalid Request error, id null.
    let Some(obj) = req.as_object() else {
        return Some(error_obj(Value::Null, -32600, "a JSON-RPC message must be an object".into(), "WC205"));
    };
    let method = obj.get("method").and_then(|m| m.as_str());
    let id = obj.get("id").cloned();

    let Some(method) = method else {
        // A response to a request: this server sends none, so there is nothing
        // to match it to, and a response must never be answered.
        if id.is_some() && (obj.contains_key("result") || obj.contains_key("error")) {
            tracing::debug!("mcp: ignored a JSON-RPC response from the client");
            return None;
        }
        let message = if obj.contains_key("method") { "'method' must be a string" } else { "a request needs a 'method'" };
        return Some(error_obj(valid_id(id.as_ref()), -32600, message.into(), "WC205"));
    };

    let Some(id) = id else {
        // A notification. Nothing we currently track needs to react.
        tracing::debug!(method, "mcp notification");
        return None;
    };
    // MCP ids are strings or integers, never null; JSON-RPC allows numbers.
    // An object, array, bool or null id was echoed back as if it were fine.
    if !(id.is_string() || id.is_number()) {
        return Some(error_obj(Value::Null, -32600, "'id' must be a string or a number".into(), "WC205"));
    }
    // MCP params are always an object. An empty list is read as "no params",
    // which is what a client that sends `[]` means; anything else is refused
    // rather than silently replaced with nothing.
    let params = match obj.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(Value::Array(a)) if a.is_empty() => json!({}),
        Some(p @ Value::Object(_)) => p.clone(),
        Some(_) => {
            return Some(error_obj(id, -32602, "'params' must be an object".into(), "WC206"));
        }
    };

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": negotiate_version(params.get("protocolVersion").and_then(|v| v.as_str())),
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
            let wc = match code {
                -32601 => "WC212",
                -32602 if message.starts_with("unknown tool") => "WC314",
                -32602 => "WC206",
                _ => "WC205",
            };
            let mut obj = error_obj(id, code, message, wc);
            if code == -32601 {
                // WC212's own hint lists control-plane GET paths, which is no
                // help to an MCP client that sent an unknown method.
                obj["error"]["data"]["hint"] = json!(
                    "This MCP server implements initialize, ping, tools/list, tools/call, \
                     resources/list and resources/read."
                );
            }
            obj
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
                "section": {"type": "string", "enum": STATUS_SECTIONS}
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

/// The sections `webcortex_status` can return on their own.
const STATUS_SECTIONS: [&str; 7] = ["requests", "errors", "agents", "usage", "budgets", "estop", "devices"];

/// A built-in tool's error result with a hint of its own, shaped like [`tool_error`].
fn builtin_error(code: &str, message: String, hint: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": format!("{message}\n\n[{code}] {hint}")}],
        "structuredContent": {"error": {"code": code, "message": message, "hint": hint}},
        "isError": true,
    })
}

/// A built-in tool call, or `None` when `name` is not one (or an app tool shadows it).
fn call_builtin(app: &App, name: &str, args: &Value) -> Option<Value> {
    if app.route_for_tool(name).is_some() {
        return None;
    }
    let value = match name {
        STATUS_TOOL => {
            let all = app.status();
            match args.get("section").and_then(|s| s.as_str()).filter(|s| !s.is_empty()) {
                None => all,
                Some(section) if STATUS_SECTIONS.contains(&section) && all.get(section).is_some() => {
                    json!({ section: all[section] })
                }
                // A typo used to return the whole document silently, so the
                // caller could not tell its filter had been ignored.
                Some(section) => {
                    return Some(builtin_error(
                        "WC206",
                        format!("unknown status section {section:?}"),
                        &format!("Pass one of {}, or omit section for everything.", STATUS_SECTIONS.join(", ")),
                    ));
                }
            }
        }
        EXPLAIN_TOOL => {
            let code = args.get("code").and_then(|c| c.as_str()).unwrap_or("").trim().to_ascii_uppercase();
            if code.is_empty() {
                return Some(builtin_error(
                    "WC206",
                    "missing required argument \"code\"".into(),
                    "Pass the code to explain, e.g. {\"code\": \"WC402\"}; read webcortex://errors for the list.",
                ));
            }
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
    // An argument list or a bare value was coerced to `{}`, and the tool ran
    // with none of what the client meant to pass.
    let args = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(a @ Value::Object(_)) => a.clone(),
        Some(_) => return Err((-32602, "tools/call 'arguments' must be an object".to_string())),
    };

    if may_observe(app, caller)
        && let Some(result) = call_builtin(app, name, &args) {
            return Ok(result);
        }

    // MCP (2025-06-18, "Error Handling") makes an unknown tool a protocol
    // error, -32602, not a tool result. Builtins the caller may not observe are
    // absent from `tools/list`, so they are unknown here too.
    if app.route_for_tool(name).is_none() {
        app.recorder().record_failure("mcp", &format!("tools/call {name}"), "WC314", request_id);
        return Err((-32602, format!("unknown tool {name:?}")));
    }

    // A gated tool is not callable straight off the MCP surface: approval gates
    // exist so a human sees the call first, and honouring them only inside the
    // agent loop would leave an obvious way around them.
    if let Some(route) = app.route_for_tool(name)
        && route.approval == crate::manifest::Approval::Required {
            let message = format!("tool {name:?} requires human approval and cannot be invoked directly over MCP");
            app.recorder().record_failure("mcp", &format!("tools/call {name}"), "WC309", request_id);
            return Ok(tool_error(&message, "WC309"));
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

/// The id to answer an invalid request with: its own when that is usable,
/// otherwise null.
fn valid_id(id: Option<&Value>) -> Value {
    match id {
        Some(v) if v.is_string() || v.is_number() => v.clone(),
        _ => Value::Null,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    async fn app() -> std::sync::Arc<App> {
        let m: Manifest = serde_json::from_value(json!({
            "name": "m",
            "routes": [
                {"id": 0, "method": "POST", "path": "/echo", "op": {"kind": "static", "body": {"ok": true}},
                 "tool": {"expose": true, "name": "echo"}},
                {"id": 1, "method": "GET", "path": "/list", "op": {"kind": "static", "body": [1, 2, 3]},
                 "tool": {"expose": true, "name": "list"}}
            ]
        }))
        .unwrap();
        App::build_without_python(m).await.expect("app").into_arc()
    }

    fn who() -> Principal {
        Principal::anonymous()
    }

    async fn send(app: &App, body: Value) -> (u16, Value) {
        let res = handle(app, &serde_json::to_vec(&body).unwrap(), &who()).await;
        let status = res.status;
        (status, if res.body.is_empty() { Value::Null } else { res.json_value() })
    }

    #[tokio::test]
    async fn initialize_answers_with_the_clients_revision_when_supported() {
        let app = app().await;
        for (asked, answered) in [
            (json!("2024-11-05"), "2024-11-05"),
            (json!("2025-03-26"), "2025-03-26"),
            (json!("2025-06-18"), "2025-06-18"),
            (json!("2099-01-01"), PROTOCOL_VERSION),
            (json!(7), PROTOCOL_VERSION),
            (Value::Null, PROTOCOL_VERSION),
        ] {
            let (_, out) = send(&app, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                                            "params": {"protocolVersion": asked}})).await;
            assert_eq!(out["result"]["protocolVersion"], answered, "asked {asked}");
        }
    }

    #[tokio::test]
    async fn invalid_messages_get_an_invalid_request_error_not_silence() {
        let app = app().await;
        // 2.5.3 read each of these as a notification and answered 202, empty.
        let (status, out) = send(&app, json!([1, "x"])).await;
        assert_eq!(status, 200);
        let errors = out.as_array().expect("one error per item");
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().all(|e| e["error"]["code"] == -32600 && e["id"].is_null()));

        let (_, out) = send(&app, json!(5)).await;
        assert_eq!(out["error"]["code"], -32600);
        let (_, out) = send(&app, json!([])).await;
        assert_eq!(out["error"]["code"], -32600, "an empty batch is invalid");

        let (_, out) = send(&app, json!({"jsonrpc": "2.0", "id": 3})).await;
        assert_eq!((out["error"]["code"].clone(), out["id"].clone()), (json!(-32600), json!(3)));
        let (_, out) = send(&app, json!({"jsonrpc": "2.0", "id": 3, "method": 9})).await;
        assert_eq!(out["error"]["code"], -32600);

        for bad in [json!({"a": 1}), json!([1]), json!(true), Value::Null] {
            let (_, out) = send(&app, json!({"jsonrpc": "2.0", "id": bad, "method": "ping"})).await;
            assert_eq!(out["error"]["code"], -32600, "id {bad}");
            assert!(out["id"].is_null());
        }
        // Numbers and strings are fine, and are echoed exactly.
        for good in [json!(0), json!(-4), json!(1.5), json!("abc"), json!("")] {
            let (_, out) = send(&app, json!({"jsonrpc": "2.0", "id": good, "method": "ping"})).await;
            assert_eq!(out["id"], good);
            assert!(out.get("result").is_some());
        }
    }

    #[tokio::test]
    async fn a_response_from_the_client_is_not_answered() {
        let app = app().await;
        let (status, out) = send(&app, json!({"jsonrpc": "2.0", "id": 4, "result": {}})).await;
        assert_eq!((status, out), (202, Value::Null));
        let (status, _) = send(&app, json!({"jsonrpc": "2.0", "id": 5, "error": {"code": 1, "message": "x"}})).await;
        assert_eq!(status, 202);
        // In a batch beside a request, only the request is answered.
        let (_, out) = send(&app, json!([{"jsonrpc": "2.0", "id": 4, "result": {}},
                                         {"jsonrpc": "2.0", "id": 6, "method": "ping"},
                                         {"jsonrpc": "2.0", "method": "notifications/initialized"}])).await;
        assert_eq!(out.as_array().unwrap().len(), 1);
        assert_eq!(out[0]["id"], 6);
    }

    #[tokio::test]
    async fn tool_arguments_that_are_not_an_object_are_refused() {
        let app = app().await;
        for bad in [json!([1]), json!("n=3"), json!(4)] {
            let (_, out) = send(&app, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                            "params": {"name": "echo", "arguments": bad}})).await;
            assert_eq!(out["error"]["code"], -32602, "{bad}");
            assert_eq!(out["error"]["data"]["code"], "WC206");
        }
        let (_, out) = send(&app, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                        "params": {"name": "echo", "arguments": null}})).await;
        assert_eq!(out["result"]["isError"], false, "null is no arguments");
        let (_, out) = send(&app, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": [1]})).await;
        assert_eq!(out["error"]["code"], -32602);
        let (_, out) = send(&app, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": []})).await;
        assert!(out["result"]["tools"].is_array(), "an empty params list is no params");
    }

    #[tokio::test]
    async fn structured_content_is_always_an_object_and_a_full_batch_is_answered_in_order() {
        let app = app().await;
        let calls: Vec<Value> = (0..MAX_BATCH)
            .map(|i| json!({"jsonrpc": "2.0", "id": i, "method": "tools/call",
                            "params": {"name": if i % 2 == 0 { "echo" } else { "list" }}}))
            .collect();
        let (_, out) = send(&app, Value::Array(calls)).await;
        let out = out.as_array().unwrap();
        assert_eq!(out.len(), MAX_BATCH);
        for (i, r) in out.iter().enumerate() {
            assert_eq!(r["id"], i);
            assert!(r["result"]["structuredContent"].is_object());
        }
        assert_eq!(out[1]["result"]["structuredContent"], json!({"result": [1, 2, 3]}));
    }
}
