"""2.6 hardening of the real-time side: WebSocket framing, slow and vanishing
peers, fan-out, the frame-rate limiter, pulled cameras, the emergency stop,
the stdlib client against a hostile server, and Image encoding edge cases.

Every live test runs against one real server. Timing assertions are one-sided
or generous: a loaded CI runner only ever makes things slower, so nothing here
depends on being fast, only on the hub never doing the wrong thing.
"""
from __future__ import annotations

import base64
import hashlib
import http.server
import json
import math
import os
import socket
import struct
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
CAM_KEY = "wc_test_camera_key_0123456789abcdef"
READ_KEY = "wc_test_reader_key_0123456789abcdef"
ADMIN_KEY = "wc_test_admin_key_0123456789abcdefgh"

APP = '''
import asyncio, time
from webcortex import WebCortex

app = WebCortex("{name}", port={port}, workers=1, request_timeout=3)
app.api_key("CAM_KEY", id="camera", scopes=["devices:ingest"])
app.api_key("READ_KEY", id="reader", scopes=["devices:read"])
app.api_key("ADMIN_KEY", id="admin", scopes=["devices:read", "devices:ingest", "ask", "webcortex:admin", "move"])

app.camera("dock", max_fps=1000, keep=4)
app.camera("burst", max_fps=1e6, keep=64)
app.camera("paced", max_fps=10)
app.camera("flood", max_fps=10)
app.camera("small", max_fps=1000, max_frame_bytes=20000)
app.camera("ipcam", source="http://127.0.0.1:{side_port}/snapshot.png")
app.sensor("temp", max_fps=1000)
app.agent("eye", tools=[], expose_scopes=["ask"])

STARTED, MOVES = [], []

@app.post("/move", actuator=True, scopes=["move"])
def move(n: int, secs: float = 0.5) -> dict:
    STARTED.append(n)
    time.sleep(secs)
    MOVES.append(n)
    return {{"moved": n}}

# Async, so it runs on an event loop and never queues behind a sleeping move.
@app.get("/moves", scopes=["move"])
async def moves() -> dict:
    return {{"started": STARTED, "moves": MOVES}}
'''


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Camera(http.server.BaseHTTPRequestHandler):
    """A slow IP camera that counts how often, and how concurrently, it is asked."""

    lock = threading.Lock()
    hits = 0
    now = 0
    most = 0

    def do_GET(self):
        with Camera.lock:
            Camera.hits += 1
            Camera.now += 1
            Camera.most = max(Camera.most, Camera.now)
        try:
            time.sleep(1.0)
            self.send_response(200)
            self.send_header("content-type", "image/png")
            self.send_header("content-length", str(len(PNG)))
            self.end_headers()
            self.wfile.write(PNG)
        finally:
            with Camera.lock:
                Camera.now -= 1

    def log_message(self, *a):
        pass


def call(base, path, method="GET", body=None, key=ADMIN_KEY, content_type="application/json", timeout=60):
    data = body if isinstance(body, bytes) or body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, method=method, headers={"content-type": content_type})
    if key:
        req.add_header("x-api-key", key)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as res:
            payload = res.read()
            return res.status, json.loads(payload or b"null"), dict(res.headers)
    except urllib.error.HTTPError as e:
        payload = e.read()
        try:
            return e.code, json.loads(payload), dict(e.headers)
        except ValueError:
            return e.code, payload, dict(e.headers)


def _start(side_port):
    """Boot the hub on a free port. Returns (base, proc), or None when the port
    was taken before the server could bind it (another process won the race)."""
    port = _free_port()
    name = f"stress-{os.urandom(4).hex()}"
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-rt-stress-"))
    (workdir / "api.py").write_text(APP.format(port=port, side_port=side_port, name=name))
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
            if "bind" in log_path.read_text():
                return None
            pytest.fail(f"server exited early:\n{log_path.read_text()}")
        try:
            # Ours, not some other process that happens to hold the port.
            with urllib.request.urlopen(base + "/", timeout=1) as res:
                if json.loads(res.read()).get("app") == name:
                    return base, proc
            proc.kill()
            return None
        except urllib.error.HTTPError:
            proc.kill()
            return None
        except Exception:
            time.sleep(0.1)
    proc.kill()
    pytest.fail(f"server never became ready:\n{log_path.read_text()}")


