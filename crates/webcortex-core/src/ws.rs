//! WebSockets: devices stream in, subscribers and agents stream out.
//!
//! Three kinds of socket, each mounted on an ordinary route so it is held to
//! that route's scopes. Authentication happens before the upgrade, exactly as
//! for HTTP; a browser, which cannot set headers on a WebSocket, may pass its
//! key as `?access_token=` instead.
//!
//! * **Device ingest** (`DeviceAction::SocketIngest`): binary messages are
//!   encoded frames, text messages are telemetry JSON.
//! * **Device stream** (`DeviceAction::SocketStream`): every frame, telemetry
//!   reading and agent insight as it happens. A frame arrives as a JSON header
//!   then the image as a binary message; `?frames=meta` sends headers only.
//! * **Agent session** (any agent route): send `{"input", "images"?,
//!   "session_id"?, "reset"?}`, receive `{"type": "step"}` for each step as it
//!   happens and then `{"type": "result"}`. The socket stays open for the next
//!   turn.

use crate::app::App;
use crate::auth::Principal;
use crate::devices::{Device, Event, IngestError};
use crate::http::WebCortexResponse;
use crate::manifest::{DeviceAction, Op};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use hyper::Request;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast::error::RecvError;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};

type Socket = WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>;

/// Inbound message ceiling for sockets that only receive control messages.
const SMALL_MESSAGE: usize = 64 * 1024;
/// Inbound ceiling for an agent turn: input plus up to 16 images.
const AGENT_MESSAGE: usize = 32 * 1024 * 1024;
/// How long a socket waits on a peer: the time one send may take before the
/// peer is treated as gone, and how often a quiet stream is pinged.
#[derive(Clone, Copy)]
struct Timing {
    stalled_send: Duration,
    keepalive: Duration,
}

const TIMING: Timing = Timing { stalled_send: Duration::from_secs(30), keepalive: Duration::from_secs(30) };
/// Replies queued for a device before further ones are dropped.
const INGEST_REPLIES: usize = 16;

/// Set at server shutdown. A dropped sender (tests, an App never served)
/// means no shutdown is coming.
type Stop = tokio::sync::watch::Receiver<bool>;

