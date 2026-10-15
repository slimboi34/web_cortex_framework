"""2.5.2: what a second round of black-box testing found in 2.5.1.

One real server with short timeouts. Each test names what used to happen.
"""
from __future__ import annotations

import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from webcortex.client import AgentSocket, WebSocketError

ADMIN_KEY = "wc_test_admin_key_0123456789abcdefgh"

APP = '''
import time
from webcortex import WebCortex

app = WebCortex("hardening", port={port}, request_timeout=2, agent_timeout=2)
app.api_key("ADMIN_KEY", id="admin", scopes=["webcortex:admin"])
app.anonymous_scopes("devices:read", "devices:ingest")

@app.get("/slow", tool=True)
def slow(secs: float = 5.0) -> dict:
    time.sleep(secs)
    return {{"slept": secs}}

app.agent("worker", tools=["get_slow"], description="sleeps when asked")
app.camera("cam")
'''


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture(scope="module")
def server():
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-hardening-"))
    (workdir / "api.py").write_text(APP.format(port=port))
    log_path = workdir / "server.log"
    env = {**os.environ, "WEBCORTEX_LOG": "warn", "WEBCORTEX_FAKE_PROVIDER": "1", "ADMIN_KEY": ADMIN_KEY}
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
    yield base, port, log_path
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
    assert "panicked" not in log_path.read_text()


def call(base, path, method="GET", body=None, key=ADMIN_KEY, timeout=30):
    data = None if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
    req = urllib.request.Request(base + path, data=data, method=method, headers={"content-type": "application/json"})
    if key:
        req.add_header("x-api-key", key)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as res:
            return res.status, json.loads(res.read() or b"null"), dict(res.headers)
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"null"), dict(e.headers)


def runs(base):
    return call(base, "/_webcortex/status")[1]["agents"]


def wait_for_no_runs_in_flight(base, seconds=8.0):
    deadline = time.time() + seconds
    while time.time() < deadline:
        agents = runs(base)
        if not agents["in_flight"]:
            return agents
        time.sleep(0.2)
    return runs(base)


def raw(port, data: bytes, wait: float = 10.0) -> tuple[bytes, float]:
    """Send bytes, read until the server closes; the reply and how long it took."""
    started = time.time()
    with socket.create_connection(("127.0.0.1", port), timeout=wait) as s:
        s.sendall(data)
        out = b""
        try:
            while chunk := s.recv(65536):
                out += chunk
        except (socket.timeout, ConnectionResetError):
            pass
    return out, time.time() - started


# ------------------------------------------------- runs that never finish


def test_a_run_dropped_by_the_request_timeout_leaves_in_flight(server):
    # 2.5.1: the 504 came back, but the run stayed "in flight" for the life of
    # the process, and never appeared in recent or by_agent.
    base, _, _ = server
    status, body, headers = call(base, "/agents/worker", "POST", {"input": 'tool:get_slow {"secs": 4}'})
    assert status == 504 and body["error"]["code"] == "WC203"
    agents = wait_for_no_runs_in_flight(base)
    assert agents["in_flight"] == []
    assert any(r["outcome"] == "cancelled" and r["agent"] == "worker" for r in agents["recent"])
    assert agents["by_agent"]["worker"]["cancelled"] >= 1


def test_a_run_whose_client_disconnects_leaves_in_flight(server):
    base, _, _ = server
    before = runs(base)["by_agent"].get("worker", {}).get("cancelled", 0)
    with pytest.raises(OSError):
        call(base, "/agents/worker", "POST", {"input": 'tool:get_slow {"secs": 1.5}'}, timeout=0.5)
    time.sleep(0.5)
    agents = wait_for_no_runs_in_flight(base)
    assert agents["in_flight"] == []
    assert agents["by_agent"]["worker"]["cancelled"] == before + 1


def test_a_socket_run_past_agent_timeout_is_coded_and_leaves_in_flight(server):
    base, _, _ = server
    with AgentSocket(base, "/agents/worker", key=ADMIN_KEY, timeout=30) as agent:
        with pytest.raises(WebSocketError) as e:
            agent.ask('tool:get_slow {"secs": 4}')
    # The error message's code reaches the Python client, not just its text.
    assert e.value.code == "WC203" and e.value.hint
    assert wait_for_no_runs_in_flight(base)["in_flight"] == []


