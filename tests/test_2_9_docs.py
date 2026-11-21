"""2.9: the docs say what the runtime does.

The 2.9 stress test found the tutorial, the use cases and the reference pages
out of step with the code: route counts from an older app, a curl that left out
the key and so could never print the 422 it promised, a 503 where the runtime
answers 502, an admin-panel example that did not boot, reference pages missing
parameters and a whole command. These tests build the apps the pages describe,
straight from their code blocks, and compare what the pages claim with what
the app reports, so the next drift fails CI instead of a reader.

The server-backed checks run against the deterministic fake provider or with
no provider at all; nothing leaves the machine.
"""

from __future__ import annotations

import importlib.util
import inspect
import json
import os
import re
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from webcortex import WebCortex, cli
from webcortex.starters import TEMPLATES

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"
KEY = "wcx_docs-test-key-0123456789abcdefghijklmnop"

# Passed only by the 2.9.0 core (batch A, L36: a gated call the run could never
# make is refused at once instead of becoming an approval). Strict, so the merge
# that brings the fix turns this into an unexpected pass and the marker comes off.
NEEDS_2_9_CORE = pytest.mark.xfail(
    strict=True, reason="needs the 2.9.0 core: WC602 before an approval is created")
# The same for the Python halves that batches C and D add. Only a missing name
# counts as the expected failure; a wrong number in a page still fails.
NEEDS_BATCH_C = pytest.mark.xfail(
    strict=True, raises=AttributeError, reason="needs batch C: webcortex._bridge.behaviour_limit")
NEEDS_BATCH_D = pytest.mark.xfail(
    strict=True, raises=AttributeError, reason="needs batch D: webcortex.cli.route_table")


# ---------------------------------------------------------------- helpers


def _read(name: str) -> str:
    return (ROOT / name if name == "README.md" else DOCS / name).read_text()


def _blocks(text: str, lang: str) -> list[tuple[str, str]]:
    """Every fenced block of one language, as (attributes, body)."""
    return [(attrs.strip(), body)
            for found, attrs, body in re.findall(r"^```(\w+)([^\n]*)\n(.*?)^```", text, re.S | re.M)
            if found == lang]


def _section(text: str, heading: str) -> str:
    """The text from a `## heading` to the next `## `."""
    start = text.index(heading)
    end = text.find("\n## ", start + len(heading))
    return text[start:] if end == -1 else text[start:end]


def _load(path: Path):
    spec = importlib.util.spec_from_file_location(f"docs_{path.stem}_{id(path)}", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.app


def _typescript_methods(code: str) -> int:
    # The count `webcortex typegen` prints; kept identical so the doc quotes it.
    return code.count("): Promise<")


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _request(base, path, method="GET", body=None, key=None):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"content-type": "application/json"} if data else {}
    if key:
        headers["x-api-key"] = key
    req = urllib.request.Request(base + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read()
            return r.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        raw = e.read()
        return e.code, json.loads(raw) if raw else None


class _Server:
    """`webcortex run` on a free port, with a chosen environment."""

    def __init__(self, workdir: Path, target: str, env: dict[str, str]):
        self.port = _free_port()
        self.base = f"http://127.0.0.1:{self.port}"
        self.log = workdir / f"{Path(target).stem}.log"
        full = {k: v for k, v in os.environ.items()
                if k not in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "WEBCORTEX_FAKE_PROVIDER",
                             "WEBCORTEX_PORT", "WEBCORTEX_HOST", "WEBCORTEX_API_KEY")}
        full.update({"WEBCORTEX_LOG": "warn", **env})
        with self.log.open("w") as log:
            self.proc = subprocess.Popen(
                [sys.executable, "-m", "webcortex.cli", "run", target, "--port", str(self.port),
                 "--host", "127.0.0.1"],
                cwd=workdir, stdout=log, stderr=subprocess.STDOUT, env=full)
        deadline = time.time() + 45
        while time.time() < deadline:
            if self.proc.poll() is not None:
                pytest.fail(f"server exited early:\n{self.log.read_text()}")
            try:
                with urllib.request.urlopen(self.base + "/_webcortex/health", timeout=1):
                    return
            except Exception:
                time.sleep(0.1)
        self.stop()
        pytest.fail(f"server never became ready:\n{self.log.read_text()}")

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.proc.kill()


