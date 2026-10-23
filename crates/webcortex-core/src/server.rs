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
use crate::http::{WebCortexRequest, WebCortexResponse, parse_query_pairs};
use crate::{mcp, middleware, openapi};
use bytes::Bytes;
use futures::FutureExt;
use http_body_util::{BodyExt, Full};
use std::panic::AssertUnwindSafe;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// The whole answer to a connection over `max_connections`: written straight
/// to the socket, which is then closed.
static TOO_MANY_CONNECTIONS: std::sync::LazyLock<Vec<u8>> = std::sync::LazyLock::new(|| {
    let body = serde_json::json!({"error": {
        "status": 503, "code": "WC218", "message": "too many open connections; retry shortly",
        "hint": crate::errors::hint("WC218"),
    }})
    .to_string();
    format!(
        "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\nretry-after: 1\r\n\
         connection: close\r\nx-webcortex-error: WC218\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
});
/// How much of an oversized body is read and thrown away before the 413, and
/// for how long. Closing on a client that is still sending resets the
/// connection, and the reset destroys the 413 before the client reads it;
/// draining a bounded overshoot lets the answer arrive. Past either bound the
/// connection is cut off, as before.
const DRAIN_BYTES: usize = 8 * 1024 * 1024;
const DRAIN_TIME: std::time::Duration = std::time::Duration::from_secs(2);
/// The longest a connection may take to send a request's headers, and the
/// longest an idle keep-alive connection is held open waiting for the next
/// one. Without it a client that opens connections and sends nothing (or a
/// header a byte at a time) holds a socket each, indefinitely.
const MAX_HEADER_READ_SECS: u64 = 30;

/// The header read timeout for this app: its request timeout, at most
/// [`MAX_HEADER_READ_SECS`], at least one second.
fn header_read_timeout(request_timeout_secs: u64) -> std::time::Duration {
    std::time::Duration::from_secs(request_timeout_secs.clamp(1, MAX_HEADER_READ_SECS))
}

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

    // Watchers hand device frames to agents on a timer. They stop with the server.
    let watchers = crate::devices::spawn_watchers(app.clone());
    if !watchers.is_empty() || !app.devices().is_empty() {
        tracing::info!(devices = app.devices().len(), watchers = watchers.len(), "device hub running");
    }

    // Shutdown is broadcast to every connection task, and each task holds a
    // receiver for as long as it runs, so "all receivers dropped" is exactly
    // "every connection has finished". 2.5.3 counted connections with a
    // semaphore and waited for `MAX_PERMITS as u32` of them back — a
    // truncation that was satisfied immediately, so the drain never waited and
    // in-flight requests were severed when the process exited. Idle keep-alive
    // connections were never told to close, either.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
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
                // Past max_connections a connection is answered and closed
                // here, without a task, so a flood costs the server nothing
                // more than the accept. 2.6.0 had no ceiling at all.
                let Some(slot) = app.admit_connection() else {
                    tracing::debug!(%peer, max = cfg.max_connections, "connection refused: max_connections open");
                    let _ = stream.try_write(&TOO_MANY_CONNECTIONS);
                    continue;
                };
                let app = app.clone();
                let header_timeout = header_read_timeout(app.manifest.server.request_timeout_secs);
                let mut stop = stop_rx.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    // hyper's header timeout starts once it reads; the auto
                    // builder's protocol sniff before that waits forever, so
                    // a client that connects and sends nothing is dropped here.
                    tokio::select! {
                        ready = tokio::time::timeout(header_timeout, stream.readable()) => {
                            if ready.is_err() {
                                tracing::debug!(%peer, "connection sent nothing; closed");
                                return;
                            }
                        }
                        _ = stop.changed() => return,
                    }
                    let activity = Arc::new(Activity::new());
                    let io = TokioIo::new(Tracked { inner: stream, activity: activity.clone() });
                    let in_service = activity.clone();
                    let svc = service_fn(move |req| {
                        let app = app.clone();
                        let client_ip = peer.ip().to_string();
                        let busy = in_service.begin();
                        async move {
                            let res = handle(app, req, client_ip).await;
                            drop(busy);
                            Ok::<_, Infallible>(res)
                        }
                    });
                    let mut builder = auto::Builder::new(TokioExecutor::new());
                    // hyper applies a header read timeout only when it has a
                    // timer; without one, slow or silent clients were held forever.
                    builder.http1().timer(TokioTimer::new()).header_read_timeout(header_timeout);
                    builder.http2().timer(TokioTimer::new());
                    let conn = builder.serve_connection_with_upgrades(io, svc);
                    tokio::pin!(conn);
                    // hyper's header timeout covers HTTP/1 only. An HTTP/2
                    // connection — or one that sent part of the HTTP/2
                    // preface, which the protocol sniff waits on forever —
                    // was held open indefinitely, so idle sockets could be
                    // piled up past every timeout. A connection with nothing
                    // in flight and no bytes moving for the header timeout is
                    // now closed, whatever its protocol.
                    let tick = (header_timeout / 4).max(std::time::Duration::from_millis(250));
                    let result = loop {
                        tokio::select! {
                            r = conn.as_mut() => break r,
                            _ = stop.changed() => {
                                // Finish the request in flight, answer it with
                                // `connection: close`, and close an idle one now.
                                conn.as_mut().graceful_shutdown();
                                break conn.await;
                            }
                            _ = tokio::time::sleep(tick) => {
                                if activity.idle_for() >= header_timeout {
                                    tracing::debug!(%peer, "idle connection closed");
                                    conn.as_mut().graceful_shutdown();
                                    break tokio::time::timeout(header_timeout, conn.as_mut())
                                        .await
                                        .unwrap_or(Ok(()));
                                }
                            }
                        }
                    };
                    if let Err(e) = result {
                        tracing::debug!(%peer, error = %e, "connection closed");
                    }
                });
            }
        }
    }

    for w in &watchers {
        w.abort();
    }

    // Draining: tell every connection to finish, then wait for them all.
    drop(listener);
    drop(stop_rx);
    let _ = stop_tx.send(true);
    // WebSockets outlive the connection task that upgraded them, so they are
    // told separately: each sends a 1001 close and its task ends. Through
    // 2.6.0 they were simply cut when the process exited.
    let deadline = std::time::Duration::from_secs(cfg.shutdown_timeout_secs);
    let drained = async { tokio::join!(stop_tx.closed(), app.close_sockets()) };
    match tokio::time::timeout(deadline, drained).await {
        Ok(_) => tracing::info!("all connections drained; exiting"),
        Err(_) => tracing::warn!(
            timeout_secs = cfg.shutdown_timeout_secs,
            open = stop_tx.receiver_count(),
            websockets = app.open_sockets(),
            "drain timed out; exiting with connections still open"
        ),
    }
    Ok(())
}