# ------------------------------------------------------ slow and broken HTTP


def test_a_client_that_never_finishes_its_headers_is_cut_off(server):
    # 2.5.1 set no header read timeout: partial headers (or an idle socket)
    # held a connection open forever.
    _, port, _ = server
    out, took = raw(port, b"GET /_webcortex/health HTTP/1.1\r\nHost: x\r\nX-Slow: ")
    assert took < 8, f"held for {took:.1f}s"
    out, took = raw(port, b"")
    assert took < 8, f"idle connection held for {took:.1f}s"
    # A keep-alive connection idle after its first request is closed too.
    out, took = raw(port, b"GET /_webcortex/health HTTP/1.1\r\nHost: x\r\n\r\n")
    assert out.startswith(b"HTTP/1.1 200") and took < 8, (out[:40], took)


def test_a_body_that_never_arrives_is_a_408(server):
    _, port, _ = server
    out, took = raw(port, b"POST /agents/worker HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n{")
    assert out.startswith(b"HTTP/1.1 408"), out[:200]
    assert b"WC216" in out
    assert took < 8


def test_a_malformed_chunked_body_is_a_400_not_a_413(server):
    _, port, _ = server
    out, _ = raw(port, b"POST /agents/worker HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n"
                       b"Connection: close\r\n\r\nZZZ\r\nabc\r\n0\r\n\r\n")
    assert out.startswith(b"HTTP/1.1 400"), out[:200]
    assert b"WC205" in out


# ---------------------------------------------------------------- devices


def test_an_undeclared_device_is_wc501(server):
    base, _, _ = server
    status, body, headers = call(base, "/devices/nope/snapshot")
    assert status == 404
    assert body["error"]["code"] == "WC501" and headers["x-webcortex-error"] == "WC501"
    assert "nope" in body["error"]["message"]
    # A declared device's missing route, and any other unknown path, stay WC201.
    assert call(base, "/devices/cam/nothing")[1]["error"]["code"] == "WC201"
    assert call(base, "/elsewhere/cam/snapshot")[1]["error"]["code"] == "WC201"


def test_an_oversized_socket_frame_is_explained_in_the_log(server):
    base, port, log_path = server
    from webcortex.client import DeviceConnection

    with DeviceConnection(base, "cam") as d:
        try:
            d.ws.send_binary(b"\xff\xd8\xff" + b"\0" * (6 * 1024 * 1024))
            d.ws.recv()
        except (OSError, WebSocketError):
            pass
    deadline = time.time() + 5
    while time.time() < deadline and "message too large" not in log_path.read_text():
        time.sleep(0.1)
    assert "message too large" in log_path.read_text()
    status = call(base, "/_webcortex/status")[1]
    cam = next(x for x in status["devices"]["devices"] if x["name"] == "cam")
    assert "frames_rejected" in cam


def test_rejected_socket_frames_show_in_status(server):
    base, _, _ = server
    from webcortex.client import DeviceConnection

    with DeviceConnection(base, "cam") as d:
        d.send_frame(b"not an image")
        assert d.ws.recv_json()["code"] == "WC505"
    status = call(base, "/_webcortex/status")[1]
    cam = next(x for x in status["devices"]["devices"] if x["name"] == "cam")
    assert cam["frames_rejected"] >= 1 and cam["frames_in"] == 0


def test_no_socket_is_left_behind(server):
    # Many sockets opened and dropped, some without a close frame: the
    # subscriber count returns to zero.
    base, _, _ = server
    from webcortex.client import WebSocket

    url = base.replace("http", "ws") + "/devices/cam/stream?frames=meta"
    socks = [WebSocket(url) for _ in range(50)]
    for i, ws in enumerate(socks):
        if i % 2:
            ws.close()
        else:
            ws._sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            ws._sock.close()
    deadline = time.time() + 5
    while time.time() < deadline:
        cam = next(x for x in call(base, "/_webcortex/status")[1]["devices"]["devices"] if x["name"] == "cam")
        if cam["subscribers"] == 0:
            break
        time.sleep(0.1)
    assert cam["subscribers"] == 0
