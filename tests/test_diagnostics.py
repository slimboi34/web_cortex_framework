"""2.5: stable error codes with hints, the live status view, `webcortex doctor`,
`webcortex status` and `webcortex explain` — over a real server where it matters."""
from __future__ import annotations

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

APP = '''
from webcortex import WebCortex, HTTPError

app = WebCortex("diag", port={port}, start_halted=True)
app.api_key("DIAG_ADMIN_KEY", id="admin", scopes=["read", "write", "webcortex:admin"])
app.api_key("DIAG_USER_KEY", id="user", scopes=["read"])
app.cors("https://good.example")

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
           "DIAG_ADMIN_KEY": ADMIN_KEY, "DIAG_USER_KEY": USER_KEY}
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
