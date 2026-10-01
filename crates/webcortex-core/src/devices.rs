//! The device hub: cameras and sensors on one side, agents and subscribers on
//! the other.
//!
//! A device pushes frames (WebSocket binary messages, or an HTTP POST of the
//! encoded image) and telemetry (WebSocket text, or a JSON POST). A camera
//! with a `source` URL is pulled instead, when a frame is asked for. Either
//! way the hub holds the newest frames in a ring buffer and fans every event
//! out to subscribers over a broadcast channel. No Python runs per frame.
//!
//! What makes it a hub for agents rather than a video relay:
//!
//! * a device's `snapshot` route is a **tool**: an agent calling it receives
//!   the newest frame as an image it can see (see [`crate::agent::vision`]);
//! * a **watcher** hands the newest frame to an agent on a timer and publishes
//!   the answer to the device's subscribers and to a webhook, so a camera's
//!   output reaches other systems already interpreted;
//! * ingest is **rate-limited** per device and frames are **size-capped**, so
//!   a misbehaving camera cannot flood the process.

use crate::app::App;
use crate::auth::{Principal, PrincipalKind};
use crate::manifest::{DeviceDef, DeviceKind, WatcherDef};
use base64::Engine;
use bytes::Bytes;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// Telemetry larger than this is refused: it is re-sent to every agent that
/// reads it.
pub const MAX_TELEMETRY_BYTES: usize = 64 * 1024;
/// Insights kept per device.
const KEEP_INSIGHTS: usize = 32;
/// Events buffered per subscriber before a slow one starts missing them.
const CHANNEL_CAPACITY: usize = 64;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One encoded image from a device.
#[derive(Debug)]
pub struct Frame {
    pub seq: u64,
    pub media_type: &'static str,
    pub data: Bytes,
    pub at_ms: u64,
}

impl Frame {
    /// The `$image` marker an agent or MCP client sees.
    pub fn marker(&self) -> Value {
        json!({"$image": {
            "media_type": self.media_type,
            "data": base64::engine::general_purpose::STANDARD.encode(&self.data),
        }})
    }

    /// Metadata only, for stream headers and listings.
    pub fn meta(&self) -> Value {
        json!({"seq": self.seq, "media_type": self.media_type, "bytes": self.data.len(),
               "at_ms": self.at_ms})
    }
}

/// Something that happened on a device, as subscribers receive it.
#[derive(Debug, Clone)]
pub enum Event {
    Frame(Arc<Frame>),
    Telemetry(Arc<Value>),
    Insight(Arc<Value>),
}

/// Why an ingest was refused.
#[derive(Debug, PartialEq)]
pub enum IngestError {
    /// Faster than `max_fps`. A WebSocket drops the frame quietly; HTTP gets 429.
    TooFast,
    Invalid(String),
}

impl IngestError {
    pub fn status(&self) -> u16 {
        match self {
            IngestError::TooFast => 429,
            IngestError::Invalid(_) => 422,
        }
    }
    pub fn message(&self) -> String {
        match self {
            IngestError::TooFast => "frame dropped: faster than this device's max_fps".into(),
            IngestError::Invalid(m) => m.clone(),
        }
    }
}

