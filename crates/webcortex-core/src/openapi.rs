//! OpenAPI 3.1 generation from the route table.
//!
//! Not a separate annotation pass — the same `input_schema` the MCP layer hands
//! to a model is the schema documented here. One declaration, three consumers:
//! humans, generated frontend clients, and agents.

use crate::manifest::{Manifest, Op, Route};
use serde_json::{Map, Value, json};

pub fn generate(m: &Manifest) -> Value {
    let mut paths: Map<String, Value> = Map::new();

    for r in &m.routes {
        if matches!(r.op, Op::Page { .. } | Op::Files { .. }) {
            continue;
        }
        let entry = paths
            .entry(r.path.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        let obj = entry.as_object_mut().expect("path item is an object");
        obj.insert(r.method.to_ascii_lowercase(), operation(r));
    }

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": m.name,
            "version": m.version,
            "description": m.description,
        },
        "paths": Value::Object(paths),
        // Non-standard but harmless, and it lets an agent discover the MCP
        // endpoint straight from the spec it already fetched.
        "x-webcortex": {
            "mcp_endpoint": format!("{}/mcp", m.server.control_prefix),
            "tools": m.routes.iter().filter(|r| r.tool.expose).map(|r| r.tool_name()).collect::<Vec<_>>(),
            "agents": m.agents.iter().map(|a| &a.name).collect::<Vec<_>>(),
            "flows": m.flows.iter().map(|f| &f.name).collect::<Vec<_>>(),
        }
    })
}


fn operation(r: &Route) -> Value {
    let mut params = Vec::new();
    for seg in r.path.split('/') {
        if let Some(name) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            params.push(json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": property_schema(r, name).unwrap_or_else(|| json!({"type": "string"})),
            }));
        }
    }

    let mut op = json!({
        "operationId": r.tool_name(),
        "summary": if r.summary.is_empty() { r.tool_name() } else { r.summary.clone() },
        "description": r.description,
        "parameters": params,
        "responses": {
            "200": {
                "description": "Success",
                "content": {
                    "application/json": {
                        "schema": r.output_schema.clone().unwrap_or_else(|| json!({}))
                    }
                }
            }
        },
        "x-webcortex-op": r.op.kind(),
        "x-webcortex-tool": r.tool.expose,
    });

    // Only methods with a body get a requestBody, and path params are stripped
    // from it so the schema matches what a client actually sends.
    if matches!(r.method.as_str(), "POST" | "PUT" | "PATCH") {
        if let Some(schema) = body_schema(r) {
            op.as_object_mut().unwrap().insert(
                "requestBody".into(),
                json!({
                    "required": true,
                    "content": {"application/json": {"schema": schema}}
                }),
            );
        }
    }
    op
}

fn path_param_names(r: &Route) -> Vec<String> {
    r.path
        .split('/')
        .filter_map(|s| s.strip_prefix('{').and_then(|x| x.strip_suffix('}')))
        .map(|s| s.to_string())
        .collect()
}

fn property_schema(r: &Route, name: &str) -> Option<Value> {
    r.input_schema
        .as_ref()?
        .get("properties")?
        .get(name)
        .cloned()
}

fn body_schema(r: &Route) -> Option<Value> {
    let schema = r.input_schema.as_ref()?;
    let in_path = path_param_names(r);
    if in_path.is_empty() {
        return Some(schema.clone());
    }
    let mut cloned = schema.clone();
    if let Some(props) = cloned.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for name in &in_path {
            props.remove(name);
        }
    }
    if let Some(req) = cloned.get_mut("required").and_then(|r| r.as_array_mut()) {
        req.retain(|v| v.as_str().map(|s| !in_path.contains(&s.to_string())).unwrap_or(true));
    }
    Some(cloned)
}