/// What a connection is doing: when bytes last moved, and how many requests
/// are being handled on it. Idle means neither, for a while.
struct Activity {
    epoch: std::time::Instant,
    last_ms: std::sync::atomic::AtomicU64,
    in_flight: std::sync::atomic::AtomicUsize,
}

/// Held while a request is being handled.
struct Busy(Arc<Activity>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        self.0.touch();
    }
}

impl Activity {
    fn new() -> Self {
        Self {
            epoch: std::time::Instant::now(),
            last_ms: std::sync::atomic::AtomicU64::new(0),
            in_flight: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn touch(&self) {
        let now = self.epoch.elapsed().as_millis() as u64;
        self.last_ms.store(now, std::sync::atomic::Ordering::Relaxed);
    }

    fn begin(self: &Arc<Self>) -> Busy {
        self.in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.touch();
        Busy(self.clone())
    }

    /// How long the connection has had nothing in flight and moved no bytes.
    fn idle_for(&self) -> std::time::Duration {
        if self.in_flight.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            return std::time::Duration::ZERO;
        }
        let last = self.last_ms.load(std::sync::atomic::Ordering::Relaxed);
        let now = self.epoch.elapsed().as_millis() as u64;
        std::time::Duration::from_millis(now.saturating_sub(last))
    }
}

/// A socket that records, in its [`Activity`], every read or write that moves bytes.
struct Tracked<T> {
    inner: T,
    activity: Arc<Activity>,
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Tracked<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let res = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if buf.filled().len() > before {
            self.activity.touch();
        }
        res
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Tracked<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let res = std::pin::Pin::new(&mut self.inner).poll_write(cx, data);
        if matches!(res, std::task::Poll::Ready(Ok(n)) if n > 0) {
            self.activity.touch();
        }
        res
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let res = std::pin::Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if matches!(res, std::task::Poll::Ready(Ok(n)) if n > 0) {
            self.activity.touch();
        }
        res
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Whether the client reached us over TLS, by the proxy's account:
/// `X-Forwarded-Proto: https` or `Forwarded: proto=https` (the first hop).
fn forwarded_https(headers: &hyper::HeaderMap) -> bool {
    let first = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(|v| v.trim().to_ascii_lowercase())
    };
    if let Some(proto) = first("x-forwarded-proto") {
        return proto == "https";
    }
    first("forwarded").is_some_and(|hop| {
        hop.split(';').any(|kv| {
            kv.split_once('=')
                .is_some_and(|(k, v)| k.trim() == "proto" && v.trim().trim_matches('"') == "https")
        })
    })
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
    // The server speaks plain HTTP; TLS ends at a proxy, which says so. A
    // forged header only earns its sender an HSTS header their own browser
    // ignores over plain HTTP, so it needs no trust check.
    let https = forwarded_https(req.headers());
    let method = req.method().as_str().to_string();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    // An agent's WebSocket rides on its POST route, so its GET upgrade is
    // counted against that route rather than as "(no route)".
    let ws_upgrade = req
        .headers()
        .get(hyper::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

    // A caller's id is echoed and logged, so only a short, plain one is kept.
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| is_plain_request_id(v))
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
                WebCortexResponse::coded(500, "WC001", "internal error"),
                "panic".to_string(),
            )
        }
    };

    strip_framing_headers(&mut res);

    // A handler can answer with a status or a header hyper cannot send. That
    // used to become a bare text/plain 500 built after logging — no security
    // headers, no request id, and a log line recording the status that was
    // never sent. It is a coded 500 now, decided before anything is recorded.
    if let Err(problem) = sendable(&res) {
        tracing::error!(
            request_id = %request_id,
            method = %method,
            path = %path,
            problem = %problem,
            "handler produced a response that cannot be sent; returning 500"
        );
        res = WebCortexResponse::coded(500, "WC001", format!("the handler's response could not be sent: {problem}"));
    }

    // Applied on every exit path, including errors and rejections. The id is
    // always ours, so it matches the log; a security header the handler set
    // itself is its decision and is not duplicated (two conflicting
    // X-Frame-Options are worse than either).
    res.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("x-request-id"));
    res.headers.push(("x-request-id".into(), request_id.clone()));
    for (k, v) in middleware::security_headers(&app.manifest.security_headers, https) {
        push_absent(&mut res.headers, k, v);
    }
    dedupe_content_type(&mut res.headers);

    let elapsed_us = started.elapsed().as_micros() as u64;
    let target = if control_path(&path, &app.manifest.server.control_prefix).is_some() {
        crate::status::Target::Control
    } else {
        let id = app.route_id_for(&method, &path).or_else(|| {
            (ws_upgrade && method == "GET")
                .then(|| app.route_id_for("POST", &path))
                .flatten()
        });
        match id {
            Some(id) => crate::status::Target::Route(id),
            None => crate::status::Target::Unmatched,
        }
    };
    let code = if res.status >= 400 { res.error_code() } else { None };
    app.recorder().record_request(target, &method, &path, res.status, elapsed_us, &request_id, code.as_deref());

    tracing::info!(
        request_id = %request_id,
        method = %method,
        path = %path,
        status = res.status,
        code = code.as_deref().unwrap_or("-"),
        principal = %principal_id,
        elapsed_us,
        "request"
    );
    to_hyper(res)
}

