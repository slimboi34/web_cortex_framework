//! The hyper serve loop and the control-plane routes.

use crate::app::App;
use crate::http::{PylonRequest, PylonResponse, parse_query};
use crate::{mcp, openapi};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

/// Bodies above this are rejected rather than buffered, so a single client
/// cannot exhaust memory before a handler ever sees the request.
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

pub async fn serve(app: Arc<App>) -> Result<(), String> {
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
        python_workers = app.bridge().workers(),
        "pylon listening"
    );

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        let app = app.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| {
                let app = app.clone();
                async move { Ok::<_, Infallible>(handle_connection(app, req).await) }
            });
            // `auto` negotiates HTTP/1.1 and HTTP/2 on the same port.
            if let Err(e) = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                tracing::debug!(%peer, error = %e, "connection closed");
            }
        });
    }
}

async fn handle_connection(app: Arc<App>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let started = std::time::Instant::now();
    let method = req.method().as_str().to_string();
    let uri = req.uri().clone();
    let path = uri.path().to_string();

    let res = route_request(app, req).await;

    tracing::info!(
        method = %method,
        path = %path,
        status = res.status,
        elapsed_us = started.elapsed().as_micros() as u64,
        "request"
    );
    to_hyper(res)
}

async fn route_request(app: Arc<App>, req: Request<Incoming>) -> PylonResponse {
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

    let body = match read_body(req).await {
        Ok(b) => b,
        Err(res) => return res,
    };

    let prefix = app.manifest.server.control_prefix.clone();
    if let Some(rest) = path.strip_prefix(&prefix) {
        return control_plane(&app, &method, rest, &body).await;
    }

    app.dispatch(PylonRequest {
        method,
        path,
        path_params: Default::default(),
        query,
        headers,
        body,
        route_id: None,
        scopes: Vec::new(),
    })
    .await
}

async fn read_body(req: Request<Incoming>) -> Result<Bytes, PylonResponse> {
    use http_body_util::Limited;
    let body = Limited::new(req.into_body(), MAX_BODY_BYTES as usize);
    match body.collect().await {
        Ok(c) => Ok(c.to_bytes()),
        Err(_) => Err(PylonResponse::error(
            413,
            format!("request body exceeds {MAX_BODY_BYTES} bytes"),
        )),
    }
}

/// Introspection endpoints, mounted under `control_prefix` (default `/_pylon`).
async fn control_plane(app: &Arc<App>, method: &str, rest: &str, body: &[u8]) -> PylonResponse {
    match (method, rest) {
        ("GET", "/health") => PylonResponse::json(
            200,
            &serde_json::json!({
                "status": "ok",
                "app": app.manifest.name,
                "version": app.manifest.version,
                "routes": app.manifest.routes.len(),
                "tools": app.exposed_tools().len(),
                "agents": app.manifest.agents.len(),
                "python_workers": app.bridge().workers(),
            }),
        ),

        ("GET", "/openapi.json") => PylonResponse::json(200, &openapi::generate(&app.manifest)),

        // A plain list of tools for clients that want discovery without
        // speaking JSON-RPC.
        ("GET", "/tools") => PylonResponse::json(
            200,
            &serde_json::json!({
                "tools": app.exposed_tools().into_iter().map(|r| serde_json::json!({
                    "name": r.tool_name(),
                    "method": r.method,
                    "path": r.path,
                    "description": r.description,
                    "input_schema": r.input_schema,
                    "read_only": r.tool.read_only,
                    "scopes": r.tool.scopes,
                })).collect::<Vec<_>>()
            }),
        ),

        ("POST", "/mcp") => mcp::handle(app, body).await,

        // Discovery handshake some MCP clients issue before POSTing.
        ("GET", "/mcp") => PylonResponse::json(
            200,
            &serde_json::json!({
                "transport": "streamable-http",
                "protocolVersion": "2025-06-18",
                "hint": "POST JSON-RPC to this endpoint",
            }),
        ),

        ("GET", "/routes") => PylonResponse::json(
            200,
            &serde_json::json!({
                "routes": app.manifest.routes.iter().map(|r| serde_json::json!({
                    "id": r.id,
                    "method": r.method,
                    "path": r.path,
                    "op": op_name(&r.op),
                    "tool": r.tool.expose.then(|| r.tool_name()),
                })).collect::<Vec<_>>()
            }),
        ),

        _ => PylonResponse::error(404, format!("no control endpoint {method} {rest}")),
    }
}

fn op_name(op: &crate::manifest::Op) -> &'static str {
    use crate::manifest::Op::*;
    match op {
        Static { .. } => "static",
        Python { .. } => "python",
        Query { .. } => "query",
        Proxy { .. } => "proxy",
        Agent { .. } => "agent",
    }
}

fn to_hyper(res: PylonResponse) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(res.status);
    for (k, v) in &res.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    builder
        .body(Full::new(res.body))
        .unwrap_or_else(|e| {
            Response::builder()
                .status(500)
                .body(Full::new(Bytes::from(format!("response build failed: {e}"))))
                .expect("500 response is always constructible")
        })
}
