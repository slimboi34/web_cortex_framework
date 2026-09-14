//! The hyper serve loop, the middleware chain, and the control plane.
//!
//! Order matters and is fixed deliberately:
//!
//! 1. CORS preflight — answered before anything else can reject it
//! 2. Request id — so every subsequent log line correlates
//! 3. Authenticate — establish the principal once
//! 4. Rate limit — keyed by that principal, or by client IP when anonymous
//! 5. Dispatch — scope checks happen inside, next to the op
//! 6. Response headers — security headers and CORS applied to every exit path
//!
//! Steps 1–4 run for the control plane too. An unauthenticated MCP endpoint
//! would hand an attacker every tool in the application.

use crate::app::App;
use crate::auth::Principal;
use crate::http::{WebCortexRequest, WebCortexResponse, parse_query};
use crate::{mcp, middleware, openapi};
use bytes::Bytes;
use futures::FutureExt;
use http_body_util::{BodyExt, Full};
use std::panic::AssertUnwindSafe;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// How much of an oversized body is read and thrown away before the 413, and
/// for how long. Closing on a client that is still sending resets the
/// connection, and the reset destroys the 413 before the client reads it;
/// draining a bounded overshoot lets the answer arrive. Past either bound the
/// connection is cut off, as before.
const DRAIN_BYTES: usize = 8 * 1024 * 1024;
const DRAIN_TIME: std::time::Duration = std::time::Duration::from_secs(2);

pub async fn serve(app: Arc<App>) -> Result<(), String> {
    serve_with_shutdown(app, shutdown_signal()).await
}