async fn route_request(
    app: &Arc<App>,
    mut req: Request<Incoming>,
    client_ip: &str,
    request_id: &str,
) -> (WebCortexResponse, String) {
    let method = req.method().as_str().to_string();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let query_all = parse_query_pairs(uri.query().unwrap_or(""));
    let query: std::collections::BTreeMap<String, String> = query_all.iter().cloned().collect();

    let mut headers = collect_headers(req.headers());
    // HTTP/2 carries the host as the `:authority` pseudo-header, never as
    // `Host`; give handlers the same `host` header either way.
    if !headers.contains_key("host")
        && let Some(authority) = uri.authority()
    {
        headers.insert("host".into(), authority.as_str().to_string());
    }
    let origin = headers.get("origin").cloned();

    // 1. CORS preflight, before any rejection could pre-empt it. Only a
    //    browser's OPTIONS (one carrying an Origin) is treated as a preflight;
    //    an OPTIONS from anything else is an ordinary request, answered by the
    //    app's own OPTIONS route or the router's `Allow` reply. 2.5.3 refused
    //    every OPTIONS without an Origin with 403 "origin not allowed" once
    //    CORS was on, which also made a declared OPTIONS route unreachable.
    let preflight = method == "OPTIONS" && origin.is_some();
    if preflight && app.manifest.cors.enabled {
        if let Some(cors_headers) = app.cors.preflight(origin.as_deref()) {
            return (
                WebCortexResponse { status: 204, headers: cors_headers, body: Bytes::new() },
                "-".into(),
            );
        }
        return (WebCortexResponse::coded(403, "WC606", "origin not allowed"), "-".into());
    }

    // 1b. Cross-origin protection. A browser sends a "simple" cross-site POST
    //     without a preflight, and opens a WebSocket to any origin, so CORS
    //     alone does not stop a hostile page acting on an app it can reach.
    let upgrade = crate::ws::is_upgrade(&headers);
    let state_changing = !matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS");
    if state_changing || upgrade {
        let host = headers
            .get("host")
            .cloned()
            .or_else(|| uri.authority().map(|a| a.as_str().to_string()));
        if !middleware::cross_origin_allowed(&headers, host.as_deref(), &app.manifest.cors) {
            tracing::warn!(
                request_id = %request_id,
                method = %method,
                path = %path,
                origin = origin.as_deref().unwrap_or("-"),
                "refused a cross-origin browser request; list the origin in cors() to allow it"
            );
            let mut res = WebCortexResponse::coded(403, "WC605", "cross-origin request refused");
            finish_cors(app, &mut res, origin.as_deref());
            return (res, "-".into());
        }
    }

    // A browser cannot set headers on a WebSocket, so an upgrade may carry its
    // key in the query string instead. Only for upgrades, and only when no
    // header credential is present; the request log records the path, never
    // the query.
    if upgrade {
        let key_header = app.authenticator.api_key_header().to_string();
        if !headers.contains_key(&key_header) && !headers.contains_key("authorization")
            && let Some(token) = query.get("access_token") {
                headers.insert(key_header, token.clone());
            }
    }

    // 2. Authenticate. Done before rate limiting is *keyed*, but the limiter
    //    still falls back to IP when the caller is anonymous.
    let principal = match app.authenticator.authenticate(&headers) {
        Ok(p) => p,
        Err(e) => {
            // Failed credentials are charged to the client's address, so
            // guessing keys or tokens is held to the same rate as anything else.
            if let Some(limiter) = &app.rate_limiter {
                let decision = limiter.check(&format!("ip:{client_ip}"));
                if !decision.allowed {
                    let mut res = rate_limited(&decision);
                    finish_cors(app, &mut res, origin.as_deref());
                    return (res, "invalid".into());
                }
            }
            let mut res = WebCortexResponse::coded(e.status(), "WC603", e.message());
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
            let mut res = rate_limited(&decision);
            finish_cors(app, &mut res, origin.as_deref());
            return (res, principal_id);
        }
    }

    if upgrade {
        let mut res = crate::ws::accept(app.clone(), &mut req, &headers, &path, &query, principal);
        finish_cors(app, &mut res, origin.as_deref());
        return (res, principal_id);
    }

    // A model loop routinely outlasts an ordinary handler's ceiling, and a
    // timeout drops the run mid-flight.
    let limits = &app.manifest.server;
    let secs = if app.runs_a_model_loop(&method, &path) {
        limits.agent_timeout_secs.max(limits.request_timeout_secs)
    } else {
        limits.request_timeout_secs
    };

    // The body is read under the same limit: a client that announces a body
    // and then sends it a byte at a time must not hold the request open.
    let body_timeout = std::time::Duration::from_secs(secs.max(1));
    let body = match tokio::time::timeout(body_timeout, read_body(req)).await {
        Ok(Ok(b)) => b,
        Ok(Err(mut res)) => {
            finish_cors(app, &mut res, origin.as_deref());
            return (res, principal_id);
        }
        Err(_) => {
            let mut res =
                WebCortexResponse::coded(408, "WC216", "the request body did not arrive within the request timeout");
            res.headers.push(("connection".into(), "close".into()));
            finish_cors(app, &mut res, origin.as_deref());
            return (res, principal_id);
        }
    };

    let mut res = if let Some(rest) = control_path(&path, &app.manifest.server.control_prefix) {
        control_plane(app, &method, rest, &body, &principal, request_id).await
    } else {
        let timeout = std::time::Duration::from_secs(secs);
        let dispatch = app.dispatch(WebCortexRequest {
            method,
            path,
            path_params: Default::default(),
            query,
            query_all,
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
            Err(_) => WebCortexResponse::coded(504, "WC203", "handler exceeded the request timeout"),
        }
    };

    finish_cors(app, &mut res, origin.as_deref());
    (res, principal_id)
}

fn rate_limited(decision: &middleware::RateDecision) -> WebCortexResponse {
    let mut res = WebCortexResponse::coded(429, "WC401", "rate limit exceeded");
    res.headers.push(("retry-after".into(), decision.retry_after_secs.to_string()));
    res.headers.push(("x-ratelimit-limit".into(), decision.limit.to_string()));
    res.headers.push(("x-ratelimit-remaining".into(), "0".into()));
    res
}

/// A client-supplied `x-request-id` worth keeping: short, and only characters
/// that cannot confuse a log line or a header.
fn is_plain_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

fn finish_cors(app: &Arc<App>, res: &mut WebCortexResponse, origin: Option<&str>) {
    if app.manifest.cors.enabled {
        for (k, v) in app.cors.headers_for(origin) {
            if k == "vary" {
                merge_vary(&mut res.headers, &v);
            } else {
                push_absent(&mut res.headers, k, v);
            }
        }
        // The CORS headers depend on Origin, so every response says so — also
        // the ones that carry none. Otherwise a shared cache can store the
        // answer to a request without Origin and replay it, header-less, to
        // the trusted origin's browser.
        merge_vary(&mut res.headers, "Origin");
    }
}

/// The request's headers, keyed by lower-case name.
///
/// A repeated header is joined the way RFC 9110 §5.3 allows — `; ` for
/// `cookie` (HTTP/2 sends each cookie as its own field), `, ` otherwise —
/// where 2.5.3 kept only the last one. A value that is not visible ASCII is
/// kept as UTF-8, lossily if it must be, rather than dropped: dropping a
/// non-ASCII `Authorization` or API key header made the caller anonymous
/// instead of refusing the credential it sent.
fn collect_headers(map: &hyper::HeaderMap) -> std::collections::BTreeMap<String, String> {
    let mut headers: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (k, v) in map {
        let name = k.as_str().to_ascii_lowercase();
        let value = String::from_utf8_lossy(v.as_bytes()).into_owned();
        match headers.get_mut(&name) {
            Some(existing) => {
                existing.push_str(if name == "cookie" { "; " } else { ", " });
                existing.push_str(&value);
            }
            None => {
                headers.insert(name, value);
            }
        }
    }
    headers
}

/// The part of `path` under the control prefix, when it is under it. Matches
/// at a segment boundary only (`/_webcortexfoo` is an application path), and
/// ignores one trailing slash (`/_webcortex/health/` is the health check).
fn control_path<'p>(path: &'p str, prefix: &str) -> Option<&'p str> {
    let rest = path.strip_prefix(prefix)?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    Some(match rest.strip_suffix('/') {
        Some(trimmed) if !trimmed.is_empty() => trimmed,
        _ => rest,
    })
}

