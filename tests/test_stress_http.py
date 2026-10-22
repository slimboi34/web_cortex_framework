"""2.6.0: the HTTP layer under malformed, odd, concurrent and impolite traffic.

One real server for most tests, plus short-lived ones for rate limiting and
shutdown. Raw sockets wherever the client library would tidy the request up.
Each test names what 2.5.3 did wrong, where it did something wrong.
"""
from __future__ import annotations

import base64
import concurrent.futures
import hashlib
import hmac
import http.client
import json
import os
import re
import signal
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

ADMIN_KEY = "stress-admin-key-0123456789abcdef"
JWT_SECRET = "stress-jwt-secret-that-is-at-least-32-bytes"

APP = '''
import time
from pathlib import Path
from webcortex import HTTPError, Response, WebCortex

app = WebCortex("stress", database="sqlite://./stress.db", templates="templates",
                port={port}, request_timeout=5)
app.api_key("ADMIN_KEY", id="admin", scopes=["admin", "webcortex:admin"])
app.jwt(secret_env="JWT_SECRET")
app.anonymous_scopes("open")
app.cors("https://trusted.test")

app.resource("items", fields={{"id": int, "name": str, "code": str}}, scopes=["open"])
app.query("GET", "/echo", "SELECT ? AS v, typeof(?) AS t", params=["v", "v"], returns="one")
app.query("GET", "/mine", "SELECT ? AS who", params=["@principal"], returns="one")
app.static("GET", "/ping", {{"pong": True}})
app.static_files("/assets", "static", index="index.html")
app.page("/page", "page.html.j2", data={{"x": "<script>alert(1)</script>"}})


@app.get("/headers")
def headers(req) -> dict:
    return {{"headers": req.headers}}


@app.get("/typed")
def typed(n: int = 0) -> dict:
    return {{"n": n}}


@app.get("/content-type")
def content_type():
    return Response("a,b", content_type="text/plain",
                    headers={{"Content-Type": "text/csv", "X-Frame-Options": "SAMEORIGIN",
                              "X-Request-Id": "handler-chose-this"}})


@app.get("/bad-status")
def bad_status():
    return Response({{"ok": True}}, status=42)


@app.get("/bad-header")
def bad_header():
    return Response({{"ok": True}}, headers={{"x-split": "a\\r\\nx-injected: yes"}})


@app.get("/wrong-length")
def wrong_length(n: str = "2"):
    return Response({{"payload": "HTTP/1.1 200 OK"}}, headers={{"Content-Length": n,
                    "Transfer-Encoding": "chunked", "Connection": "keep-alive"}})


@app.route("OPTIONS", "/custom-options")
def custom_options() -> dict:
    return {{"custom": True}}


@app.post("/size")
def size(req) -> dict:
    return {{"bytes": len(req.body)}}


@app.get("/slow")
def slow(secs: float = 0.2) -> dict:
    time.sleep(secs)
    return {{"slept": secs}}
'''


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _start(source: str, files: dict[str, str] | None = None, env: dict | None = None,
           http_ready: bool = True):
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-stress-"))
    (workdir / "api.py").write_text(source.format(port=port))
    for name, text in (files or {}).items():
        (workdir / name).parent.mkdir(parents=True, exist_ok=True)
        (workdir / name).write_text(text)
    log_path = workdir / "server.log"
    full_env = {**os.environ, "WEBCORTEX_LOG": "warn", "ADMIN_KEY": ADMIN_KEY,
                "JWT_SECRET": JWT_SECRET, **(env or {})}
    with log_path.open("w") as log:
        proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", "run", "api.py"],
                                cwd=workdir, stdout=log, stderr=subprocess.STDOUT, env=full_env)
    base = f"http://127.0.0.1:{port}"
    deadline = time.time() + 60
    while time.time() < deadline:
        if proc.poll() is not None:
            pytest.fail(f"server exited early:\n{log_path.read_text()}")
        try:
            if http_ready:
                with urllib.request.urlopen(base + "/_webcortex/health", timeout=1):
                    break
            else:
                # A bare connect, so readiness spends nothing a test counts.
                socket.create_connection(("127.0.0.1", port), timeout=1).close()
                break
        except Exception:
            time.sleep(0.1)
    else:
        proc.kill()
        pytest.fail(f"server never became ready:\n{log_path.read_text()}")
    return proc, port, base, workdir, log_path


