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
| `max_fps` | `30` | Ingest ceiling. Faster frames are dropped (WebSocket) or refused with 429 (HTTP) |
| `max_frame_bytes` | 5 MB | Frames larger than this are refused |
| `ingest_scopes` | `["devices:ingest"]` | Required to send frames and telemetry |
| `read_scopes` | `["devices:read"]` | Required to read anything back |
| `tool` | `True` | Whether agents get the device's tools |

Give each device its own key with only the ingest scope. A stolen camera key
can then send frames but cannot read anything back.

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

**A microcontroller** such as an ESP32-CAM sends an HTTP POST:

```bash
curl -X POST https://hub.example.com/devices/dock/frames \
     -H "x-api-key: $DEVICE_KEY" -H "content-type: image/jpeg" --data-binary @frame.jpg
```

**An IP camera:** set `source=` to its snapshot URL. Nothing is fetched until a
frame is asked for. RTSP streams need a sidecar such as ffmpeg or go2rtc, which
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
    scopes=["devices:read"],           # the authority the agent runs with
    max_runs_per_hour=120,             # the spend ceiling
    webhook="https://ops.example.com/hooks/dock",
)
```

Every `every` seconds the watcher takes the newest frame (pulling it, for an IP
camera) and the newest telemetry, and **skips the tick if nothing changed**.
Changed means different bytes, not a new sequence number. Otherwise it runs the
agent with the frame attached and the telemetry in the input, and publishes the
answer as an **insight**:

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

## Authentication for sockets

A WebSocket upgrade is authenticated exactly like an HTTP request, and a socket
is held to the scopes of the route it is mounted on. Browsers cannot set
headers on a WebSocket, so an upgrade request may pass its key as
`?access_token=` instead. This is accepted only on upgrades and only when no
header credential is present. The request log records the path, never the
query, but a proxy in front of the hub may log full URLs: give browsers keys
with the narrowest scope that works.

The `/connect` and `/view` pages are public. They are static HTML containing no
data, and everything they show needs a key.
