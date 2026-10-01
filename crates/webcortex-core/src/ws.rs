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
use crate::devices::{Event, IngestError};
use crate::http::WebCortexResponse;
use crate::manifest::{DeviceAction, Op};
use futures::{SinkExt, StreamExt};
use hyper::Request;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

type Socket = WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>;

/// Inbound message ceiling for sockets that only receive control messages.
const SMALL_MESSAGE: usize = 64 * 1024;
/// Inbound ceiling for an agent turn: input plus up to 16 images.
const AGENT_MESSAGE: usize = 32 * 1024 * 1024;

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
        let mut res = WebCortexResponse::error(426, "unsupported WebSocket version; use 13");
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
    let accept_key = derive_accept_key(key.as_bytes());
    let on_upgrade = hyper::upgrade::on(req);
    tokio::spawn(async move {
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
            Mode::Ingest { device } => ingest(app, device, principal, ws).await,
            Mode::Stream { device, binary } => stream(app, device, binary, ws).await,
            Mode::Agent { agent } => agent_session(app, agent, principal, ws).await,
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

fn error(message: impl Into<String>) -> Message {
    text(json!({"type": "error", "message": message.into()}))
}

async fn ingest(app: Arc<App>, name: String, principal: Principal, mut ws: Socket) {
    let Some(device) = app.devices().get(&name).cloned() else { return };
    tracing::info!(device = %name, principal = %principal.id, "device connected");
    let _ = ws.send(text(json!({"type": "ready", "device": name, "max_fps": device.def.max_fps}))).await;
    while let Some(msg) = ws.next().await {
        let reply = match msg {
            Ok(Message::Binary(data)) => match device.push_frame(data) {
                // Dropping a frame that came too fast is the point of max_fps,
                // not news worth a message per frame.
                Ok(_) | Err(IngestError::TooFast) => None,
                Err(e) => Some(error(e.message())),
            },
            Ok(Message::Text(t)) => match serde_json::from_str::<Value>(t.as_str()) {
                Ok(v) => device.push_telemetry(v).err().map(|e| error(e.message())),
                Err(e) => Some(error(format!("telemetry must be JSON: {e}"))),
            },
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => None,
        };
        if let Some(reply) = reply {
            if ws.send(reply).await.is_err() {
                break;
            }
        }
    }
    tracing::info!(device = %name, "device disconnected");
}

async fn stream(app: Arc<App>, name: String, binary: bool, ws: Socket) {
    let Some(device) = app.devices().get(&name).cloned() else { return };
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
        if out.send(m).await.is_err() {
            return;
        }
    }

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
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => continue,
            },
        };
        for m in batch {
            if out.send(m).await.is_err() {
                return;
            }
        }
    }
}

fn frame_event(f: &crate::devices::Frame) -> Value {
    let mut v = f.meta();
    v["type"] = json!("frame");
    v
}

async fn agent_session(app: Arc<App>, agent: String, principal: Principal, ws: Socket) {
    let (mut out, mut inbound) = ws.split();
    if out.send(text(json!({"type": "ready", "agent": agent}))).await.is_err() {
        return;
    }
    while let Some(msg) = inbound.next().await {
        let raw = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        let turn = match parse_turn(&raw, &principal) {
            Ok(t) => t,
            Err(e) => {
                if out.send(error(e)).await.is_err() {
                    break;
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
        let outcome = loop {
            tokio::select! {
                Some(ev) = rx.recv() => {
                    if out.send(text(ev)).await.is_err() {
                        return;
                    }
                }
                done = &mut run => break done,
            }
        };
        while let Ok(ev) = rx.try_recv() {
            let _ = out.send(text(ev)).await;
        }
        let reply = match outcome {
            Ok(Ok(result)) => text(json!({"type": "result", "result": result})),
            Ok(Err(e)) => error(e),
            Err(_) => error("the run exceeded agent_timeout_secs"),
        };
        if out.send(reply).await.is_err() {
            break;
        }
    }
}

#[derive(Debug)]
struct Turn {
    input: String,
    images: Vec<Value>,
    session_id: Option<String>,
    reset: bool,
}

fn parse_turn(raw: &str, principal: &Principal) -> Result<Turn, String> {
    let body: Value = serde_json::from_str(raw).map_err(|e| format!("send JSON: {e}"))?;
    let input = body
        .get("input")
        .and_then(|v| v.as_str())
        .ok_or("each message needs an 'input' string")?
        .to_string();
    let images = crate::agent::vision::input_images(body.get("images"))?;
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if session_id.is_some() && principal.root_is_anonymous() {
        return Err("sessions need an authenticated caller".into());
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
    fn a_turn_needs_input_and_valid_images() {
        let anon = Principal::anonymous();
        assert!(parse_turn("{}", &anon).is_err());
        assert!(parse_turn("nope", &anon).is_err());
        assert!(parse_turn(r#"{"input": "hi", "session_id": "s"}"#, &anon).unwrap_err().contains("authenticated"));
        let t = parse_turn(r#"{"input": "hi"}"#, &anon).unwrap();
        assert_eq!(t.input, "hi");
        assert!(parse_turn(r#"{"input": "hi", "images": [{"url": "ftp://x"}]}"#, &anon).is_err());
    }
}
