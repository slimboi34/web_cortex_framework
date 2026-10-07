"""2.3: the device hub. Cameras and sensors push (HTTP or WebSocket) or are
pulled; agents see their frames as tools; subscribers and webhooks receive
frames, telemetry and the insights watcher agents publish; agents run over a
WebSocket, step by step.

One real server, a fake IP camera and a webhook receiver; the fake provider
stands in for the model.
"""
from __future__ import annotations

import base64
import http.server
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from webcortex import Image, WebCortex
from webcortex.client import AgentSocket, DeviceConnection, WebSocket, WebSocketError, subscribe

PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="
)
JPEG = b"\xff\xd8\xff\xe0" + b"\0" * 64

CAM_KEY = "wc_test_camera_key_0123456789abcdef"
READ_KEY = "wc_test_reader_key_0123456789abcdef"
ADMIN_KEY = "wc_test_admin_key_0123456789abcdefgh"


# ------------------------------------------------------------ declarations


def test_a_camera_declares_its_routes_tools_and_sockets():
    app = WebCortex("t")
    app.camera("dock", description="the loading dock")
    routes = {(r["method"], r["path"]): r for r in app.manifest()["routes"]}
    for method, path in [("POST", "/devices/dock/frames"), ("GET", "/devices/dock/snapshot"),
                         ("GET", "/devices/dock/latest"), ("GET", "/devices/dock/ws"),
                         ("GET", "/devices/dock/stream"), ("GET", "/devices/dock/connect"),
                         ("GET", "/devices/dock/view"), ("POST", "/devices/dock/telemetry"),
                         ("GET", "/devices/dock/telemetry"), ("GET", "/devices/dock/insights")]:
        assert (method, path) in routes, (method, path)
    assert routes[("POST", "/devices/dock/frames")]["scopes"] == ["devices:ingest"]
    assert routes[("GET", "/devices/dock/snapshot")]["tool"]["name"] == "dock_snapshot"
    assert app.manifest()["devices"][0]["kind"] == "camera"


def test_a_sensor_has_no_frame_routes():
    app = WebCortex("t")
    app.sensor("temp")
    paths = {r["path"] for r in app.manifest()["routes"]}
    assert "/devices/temp/telemetry" in paths and "/devices/temp/frames" not in paths


def test_declarations_are_checked():
    app = WebCortex("t")
    with pytest.raises(ValueError, match="identifier"):
        app.camera("front-door")
    with pytest.raises(ValueError, match="http"):
        app.camera("cam", source="rtsp://cam/stream")
    app.camera("cam")
    with pytest.raises(ValueError, match="twice"):
        app.camera("cam")
    with pytest.raises(ValueError, match="not declared"):
        app.watch("w", device="nope", agent="a", input="x")
    with pytest.raises(ValueError, match="at least 1"):
        app.watch("w", device="cam", agent="a", input="x", every=0.1)


def test_a_watcher_naming_an_unknown_agent_fails_at_boot():
    app = WebCortex("t")
    app.camera("cam")
    app.watch("w", device="cam", agent="ghost", input="x")
    with pytest.raises(ValueError, match="undeclared agent"):
        app.check()


# -------------------------------------------------------------------- live

APP = '''
from webcortex import WebCortex

app = WebCortex("hub", port={port})
app.api_key("CAM_KEY", id="camera", scopes=["devices:ingest"])
app.api_key("READ_KEY", id="reader", scopes=["devices:read"])
app.api_key("ADMIN_KEY", id="admin", scopes=["devices:read", "devices:ingest", "ask", "webcortex:admin"])

app.camera("dock", description="the loading dock", max_fps=1000)
app.camera("slow", max_fps=1)
app.camera("ipcam", source="http://127.0.0.1:{side_port}/snapshot.png")
app.sensor("temp", max_fps=1000)

app.agent("eye", tools=["dock_snapshot", "temp_telemetry"], scopes=["devices:read"], expose_scopes=["ask"])
app.watch("dock_watch", device="dock", agent="eye", input="Anything unusual?", every=1,
          scopes=["devices:read"], webhook="http://127.0.0.1:{side_port}/hook")
'''


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Side(http.server.BaseHTTPRequestHandler):
    """A fake IP camera and a webhook receiver."""

    hooks: list = []

    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type", "image/png")
        self.end_headers()
        self.wfile.write(PNG)

    def do_POST(self):
        body = self.rfile.read(int(self.headers["content-length"]))
        Side.hooks.append(json.loads(body))
        self.send_response(204)
        self.end_headers()

    def log_message(self, *a):
        pass


def call(base, path, method="GET", body=None, key=ADMIN_KEY, raw=False, content_type="application/json"):
    data = body if isinstance(body, bytes) or body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, method=method,
                                 headers={"content-type": content_type})
    if key:
        req.add_header("x-api-key", key)
    try:
        with urllib.request.urlopen(req, timeout=30) as res:
            payload = res.read()
            return res.status, (payload if raw else json.loads(payload or b"null")), dict(res.headers)
    except urllib.error.HTTPError as e:
        payload = e.read()
        try:
            return e.code, json.loads(payload), dict(e.headers)
        except ValueError:
            return e.code, payload, dict(e.headers)