def _stop(proc):
    proc.terminate()
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=10)


@pytest.fixture(scope="module")
def server():
    files = {
        "static/a.txt": "hello",
        "static/index.html": "<h1>root index</h1>",
        "static/guide/index.html": "<h1>guide</h1>",
        "templates/page.html.j2": "<p>{{ data.x }}</p>",
    }
    proc, port, base, workdir, log_path = _start(APP, files)
    yield base, port, log_path
    _stop(proc)
    assert "panicked" not in log_path.read_text()


def call(base, path, method="GET", body=None, headers=None, timeout=30):
    """(status, raw body bytes, headers as a list of (name, value))."""
    data = body if isinstance(body, bytes) or body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, method=method, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as res:
            return res.status, res.read(), list(res.headers.items())
    except urllib.error.HTTPError as e:
        return e.code, e.read(), list(e.headers.items())


def header(headers, name):
    return [v for k, v in headers if k.lower() == name.lower()]


def raw(port, data: bytes, wait: float = 15.0) -> bytes:
    """Send bytes, read until the server closes the connection."""
    with socket.create_connection(("127.0.0.1", port), timeout=wait) as s:
        s.sendall(data)
        out = b""
        try:
            while chunk := s.recv(65536):
                out += chunk
        except (socket.timeout, ConnectionResetError):
            pass
    return out


def status_of(reply: bytes) -> int:
    match = re.match(rb"HTTP/1\.[01] (\d{3})", reply)
    assert match, f"no status line in {reply[:200]!r}"
    return int(match.group(1))


def statuses(reply: bytes) -> list[int]:
    return [int(m) for m in re.findall(rb"HTTP/1\.1 (\d{3}) ", reply)]


def healthy(base) -> bool:
    return call(base, "/ping")[0] == 200