/// Serve until `shutdown` resolves, then stop accepting and drain in-flight
/// connections. A deploy should never sever a request that was mid-flight.
pub async fn serve_with_shutdown<F>(app: Arc<App>, shutdown: F) -> Result<(), String>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let cfg = app.manifest.server.clone();
    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port)
        .parse()
        .map_err(|e| format!("bad listen address: {e}"))?;

    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr} failed: {e}"))?;

    let native = app
        .manifest
        .routes
        .iter()
        .filter(|r| !matches!(r.op, crate::manifest::Op::Python { .. }))
        .count();
    tracing::info!(
        %addr,
        routes = app.manifest.routes.len(),
        native_routes = native,
        tools = app.exposed_tools().len(),
        agents = app.manifest.agents.len(),
        python_workers = app.bridge().workers(),
        rate_limited = app.rate_limiter.is_some(),
        "webcortex listening"
    );

    let tracker = Arc::new(tokio::sync::Semaphore::new(Semaphore::MAX_PERMITS));
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received; draining in-flight requests");
                break;
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        continue;
                    }
                };
                let app = app.clone();
                let permit = tracker.clone().acquire_owned().await.ok();
                tokio::spawn(async move {
                    let _permit = permit;
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req| {
                        let app = app.clone();
                        let client_ip = peer.ip().to_string();
                        async move { Ok::<_, Infallible>(handle(app, req, client_ip).await) }
                    });
                    if let Err(e) = auto::Builder::new(TokioExecutor::new())
                        .serve_connection(io, svc)
                        .await
                    {
                        tracing::debug!(%peer, error = %e, "connection closed");
                    }
                });
            }
        }
    }

    // Draining: wait for every outstanding connection permit to come back.
    let deadline = std::time::Duration::from_secs(cfg.shutdown_timeout_secs);
    match tokio::time::timeout(
        deadline,
        tracker.acquire_many(Semaphore::MAX_PERMITS as u32),
    )
    .await
    {
        Ok(_) => tracing::info!("all connections drained; exiting"),
        Err(_) => tracing::warn!(
            timeout_secs = cfg.shutdown_timeout_secs,
            "drain timed out; exiting with connections still open"
        ),
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

async fn handle(app: Arc<App>, req: Request<Incoming>, client_ip: String) -> Response<Full<Bytes>> {
    let started = std::time::Instant::now();
    let method = req.method().as_str().to_string();
    let uri = req.uri().clone();
    let path = uri.path().to_string();

    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Defence in depth. A panic anywhere in the request path — ours or a
    // dependency's — must become a 500 for that one request, not a severed
    // connection with no status line. A dependency panicking inside the auth
    // path is exactly how this was discovered, and the symptom (connection
    // closed, no response) is far harder to diagnose than a 500 would have been.
    let outcome = AssertUnwindSafe(route_request(&app, req, &client_ip, &request_id))
        .catch_unwind()
        .await;

    let (mut res, principal_id) = match outcome {
        Ok(v) => v,
        Err(payload) => {
            let detail = panic_message(&payload);
            tracing::error!(
                request_id = %request_id,
                method = %method,
                path = %path,
                panic = %detail,
                "request handler panicked; returning 500"
            );
            (
                WebCortexResponse::error(500, "internal error"),
                "panic".to_string(),
            )
        }
    };

    // Applied on every exit path, including errors and rejections.
    res.headers.push(("x-request-id".into(), request_id.clone()));
    for h in middleware::security_headers(&app.manifest.security_headers, false) {
        res.headers.push(h);
    }

    tracing::info!(
        request_id = %request_id,
        method = %method,
        path = %path,
        status = res.status,
        principal = %principal_id,
        elapsed_us = started.elapsed().as_micros() as u64,
        "request"
    );
    to_hyper(res)
}

async fn route_request(
    app: &Arc<App>,
    req: Request<Incoming>,
    client_ip: &str,
    request_id: &str,
) -> (WebCortexResponse, String) {
    let method = req.method().as_str().to_string();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let query = parse_query(uri.query().unwrap_or(""));

    let mut headers = std::collections::BTreeMap::new();
    for (k, v) in req.headers() {
        if let Ok(s) = v.to_str() {
            headers.insert(k.as_str().to_ascii_lowercase(), s.to_string());
        }
    }
    let origin = headers.get("origin").cloned();

    // 1. CORS preflight, before any rejection could pre-empt it.
    if method == "OPTIONS" && app.manifest.cors.enabled {
        if let Some(cors_headers) = app.cors.preflight(origin.as_deref()) {
            return (
                WebCortexResponse { status: 204, headers: cors_headers, body: Bytes::new() },
                "-".into(),
            );
        }
        return (WebCortexResponse::error(403, "origin not allowed"), "-".into());
    }

    // 2. Authenticate. Done before rate limiting is *keyed*, but the limiter
    //    still falls back to IP when the caller is anonymous.
    let principal = match app.authenticator.authenticate(&headers) {
        Ok(p) => p,
        Err(e) => {
            let mut res = WebCortexResponse::error(e.status(), e.message());
            res.headers.push((
                "www-authenticate".into(),
                "Bearer realm=\"webcortex\"".into(),
            ));
            finish_cors(app, &mut res, origin.as_deref());
            return (res, "invalid".into());
        }
    };
    let principal_id = principal.id.clone();

    // 3. Rate limit, keyed by principal so one tenant cannot exhaust another,
    //    falling back to client IP for anonymous traffic.
    if let Some(limiter) = &app.rate_limiter {
        let key = if principal.is_anonymous() {
            format!("ip:{client_ip}")
        } else {
            format!("id:{}", principal.id)
        };
        let decision = limiter.check(&key);
        if !decision.allowed {
            let mut res = WebCortexResponse::error(429, "rate limit exceeded");
            res.headers.push(("retry-after".into(), decision.retry_after_secs.to_string()));
            res.headers.push(("x-ratelimit-limit".into(), decision.limit.to_string()));
            res.headers.push(("x-ratelimit-remaining".into(), "0".into()));
            finish_cors(app, &mut res, origin.as_deref());
            return (res, principal_id);
        }
    }

    let body = match read_body(req).await {
        Ok(b) => b,
        Err(mut res) => {
            finish_cors(app, &mut res, origin.as_deref());
            return (res, principal_id);
        }
    };

    let prefix = app.manifest.server.control_prefix.clone();
    let mut res = if let Some(rest) = path.strip_prefix(&prefix) {
        control_plane(app, &method, rest, &body, &principal, request_id).await
    } else {
        // A model loop routinely outlasts an ordinary handler's ceiling, and a
        // timeout drops the run mid-flight.
        let limits = &app.manifest.server;
        let secs = if app.runs_a_model_loop(&method, &path) {
            limits.agent_timeout_secs.max(limits.request_timeout_secs)
        } else {
            limits.request_timeout_secs
        };
        let timeout = std::time::Duration::from_secs(secs);
        let dispatch = app.dispatch(WebCortexRequest {
            method,
            path,
            path_params: Default::default(),
            query,
            headers,
            body,
            route_id: None,
            principal,
            depth: 0,
            budget: None,
        });
        // A handler that hangs must not hold a connection forever.
        match tokio::time::timeout(timeout, dispatch).await {
            Ok(r) => r,
            Err(_) => WebCortexResponse::error(504, "handler exceeded the request timeout"),
        }
    };

    finish_cors(app, &mut res, origin.as_deref());
    (res, principal_id)
}

fn finish_cors(app: &Arc<App>, res: &mut WebCortexResponse, origin: Option<&str>) {
    if app.manifest.cors.enabled {
        for h in app.cors.headers_for(origin) {
            res.headers.push(h);
        }
    }
}

async fn read_body(req: Request<Incoming>) -> Result<Bytes, WebCortexResponse> {
    let too_large = || WebCortexResponse::error(413, format!("request body exceeds {MAX_BODY_BYTES} bytes"));
    let mut body = req.into_body();
    let mut buf = bytes::BytesMut::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { return Err(too_large()) };
        let Ok(data) = frame.into_data() else { continue };
        if buf.len() + data.len() > MAX_BODY_BYTES {
            let _ = tokio::time::timeout(DRAIN_TIME, async {
                let mut drained = 0usize;
                while drained <= DRAIN_BYTES {
                    match body.frame().await {
                        Some(Ok(f)) => drained += f.data_ref().map_or(0, |d| d.len()),
                        _ => break,
                    }
                }
            })
            .await;
            return Err(too_large());
        }
        buf.extend_from_slice(&data);
    }
    Ok(buf.freeze())
}