@pytest.fixture(scope="module")
def hub():
    side_port, port = _free_port(), _free_port()
    side = http.server.ThreadingHTTPServer(("127.0.0.1", side_port), Side)
    threading.Thread(target=side.serve_forever, daemon=True).start()

    workdir = Path(tempfile.mkdtemp(prefix="webcortex-hub-"))
    (workdir / "api.py").write_text(APP.format(port=port, side_port=side_port))
    log_path = workdir / "server.log"
    env = {**os.environ, "WEBCORTEX_LOG": "warn", "WEBCORTEX_FAKE_PROVIDER": "1",
           "CAM_KEY": CAM_KEY, "READ_KEY": READ_KEY, "ADMIN_KEY": ADMIN_KEY}
    env.pop("ANTHROPIC_API_KEY", None)
    with log_path.open("w") as log:
        proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", "run", "api.py"],
                                cwd=workdir, stdout=log, stderr=subprocess.STDOUT, env=env)
    base = f"http://127.0.0.1:{port}"
    deadline = time.time() + 45
    while time.time() < deadline:
        if proc.poll() is not None:
            pytest.fail(f"server exited early:\n{log_path.read_text()}")
        try:
            with urllib.request.urlopen(base + "/_webcortex/health", timeout=1):
                break
        except Exception:
            time.sleep(0.1)
    else:
        proc.kill()
        pytest.fail(f"server never became ready:\n{log_path.read_text()}")
    yield base
    proc.terminate()
    side.shutdown()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()


def test_a_frame_pushed_over_http_is_what_an_agent_and_a_browser_see(hub):
    status, out, _ = call(hub, "/devices/dock/frames", "POST", PNG, key=CAM_KEY, content_type="image/png")
    assert status == 200 and out["seq"] >= 1, out
    status, snap, _ = call(hub, "/devices/dock/snapshot", key=READ_KEY)
    assert status == 200
    assert base64.b64decode(snap["frame"]["$image"]["data"]) == PNG
    assert snap["age_ms"] >= 0
    status, body, headers = call(hub, "/devices/dock/latest", key=READ_KEY, raw=True)
    assert status == 200 and body == PNG and headers["content-type"] == "image/png"
    status, listing, _ = call(hub, "/_webcortex/devices")
    dock = next(d for d in listing["devices"] if d["name"] == "dock")
    assert dock["frames_in"] >= 1 and dock["latest"]["media_type"] == "image/png"


def test_ingest_and_read_are_separately_scoped(hub):
    assert call(hub, "/devices/dock/frames", "POST", PNG, key=READ_KEY)[0] == 403
    assert call(hub, "/devices/dock/snapshot", key=CAM_KEY)[0] == 403
    assert call(hub, "/devices/dock/snapshot", key=None)[0] == 401
    assert call(hub, "/devices/dock/frames", "POST", b"not an image", key=CAM_KEY)[0] == 422


def test_a_device_that_floods_is_throttled(hub):
    assert call(hub, "/devices/slow/frames", "POST", JPEG, key=CAM_KEY)[0] == 200
    assert call(hub, "/devices/slow/frames", "POST", JPEG, key=CAM_KEY)[0] == 429


def test_an_ip_camera_is_pulled_when_asked(hub):
    status, snap, _ = call(hub, "/devices/ipcam/snapshot", key=READ_KEY)
    assert status == 200, snap
    assert base64.b64decode(snap["frame"]["$image"]["data"]) == PNG


def test_a_sensor_reports_telemetry(hub):
    assert call(hub, "/devices/temp/telemetry", "POST", {"temp_c": 21.5}, key=CAM_KEY)[0] == 200
    status, out, _ = call(hub, "/devices/temp/telemetry", key=READ_KEY)
    assert out["telemetry"] == {"temp_c": 21.5}


def test_websocket_ingest_reaches_websocket_subscribers(hub):
    events: list = []
    ready = threading.Event()

    def listen():
        for event in subscribe(hub, "dock", key=READ_KEY, timeout=15):
            events.append(event)
            if event["type"] == "hello":
                ready.set()
            if event["type"] == "telemetry" and event["data"].get("from") == "ws":
                return

    t = threading.Thread(target=listen, daemon=True)
    t.start()
    assert ready.wait(10), "subscriber never got hello"

    frame = Image.from_array([[(200, 10, 10)] * 4] * 3)
    with DeviceConnection(hub, "dock", key=CAM_KEY) as cam:
        assert cam.ready["type"] == "ready"
        cam.send_frame(frame)
        cam.send_telemetry({"from": "ws", "lux": 300})
        t.join(10)

    frames = [e for e in events if e["type"] == "frame" and e.get("data") == frame.data]
    assert frames, [e["type"] for e in events]
    assert frames[0]["media_type"] == "image/png"
    assert any(e["type"] == "telemetry" and e["data"]["lux"] == 300 for e in events)


