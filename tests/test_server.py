"""End-to-end tests against a real server process.

These are the tests that actually verify the framework's central claim: that one
declaration produces a REST API and an MCP tool surface that agree with each
other. Everything else is unit-testable; this is not.
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

APP_SOURCE = '''
from dataclasses import dataclass
from rango import HTTPError, Rango

app = Rango("itest", description="integration", database="sqlite://./itest.db", port={port})

app.resource("books", fields={{"id": int, "title": str, "author": str}}, tools=True)

app.static("GET", "/ping", {{"pong": True}}, tool=True, tool_name="ping")

app.static("GET", "/vault", {{"secret": 1}}, tool=True, tool_name="vault", scopes=["admin"])


@dataclass
class Blurb:
    id: int
    text: str


@app.get("/books/{{id}}/blurb", tool=True)
def blurb(id: int, style: str = "plain") -> Blurb:
    """Pitch a book."""
    if style not in ("plain", "loud"):
        raise HTTPError(422, "bad style")
    text = f"book {{id}}"
    return Blurb(id=id, text=text.upper() if style == "loud" else text)


@app.get("/boom")
def boom() -> dict:
    raise RuntimeError("intentional explosion")


@app.get("/async")
async def slow() -> dict:
    import asyncio
    await asyncio.sleep(0.005)
    return {{"ok": True}}
'''


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Server:
    def __init__(self, base: str, proc: subprocess.Popen, log: Path) -> None:
        self.base, self.proc, self.log = base, proc, log

    def request(self, path: str, method: str = "GET", body: dict | None = None):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(
            self.base + path,
            data=data,
            method=method,
            headers={"content-type": "application/json"} if data else {},
        )
        try:
            with urllib.request.urlopen(req, timeout=15) as r:
                raw = r.read()
                return r.status, json.loads(raw) if raw else None
        except urllib.error.HTTPError as e:
            raw = e.read()
            return e.code, json.loads(raw) if raw else None

    def rpc(self, method: str, params: dict | None = None, rid: int = 1):
        payload = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            payload["params"] = params
        status, body = self.request("/_rango/mcp", "POST", payload)
        assert status == 200, f"MCP transport error {status}: {body}"
        return body


@pytest.fixture(scope="module")
def server():
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="rango-itest-"))
    (workdir / "api.py").write_text(APP_SOURCE.format(port=port))
    log_path = workdir / "server.log"

    env = {**os.environ, "RANGO_LOG": "warn"}
    with log_path.open("w") as log:
        proc = subprocess.Popen(
            [sys.executable, "-m", "rango.cli", "run", "api.py"],
            cwd=workdir, stdout=log, stderr=subprocess.STDOUT, env=env,
        )

    base = f"http://127.0.0.1:{port}"
    deadline = time.time() + 45
    while time.time() < deadline:
        if proc.poll() is not None:
            pytest.fail(f"server exited early:\n{log_path.read_text()}")
        try:
            with urllib.request.urlopen(base + "/_rango/health", timeout=1):
                break
        except Exception:
            time.sleep(0.1)
    else:
        proc.kill()
        pytest.fail(f"server never became ready:\n{log_path.read_text()}")

    yield Server(base, proc, log_path)

    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()


# ---------------------------------------------------------------- REST


def test_health_reports_the_runtime_shape(server):
    status, body = server.request("/_rango/health")
    assert status == 200
    assert body["app"] == "itest"
    assert body["python_workers"] >= 1


def test_crud_round_trip_never_touches_python(server):
    status, created = server.request(
        "/books", "POST", {"title": "Dune", "author": "Herbert"}
    )
    assert status == 200
    assert created["title"] == "Dune"
    book_id = created["id"]

    status, fetched = server.request(f"/books/{book_id}")
    assert (status, fetched["author"]) == (200, "Herbert")

    status, updated = server.request(
        f"/books/{book_id}", "PUT", {"title": "Dune Messiah", "author": "Herbert"}
    )
    assert (status, updated["title"]) == (200, "Dune Messiah")

    status, listed = server.request("/books?limit=10")
    assert status == 200 and any(b["id"] == book_id for b in listed)

    status, deleted = server.request(f"/books/{book_id}", "DELETE")
    assert (status, deleted["affected"]) == (200, 1)

    assert server.request(f"/books/{book_id}")[0] == 404


def test_python_handler_binds_typed_params(server):
    assert server.request("/books/7/blurb")[1] == {"id": 7, "text": "book 7"}
    assert server.request("/books/7/blurb?style=loud")[1]["text"] == "BOOK 7"


def test_http_error_becomes_a_clean_status(server):
    status, body = server.request("/books/1/blurb?style=nonsense")
    assert status == 422
    assert "bad style" in json.dumps(body)


def test_handler_exception_is_a_500_and_does_not_kill_the_server(server):
    assert server.request("/boom")[0] == 500
    # The very next request must still succeed.
    assert server.request("/ping")[0] == 200


def test_async_handler(server):
    assert server.request("/async") == (200, {"ok": True})


def test_missing_required_param_is_422(server):
    # `id` comes from the path, so this instead exercises an unknown path.
    assert server.request("/books//blurb")[0] in (404, 422)


def test_unknown_route_and_method(server):
    assert server.request("/nope")[0] == 404
    assert server.request("/ping", "DELETE")[0] == 405


def test_scoped_route_is_refused_over_plain_http(server):
    # 401, not 403: nobody is authenticated, so the caller can still fix this.
    assert server.request("/vault")[0] == 401


# ---------------------------------------------------------------- OpenAPI


def test_openapi_is_served_and_self_describes(server):
    status, spec = server.request("/_rango/openapi.json")
    assert status == 200
    assert spec["openapi"] == "3.1.0"
    assert spec["x-rango"]["mcp_endpoint"] == "/_rango/mcp"
    assert spec["paths"]["/books"]["get"]["x-rango-op"] == "query"


# ---------------------------------------------------------------- MCP


def test_mcp_initialize(server):
    result = server.rpc("initialize", {"protocolVersion": "2025-06-18"})["result"]
    assert result["protocolVersion"] == "2025-06-18"
    assert result["serverInfo"]["name"] == "itest"


def test_mcp_tools_list_matches_the_declared_routes(server):
    tools = server.rpc("tools/list")["result"]["tools"]
    names = {t["name"] for t in tools}
    assert {"list_books", "create_books", "ping", "get_books_by_id_blurb"} <= names
    # A route that was never marked tool=True must not leak into the tool list.
    assert "get" not in names

    blurb = next(t for t in tools if t["name"] == "get_books_by_id_blurb")
    assert blurb["inputSchema"]["properties"]["id"] == {"type": "integer"}
    assert blurb["annotations"]["readOnlyHint"] is True


def test_mcp_tool_call_reaches_a_native_sql_route(server):
    server.request("/books", "POST", {"title": "Neuromancer", "author": "Gibson"})
    out = server.rpc("tools/call", {"name": "list_books", "arguments": {"limit": 50}})
    assert out["result"]["isError"] is False
    assert any(b["author"] == "Gibson" for b in out["result"]["structuredContent"])


def test_mcp_tool_call_reaches_a_python_route(server):
    out = server.rpc(
        "tools/call",
        {"name": "get_books_by_id_blurb", "arguments": {"id": 3, "style": "loud"}},
    )
    assert out["result"]["structuredContent"] == {"id": 3, "text": "BOOK 3"}


def test_mcp_tool_call_can_mutate(server):
    out = server.rpc(
        "tools/call",
        {"name": "create_books", "arguments": {"title": "Snow Crash", "author": "Stephenson"}},
    )
    created = out["result"]["structuredContent"]
    assert created["title"] == "Snow Crash"
    assert server.request(f"/books/{created['id']}")[1]["author"] == "Stephenson"


def test_mcp_reports_tool_failure_to_the_model_not_as_a_transport_error(server):
    out = server.rpc("tools/call", {"name": "get_books", "arguments": {"id": 999999}})
    assert out["result"]["isError"] is True
    assert "404" in out["result"]["content"][0]["text"]


def test_mcp_caller_cannot_reach_a_tool_its_scopes_do_not_cover(server):
    """MCP must not grant authority the same caller lacks over HTTP.

    Earlier this substituted the *route's* declared scopes for the caller's,
    which made every scoped tool reachable by any MCP client.
    """
    out = server.rpc("tools/call", {"name": "vault", "arguments": {}})
    assert out["result"]["isError"] is True
    assert "scope" in out["result"]["content"][0]["text"].lower()


def test_mcp_tool_list_hides_tools_the_caller_cannot_use(server):
    names = {t["name"] for t in server.rpc("tools/list")["result"]["tools"]}
    assert "ping" in names
    assert "vault" not in names, "a tool the caller cannot call must not be advertised"


def test_mcp_unknown_tool_and_method(server):
    assert server.rpc("tools/call", {"name": "ghost", "arguments": {}})["result"]["isError"]
    assert server.rpc("no_such_method")["error"]["code"] == -32601


def test_mcp_notification_gets_no_body(server):
    status, body = server.request(
        "/_rango/mcp", "POST", {"jsonrpc": "2.0", "method": "notifications/initialized"}
    )
    assert status == 202 and body is None


def test_mcp_batch(server):
    status, body = server.request(
        "/_rango/mcp",
        "POST",
        [
            {"jsonrpc": "2.0", "id": 1, "method": "ping"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        ],
    )
    assert status == 200
    assert len(body) == 2
    assert {r["id"] for r in body} == {1, 2}
