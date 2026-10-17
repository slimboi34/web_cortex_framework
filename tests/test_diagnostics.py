"""2.5: stable error codes with hints, the live status view, `webcortex doctor`,
`webcortex status` and `webcortex explain` — over a real server where it matters."""
from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from webcortex import WebCortex, errors
from webcortex.cli import main

ROOT = Path(__file__).resolve().parent.parent
ADMIN_KEY = "diag-admin-key-0123456789abcdef"
USER_KEY = "diag-user-key-0123456789abcdef"
JWT_SECRET = "diag-jwt-signing-secret-at-least-32-bytes"

APP = '''
from webcortex import WebCortex, HTTPError

app = WebCortex("diag", port={port}, start_halted=True)
app.api_key("DIAG_ADMIN_KEY", id="admin", scopes=["read", "write", "webcortex:admin"])
app.api_key("DIAG_USER_KEY", id="user", scopes=["read"])
app.cors("https://good.example")
app.jwt(secret_env="DIAG_JWT_SECRET")

app.static("GET", "/ping", {{"pong": True}}, tool=True, tool_name="ping", scopes=["read"])

@app.post("/notes", tool=True, tool_name="add_note", scopes=["read"])
def add_note(title: str) -> dict:
    return {{"title": title}}

@app.post("/arm/move", tool=True, tool_name="move", actuator=True, scopes=["write"], approval="required")
def move(x: int = 0) -> dict:
    return {{"moved": x}}

@app.get("/boom", scopes=["read"])
def boom() -> dict:
    raise RuntimeError("kaboom: secret internals")

@app.get("/teapot", scopes=["read"])
def teapot() -> dict:
    raise HTTPError(409, "the pot is full", code="WC214", hint="Empty the pot first.")

@app.get("/login")
def login() -> dict:
    raise HTTPError(401, "wrong email or password")

@app.get("/owner-only", scopes=["read"])
def owner_only() -> dict:
    raise HTTPError(403, "this camera belongs to another customer")

app.camera("cam", read_scopes=["read"], ingest_scopes=["write"])
app.agent("desk", tools=["ping", "add_note"], scopes=["read"], token_budget=100000)
app.agent("tight", tools=["ping"], scopes=["read"], token_budget=1)
'''


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _request(base, path, method="GET", body=None, key=None, headers=None):
    data = json.dumps(body).encode() if body is not None else None
    h = {"content-type": "application/json"} if data else {}
    h.update(headers or {})
    if key:
        h["x-api-key"] = key
    req = urllib.request.Request(base + path, data=data, method=method, headers=h)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read()
            return r.status, dict(r.headers), json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        raw = e.read()
        return e.code, dict(e.headers), json.loads(raw) if raw else None


@pytest.fixture(scope="module")
def server():
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-diag-"))
    (workdir / "api.py").write_text(APP.format(port=port))
    log_path = workdir / "server.log"
    env = {**os.environ, "WEBCORTEX_LOG": "warn", "WEBCORTEX_FAKE_PROVIDER": "1",
           "DIAG_ADMIN_KEY": ADMIN_KEY, "DIAG_USER_KEY": USER_KEY,
           "DIAG_JWT_SECRET": JWT_SECRET}
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
    yield base, workdir, port
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()


def _assert_coded(body, status, code):
    err = body["error"]
    assert err["status"] == status and isinstance(err["message"], str)  # 2.4 fields kept
    assert err["code"] == code, body
    assert err["hint"], body
    assert errors.lookup(code) is not None


def _status(base):
    s, _, body = _request(base, "/_webcortex/status", key=ADMIN_KEY)
    assert s == 200, body
    return body


# --- error bodies over HTTP ------------------------------------------------------