/// Resolves when shutdown begins; never, if it cannot.
async fn stopped(stop: &mut Stop) {
    if stop.wait_for(|s| *s).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// The close a socket gets when the server shuts down (RFC 6455 §7.4.1).
fn going_away() -> Message {
    Message::Close(Some(CloseFrame { code: CloseCode::Away, reason: "server shutting down".into() }))
}

/// True for a WebSocket upgrade request.
pub fn is_upgrade(headers: &BTreeMap<String, String>) -> bool {
    headers
        .get("upgrade")
        .is_some_and(|v| v.split(',').any(|p| p.trim().eq_ignore_ascii_case("websocket")))
        && headers.contains_key("sec-websocket-key")
}

enum Mode {
    Ingest { device: String },
    Stream { device: String, binary: bool },
    Agent { agent: String },
}

/// Authorise and accept an upgrade. Returns the `101` (or the refusal); the
/// socket itself is served on a task once hyper hands the connection over.
pub fn accept(
    app: Arc<App>,
    req: &mut Request<Incoming>,
    headers: &BTreeMap<String, String>,
    path: &str,
    query: &BTreeMap<String, String>,
    principal: Principal,
) -> WebCortexResponse {
    if headers.get("sec-websocket-version").map(String::as_str) != Some("13") {
        let mut res = WebCortexResponse::coded(426, "WC213", "unsupported WebSocket version; use 13");
        res.headers.push(("sec-websocket-version".into(), "13".into()));
        return res;
    }
    // Device sockets are GET routes; an agent's socket rides on its POST route.
    let route = match app.authorize("GET", path, &principal) {
        Ok(r) => r,
        Err(e) if e.status == 405 => match app.authorize("POST", path, &principal) {
            Ok(r) => r,
            Err(e) => return e,
        },
        Err(e) => return e,
    };
    let (mode, max_message) = match &route.op {
        Op::Device { device, action: DeviceAction::SocketIngest } => {
            let max = app
                .devices()
                .get(device)
                .map(|d| d.def.max_frame_bytes.max(SMALL_MESSAGE))
                .unwrap_or(SMALL_MESSAGE);
            (Mode::Ingest { device: device.clone() }, max)
        }
        Op::Device { device, action: DeviceAction::SocketStream } => (
            Mode::Stream {
                device: device.clone(),
                binary: query.get("frames").map(String::as_str) != Some("meta"),
            },
            SMALL_MESSAGE,
        ),
        Op::Agent { agent } => (Mode::Agent { agent: agent.clone() }, AGENT_MESSAGE),
        _ => return WebCortexResponse::error(400, format!("{path} does not accept WebSocket connections")),
    };

    let Some(key) = headers.get("sec-websocket-key") else {
        return WebCortexResponse::error(400, "missing sec-websocket-key");
    };
    // The socket outlives the connection task that upgraded it, so it holds
    // its own place under max_connections, and its own shutdown watch.
    let Some(slot) = app.admit_connection() else {
        let mut res = WebCortexResponse::coded(503, "WC218", "too many open connections; retry shortly");
        res.headers.push(("retry-after".into(), "1".into()));
        return res;
    };
    let stop = app.shutdown_watch();
    let accept_key = derive_accept_key(key.as_bytes());
    let on_upgrade = hyper::upgrade::on(req);
    tokio::spawn(async move {
        let _slot = slot;
        let upgraded = match on_upgrade.await {
            Ok(u) => u,
            Err(e) => {
                tracing::debug!(error = %e, "websocket upgrade failed");
                return;
            }
        };
        let config = WebSocketConfig::default()
            .max_message_size(Some(max_message))
            .max_frame_size(Some(max_message));
        let ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(config)).await;
        match mode {
            Mode::Ingest { device } => ingest(app, device, principal, ws, stop).await,
            Mode::Stream { device, binary } => stream(app, device, binary, ws, stop).await,
            Mode::Agent { agent } => agent_session(app, agent, principal, ws, stop).await,
        }
    });

    WebCortexResponse {
        status: 101,
        headers: vec![
            ("upgrade".into(), "websocket".into()),
            ("connection".into(), "Upgrade".into()),
            ("sec-websocket-accept".into(), accept_key),
        ],
        body: bytes::Bytes::new(),
    }
}

fn text(v: Value) -> Message {
    Message::Text(v.to_string().into())
}

/// An error message on an open socket: the same `code` and `hint` an HTTP
/// error body carries, so a device or agent client can branch on the code.
fn error(code: &str, message: impl Into<String>) -> Message {
    text(json!({
        "type": "error",
        "message": message.into(),
        "code": code,
        "hint": crate::errors::hint(code),
    }))
}

async fn ingest<S>(app: Arc<App>, name: String, principal: Principal, ws: WebSocketStream<S>, stop: Stop)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Some(device) = app.devices().get(&name).cloned() else { return };
    tracing::info!(device = %name, principal = %principal.id, "device connected");
    ingest_loop(device, ws, TIMING, stop).await;
    tracing::info!(device = %name, "device disconnected");
}

