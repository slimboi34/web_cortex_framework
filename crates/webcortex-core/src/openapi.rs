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
            .entry(openapi_path(&r.path))
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


/// A route path as an OpenAPI path template: `{*rest}` is `{rest}`, since a
/// catch-all marker is not legal in an OpenAPI template.
fn openapi_path(path: &str) -> String {
    path.replace("{*", "{")
}

fn operation(r: &Route) -> Value {
    let mut params = Vec::new();
    let in_path = path_param_names(r);
    for name in &in_path {
        params.push(json!({
            "name": name,
            "in": "path",
            "required": true,
            "schema": property_schema(r, name).unwrap_or_else(|| json!({"type": "string"})),
        }));
    }
    // A method without a body reads its other inputs from the query string
    // (path → query → body), so they are query parameters. 2.5.3 documented
    // only the path ones, and a client generated from the spec could not send
    // the rest.
    let has_body = matches!(r.method.as_str(), "POST" | "PUT" | "PATCH");
    if !has_body
        && let Some(schema) = r.input_schema.as_ref()
        && let Some(props) = schema.get("properties").and_then(|p| p.as_object())
    {
        let required = required_names(schema);
        for (name, prop) in props {
            if in_path.contains(name) {
                continue;
            }
            params.push(json!({
                "name": name,
                "in": "query",
                "required": required.contains(name),
                "schema": prop,
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
    if has_body
        && let Some(schema) = body_schema(r) {
            // Required only when the schema requires a field of it.
            let required = !required_names(&schema).is_empty();
            op.as_object_mut().unwrap().insert(
                "requestBody".into(),
                json!({
                    "required": required,
                    "content": {"application/json": {"schema": schema}}
                }),
            );
        }
    op
}

fn required_names(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn path_param_names(r: &Route) -> Vec<String> {
    r.path
        .split('/')
        .filter_map(|s| s.strip_prefix('{').and_then(|x| x.strip_suffix('}')))
        .map(|s| s.trim_start_matches('*').to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(routes: Value) -> Manifest {
        serde_json::from_value(json!({"name": "t", "routes": routes})).expect("manifest")
    }

    #[test]
    fn query_parameters_and_catch_alls_are_documented() {
        let m = manifest(json!([
            {"id": 0, "method": "GET", "path": "/books/{id}/blurb",
             "op": {"kind": "python", "handler": 0},
             "input_schema": {"type": "object", "required": ["id"], "properties": {
                 "id": {"type": "integer"}, "style": {"type": "string"}}}},
            {"id": 1, "method": "GET", "path": "/files/{*rest}",
             "op": {"kind": "python", "handler": 1}},
            {"id": 2, "method": "POST", "path": "/notes",
             "op": {"kind": "python", "handler": 2},
             "input_schema": {"type": "object", "properties": {"tag": {"type": "string"}}}},
        ]));
        let spec = generate(&m);
        let blurb = &spec["paths"]["/books/{id}/blurb"]["get"]["parameters"];
        assert_eq!(blurb[0]["in"], "path");
        assert_eq!(blurb[0]["schema"]["type"], "integer");
        assert_eq!(blurb[1], json!({"name": "style", "in": "query", "required": false, "schema": {"type": "string"}}));
        let rest = &spec["paths"]["/files/{rest}"]["get"]["parameters"][0];
        assert_eq!(rest["name"], "rest");
        assert_eq!(spec["paths"]["/notes"]["post"]["requestBody"]["required"], false);
    }
}