@pytest.mark.parametrize("path,method,body,key,headers,status,code", [
    ("/nope", "GET", None, USER_KEY, {}, 404, "WC201"),
    ("/ping", "DELETE", None, USER_KEY, {}, 405, "WC202"),
    ("/ping", "GET", None, None, {}, 401, "WC601"),
    ("/arm/move", "POST", {"x": 1}, USER_KEY, {}, 403, "WC602"),
    ("/arm/move", "POST", {"x": 1}, ADMIN_KEY, {}, 423, "WC502"),
    ("/notes", "POST", {}, USER_KEY, {}, 422, "WC206"),
    ("/boom", "GET", None, USER_KEY, {}, 500, "WC002"),
    ("/devices/cam/latest", "GET", None, USER_KEY, {}, 404, "WC503"),
    ("/agents/desk", "POST", {"nope": 1}, USER_KEY, {}, 400, "WC306"),
    ("/notes", "POST", {"title": "x"}, USER_KEY, {"origin": "https://evil.example"}, 403, "WC605"),
    ("/ping", "GET", None, "not-a-real-key-at-all-000000", {}, 401, "WC603"),
    ("/_webcortex/usage", "GET", None, None, {}, 401, "WC604"),
    ("/_webcortex/nothing", "GET", None, ADMIN_KEY, {}, 404, "WC212"),
])
def test_error_bodies_carry_code_and_hint_and_reach_the_status_view(
        server, path, method, body, key, headers, status, code):
    base, _, _ = server
    rid = f"diag-{code}-{time.time_ns()}"
    s, h, out = _request(base, path, method, body, key, {**headers, "x-request-id": rid})
    assert s == status, out
    _assert_coded(out, status, code)
    assert h.get("x-webcortex-error") == code
    recent = _status(base)["errors"]["recent"]
    hit = [e for e in recent if e["request_id"] == rid]
    assert hit and hit[0]["code"] == code and hit[0]["status"] == status


def test_a_python_exception_never_leaks_into_the_body(server):
    base, _, _ = server
    _, _, out = _request(base, "/boom", key=USER_KEY)
    assert "kaboom" not in json.dumps(out)


def test_http_error_may_name_its_own_code_and_hint(server):
    base, _, _ = server
    s, h, out = _request(base, "/teapot", key=USER_KEY)
    assert s == 409
    assert out["error"] == {"status": 409, "message": "the pot is full", "code": "WC214",
                            "hint": "Empty the pot first."}
    assert h.get("x-webcortex-error") == "WC214"


# --- the status view ---------------------------------------------------------------


def test_status_is_admin_only(server):
    base, _, _ = server
    s, _, out = _request(base, "/_webcortex/status")
    assert s == 401 and out["error"]["code"] == "WC604"
    s, _, out = _request(base, "/_webcortex/status", key=USER_KEY)
    assert s == 403 and out["error"]["code"] == "WC602"


def test_status_describes_the_running_app(server):
    base, _, _ = server
    _request(base, "/ping", key=USER_KEY)
    st = _status(base)
    for key in ("uptime_secs", "webcortex", "app_version", "requests", "errors", "agents", "usage",
                "budgets", "estop", "devices"):
        assert key in st, key
    import webcortex
    assert st["webcortex"] == webcortex.__version__
    ping = next(r for r in st["requests"]["routes"] if r["route"] == "GET /ping")
    assert ping["status"]["2xx"] >= 1 and ping["p50_ms"] > 0
    assert st["estop"]["halted"] is True and st["estop"]["actuators"] == ["move"]
    cam = st["devices"]["devices"][0]
    assert cam["name"] == "cam" and cam["frames_in"] == 0 and cam["connected"] is False
    # Nothing secret: no key, no body text.
    text = json.dumps(st)
    assert ADMIN_KEY not in text and USER_KEY not in text and "kaboom" not in text


def test_agent_failures_carry_codes_and_show_up_as_runs(server):
    base, _, _ = server
    s, _, out = _request(base, "/agents/desk", "POST", {"input": "fail:anthropic 529: overloaded"},
                         key=USER_KEY)
    assert s == 502 and out["status"] == "failed"
    assert out["code"] == "WC304" and out["hint"]
    s, _, tight = _request(base, "/agents/tight", "POST", {"input": "tool:ping {}"}, key=USER_KEY)
    assert s == 200 and tight["status"] == "budget_exhausted" and tight["code"] == "WC402"
    s, _, ok = _request(base, "/agents/desk", "POST", {"input": "hi"}, key=USER_KEY)
    assert ok["status"] == "completed" and "code" not in ok
    runs = {r["run_id"]: r for r in _status(base)["agents"]["recent"]}
    assert runs[out["run_id"]]["outcome"] == "failed" and runs[out["run_id"]]["code"] == "WC304"
    assert runs[tight["run_id"]]["code"] == "WC402"
    assert runs[ok["run_id"]]["tokens"] > 0
    budgets = {b["name"]: b for b in _status(base)["budgets"]}
    assert budgets["tight"]["budget_exhausted"] >= 1 and budgets["tight"]["token_budget"] == 1