@pytest.fixture(scope="module")
def hub():
    side = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Camera)
    threading.Thread(target=side.serve_forever, daemon=True).start()
    for _ in range(5):
        started = _start(side.server_address[1])
        if started:
            break
    else:
        pytest.fail("could not find a free port for the hub")
    base, proc = started
    yield base
    proc.terminate()
    side.shutdown()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()


def device(base, name):
    return next(d for d in call(base, "/_webcortex/devices")[1]["devices"] if d["name"] == name)


def eventually(predicate, timeout=20.0, what="condition"):
    deadline = time.time() + timeout
    while time.time() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {what}")


# ------------------------------------------------------------ raw sockets


def raw_connect(base, path, key):
    host, port = base.split("//")[1].split(":")
    s = socket.create_connection((host, int(port)), timeout=30)
    k = base64.b64encode(os.urandom(16)).decode()
    s.sendall((f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
               f"Sec-WebSocket-Key: {k}\r\nSec-WebSocket-Version: 13\r\nx-api-key: {key}\r\n\r\n").encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = s.recv(4096)
        assert chunk, buf
        buf += chunk
    head, _, rest = buf.partition(b"\r\n\r\n")
    assert b" 101 " in head.split(b"\r\n")[0], head
    return s, rest


def frame(op, payload, *, fin=True, mask=True, rsv=0):
    h = bytearray([(0x80 if fin else 0) | rsv << 4 | op])
    n, m = len(payload), (0x80 if mask else 0)
    if n < 126:
        h.append(m | n)
    elif n < 65536:
        h.append(m | 126)
        h += struct.pack(">H", n)
    else:
        h.append(m | 127)
        h += struct.pack(">Q", n)
    if mask:
        key = os.urandom(4)
        h += key
        payload = bytes(b ^ key[i % 4] for i, b in enumerate(payload))
    return bytes(h) + payload


def read_frames(s, rest=b"", wait=5.0, until_close=True):
    """Server frames as (opcode, payload) until a close frame, EOF or `wait`."""
    s.settimeout(wait)
    buf, out = rest, []
    while True:
        while len(buf) >= 2:
            n, at = buf[1] & 0x7F, 2
            if n == 126:
                if len(buf) < 4:
                    break
                n, at = struct.unpack(">H", buf[2:4])[0], 4
            elif n == 127:
                if len(buf) < 10:
                    break
                n, at = struct.unpack(">Q", buf[2:10])[0], 10
            if len(buf) < at + n:
                break
            out.append((buf[0] & 0x0F, buf[at:at + n]))
            buf = buf[at + n:]
            if until_close and out[-1][0] == 0x8:
                return out
        try:
            chunk = s.recv(65536)
        except (socket.timeout, ConnectionResetError):
            return out
        if not chunk:
            return out
        buf += chunk


def close_code(frames):
    closes = [p for op, p in frames if op == 0x8]
    return struct.unpack(">H", closes[0][:2])[0] if closes and len(closes[0]) >= 2 else None


# ------------------------------------------------------------ framing


@pytest.mark.parametrize("name,data,code", [
    ("unmasked client frame", frame(0x1, b"{}", mask=False), 1002),
    ("reserved bits", frame(0x1, b"{}", rsv=4), 1002),
    ("invalid UTF-8 text", frame(0x1, b"\xff\xfe"), 1007),
    ("continuation with nothing to continue", frame(0x0, b"{}"), 1002),
    ("new message mid-fragment", frame(0x1, b'{"a"', fin=False) + frame(0x1, b"{}"), 1002),
    ("control frame over 125 bytes", frame(0x9, b"x" * 126), 1002),
    ("fragmented control frame", frame(0x9, b"x", fin=False), 1002),
    ("reserved opcode", frame(0x3, b"x"), 1002),
    ("close with a forbidden code", frame(0x8, struct.pack(">H", 999)), 1002),
    ("clean close is echoed", frame(0x8, struct.pack(">H", 1000)), 1000),
    ("64-bit length with the top bit set", bytes([0x82, 0xFF]) + (1 << 63 | 5).to_bytes(8, "big") + b"mask", 1009),
])
def test_protocol_violations_get_the_rfc_close_code(hub, name, data, code):
    # 2.5.3 dropped the connection with no close frame (the client saw 1006).
    s, rest = raw_connect(hub, "/devices/temp/ws", CAM_KEY)
    s.sendall(data)
    frames = read_frames(s, rest)
    s.close()
    assert frames and frames[0][0] == 0x1 and b'"ready"' in frames[0][1], f"{name}: ready comes first"
    assert close_code(frames) == code, f"{name}: {frames}"


def test_a_ping_inside_a_fragmented_message_is_answered_and_the_message_kept(hub):
    s, rest = raw_connect(hub, "/devices/temp/ws", CAM_KEY)
    s.sendall(frame(0x1, b'{"frag', fin=False) + frame(0x9, b"hi") + frame(0x0, b'mented": 7}'))
    frames = read_frames(s, rest, wait=2, until_close=False)
    assert (0xA, b"hi") in frames
    s.sendall(frame(0x8, struct.pack(">H", 1000)))
    s.close()
    assert call(hub, "/devices/temp/telemetry", key=READ_KEY)[1]["telemetry"] == {"fragmented": 7}


def test_a_socket_frame_over_max_frame_bytes_is_counted_and_closed_with_1009(hub):
    before = device(hub, "small")["frames_rejected"]
    s, rest = raw_connect(hub, "/devices/small/ws", CAM_KEY)
    s.sendall(frame(0x2, b"\x89PNG\r\n\x1a\n" + b"\0" * 70_000))
    frames = read_frames(s, rest)
    s.close()
    assert close_code(frames) == 1009
    # 2.5.3 left frames refused by the socket layer out of frames_rejected.
    eventually(lambda: device(hub, "small")["frames_rejected"] == before + 1, what="the refusal to be counted")


def test_a_device_that_never_reads_its_replies_is_never_stalled(hub):
    # Each of these frames is refused with an error reply that the device
    # never reads. In 2.5.3 the replies filled the socket after ~2000 frames,
    # the hub stopped reading, and the device blocked for good.
    before = device(hub, "dock")
    s, rest = raw_connect(hub, "/devices/dock/ws", CAM_KEY)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
    bad = frame(0x2, b"BM not a supported image")
    s.sendall(bad * 20_000)  # would block (and time out) if the hub stopped reading
    s.sendall(frame(0x2, PNG))
    eventually(lambda: device(hub, "dock")["frames_rejected"] >= before["frames_rejected"] + 20_000,
               timeout=60, what="every refused frame to be read")
    eventually(lambda: device(hub, "dock")["frames_in"] > before["frames_in"], what="the good frame")
    s.close()


# ------------------------------------------------------------ fan-out and peers


def test_every_subscriber_gets_every_frame_in_order(hub):
    n_subs, n_frames = 25, 5
    got = [[] for _ in range(n_subs)]
    ready = threading.Barrier(n_subs + 1, timeout=30)
    frames = [Image.from_array([[(i, 0, 255 - i)]]).data for i in range(n_frames)]

    def listen(i):
        hello = False
        for ev in subscribe(hub, "burst", key=READ_KEY, timeout=30):
            if ev["type"] == "hello" and not hello:
                hello = True
                ready.wait()
            elif ev["type"] == "frame" and ev.get("data") in frames:
                got[i].append((ev["seq"], ev["data"]))
                if len(got[i]) == n_frames:
                    return

    threads = [threading.Thread(target=listen, args=(i,), daemon=True) for i in range(n_subs)]
    for t in threads:
        t.start()
    ready.wait()
    for f in frames:
        assert call(hub, "/devices/burst/frames", "POST", f, key=CAM_KEY, content_type="image/png")[0] == 200
    for t in threads:
        t.join(30)
    for g in got:
        assert [d for _, d in g] == frames, "a subscriber missed or reordered a frame"
        assert [s for s, _ in g] == sorted(s for s, _ in g)


def test_concurrent_pushes_reach_subscribers_in_sequence_order(hub):
    seqs: list = []
    started = threading.Event()

    def listen():
        try:
            for ev in subscribe(hub, "burst", key=READ_KEY, frames="meta", timeout=30):
                if ev["type"] == "hello":
                    started.set()
                elif ev["type"] == "frame":
                    seqs.append(ev["seq"])
        except OSError:  # the hub shut down, or the stream went quiet for 30 s
            pass

    threading.Thread(target=listen, daemon=True).start()
    assert started.wait(15)
    accepted = []

    def push():
        for _ in range(15):
            # Two pushes in the same instant can still trip max_fps (429);
            # what matters is the order of the ones accepted.
            status, body, _ = call(hub, "/devices/burst/frames", "POST", PNG, key=CAM_KEY, content_type="image/png")
            assert status in (200, 429), body
            if status == 200:
                accepted.append(body["seq"])

    pushers = [threading.Thread(target=push) for _ in range(8)]
    for p in pushers:
        p.start()
    for p in pushers:
        p.join(60)
    assert len(accepted) >= 60
    eventually(lambda: set(accepted) <= set(seqs), what="every accepted frame to reach the subscriber")
    mine = [s for s in seqs if s in set(accepted)]
    assert mine == sorted(mine), "frames reached a subscriber out of sequence order"
    assert device(hub, "burst")["latest"]["seq"] == max(accepted)


def test_a_viewer_that_stops_reading_stalls_neither_ingest_nor_other_viewers(hub):
    stuck = []
    for _ in range(3):
        s, _ = raw_connect(hub, "/devices/dock/stream", READ_KEY)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        stuck.append(s)
    seen = []
    hello = threading.Event()

    def watch():
        for ev in subscribe(hub, "dock", key=READ_KEY, frames="meta", timeout=30):
            if ev["type"] == "hello":
                hello.set()
            elif ev["type"] == "frame" and ev["bytes"] == 256 * 1024:
                seen.append(ev["seq"])
                if len(seen) == 40:
                    return

    t = threading.Thread(target=watch, daemon=True)
    t.start()
    assert hello.wait(15)
    big = b"\x89PNG\r\n\x1a\n" + b"\0" * (256 * 1024 - 8)
    for _ in range(40):
        assert call(hub, "/devices/dock/frames", "POST", big, key=CAM_KEY, content_type="image/png",
                    timeout=30)[0] == 200
        time.sleep(0.003)  # within max_fps=1000
    t.join(30)
    assert len(seen) == 40, "the reading viewer was held up by the stuck ones"
    for s in stuck:
        s.close()


def test_a_reconnect_storm_releases_every_subscription(hub):
    base_subs = device(hub, "dock")["subscribers"]
    for i in range(60):
        s, rest = raw_connect(hub, "/devices/dock/stream?frames=meta", READ_KEY)
        if i % 3 == 0:
            s.sendall(frame(0x8, struct.pack(">H", 1000)))  # a polite close
        elif i % 3 == 1:
            s.sendall(frame(0x1, b"x")[:3])  # vanishes mid-frame
        s.close()
    eventually(lambda: device(hub, "dock")["subscribers"] == base_subs, what="subscriptions to be released")
    assert call(hub, "/_webcortex/health")[0] == 200


# ------------------------------------------------------------ max_fps


def test_a_camera_sending_at_exactly_max_fps_keeps_its_frames(hub):
    # 2.5.3 refused any frame less than a full interval after the last, so a
    # 10 fps camera whose timer ran a few ms early lost about half its frames.
    before = device(hub, "paced")["frames_dropped"]
    with DeviceConnection(hub, "paced", key=CAM_KEY) as cam:
        last = time.monotonic()
        for k in range(20):
            # Up to 10 ms early each time; sleeps only ever overrun.
            target = last + 0.1 - (0.01 if k % 2 else 0.0)
            time.sleep(max(0.0, target - time.monotonic()))
            cam.send_frame(PNG)
            last = time.monotonic()
        time.sleep(0.3)
    dropped = device(hub, "paced")["frames_dropped"] - before
    # A stalled runner can bunch two frames together; allow for a couple.
    assert dropped <= 3, f"{dropped} of 20 on-rate frames dropped"


def test_a_flood_is_still_held_to_max_fps(hub):
    before = device(hub, "flood")
    started = time.monotonic()
    with DeviceConnection(hub, "flood", key=CAM_KEY) as cam:
        for _ in range(300):
            cam.send_frame(PNG)
    eventually(lambda: (lambda d: d["frames_in"] + d["frames_dropped"]
                        == before["frames_in"] + before["frames_dropped"] + 300)(device(hub, "flood")),
               what="every flooded frame to be processed")
    elapsed = time.monotonic() - started
    accepted = device(hub, "flood")["frames_in"] - before["frames_in"]
    assert 1 <= accepted <= 10 * elapsed + 2, f"{accepted} frames accepted in {elapsed:.2f}s at max_fps=10"


# ------------------------------------------------------------ pulled cameras


def test_concurrent_snapshots_share_one_pull_of_an_ip_camera(hub):
    before = Camera.hits
    gate = threading.Barrier(30, timeout=30)
    statuses = []

    def snap():
        gate.wait()
        statuses.append(call(hub, "/devices/ipcam/snapshot", key=READ_KEY)[0])

    threads = [threading.Thread(target=snap) for _ in range(30)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(60)
    assert statuses == [200] * 30
    # 2.5.3 fetched once per asker: thirty connections to one camera.
    assert Camera.hits - before <= 5, f"the camera was fetched {Camera.hits - before} times"
    assert Camera.most <= 2


# ------------------------------------------------------------ e-stop


def test_the_emergency_stop_stops_actuator_calls_queued_behind_running_ones(hub):
    # workers=1 gives four handler threads: four moves run, eight queue. In
    # 2.5.3 the queued eight still ran after the stop was pressed.
    results = {}

    def mover(i):
        results[i] = call(hub, "/move", "POST", {"n": 100 + i, "secs": 0.5})[0]

    threads = [threading.Thread(target=mover, args=(i,)) for i in range(12)]
    for t in threads:
        t.start()
    eventually(lambda: len([n for n in call(hub, "/moves")[1]["started"] if n >= 100]) >= 4,
               what="four moves to start")
    try:
        assert call(hub, "/_webcortex/halt", "POST", {"reason": "stress test"})[0] == 200
        for t in threads:
            t.join(60)
        statuses = sorted(results.values())
        assert statuses.count(200) <= 4, statuses
        assert statuses.count(423) >= 8, statuses
        moved = [n for n in call(hub, "/moves")[1]["moves"] if n >= 100]
        assert len(moved) == statuses.count(200)
    finally:
        call(hub, "/_webcortex/release", "POST", {})


def test_an_actuator_call_whose_caller_gave_up_never_starts(hub):
    # request_timeout=3: four long moves fill the handler threads, a fifth
    # queues, its caller is answered 504, and it must not move later.
    longs = [threading.Thread(target=call, args=(hub, "/move", "POST", {"n": 200 + i, "secs": 4.5}))
             for i in range(4)]
    for t in longs:
        t.start()
    eventually(lambda: len([n for n in call(hub, "/moves")[1]["started"] if 200 <= n < 300]) == 4,
               what="four long moves to start")
    status, body, _ = call(hub, "/move", "POST", {"n": 299, "secs": 0})
    assert status == 504, body
    for t in longs:
        t.join(60)
    eventually(lambda: len([n for n in call(hub, "/moves")[1]["moves"] if 200 <= n < 300]) == 4,
               what="the long moves to finish")
    time.sleep(0.5)
    assert 299 not in call(hub, "/moves")[1]["started"], "a move nobody was waiting for still ran"


# ------------------------------------------------------------ auth and agents


def test_access_token_authenticates_upgrades_only(hub):
    assert call(hub, f"/devices/dock/snapshot?access_token={READ_KEY}", key=None)[0] == 401
    ws = hub.replace("http", "ws")
    with WebSocket(f"{ws}/devices/dock/stream?frames=meta&access_token={READ_KEY}") as s:
        assert s.recv_json()["type"] == "hello"
    # A header credential wins over the query: the camera key cannot read.
    with pytest.raises(WebSocketError, match="403"):
        WebSocket(f"{ws}/devices/dock/stream?access_token={READ_KEY}", headers={"x-api-key": CAM_KEY})


def test_an_agent_socket_answers_a_binary_turn_instead_of_ignoring_it(hub):
    with AgentSocket(hub, "/agents/eye", key=ADMIN_KEY, timeout=30) as agent:
        agent.ws.send_binary(b"\x00\x01")
        event = agent.ws.recv_json()
        assert event["type"] == "error" and event["code"] == "WC306"
        assert agent.ask("still there?")["status"] == "completed"


def test_a_device_connection_sees_refusals_and_why_it_was_closed(hub):
    with DeviceConnection(hub, "small", key=CAM_KEY) as cam:
        cam.send_frame(b"BM not an image")
        time.sleep(0.5)
        cam.send_telemetry({"ok": True})
        assert cam.errors >= 1 and cam.last_error["code"] == "WC505"
        cam.send_frame(b"\x89PNG\r\n\x1a\n" + b"\0" * 70_000)
        with pytest.raises(WebSocketError, match="1009|closed"):
            deadline = time.time() + 15
            while time.time() < deadline:
                cam.send_frame(PNG)
                time.sleep(0.05)


# ------------------------------------------------------------ hostile server


def hostile(script=b"", head=None):
    """A server that completes the handshake (or sends `head`) then `script`."""
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    received = []

    def run():
        conn, _ = srv.accept()
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = conn.recv(4096)
            if not chunk:
                return
            buf += chunk
        try:
            if head is not None:
                head(conn)
            else:
                key = [l.split(b":", 1)[1].strip() for l in buf.split(b"\r\n")
                       if l.lower().startswith(b"sec-websocket-key")][0]
                accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
                conn.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                             b"Connection: Upgrade\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n" + script)
            conn.settimeout(3)
            while True:
                chunk = conn.recv(65536)
                if not chunk:
                    break
                received.append(chunk)
        except OSError:
            pass
        finally:
            conn.close()
            srv.close()

    threading.Thread(target=run, daemon=True).start()
    return f"ws://127.0.0.1:{srv.getsockname()[1]}/x", received


def server_frame(op, payload, *, fin=True, mask=False, rsv=0):
    return frame(op, payload, fin=fin, mask=mask, rsv=rsv)


@pytest.mark.parametrize("name,script", [
    ("masked frame from a server", server_frame(0x1, b'{"a": 1}', mask=True)),
    ("reserved bits", server_frame(0x1, b"{}", rsv=2)),
    ("invalid UTF-8 text", server_frame(0x1, b"\xff\xfe")),
    ("continuation with nothing to continue", server_frame(0x0, b"{}")),
    ("new message mid-fragment", server_frame(0x1, b"[", fin=False) + server_frame(0x1, b"{}")),
    ("oversized control frame", server_frame(0x9, b"x" * 126)),
    ("fragmented control frame", server_frame(0x9, b"x", fin=False)),
    ("reserved opcode", server_frame(0x3, b"x")),
    ("text that is not JSON", server_frame(0x1, b"not json")),
])
def test_the_client_refuses_a_hostile_server_with_its_own_error(name, script):
    # 2.5.3 returned garbage for a masked frame, merged interleaved messages,
    # and leaked UnicodeDecodeError / JSONDecodeError for bad text.
    url, received = hostile(script)
    ws = WebSocket(url, timeout=10)
    with pytest.raises(WebSocketError):
        ws.recv_json()
    ws.close()


def test_the_client_bounds_the_handshake_and_parses_a_bad_status_safely():
    def endless(conn):
        conn.sendall(b"HTTP/1.1 101 Switching Protocols\r\n")
        for _ in range(200):
            conn.sendall(b"x-junk: " + b"a" * 4096 + b"\r\n")

    url, _ = hostile(head=endless)
    with pytest.raises(WebSocketError, match="handshake"):
        WebSocket(url, timeout=10)
    url, _ = hostile(head=lambda c: c.sendall(b"HTTP/1.1 abc nonsense\r\n\r\n"))
    with pytest.raises(WebSocketError, match="refused"):
        WebSocket(url, timeout=10)


def test_the_client_completes_the_closing_handshake():
    url, received = hostile(server_frame(0x8, struct.pack(">H", 1001) + b"going away"))
    ws = WebSocket(url, timeout=10)
    with pytest.raises(WebSocketError, match="1001"):
        ws.recv()
    ws.close()  # no second close frame
    eventually(lambda: received, timeout=10, what="the close reply")
    data = b"".join(received)
    assert data[0] == 0x88 and (data[1] & 0x7F) == 2, data[:4].hex()
    masked = data[6:8]
    key = data[2:6]
    assert bytes(b ^ key[i] for i, b in enumerate(masked)) == struct.pack(">H", 1001)
    assert len(data) == 8, "the client sent more than one close frame"


def test_the_client_reassembles_a_message_of_many_fragments():
    chunk = os.urandom(64 * 1024)
    parts = 160  # 10 MB in 64 KB fragments; quadratic buffering made this slow
    script = server_frame(0x2, chunk, fin=False)
    script += b"".join(server_frame(0x0, chunk, fin=False) for _ in range(parts - 2))
    script += server_frame(0x9, b"ping") + server_frame(0x0, chunk)
    url, _ = hostile(script)
    ws = WebSocket(url, timeout=30)
    op, payload = ws.recv()
    assert op == 0x2 and payload == chunk * parts
    ws.close()


# ------------------------------------------------------------ Image


def test_image_refuses_a_zero_sized_array():
    np = pytest.importorskip("numpy")
    for shape in ((0, 4, 3), (4, 0), (0, 0)):
        with pytest.raises(ValueError, match="1x1"):
            Image.from_array(np.zeros(shape, np.uint8))


def test_image_trusts_the_bytes_not_the_label(tmp_path):
    jpeg = b"\xff\xd8\xff\xe0" + b"\0" * 16
    with pytest.raises(ValueError, match="labelled image/png"):
        Image(jpeg, "image/png")
    with pytest.raises(ValueError, match="unsupported"):
        Image(b"junk", "image/png")
    fake = tmp_path / "photo.png"
    fake.write_bytes(b"BM a bitmap with a png name")
    with pytest.raises(ValueError, match="unsupported"):
        Image.from_path(fake)
    real = tmp_path / "photo.dat"
    real.write_bytes(jpeg)
    assert Image.from_path(real).media_type == "image/jpeg"
    assert Image(bytearray(PNG)).media_type == "image/png"
    with pytest.raises(TypeError, match="bytes"):
        Image("\x89PNG")  # type: ignore[arg-type]


def test_image_from_array_edge_cases():
    np = pytest.importorskip("numpy")
    with pytest.raises(ValueError, match=r">> 8"):
        Image.from_array(np.zeros((2, 2), np.uint16))
    with pytest.raises(ValueError, match="uint8"):
        Image.from_array(np.zeros((2, 2, 3), np.float32))
    with pytest.raises(ValueError, match="channels"):
        Image.from_array(np.zeros((2, 2, 5), np.uint8))
    # A single-channel third axis, a transposed (non-contiguous) array, and
    # nested lists holding NumPy scalars all encode.
    assert Image.from_array(np.zeros((2, 3, 1), np.uint8)).media_type == "image/png"
    a = np.arange(12, dtype=np.uint8).reshape(3, 4)
    assert Image.from_array(a.T) == Image.from_array(np.ascontiguousarray(a.T))
    assert Image.from_array([[np.uint8(1), np.uint8(2)]]) == Image.from_array([[1, 2]])
    for bad in ([[1.5, 2]], [[300]], [1, 2, 3], [[(1, 2, 3), (1, 2)]]):
        with pytest.raises(ValueError):
            Image.from_array(bad)


def test_a_large_frame_round_trips_through_the_encoder():
    np = pytest.importorskip("numpy")
    import zlib
    rng = np.random.default_rng(7)
    pixels = rng.integers(0, 8, size=(480, 640, 3), dtype=np.uint8)
    img = Image.from_array(pixels, bgr=True)
    data = img.data
    width, height = struct.unpack(">II", data[16:24])
    assert (width, height) == (640, 480)
    idat = data.index(b"IDAT")
    length = struct.unpack(">I", data[idat - 4:idat])[0]
    raw = zlib.decompress(data[idat + 4: idat + 4 + length])
    rows = [raw[y * (640 * 3 + 1) + 1:(y + 1) * (640 * 3 + 1)] for y in range(480)]
    assert b"".join(rows) == pixels[..., ::-1].tobytes()


# ------------------------------------------------------------ declarations


def test_non_finite_rates_and_periods_are_refused_where_declared():
    app = WebCortex("t")
    for bad in (float("nan"), float("inf"), 0, -1):
        with pytest.raises(ValueError, match="positive"):
            app.camera(f"c{abs(hash(str(bad)))}", max_fps=bad)
    app.camera("cam")
    for bad in (float("nan"), float("inf"), 0.5):
        with pytest.raises(ValueError, match="at least 1"):
            app.watch("w", device="cam", agent="a", input="x", every=bad)


def test_an_enormous_watch_period_is_a_boot_error_not_a_dead_watcher():
    # 1e30 seconds is finite, but overflowed a Duration and panicked the
    # watcher's task at boot, silently.
    app = WebCortex("t")
    app.camera("cam")
    app.agent("a", tools=[])
    app.watch("w", device="cam", agent="a", input="x", every=1e30)
    with pytest.raises(ValueError, match="too large"):
        app.check()
    assert math.isfinite(1e30)