/// Add a response header unless the response already carries one by that name.
fn push_absent(headers: &mut Vec<(String, String)>, name: String, value: String) {
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(&name)) {
        headers.push((name, value));
    }
}

/// Add a token to `Vary`, once, alongside whatever the handler varied on.
fn merge_vary(headers: &mut Vec<(String, String)>, token: &str) {
    let present = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("vary"))
        .flat_map(|(_, v)| v.split(','))
        .any(|t| t.trim() == "*" || t.trim().eq_ignore_ascii_case(token));
    if !present {
        headers.push(("vary".into(), token.to_string()));
    }
}

/// One `Content-Type`, the last one set. A Python handler's
/// `Response(headers={"Content-Type": ...})` used to be sent alongside the
/// default `application/json`, and a client is free to honour either.
fn dedupe_content_type(headers: &mut Vec<(String, String)>) {
    let count = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("content-type")).count();
    if count <= 1 {
        return;
    }
    let mut seen = 0;
    headers.retain(|(k, _)| {
        if !k.eq_ignore_ascii_case("content-type") {
            return true;
        }
        seen += 1;
        seen == count
    });
}

/// Remove the headers that frame the message on the wire, which the server
/// owns. hyper trusts a `Content-Length` it is handed (it only asserts in
/// debug builds), so a Python handler that set one shorter than its body made
/// the rest of the body read as the *next* response on a keep-alive
/// connection — response splitting from one wrong header — and one longer
/// left the client waiting for bytes that never came. `Connection: close` is
/// kept (the server sends it on a 408); on a 101 the upgrade headers are the
/// point of the response and are left alone.
fn strip_framing_headers(res: &mut WebCortexResponse) {
    let upgrade = res.status == 101;
    res.headers.retain(|(k, v)| {
        let k = k.to_ascii_lowercase();
        match k.as_str() {
            "content-length" | "transfer-encoding" | "keep-alive" | "proxy-connection" | "te"
            | "trailer" => false,
            "connection" => upgrade || v.trim().eq_ignore_ascii_case("close"),
            _ => true,
        }
    });
}