def test_a_tool_validation_failure_inside_a_run_names_its_code(server):
    base, _, _ = server
    s, _, out = _request(base, "/agents/desk", "POST", {"input": "tool:add_note {}"}, key=USER_KEY)
    assert s == 200
    step = next(st for st in out["steps"] if st.get("tool") == "add_note")
    assert step["code"] == "WC206" and "missing required parameter" in step["error"]
    assert step["hint"] == errors.hint("WC206")
    recent = _status(base)["errors"]["recent"]
    assert any(e["source"] == "tool" and e["request_id"] == out["run_id"] and e["code"] == "WC206"
               for e in recent)


def test_agent_instructions_and_context_pack_teach_the_diagnostics():
    from webcortex import starters
    agents_md = starters.files_for("api", "demo", "a demo")["AGENTS.md"]
    for must in ("webcortex doctor", "webcortex status", "webcortex explain", "hint"):
        assert must in agents_md, must
    pack = WebCortex("p").context_pack()
    for must in ("webcortex doctor", "webcortex_status", "WC402", "fail:"):
        assert must in pack, must


def test_mcp_exposes_status_and_codes_tool_errors(server):
    base, _, _ = server

    def rpc(method, params=None):
        s, _, out = _request(base, "/_webcortex/mcp", "POST",
                             {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}},
                             key=ADMIN_KEY)
        assert s == 200
        return out["result"]

    names = [t["name"] for t in rpc("tools/list")["tools"]]
    assert "webcortex_status" in names and "webcortex_explain" in names
    st = rpc("tools/call", {"name": "webcortex_status", "arguments": {}})["structuredContent"]
    assert st["app"] == "diag"
    err = rpc("tools/call", {"name": "move", "arguments": {}})
    assert err["isError"] is True and err["structuredContent"]["error"]["code"] == "WC309"
    uris = [r["uri"] for r in rpc("resources/list")["resources"]]
    assert uris == ["webcortex://status", "webcortex://errors"]


# --- the CLI -------------------------------------------------------------------------


def test_status_command_reads_the_running_app(server, capsys, monkeypatch):
    base, workdir, _ = server
    monkeypatch.chdir(workdir)
    monkeypatch.setenv("DIAG_ADMIN_KEY", ADMIN_KEY)
    monkeypatch.setenv("WEBCORTEX_API_KEY", ADMIN_KEY)
    assert main(["status", "--json", "--url", base]) == 0
    assert json.loads(capsys.readouterr().out)["app"] == "diag"
    assert main(["status", "--url", base]) == 0
    assert "diag" in capsys.readouterr().out
    monkeypatch.delenv("WEBCORTEX_API_KEY")
    assert main(["status", "--json", "--url", base]) == 1
    assert json.loads(capsys.readouterr().out)["error"]["code"] == "WC604"


def test_an_app_raised_401_or_403_is_not_blamed_on_webcortex_auth(server):
    # A login form's wrong password is the app's answer, not a missing API key.
    base, _, _ = server
    s, h, out = _request(base, "/login")
    _assert_coded(out, 401, "WC609")
    assert "x-api-key" not in out["error"]["hint"]
    assert h.get("x-webcortex-error") == "WC609"
    s, _, out = _request(base, "/owner-only", key=USER_KEY)
    _assert_coded(out, 403, "WC610")
    # WebCortex's own refusals keep their codes.
    _assert_coded(_request(base, "/ping")[2], 401, "WC601")