def _b64(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def make_jwt(payload: dict) -> str:
    head = _b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    body = _b64(json.dumps(payload).encode())
    sig = hmac.new(JWT_SECRET.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest()
    return f"{head}.{body}.{_b64(sig)}"


# ================================================================= HEAD / OPTIONS


def test_head_is_answered_wherever_get_is(server):
    # 2.5.3: 405 on every route, so `curl -I` and load-balancer checks failed.
    base, port, _ = server
    for path in ("/ping", "/assets/a.txt", "/headers", "/_webcortex/health"):
        get_status, get_body, _ = call(base, path)
        status, body, headers = call(base, path, "HEAD")
        assert (status, body) == (get_status, b""), path
        assert header(headers, "content-length") == [str(len(get_body))], path
    # HEAD then GET pipelined on one connection: the HEAD carries no body, so
    # the second response is framed correctly.
    reply = raw(port, b"HEAD /ping HTTP/1.1\r\nHost: x\r\n\r\n"
                      b"GET /ping HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
    assert statuses(reply) == [200, 200]
    assert reply.endswith(b'{"pong":true}')


def test_options_lists_the_allowed_methods(server):
    # 2.5.3: 403 "origin not allowed" for any OPTIONS without an Origin once
    # CORS was configured, and a declared OPTIONS route was unreachable.
    base, _, _ = server
    status, body, headers = call(base, "/items", "OPTIONS")
    assert (status, body) == (204, b"")
    assert header(headers, "allow") == ["GET, HEAD, OPTIONS, POST"]
    status, body, _ = call(base, "/custom-options", "OPTIONS")
    assert status == 200 and json.loads(body) == {"custom": True}
    assert call(base, "/no-such-path", "OPTIONS")[0] == 404
    # A real preflight is still a preflight.
    preflight = {"origin": "https://trusted.test", "access-control-request-method": "POST"}
    status, _, headers = call(base, "/items", "OPTIONS", headers=preflight)
    assert status == 204
    assert header(headers, "access-control-allow-origin") == ["https://trusted.test"]
    evil = {"origin": "https://evil.test", "access-control-request-method": "POST"}
    assert call(base, "/items", "OPTIONS", headers=evil)[0] == 403


def test_405_names_head_and_options_in_allow(server):
    base, _, _ = server
    status, _, headers = call(base, "/ping", "DELETE")
    assert status == 405
    assert header(headers, "allow") == ["GET, HEAD, OPTIONS"]


def test_every_cors_response_varies_on_origin(server):
    # Without Vary on the Origin-less response a shared cache can replay it,
    # without CORS headers, to the trusted origin.
    base, _, _ = server
    for headers in ({}, {"origin": "https://trusted.test"}, {"origin": "https://evil.test"}):
        _, _, got = call(base, "/ping", headers=headers)
        assert header(got, "vary") == ["Origin"], headers


# ================================================================= request headers


def test_repeated_and_non_ascii_request_headers_reach_the_handler(server):
    # 2.5.3 kept only the last of a repeated header (HTTP/2 sends every cookie
    # as its own field) and silently dropped any value that was not ASCII.
    _, port, _ = server
    reply = raw(port, b"GET /headers HTTP/1.1\r\nHost: x\r\nCookie: a=1\r\nCookie: b=2\r\n"
                      b"X-Name: caf\xc3\xa9\r\nConnection: close\r\n\r\n")
    seen = json.loads(reply.split(b"\r\n\r\n", 1)[1])["headers"]
    assert seen["cookie"] == "a=1; b=2"
    assert seen["x-name"] == "café"


def test_a_non_ascii_credential_is_refused_not_ignored(server):
    # 2.5.3 dropped the header, so the caller was served as anonymous.
    _, port, _ = server
    for line in (b"x-api-key: \xffnot-a-key", b"Authorization: Bearer \xff\xfe"):
        reply = raw(port, b"GET /items HTTP/1.1\r\nHost: x\r\n" + line +
                    b"\r\nConnection: close\r\n\r\n")
        assert status_of(reply) == 401, line


def test_a_repeated_credential_is_refused(server):
    _, port, _ = server
    key = ADMIN_KEY.encode()
    reply = raw(port, b"GET /_webcortex/routes HTTP/1.1\r\nHost: x\r\nx-api-key: " + key +
                b"\r\nx-api-key: other\r\nConnection: close\r\n\r\n")
    assert status_of(reply) == 401


# ================================================================= responses


def test_framework_headers_never_duplicate_the_handlers(server):
    # 2.5.3 sent two Content-Types and two (conflicting) X-Frame-Options.
    base, _, _ = server
    status, body, headers = call(base, "/content-type")
    assert status == 200 and body == b"a,b"
    assert header(headers, "content-type") == ["text/csv"]
    assert header(headers, "x-frame-options") == ["SAMEORIGIN"]
    assert header(headers, "x-content-type-options") == ["nosniff"]
    ids = header(headers, "x-request-id")
    assert len(ids) == 1 and ids[0] != "handler-chose-this"


def test_an_unsendable_response_is_a_coded_500(server):
    # 2.5.3: a bare text/plain "response build failed", with no request id and
    # no security headers, and a log line recording a status never sent.
    base, _, _ = server
    for path in ("/bad-status", "/bad-header"):
        status, body, headers = call(base, path)
        assert status == 500, path
        # A Python handler's response is validated in the bridge (WC002); the
        # server's own guard (WC001) stays behind it for every other route kind.
        assert json.loads(body)["error"]["code"] in ("WC001", "WC002")
        assert header(headers, "x-request-id") and header(headers, "x-content-type-options")
        assert not header(headers, "x-injected")


def test_a_handler_cannot_frame_the_response(server):
    # 2.5.3 sent a handler's Content-Length as given: hyper trusts it, so a
    # short one turned the rest of the body into the next "response" on the
    # connection, and a long one hung the client.
    _, port, _ = server
    for n in (b"2", b"500"):
        reply = raw(port, b"GET /wrong-length?n=" + n + b" HTTP/1.1\r\nHost: x\r\n\r\n"
                          b"GET /ping HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        assert status_of(reply) == 200
        assert reply.count(b"x-request-id: ") == 2, reply[:400]
        first = reply.split(b"\r\n\r\n", 1)
        assert b"content-length: 30\r\n" in first[0].lower()
        assert b"transfer-encoding" not in first[0].lower()
        assert first[1].startswith(b'{"payload": "HTTP/1.1 200 OK"}HTTP/1.1 200 OK')
        assert reply.endswith(b'{"pong":true}')


# ================================================================= input


def test_query_values_that_only_look_numeric_stay_text(server):
    # 2.5.3 bound nan/inf/1e999 as NULL, stripped the zero from 007 and
    # rounded 20-digit numbers through f64.
    base, _, _ = server
    for value, expected in (("nan", ("nan", "text")), ("inf", ("inf", "text")),
                            ("1e999", ("1e999", "text")), ("007", ("007", "text")),
                            ("99999999999999999999", ("99999999999999999999", "text")),
                            ("12", (12, "integer")), ("-1.5", (-1.5, "real")),
                            ("true", (1, "integer"))):
        status, body, _ = call(base, f"/echo?v={value}")
        got = json.loads(body)
        assert status == 200 and (got["v"], got["t"]) == expected, value


def test_a_body_a_query_cannot_read_is_a_400_and_writes_nothing(server):
    # 2.5.3 treated a malformed, non-object or too-deep body as absent and
    # inserted a row of NULLs.
    base, _, _ = server
    before = len(json.loads(call(base, "/items")[1]))
    deep = '{"name": ' + "[" * 300 + "]" * 300 + "}"
    for body in (b"{bad json", b"[1, 2]", b'"text"', deep.encode()):
        status, raw_body, _ = call(base, "/items", "POST", body=body,
                                   headers={"content-type": "application/json"})
        assert status == 400, body[:30]
        assert json.loads(raw_body)["error"]["code"] == "WC205"
    assert len(json.loads(call(base, "/items")[1])) == before
    status, created, _ = call(base, "/items", "POST", body={"name": "ok", "code": "02134"})
    assert status == 200 and json.loads(created)["code"] == "02134"


def test_percent_encoded_and_odd_paths_are_answered_cleanly(server):
    base, port, _ = server
    for path in ("/ping/", "//ping", "/p%69ng", "/%00", "/%ff%fe", "/" + "a/" * 2000,
                 "/assets/%2e%2e/api.py", "/assets/..%2fapi.py", "/assets/%252e%252e/api.py"):
        status, body, _ = call(base, path)
        assert status in (400, 404), (path, status)
        assert b"WebCortex(" not in body
    reply = raw(port, b"GET /p\xffing HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
    assert status_of(reply) in (400, 404)
    assert healthy(base)


# ================================================================= auth


def test_a_token_claim_cannot_choose_whose_data_is_read(server):
    # 2.5.3: a signed "root" claim became the caller's root identity, which is
    # what @principal binds.
    base, _, _ = server
    token = make_jwt({"sub": "mallory", "root": "alice", "exp": int(time.time()) + 300})
    status, body, _ = call(base, "/mine", headers={"authorization": f"Bearer {token}"})
    assert status == 200 and json.loads(body)["who"] == "mallory"


def test_a_token_without_a_subject_cannot_bind_principal(server):
    # 2.5.3: every sub-less token was the one principal "jwt-subject-unset",
    # so @principal data was shared between all of them.
    base, _, _ = server
    for claims in ({}, {"sub": ""}):
        token = make_jwt({**claims, "exp": int(time.time()) + 300})
        status, body, _ = call(base, "/mine", headers={"authorization": f"Bearer {token}"})
        assert status == 401 and json.loads(body)["error"]["code"] == "WC607", claims


def test_the_bearer_scheme_is_case_insensitive(server):
    base, _, _ = server
    token = make_jwt({"sub": "bob", "exp": int(time.time()) + 300})
    for scheme in ("Bearer", "bearer", "BEARER", "bEaReR"):
        status, body, _ = call(base, "/mine", headers={"authorization": f"{scheme}  {token}"})
        assert status == 200 and json.loads(body)["who"] == "bob", scheme


def test_expired_and_premature_tokens_are_refused(server):
    base, _, _ = server
    now = int(time.time())
    for claims in ({"sub": "x", "exp": now - 3600}, {"sub": "x", "exp": now + 7200, "nbf": now + 3600},
                   {"sub": "x", "exp": str(now + 60)}, {"sub": "x", "exp": 10**30}):
        token = make_jwt(claims)
        assert call(base, "/mine", headers={"authorization": f"Bearer {token}"})[0] == 401, claims


# ================================================================= static files, pages


def test_a_static_mount_point_serves_its_index(server):
    # 2.5.3: 404 at /assets/ and /assets, since a catch-all never matches "".
    base, _, _ = server
    status, body, _ = call(base, "/assets/")
    assert (status, body) == (200, b"<h1>root index</h1>")
    status, body, _ = call(base, "/assets")
    assert (status, body) == (200, b"<h1>root index</h1>"), "followed the redirect"


def test_a_directory_without_its_slash_redirects(server):
    # 2.5.3 served guide/index.html at /assets/guide, breaking relative links.
    _, port, _ = server
    reply = raw(port, b"GET /assets/guide HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
    assert status_of(reply) == 301
    assert b"\r\nlocation: ./guide/\r\n" in reply.lower()


def test_if_none_match_accepts_a_list_and_the_strong_form(server):
    # 2.5.3 compared the whole header to one tag and never answered 304 to a list.
    base, _, _ = server
    _, _, headers = call(base, "/assets/a.txt")
    etag = header(headers, "etag")[0]
    for value in (f'W/"nope", {etag}', etag.removeprefix("W/"), "*"):
        status, body, _ = call(base, "/assets/a.txt", headers={"if-none-match": value})
        assert (status, body) == (304, b""), value
    assert call(base, "/assets/a.txt", headers={"if-none-match": 'W/"nope"'})[0] == 200


def test_a_jinja_suffixed_html_template_is_escaped(server):
    # 2.5.3 escaped only names ending .html/.htm/.xml, so page.html.j2 was an XSS.
    base, _, _ = server
    status, body, _ = call(base, "/page")
    assert status == 200
    assert b"<script>" not in body and b"&lt;script&gt;" in body


def test_the_control_prefix_matches_only_at_a_segment_boundary(server):
    base, _, _ = server
    status, body, _ = call(base, "/_webcortexhealth")
    assert status == 404 and json.loads(body)["error"]["code"] == "WC201"
    assert call(base, "/_webcortex/health/")[0] == 200


def test_openapi_documents_query_parameters(server):
    base, _, _ = server
    _, body, _ = call(base, "/_webcortex/openapi.json", headers={"x-api-key": ADMIN_KEY})
    params = json.loads(body)["paths"]["/typed"]["get"]["parameters"]
    assert [(p["name"], p["in"]) for p in params] == [("n", "query")]


# ================================================================= framing


@pytest.mark.parametrize("request_bytes, allowed", [
    (b"POST /size HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello!", {400}),
    (b"POST /size HTTP/1.1\r\nHost: x\r\nContent-Length: -1\r\n\r\n", {400}),
    (b"POST /size HTTP/1.1\r\nHost: x\r\nContent-Length: 1x\r\n\r\nx", {400}),
    (b"POST /size HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nhello\r\n0\r\n\r\n", {400}),
    (b"POST /size HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked, identity\r\n\r\n5\r\nhello\r\n0\r\n\r\n", {400, 501}),
    (b"GET /ping\r\n\r\n", {400}),
    (b"GET  /ping HTTP/1.1\r\nHost: x\r\n\r\n", {400}),
    (b"GET /ping HTTP/1.1\r\nHost: x\r\n folded: y\r\n\r\n", {400}),
    (b"GET /ping HTTP/1.1\r\nBad Header: y\r\n\r\n", {400}),
    (b"GET /ping HTTP/9.9\r\nHost: x\r\n\r\n", {400, 505}),
    (b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03" + b"\x00" * 64, {400}),
    (b"GET /ping HTTP/1.1\r\nHost: x\r\n" + b"".join(b"X-H%d: v\r\n" % i for i in range(150)) + b"\r\n", {431}),
    (b"GET /ping HTTP/1.1\r\nHost: x\r\nX-Big: " + b"a" * 600_000 + b"\r\n\r\n", {431}),
    (b"GET /ping?" + b"a" * 100_000 + b" HTTP/1.1\r\nHost: x\r\n\r\n", {414, 431}),
])
def test_malformed_framing_is_refused_cleanly(server, request_bytes, allowed):
    base, port, _ = server
    reply = raw(port, request_bytes)
    if reply:
        assert status_of(reply) in allowed, reply[:200]
    assert healthy(base)


def test_content_length_and_chunked_together_use_chunked(server):
    # RFC 9112 §6.3: Transfer-Encoding wins, and the connection is not reused.
    _, port, _ = server
    reply = raw(port, b"POST /size HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n"
                      b"Content-Length: 3\r\n\r\n5\r\nhello\r\n0\r\n\r\n"
                      b"GET /ping HTTP/1.1\r\nHost: x\r\n\r\n")
    assert status_of(reply) == 200
    assert json.loads(reply.split(b"\r\n\r\n", 1)[1].split(b"HTTP/1.1")[0]) == {"bytes": 5}


def test_many_pipelined_requests_are_answered_in_order(server):
    _, port, _ = server
    batch = b"".join(b"GET /echo?v=%d HTTP/1.1\r\nHost: x\r\n\r\n" % i for i in range(60))
    reply = raw(port, batch + b"GET /ping HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
    assert statuses(reply) == [200] * 61
    values = [json.loads(m)["v"] for m in re.findall(rb'\{"t":"integer","v":\d+\}', reply)]
    assert values == list(range(60))


def test_an_oversized_declared_body_is_refused_before_it_is_sent(server):
    # A client that asks first gets its 413 without sending a byte of body.
    _, port, _ = server
    with socket.create_connection(("127.0.0.1", port), timeout=15) as s:
        s.sendall(b"POST /size HTTP/1.1\r\nHost: x\r\nContent-Length: 999999999\r\n"
                  b"Expect: 100-continue\r\n\r\n")
        reply = s.recv(4096)
    assert status_of(reply) == 413
    assert b"WC204" in reply


def test_a_body_that_stops_short_is_a_400(server):
    base, port, _ = server
    with socket.create_connection(("127.0.0.1", port), timeout=15) as s:
        s.sendall(b"POST /size HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\nshort")
        s.shutdown(socket.SHUT_WR)
        reply = b""
        try:
            while chunk := s.recv(4096):
                reply += chunk
        except (socket.timeout, ConnectionResetError):
            pass
    if reply:
        assert status_of(reply) == 400
    assert healthy(base)


# ================================================================= concurrency


def test_many_concurrent_keep_alive_clients(server):
    base, port, _ = server
    paths = ["/ping", "/echo?v=1", "/assets/a.txt", "/headers", "/items", "/slow?secs=0.01"]

    def client(n: int) -> list[int]:
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=60)
        out = []
        try:
            for i in range(15):
                conn.request("GET", paths[(n + i) % len(paths)])
                res = conn.getresponse()
                res.read()
                out.append(res.status)
        finally:
            conn.close()
        return out

    with concurrent.futures.ThreadPoolExecutor(max_workers=48) as pool:
        results = list(pool.map(client, range(48)))
    assert all(s == 200 for r in results for s in r), results
    assert healthy(base)


def test_slow_and_silent_clients_do_not_block_others(server):
    base, port, _ = server
    held = []
    try:
        for _ in range(40):
            s = socket.create_connection(("127.0.0.1", port), timeout=10)
            s.sendall(b"GET /ping HTTP/1.1\r\nHost: x\r\nX-Slow: ")  # never finished
            held.append(s)
        for _ in range(40):
            held.append(socket.create_connection(("127.0.0.1", port), timeout=10))  # silent
        started = time.time()
        assert healthy(base)
        assert time.time() - started < 10
    finally:
        for s in held:
            s.close()


@pytest.mark.parametrize("preface", [
    b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",   # all of it, then silence
    b"PRI * HTTP/2.0\r\n\r\nSM",           # part of it: the sniff waited forever
])
def test_an_idle_http2_connection_is_closed(server, preface):
    # 2.5.3 cut idle HTTP/1 connections at the header timeout but held an
    # HTTP/2 one — or one stuck in the protocol sniff — open indefinitely.
    _, port, _ = server
    with socket.create_connection(("127.0.0.1", port), timeout=10) as s:
        s.sendall(preface)
        s.settimeout(30)  # request_timeout=5 here, so ~5-10 s is expected
        started = time.time()
        try:
            while s.recv(4096):
                pass
        except ConnectionResetError:
            pass
        assert time.time() - started < 30


def test_a_slow_request_is_not_mistaken_for_an_idle_connection(server):
    base, _, _ = server
    status, body, _ = call(base, "/slow?secs=3")
    assert status == 200 and json.loads(body) == {"slept": 3.0}


# ================================================================= rate limiting

RATE_APP = '''
from webcortex import WebCortex
app = WebCortex("limited", port={port})
app.api_key("ADMIN_KEY", id="admin", scopes=["webcortex:admin"])
app.rate_limit(per_second=0.01, burst=30)
app.static("GET", "/ping", {{"pong": True}})
'''


def test_the_rate_limit_is_exact_under_concurrency():
    proc, port, base, _, log_path = _start(RATE_APP, http_ready=False)
    try:
        def hit(_):
            return call(base, "/ping")

        with concurrent.futures.ThreadPoolExecutor(max_workers=60) as pool:
            results = list(pool.map(hit, range(60)))
        codes = sorted(r[0] for r in results)
        assert codes.count(200) == 30 and codes.count(429) == 30, codes
        refused = next(r for r in results if r[0] == 429)
        assert int(header(refused[2], "retry-after")[0]) >= 1
        assert json.loads(refused[1])["error"]["code"] == "WC401"
        # A different principal has its own bucket.
        assert call(base, "/ping", headers={"x-api-key": ADMIN_KEY})[0] == 200
    finally:
        _stop(proc)
    assert "panicked" not in log_path.read_text()


# ================================================================= shutdown

SHUTDOWN_APP = '''
import time
from pathlib import Path
from webcortex import WebCortex
app = WebCortex("draining", port={port}, request_timeout=30, shutdown_timeout=20)
app.static("GET", "/ping", {{"pong": True}})

@app.get("/work")
def work() -> dict:
    Path("started").write_text("1")
    time.sleep(2.0)
    return {{"finished": True}}
'''


@pytest.mark.skipif(sys.platform == "win32", reason="SIGTERM is TerminateProcess on Windows")
def test_shutdown_finishes_in_flight_requests_and_closes_idle_ones():
    # 2.5.3 waited for `MAX_PERMITS as u32` connection permits — a truncation
    # satisfied at once — so the drain never waited: a request in flight at
    # SIGTERM was cut off with no response, and idle keep-alive connections
    # were never told to close.
    proc, port, base, workdir, log_path = _start(SHUTDOWN_APP)
    try:
        idle = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
        idle.request("GET", "/ping")
        assert idle.getresponse().read() == b'{"pong":true}'

        outcome = {}

        def in_flight():
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=60)
            try:
                conn.request("GET", "/work")
                res = conn.getresponse()
                outcome["status"], outcome["body"] = res.status, res.read()
                outcome["connection"] = res.getheader("connection")
            except Exception as e:  # pragma: no cover - the failure being tested
                outcome["error"] = repr(e)

        worker = threading.Thread(target=in_flight)
        worker.start()
        deadline = time.time() + 30
        while not (workdir / "started").exists() and time.time() < deadline:
            time.sleep(0.02)
        assert (workdir / "started").exists(), "the handler never started"

        signalled = time.time()
        proc.send_signal(signal.SIGTERM)

        # The idle connection is closed promptly rather than held to the deadline.
        idle.sock.settimeout(15)
        assert idle.sock.recv(1) == b""
        worker.join(timeout=60)
        assert outcome.get("status") == 200, outcome
        assert json.loads(outcome["body"]) == {"finished": True}
        assert outcome["connection"] == "close"
        proc.wait(timeout=30)
        assert proc.returncode == 0
        assert time.time() - signalled < 15, "exited only at the drain deadline"
        # Nothing new is accepted once draining starts.
        with pytest.raises(OSError):
            socket.create_connection(("127.0.0.1", port), timeout=2).close()
    finally:
        if proc.poll() is None:
            proc.kill()
    assert "panicked" not in log_path.read_text()


# ================================================================= declarations


def test_a_static_root_with_an_index_takes_the_root_path(tmp_path):
    from webcortex import WebCortex

    (tmp_path / "index.html").write_text("<h1>site</h1>")
    app = WebCortex("t")
    app.static_files("/", str(tmp_path), index="index.html")
    roots = [r for r in app.manifest()["routes"] if r["path"] == "/"]
    assert roots == [], "the discovery route must not shadow the site's index"
    bare = WebCortex("t")
    bare.static_files("/", str(tmp_path))
    assert [r["op"]["kind"] for r in bare.manifest()["routes"] if r["path"] == "/"] == ["static"]