/// A device's socket: frames and telemetry in, the occasional reply out.
///
/// Replies go out on their own task through a short queue. A device that
/// never reads them (most camera drivers only write) used to fill its TCP
/// window with error replies, after which the hub blocked sending the next
/// one and stopped reading frames: the device then blocked too, for good.
/// Now a reply that does not fit in the queue is dropped; `frames_rejected`
/// still counts every refusal.
async fn ingest_loop<S>(device: Arc<Device>, mut ws: WebSocketStream<S>, t: Timing, mut stop: Stop)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let name = device.def.name.clone();
    // Sent before anything is read, so it is always the first message.
    if !deliver(&mut ws, text(json!({"type": "ready", "device": name, "max_fps": device.def.max_fps})), t).await {
        return;
    }
    let (out, mut inbound) = ws.split();
    let (replies, queue) = tokio::sync::mpsc::channel::<Message>(INGEST_REPLIES);
    let writer = tokio::spawn(write_replies(out, queue, t));
    loop {
        let msg = tokio::select! {
            m = inbound.next() => match m {
                Some(m) => m,
                None => break,
            },
            _ = stopped(&mut stop) => {
                let _ = replies.send(going_away()).await;
                break;
            }
        };
        let reply = match msg {
            Ok(Message::Binary(data)) => match device.push_frame(data) {
                // Dropping a frame that came too fast is the point of max_fps,
                // not news worth a message per frame.
                Ok(_) | Err(IngestError::TooFast) => None,
                Err(e) => Some(error(e.code(), e.message())),
            },
            Ok(Message::Text(t)) => match serde_json::from_str::<Value>(t.as_str()) {
                Ok(v) => device.push_telemetry(v).err().map(|e| error(e.code(), e.message())),
                Err(e) => Some(error("WC505", format!("telemetry must be JSON: {e}"))),
            },
            // The writer's close flushes the reply tungstenite queued.
            Ok(Message::Close(_)) => break,
            Err(e) => {
                if let WsError::Capacity(c) = &e {
                    // Almost always a frame (telemetry is refused far below
                    // a camera's ceiling); count it like any refused frame.
                    if device.def.kind == crate::manifest::DeviceKind::Camera {
                        device.note_rejected();
                    }
                    // A frame over the socket's ceiling: say why before closing,
                    // in the log and (if the device still reads) a 1009 close,
                    // rather than a bare reset nobody can explain.
                    tracing::warn!(device = %name, error = %c, max_frame_bytes = device.def.max_frame_bytes,
                                   "device socket closed: message too large");
                    let _ = replies
                        .send(Message::Close(Some(CloseFrame {
                            code: CloseCode::Size,
                            reason: "[WC505] message over max_frame_bytes".into(),
                        })))
                        .await;
                } else {
                    tracing::debug!(device = %name, error = %e, "device socket error");
                    if let Some(close) = close_for(&e) {
                        let _ = replies.send(Message::Close(Some(close))).await;
                    }
                }
                break;
            }
            Ok(_) => None,
        };
        if let Some(reply) = reply {
            // Full means the device is not reading: drop the reply, not the frame.
            let _ = replies.try_send(reply);
        }
    }
    drop(replies);
    let _ = writer.await;
}

async fn write_replies<S>(
    mut out: SplitSink<WebSocketStream<S>, Message>,
    mut queue: tokio::sync::mpsc::Receiver<Message>,
    t: Timing,
)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(m) = queue.recv().await {
        let closing = matches!(m, Message::Close(_));
        if !deliver(&mut out, m, t).await || closing {
            break;
        }
    }
    // Completes the closing handshake: after the peer's close, this sends the
    // reply tungstenite queued; otherwise it sends our own close.
    let _ = tokio::time::timeout(t.stalled_send, out.close()).await;
}

/// Send one message, giving up on a peer that has stopped reading. Without
/// the limit a subscriber that connected and never read held its task, its
/// connection and its place in the subscriber count forever.
async fn deliver<S>(out: &mut S, m: Message, t: Timing) -> bool
where
    S: futures::Sink<Message> + Unpin,
{
    matches!(tokio::time::timeout(t.stalled_send, out.send(m)).await, Ok(Ok(())))
}

/// The close frame owed to a peer whose messages broke the protocol
/// (RFC 6455 §7.4.1), or `None` when the connection is simply gone.
fn close_for(e: &WsError) -> Option<CloseFrame> {
    use tokio_tungstenite::tungstenite::error::ProtocolError;
    let (code, reason) = match e {
        WsError::Capacity(_) => (CloseCode::Size, "message too large"),
        WsError::Utf8(_) => (CloseCode::Invalid, "text message is not valid UTF-8"),
        WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => return None,
        WsError::Protocol(_) => (CloseCode::Protocol, "protocol error"),
        _ => return None,
    };
    Some(CloseFrame { code, reason: reason.into() })
}

async fn stream<S>(app: Arc<App>, name: String, binary: bool, ws: WebSocketStream<S>, stop: Stop)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Some(device) = app.devices().get(&name).cloned() else { return };
    stream_loop(device, binary, ws, TIMING, stop).await;
}