def _jwt(payload: dict) -> str:
    def b64(raw: bytes) -> str:
        return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()
    head = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    body = b64(json.dumps(payload).encode())
    sig = hmac.new(JWT_SECRET.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest()
    return f"{head}.{body}.{b64(sig)}"


def test_credential_headers_send_a_jwt_as_a_bearer_token():
    from webcortex.diagnostics import credential_headers
    assert credential_headers("wc_live_abc") == {"x-api-key": "wc_live_abc"}
    token = _jwt({"sub": "ops", "exp": 1})
    assert credential_headers(token) == {"authorization": f"Bearer {token}"}
    assert credential_headers(f"Bearer {token}") == {"authorization": f"Bearer {token}"}
    assert credential_headers(None) == {} and credential_headers("  ") == {}


def test_status_command_accepts_a_jwt(server, capsys, monkeypatch):
    base, workdir, _ = server
    monkeypatch.chdir(workdir)
    monkeypatch.delenv("WEBCORTEX_API_KEY", raising=False)
    token = _jwt({"sub": "ops", "scope": "webcortex:admin", "exp": int(time.time()) + 300})
    assert main(["status", "--json", "--url", base, "--key", token]) == 0
    assert json.loads(capsys.readouterr().out)["app"] == "diag"
    monkeypatch.setenv("WEBCORTEX_TOKEN", token)
    assert main(["status", "--json", "--url", base]) == 0
    assert json.loads(capsys.readouterr().out)["app"] == "diag"


def test_status_command_when_nothing_is_running(tmp_path, capsys, monkeypatch):
    monkeypatch.chdir(tmp_path)
    port = _free_port()
    assert main(["status", "--json", "--url", f"http://127.0.0.1:{port}"]) == 1
    assert json.loads(capsys.readouterr().out)["error"]["code"] == "WC108"


def test_explain(capsys):
    assert main(["explain", "wc402"]) == 0
    out = capsys.readouterr().out
    assert out.startswith("WC402  Token budget exhausted") and "token_budget" in out
    assert main(["explain", "WC402", "--json"]) == 0
    assert json.loads(capsys.readouterr().out)["category"] == "limits"
    assert main(["explain", "--list", "--json"]) == 0
    listed = json.loads(capsys.readouterr().out)["errors"]
    assert [e["code"] for e in listed] == [e["code"] for e in errors.catalogue()]
    assert main(["explain", "WC999"]) == 1
    assert "error[WC205]" in capsys.readouterr().err


def _write(tmp_path, source):
    (tmp_path / "api.py").write_text(source)


def test_check_reports_an_invalid_app_as_json_with_a_code(tmp_path, capsys, monkeypatch):
    monkeypatch.chdir(tmp_path)
    _write(tmp_path, "from webcortex import WebCortex\napp = WebCortex('x')\n"
                     "app.agent('a', tools=['no_such_tool'])\n")
    assert main(["check"]) == 1
    out = json.loads(capsys.readouterr().out)
    assert out["ok"] is False and out["error"]["code"] == "WC101" and out["error"]["hint"]
    assert "no_such_tool" in out["error"]["message"]


def test_a_module_that_does_not_import_is_wc102(tmp_path, capsys, monkeypatch):
    monkeypatch.chdir(tmp_path)
    _write(tmp_path, "import this_module_does_not_exist\n")
    assert main(["check"]) == 1
    out = json.loads(capsys.readouterr().out)
    assert out["error"]["code"] == "WC102" and "this_module_does_not_exist" in out["error"]["message"]
    assert main(["doctor", "--json"]) == 1
    report = json.loads(capsys.readouterr().out)
    assert report["checks"][0]["code"] == "WC102"


def test_doctor_names_missing_keys_without_printing_values(tmp_path, capsys, monkeypatch):
    monkeypatch.chdir(tmp_path)
    port = _free_port()
    _write(tmp_path, f"from webcortex import WebCortex\napp = WebCortex('doc', port={port})\n"
                     "app.api_key('DOC_KEY', id='svc', scopes=['webcortex:admin'])\n"
                     "app.agent('a', model='claude-test')\n")
    for var in ("DOC_KEY", "ANTHROPIC_API_KEY", "WEBCORTEX_FAKE_PROVIDER"):
        monkeypatch.delenv(var, raising=False)
    assert main(["doctor", "--json"]) == 1
    report = json.loads(capsys.readouterr().out)
    by_id = {c["id"]: c for c in report["checks"]}
    assert by_id["api_key:DOC_KEY"]["status"] == "fail" and by_id["api_key:DOC_KEY"]["code"] == "WC106"
    assert by_id["model:claude-test"]["code"] == "WC301"
    assert by_id["budgets"]["status"] == "warn"
    assert by_id["port"]["status"] == "ok"
    for c in report["checks"]:
        assert set(c) >= {"id", "status", "message", "hint"} and c["status"] in ("ok", "warn", "fail")

    secret = "doc-secret-value-0123456789abcdef"
    monkeypatch.setenv("DOC_KEY", secret)
    monkeypatch.setenv("WEBCORTEX_FAKE_PROVIDER", "1")
    assert main(["doctor", "--json"]) == 0
    out = capsys.readouterr().out
    assert secret not in out
    report = json.loads(out)
    assert report["ok"] is True and report["summary"]["fail"] == 0
    assert main(["doctor"]) == 0
    assert "0 failure(s)" in capsys.readouterr().out


def test_doctor_sees_the_running_app_and_a_foreign_port(server, monkeypatch):
    base, workdir, port = server
    monkeypatch.chdir(workdir)
    monkeypatch.setenv("DIAG_ADMIN_KEY", ADMIN_KEY)
    monkeypatch.setenv("DIAG_USER_KEY", USER_KEY)
    monkeypatch.setenv("WEBCORTEX_FAKE_PROVIDER", "1")
    from webcortex.cli import _load_app
    report = _load_app("api.py").doctor()
    by_id = {c["id"]: c for c in report["checks"]}
    assert by_id["running"]["status"] == "ok"
    assert by_id["estop.live"]["message"].endswith("HALTED")

    other = WebCortex("someone-else", port=port)
    by_id = {c["id"]: c for c in other.doctor()["checks"]}
    assert by_id["port"]["status"] == "fail" and by_id["port"]["code"] == "WC104"


def test_doctor_online_probes_a_dead_provider(tmp_path, monkeypatch):
    dead = _free_port()
    app = WebCortex("probe", port=_free_port())
    app.provider("dead", base_url=f"http://127.0.0.1:{dead}/v1")
    app.agent("a", model="dead/m", token_budget=100)
    monkeypatch.delenv("WEBCORTEX_FAKE_PROVIDER", raising=False)
    report = app.doctor(online=True)
    probe = next(c for c in report["checks"] if c["id"].startswith("online:"))
    assert probe["status"] == "fail" and probe["code"] == "WC303"
    assert report["ok"] is False


# --- the catalogue is the single source of truth -------------------------------------


def test_every_code_used_anywhere_is_catalogued_and_documented():
    known = {e["code"] for e in errors.catalogue()}
    used = set()
    for folder, pattern in ((ROOT / "python" / "webcortex", "*.py"), (ROOT / "crates", "*.rs")):
        for path in folder.rglob(pattern):
            if "target" in path.parts:
                continue
            used |= set(re.findall(r'"(WC\d{3})"', path.read_text()))
    unknown = used - known
    assert not unknown, f"codes used but not catalogued: {sorted(unknown)}"
    doc = (ROOT / "docs" / "error-codes.md").read_text()
    missing = [c for c in sorted(known) if f"{{#{c.lower()}}}" not in doc]
    assert not missing, f"docs/error-codes.md lacks {missing}"


def test_status_defaults_agree_with_the_catalogue():
    for status, code in errors._STATUS_DEFAULTS.items():
        entry = errors.lookup(code)
        assert entry is not None and entry["status"] in (status, None), (status, code)
    assert errors.code_for_status(418) == "WC200"


# --- 2.5.1: found by black-box testing the 2.5.0 wheel ---------------------------------


def test_a_failed_run_is_recorded_under_its_own_code(server):
    # 2.5.0 sent no x-webcortex-error on a failed run's 502, so the status view
    # filed the HTTP failure as the generic WC300 next to the run's WC304.
    base, _, _ = server
    rid = f"diag-run-{time.time_ns()}"
    s, h, out = _request(base, "/agents/desk", "POST", {"input": "fail:anthropic 529: overloaded"},
                         key=USER_KEY, headers={"x-request-id": rid})
    assert s == 502 and out["code"] == "WC304"
    assert h.get("x-webcortex-error") == "WC304"
    hit = [e for e in _status(base)["errors"]["recent"] if e["request_id"] == rid]
    assert hit and hit[0]["code"] == "WC304" and hit[0]["status"] == 502


def test_no_percentile_exceeds_the_maximum(server):
    # 2.5.0 printed "p95 0.256ms  max 0.162ms": bucket upper bounds, uncapped.
    base, _, _ = server
    for _ in range(5):
        _request(base, "/ping", key=USER_KEY)
    st = _status(base)["requests"]
    for row in [st["total"], *st["routes"]]:
        for p in ("p50_ms", "p95_ms", "p99_ms"):
            assert row[p] <= row["max_ms"], row


def test_sockets_carry_codes_and_count_against_their_route(server):
    from webcortex.client import AgentSocket, WebSocket

    base, _, _ = server
    ws = WebSocket(base + "/devices/cam/ws", headers={"x-api-key": ADMIN_KEY}, timeout=10)
    try:
        assert ws.recv_json()["type"] == "ready"
        ws.send_text("not json")
        err = ws.recv_json()
        assert err["type"] == "error" and err["code"] == "WC505" and err["hint"] == errors.hint("WC505")
    finally:
        ws.close()

    with AgentSocket(base, "/agents/desk", key=USER_KEY, timeout=30) as agent:
        agent.ws.send_text("{}")
        err = agent.ws.recv_json()
        while err.get("type") != "error":
            err = agent.ws.recv_json()
        assert err["code"] == "WC306"
        assert agent.ask("hi")["status"] == "completed"

    routes = {r["route"]: r for r in _status(base)["requests"]["routes"]}
    # An agent's socket rides on its POST route; 2.5.0 counted it as "(no route)".
    assert routes["POST /agents/desk"]["status"]["1xx"] >= 1
    assert routes.get("(no route)", {"status": {"1xx": 0}})["status"]["1xx"] == 0


def test_server_logs_are_plain_text_when_not_on_a_terminal(server):
    _, workdir, _ = server
    log = (workdir / "server.log").read_text()
    assert "WARN" in log and "\x1b[" not in log


def test_commands_report_an_invalid_app_with_a_code_not_a_traceback(tmp_path, capsys, monkeypatch):
    monkeypatch.chdir(tmp_path)
    _write(tmp_path, "from webcortex import WebCortex\napp = WebCortex('x')\n"
                     "app.agent('a', tools=['no_such_tool'])\n")
    for command in (["tools"], ["typegen"]):
        assert main(command) == 1, command
        err = capsys.readouterr().err
        assert err.startswith("error[WC101]") and "no_such_tool" in err


def test_a_syntax_error_names_the_file_and_line(tmp_path, capsys, monkeypatch):
    monkeypatch.chdir(tmp_path)
    _write(tmp_path, "from webcortex import WebCortex\napp = WebCortex('x')\ndef broken(:\n")
    assert main(["check"]) == 1
    message = json.loads(capsys.readouterr().out)["error"]["message"]
    assert "api.py:3" in message and "importlib" not in message


def test_a_handler_name_in_tools_suggests_the_derived_tool_name(tmp_path):
    app = WebCortex("t")

    @app.post("/arm/move", tool=True)
    def move(joint: int) -> dict:
        return {}

    app.agent("mover", tools=["move"])
    with pytest.raises(ValueError, match='Did you mean "create_arm_move"'):
        app.check()


class _NotWebCortex:
    """A local HTTP server answering every GET with HTML, like a wrong --url."""

    def __enter__(self):
        import http.server
        import threading

        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                body = b"<html>not webcortex</html>"
                self.send_response(200 if not self.path.endswith("/missing") else 404)
                self.send_header("content-type", "text/html")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *a):
                pass

        self.httpd = http.server.HTTPServer(("127.0.0.1", 0), H)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        return f"http://127.0.0.1:{self.httpd.server_address[1]}"

    def __exit__(self, *exc):
        self.httpd.shutdown()


