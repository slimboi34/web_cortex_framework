"""Regression tests for the 2.9.0 core-runtime fixes.

Each test names the defect it guards against and what 2.8.0 did. Servers run
on ephemeral ports under the deterministic FakeProvider
(`WEBCORTEX_FAKE_PROVIDER=1`); nothing here calls a real model.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
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
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from webcortex import WebCortex
from webcortex.client import AgentSocket, WebSocket, WebSocketError

ADMIN_KEY = "core-admin-key-0123456789abcdef0123"
USER_KEY = "core-user-key-0123456789abcdef01234"
JWT_SECRET = "core-jwt-secret-0123456789-abcdefghijklmnop"


# --------------------------------------------------------------------------- helpers


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _start(source: str, env_extra: dict | None = None) -> tuple[str, subprocess.Popen, Path]:
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-core-"))
    (workdir / "api.py").write_text(source.replace("__PORT__", str(port)))
    log_path = workdir / "server.log"
    env = {**os.environ, "WEBCORTEX_LOG": "warn", "WEBCORTEX_FAKE_PROVIDER": "1",
           "CORE_ADMIN": ADMIN_KEY, "CORE_USER": USER_KEY, "CORE_JWT": JWT_SECRET,
           "MARKER": str(workdir / "marker.log"), **(env_extra or {})}
    for k in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY"):
        env.pop(k, None)
    with log_path.open("w") as log:
        proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", "run", "api.py"],
                                cwd=workdir, stdout=log, stderr=subprocess.STDOUT, env=env)
    base = f"http://127.0.0.1:{port}"
    deadline = time.time() + 60
    while time.time() < deadline:
        if proc.poll() is not None:
            pytest.fail(f"server exited early:\n{log_path.read_text()}")
        try:
            with urllib.request.urlopen(base + "/_webcortex/health", timeout=1):
                return base, proc, workdir
        except Exception:
            time.sleep(0.1)
    proc.kill()
    pytest.fail(f"server never became ready:\n{log_path.read_text()}")


def _stop(proc: subprocess.Popen) -> None:
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()


def _request(base, path, method="GET", body=None, headers=None, timeout=30):
    data = json.dumps(body).encode() if body is not None else None
    h = {"content-type": "application/json"} if data is not None else {}
    h.update(headers or {})
    req = urllib.request.Request(base + path, data=data, method=method, headers=h)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            payload = r.read()
            return r.status, (json.loads(payload) if payload else None), dict(r.headers)
    except urllib.error.HTTPError as e:
        payload = e.read()
        try:
            parsed = json.loads(payload) if payload else None
        except ValueError:
            parsed = payload
        return e.code, parsed, dict(e.headers)


def _key(k: str) -> dict:
    return {"x-api-key": k}


def _b64u(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def _jwt(claims: dict, secret: str = JWT_SECRET) -> str:
    head = _b64u(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    body = _b64u(json.dumps(claims).encode())
    sig = hmac.new(secret.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest()
    return f"{head}.{body}.{_b64u(sig)}"


def _bearer(claims: dict) -> dict:
    return {"authorization": "Bearer " + _jwt(claims)}


def _marks(workdir: Path) -> list:
    f = workdir / "marker.log"
    return f.read_text().splitlines() if f.exists() else []


# =========================================================== approval gates (H1, L36)


def _gated_app() -> WebCortex:
    app = WebCortex("gates")
    app.static("POST", "/purge", {"purged": True}, tool=True, tool_name="purge", approval="required")
    app.static("POST", "/ok", {"ok": True}, tool=True, tool_name="ok")
    return app


@pytest.mark.parametrize("shape", ["pipeline", "parallel", "route", "nested"])
def test_a_flow_that_would_run_an_approval_gated_tool_does_not_boot(shape):
    # 2.8.0 booted each of these, and the gated tool then ran with nobody
    # asked whenever the flow was called: by an agent, over MCP, from a behaviour.
    app = _gated_app()
    if shape == "pipeline":
        app.flow("cleanup", pipeline=["ok", {"tool": "purge", "input": {"confirm": True}}])
    elif shape == "parallel":
        app.flow("cleanup", parallel=["ok", "purge"])
    elif shape == "route":
        app.flow("cleanup", route={"tidy": "ok"}, default="purge")
    else:
        app.flow("inner", pipeline=["purge"], tool=True)
        app.flow("cleanup", pipeline=["ok", "inner"])
    with pytest.raises(ValueError) as e:
        app.check()
    message = str(e.value)
    assert '"purge"' in message and "flow" in message, message
    assert '"cleanup"' in message or '"inner"' in message, message


def test_a_flow_whose_agent_step_meets_a_gate_still_boots():
    # An agent asks a human before a gated call, so it may be a step.
    app = _gated_app()
    app.agent("asker", tools=["purge"])
    app.flow("cleanup", pipeline=["ok", "asker"])
    assert "cleanup" in app.check()["flows"]


GATE_APP = '''
import json, os
from webcortex import WebCortex
app = WebCortex("gates", port=__PORT__)
app.api_key("CORE_ADMIN", id="admin", scopes=["read", "write", "admin", "webcortex:admin"])

@app.post("/purge", tool=True, tool_name="purge", scopes=["write"], approval="required")
def purge(confirm: bool = True) -> dict:
    with open(os.environ["MARKER"], "a") as f:
        f.write("purge\\n")
    return {"purged": True}

@app.post("/archive", tool=True, tool_name="archive", scopes=["admin"], approval="required")
def archive() -> dict:
    with open(os.environ["MARKER"], "a") as f:
        f.write("archive\\n")
    return {"archived": True}

app.agent("assistant", tools=["purge"], scopes=["read", "write"], max_steps=3)
app.agent("reader", tools=["archive"], scopes=["read"], max_steps=3)
app.flow("delegate", pipeline=["assistant"])

@app.behaviour("sweeper", tools=["purge"])
def sweeper(ctx, input):
    return ctx.call("purge", confirm=True)
'''


@pytest.fixture(scope="module")
def gates():
    base, proc, workdir = _start(GATE_APP)
    yield base, workdir
    _stop(proc)


def test_the_gate_holds_on_every_path_that_remains(gates):
    base, workdir = gates
    admin = _key(ADMIN_KEY)
    status, out, _ = _request(base, "/agents/assistant", "POST", {"input": "tool:purge {}"}, admin)
    assert status == 202 and out["status"] == "awaiting_approval"
    # A flow step that is an agent fails the flow with the approval id.
    status, out, _ = _request(base, "/flows/delegate", "POST", {"input": "tool:purge {}"}, admin)
    assert status == 500 and "approval" in out["error"]["message"]
    # Over MCP the gated tool is refused outright.
    status, out, _ = _request(base, "/_webcortex/mcp", "POST", {
        "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "purge", "arguments": {}}}, admin)
    assert out["result"]["isError"] is True
    assert out["result"]["structuredContent"]["error"]["code"] == "WC309"
    # A behaviour halts.
    status, out, _ = _request(base, "/behaviours/sweeper", "POST", {}, admin)
    assert status == 200 and out["halted"] is True and "approval" in out["reason"], out
    assert _marks(workdir) == [], "the gated tool ran unattended"


def test_approval_is_not_requested_for_a_call_the_agent_could_never_make(gates):
    # 2.8.0 suspended for approval, the human approved, and the call then
    # failed: the agent holds only "read", the tool needs "admin". The hint
    # told the caller — who holds "admin" — to use a key with it.
    base, workdir = gates
    status, out, _ = _request(base, "/agents/reader", "POST", {"input": "tool:archive {}"}, _key(ADMIN_KEY))
    assert status == 200 and out["status"] == "completed", out
    assert out.get("pending_approval") is None
    refused = [s for s in out["steps"] if s["kind"] == "tool_refused"]
    assert refused and refused[0]["code"] == "WC602"
    assert '"reader"' in refused[0]["hint"] and "scopes=" in refused[0]["hint"], refused[0]["hint"]
    _, pending, _ = _request(base, "/_webcortex/approvals", headers=_key(ADMIN_KEY))
    assert not [p for p in pending["approvals"] if p["agent"] == "reader"]
    assert "archive" not in _marks(workdir)


# =========================================================== bearer tokens (M1, L1, L4, L10, L3)

AUTH_APP = '''
from webcortex import WebCortex
app = WebCortex("auth", port=__PORT__)
app.api_key("CORE_ADMIN", id="ops", scopes=["read", "webcortex:admin"])
app.api_key("CORE_PLACEHOLDER", id="starter", scopes=["read", "webcortex:admin"])
app.jwt(secret_env="CORE_JWT", algorithm="HS256", audience="api.example", issuer="https://idp.example",
        leeway_secs=0)
app.static("GET", "/billing", {"invoices": 1}, scopes=["read"])
app.agent("chat", scopes=["read"], max_steps=1)
'''


@pytest.fixture(scope="module")
def auth_server():
    base, proc, _ = _start(AUTH_APP, {"CORE_PLACEHOLDER": "replace-me-run-webcortex-keygen"})
    yield base
    _stop(proc)


def _claims(**over):
    claims = {"sub": "alice", "exp": int(time.time()) + 600, "aud": "api.example",
              "iss": "https://idp.example", "scope": "read"}
    claims.update(over)
    return {k: v for k, v in claims.items() if v is not None}


def test_a_configured_audience_and_issuer_are_required(auth_server):
    # 2.8.0 checked aud and iss only when a token carried them, so a token
    # minted for another service that simply left them out was accepted.
    assert _request(auth_server, "/billing", headers=_bearer(_claims()))[0] == 200
    for missing in ("aud", "iss"):
        status, body, _ = _request(auth_server, "/billing", headers=_bearer(_claims(**{missing: None})))
        assert status == 401 and body["error"]["code"] == "WC603", missing
    assert _request(auth_server, "/billing", headers=_bearer(_claims(aud="other")))[0] == 401


def test_a_token_may_not_claim_an_api_keys_identity(auth_server):
    # A bearer token whose sub equalled an API key's id shared that service's
    # memory, sessions and rate-limit bucket.
    for sub in ("ops", "watcher:door"):
        status, body, _ = _request(auth_server, "/billing", headers=_bearer(_claims(sub=sub)))
        assert status == 401 and body["error"]["code"] == "WC603", sub
        assert "reserved" in body["error"]["message"]
    assert _request(auth_server, "/billing", headers=_bearer(_claims(sub="ops-user")))[0] == 200


def test_the_starter_placeholder_is_not_a_working_key(auth_server):
    # `webcortex new` writes this value into .env.example; copied to .env it
    # booted as a working admin key that anyone who read the starter knew.
    status, _, _ = _request(auth_server, "/_webcortex/status", headers=_key("replace-me-run-webcortex-keygen"))
    assert status == 401
    assert _request(auth_server, "/_webcortex/status", headers=_key(ADMIN_KEY))[0] == 200


def test_a_bearer_token_may_open_a_socket_in_access_token(auth_server):
    # A browser cannot set Authorization on a WebSocket. 2.8.0 read
    # ?access_token= only as an API key, so a JWT there was refused.
    url = auth_server.replace("http", "ws") + "/agents/chat?access_token=" + _jwt(_claims())
    with WebSocket(url) as ws:
        assert ws.recv_json()["type"] == "ready"
    # An API key in the same parameter still works.
    with WebSocket(auth_server.replace("http", "ws") + "/agents/chat?access_token=" + ADMIN_KEY) as ws:
        assert ws.recv_json()["type"] == "ready"
    # A header credential still wins over the query.
    with pytest.raises(WebSocketError, match="401"):
        WebSocket(url, headers={"x-api-key": "wrong"})


def test_an_agent_socket_stops_honouring_an_expired_token():
    # 2.8.0 checked a token's exp at the upgrade only: the socket kept running
    # agent turns, with the token's scopes and spend, for as long as it was open.
    source = '''
from webcortex import WebCortex
app = WebCortex("exp", port=__PORT__)
app.jwt(secret_env="CORE_JWT", algorithm="HS256", leeway_secs=0)
app.agent("chat", scopes=["read"], max_steps=1)
'''
    base, proc, _ = _start(source)
    try:
        token = _jwt({"sub": "contractor", "exp": int(time.time()) + 2, "scope": "read"})
        url = base.replace("http", "ws") + "/agents/chat"
        ws = WebSocket(url, headers={"authorization": "Bearer " + token}, timeout=15)
        assert ws.recv_json()["type"] == "ready"
        ws.send_json({"input": "hello"})
        assert ws.recv_json()["type"] in ("step", "result")
        while ws.recv_json()["type"] != "result":
            pass
        time.sleep(3.5)
        # Idle past expiry, the socket is told and closed (1008).
        event = ws.recv_json()
        assert event["type"] == "error" and event["code"] == "WC603", event
        with pytest.raises(WebSocketError, match="1008"):
            ws.recv_json()
        ws.close()
    finally:
        _stop(proc)


# =========================================================== socket turns and the rate limit (M2)


def test_agent_socket_turns_are_charged_to_the_rate_limit():
    # 2.8.0 charged the upgrade only: a caller held to 3 runs over HTTP got
    # as many as it sent over one socket.
    source = '''
from webcortex import WebCortex
app = WebCortex("rl", port=__PORT__)
app.api_key("CORE_USER", id="tenant", scopes=["read"])
app.rate_limit(per_second=0.01, burst=3)
app.agent("chat", scopes=["read"], max_steps=1)
'''
    base, proc, _ = _start(source)
    try:
        sock = AgentSocket(base, "/agents/chat", key=USER_KEY, timeout=15)
        done = [sock.ask(f"turn {i}")["status"] for i in range(3)]
        assert done == ["completed"] * 3, "the upgrade pays for the first turn, as an HTTP run would"
        errors = []
        for i in range(3):
            with pytest.raises(WebSocketError) as e:
                sock.ask(f"over {i}")
            errors.append(e.value)
        assert all(err.code == "WC401" for err in errors), [str(e) for e in errors]
        # Refused three times in a row, the socket is closed (1008).
        with pytest.raises(WebSocketError, match="1008"):
            sock.ws.recv_json()
        sock.close()
        # HTTP shares the bucket.
        assert _request(base, "/agents/chat", "POST", {"input": "hi"}, _key(USER_KEY))[0] == 429
    finally:
        _stop(proc)


def test_a_refused_socket_turn_says_when_to_retry():
    source = '''
from webcortex import WebCortex
app = WebCortex("rl2", port=__PORT__)
app.api_key("CORE_USER", id="tenant", scopes=["read"])
app.rate_limit(per_second=0.01, burst=1)
app.agent("chat", scopes=["read"], max_steps=1)
'''
    base, proc, _ = _start(source)
    try:
        ws = WebSocket(base.replace("http", "ws") + "/agents/chat", headers=_key(USER_KEY), timeout=15)
        ws.recv_json()
        ws.send_json({"input": "first"})
        while ws.recv_json()["type"] != "result":
            pass
        ws.send_json({"input": "second"})
        refused = ws.recv_json()
        assert refused["type"] == "error" and refused["code"] == "WC401"
        assert refused["retry_after"] >= 1 and refused["hint"]
        ws.close()
    finally:
        _stop(proc)


# =========================================================== the proxy and redirects (M4)


def test_the_proxy_relays_an_upstream_redirect_instead_of_following_it():
    # 2.8.0 followed a 302 to any host — loopback, a metadata address — sending
    # the upstream's static headers and a Referer there, and relayed the answer.
    seen: list = []

    class Internal(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_GET(self):
            seen.append(dict(self.headers))
            body = b'{"internal": "secret"}'
            self.send_response(200)
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    internal = ThreadingHTTPServer(("127.0.0.1", 0), Internal)
    threading.Thread(target=internal.serve_forever, daemon=True).start()
    internal_url = f"http://127.0.0.1:{internal.server_address[1]}/admin"

    class Upstream(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_GET(self):
            target = internal_url if "away" in self.path else f"http://127.0.0.1:{self.server.server_address[1]}/v2/home?a=1"
            self.send_response(302)
            self.send_header("location", target)
            self.send_header("content-length", "0")
            self.end_headers()

    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    source = f'''
from webcortex import WebCortex
app = WebCortex("gw", port=__PORT__)
app.upstream("billing", "http://127.0.0.1:{upstream.server_address[1]}/v2", headers={{"X-Api-Key": "STATIC"}})
app.proxy("GET", "/billing/away", upstream="billing", rewrite="/away")
app.proxy("GET", "/billing/inside", upstream="billing", rewrite="/inside")
'''
    base, proc, _ = _start(source)
    try:
        import http.client
        port = int(base.rsplit(":", 1)[1])
        for path, expected in (("/billing/away", internal_url), ("/billing/inside", "/home?a=1")):
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
            c.request("GET", path)
            r = c.getresponse()
            r.read()
            assert r.status == 302, path
            assert r.getheader("location") == expected, (path, r.getheader("location"))
            c.close()
        assert seen == [], "the server fetched the redirect target itself"
    finally:
        _stop(proc)
        internal.shutdown()
        upstream.shutdown()


# =========================================================== bodies and slow readers (M5, H2)

LOCKED_APP = '''
import os
from webcortex import WebCortex
HERE = os.path.dirname(os.path.abspath(__file__))
app = WebCortex("locked", port=__PORT__, request_timeout=2)
app.api_key("CORE_ADMIN", id="admin", scopes=["write", "webcortex:admin"])

@app.post("/admin/import", scopes=["write"])
def imp() -> dict:
    return {"ok": True}

app.static_files("/files", os.path.join(HERE, "files"))
'''


@pytest.fixture(scope="module")
def locked():
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-core-files-"))
    (workdir / "files").mkdir()
    (workdir / "files" / "big.bin").write_bytes(os.urandom(12 * 1024 * 1024))
    (workdir / "api.py").write_text(LOCKED_APP.replace("__PORT__", str(port)))
    env = {**os.environ, "WEBCORTEX_LOG": "warn", "CORE_ADMIN": ADMIN_KEY}
    proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", "run", "api.py"], cwd=workdir,
                            stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT, env=env)
    base = f"http://127.0.0.1:{port}"
    for _ in range(300):
        try:
            with urllib.request.urlopen(base + "/_webcortex/health", timeout=1):
                break
        except Exception:
            time.sleep(0.1)
    yield base, port, proc
    _stop(proc)


def _head_before_body(port: int, request: bytes) -> bytes:
    with socket.create_connection(("127.0.0.1", port), timeout=5) as s:
        s.sendall(request)
        s.settimeout(3)
        try:
            return s.recv(4096)
        except socket.timeout:
            return b""


@pytest.mark.parametrize("path,status", [("/no/such/route", b"404"), ("/admin/import", b"401"),
                                         ("/_webcortex/mcp", b"401")])
def test_a_refused_request_is_answered_before_its_body_is_read(locked, path, status):
    # 2.8.0 read (and buffered) up to 32 MB of body before routing or checking
    # scopes, from anyone; the refusal came only once the body had arrived.
    _, port, _ = locked
    reply = _head_before_body(port, f"POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {30 * 1024 * 1024}\r\n"
                                    f"\r\n".encode())
    assert reply.startswith(b"HTTP/1.1 " + status), reply[:200]
    assert b"connection: close" in reply.lower()


def test_a_client_asking_to_continue_is_refused_without_100_continue(locked):
    _, port, _ = locked
    reply = _head_before_body(port, b"POST /admin/import HTTP/1.1\r\nHost: x\r\nContent-Length: 1000\r\n"
                                    b"Expect: 100-continue\r\n\r\n")
    assert reply.startswith(b"HTTP/1.1 401"), reply[:200]


def test_an_authorised_body_is_still_read(locked):
    base, _, _ = locked
    assert _request(base, "/admin/import", "POST", {"x": 1}, _key(ADMIN_KEY))[0] == 200


TRICKLE_APP = '''
import os
from webcortex import WebCortex
HERE = os.path.dirname(os.path.abspath(__file__))
app = WebCortex("trickle", port=__PORT__, request_timeout=30)
app.static_files("/files", os.path.join(HERE, "files"))
'''


def _rss_mb(pid: int) -> int:
    return int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)]).strip()) // 1024


@pytest.mark.skipif(sys.platform == "win32", reason="reads the server's RSS with ps")
def test_slow_readers_do_not_hold_a_copy_of_the_file_each_over_http1():
    # 2.8.0 read a static file whole into memory for every request and kept it
    # until the client had taken it all: eight clients reading 64 KB every
    # second and a half held 8 x 12 MB, and it was never given back.
    size, readers = 12 * 1024 * 1024, 8
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-core-trickle-"))
    (workdir / "files").mkdir()
    (workdir / "files" / "big.bin").write_bytes(os.urandom(size))
    (workdir / "api.py").write_text(TRICKLE_APP.replace("__PORT__", str(port)))
    env = {**os.environ, "WEBCORTEX_LOG": "warn"}
    proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", "run", "api.py"], cwd=workdir,
                            stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT, env=env)
    stop = threading.Event()
    try:
        for _ in range(300):
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/_webcortex/health", timeout=1):
                    break
            except Exception:
                time.sleep(0.1)
        base = _rss_mb(proc.pid)

        def slow(s: socket.socket) -> None:
            s.settimeout(1)
            while not stop.is_set():
                try:
                    if not s.recv(65536):
                        return
                except socket.timeout:
                    pass
                except OSError:
                    return
                time.sleep(1.5)

        socks = []
        for _ in range(readers):
            s = socket.socket()
            s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 65536)
            s.connect(("127.0.0.1", port))
            s.sendall(b"GET /files/big.bin HTTP/1.1\r\nHost: x\r\n\r\n")
            socks.append(s)
            threading.Thread(target=slow, args=(s,), daemon=True).start()
        time.sleep(2.5)
        grew = _rss_mb(proc.pid) - base
        stop.set()
        for s in socks:
            s.close()
        assert grew < readers * size / (1024 * 1024) / 2, f"server memory grew {grew} MB for {readers} slow readers"
    finally:
        stop.set()
        _stop(proc)


def test_a_file_is_streamed_whole_to_a_client_that_reads(locked):
    base, _, _ = locked
    with urllib.request.urlopen(base + "/files/big.bin", timeout=60) as r:
        assert r.status == 200 and int(r.headers["content-length"]) == 12 * 1024 * 1024
        assert len(r.read()) == 12 * 1024 * 1024


def _h2_frame(kind: int, flags: int, stream: int, payload: bytes = b"") -> bytes:
    return struct.pack(">I", len(payload))[1:] + bytes([kind, flags]) + struct.pack(">I", stream) + payload


def _hpack_literal(index: int, value: str) -> bytes:
    raw = value.encode()
    return bytes([index]) + bytes([len(raw)]) + raw


def test_a_response_the_client_never_lets_flow_is_cut_off_over_http2(locked):
    # A zero flow-control window and a PING every second: 2.8.0 counted the
    # pings as activity and held every requested response in memory forever.
    _, port, _ = locked
    s = socket.create_connection(("127.0.0.1", port), timeout=5)
    settings = struct.pack(">HI", 0x4, 0)  # SETTINGS_INITIAL_WINDOW_SIZE = 0
    out = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" + _h2_frame(0x4, 0, 0, settings)
    block = b"\x82\x86" + _hpack_literal(0x04, "/files/big.bin") + _hpack_literal(0x01, "x")
    for stream in (1, 3, 5):
        out += _h2_frame(0x1, 0x5, stream, block)  # HEADERS, END_STREAM | END_HEADERS
    s.sendall(out)
    s.settimeout(0.5)
    started, ended = time.time(), False
    while time.time() - started < 25:
        try:
            if not s.recv(65536):
                ended = True
                break
        except socket.timeout:
            pass
        except OSError:
            ended = True
            break
        try:
            s.sendall(_h2_frame(0x6, 0, 0, b"keepkeep"))  # PING
        except OSError:
            ended = True
            break
    took = time.time() - started
    s.close()
    assert ended, "pings kept a connection with stalled responses open"
    assert took < 15, f"closed only after {took:.0f}s"


# =========================================================== security report (M6)


def test_the_security_report_lists_routes_opened_by_anonymous_scopes(tmp_path):
    # 2.8.0 listed only routes declaring no scope, so a write route opened to
    # everyone by anonymous_scopes("write") was missing from the report.
    source = f'''
from webcortex import WebCortex
app = WebCortex("anon", port=__PORT__, database="sqlite://{tmp_path}/anon.db")
app.api_key("CORE_ADMIN", id="admin", scopes=["read", "write", "webcortex:admin"])
app.anonymous_scopes("write")
app.resource("items", fields={{"id": int, "name": str}}, read_scopes=["read"], write_scopes=["write"])
'''
    base, proc, _ = _start(source)
    try:
        _, report, _ = _request(base, "/_webcortex/security", headers=_key(ADMIN_KEY))
        assert "POST /items" in report["public_routes"], report["public_routes"]
        assert "GET /items" not in report["public_routes"]
    finally:
        _stop(proc)


# =========================================================== structured answers and handoffs (L16)


def test_a_handoff_target_may_not_list_a_final_answer_tool():
    app = WebCortex("h")
    app.static("POST", "/final_answer", {"filed": True}, tool=True, tool_name="final_answer")
    app.agent("specialist", tools=["final_answer"])
    app.agent("desk", handoffs=["specialist"],
              output={"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]})
    # app.manifest() refuses it first (the Python half of the same rule).
    with pytest.raises(ValueError, match="specialist"):
        app.check()


def test_the_core_refuses_the_handoff_final_answer_clash_on_its_own():
    # A manifest that did not come through app.py's check still fails in the core.
    from webcortex import _core
    app = WebCortex("h")
    app.static("POST", "/final_answer", {"filed": True}, tool=True, tool_name="final_answer")
    app.agent("specialist", tools=["final_answer"])
    app.agent("desk", output={"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]})
    manifest = app.manifest()
    for agent in manifest["agents"]:
        if agent["name"] == "desk":
            agent["handoffs"] = ["specialist"]
    with pytest.raises(ValueError, match='"specialist"'):
        _core.inspect_manifest(json.dumps(manifest))


# =========================================================== timeouts and traces (L18, L28)

SLOW_APP = '''
import time
from webcortex import WebCortex
app = WebCortex("slow", port=__PORT__, request_timeout=1, agent_timeout=1)

@app.post("/slow", tool=True, tool_name="slow")
def slow(s: float = 0) -> dict:
    time.sleep(s)
    return {"slept": s}

app.agent("worker", tools=["slow"])
app.flow("pipe", pipeline=[{"tool": "slow", "input": {"s": 2}}])

@app.behaviour("beh", tools=["slow"])
def beh(ctx, input):
    return ctx.call("slow", s=2)
'''


@pytest.mark.parametrize("path,body", [("/flows/pipe", {"input": "x"}), ("/agents/worker", {"input": 'tool:slow {"s": 2}'}),
                                       ("/behaviours/beh", {})])
def test_a_timed_out_run_names_its_trace_and_its_span_is_ended(path, body):
    base, proc, _ = _start(SLOW_APP)
    try:
        status, out, headers = _request(base, path, "POST", body)
        assert status == 504 and out["error"]["code"] == "WC203"
        trace_id = headers.get("x-webcortex-trace")
        assert trace_id, "2.8.0 sent the 504 without the run's trace id"
        assert out["error"]["trace_id"] == trace_id
        time.sleep(2.5)
        status, trace, _ = _request(base, f"/_webcortex/traces/{trace_id}")
        assert status == 200
        # 2.8.0 left a timed-out flow's span open for good, never exported.
        assert trace["status"] == "cancelled" and trace["open_spans"] == 0, trace
    finally:
        _stop(proc)


# =========================================================== webcortex check and templates (L29)


@pytest.mark.parametrize("template", ["{{ a }} {% if %}\n", None])
def test_check_fails_an_app_whose_page_template_cannot_load(tmp_path, template):
    # 2.8.0 parsed templates only at boot, so `webcortex check` passed an app
    # that `webcortex run` then refused.
    (tmp_path / "templates").mkdir()
    if template is not None:
        (tmp_path / "templates" / "p.html").write_text(template)
    app = WebCortex("t", templates=str(tmp_path / "templates"))
    app.page("/p", "p.html", data={})
    with pytest.raises(ValueError, match="p.html"):
        app.check()
    (tmp_path / "templates" / "p.html").write_text("<p>{{ data }}</p>")
    assert app.check()["routes"] >= 1


# =========================================================== behaviours and a full pool (H3)

POOL_APP = '''
import os, time
from webcortex import WebCortex
app = WebCortex("pool", port=__PORT__, workers=1, request_timeout=2, agent_timeout=2, shutdown_timeout=5)

@app.get("/inner", tool=True, tool_name="inner")
def inner() -> dict:
    return {"ok": 1}

@app.post("/slow", tool=True, tool_name="slow")
def slow(s: float = 0) -> dict:
    time.sleep(s)
    return {"slept": s}

@app.get("/ping")
def ping() -> dict:
    return {"pong": 1}

@app.behaviour("bh", tools=["inner"])
def bh(ctx, input):
    return ctx.call("inner")

@app.behaviour("patient", tools=["slow"])
def patient(ctx, input):
    try:
        return ctx.call("slow", s=6)
    except Exception as e:
        with open(os.environ["MARKER"], "a") as f:
            f.write(f"{type(e).__name__} {getattr(e, 'code', None)}\\n")
        raise
'''


def test_behaviours_that_fill_the_pool_do_not_wedge_it():
    # workers=1 gives four pool threads. Each behaviour holds one while its
    # ctx.call waits for /inner, which needs a thread of its own. 2.8.0 waited
    # forever: once four behaviours held every thread, no Python route ran
    # again and SIGTERM could not stop the process.
    base, proc, workdir = _start(POOL_APP)
    try:
        statuses: list = []

        def hit() -> None:
            statuses.append(_request(base, "/behaviours/bh", "POST", {}, timeout=15)[0])

        threads = [threading.Thread(target=hit) for _ in range(24)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(20)
        assert len(statuses) == 24 and set(statuses) <= {200, 504}, statuses
        deadline = time.time() + 10
        while time.time() < deadline:
            try:
                if _request(base, "/ping", timeout=2)[0] == 200:
                    break
            except Exception:
                pass
            time.sleep(0.2)
        else:
            pytest.fail("Python routes never answered again: the pool is wedged")
        # A call cut off by the route's timeout tells the behaviour why.
        status, out, _ = _request(base, "/behaviours/patient", "POST", {}, timeout=15)
        assert status == 504, out
        for _ in range(50):
            if _marks(workdir):
                break
            time.sleep(0.1)
        assert _marks(workdir) == ["BehaviourHalted WC203"], _marks(workdir)
    finally:
        started = time.time()
        proc.terminate()
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            pytest.fail("the server did not exit on SIGTERM")
    assert time.time() - started < 10, "exit took longer than shutdown_timeout allows"


# =========================================================== structured asks (M13)

ASK_APP = '''
from webcortex import WebCortex
app = WebCortex("ask", port=__PORT__)
SCHEMA = {"type": "object",
          "properties": {"urgency": {"type": "integer"}, "category": {"enum": ["bug", "billing", "other"]}},
          "required": ["urgency", "category"]}

def outcome(ctx, fn):
    try:
        return {"value": fn()}
    except RuntimeError as e:
        return {"error": str(e), "code": getattr(e, "code", None), "problems": getattr(e, "problems", None),
                "rejected": sum(1 for t in ctx.trace if t["kind"] == "ask_rejected"),
                "steps": ctx.usage["steps"]}

@app.behaviour("one", max_steps=10)
def one(ctx, input):
    return outcome(ctx, lambda: ctx.ask("Grade json:" + input["answer"], schema=SCHEMA))

@app.behaviour("many", max_steps=10)
def many(ctx, input):
    return outcome(ctx, lambda: ctx.ask_many(["Grade json:" + a for a in input["answers"]], schema=SCHEMA))
'''


@pytest.fixture(scope="module")
def asker():
    base, proc, _ = _start(ASK_APP)
    yield base
    _stop(proc)


@pytest.mark.parametrize("answer,problem", [
    ('{"urgency": 3}', 'missing required property "category"'),
    ('{"urgency": "very high", "category": "bug"}', "$.urgency: expected integer"),
    ('{"urgency": 3, "category": "weather"}', "$.category: must be one of"),
    ("[1, 2]", "expected object"),
])
def test_a_structured_ask_is_held_to_its_schema(asker, answer, problem):
    # 2.8.0 handed whatever the model sent back to the behaviour as the answer.
    status, out, _ = _request(asker, "/behaviours/one", "POST", {"answer": answer})
    assert status == 200, out
    assert out["code"] == "WC318" and out["error"].startswith("WC318:"), out
    assert any(problem in p for p in out["problems"]), out["problems"]
    # Sent back twice with what was wrong, as an agent's output= answer is;
    # each correction is a step.
    assert out["rejected"] == 2 and out["steps"] == 3, out


def test_a_conforming_structured_answer_is_returned(asker):
    status, out, _ = _request(asker, "/behaviours/one", "POST", {"answer": '{"urgency": 3, "category": "bug"}'})
    assert out == {"value": {"urgency": 3, "category": "bug"}}
    status, out, _ = _request(asker, "/behaviours/many", "POST",
                              {"answers": ['{"urgency": 1, "category": "other"}', '{"urgency": 2, "category": "bug"}']})
    assert out == {"value": [{"urgency": 1, "category": "other"}, {"urgency": 2, "category": "bug"}]}


def test_ask_many_holds_every_answer_to_the_schema(asker):
    status, out, _ = _request(asker, "/behaviours/many", "POST",
                              {"answers": ['{"urgency": 1, "category": "other"}', '{"urgency": "9"}']})
    assert out["code"] == "WC318", out
    assert any("$.urgency: expected integer" in p for p in out["problems"]), out


# =========================================================== resources and their schemas (M9)


def test_a_resource_enforces_its_advertised_input_schema(tmp_path):
    # 2.8.0 advertised required fields, types and additionalProperties: false,
    # and applied none of them: {} inserted a row of NULLs, a misspelt key on
    # PUT wiped the row, and "lol" was stored in a REAL column.
    source = f'''
from webcortex import WebCortex
app = WebCortex("res", port=__PORT__, database="sqlite://{tmp_path}/res.db")
app.resource("items", fields={{"id": int, "title": str, "price": float, "ok": bool}})
'''
    base, proc, _ = _start(source)
    try:
        for body, needle in (({}, 'missing required property "title"'),
                             ({"title": "x", "price": "lol", "ok": True}, "$.price: expected number"),
                             ({"title": "x", "price": 1, "ok": True, "colour": "red"}, "colour"),
                             ({"title": "x", "price": 1, "ok": "yes"}, "$.ok: expected boolean")):
            status, out, headers = _request(base, "/items", "POST", body)
            assert status == 422 and out["error"]["code"] == "WC206", out
            assert needle in out["error"]["message"] and out["error"]["problems"], out
        status, row, _ = _request(base, "/items", "POST", {"title": "Laptop", "price": 999.0, "ok": True})
        assert status == 200 and row["ok"] is True, row
        status, out, _ = _request(base, f"/items/{row['id']}", "PUT", {"titel": "typo", "price": 1.0, "ok": False})
        assert status == 422 and out["error"]["code"] == "WC206"
        status, kept, _ = _request(base, f"/items/{row['id']}")
        assert kept == row, "a refused update changed the row"
        status, updated, _ = _request(base, f"/items/{row['id']}", "PUT", {"title": "Laptop", "price": 899.0, "ok": False})
        assert status == 200 and updated["ok"] is False, updated
        # A bool field reads back as a bool, as the output schema says.
        status, rows, _ = _request(base, "/items")
        assert [r["ok"] for r in rows] == [False], rows
    finally:
        _stop(proc)


# =========================================================== one principal and everyone's state (L2)


def test_one_principal_cannot_evict_anothers_approvals_or_sessions():
    # 2.8.0 evicted the oldest approval and least recently used session of
    # anyone's, so one caller suspending a thousand runs (or opening a
    # thousand sessions) destroyed every other caller's.
    source = '''
from webcortex import WebCortex
app = WebCortex("evict", port=__PORT__)
app.api_key("CORE_ADMIN", id="alice", scopes=["webcortex:admin", "write", "read"])
app.api_key("CORE_USER", id="mallory", scopes=["read", "write"])

@app.post("/refund", tool=True, tool_name="refund", scopes=["write"], approval="required")
def refund(amount: int = 1) -> dict:
    return {"refunded": amount}

app.agent("assistant", tools=["refund"], scopes=["write", "read"], max_steps=2)
app.agent("chat", scopes=["read"], max_steps=1)
'''
    base, proc, _ = _start(source)
    try:
        alice, mallory = _key(ADMIN_KEY), _key(USER_KEY)
        _, out, _ = _request(base, "/agents/assistant", "POST", {"input": 'tool:refund {"amount": 900}'}, alice)
        victim = out["pending_approval"]["approval_id"]
        for _ in range(2):
            _, before, _ = _request(base, "/agents/chat", "POST", {"input": "alice context " * 20, "session_id": "a1"}, alice)
        for i in range(110):
            _request(base, "/agents/assistant", "POST", {"input": "tool:refund {}"}, mallory)
            _request(base, "/agents/chat", "POST", {"input": "x", "session_id": f"m{i}"}, mallory)
        _, pending, _ = _request(base, "/_webcortex/approvals", headers=alice)
        assert sum(1 for p in pending["approvals"] if p["caller"] == "mallory") == 100
        status, out, _ = _request(base, f"/_webcortex/approvals/{victim}", "POST", {"approve": True}, alice)
        assert status == 200, out
        _, after, _ = _request(base, "/agents/chat", "POST", {"input": "hi", "session_id": "a1"}, alice)
        assert after["usage"]["input_tokens"] >= before["usage"]["input_tokens"], "alice's session was evicted"
    finally:
        _stop(proc)