async fn stream_loop<S>(device: Arc<Device>, binary: bool, ws: WebSocketStream<S>, t: Timing, mut stop: Stop)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut events = device.subscribe();
    let (mut out, mut inbound) = ws.split();

    let mut opening = vec![text(json!({
        "type": "hello", "device": device.describe(), "insights": device.insights(),
    }))];
    if let Some(f) = device.latest() {
        opening.push(text(frame_event(&f)));
        if binary {
            opening.push(Message::Binary(f.data.clone()));
        }
    }
    if let Some((_, t)) = device.telemetry() {
        opening.push(text(json!({"type": "telemetry", "data": *t})));
    }
    for m in opening {
        if !deliver(&mut out, m, t).await {
            return;
        }
    }

    // A quiet stream is pinged, so a viewer that vanished without closing is
    // eventually noticed and its subscription released.
    let mut keepalive = tokio::time::interval_at(tokio::time::Instant::now() + t.keepalive, t.keepalive);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let batch: Vec<Message> = tokio::select! {
            ev = events.recv() => match ev {
                Ok(Event::Frame(f)) => {
                    let mut v = vec![text(frame_event(&f))];
                    if binary {
                        v.push(Message::Binary(f.data.clone()));
                    }
                    v
                }
                Ok(Event::Telemetry(t)) => vec![text(json!({"type": "telemetry", "data": *t}))],
                Ok(Event::Insight(i)) => vec![text((*i).clone())],
                // A slow subscriber misses frames rather than slowing the
                // device or growing memory; it is told how many.
                Err(RecvError::Lagged(n)) => vec![text(json!({"type": "lagged", "skipped": n}))],
                Err(RecvError::Closed) => break,
            },
            m = inbound.next() => match m {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(e)) => {
                    if let Some(close) = close_for(&e) {
                        let _ = deliver(&mut out, Message::Close(Some(close)), t).await;
                    }
                    return;
                }
                _ => continue,
            },
            _ = keepalive.tick() => vec![Message::Ping(bytes::Bytes::new())],
            _ = stopped(&mut stop) => {
                let _ = deliver(&mut out, going_away(), t).await;
                break;
            }
        };
        for m in batch {
            if !deliver(&mut out, m, t).await {
                return;
            }
        }
        keepalive.reset();
    }
    let _ = tokio::time::timeout(t.stalled_send, out.close()).await;
}

fn frame_event(f: &crate::devices::Frame) -> Value {
    let mut v = f.meta();
    v["type"] = json!("frame");
    v
}

/// Turns a client may send ahead while one is running. They run in order once
/// it finishes; past this many the extra ones are refused, not buffered.
const MAX_QUEUED_TURNS: usize = 8;