def test_a_browser_can_authenticate_a_socket_with_access_token(hub):
    url = hub.replace("http", "ws") + f"/devices/dock/stream?frames=meta&access_token={READ_KEY}"
    with WebSocket(url) as ws:
        assert ws.recv_json()["type"] == "hello"
    with pytest.raises(WebSocketError, match="401|403"):
        WebSocket(hub.replace("http", "ws") + "/devices/dock/stream")
    with pytest.raises(WebSocketError, match="403"):
        WebSocket(hub.replace("http", "ws") + f"/devices/dock/ws?access_token={READ_KEY}")


def test_a_hostile_page_cannot_open_a_socket_with_the_operators_key(hub):
    """Cross-site WebSocket hijacking: WebSockets ignore CORS, so the Origin is checked."""
    url = hub.replace("http", "ws") + f"/devices/dock/stream?frames=meta&access_token={READ_KEY}"
    with pytest.raises(WebSocketError, match="403"):
        WebSocket(url, headers={"origin": "https://evil.test"})
    with pytest.raises(WebSocketError, match="403"):
        WebSocket(url, headers={"origin": "https://evil.test", "sec-fetch-site": "cross-site"})
    # The hub's own /view page is same-origin, and a device sends no Origin.
    port = hub.rsplit(":", 1)[1]
    for headers in ({"origin": f"http://127.0.0.1:{port}"}, {}):
        with WebSocket(url, headers=headers) as ws:
            assert ws.recv_json()["type"] == "hello"


def test_the_client_refuses_header_injection_and_oversized_messages():
    with pytest.raises(ValueError, match="line break"):
        WebSocket("ws://127.0.0.1:9/x", headers={"x-api-key": "k\r\nx-evil: 1"})

    # A server that completes the handshake and then announces a 2**62-byte frame.
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)

    def serve():
        conn, _ = srv.accept()
        head = b""
        while b"\r\n\r\n" not in head:
            head += conn.recv(4096)
        key = [l.split(b":", 1)[1].strip() for l in head.split(b"\r\n")
               if l.lower().startswith(b"sec-websocket-key")][0]
        import hashlib
        accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
        conn.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                     b"Connection: Upgrade\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
        conn.sendall(bytes([0x82, 127]) + (1 << 62).to_bytes(8, "big"))
        time.sleep(1)
        conn.close()

    threading.Thread(target=serve, daemon=True).start()
    ws = WebSocket(f"ws://127.0.0.1:{srv.getsockname()[1]}/x", timeout=5)
    with pytest.raises(WebSocketError, match="max_message"):
        ws.recv()
    srv.close()


def test_sockets_only_where_declared(hub):
    with pytest.raises(WebSocketError, match="400"):
        WebSocket(hub.replace("http", "ws") + "/devices/dock/snapshot", headers={"x-api-key": READ_KEY})
    assert call(hub, "/devices/dock/stream", key=READ_KEY)[0] == 426


def test_an_agent_runs_over_a_websocket_step_by_step(hub):
    call(hub, "/devices/dock/frames", "POST", PNG, key=CAM_KEY)
    steps = []
    with AgentSocket(hub, "/agents/eye", key=ADMIN_KEY) as agent:
        assert agent.ready == {"type": "ready", "agent": "eye"}
        result = agent.ask("tool:dock_snapshot {}", on_step=steps.append)
        assert result["status"] == "completed"
        assert result["output"].endswith("[saw 1 image(s)]"), result["output"]
        assert [s["step"]["kind"] for s in steps] == ["model", "tool_call", "model"]
        # The socket stays open for the next turn, and takes images.
        again = agent.ask("what is this?", images=[Image.from_bytes(PNG)])
        assert again["output"] == "eye echoes: what is this? [saw 1 image(s)]"
        agent.ws.send_json({"nope": 1})
        assert "input" in agent.ws.recv_json()["message"]
    with pytest.raises(WebSocketError, match="403"):
        AgentSocket(hub, "/agents/eye", key=READ_KEY)


def test_a_watcher_sees_new_frames_and_publishes_insights_everywhere(hub):
    seq = call(hub, "/devices/dock/frames", "POST", Image.from_array([[1, 2, 3]]).data, key=CAM_KEY)[1]["seq"]
    deadline = time.time() + 15
    latest = None
    while time.time() < deadline:
        insights = call(hub, "/devices/dock/insights", key=READ_KEY)[1]["insights"]
        latest = next((i for i in insights if i["seq"] == seq), None)
        if latest and any(h["seq"] == seq for h in Side.hooks):
            break
        time.sleep(0.3)
    assert latest, "the watcher never published about the new frame"
    assert latest["watcher"] == "dock_watch" and latest["agent"] == "eye"
    assert latest["output"].startswith("eye echoes: Anything unusual?")
    assert latest["output"].endswith("[saw 1 image(s)]"), "the agent was shown the frame"
    assert any(h["seq"] == seq and h["watcher"] == "dock_watch" for h in Side.hooks), "the webhook received it"


def test_an_unchanged_frame_does_not_rerun_the_watcher(hub):
    before = len(call(hub, "/devices/dock/insights", key=READ_KEY)[1]["insights"])
    time.sleep(2.5)
    after = len(call(hub, "/devices/dock/insights", key=READ_KEY)[1]["insights"])
    assert after == before, "nothing new arrived, so nothing should have been spent"
