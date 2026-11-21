# The device hub

WebCortex 2.3 sits between the physical world and AI agents. Cameras and
sensors stream in. Agents see what they send. What the agents conclude streams
out to dashboards, services and other systems. All of this runs in the Rust
core: frames, telemetry, fan-out and the watcher loop never touch Python.

```
 phone / laptop / Pi / IP camera ──frames──▶ ┌──────────────┐ ──tool──▶ agent sees the frame
 sensor / PLC / IMU ─────telemetry─────────▶ │  device hub  │ ◀─insight──┘
                                             │    (Rust)    │
 dashboards, services   ◀── WebSocket ────── │  ring buffer │
 any other system       ◀── webhook POST ─── │  broadcast   │
                                             └──────────────┘
```

```bash
webcortex new site -t hub && cd site
export WEBCORTEX_API_KEY=$(webcortex keygen)
webcortex dev
open http://127.0.0.1:8000/devices/dock/connect    # this laptop's camera becomes "dock"
open http://127.0.0.1:8000/devices/dock/view       # the live stream, and what the agent says
```

## Declaring devices

```python
app.camera("dock", description="the loading dock", max_fps=10)
app.camera("lobby", source="http://192.168.1.40/snapshot.jpg", bearer_env="LOBBY_TOKEN")
app.sensor("dock_env", description="temperature and humidity")
```

| Option | Default | |
|---|---|---|
| `description` | — | What the agent is told the device is |
| `source` | `None` | An HTTP snapshot URL. The camera is pulled whenever a frame is asked for |
| `headers`, `bearer_env` | — | Credentials for a pulled camera |
| `keep` | `8` | Frames held in the ring buffer |
| `max_fps` | `30` | Ingest ceiling. Faster frames are dropped (WebSocket) or refused with 429 (HTTP). A frame may arrive up to half an interval early, so a camera sending at exactly `max_fps` with ordinary jitter keeps every frame; the long-run rate never exceeds it |
| `max_frame_bytes` | 5 MB | Frames larger than this are refused (422 over HTTP; over the socket, a message past it closes the connection with 1009 and a warning in the log) |
| `ingest_scopes` | `["devices:ingest"]` | Required to send frames and telemetry. The default is shared by every device that keeps it |
| `read_scopes` | `["devices:read"]` | Required to read anything back |
| `tool` | `True` | Whether agents get the device's tools |
| `prefix` | `/devices` | Where the device's routes are mounted |

### One key per device

Give each device its own key with only an ingest scope. A stolen camera key can
then send frames but cannot read anything back.

That alone does not tie the key to *its* camera. Ingest is authorised by scope,
and every device that keeps the default `ingest_scopes` accepts the same
`devices:ingest`, so a key stolen from the cheapest camera could also send
forged frames and telemetry to every other device, and to the watchers and
agents that act on them. To bind each key to one device, give each device a
scope of its own:

```python
app.api_key("DOCK_CAMERA_KEY",  id="dock-camera",  scopes=["devices:ingest:dock"])
app.api_key("VAULT_CAMERA_KEY", id="vault-camera", scopes=["devices:ingest:vault"])

app.camera("dock",  ingest_scopes=["devices:ingest:dock"])
app.camera("vault", ingest_scopes=["devices:ingest:vault"])
```

Now the dock's key gets `403` from `/devices/vault/frames`. The default is kept
for compatibility with existing device keys, so a shared scope is still what you
get unless you ask for this.

Each device gets these routes (prefix `/devices/<name>`):

