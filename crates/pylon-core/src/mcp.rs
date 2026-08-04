//! Model Context Protocol server, served over Streamable HTTP JSON-RPC.
//!
//! This is the feature that earns the "for AI agents" framing. Any route marked
//! `tool=True` is callable by any MCP client the moment the server boots — no
//! separate tool server, no hand-maintained schema, no drift. Tool calls land on
//! `App::dispatch` in-process, so an agent pays a function call rather than a
//! loopback HTTP round trip.

use crate::app::App;
use crate::auth::Principal;
use crate::http::PylonResponse;
use serde_json::{Value, json};

/// The MCP revision this server implements.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

pub async fn handle(app: &App, body: &[u8], caller: &Principal) -> PylonResponse {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return jsonrpc_response(error_obj(Value::Null, -32700, format!("parse error: {e}")));
        }
    };

    match parsed {
        // JSON-RPC batch.
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(res) = handle_one(app, item, caller).await {
                    out.push(res);
                }
            }
            if out.is_empty() {
                return accepted();
            }
            jsonrpc_response(Value::Array(out))
        }
        single => match handle_one(app, single, caller).await {
            Some(res) => jsonrpc_response(res),
            // Notifications get 202 with no body, per the spec.
            None => accepted(),
        },
    }
}

/// Returns `None` for notifications, which must not receive a response.
async fn handle_one(app: &App, req: Value, caller: &Principal) -> Option<Value> {
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
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": app.manifest.name,
                "version": app.manifest.version,
            },
            "instructions": if app.manifest.description.is_empty() {
                format!("Tools exposed by the {} Pylon application.", app.manifest.name)
            } else {
                app.manifest.description.clone()
            },
        })),

        "ping" => Ok(json!({})),

        // Only tools this caller could actually invoke are advertised. Listing
        // a tool the caller cannot use invites the model to plan around it and
        // then fail, and it discloses the shape of the privileged surface.
        "tools/list" => Ok(json!({ "tools": tool_descriptors(app, caller) })),

        "tools/call" => call_tool(app, &params, caller).await,

        other => Err((-32601, format!("method not found: {other}"))),
    };

    Some(match result {
        Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
        Err((code, message)) => error_obj(id, code, message),
    })
}

fn tool_descriptors(app: &App, caller: &Principal) -> Vec<Value> {
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
                },
            })
        })
        .collect()
}

async fn call_tool(app: &App, params: &Value, caller: &Principal) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or((-32602, "tools/call requires a 'name'".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    // A gated tool is not callable straight off the MCP surface: approval gates
    // exist so a human sees the call first, and honouring them only inside the
    // agent loop would leave an obvious way around them.
    if let Some(route) = app.route_for_tool(name) {
        if route.approval == crate::manifest::Approval::Required {
            return Ok(json!({
                "content": [{"type": "text", "text": format!(
                    "tool {name:?} requires human approval and cannot be invoked directly over MCP"
                )}],
                "isError": true,
            }));
        }
    }

    // The MCP caller's own principal is used — no scope substitution. This is
    // the same authority the caller would have over plain HTTP.
    match app.call_tool_as(name, &args, caller).await {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            Ok(json!({
                "content": [{"type": "text", "text": text}],
                "structuredContent": value,
                "isError": false,
            }))
        }
        // A failing tool is a successful protocol exchange reporting an error,
        // so the model can read it and adapt rather than seeing a transport fault.
        Err(e) => Ok(json!({
            "content": [{"type": "text", "text": e}],
            "isError": true,
        })),
    }
}

fn error_obj(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn jsonrpc_response(value: Value) -> PylonResponse {
    PylonResponse::json(200, &value)
}

fn accepted() -> PylonResponse {
    PylonResponse {
        status: 202,
        headers: vec![],
        body: bytes::Bytes::new(),
    }
}