/// Introspection and operations endpoints, mounted under `control_prefix`.
///
/// Everything except `/health` requires the `webcortex:admin` scope when any
/// authentication is configured. An open MCP endpoint is an open door to every
/// tool in the app.
async fn control_plane(
    app: &Arc<App>,
    method: &str,
    rest: &str,
    body: &[u8],
    principal: &Principal,
    request_id: &str,
) -> WebCortexResponse {
    let auth_configured = !app.manifest.auth.api_keys.is_empty() || app.manifest.auth.jwt.is_some();
    let admin_required = auth_configured && rest != "/health";
    if admin_required && !principal.has_scope("webcortex:admin") {
        let status = if principal.is_anonymous() { 401 } else { 403 };
        return WebCortexResponse::error(
            status,
            "the webcortex control plane requires the 'webcortex:admin' scope",
        );
    }

    match (method, rest) {
        ("GET", "/health") => WebCortexResponse::json(
            200,
            &serde_json::json!({
                "status": "ok",
                "app": app.manifest.name,
                "version": app.manifest.version,
                "routes": app.manifest.routes.len(),
                "tools": app.exposed_tools().len(),
                "agents": app.manifest.agents.len(),
                "behaviours": app.manifest.behaviours.len(),
                "flows": app.manifest.flows.len(),
                "python_workers": app.bridge().workers(),
                "sessions": app.sessions().len(),
                "pending_approvals": app.pending_approvals().len(),
            }),
        ),

        // Spend, by model and by what spent it. Cost is reported only when
        // every model involved has a declared price.
        ("GET", "/usage") => WebCortexResponse::json(
            200,
            &app.ledger().snapshot(|m| app.registry().price_for(m)),
        ),

        ("GET", "/models") => WebCortexResponse::json(200, &app.registry().describe()),

        ("GET", "/approvals") => WebCortexResponse::json(
            200,
            &serde_json::json!({"approvals": app.pending_approvals()}),
        ),

        // Decide a gated call and continue the run that asked for it.
        ("POST", path) if path.starts_with("/approvals/") => {
            let id = path.trim_start_matches("/approvals/").trim_end_matches('/');
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
            let approve = parsed.get("approve").and_then(|v| v.as_bool());
            let Some(approve) = approve else {
                return WebCortexResponse::error(400, "body must be {\"approve\": true|false, \"note\": \"…\"}");
            };
            let note = parsed.get("note").and_then(|v| v.as_str()).unwrap_or("");
            match app.resolve_approval(id, approve, note, principal).await {
                Ok(result) => crate::app::run_result_response(&result),
                Err(e) => WebCortexResponse::error(404, e),
            }
        }

        ("GET", "/flows") => WebCortexResponse::json(
            200,
            &serde_json::json!({
                "flows": app.manifest.flows.iter().map(|f| serde_json::json!({
                    "name": f.name,
                    "description": f.description,
                    "kind": f.kind,
                    "scopes": f.scopes,
                    "token_budget": f.token_budget,
                    "input_schema": f.input_schema,
                })).collect::<Vec<_>>()
            }),
        ),

        ("GET", "/contexts") => WebCortexResponse::json(
            200,
            &serde_json::json!({
                "contexts": app.manifest.contexts.iter().map(|c| serde_json::json!({
                    "name": c.name,
                    "description": c.description,
                    "kind": match c.source {
                        crate::manifest::ContextSource::Static { .. } => "static",
                        crate::manifest::ContextSource::Query { .. } => "query",
                        crate::manifest::ContextSource::Python { .. } => "python",
                    },
                    "max_chars": c.max_chars,
                })).collect::<Vec<_>>()
            }),
        ),

        ("GET", "/openapi.json") => WebCortexResponse::json(200, &openapi::generate(&app.manifest)),

        ("GET", "/tools") => WebCortexResponse::json(
            200,
            &serde_json::json!({
                "tools": app.exposed_tools().into_iter().map(|r| serde_json::json!({
                    "name": r.tool_name(),
                    "method": r.method,
                    "path": r.path,
                    "description": r.description,
                    "input_schema": r.input_schema,
                    "read_only": r.tool.read_only,
                    "scopes": r.scopes,
                    "approval": r.approval,
                })).collect::<Vec<_>>()
            }),
        ),

        ("POST", "/mcp") => mcp::handle(app, body, principal).await,

        ("GET", "/mcp") => WebCortexResponse::json(
            200,
            &serde_json::json!({
                "transport": "streamable-http",
                "protocolVersion": mcp::PROTOCOL_VERSION,
                "hint": "POST JSON-RPC to this endpoint",
            }),
        ),

        ("GET", "/routes") => WebCortexResponse::json(
            200,
            &serde_json::json!({
                "routes": app.manifest.routes.iter().map(|r| serde_json::json!({
                    "id": r.id,
                    "method": r.method,
                    "path": r.path,
                    "op": r.op.kind(),
                    "tool": r.tool.expose.then(|| r.tool_name()),
                    "scopes": r.scopes,
                })).collect::<Vec<_>>()
            }),
        ),

        // Agent activity, without needing a log pipeline first.
        ("GET", "/audit") => WebCortexResponse::json(
            200,
            &serde_json::json!({"events": app.audit.recent(200)}),
        ),

        ("GET", "/behaviours") => WebCortexResponse::json(
            200,
            &serde_json::json!({
                "behaviours": app.manifest.behaviours.iter().map(|b| serde_json::json!({
                    "name": b.name,
                    "description": b.description,
                    "tools": b.tools,
                    "scopes": b.scopes,
                    "max_steps": b.max_steps,
                    "token_budget": b.token_budget,
                    "input_schema": b.input_schema,
                })).collect::<Vec<_>>()
            }),
        ),

        ("GET", "/agents") => WebCortexResponse::json(
            200,
            &serde_json::json!({
                "agents": app.manifest.agents.iter().map(|a| serde_json::json!({
                    "name": a.name,
                    "model": a.model,
                    "description": a.description,
                    "tools": a.tools,
                    "handoffs": a.handoffs,
                    "context": a.context,
                    "max_steps": a.max_steps,
                    "token_budget": a.token_budget,
                    "scopes": a.scopes,
                    "cache": a.cache,
                    "policy": a.policy,
                })).collect::<Vec<_>>()
            }),
        ),

        // Security posture on one screen: what is reachable with no credential.
        ("GET", "/security") => {
            let public = app.manifest.public_routes();
            WebCortexResponse::json(
                200,
                &serde_json::json!({
                    "auth_configured": auth_configured,
                    "cors_enabled": app.manifest.cors.enabled,
                    "cors_origins": app.manifest.cors.allow_origins,
                    "rate_limited": app.rate_limiter.is_some(),
                    "security_headers": app.manifest.security_headers.enabled,
                    "anonymous_scopes": app.manifest.auth.anonymous_scopes,
                    "public_routes": public.iter().map(|r| format!("{} {}", r.method, r.path))
                        .collect::<Vec<_>>(),
                    "gated_tools": app.manifest.routes.iter()
                        .filter(|r| r.approval == crate::manifest::Approval::Required)
                        .map(|r| r.tool_name()).collect::<Vec<_>>(),
                }),
            )
        }

        _ => WebCortexResponse::error(
            404,
            format!("no control endpoint {method} {rest} (request {request_id})"),
        ),
    }
}

/// Best-effort extraction of a panic message for the log. Never surfaced to the
/// client, which only ever sees "internal error".
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "non-string panic payload".to_string()
}

fn to_hyper(res: WebCortexResponse) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(res.status);
    for (k, v) in &res.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    builder.body(Full::new(res.body)).unwrap_or_else(|e| {
        Response::builder()
            .status(500)
            .body(Full::new(Bytes::from(format!("response build failed: {e}"))))
            .expect("500 response is always constructible")
    })
}
