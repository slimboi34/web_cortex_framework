//! WebCortex core runtime.
//!
//! A WebCortex application is declared in Python, compiled to a [`manifest::Manifest`],
//! and executed here. Routes whose work is expressible as data — queries,
//! proxies, static responses, agent invocations — never re-enter the
//! interpreter. Only [`manifest::Op::Python`] routes cross the bridge.

pub mod agent;
pub mod app;
pub mod audit;
pub mod auth;
pub mod bridge;
pub mod context;
pub mod files;
pub mod flow;
pub mod http;
pub mod ledger;
pub mod manifest;
pub mod mcp;
pub mod middleware;
pub mod openapi;
pub mod router;
pub mod server;
pub mod templates;
pub mod typegen;

#[cfg(feature = "sqlite")]
pub mod db;

pub use agent::{AgentRuntime, ProviderRegistry, RunOptions, RunResult, RunStatus, SharedBudget, SuspendedRun};
pub use app::App;
pub use audit::{AuditSink, MemoryAudit, TracingAudit};
pub use auth::{Principal, PrincipalKind};
pub use bridge::{NoBridge, PyBridge};
pub use http::{WebCortexRequest, WebCortexResponse};
pub use ledger::Ledger;
pub use manifest::Manifest;


/// Install the default tracing subscriber unless the host already did.
pub fn init_tracing(default_level: &str) {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_env("WEBCORTEX_LOG")
        .unwrap_or_else(|_| EnvFilter::new(default_level));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifest::derive_tool_name;

    #[test]
    fn tool_names_read_like_function_names() {
        assert_eq!(derive_tool_name("GET", "/users/{id}"), "get_users_by_id");
        assert_eq!(derive_tool_name("POST", "/users"), "create_users");
        assert_eq!(derive_tool_name("DELETE", "/a/b/{c}"), "delete_a_b_by_c");
    }

    fn manifest_json(routes: serde_json::Value) -> Manifest {
        serde_json::from_value(serde_json::json!({
            "name": "t", "routes": routes
        }))
        .expect("test manifest parses")
    }

    #[tokio::test]
    async fn static_route_serves_without_an_interpreter() {
        let m = manifest_json(serde_json::json!([{
            "id": 0, "method": "GET", "path": "/ping",
            "op": {"kind": "static", "body": {"pong": true}}
        }]));
        let app = App::build_without_python(m).await.expect("app builds");
        let res = app.dispatch(WebCortexRequest::synthetic("GET", "/ping")).await;
        assert_eq!(res.status, 200);
        assert_eq!(res.json_value(), serde_json::json!({"pong": true}));
    }

    #[tokio::test]
    async fn unknown_path_404s_and_wrong_method_405s() {
        let m = manifest_json(serde_json::json!([{
            "id": 0, "method": "GET", "path": "/ping",
            "op": {"kind": "static", "body": {}}
        }]));
        let app = App::build_without_python(m).await.expect("app builds");
        assert_eq!(app.dispatch(WebCortexRequest::synthetic("GET", "/nope")).await.status, 404);
        assert_eq!(app.dispatch(WebCortexRequest::synthetic("POST", "/ping")).await.status, 405);
    }

    #[tokio::test]
    async fn undeclared_upstream_fails_at_boot_not_at_request_time() {
        let m = manifest_json(serde_json::json!([{
            "id": 0, "method": "GET", "path": "/x",
            "op": {"kind": "proxy", "upstream": "ghost"}
        }]));
        let err = match App::build_without_python(m).await {
            Err(e) => e,
            Ok(_) => panic!("an undeclared upstream should have failed validation"),
        };
        assert!(err.contains("ghost"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn exposed_route_is_callable_as_a_tool_in_process() {
        let m = manifest_json(serde_json::json!([{
            "id": 0, "method": "GET", "path": "/ping",
            "op": {"kind": "static", "body": {"pong": true}},
            "tool": {"expose": true, "name": "ping", "read_only": true}
        }]));
        let app = App::build_without_python(m).await.expect("app builds");
        let out = app
            .call_tool("ping", &serde_json::json!({}), vec![])
            .await
            .expect("tool call succeeds");
        assert_eq!(out, serde_json::json!({"pong": true}));
    }

    #[tokio::test]
    async fn scoped_routes_reject_callers_without_the_scope() {
        let m = manifest_json(serde_json::json!([{
            "id": 0, "method": "GET", "path": "/secret",
            "op": {"kind": "static", "body": {"ok": true}},
            "tool": {"expose": true, "name": "secret", "scopes": ["admin"]}
        }]));
        let app = App::build_without_python(m).await.expect("app builds");

        // Anonymous gets 401: the client can fix this by authenticating.
        assert_eq!(app.dispatch(WebCortexRequest::synthetic("GET", "/secret")).await.status, 401);

        // A known principal holding the wrong scope gets 403: authenticating
        // again will not help, and conflating the two makes auth bugs opaque.
        let wrong_scope = WebCortexRequest::synthetic_with_scopes("GET", "/secret", &["reader"]);
        assert_eq!(app.dispatch(wrong_scope).await.status, 403);

        assert!(
            app.call_tool("secret", &serde_json::json!({}), vec!["admin".into()])
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn mcp_lists_only_exposed_routes() {
        let m = manifest_json(serde_json::json!([
            {"id": 0, "method": "GET", "path": "/a", "op": {"kind": "static", "body": {}},
             "tool": {"expose": true, "name": "a"}},
            {"id": 1, "method": "GET", "path": "/b", "op": {"kind": "static", "body": {}}}
        ]));
        let app = App::build_without_python(m).await.expect("app builds");
        let req = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let res = mcp::handle(&app, req, &Principal::anonymous()).await;
        let tools = res.json_value()["result"]["tools"].clone();
        assert_eq!(tools.as_array().expect("tools array").len(), 1);
        assert_eq!(tools[0]["name"], "a");
    }

    #[tokio::test]
    async fn mcp_notifications_get_no_response_body() {
        let m = manifest_json(serde_json::json!([]));
        let app = App::build_without_python(m).await.expect("app builds");
        let res = mcp::handle(
            &app,
            br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &Principal::anonymous(),
        )
        .await;
        assert_eq!(res.status, 202);
        assert!(res.body.is_empty());
    }
}