async fn agent_session(app: Arc<App>, agent: String, principal: Principal, ws: Socket, mut stop: Stop) {
    let t = TIMING;
    let (mut out, mut inbound) = ws.split();
    if !deliver(&mut out, text(json!({"type": "ready", "agent": agent})), t).await {
        return;
    }
    let mut queued: std::collections::VecDeque<Message> = std::collections::VecDeque::new();
    // A turn in flight at shutdown is finished; the socket closes before the
    // next one, queued or not.
    loop {
        if *stop.borrow() {
            let _ = deliver(&mut out, going_away(), t).await;
            break;
        }
        let msg = match queued.pop_front() {
            Some(m) => Ok(m),
            None => tokio::select! {
                m = inbound.next() => match m {
                    Some(m) => m,
                    None => break,
                },
                _ = stopped(&mut stop) => {
                    let _ = deliver(&mut out, going_away(), t).await;
                    break;
                }
            },
        };
        let raw = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) => break,
            Err(e) => {
                if let Some(close) = close_for(&e) {
                    let _ = deliver(&mut out, Message::Close(Some(close)), t).await;
                }
                return;
            }
            // A binary message used to be ignored without a word, leaving the
            // client waiting for a result that would never come.
            Ok(Message::Binary(_)) => {
                if !deliver(&mut out, error("WC306", "send each turn as a JSON text message"), t).await {
                    return;
                }
                continue;
            }
            Ok(_) => continue,
        };
        let turn = match parse_turn(&raw, &principal) {
            Ok(t) => t,
            Err((code, e)) => {
                if !deliver(&mut out, error(code, e), t).await {
                    return;
                }
                continue;
            }
        };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let opts = crate::agent::RunOptions {
            images: turn.images,
            session_id: turn.session_id,
            reset_session: turn.reset,
            events: Some(tx),
            ..Default::default()
        };
        let limit = Duration::from_secs(app.manifest.server.agent_timeout_secs);
        let run = tokio::time::timeout(limit, app.invoke_agent(&agent, &turn.input, &principal, opts));
        tokio::pin!(run);
        // The socket is read during the turn too. 2.6.0 read it only between
        // turns, so a client that closed mid-turn kept its run, and the model
        // calls it paid for, going until the model's step finished.
        let outcome = loop {
            tokio::select! {
                Some(ev) = rx.recv() => {
                    if !deliver(&mut out, text(ev), t).await {
                        return;
                    }
                }
                m = inbound.next() => match m {
                    // Returning drops `run`, which cancels it.
                    Some(Ok(Message::Close(_))) | None => {
                        let _ = tokio::time::timeout(t.stalled_send, out.close()).await;
                        return;
                    }
                    Some(Err(e)) => {
                        if let Some(close) = close_for(&e) {
                            let _ = deliver(&mut out, Message::Close(Some(close)), t).await;
                        }
                        return;
                    }
                    Some(Ok(m @ (Message::Text(_) | Message::Binary(_)))) => {
                        if queued.len() < MAX_QUEUED_TURNS {
                            queued.push_back(m);
                        } else if !deliver(&mut out, error("WC306", format!(
                            "{MAX_QUEUED_TURNS} turns are already waiting; send the next once a result arrives"
                        )), t).await {
                            return;
                        }
                    }
                    Some(Ok(_)) => {}
                },
                done = &mut run => break done,
            }
        };
        while let Ok(ev) = rx.try_recv() {
            if !deliver(&mut out, text(ev), t).await {
                return;
            }
        }
        let reply = match outcome {
            Ok(Ok(result)) => text(json!({"type": "result", "result": result})),
            Ok(Err(e)) => error(crate::errors::classify_run_error(&e), e),
            Err(_) => error("WC203", "the run exceeded agent_timeout_secs"),
        };
        if !deliver(&mut out, reply, t).await {
            return;
        }
    }
    let _ = tokio::time::timeout(t.stalled_send, out.close()).await;
}

#[derive(Debug)]
struct Turn {
    input: String,
    images: Vec<Value>,
    session_id: Option<String>,
    reset: bool,
}