# ---------------------------------------------------------------- the tutorial


@pytest.fixture(scope="module")
def tutorial(tmp_path_factory):
    """The tutorial's app after steps 2, 3, 5, 6 and 7, built from its own code blocks."""
    doc = _read("tutorial.md")
    py = [body for _, body in _blocks(doc, "python")]
    assert len(py) == 5, "the tutorial's python blocks moved; update this fixture"
    base, handler, purge, agent, page = py
    with_templates = base.replace('    database="sqlite://./supportdesk.db",\n)',
                                  '    database="sqlite://./supportdesk.db",\n    templates="templates",\n)')
    assert with_templates != base, "step 7 says to add templates= to the constructor"
    stages = {
        "step2": base,
        "step3": base + "\n" + handler,
        "step5": base + "\n" + handler + "\n" + purge,
        "step6": base + "\n" + handler + "\n" + purge + "\n" + agent,
        "step7": with_templates + "\n" + handler + "\n" + purge + "\n" + agent + "\n" + page,
    }
    workdir = tmp_path_factory.mktemp("tutorial")
    template = [body for attrs, body in _blocks(doc, "html") if "templates/index.html" in attrs]
    (workdir / "templates").mkdir()
    (workdir / "templates" / "index.html").write_text(template[0])
    apps = {}
    with pytest.MonkeyPatch.context() as mp:
        mp.chdir(workdir)
        mp.setenv("WEBCORTEX_API_KEY", KEY)
        for name, source in stages.items():
            (workdir / f"{name}.py").write_text(source)
            apps[name] = _load(workdir / f"{name}.py")
    return {"doc": doc, "dir": workdir, **apps}


def test_step_2_banner_matches_the_app(tutorial):
    doc, app = tutorial["doc"], tutorial["step2"]
    report, security = app.check(), app.security_report()
    banner = _blocks(_section(doc, "## 2."), "console")[0][1]

    routes, native = map(int, re.search(r"(\d+) routes, (\d+) served without touching Python", banner).groups())
    assert (routes, native) == (report["routes"], report["native_routes"])
    bold = re.search(r"\*\*(\d+) of (\d+) routes served without touching Python", doc)
    assert (int(bold.group(2)), int(bold.group(1))) == (report["routes"], report["native_routes"])

    count, names = re.search(r"(\d+) agent tools: (.+)", banner).groups()
    assert int(count) == len(report["tools"])
    assert [n.strip() for n in names.split(",")] == report["tools"]

    # The warning line is shown exactly when the banner would print it.
    warning = re.search(r"⚠ (\d+) route\(s\) need no credential", banner)
    assert (int(warning.group(1)) if warning else 0) == len(security["public_routes"])


def test_what_you_built_counts_match_the_finished_app(tutorial):
    report = tutorial["step7"].check()
    native, routes = map(int, re.search(r"A REST API where (\d+) of (\d+) routes never touch Python",
                                        tutorial["doc"]).groups())
    assert (routes, native) == (report["routes"], report["native_routes"])


def test_typegen_line_quotes_the_method_count_the_cli_prints(tutorial, capsys):
    out = tutorial["dir"] / "client" / "api.ts"
    with pytest.MonkeyPatch.context() as mp:
        mp.chdir(tutorial["dir"])
        mp.setenv("WEBCORTEX_API_KEY", KEY)
        assert cli.main(["typegen", "step7.py", "--out", str(out)]) == 0
    printed = re.search(r"\((\d+) typed methods", capsys.readouterr().out).group(1)
    claimed = re.search(r"Wrote client/api\.ts \((\d+) typed methods", tutorial["doc"]).group(1)
    assert claimed == printed == str(_typescript_methods(out.read_text()))