/// The media type of encoded image bytes, from their magic number.
pub fn sniff(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub struct Device {
    pub def: DeviceDef,
    frames: Mutex<VecDeque<Arc<Frame>>>,
    telemetry: Mutex<Option<(u64, Arc<Value>)>>,
    insights: Mutex<VecDeque<Value>>,
    seq: AtomicU64,
    last_ingest: Mutex<Option<Instant>>,
    frames_in: AtomicU64,
    frames_dropped: AtomicU64,
    tx: broadcast::Sender<Event>,
}

impl Device {
    fn new(def: DeviceDef) -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            def,
            frames: Mutex::new(VecDeque::new()),
            telemetry: Mutex::new(None),
            insights: Mutex::new(VecDeque::new()),
            seq: AtomicU64::new(0),
            last_ingest: Mutex::new(None),
            frames_in: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
            tx,
        }
    }

    /// Accept one encoded frame. The type is sniffed from the bytes; the
    /// declared content type is only a fallback for the error message.
    pub fn push_frame(&self, data: Bytes) -> Result<u64, IngestError> {
        if self.def.kind == DeviceKind::Sensor {
            return Err(IngestError::Invalid(format!(
                "{:?} is a sensor; send telemetry, not frames",
                self.def.name
            )));
        }
        if data.is_empty() {
            return Err(IngestError::Invalid("empty frame".into()));
        }
        if data.len() > self.def.max_frame_bytes {
            return Err(IngestError::Invalid(format!(
                "frame is {} bytes; this device accepts at most {}",
                data.len(),
                self.def.max_frame_bytes
            )));
        }
        let media_type = sniff(&data).ok_or_else(|| {
            IngestError::Invalid("frame is not a PNG, JPEG, GIF or WebP image".into())
        })?;
        {
            let interval = Duration::from_secs_f64(1.0 / self.def.max_fps);
            let mut last = self.last_ingest.lock().unwrap_or_else(|p| p.into_inner());
            if last.is_some_and(|t| t.elapsed() < interval) {
                self.frames_dropped.fetch_add(1, Ordering::Relaxed);
                return Err(IngestError::TooFast);
            }
            *last = Some(Instant::now());
        }
        Ok(self.store(media_type, data))
    }

    fn store(&self, media_type: &'static str, data: Bytes) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let frame = Arc::new(Frame { seq, media_type, data, at_ms: now_ms() });
        {
            let mut frames = self.frames.lock().unwrap_or_else(|p| p.into_inner());
            frames.push_back(frame.clone());
            while frames.len() > self.def.keep {
                frames.pop_front();
            }
        }
        self.frames_in.fetch_add(1, Ordering::Relaxed);
        // No subscribers is not an error.
        let _ = self.tx.send(Event::Frame(frame));
        seq
    }

    pub fn push_telemetry(&self, value: Value) -> Result<(), IngestError> {
        let size = serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(usize::MAX);
        if size > MAX_TELEMETRY_BYTES {
            return Err(IngestError::Invalid(format!(
                "telemetry is {size} bytes; the limit is {MAX_TELEMETRY_BYTES}"
            )));
        }
        let value = Arc::new(value);
        *self.telemetry.lock().unwrap_or_else(|p| p.into_inner()) = Some((now_ms(), value.clone()));
        let _ = self.tx.send(Event::Telemetry(value));
        Ok(())
    }

    pub fn publish_insight(&self, insight: Value) {
        {
            let mut all = self.insights.lock().unwrap_or_else(|p| p.into_inner());
            all.push_back(insight.clone());
            while all.len() > KEEP_INSIGHTS {
                all.pop_front();
            }
        }
        let _ = self.tx.send(Event::Insight(Arc::new(insight)));
    }

    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.frames.lock().unwrap_or_else(|p| p.into_inner()).back().cloned()
    }

    /// `(received_at_ms, value)` of the newest telemetry.
    pub fn telemetry(&self) -> Option<(u64, Arc<Value>)> {
        self.telemetry.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn insights(&self) -> Vec<Value> {
        self.insights.lock().unwrap_or_else(|p| p.into_inner()).iter().cloned().collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn describe(&self) -> Value {
        json!({
            "name": self.def.name,
            "kind": self.def.kind,
            "description": self.def.description,
            "pulled": self.def.source.is_some(),
            "latest": self.latest().map(|f| f.meta()),
            "telemetry_at_ms": self.telemetry().map(|(at, _)| at),
            "frames_in": self.frames_in.load(Ordering::Relaxed),
            "frames_dropped": self.frames_dropped.load(Ordering::Relaxed),
            "subscribers": self.tx.receiver_count(),
            "insights": self.insights.lock().unwrap_or_else(|p| p.into_inner()).len(),
            "max_fps": self.def.max_fps,
        })
    }
}

pub struct DeviceHub {
    devices: HashMap<String, Arc<Device>>,
    client: reqwest::Client,
}

impl DeviceHub {
    pub fn new(defs: &[DeviceDef], client: reqwest::Client) -> Self {
        Self {
            devices: defs.iter().map(|d| (d.name.clone(), Arc::new(Device::new(d.clone())))).collect(),
            client,
        }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<Device>> {
        self.devices.get(name)
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    pub fn describe(&self) -> Value {
        let mut all: Vec<Value> = self.devices.values().map(|d| d.describe()).collect();
        all.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        json!({"devices": all})
    }

    /// The newest frame, fetching a fresh one first when the device is pulled.
    pub async fn current_frame(&self, device: &Device) -> Result<Option<Arc<Frame>>, String> {
        let Some(src) = &device.def.source else { return Ok(device.latest()) };
        let mut req = self
            .client
            .get(&src.url)
            .timeout(Duration::from_millis(src.timeout_ms));
        for (k, v) in &src.headers {
            req = req.header(k, v);
        }
        if let Some(token) = src.bearer_env.as_ref().and_then(|e| std::env::var(e).ok()) {
            req = req.bearer_auth(token);
        }
        let res = req
            .send()
            .await
            .map_err(|e| format!("camera {:?} did not answer: {e}", device.def.name))?;
        if !res.status().is_success() {
            return Err(format!("camera {:?} answered {}", device.def.name, res.status()));
        }
        let data = res
            .bytes()
            .await
            .map_err(|e| format!("camera {:?} sent an unreadable body: {e}", device.def.name))?;
        if data.len() > device.def.max_frame_bytes {
            return Err(format!("camera {:?} sent {} bytes, over max_frame_bytes", device.def.name, data.len()));
        }
        let media_type = sniff(&data)
            .ok_or_else(|| format!("camera {:?} did not send a PNG, JPEG, GIF or WebP image", device.def.name))?;
        // A pull is not rate-limited like a push: it happens because someone asked.
        device.store(media_type, data);
        Ok(device.latest())
    }

    /// The JSON a `snapshot` route and tool return.
    pub async fn snapshot(&self, device: &Device) -> Result<Value, String> {
        let frame = if device.def.kind == DeviceKind::Camera {
            self.current_frame(device).await?
        } else {
            None
        };
        let telemetry = device.telemetry();
        if frame.is_none() && telemetry.is_none() {
            return Err(format!(
                "device {:?} has not sent anything yet; connect it first",
                device.def.name
            ));
        }
        let now = now_ms();
        Ok(json!({
            "device": device.def.name,
            "description": device.def.description,
            "frame": frame.as_ref().map(|f| f.marker()),
            "seq": frame.as_ref().map(|f| f.seq),
            "age_ms": frame.as_ref().map(|f| now.saturating_sub(f.at_ms)),
            "telemetry": telemetry.as_ref().map(|(_, v)| (**v).clone()),
            "telemetry_age_ms": telemetry.as_ref().map(|(at, _)| now.saturating_sub(*at)),
        }))
    }
}

// ---------------------------------------------------------------------------
// Watchers
// ---------------------------------------------------------------------------

/// What a watcher remembers between ticks.
#[derive(Default)]
pub struct WatchState {
    last_digest: Option<Vec<u8>>,
    runs: VecDeque<Instant>,
}

/// Start every declared watcher. Each is a task on the runtime; they end with
/// the process.
pub fn spawn_watchers(app: Arc<App>) -> Vec<tokio::task::JoinHandle<()>> {
    app.manifest
        .watchers
        .clone()
        .into_iter()
        .map(|w| {
            let app = app.clone();
            tokio::spawn(async move {
                let mut state = WatchState::default();
                let mut tick = tokio::time::interval(Duration::from_secs_f64(w.every_secs));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    run_watcher_once(&app, &w, &mut state).await;
                }
            })
        })
        .collect()
}

/// One tick: look, decide whether anything changed, ask the agent, publish.
/// Returns the insight when the agent ran.
pub async fn run_watcher_once(app: &App, w: &WatcherDef, state: &mut WatchState) -> Option<Value> {
    let hub = app.devices();
    let device = hub.get(&w.device)?;

    let frame = match device.def.kind {
        DeviceKind::Camera => match hub.current_frame(device).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(watcher = %w.name, error = %e, "watcher could not get a frame");
                return None;
            }
        },
        DeviceKind::Sensor => None,
    };
    let telemetry = device.telemetry().map(|(_, v)| v);
    if frame.is_none() && telemetry.is_none() {
        return None;
    }

    // Changed means different bytes, not a new sequence number: a pulled
    // camera pointed at an empty room produces new frames that say nothing new.
    let digest = {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        if let Some(f) = &frame {
            h.update(&f.data);
        }
        if let Some(t) = &telemetry {
            h.update(t.to_string().as_bytes());
        }
        h.finalize().to_vec()
    };
    if w.only_on_change && state.last_digest.as_ref() == Some(&digest) {
        return None;
    }

    let hour = Duration::from_secs(3600);
    while state.runs.front().is_some_and(|t| t.elapsed() > hour) {
        state.runs.pop_front();
    }
    if state.runs.len() >= w.max_runs_per_hour as usize {
        tracing::debug!(watcher = %w.name, "watcher at max_runs_per_hour; skipping");
        return None;
    }
    state.runs.push_back(Instant::now());
    state.last_digest = Some(digest);

    let images = frame
        .as_ref()
        .and_then(|f| crate::agent::vision::image_block(&f.marker()).ok())
        .into_iter()
        .collect();
    let input = match &telemetry {
        Some(t) => format!("{}\n\nLatest telemetry from {}: {t}", w.input, w.device),
        None => w.input.clone(),
    };
    let principal = Principal {
        id: format!("watcher:{}", w.name),
        kind: PrincipalKind::ApiKey,
        scopes: w.scopes.clone(),
        claims: Default::default(),
    };
    let opts = crate::agent::RunOptions { images, ..Default::default() };
    let result = match app.invoke_agent(&w.agent, &input, &principal, opts).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(watcher = %w.name, error = %e, "watcher could not run its agent");
            return None;
        }
    };

    let insight = json!({
        "type": "insight",
        "watcher": w.name,
        "device": w.device,
        "agent": result.agent,
        "seq": frame.as_ref().map(|f| f.seq),
        "status": result.status,
        "output": result.output,
        // Why a run that failed failed: without it a monitor shows "failed" and nothing else.
        "error": result.steps.iter().rev().find_map(|s| s.error.clone()),
        "run_id": result.run_id,
        "tokens": result.usage.total_tokens(),
        "at_ms": now_ms(),
    });
    device.publish_insight(insight.clone());
    app.audit.record(crate::audit::AuditEvent {
        kind: "watcher_run".into(),
        run_id: result.run_id.clone(),
        actor: Some(principal.id.clone()),
        tool: None,
        detail: json!({"watcher": w.name, "device": w.device, "status": result.status}),
    });

    if let Some(hook) = &w.webhook {
        let client = app.http_client().clone();
        let hook = hook.clone();
        let body = insight.clone();
        let name = w.name.clone();
        tokio::spawn(async move {
            let mut req = client
                .post(&hook.url)
                .timeout(Duration::from_millis(hook.timeout_ms))
                .json(&body);
            if let Some(token) = hook.bearer_env.as_ref().and_then(|e| std::env::var(e).ok()) {
                req = req.bearer_auth(token);
            }
            match req.send().await {
                Ok(res) if res.status().is_success() => {}
                Ok(res) => tracing::warn!(watcher = %name, status = %res.status(), "webhook refused the insight"),
                Err(e) => tracing::warn!(watcher = %name, error = %e, "webhook failed"),
            }
        });
    }
    Some(insight)
}