/// Whether hyper can send this response: a final status, and header names and
/// values that are legal on the wire (no CR, LF or NUL to split a header with).
fn sendable(res: &WebCortexResponse) -> Result<(), String> {
    // 101 is the one interim status a response may carry: a WebSocket upgrade.
    if res.status != 101 && !(200..=599).contains(&res.status) {
        return Err(format!("{} is not a final HTTP status", res.status));
    }
    for (k, v) in &res.headers {
        if hyper::header::HeaderName::from_bytes(k.as_bytes()).is_err() {
            return Err(format!("{k:?} is not a valid header name"));
        }
        if hyper::header::HeaderValue::from_str(v).is_err() {
            return Err(format!("the value of header {k:?} contains a control character"));
        }
    }
    Ok(())
}

async fn read_body(req: Request<Incoming>) -> Result<Bytes, WebCortexResponse> {
    let too_large = || WebCortexResponse::coded(413, "WC204", format!("request body exceeds {MAX_BODY_BYTES} bytes"));
    // A declared length over the limit is never buffered. 2.5.3 held up to
    // 32 MB per connection of a request that was always going to be refused.
    // A client waiting for `100 Continue` is answered at once. One already
    // sending is read (and discarded) as far as the limit and drained a
    // little past it, exactly as an undeclared oversized body is, so it gets
    // to read the 413 instead of a reset.
    let declared = req
        .headers()
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let expects_continue = req
        .headers()
        .get(hyper::header::EXPECT)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"));
    let refused = declared.is_some_and(|n| n > MAX_BODY_BYTES as u64);
    if refused && expects_continue {
        return Err(too_large());
    }
    let mut body = req.into_body();
    let mut buf = bytes::BytesMut::new();
    let mut seen = 0usize;
    while let Some(frame) = body.frame().await {
        // A body that fails to decode (a malformed chunk size, a connection
        // cut mid-body) is a bad request, not an oversized one.
        let Ok(frame) = frame else {
            return Err(WebCortexResponse::coded(400, "WC205", "malformed or incomplete request body"));
        };
        let Ok(data) = frame.into_data() else { continue };
        seen += data.len();
        if seen > MAX_BODY_BYTES {
            drain(&mut body).await;
            return Err(too_large());
        }
        if !refused {
            buf.extend_from_slice(&data);
        }
    }
    if refused {
        // The client sent less than it declared; hyper reports that as an
        // error frame above, but answer the declaration either way.
        return Err(too_large());
    }
    Ok(buf.freeze())
}