def test_security_sample_matches_the_finished_app(tutorial):
    sample = _blocks(_section(tutorial["doc"], "## 9."), "console")[0][1]
    security = tutorial["step7"].security_report()
    for key in ("public_routes", "gated_tools", "anonymous_scopes"):
        claimed = json.loads(re.search(rf'"{key}": (\[[^\]]*\])', sample).group(1))
        assert claimed == security[key], key
    agent_scopes = json.loads(re.search(r'"scopes": (\[[^\]]*\])', sample).group(1))
    assert agent_scopes == security["agents"][0]["scopes"]


def test_mcp_sample_is_marked_as_an_excerpt_and_matches_the_route(tutorial):
    section = _section(tutorial["doc"], "## 4.")
    assert "excerpt" in section.lower()
    sample = json.loads(_blocks(section, "json")[0][1])
    [tool] = sample["result"]["tools"]
    app = tutorial["step3"]
    assert tool["name"] in app.check()["tools"]
    route = next(r for r in app.manifest()["routes"] if r["path"] == "/tickets/{id}/summary")
    assert tool["inputSchema"] == route["input_schema"]
    assert tool["description"] == route["description"]


def test_curls_that_expect_success_send_the_key():
    doc = _read("tutorial.md")
    for _, block in _blocks(doc, "console"):
        lines = block.replace("\\\n", " ").splitlines()
        for i, line in enumerate(lines):
            if not line.startswith("$ curl"):
                continue
            output = lines[i + 1].strip() if i + 1 < len(lines) else ""
            if output == "401":
                assert "x-api-key" not in line  # the one that demonstrates the 401
            else:
                assert "x-api-key" in line, line


def test_tutorial_says_restart_and_quotes_the_real_status_codes():
    doc = _read("tutorial.md")
    assert "503" not in doc
    step6 = _section(doc, "## 6.")
    assert "502" in step6 and "WC301" in step6
    step3 = _section(doc, "## 3.")
    assert "start it again" in step3 and "does not watch" in step3
    # The typed-parameter promise is qualified (L9).
    assert "unchanged" in step3 and "arrives as an `int`" not in doc


def test_the_agent_can_propose_every_gated_tool_it_lists(tutorial):
    """An agent can only propose a gated call its own scopes allow (2.9: WC602 otherwise)."""
    _assert_agents_hold_their_gated_tools(tutorial["step7"])


@pytest.fixture(scope="module")
def tutorial_server(tutorial):
    server = _Server(tutorial["dir"], "step7.py", {"WEBCORTEX_API_KEY": KEY})
    yield server
    server.stop()


def test_tutorial_requests_answer_as_the_page_says(tutorial_server):
    base = tutorial_server.base
    # Step 2: no key, 401.
    assert _request(base, "/tickets")[0] == 401
    # Step 3: bad input with the key is a 422; the same request without it is a 401.
    status, body = _request(base, "/tickets/1/summary?style=sideways", key=KEY)
    assert status == 422 and body["error"]["code"] == "WC206"
    assert _request(base, "/tickets/1/summary?style=sideways")[0] == 401
    # Step 3 and routing.md: an unconvertible value reaches the handler unchanged.
    status, body = _request(base, "/tickets/abc/summary", key=KEY)
    assert status == 200 and body["id"] == "abc"
    # Step 5: the gate holds over MCP, not over plain HTTP.
    status, body = _request(base, "/_webcortex/mcp", "POST", {
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "create_tickets_purge", "arguments": {"confirm": True}}}, key=KEY)
    assert body["result"]["isError"] and body["result"]["structuredContent"]["error"]["code"] == "WC309"
    assert _request(base, "/tickets/purge", "POST", {"confirm": True}, key=KEY) == (200, {"purged": True})
    # Step 6: no provider key, so /ask is a 502 with WC301 (not a 503).
    status, body = _request(base, "/ask", "POST", {"input": "What tickets are open?"}, key=KEY)
    assert status == 502 and body["code"] == "WC301"