| Route | |
|---|---|
| `POST …/frames` | One encoded image (PNG, JPEG, GIF or WebP; the type is sniffed) |
| `POST …/telemetry` | One JSON reading, at most 64 KB |
| `GET …/ws` | **WebSocket in.** Binary messages are frames, text messages are telemetry |
| `GET …/stream` | **WebSocket out.** Every frame, telemetry reading and insight as it happens |
| `GET …/snapshot` | JSON with the newest frame as an image marker, its age, and the latest telemetry. **Agent tool `<name>_snapshot`** |
| `GET …/latest` | The newest frame's bytes, for an `<img>` |
| `GET …/telemetry` | The newest reading. **Agent tool `<name>_telemetry`** |
| `GET …/insights` | Recent insights from watchers. **Agent tool `<name>_insights`** |
| `GET …/connect` | A page that turns a browser's camera into this device |
| `GET …/view` | A live monitor |

`GET /_webcortex/devices` lists every device with frames in, frames dropped,
subscribers and the newest frame.

## Connecting a camera

**A phone or laptop:** open `/devices/<name>/connect`, paste a key with the
ingest scope, and press start. Frames go over the WebSocket as JPEG at the rate
and width you choose. Browsers only allow camera access on `https://` or
`localhost`, so to use a phone, put the hub behind TLS: a reverse proxy, or a
tunnel such as `cloudflared` or `ngrok`.

**A Raspberry Pi, a Jetson, or any machine with Python** uses the standard
library client:

```python
import cv2
from webcortex import Image
from webcortex.client import DeviceConnection

cap = cv2.VideoCapture(0)
with DeviceConnection("https://hub.example.com", "dock", key=DEVICE_KEY) as cam:
    while True:
        ok, frame = cap.read()
        if ok:
            cam.send_frame(cv2.imencode(".jpg", frame)[1].tobytes())   # or Image.from_array(frame, bgr=True)
        cam.send_telemetry({"cpu_temp_c": read_cpu_temp()})
```

The hub answers a refused frame or reading with an error message. Each send
first reads any that have arrived, without blocking: `cam.last_error` is the
newest and `cam.errors` counts them. If the hub closed the socket (a frame over
`max_frame_bytes` is closed with 1009), the next send raises `WebSocketError`
saying why. A device that never reads is never stalled by replies it ignores:
the hub drops them rather than wait.

**A microcontroller** such as an ESP32-CAM sends an HTTP POST:

```bash
curl -X POST https://hub.example.com/devices/dock/frames \
     -H "x-api-key: $DEVICE_KEY" -H "content-type: image/jpeg" --data-binary @frame.jpg
```

**An IP camera:** set `source=` to its snapshot URL. Nothing is fetched until a
frame is asked for, and everyone who asks while a fetch is in flight shares it
(its frame, or its error), so forty agents and viewers looking at once cost the
camera one request, not forty. RTSP streams need a sidecar such as ffmpeg or go2rtc, which
exposes a snapshot URL or pushes frames.

## Agents that look

The `<name>_snapshot` tool returns the newest frame, and an agent that calls it
receives the image itself (see [Vision and robotics](vision-and-robotics.md)).
Only the last `max_images` frames stay in context.

```python
app.agent("inspector", tools=["dock_snapshot", "dock_env_telemetry"], scopes=["devices:read"], max_images=2)
```

## Watchers: the hub's middle

```python
app.watch(
    "dock_watch",
    device="dock",
    agent="inspector",
    input="Anything unsafe or unusual at the dock right now?",
    every=15,                          # seconds
    only_on_change=True,               # the default: skip a tick when nothing changed
    scopes=["devices:read"],           # the authority the agent runs with
    max_runs_per_hour=120,             # the spend ceiling
    webhook="https://ops.example.com/hooks/dock",
)
```

Every `every` seconds the watcher takes the newest frame (pulling it, for an IP
camera) and the newest telemetry, and **skips the tick if nothing changed**.
Changed means different bytes, not a new sequence number. Pass
`only_on_change=False` to run the agent on every tick regardless, for a
periodic report on a scene that rarely changes; the hourly ceiling still
applies. Otherwise it runs the agent with the frame attached and the telemetry
in the input, and publishes the answer as an **insight**:

