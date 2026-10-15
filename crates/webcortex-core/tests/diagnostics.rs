//! Error codes, hints and the live status view, end to end through `App`.

use serde_json::{Value, json};
use std::sync::Arc;
use webcortex_core::agent::provider::FakeProvider;
use webcortex_core::{AgentRuntime, App, Manifest, MemoryAudit, Principal, RunOptions, WebCortexRequest, mcp};

async fn app() -> Arc<App> {
    let manifest: Manifest = serde_json::from_value(json!({
        "name": "diag",
        "routes": [
            {"id": 0, "method": "GET", "path": "/ping", "op": {"kind": "static", "body": {"pong": true}},
             "tool": {"expose": true, "name": "ping"}},
            {"id": 1, "method": "POST", "path": "/arm/move", "op": {"kind": "static", "body": {"moved": true}},
             "tool": {"expose": true, "name": "move_arm"}, "actuator": true},
            {"id": 2, "method": "GET", "path": "/secret", "op": {"kind": "static", "body": {}},
             "scopes": ["admin"]},
            {"id": 3, "method": "POST", "path": "/agents/desk", "op": {"kind": "agent", "agent": "desk"}},
            {"id": 4, "method": "POST", "path": "/agents/tight", "op": {"kind": "agent", "agent": "tight"}}
        ],
        "agents": [
            {"name": "desk", "model": "test", "tools": ["ping"], "max_steps": 4},
            {"name": "tight", "model": "test", "tools": ["ping"], "max_steps": 4, "token_budget": 1}
        ],
        "server": {"start_halted": true}
    }))
    .expect("manifest");
    let audit = Arc::new(MemoryAudit::default());
    let rt = AgentRuntime::new(Arc::new(FakeProvider), audit);
    App::build_without_python(manifest).await.expect("app").with_agent_runtime(rt).into_arc()
}

fn error_of(v: &Value) -> (&str, &str) {
    (v["error"]["code"].as_str().unwrap_or(""), v["error"]["hint"].as_str().unwrap_or(""))
}

#[tokio::test]
async fn http_errors_carry_a_catalogued_code_and_hint_beside_the_old_fields() {
    let app = app().await;
    let cases = [
        (WebCortexRequest::synthetic("GET", "/nope"), 404, "WC201"),
        (WebCortexRequest::synthetic("DELETE", "/ping"), 405, "WC202"),
        (WebCortexRequest::synthetic("POST", "/arm/move"), 423, "WC502"),
        (WebCortexRequest::synthetic("GET", "/secret"), 401, "WC601"),
        (WebCortexRequest::synthetic_with_scopes("GET", "/secret", &["reader"]), 403, "WC602"),
        (WebCortexRequest::synthetic("POST", "/agents/desk"), 400, "WC306"),
    ];
    for (req, status, code) in cases {
        let res = app.dispatch(req).await;
        assert_eq!(res.status, status);
        let body = res.json_value();
        // The fields 2.4 had are still there.
        assert_eq!(body["error"]["status"], status);
        assert!(body["error"]["message"].is_string());
        let (got, hint) = error_of(&body);
        assert_eq!(got, code, "{body}");
        assert!(!hint.is_empty());
        assert_eq!(res.error_code().as_deref(), Some(code));
        assert!(webcortex_core::errors::lookup(code).is_some());
    }
}