// ---------------------------------------------------------------------------
// Pages: a browser becomes a camera, or a monitor
// ---------------------------------------------------------------------------

/// Device names are the developer's, but a page is still no place for raw text.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Turns any browser with a camera — a phone, a laptop — into this device.
/// Browsers only grant camera access on https or localhost.
pub const CONNECT_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>__DEVICE__ · connect a camera</title>
<style>
body{font:15px system-ui,sans-serif;margin:0;background:#111;color:#eee}
main{max-width:640px;margin:auto;padding:16px}
video{width:100%;border-radius:8px;background:#000}
label{display:block;margin:10px 0 4px;color:#aaa;font-size:13px}
input,select,button{font:inherit;padding:8px;border-radius:6px;border:1px solid #444;background:#222;color:#eee;width:100%;box-sizing:border-box}
button{background:#2f6fde;border:0;margin-top:14px;cursor:pointer}
#status{margin-top:12px;font-family:ui-monospace,monospace;font-size:13px;color:#9c9}
.row{display:flex;gap:8px}.row>*{flex:1}
</style></head><body><main>
<h2>Connect a camera to <code>__DEVICE__</code></h2>
<video id="v" autoplay playsinline muted></video>
<label>API key (needs this device's ingest scope; kept in this browser)</label>
<input id="key" type="password" autocomplete="off">
<div class="row">
<div><label>Camera</label><select id="facing"><option value="environment">back</option><option value="user">front</option></select></div>
<div><label>Frames per second</label><select id="fps"><option>0.5</option><option selected>1</option><option>2</option><option>5</option><option>10</option></select></div>
<div><label>Width</label><select id="width"><option>320</option><option selected>640</option><option>1024</option></select></div>
</div>
<button id="go">Start streaming</button>
<div id="status">idle</div>
</main><script>
const base = location.pathname.replace(/\/connect\/?$/, "");
const $ = id => document.getElementById(id);
$("key").value = localStorage.getItem("wcx_key") || "";
let ws, timer, sent = 0;
$("go").onclick = async () => {
  if (ws) { ws.close(); clearInterval(timer); ws = null; $("go").textContent = "Start streaming"; return; }
  localStorage.setItem("wcx_key", $("key").value);
  let stream;
  try { stream = await navigator.mediaDevices.getUserMedia({video: {facingMode: $("facing").value}}); }
  catch (e) { $("status").textContent = "camera unavailable: " + e.message + " (browsers need https or localhost)"; return; }
  $("v").srcObject = stream;
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  ws = new WebSocket(proto + "//" + location.host + base + "/ws?access_token=" + encodeURIComponent($("key").value));
  ws.onopen = () => {
    $("go").textContent = "Stop";
    const canvas = document.createElement("canvas");
    timer = setInterval(() => {
      const v = $("v"); if (!v.videoWidth || ws.readyState !== 1) return;
      const w = +$("width").value; canvas.width = w; canvas.height = Math.round(w * v.videoHeight / v.videoWidth);
      canvas.getContext("2d").drawImage(v, 0, 0, canvas.width, canvas.height);
      canvas.toBlob(b => { if (b && ws && ws.readyState === 1) { ws.send(b); sent++; $("status").textContent = "streaming · " + sent + " frames sent"; } }, "image/jpeg", 0.75);
    }, 1000 / +$("fps").value);
  };
  ws.onmessage = m => { try { const e = JSON.parse(m.data); if (e.type === "error") $("status").textContent = "error: " + e.message; } catch (_) {} };
  ws.onclose = e => { clearInterval(timer); $("status").textContent = "disconnected" + (e.reason ? ": " + e.reason : " (check the key)"); ws = null; $("go").textContent = "Start streaming"; };
};
</script></body></html>
"#;

/// A live monitor: the stream, the telemetry, and what the agents said.
pub const VIEW_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>__DEVICE__ · live</title>
<style>
body{font:15px system-ui,sans-serif;margin:0;background:#111;color:#eee}
main{max-width:900px;margin:auto;padding:16px;display:grid;grid-template-columns:3fr 2fr;gap:16px}
img{width:100%;border-radius:8px;background:#000;min-height:120px}
input,button{font:inherit;padding:8px;border-radius:6px;border:1px solid #444;background:#222;color:#eee}
pre{background:#1b1b1b;padding:8px;border-radius:6px;overflow:auto;font-size:12px}
.insight{border-left:3px solid #2f6fde;padding:6px 10px;margin:8px 0;background:#181818;font-size:14px;white-space:pre-wrap}
.meta{color:#888;font-size:12px}
@media(max-width:700px){main{grid-template-columns:1fr}}
</style></head><body><main>
<section><h2><code>__DEVICE__</code></h2><img id="img" alt="waiting for a frame">
<p class="meta" id="frame">no frame yet</p>
<input id="key" type="password" placeholder="API key with read scope"> <button id="go">Watch</button></section>
<section><h3>Telemetry</h3><pre id="tel">—</pre><h3>Insights</h3><div id="ins"></div></section>
</main><script>
const base = location.pathname.replace(/\/view\/?$/, "");
const $ = id => document.getElementById(id);
$("key").value = localStorage.getItem("wcx_key") || "";
$("go").onclick = () => {
  localStorage.setItem("wcx_key", $("key").value);
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const ws = new WebSocket(proto + "//" + location.host + base + "/stream?access_token=" + encodeURIComponent($("key").value));
  ws.binaryType = "blob";
  let url;
  ws.onmessage = m => {
    if (typeof m.data !== "string") { if (url) URL.revokeObjectURL(url); url = URL.createObjectURL(m.data); $("img").src = url; return; }
    const e = JSON.parse(m.data);
    if (e.type === "frame") $("frame").textContent = "frame " + e.seq + " · " + e.media_type + " · " + e.bytes + " bytes";
    if (e.type === "telemetry") $("tel").textContent = JSON.stringify(e.data, null, 2);
    if (e.type === "insight") { const d = document.createElement("div"); d.className = "insight";
      d.textContent = e.output || (e.error ? "error: " + e.error : ""); const s = document.createElement("div"); s.className = "meta";
      s.textContent = e.watcher + " → " + e.agent + " · " + e.status + " · " + e.tokens + " tokens"; d.prepend(s); $("ins").prepend(d); }
    if (e.type === "hello") (e.insights || []).forEach(i => ws.onmessage({data: JSON.stringify(i)}));
  };
  ws.onclose = e => { $("frame").textContent = "disconnected" + (e.reason ? ": " + e.reason : " (check the key)"); };
};
</script></body></html>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::DeviceDef;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn def(max_fps: f64, keep: usize) -> DeviceDef {
        serde_json::from_value(json!({"name": "cam", "max_fps": max_fps, "keep": keep})).unwrap()
    }

    #[test]
    fn frames_are_sniffed_ring_buffered_and_broadcast() {
        let d = Device::new(def(1000.0, 2));
        let mut rx = d.subscribe();
        for _ in 0..3 {
            d.push_frame(Bytes::from_static(PNG)).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(d.latest().unwrap().seq, 3);
        assert_eq!(d.frames.lock().unwrap().len(), 2, "keep=2");
        match rx.try_recv().unwrap() {
            Event::Frame(f) => assert_eq!((f.seq, f.media_type), (1, "image/png")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ingest_refuses_floods_garbage_and_oversize() {
        let d = Device::new(def(1.0, 4));
        d.push_frame(Bytes::from_static(PNG)).unwrap();
        assert_eq!(d.push_frame(Bytes::from_static(PNG)), Err(IngestError::TooFast));
        assert_eq!(d.describe()["frames_dropped"], 1);
        let d = Device::new(def(1000.0, 4));
        assert!(matches!(d.push_frame(Bytes::from_static(b"hello")), Err(IngestError::Invalid(_))));
        let mut small = def(1000.0, 4);
        small.max_frame_bytes = 4;
        assert!(matches!(Device::new(small).push_frame(Bytes::from_static(PNG)), Err(IngestError::Invalid(_))));
        let big = json!({"x": "y".repeat(MAX_TELEMETRY_BYTES)});
        assert!(d.push_telemetry(big).is_err());
    }

    #[test]
    fn a_sensor_takes_telemetry_not_frames() {
        let mut s = def(10.0, 4);
        s.kind = DeviceKind::Sensor;
        let d = Device::new(s);
        assert!(d.push_frame(Bytes::from_static(PNG)).is_err());
        d.push_telemetry(json!({"temp_c": 21.5})).unwrap();
        assert_eq!(d.telemetry().unwrap().1["temp_c"], 21.5);
    }

    #[test]
    fn insights_are_kept_and_bounded() {
        let d = Device::new(def(10.0, 4));
        for i in 0..(KEEP_INSIGHTS + 5) {
            d.publish_insight(json!({"i": i}));
        }
        let all = d.insights();
        assert_eq!(all.len(), KEEP_INSIGHTS);
        assert_eq!(all.last().unwrap()["i"], KEEP_INSIGHTS + 4);
    }

    #[tokio::test]
    async fn a_watcher_runs_on_change_and_stops_at_its_hourly_ceiling() {
        use crate::agent::AgentRuntime;
        use crate::agent::provider::ScriptedProvider;
        let manifest: crate::manifest::Manifest = serde_json::from_value(json!({
            "name": "w",
            "devices": [{"name": "cam", "max_fps": 1000}],
            "agents": [{"name": "eye", "model": "test", "tools": []}],
            "watchers": [{"name": "w", "device": "cam", "agent": "eye", "input": "look",
                          "every_secs": 1, "max_runs_per_hour": 2}]
        }))
        .unwrap();
        let provider = Arc::new(ScriptedProvider::text("all clear"));
        let rt = AgentRuntime::new(provider.clone(), Arc::new(crate::audit::MemoryAudit::default()));
        let app = App::build_without_python(manifest).await.unwrap().with_agent_runtime(rt);
        let w = app.manifest.watchers[0].clone();
        let mut state = WatchState::default();

        assert!(run_watcher_once(&app, &w, &mut state).await.is_none(), "nothing to look at yet");
        let cam = app.devices().get("cam").unwrap().clone();
        cam.push_frame(Bytes::from_static(PNG)).unwrap();
        let insight = run_watcher_once(&app, &w, &mut state).await.expect("a new frame runs the agent");
        assert_eq!(insight["output"], "all clear");
        assert_eq!(cam.insights().len(), 1);
        let seen = provider.snapshots()[0].messages[0].clone();
        assert_eq!(seen["content"][0]["type"], "image", "the agent was shown the frame");

        assert!(run_watcher_once(&app, &w, &mut state).await.is_none(), "same frame: skipped");
        std::thread::sleep(Duration::from_millis(2));
        cam.push_frame(Bytes::from_static(b"\x89PNG\r\n\x1a\n-different")).unwrap();
        assert!(run_watcher_once(&app, &w, &mut state).await.is_some());
        std::thread::sleep(Duration::from_millis(2));
        cam.push_frame(Bytes::from_static(b"\x89PNG\r\n\x1a\n-third")).unwrap();
        assert!(run_watcher_once(&app, &w, &mut state).await.is_none(), "two runs an hour, and both are spent");
        assert_eq!(provider.calls(), 2);
    }

    #[test]
    fn sniff_knows_the_four_formats() {
        assert_eq!(sniff(PNG), Some("image/png"));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a"), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"BM"), None);
    }
}