```json
{"type": "insight", "watcher": "dock_watch", "device": "dock", "agent": "inspector",
 "seq": 412, "status": "completed", "output": "A pallet is blocking the east exit.",
 "error": null, "run_id": "…", "tokens": 1830, "at_ms": 1790866914641}
```

The insight goes to every `/stream` subscriber, to `dock_insights`, and as a
JSON POST to the webhook (with `webhook_bearer_env` for a bearer token). It is
also written to the audit log. A watcher has no caller, so its agent runs with
exactly `scopes`, intersected with the agent's own.

!!! warning "A watcher spends tokens on a timer"
    The worst case per hour is `max_runs_per_hour × the agent's token_budget`.
    Set both. Every run is in `GET /_webcortex/usage`.

## Subscribing

From a browser, the `/view` page. From Python:

```python
from webcortex.client import subscribe

for event in subscribe("https://hub.example.com", "dock", key=READER_KEY, frames="meta"):
    if event["type"] == "insight":
        notify(event["output"])
```

The `/stream` protocol:

| Message | |
|---|---|
| `{"type": "hello", "device": {…}, "insights": […]}` | On connect, with recent insights |
| `{"type": "frame", "seq", "media_type", "bytes", "at_ms"}`, then a binary message | A frame. Add `?frames=meta` to receive the header only |
| `{"type": "telemetry", "data": {…}}` | A reading |
| `{"type": "insight", …}` | A watcher's conclusion |
| `{"type": "lagged", "skipped": n}` | A slow subscriber missed `n` events. Frames are dropped for it rather than buffered |

A quiet stream is pinged every 30 seconds, and a subscriber that has not read
a message within 30 seconds of it being sent is disconnected, so a viewer that
vanished without closing does not hold its subscription forever. Every socket
answers a protocol violation with the RFC 6455 close code (1002, 1007 for text
that is not UTF-8, 1009 for a message that is too large) and answers a close
with a close.

## Agents over a WebSocket

Every agent route also accepts a WebSocket. Send a turn and receive each step
as it happens, then the result. The socket stays open for the next turn.

```python
from webcortex.client import AgentSocket

with AgentSocket("https://hub.example.com", "/agents/inspector", key=KEY) as agent:
    result = agent.ask("Check the dock", on_step=lambda e: print(e["step"]["kind"], e["step"].get("tool")))
```

| You send | You receive |
|---|---|
| `{"input": "…", "images"?: […], "session_id"?: "…", "reset"?: false}` | `{"type": "step", "run_id", "agent", "step": {…}}` for each step, then `{"type": "result", "result": {…}}` |

Each turn is held to the app's rate limit, exactly as a separate HTTP request
would be (since 2.9): a turn over the limit is not run, and the socket answers
with an error carrying `WC401` and `retry_after`, then closes if the client
keeps going.

## Authentication for sockets

A WebSocket upgrade is authenticated exactly like an HTTP request, and a socket
is held to the scopes of the route it is mounted on. Browsers cannot set
headers on a WebSocket, so an upgrade request may pass its credential as
`?access_token=` instead: an API key, or (since 2.9, when `app.jwt(...)` is
configured) a JWT, recognised by its three dot-separated segments. This is
accepted only on upgrades and only when no header credential is present. The
request log records the path, never the query, but a proxy in front of the hub
may log full URLs: give browsers keys with the narrowest scope that works.

A socket opened with a JWT holds that token's authority only until it expires
(plus `leeway_secs`). After that an agent socket refuses the next turn with
`WC603` and closes with 1008, and a stream or ingest socket is closed. Open a
new socket with a fresh token.

A browser opening a socket from another origin is refused (`403`) unless that
origin is listed in `app.cors(...)`, so a page elsewhere cannot open the
stream with a key the operator's browser has saved. Devices and
`webcortex.client` send no `Origin` and are unaffected.

The `/connect` and `/view` pages are public. They are static HTML containing no
data, and everything they show needs a key.