#[tokio::test]
async fn failed_and_limited_runs_carry_codes_and_reach_the_status_view() {
    let app = app().await;
    let who = Principal::anonymous();

    let failed = app.invoke_agent("desk", "fail:anthropic 529: overloaded", &who, RunOptions::default()).await.unwrap();
    assert_eq!(serde_json::to_value(failed.status).unwrap(), "failed");
    assert_eq!(failed.code.as_deref(), Some("WC304"));
    assert!(failed.hint.as_deref().is_some_and(|h| !h.is_empty()));
    assert_eq!(failed.steps.last().unwrap().code.as_deref(), Some("WC304"));

    let tight = app.invoke_agent("tight", "tool:ping {}", &who, RunOptions::default()).await.unwrap();
    assert_eq!(serde_json::to_value(tight.status).unwrap(), "budget_exhausted");
    assert_eq!(tight.code.as_deref(), Some("WC402"));

    let ok = app.invoke_agent("desk", "hello", &who, RunOptions::default()).await.unwrap();
    assert!(ok.code.is_none() && ok.hint.is_none());
    let wire = serde_json::to_value(&ok).unwrap();
    assert!(wire.get("code").is_none(), "a completed run serialises as before");

    let status = app.status();
    let recent = status["agents"]["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 3);
    assert_eq!(recent[0]["run_id"], ok.run_id);
    assert_eq!(recent[1]["outcome"], "budget_exhausted");
    assert_eq!(recent[2]["code"], "WC304");
    assert!(status["agents"]["in_flight"].as_array().unwrap().is_empty());
    let errors = status["errors"]["recent"].as_array().unwrap();
    assert!(errors.iter().any(|e| e["code"] == "WC304" && e["request_id"] == failed.run_id));
    let tight_budget = status["budgets"].as_array().unwrap().iter().find(|b| b["name"] == "tight").unwrap();
    assert_eq!(tight_budget["budget_exhausted"], 1);
    assert_eq!(status["estop"]["halted"], true);
    assert_eq!(status["webcortex"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn a_tool_that_fails_inside_a_run_names_its_code_on_the_step() {
    let app = app().await;
    let r = app
        .invoke_agent("desk", "tool:move_arm {}", &Principal::anonymous(), RunOptions::default())
        .await
        .unwrap();
    // move_arm is not one of desk's tools: refused, not dispatched.
    let step = r.steps.iter().find(|s| s.tool.as_deref() == Some("move_arm")).unwrap();
    assert_eq!(step.code.as_deref(), Some("WC307"));
    assert_eq!(step.hint.as_deref(), Some(webcortex_core::errors::hint("WC307")));
    // ...and the refusal is in the live view, under the run's id.
    let recent = app.status()["errors"]["recent"].clone();
    assert!(recent.as_array().unwrap().iter().any(|e| e["source"] == "tool" && e["request_id"] == r.run_id));
}

#[tokio::test]
async fn mcp_offers_status_and_explain_and_codes_its_tool_errors() {
    let app = app().await;
    let who = Principal::anonymous();
    let call = |name: &str, args: Value| {
        serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                   "params": {"name": name, "arguments": args}})).unwrap()
    };

    let res = mcp::handle(&app, &call("move_arm", json!({})), &who).await.json_value();
    assert_eq!(res["result"]["isError"], true);
    assert_eq!(res["result"]["structuredContent"]["error"]["code"], "WC502");

    let res = mcp::handle(&app, &call("ghost", json!({})), &who).await.json_value();
    assert_eq!(res["result"]["structuredContent"]["error"]["code"], "WC314");

    let res = mcp::handle(&app, &call(mcp::STATUS_TOOL, json!({"section": "errors"})), &who).await.json_value();
    let recent = res["result"]["structuredContent"]["errors"]["recent"].as_array().unwrap().clone();
    assert!(recent.iter().any(|e| e["code"] == "WC502" && e["source"] == "mcp"));

    let res = mcp::handle(&app, &call(mcp::EXPLAIN_TOOL, json!({"code": "wc402"})), &who).await.json_value();
    assert_eq!(res["result"]["structuredContent"]["title"], "Token budget exhausted");

    let list = br#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#;
    let res = mcp::handle(&app, list, &who).await.json_value();
    assert_eq!(res["result"]["resources"].as_array().unwrap().len(), 2);
    let read = br#"{"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"webcortex://status"}}"#;
    let res = mcp::handle(&app, read, &who).await.json_value();
    let text = res["result"]["contents"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap()["app"], "diag");
}

#[tokio::test]
async fn mcp_hides_the_status_view_from_a_caller_without_admin_once_auth_exists() {
    unsafe { std::env::set_var("WC_DIAG_TEST_KEY", "k-123456789") };
    let manifest: Manifest = serde_json::from_value(json!({
        "name": "locked",
        "auth": {"api_keys": {"WC_DIAG_TEST_KEY": {"id": "svc", "scopes": ["read"]}}},
        "routes": []
    }))
    .expect("manifest");
    let app = App::build_without_python(manifest).await.expect("app").into_arc();
    let reader = Principal { id: "svc".into(), kind: webcortex_core::PrincipalKind::ApiKey,
                             scopes: vec!["read".into()], claims: Default::default() };
    let res = mcp::handle(&app, br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, &reader).await.json_value();
    assert!(res["result"]["tools"].as_array().unwrap().is_empty());
    let read = br#"{"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"webcortex://status"}}"#;
    let res = mcp::handle(&app, read, &reader).await.json_value();
    assert!(res["error"].is_object());
}

#[tokio::test]
async fn a_failed_run_over_http_sends_its_own_code_in_the_header() {
    // 2.5.0 sent no x-webcortex-error on a failed run's 502, so the request log
    // and the status view recorded the generic WC300 beside the run's WC304.
    let app = app().await;
    let mut req = WebCortexRequest::synthetic("POST", "/agents/desk");
    req.body = serde_json::to_vec(&json!({"input": "fail:anthropic 529: overloaded"})).unwrap().into();
    let res = app.dispatch(req).await;
    assert_eq!(res.status, 502);
    assert_eq!(res.json_value()["code"], "WC304");
    assert_eq!(res.error_code().as_deref(), Some("WC304"));

    // A limit is an outcome, not an error: no header on its 200.
    let mut req = WebCortexRequest::synthetic("POST", "/agents/tight");
    req.body = serde_json::to_vec(&json!({"input": "tool:ping {}"})).unwrap().into();
    let res = app.dispatch(req).await;
    assert_eq!(res.status, 200);
    assert!(!res.headers.iter().any(|(k, _)| k == "x-webcortex-error"));
}

#[tokio::test]
async fn a_tool_call_missing_a_path_argument_is_wc206_not_no_route() {
    // 2.5.0 built "/items/" from {} and failed with WC201 "no route", which
    // sends a model hunting for a route instead of the argument it omitted.
    let manifest: Manifest = serde_json::from_value(json!({
        "name": "args",
        "routes": [
            {"id": 0, "method": "GET", "path": "/items/{id}", "op": {"kind": "static", "body": {"ok": true}},
             "tool": {"expose": true, "name": "get_item"}}
        ]
    }))
    .expect("manifest");
    let app = App::build_without_python(manifest).await.expect("app").into_arc();
    for args in [json!({}), json!({"id": null}), json!({"id": ""})] {
        let err = app.call_tool("get_item", &args, vec![]).await.unwrap_err();
        assert!(err.contains("failed with 422"), "{err}");
        assert_eq!(webcortex_core::errors::code_in(&err), Some("WC206"), "{err}");
        assert!(err.contains("\\\"id\\\""), "names the argument: {err}");
    }
    assert_eq!(app.call_tool("get_item", &json!({"id": 7}), vec![]).await.unwrap(), json!({"ok": true}));

    let who = Principal::anonymous();
    let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_item","arguments":{}}}"#;
    let res = mcp::handle(&app, body, &who).await.json_value();
    assert_eq!(res["result"]["structuredContent"]["error"]["code"], "WC206");
}

#[tokio::test]
async fn mcp_builtins_refuse_a_bad_section_or_a_missing_code() {
    let app = app().await;
    let who = Principal::anonymous();
    let call = |name: &str, args: Value| {
        serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                   "params": {"name": name, "arguments": args}})).unwrap()
    };
    let res = mcp::handle(&app, &call(mcp::STATUS_TOOL, json!({"section": "bogus"})), &who).await.json_value();
    assert_eq!(res["result"]["isError"], true);
    assert_eq!(res["result"]["structuredContent"]["error"]["code"], "WC206");
    assert!(res["result"]["structuredContent"]["error"]["hint"].as_str().unwrap().contains("requests"));

    let res = mcp::handle(&app, &call(mcp::EXPLAIN_TOOL, json!({})), &who).await.json_value();
    assert_eq!(res["result"]["structuredContent"]["error"]["code"], "WC206");

    let res = mcp::handle(&app, br#"{"jsonrpc":"2.0","id":9,"method":"nosuch"}"#, &who).await.json_value();
    assert_eq!(res["error"]["code"], -32601);
    assert!(res["error"]["data"]["hint"].as_str().unwrap().contains("tools/call"));
}