def test_status_and_doctor_against_something_that_is_not_webcortex(tmp_path, capsys, monkeypatch):
    monkeypatch.chdir(tmp_path)
    _write(tmp_path, f"from webcortex import WebCortex\napp = WebCortex('mine', port={_free_port()})\n")
    with _NotWebCortex() as url:
        # 2.5.0: a JSONDecodeError traceback.
        assert main(["status", "--json", "--url", url]) == 1
        err = json.loads(capsys.readouterr().out)["error"]
        assert err["code"] == "WC108" and "not with a WebCortex status document" in err["message"]
        assert main(["status", "--url", url]) == 1
        assert "Traceback" not in capsys.readouterr().err
        # 2.5.0: "nothing answered", while something plainly did.
        main(["doctor", "--json", "--url", url])
        running = next(c for c in json.loads(capsys.readouterr().out)["checks"] if c["id"] == "running")
        assert running["status"] == "warn" and "not a WebCortex app" in running["message"]


def test_the_budget_warning_does_not_claim_a_budget_was_exhausted(tmp_path, monkeypatch):
    monkeypatch.setenv("WEBCORTEX_FAKE_PROVIDER", "1")
    app = WebCortex("b", port=_free_port())
    app.agent("a")
    budgets = next(c for c in app.doctor()["checks"] if c["id"] == "budgets")
    assert budgets["status"] == "warn" and "code" not in budgets