def test_mcp_excerpt_is_what_the_server_sends(tutorial, tutorial_server):
    sample = json.loads(_blocks(_section(tutorial["doc"], "## 4."), "json")[0][1])
    [claimed] = sample["result"]["tools"]
    status, body = _request(tutorial_server.base, "/_webcortex/mcp", "POST",
                            {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}, key=KEY)
    assert status == 200 and {"id", "jsonrpc", "result"} <= set(body)
    sent = next(t for t in body["result"]["tools"] if t["name"] == claimed["name"])
    assert sent == claimed
    assert len(body["result"]["tools"]) > 1  # so the page is right to call it an excerpt


def test_the_approval_flow_in_step_6_works(tutorial):
    """An admin caller's run suspends on the purge, and approving it runs the purge."""
    app = tutorial["step7"]
    app.eval("purge_waits_for_a_human", target="assistant",
             input='tool:create_tickets_purge {"confirm": true}', scopes=["read", "admin"],
             expect={"status": "awaiting_approval", "tools_not_called": ["create_tickets_purge"]})
    with pytest.MonkeyPatch.context() as mp:
        mp.chdir(tutorial["dir"])
        mp.setenv("WEBCORTEX_FAKE_PROVIDER", "1")
        mp.setenv("WEBCORTEX_DATABASE_URL", "sqlite://:memory:")
        report = app.run_evals(mode="live", only=["purge_waits_for_a_human"])
    assert report["ok"], report["results"][0]["failures"]


@NEEDS_2_9_CORE
def test_a_read_only_caller_cannot_even_propose_a_purge(tutorial):
    app = tutorial["step7"]
    app.eval("reader_cannot_propose", target="assistant",
             input='tool:create_tickets_purge {"confirm": true}', scopes=["read"])
    with pytest.MonkeyPatch.context() as mp:
        mp.chdir(tutorial["dir"])
        mp.setenv("WEBCORTEX_FAKE_PROVIDER", "1")
        mp.setenv("WEBCORTEX_DATABASE_URL", "sqlite://:memory:")
        report = app.run_evals(mode="live", only=["reader_cannot_propose"])
    body = report["results"][0]["result"]
    assert body["status"] != "awaiting_approval"
    kinds = [s.get("kind") for s in body["steps"]]
    assert "approval_requested" not in kinds
    assert any(s.get("tool") == "create_tickets_purge" and s.get("code") == "WC602" for s in body["steps"])


# ---------------------------------------------------------------- use cases


def _assert_agents_hold_their_gated_tools(app: WebCortex) -> None:
    manifest = app.manifest()
    gated = {}
    for r in manifest["routes"]:
        if r.get("approval") == "required":
            name = r["tool"]["name"] or cli_tool_name(r)
            gated[name] = set(r.get("scopes") or [])
    for agent in manifest["agents"]:
        held = set(agent.get("scopes") or [])
        for tool in agent.get("tools", []):
            if tool in gated and held:
                assert gated[tool] <= held, (
                    f"agent {agent['name']!r} lists gated {tool!r} (needs {sorted(gated[tool])}) "
                    f"but holds {sorted(held)}: it could never propose it")


def cli_tool_name(route: dict) -> str:
    from webcortex.app import _derive_tool_name
    return _derive_tool_name(route["method"], route["path"])


@pytest.fixture(scope="module")
def use_case_apps(tmp_path_factory):
    doc = _read("use-cases.md")
    workdir = tmp_path_factory.mktemp("use-cases")
    (workdir / "templates").mkdir()
    (workdir / "static").mkdir()
    for attrs, body in _blocks(doc, "html"):
        m = re.search(r'title="templates/([^"]+)"', attrs)
        if m:
            (workdir / "templates" / m.group(1)).write_text(body)
    apps = {}
    complete = [body for _, body in _blocks(doc, "python") if body.startswith("from webcortex import")]
    assert len(complete) == 4, "a use case was added or lost its imports; update this test"
    with pytest.MonkeyPatch.context() as mp:
        mp.chdir(workdir)
        for name in ("OPS_KEY", "VIEWER_KEY", "CUSTOMER_KEY", "ADMIN_KEY", "PARTNER_KEY",
                     "BILLING_TOKEN", "CATALOG_TOKEN"):
            mp.setenv(name, KEY + name.lower())
        mp.setenv("JWT_SECRET", "x" * 40)
        for i, body in enumerate(complete):
            path = workdir / f"use_case_{i}.py"
            path.write_text(body)
            app = _load(path)
            apps[app.name] = app
    return apps