/// A turn, or the code and message of why it cannot be run.
fn parse_turn(raw: &str, principal: &Principal) -> Result<Turn, (&'static str, String)> {
    let body: Value = serde_json::from_str(raw).map_err(|e| ("WC306", format!("send JSON: {e}")))?;
    let input = body
        .get("input")
        .and_then(|v| v.as_str())
        .ok_or(("WC306", "each message needs an 'input' string".to_string()))?
        .to_string();
    let images = crate::agent::vision::input_images(body.get("images")).map_err(|e| ("WC313", e))?;
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if session_id.is_some() && principal.root_is_anonymous() {
        return Err(("WC607", "sessions need an authenticated caller".into()));
    }
    let reset = body.get("reset").and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(Turn { input, images, session_id, reset })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_detection_needs_the_header_and_a_key() {
        let mut h = BTreeMap::new();
        h.insert("upgrade".to_string(), "WebSocket".to_string());
        assert!(!is_upgrade(&h));
        h.insert("sec-websocket-key".to_string(), "dGhlIHNhbXBsZSBub25jZQ==".to_string());
        assert!(is_upgrade(&h));
        h.insert("upgrade".to_string(), "h2c".to_string());
        assert!(!is_upgrade(&h));
    }

    #[test]
    fn socket_errors_carry_a_code_and_its_hint() {
        let anon = Principal::anonymous();
        assert_eq!(parse_turn("not json", &anon).unwrap_err().0, "WC306");
        assert_eq!(parse_turn(r#"{"inptu": 1}"#, &anon).unwrap_err().0, "WC306");
        assert_eq!(parse_turn(r#"{"input": "x", "session_id": "s"}"#, &anon).unwrap_err().0, "WC607");
        let Message::Text(t) = error("WC505", "telemetry must be JSON") else { panic!("text") };
        let v: Value = serde_json::from_str(t.as_str()).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "WC505");
        assert_eq!(v["hint"], crate::errors::hint("WC505"));
        assert_eq!(v["message"], "telemetry must be JSON");
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn device(max_fps: f64) -> Arc<Device> {
        let def: crate::manifest::DeviceDef =
            serde_json::from_value(json!({"name": "cam", "max_fps": max_fps, "keep": 2})).unwrap();
        crate::devices::DeviceHub::new(&[def], reqwest::Client::new()).get("cam").unwrap().clone()
    }

    async fn server(io: tokio::io::DuplexStream) -> WebSocketStream<tokio::io::DuplexStream> {
        WebSocketStream::from_raw_socket(io, Role::Server, None).await
    }

    async fn client(io: tokio::io::DuplexStream) -> WebSocketStream<tokio::io::DuplexStream> {
        WebSocketStream::from_raw_socket(io, Role::Client, None).await
    }

    /// A shutdown watch whose sender is gone: shutdown never comes.
    fn no_stop() -> Stop {
        tokio::sync::watch::channel(false).1
    }

    const QUICK: Timing = Timing { stalled_send: Duration::from_millis(300), keepalive: Duration::from_secs(60) };

    /// A masked client frame, written by hand so it can be malformed.
    fn raw_frame(first: u8, payload: &[u8], mask: bool) -> Vec<u8> {
        let mut f = vec![first, (if mask { 0x80 } else { 0 }) | payload.len() as u8];
        let key = [1u8, 2, 3, 4];
        if mask {
            f.extend_from_slice(&key);
        }
        f.extend(payload.iter().enumerate().map(|(i, b)| if mask { b ^ key[i % 4] } else { *b }));
        f
    }

    /// The close code in the first close frame the server wrote back.
    async fn close_code_after(device: Arc<Device>, bytes: Vec<u8>) -> Option<u16> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (a, mut b) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(ingest_loop(device, server(a).await, QUICK, no_stop()));
        b.write_all(&bytes).await.unwrap();
        let mut got = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), b.read_to_end(&mut got)).await;
        task.abort();
        // Walk the server's (unmasked, short) frames to the close.
        let mut i = 0;
        while i + 2 <= got.len() {
            let (op, mut len, mut at) = (got[i] & 0x0f, (got[i + 1] & 0x7f) as usize, i + 2);
            if len == 126 {
                len = u16::from_be_bytes([got[at], got[at + 1]]) as usize;
                at += 2;
            }
            if op == 0x8 {
                return (len >= 2).then(|| u16::from_be_bytes([got[at], got[at + 1]]));
            }
            i = at + len;
        }
        None
    }

    #[tokio::test]
    async fn protocol_violations_are_answered_with_the_rfc_close_code() {
        // Through 2.5.3 the hub dropped the TCP connection without a close
        // frame, so a client saw 1006 and no reason.
        let unmasked = raw_frame(0x81, b"{}", false);
        assert_eq!(close_code_after(device(10.0), unmasked).await, Some(1002));
        let bad_utf8 = raw_frame(0x81, b"\xff\xfe", true);
        assert_eq!(close_code_after(device(10.0), bad_utf8).await, Some(1007));
        let reserved_bits = raw_frame(0xC1, b"{}", true);
        assert_eq!(close_code_after(device(10.0), reserved_bits).await, Some(1002));
        let lone_continuation = raw_frame(0x80, b"{}", true);
        assert_eq!(close_code_after(device(10.0), lone_continuation).await, Some(1002));
        // A clean close is answered with a close, completing the handshake.
        let close = raw_frame(0x88, &1000u16.to_be_bytes(), true);
        assert_eq!(close_code_after(device(10.0), close).await, Some(1000));
    }

    #[tokio::test]
    async fn a_device_that_never_reads_its_replies_is_still_read() {
        // Every one of these frames is refused with an error reply the device
        // never reads. In 2.5.3 the replies filled the socket and the hub
        // stopped reading after a couple of thousand; the device then blocked.
        let d = device(1e9);
        let (a, b) = tokio::io::duplex(4096);
        let task = tokio::spawn(ingest_loop(d.clone(), server(a).await, QUICK, no_stop()));
        let mut cam = client(b).await;
        let sending = async {
            for _ in 0..3000 {
                cam.send(Message::Binary(bytes::Bytes::from_static(b"BM not an image"))).await.unwrap();
            }
            cam.send(Message::Binary(bytes::Bytes::from_static(PNG))).await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(60), sending).await.expect("the device was blocked");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while d.describe()["frames_in"] != 1 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(d.describe()["frames_rejected"], 3000);
        assert_eq!(d.describe()["frames_in"], 1);
        task.abort();
    }

    #[tokio::test]
    async fn a_subscriber_that_stops_reading_is_let_go() {
        let d = device(1e9);
        let (a, _b) = tokio::io::duplex(1024);
        let task = tokio::spawn(stream_loop(d.clone(), true, server(a).await, QUICK, no_stop()));
        tokio::time::timeout(Duration::from_secs(10), async {
            while d.describe()["subscribers"] != 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut big = PNG.to_vec();
        big.resize(256 * 1024, 0);
        d.push_frame(bytes::Bytes::from(big)).unwrap();
        // The viewer reads nothing; its send stalls, and after stalled_send
        // the hub lets it go rather than holding it (and its subscription) forever.
        tokio::time::timeout(Duration::from_secs(30), task).await.expect("the stalled subscriber was held").unwrap();
        assert_eq!(d.describe()["subscribers"], 0);
        // Ingest was never held up by it.
        d.push_frame(bytes::Bytes::from_static(PNG)).unwrap();
    }

    #[tokio::test]
    async fn a_quiet_stream_is_pinged() {
        let d = device(10.0);
        let (a, b) = tokio::io::duplex(64 * 1024);
        let t = Timing { stalled_send: Duration::from_secs(5), keepalive: Duration::from_millis(100) };
        let task = tokio::spawn(stream_loop(d, true, server(a).await, t, no_stop()));
        let mut viewer = client(b).await;
        let hello = viewer.next().await.unwrap().unwrap();
        assert!(hello.to_text().unwrap().contains("hello"));
        let next = tokio::time::timeout(Duration::from_secs(10), viewer.next()).await.unwrap().unwrap().unwrap();
        assert!(matches!(next, Message::Ping(_)), "{next:?}");
        // And a viewer's close is answered, not cut off.
        viewer.close(None).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(10), viewer.next()).await.unwrap();
        assert!(matches!(reply, Some(Ok(Message::Close(_))) | None), "{reply:?}");
        tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn shutdown_closes_sockets_with_going_away() {
        // Through 2.6.0 a shutdown cut every socket with no close frame; a
        // client saw 1006 and could not tell a deploy from a crash.
        for ingest in [true, false] {
            let d = device(10.0);
            let (a, b) = tokio::io::duplex(64 * 1024);
            let (tx, rx) = tokio::sync::watch::channel(false);
            let task = if ingest {
                tokio::spawn(ingest_loop(d, server(a).await, QUICK, rx))
            } else {
                tokio::spawn(stream_loop(d, true, server(a).await, QUICK, rx))
            };
            let mut peer = client(b).await;
            let first = peer.next().await.unwrap().unwrap();
            assert!(matches!(first, Message::Text(_)), "{first:?}");
            tx.send_replace(true);
            let close = loop {
                match tokio::time::timeout(Duration::from_secs(10), peer.next()).await.unwrap() {
                    Some(Ok(Message::Close(c))) => break c,
                    Some(Ok(_)) => continue,
                    other => panic!("no close frame: {other:?}"),
                }
            };
            assert_eq!(close.map(|c| u16::from(c.code)), Some(1001), "ingest={ingest}");
            tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap();
        }
    }

    #[test]
    fn a_turn_needs_input_and_valid_images() {
        let anon = Principal::anonymous();
        assert!(parse_turn("{}", &anon).is_err());
        assert!(parse_turn("nope", &anon).is_err());
        assert!(parse_turn(r#"{"input": "hi", "session_id": "s"}"#, &anon).unwrap_err().1.contains("authenticated"));
        let t = parse_turn(r#"{"input": "hi"}"#, &anon).unwrap();
        assert_eq!(t.input, "hi");
        assert!(parse_turn(r#"{"input": "hi", "images": [{"url": "ftp://x"}]}"#, &anon).is_err());
    }
}