def test_doctor_online_probes_providers_concurrently(monkeypatch):
    from webcortex.diagnostics import doctor

    # Sockets that accept a connection (the backlog does) but never answer.
    silent = []
    for _ in range(3):
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        s.listen(8)
        silent.append(s)
    try:
        app = WebCortex("probe", port=_free_port())
        for i, s in enumerate(silent):
            app.provider(f"p{i}", base_url=f"http://127.0.0.1:{s.getsockname()[1]}/v1")
            app.agent(f"a{i}", model=f"p{i}/m", token_budget=100)
        monkeypatch.delenv("WEBCORTEX_FAKE_PROVIDER", raising=False)
        started = time.monotonic()
        report = doctor(app, online=True, timeout=1.0)
        elapsed = time.monotonic() - started
        probes = [c for c in report["checks"] if c["id"].startswith("online:")]
        assert len(probes) == 3 and all(c["code"] == "WC303" for c in probes)
        assert elapsed < 2.5, f"three 1 s probes took {elapsed:.1f}s: run one after another"
    finally:
        for s in silent:
            s.close()


def _run_cli(workdir, args, env_extra, wait_for=None, timeout=30):
    env = {**os.environ, "WEBCORTEX_LOG": "warn", **env_extra}
    proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", *args], cwd=workdir,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
    if wait_for is None:
        out, err = proc.communicate(timeout=timeout)
        return proc.returncode, out, err
    deadline = time.time() + timeout
    ok = False
    while time.time() < deadline and proc.poll() is None:
        try:
            with urllib.request.urlopen(wait_for, timeout=1):
                ok = True
                break
        except Exception:
            time.sleep(0.1)
    proc.terminate()
    out, err = proc.communicate(timeout=10)
    return ok, out, err