def test_every_complete_use_case_boots(use_case_apps):
    assert set(use_case_apps) == {"deployhub", "marketdata", "gateway", "admin"}
    for name, app in use_case_apps.items():
        app.check()  # raises on an invalid app, as `webcortex check` would


def test_admin_panel_detail_page_does_not_collide_with_the_resource(use_case_apps):
    routes = {(r["method"], r["path"]) for r in use_case_apps["admin"].manifest()["routes"]}
    assert ("GET", "/users/{id}") in routes and ("GET", "/admin/users/{id}") in routes
    assert 'href="/admin/users/{{ u.id }}"' in _read("use-cases.md")


def test_use_case_agents_can_propose_their_gated_tools(use_case_apps):
    for app in use_case_apps.values():
        _assert_agents_hold_their_gated_tools(app)


# ---------------------------------------------------------------- reference pages


def test_installation_lists_every_starter():
    doc = _read("installation.md")
    assert "Five starters" not in doc
    for template in TEMPLATES:
        assert f"| `{template}` |" in doc, template
    cli_doc = _read("cli.md")
    assert " · ".join(f"`{t}`" for t in TEMPLATES) in cli_doc
    readme = _read("README.md")
    assert " | ".join(TEMPLATES) in readme


def test_api_reference_names_every_public_parameter():
    doc = _read("api-reference.md")
    missing = []
    for name, member in inspect.getmembers(WebCortex):
        if name.startswith("_") and name != "__init__":
            continue
        if not callable(member) or isinstance(inspect.getattr_static(WebCortex, name), property):
            continue
        if name != "__init__" and not re.search(rf"\b{re.escape(name)}\b", doc):
            missing.append(f"app.{name}")
        for pname, p in inspect.signature(member).parameters.items():
            if pname == "self" or p.kind in (p.VAR_KEYWORD, p.VAR_POSITIONAL):
                continue
            if not re.search(rf"\b{re.escape(pname)}\b", doc):
                missing.append(f"{name}({pname}=)")
    assert not missing, f"docs/api-reference.md does not mention: {missing}"


@NEEDS_BATCH_D
def test_check_route_table_sample_is_what_check_prints(tutorial):
    section = _section(_read("cli.md"), "## `webcortex check`")
    claimed = [json.loads(line.strip().rstrip(","))
               for line in _blocks(section, "console")[0][1].splitlines()
               if line.strip().startswith('{"method"')]
    assert claimed, "the check sample shows no route_table rows"
    assert "`route_table`" in section
    rows = cli.route_table(tutorial["step7"])
    by_route = {(r["method"], r["path"]): r for r in rows}
    for row in claimed:
        assert set(row) == set(rows[0])
        assert by_route[(row["method"], row["path"])] == row


@NEEDS_BATCH_C
def test_behaviour_pool_limit_is_what_the_pages_say(monkeypatch):
    for page in ("behaviours.md", "security.md", "deployment.md"):
        doc = _read(page)
        for phrase in ("max(32, workers × 8)", "WEBCORTEX_MAX_BEHAVIOURS", "WC215"):
            assert phrase in doc, (page, phrase)
    from webcortex import _bridge
    monkeypatch.delenv("WEBCORTEX_MAX_BEHAVIOURS", raising=False)
    for workers in (1, 2, 4, 8, 16):
        assert _bridge.behaviour_limit(workers) == max(32, workers * 8)
    monkeypatch.setenv("WEBCORTEX_MAX_BEHAVIOURS", "5")
    assert _bridge.behaviour_limit(4) == 5