/// Read and discard a bounded overshoot of a refused body (see [`DRAIN_BYTES`]).
async fn drain(body: &mut Incoming) {
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
        let code = if status == 401 { "WC604" } else { "WC602" };
        return WebCortexResponse::coded(
            status,
            code,
            "the webcortex control plane requires the 'webcortex:admin' scope",
        );
    }

    // HEAD is GET without the body; hyper drops the body.
    let method = if method == "HEAD" { "GET" } else { method };
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
                "halted": app.halted().is_some(),
                "devices": app.devices().len(),
                "watchers": app.manifest.watchers.len(),
            }),
        ),

        ("GET", "/devices") => WebCortexResponse::json(200, &app.devices().describe()),

        // The live view: what is this app doing right now.
        ("GET", "/status") => WebCortexResponse::json(200, &app.status()),

        // The error catalogue, so a client can explain a code it received.
        ("GET", "/errors") => WebCortexResponse::json(
            200,
            &serde_json::json!({"errors": crate::errors::catalogue_json()}),
        ),

        // The emergency stop. Engaging it needs the same admin scope as every
        // other control route, but nothing else: no body is required, because
        // whoever reaches for an e-stop should not have to compose JSON.
        ("GET", "/halt") => WebCortexResponse::json(
            200,
            &serde_json::json!({"halted": app.halted().is_some(), "state": app.halted()}),
        ),
        ("POST", "/halt") => {
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
            let reason = parsed
                .get("reason")
                .and_then(|r| r.as_str())
                .filter(|r| !r.is_empty())
                .unwrap_or("emergency stop");
            app.halt(reason, principal);
            WebCortexResponse::json(200, &serde_json::json!({"halted": true, "state": app.halted()}))
        }
        ("POST", "/release") => {
            let was = app.release(principal);
            WebCortexResponse::json(200, &serde_json::json!({"halted": false, "was_halted": was}))
        }

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
                return WebCortexResponse::coded(400, "WC205", "body must be {\"approve\": true|false, \"note\": \"…\"}");
            };
            let note = parsed.get("note").and_then(|v| v.as_str()).unwrap_or("");
            match app.resolve_approval(id, approve, note, principal).await {
                Ok(result) => crate::app::run_result_response(&result),
                Err(e) => WebCortexResponse::coded(404, "WC310", e),
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
                    "actuator": r.actuator,
                })).collect::<Vec<_>>()
            }),
        ),

        ("POST", "/mcp") => mcp::handle_request(app, body, principal, request_id).await,

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

        _ => WebCortexResponse::coded(
            404,
            "WC212",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_request_headers_are_joined_and_non_ascii_is_kept() {
        let mut map = hyper::HeaderMap::new();
        map.append("cookie", "a=1".parse().unwrap());
        map.append("cookie", "b=2".parse().unwrap());
        map.append("accept", "text/html".parse().unwrap());
        map.append("accept", "application/json".parse().unwrap());
        map.append("x-name", hyper::header::HeaderValue::from_bytes("café".as_bytes()).unwrap());
        map.append("x-api-key", hyper::header::HeaderValue::from_bytes(b"\xffkey").unwrap());
        let h = collect_headers(&map);
        assert_eq!(h["cookie"], "a=1; b=2");
        assert_eq!(h["accept"], "text/html, application/json");
        assert_eq!(h["x-name"], "café");
        // Present, so it is checked (and refused) rather than ignored.
        assert_eq!(h["x-api-key"], "\u{fffd}key");
    }

    #[test]
    fn the_control_prefix_matches_at_a_segment_boundary() {
        let p = "/_webcortex";
        assert_eq!(control_path("/_webcortex/health", p), Some("/health"));
        assert_eq!(control_path("/_webcortex/health/", p), Some("/health"));
        assert_eq!(control_path("/_webcortex", p), Some(""));
        assert_eq!(control_path("/_webcortex/", p), Some("/"));
        assert_eq!(control_path("/_webcortexfoo", p), None);
        assert_eq!(control_path("/other", p), None);
    }

    #[test]
    fn framework_headers_do_not_duplicate_the_handlers() {
        let mut h = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-Frame-Options".to_string(), "SAMEORIGIN".to_string()),
            ("content-type".to_string(), "text/csv".to_string()),
            ("Vary".to_string(), "Accept, origin".to_string()),
        ];
        push_absent(&mut h, "x-frame-options".into(), "DENY".into());
        push_absent(&mut h, "referrer-policy".into(), "no-referrer".into());
        merge_vary(&mut h, "Origin");
        dedupe_content_type(&mut h);
        assert_eq!(
            h,
            vec![
                ("X-Frame-Options".to_string(), "SAMEORIGIN".to_string()),
                ("content-type".to_string(), "text/csv".to_string()),
                ("Vary".to_string(), "Accept, origin".to_string()),
                ("referrer-policy".to_string(), "no-referrer".to_string()),
            ]
        );
        let mut plain = Vec::new();
        merge_vary(&mut plain, "Origin");
        merge_vary(&mut plain, "Origin");
        assert_eq!(plain, vec![("vary".to_string(), "Origin".to_string())]);
    }

    #[test]
    fn the_server_owns_message_framing() {
        let mut res = WebCortexResponse::text(200, "body");
        for (k, v) in [("Content-Length", "2"), ("transfer-encoding", "chunked"), ("Connection", "keep-alive, x"),
                       ("keep-alive", "timeout=5"), ("TE", "trailers"), ("trailer", "x"), ("x-kept", "1")] {
            res.headers.push((k.into(), v.into()));
        }
        strip_framing_headers(&mut res);
        let names: Vec<&str> = res.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["content-type", "x-kept"]);
        let mut closing = WebCortexResponse::text(408, "");
        closing.headers.push(("connection".into(), "close".into()));
        strip_framing_headers(&mut closing);
        assert!(closing.headers.iter().any(|(k, v)| k == "connection" && v == "close"));
        let mut upgrade = WebCortexResponse::text(101, "");
        upgrade.headers.push(("connection".into(), "Upgrade".into()));
        strip_framing_headers(&mut upgrade);
        assert!(upgrade.headers.iter().any(|(k, _)| k == "connection"));
    }

    #[test]
    fn an_unsendable_response_is_caught_before_hyper_sees_it() {
        let ok = WebCortexResponse::text(200, "x");
        assert!(sendable(&ok).is_ok());
        for status in [42, 100, 199, 600, 999] {
            assert!(sendable(&WebCortexResponse::text(status, "x")).is_err(), "{status}");
        }
        assert!(sendable(&WebCortexResponse::text(101, "")).is_ok(), "a WebSocket upgrade");
        let mut split = WebCortexResponse::text(200, "x");
        split.headers.push(("x-a".into(), "ok\r\nx-injected: yes".into()));
        assert!(sendable(&split).is_err());
        let mut bad_name = WebCortexResponse::text(200, "x");
        bad_name.headers.push(("bad name".into(), "v".into()));
        assert!(sendable(&bad_name).is_err());
        let mut utf8 = WebCortexResponse::text(200, "x");
        utf8.headers.push(("x-u".into(), "café".into()));
        assert!(sendable(&utf8).is_ok(), "obs-text is legal in a value");
    }

    #[test]
    fn only_a_short_plain_request_id_is_echoed() {
        assert!(is_plain_request_id("3f1c9a2e-7b1d-4c8e-9f00-1a2b3c4d5e6f"));
        assert!(is_plain_request_id("trace:abc_1.2"));
        assert!(!is_plain_request_id(""));
        assert!(!is_plain_request_id("a b"));
        assert!(!is_plain_request_id("<script>"));
        assert!(!is_plain_request_id(&"a".repeat(129)));
    }

    #[test]
    fn https_is_read_from_the_proxys_headers() {
        let h = |pairs: &[(&'static str, &'static str)]| {
            let mut m = hyper::HeaderMap::new();
            for (k, v) in pairs {
                m.insert(*k, hyper::header::HeaderValue::from_static(v));
            }
            m
        };
        assert!(!forwarded_https(&h(&[])));
        assert!(forwarded_https(&h(&[("x-forwarded-proto", "HTTPS")])));
        assert!(forwarded_https(&h(&[("x-forwarded-proto", "https, http")])), "the first hop counts");
        assert!(!forwarded_https(&h(&[("x-forwarded-proto", "http")])));
        assert!(forwarded_https(&h(&[("forwarded", "for=1.2.3.4;proto=\"https\";host=a")])));
        assert!(!forwarded_https(&h(&[("forwarded", "for=1.2.3.4;proto=http")])));
    }

    #[test]
    fn the_connection_refusal_is_a_complete_response() {
        let text = String::from_utf8(TOO_MANY_CONNECTIONS.clone()).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 503 "));
        assert!(head.contains(&format!("content-length: {}", body.len())));
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["error"]["code"], "WC218");
    }
}