def test_run_on_a_busy_port_is_wc104_not_a_traceback(tmp_path):
    with socket.socket() as busy:
        busy.bind(("127.0.0.1", 0))
        busy.listen(1)
        port = busy.getsockname()[1]
        (tmp_path / "api.py").write_text(f"from webcortex import WebCortex\napp = WebCortex('b', port={port})\n")
        code, _, err = _run_cli(tmp_path, ["run"], {})
    assert code == 1
    assert "error[WC104]" in err and "Traceback" not in err


def test_the_port_flag_wins_over_the_environment_and_the_banner_names_both_versions(tmp_path):
    import webcortex

    want, other = _free_port(), _free_port()
    (tmp_path / "api.py").write_text("from webcortex import WebCortex\napp = WebCortex('flagged', version='9.9.9')\n")
    ok, out, err = _run_cli(tmp_path, ["run", "--port", str(want)], {"WEBCORTEX_PORT": str(other)},
                            wait_for=f"http://127.0.0.1:{want}/_webcortex/health")
    assert ok, err
    assert f"webcortex {webcortex.__version__}  ·  flagged 9.9.9" in out
    assert f"127.0.0.1:{want}" in out


def test_starters_point_at_doctor():
    from webcortex import starters
    readme = starters.files_for("hub", "demo", "a demo")["README.md"]
    assert "webcortex doctor" in readme and "webcortex status" in readme