def test_json_filter_advice_quotes_attributes_with_single_quotes():
    filters = _section(_read("frontend.md"), "### Filters")
    assert "single-quoted" in filters
    assert "='{{ ticket | json }}'" in filters
    assert re.search(r"never `[^`]*=\"\{\{ ticket \| json \}\}\"`", filters)


def test_cli_reference_has_a_section_for_every_command():
    doc = _read("cli.md")
    headings = " ".join(re.findall(r"^## (.+)$", doc, re.M))
    parser = cli._parser()
    sub = next(a for a in parser._actions if a.__class__.__name__ == "_SubParsersAction")
    missing = [name for name in sub.choices if f"`webcortex {name}`" not in headings]
    assert not missing, f"docs/cli.md has no section for: {missing}"


def test_device_hub_documents_only_on_change_and_per_device_scopes():
    doc = _read("device-hub.md")
    assert "only_on_change" in doc
    section = _section(doc, "### One key per device") if "### One key per device" in doc else ""
    assert 'ingest_scopes=["devices:ingest:dock"]' in section


@pytest.fixture(scope="module")
def hub_server(tmp_path_factory):
    """device-hub.md's per-device example, served."""
    doc = _read("device-hub.md")
    snippet = next(body for _, body in _blocks(doc, "python") if "devices:ingest:dock" in body)
    workdir = tmp_path_factory.mktemp("hub")
    (workdir / "api.py").write_text("from webcortex import WebCortex\napp = WebCortex('hub')\n" + snippet)
    server = _Server(workdir, "api.py", {"DOCK_CAMERA_KEY": KEY + "dock", "VAULT_CAMERA_KEY": KEY + "vault"})
    yield server
    server.stop()


def test_a_per_device_scope_binds_the_key_to_its_device(hub_server):
    reading = {"temp_c": 4}
    dock, vault = KEY + "dock", KEY + "vault"
    assert _request(hub_server.base, "/devices/dock/telemetry", "POST", reading, key=dock)[0] < 300
    assert _request(hub_server.base, "/devices/vault/telemetry", "POST", reading, key=dock)[0] == 403
    assert _request(hub_server.base, "/devices/vault/telemetry", "POST", reading, key=vault)[0] < 300


# ---------------------------------------------------------------- README


@pytest.fixture(scope="module")
def readme_app(tmp_path_factory):
    readme = _read("README.md")
    py = [body for _, body in _blocks(readme, "python")]
    workdir = tmp_path_factory.mktemp("readme")
    (workdir / "api.py").write_text(py[0] + "\n" + py[1])
    with pytest.MonkeyPatch.context() as mp:
        mp.chdir(workdir)
        mp.setenv("WEBCORTEX_API_KEY", KEY)
        return _load(workdir / "api.py")


def test_readme_flagship_runs_are_budgeted(readme_app):
    """'The whole run is bounded by one budget': every agent and flow has one."""
    manifest = readme_app.manifest()
    for agent in manifest["agents"]:
        assert agent.get("token_budget"), agent["name"]
    for flow in manifest["flows"]:
        assert flow.get("token_budget"), flow["name"]
    # And the page says out loud that anonymous callers can start them.
    multi_agent = _blocks(_read("README.md"), "python")[1][1]
    assert "anonymous_scopes" in multi_agent and "/ask" in multi_agent


def test_readme_behaviour_example_is_scoped():
    readme = _read("README.md")
    triage = next(body for _, body in _blocks(readme, "python") if '@app.behaviour("triage"' in body)
    assert re.search(r"scopes=\[[^\]]+\]", triage)


# ---------------------------------------------------------------- the 2.9 behaviour changes


def test_security_and_agent_pages_describe_2_9():
    security = _read("security.md")
    for phrase in ("ES256", "ES384", "EdDSA", "api_key_header", "WC401", "WC603", "anonymous_scopes"):
        assert phrase in security, phrase
    assert "boot error" in _section(_read("agents.md"), "## Approval gates")
    assert "boot error" in _read("orchestration.md")
    assert "WC318" in _section(_read("behaviours.md"), "### `ctx.ask(")
    assert "## Review — 2.9.0" in (ROOT / "SECURITY.md").read_text()
