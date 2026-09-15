"""Orchestration: handoffs, sessions, approvals that resume, flows, memory,
context providers, concurrent leaves, and the spend ledger.

The live tests run a real server against the deterministic fake provider
(`WEBCORTEX_FAKE_PROVIDER=1`), so the *whole* stack — HTTP, MCP, sessions,
handoffs, approval resume, budgets — is exercised without a network or a key.
That provider answers with a tiny command language; see provider.rs.
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

from webcortex import WebCortex


def make_app(**kw) -> WebCortex:
    return WebCortex("t", database="sqlite://:memory:", **kw)


# ---------------------------------------------------------------- declarations


def test_every_agent_is_a_tool_named_after_itself():
    app = make_app()
    app.agent("helper", description="Helps.")
    report = app.check()
    assert "helper" in report["tools"]
    route = next(r for r in app.manifest()["routes"] if r["op"]["kind"] == "agent")
    assert route["path"] == "/agents/helper"
    assert route["input_schema"]["properties"]["session_id"]["type"] == "string"
    assert route["input_schema"]["properties"]["reset"]["type"] == "boolean"


def test_a_supervisor_may_list_another_agent_as_a_tool():
    app = make_app()
    app.agent("worker")
    app.agent("boss", tools=["worker"])
    assert set(app.check()["agents"]) == {"worker", "boss"}


def test_handoff_to_an_undeclared_agent_is_a_boot_error_with_a_hint():
    app = make_app()
    app.agent("billing")
    app.agent("desk", handoffs=["biling"])
    with pytest.raises(ValueError, match='Did you mean "billing"'):
        app.check()


def test_a_tool_named_like_a_handoff_is_a_boot_error():
    app = make_app()
    app.static("GET", "/t", {"ok": True}, tool=True, tool_name="transfer_to_billing")
    app.agent("billing")
    app.agent("desk", tools=["transfer_to_billing"], handoffs=["billing"])
    with pytest.raises(ValueError, match="collides"):
        app.check()


def test_an_agent_cannot_hand_off_to_itself():
    app = make_app()
    app.agent("loop", handoffs=["loop"])
    with pytest.raises(ValueError, match="itself"):
        app.check()


def test_agent_model_defaults_to_the_default_alias():
    app = make_app()
    app.agent("a")
    assert app.manifest()["agents"][0]["model"] == "default"


def test_models_declares_aliases_providers_and_prices():
    app = make_app()
    app.models(fast="ollama/qwen3.5:9b", local="fast")
    app.provider("groq", base_url="https://api.groq.com/openai/v1", api_key_env="GROQ_API_KEY")
    app.pricing("claude-opus-5", input_per_mtok=15, output_per_mtok=75)
    m = app.manifest()["models"]
    assert m["aliases"] == {"fast": "ollama/qwen3.5:9b", "local": "fast"}
    assert m["providers"]["groq"]["kind"] == "openai"
    assert m["pricing"]["claude-opus-5"]["output_per_mtok"] == 75.0
    with pytest.raises(ValueError, match="http"):
        app.provider("bad", base_url="not-a-url")
    with pytest.raises(ValueError, match="kind"):
        app.provider("bad", base_url="https://x", kind="magic")


def test_context_providers_have_three_sources_and_are_validated():
    app = make_app()
    app.context("policy", data={"limit": 5})
    app.context("recent", sql="SELECT 1 AS one", description="a query")

    @app.context("account")
    def account(req) -> dict:
        """Who is asking."""
        return {"id": req.user["id"]}

    kinds = {c["name"]: c["source"]["kind"] for c in app.manifest()["contexts"]}
    assert kinds == {"policy": "static", "recent": "query", "account": "python"}
    assert next(c for c in app.manifest()["contexts"] if c["name"] == "account")["description"] == "Who is asking."

    app.agent("a", context=["policy", "ghost"])
    with pytest.raises(ValueError, match='ghost'):
        app.check()


def test_context_without_a_database_cannot_use_sql():
    app = WebCortex("t")
    with pytest.raises(ValueError, match="database"):
        app.context("x", sql="SELECT 1")
    with pytest.raises(ValueError, match="not both"):
        make_app().context("x", sql="SELECT 1", data={})


def test_memory_declares_four_rust_tools_and_a_table():
    app = make_app()
    tools = app.memory("notes", scopes=["read"])
    assert tools == ["notes_remember", "notes_recall", "notes_search", "notes_forget"]
    report = app.check()
    assert set(tools) <= set(report["tools"])
    assert report["native_routes"] == report["routes"], "memory must never compile to Python"
    assert "CREATE TABLE IF NOT EXISTS wcx_notes" in app.schema_sql
    assert "PRIMARY KEY (principal, key)" in app.schema_sql
    with pytest.raises(ValueError, match="already"):
        app.memory("notes")


def test_agent_memory_shorthand_adds_the_tools_and_a_hint():
    app = make_app()
    app.memory("notes")
    app.agent("a", memory="notes", system="Be brief.", tools=["notes_recall"])
    a = app.manifest()["agents"][0]
    assert a["tools"] == ["notes_recall", "notes_remember", "notes_search", "notes_forget"]
    assert "notes_remember" in a["system"] and a["system"].startswith("Be brief.")
    with pytest.raises(ValueError, match="not declared"):
        app.agent("b", memory="ghost")


def test_flows_take_exactly_one_shape_and_validate_their_steps():
    app = make_app()
    app.static("GET", "/p", {"ok": True}, tool=True, tool_name="ping")
    with pytest.raises(ValueError, match="exactly one"):
        app.flow("x", pipeline=["ping"], parallel=["ping"])
    with pytest.raises(ValueError, match="exactly one"):
        app.flow("x")
    with pytest.raises(ValueError, match="merge"):
        app.flow("x", parallel=["ping"], merge="zip")
    app.flow("bad", pipeline=["pong"])
    with pytest.raises(ValueError, match='Did you mean "ping"'):
        app.check()


def test_a_flow_is_a_tool_and_a_route():
    app = make_app()
    app.static("GET", "/p", {"ok": True}, tool=True, tool_name="ping")
    app.flow("twice", pipeline=["ping", {"tool": "ping", "input": {"x": "$"}}], scopes=["read"])
    report = app.check()
    assert "twice" in report["tools"]
    assert report["flows"] == ["twice"]
    route = next(r for r in app.manifest()["routes"] if r["op"]["kind"] == "flow")
    assert route["path"] == "/flows/twice"
    assert route["scopes"] == ["read"]
    assert app.openapi()["paths"]["/flows/twice"]["post"]["x-webcortex-op"] == "flow"


def test_a_flow_cannot_contain_itself():
    app = make_app()
    app.static("GET", "/p", {"ok": True}, tool=True, tool_name="ping")
    app.flow("loop", pipeline=["ping", "loop"])
    with pytest.raises(ValueError, match="itself"):
        app.check()


def test_security_report_covers_flows_and_memory():
    app = make_app()
    app.memory("notes")
    app.static("GET", "/p", {"ok": True}, tool=True, tool_name="ping")
    app.flow("f", parallel=["ping"], token_budget=10)
    app.agent("a", handoffs=[], token_budget=5)
    s = app.security_report()
    assert s["flows"] == [{"name": "f", "kind": "parallel", "scopes": [], "token_budget": 10}]
    assert s["memories"] == ["notes"]
    assert s["agents"][0]["token_budget"] == 5


def test_context_policy_is_carried_in_the_manifest():
    app = make_app()
    app.agent("a", context_window=50_000, tool_result_limit=2048, compact_with="fast", keep_recent=3, cache=False)
    a = app.manifest()["agents"][0]
    assert a["policy"] == {"max_tool_result_bytes": 2048, "max_context_tokens": 50_000,
                           "compact_with": "fast", "keep_recent": 3}
    assert a["cache"] is False
    with pytest.raises(ValueError, match="keep_recent"):
        app.agent("b", keep_recent=0)


def test_the_context_pack_describes_the_whole_app_tersely():
    app = make_app()
    app.api_key("K", id="svc", scopes=["read"])
    app.resource("books", fields={"id": int, "title": str}, tools=True, scopes=["read"])
    app.memory("notes")
    app.context("policy", data={"x": 1}, description="rules")
    app.agent("a", tools=["list_books"], handoffs=[], context=["policy"], description="reads books")
    app.flow("f", pipeline=["a"], description="one step")
    app.models(fast="ollama/qwen3.5:9b")

    @app.behaviour("b", tools=["list_books"])
    def b(ctx, input):
        """Does things."""
        return {}

    pack = app.context_pack()
    for heading in ("## Security", "## Routes", "## Tools", "## Agents", "## Behaviours",
                    "## Flows", "## Context providers", "## Memory", "## Models",
                    "## WebCortex 2 — how to extend this app"):
        assert heading in pack, heading
    assert "`list_books(limit?: integer, offset?: integer)`" in pack
    assert "**a** — reads books" in pack
    assert "f → " not in pack and "a" in pack
    assert "fast=ollama/qwen3.5:9b" in pack
    assert len(pack) < 12_000, "the pack must stay small enough to paste into a prompt"


def test_request_exposes_the_user():
    from webcortex._bridge import Request
    req = Request({"method": "GET", "path": "/", "path_params": {}, "query": {}, "headers": {},
                   "body": b"", "scopes": ["read"], "user": {"id": "u1", "authenticated": True, "scopes": ["read"]}})
    assert req.user["id"] == "u1"


# ---------------------------------------------------------------- live

ADMIN_KEY = "admin-key-for-orchestration-tests-0001"
USER_KEY = "user-key-for-orchestration-tests-00002"

APP = '''
from webcortex import WebCortex

app = WebCortex("orch", database="sqlite://./orch.db", port={port})

app.api_key("ADMIN_KEY", id="admin", scopes=["read", "write", "webcortex:admin"])
app.api_key("USER_KEY", id="user2", scopes=["read", "write"])
app.anonymous_scopes("read", "write")

app.models(fast="test-fast")
app.pricing("test-fast", input_per_mtok=1.0, output_per_mtok=2.0)

app.resource("tickets", fields={{"id": int, "body": str, "state": str}}, tools=True)
app.static("GET", "/ping", {{"pong": True}}, tool=True, tool_name="ping")
app.static("GET", "/ping2", {{"pong2": True}}, tool=True, tool_name="ping2")
app.static("GET", "/nuke", {{"nuked": True}}, tool=True, tool_name="nuke", approval="required")
app.static("GET", "/big", {{"rows": list(range(3000))}}, tool=True, tool_name="big")

app.context("policy", data={{"refund_days": 30}}, description="the rules")
app.context("recent", sql="SELECT id, body FROM tickets ORDER BY id DESC LIMIT 5")
app.context("whoami", sql="SELECT ? AS me", params=["@principal"], returns="one")

@app.context("computed")
def computed(input: str = "") -> dict:
    return {{"echo": input, "n": 42}}

notes = app.memory("notes")

app.agent("billing", description="the money one", tools=["list_tickets"], scopes=["read"],
          context=["policy"])
app.agent("front_desk", description="first contact", tools=["list_tickets", "ping", "big"],
          handoffs=["billing"], context=["policy", "recent", "whoami", "computed"],
          tool_result_limit=400, expose_at="/ask")
app.agent("danger_agent", tools=["ping", "nuke"])
app.agent("memory_agent", memory="notes")
app.agent("tight", tools=["ping"], token_budget=10)
app.agent("supervisor", tools=["billing", "ping"], token_budget=60)

app.flow("pipe", pipeline=["front_desk", "billing"])
app.flow("both", parallel=["ping", "ping2"])
app.flow("merged", parallel=["ping", "ping2"], merge="merge")
app.flow("desk", route={{"billing": "billing", "other": "front_desk"}},
         classify_prompt='Pick one. json:{{"label": "billing"}}')
app.flow("desk_default", route={{"billing": "billing"}}, default="front_desk",
         classify_prompt='json:{{"label": "nonsense"}}')
app.flow("mapped", pipeline=[{{"tool": "get_tickets", "input": {{"id": "$input.ticket"}}}},
                             {{"tool": "billing", "input": {{"input": "$.body"}}}}])


@app.behaviour("fanout", tools=["ping", "ping2", "get_tickets"])
def fanout(ctx, input):
    ok = ctx.gather(("ping", {{}}), "ping2")
    soft = ctx.gather(("get_tickets", {{"id": 999999}}), ("ping", {{}}), return_exceptions=True)
    try:
        ctx.gather(("get_tickets", {{"id": 999999}}))
        raised = False
    except RuntimeError:
        raised = True
    return {{"ok": ok, "soft": soft, "raised": raised, "steps": ctx.usage["steps"]}}


@app.behaviour("classify_all", tools=[], model="fast")
def classify_all(ctx, input):
    labels = ctx.ask_many(
        ['json:{{"label": "a"}}', 'json:{{"label": "b"}}', 'json:{{"label": "c"}}'],
        schema={{"type": "object", "properties": {{"label": {{"type": "string"}}}}}},
    )
    text = ctx.ask("plain question")
    return {{"labels": labels, "text": text, "usage": ctx.usage}}


@app.behaviour("reads_context", tools=[], context=["policy", "computed"])
def reads_context(ctx, input):
    try:
        ctx.context("recent")
        leaked = True
    except PermissionError:
        leaked = False
    return {{"policy": ctx.context("policy"), "computed": ctx.context("computed"),
            "leaked": leaked, "contexts": ctx.contexts}}


@app.behaviour("gate_probe", tools=["ping", "nuke"])
def gate_probe(ctx, input):
    return {{"soft": ctx.gather(("nuke", {{}}), return_exceptions=True)}}
'''


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Server:
    def __init__(self, base: str):
        self.base = base

    def raw(self, path, method="GET", body=None, key=None, timeout=60):
        data = json.dumps(body).encode() if body is not None else None
        headers = {"content-type": "application/json"} if data else {}
        if key:
            headers["x-api-key"] = key
        req = urllib.request.Request(self.base + path, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                raw = r.read()
                return r.status, json.loads(raw) if raw else None
        except urllib.error.HTTPError as e:
            raw = e.read()
            return e.code, json.loads(raw) if raw else None

    def ask(self, agent_path, input, key=None, **extra):
        return self.raw(agent_path, "POST", {"input": input, **extra}, key=key)

    def behaviour(self, name, payload=None, key=None):
        return self.raw(f"/behaviours/{name.replace('_', '-')}", "POST", payload or {}, key=key)

    def flow(self, name, payload=None, key=None):
        return self.raw(f"/flows/{name.replace('_', '-')}", "POST", payload or {}, key=key)


@pytest.fixture(scope="module")
def server():
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-orch-"))
    (workdir / "api.py").write_text(APP.format(port=port))
    log_path = workdir / "server.log"

    env = {**os.environ, "WEBCORTEX_LOG": "warn", "WEBCORTEX_FAKE_PROVIDER": "1",
           "ADMIN_KEY": ADMIN_KEY, "USER_KEY": USER_KEY}
    env.pop("ANTHROPIC_API_KEY", None)
    with log_path.open("w") as log:
        proc = subprocess.Popen(
            [sys.executable, "-m", "webcortex.cli", "run", "api.py"],
            cwd=workdir, stdout=log, stderr=subprocess.STDOUT, env=env,
        )
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

    s = Server(base)
    for body in ("first ticket", "second ticket"):
        assert s.raw("/tickets", "POST", {"body": body, "state": "open"})[0] == 200
    yield s
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()


# --- agents over HTTP ---------------------------------------------------------


def test_an_agent_answers_over_http(server):
    status, out = server.ask("/ask", "hello there")
    assert status == 200, out
    assert out["status"] == "completed"
    assert out["output"] == "front_desk echoes: hello there"
    assert out["path"] == ["front_desk"]
    assert out["usage"]["steps"] == 1


def test_an_agent_calls_a_rust_tool_in_process(server):
    status, out = server.ask("/ask", "tool:list_tickets {}")
    assert status == 200
    kinds = [s["kind"] for s in out["steps"]]
    assert kinds == ["model", "tool_call", "model"]
    assert out["steps"][1]["result"][0]["body"] == "first ticket"
    assert out["output"].startswith("front_desk says:")


def test_the_agent_route_is_an_mcp_tool_too(server):
    status, out = server.raw("/_webcortex/mcp", "POST", {
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "billing", "arguments": {"input": "hi"}},
    }, key=ADMIN_KEY)
    assert status == 200
    assert out["result"]["structuredContent"]["output"] == "billing echoes: hi"


def test_missing_input_is_a_400_not_a_500(server):
    status, out = server.raw("/ask", "POST", {"nope": 1})
    assert status == 400
    assert "input" in out["error"]["message"]


def test_large_tool_results_are_bounded_for_the_model(server):
    status, out = server.ask("/ask", "tool:big {}")
    assert status == 200
    # The step keeps the whole thing; the model was shown a cut copy.
    assert len(out["steps"][1]["result"]["rows"]) == 3000
    assert "truncated" in out["output"]
    shown = int(out["output"].rsplit("(len=", 1)[1].rstrip(")"))
    assert 400 <= shown < 600, shown


# --- handoffs -----------------------------------------------------------------


def test_a_handoff_moves_the_conversation_to_the_specialist(server):
    status, out = server.ask("/ask", "handoff:billing")
    assert status == 200, out
    assert out["path"] == ["front_desk", "billing"]
    assert out["agent"] == "billing"
    assert out["usage"]["handoffs"] == 1
    assert any(s["kind"] == "handoff" for s in out["steps"])
    assert out["output"].startswith("billing says:")


def test_the_audit_trail_records_the_handoff(server):
    _, audit = server.raw("/_webcortex/audit", key=ADMIN_KEY)
    kinds = {e["kind"] for e in audit["events"]}
    assert "handoff" in kinds


# --- sessions -----------------------------------------------------------------


def test_sessions_carry_the_conversation_and_are_per_principal(server):
    _, first = server.ask("/ask", "remember this", session_id="s1", key=ADMIN_KEY)
    _, second = server.ask("/ask", "and this", session_id="s1", key=ADMIN_KEY)
    assert first["session_id"] == "s1"
    # The fake provider's input count grows with the conversation it was sent.
    assert second["usage"]["input_tokens"] > first["usage"]["input_tokens"]

    _, other = server.ask("/ask", "and this", session_id="s1", key=USER_KEY)
    assert other["usage"]["input_tokens"] <= first["usage"]["input_tokens"] + 2, \
        "another principal must not inherit the session"

    _, reset = server.ask("/ask", "start over", session_id="s1", key=ADMIN_KEY, reset=True)
    assert reset["usage"]["input_tokens"] <= first["usage"]["input_tokens"] + 2


# --- context providers --------------------------------------------------------


def test_context_providers_land_in_the_system_prompt(server):
    status, out = server.ask("/ask", "context?", key=ADMIN_KEY)
    assert status == 200, out
    suffix = out["output"]
    assert '<context name="policy" description="the rules">' in suffix
    assert '"refund_days": 30' in suffix
    assert '<context name="recent">' in suffix and "second ticket" in suffix
    assert '"me": "admin"' in suffix, "@principal must bind to the caller"
    assert '"n": 42' in suffix, "a Python provider must run"
    assert "transfer_to_" in suffix, "handoff instructions must be present"


def test_a_behaviour_reads_only_the_context_it_declared(server):
    status, out = server.behaviour("reads_context")
    assert status == 200, out
    assert out["policy"] == {"refund_days": 30}
    assert out["computed"]["n"] == 42
    assert out["leaked"] is False
    assert out["contexts"] == ["policy", "computed"]


# --- memory -------------------------------------------------------------------


def test_memory_is_per_root_principal_and_executed_in_rust(server):
    assert server.raw("/notes/remember", "POST", {"key": "colour", "value": "blue"}, key=ADMIN_KEY)[0] == 200
    status, row = server.raw("/notes/recall/colour", key=ADMIN_KEY)
    assert (status, row["value"]) == (200, "blue")
    assert server.raw("/notes/recall/colour", key=USER_KEY)[0] == 404, "another caller must not see it"
    _, found = server.raw("/notes/search?query=blu", key=ADMIN_KEY)
    assert [r["key"] for r in found] == ["colour"]
    # Upsert, not duplicate.
    server.raw("/notes/remember", "POST", {"key": "colour", "value": "green"}, key=ADMIN_KEY)
    assert server.raw("/notes/recall/colour", key=ADMIN_KEY)[1]["value"] == "green"
    assert server.raw("/notes/forget/colour", "DELETE", key=ADMIN_KEY)[1]["affected"] == 1


def test_an_agent_writes_memory_as_the_human_behind_it(server):
    status, out = server.ask(
        "/agents/memory-agent", 'tool:notes_remember {"key": "pet", "value": "cat"}', key=USER_KEY
    )
    assert status == 200, out
    assert out["steps"][1]["result"]["value"] == "cat"
    assert server.raw("/notes/recall/pet", key=USER_KEY)[1]["value"] == "cat"
    assert server.raw("/notes/recall/pet", key=ADMIN_KEY)[0] == 404


def test_anonymous_callers_get_no_session_and_no_per_caller_data(server):
    # Every anonymous caller is the same principal, so none of this may be pooled.
    status, _ = server.ask("/ask", "hello", session_id="shared")
    assert status == 401
    assert server.raw("/notes/remember", "POST", {"key": "k", "value": "v"})[0] == 401
    assert server.raw("/notes/recall/k")[0] == 401
    status, out = server.ask("/ask", "context?")
    assert status == 200, out
    assert '"me": null' in out["output"], "@principal binds nothing for an anonymous caller"


# --- approvals that resume ----------------------------------------------------


def test_a_gated_tool_suspends_and_the_run_resumes_after_approval(server):
    status, out = server.ask("/agents/danger-agent", "tool:ping {} ; tool:nuke {} ; tool:ping {}", key=ADMIN_KEY)
    assert status == 202, out
    assert out["status"] == "awaiting_approval"
    approval_id = out["pending_approval"]["approval_id"]
    assert out["pending_approval"]["tool"] == "nuke"
    assert [s["kind"] for s in out["steps"]].count("tool_call") == 1, "only the call before the gate ran"

    _, pending = server.raw("/_webcortex/approvals", key=ADMIN_KEY)
    assert any(p["approval_id"] == approval_id for p in pending["approvals"])

    status, resumed = server.raw(f"/_webcortex/approvals/{approval_id}", "POST",
                                 {"approve": True, "note": "go"}, key=ADMIN_KEY)
    assert status == 200, resumed
    assert resumed["status"] == "completed"
    tools = [s["tool"] for s in resumed["steps"] if s["kind"] == "tool_call"]
    assert tools == ["ping", "nuke", "ping"], "the rest of the turn must run after approval"
    nuke = next(s for s in resumed["steps"] if s["kind"] == "tool_call" and s["tool"] == "nuke")
    assert nuke["result"] == {"nuked": True}

    status, again = server.raw(f"/_webcortex/approvals/{approval_id}", "POST", {"approve": True}, key=ADMIN_KEY)
    assert status == 404, "a decision is consumed"


def test_a_denied_approval_continues_the_run_without_the_tool(server):
    _, out = server.ask("/agents/danger-agent", "tool:nuke {}", key=ADMIN_KEY)
    approval_id = out["pending_approval"]["approval_id"]
    status, resumed = server.raw(f"/_webcortex/approvals/{approval_id}", "POST",
                                 {"approve": False, "note": "not today"}, key=ADMIN_KEY)
    assert status == 200
    assert resumed["status"] == "completed"
    assert any(s["kind"] == "approval_denied" for s in resumed["steps"])
    assert not any(s["kind"] == "tool_call" for s in resumed["steps"])
    assert "not today" in resumed["output"]


def test_approvals_need_the_admin_scope(server):
    assert server.raw("/_webcortex/approvals", key=USER_KEY)[0] == 403
    assert server.raw("/_webcortex/approvals", key=None)[0] == 401
    status, _ = server.raw("/_webcortex/approvals/x", "POST", {"approve": "yes"}, key=ADMIN_KEY)
    assert status == 400


def test_a_gate_cannot_be_laundered_through_gather(server):
    status, out = server.behaviour("gate_probe")
    assert status == 200
    assert out["halted"] is True and "approval" in out["reason"]


# --- budgets compose ----------------------------------------------------------


def test_a_supervisor_and_its_worker_share_one_budget(server):
    status, out = server.ask("/agents/supervisor", 'tool:billing {"input": "tool:list_tickets {}"}', key=ADMIN_KEY)
    assert status == 200, out
    assert out["usage"]["tree_tokens"] > out["usage"]["input_tokens"] + out["usage"]["output_tokens"], \
        "the worker's spend must count against the supervisor's tree"


def test_a_tight_budget_stops_a_run(server):
    status, out = server.ask("/agents/tight", "tool:ping {}", key=ADMIN_KEY)
    assert status == 200
    assert out["status"] == "budget_exhausted"


# --- flows --------------------------------------------------------------------


def test_a_pipeline_feeds_each_agent_the_previous_output(server):
    status, out = server.flow("pipe", {"input": "hi"})
    assert status == 200, out
    assert out["status"] == "completed"
    assert out["output"] == "billing echoes: front_desk echoes: hi"
    assert [s["tool"] for s in out["steps"]] == ["front_desk", "billing"]
    assert out["usage"]["tree_tokens"] > 0


def test_a_parallel_flow_collects_or_merges(server):
    _, out = server.flow("both", {"input": "x"})
    assert out["output"] == [{"pong": True}, {"pong2": True}]
    _, merged = server.flow("merged", {"input": "x"})
    assert merged["output"] == {"pong": True, "pong2": True}


def test_a_router_flow_classifies_then_dispatches(server):
    status, out = server.flow("desk", {"input": "my invoice is wrong"})
    assert status == 200, out
    assert out["steps"][0] == {**out["steps"][0], "tool": "classify", "label": "billing", "ok": True}
    assert out["steps"][1]["tool"] == "billing"
    assert out["output"] == "billing echoes: my invoice is wrong"

    _, fallback = server.flow("desk_default", {"input": "??"})
    assert fallback["steps"][1]["tool"] == "front_desk"


def test_step_input_templates_map_arguments(server):
    status, out = server.flow("mapped", {"ticket": 1})
    assert status == 200, out
    assert out["output"] == "billing echoes: first ticket"


def test_a_flow_is_callable_as_an_mcp_tool(server):
    _, out = server.raw("/_webcortex/mcp", "POST", {
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": {"name": "both", "arguments": {"input": "x"}},
    }, key=ADMIN_KEY)
    assert out["result"]["structuredContent"]["output"] == [{"pong": True}, {"pong2": True}]


def test_flows_are_listed_on_the_control_plane(server):
    _, flows = server.raw("/_webcortex/flows", key=ADMIN_KEY)
    kinds = {f["name"]: f["kind"]["kind"] for f in flows["flows"]}
    assert kinds["pipe"] == "pipeline" and kinds["both"] == "parallel" and kinds["desk"] == "route"
    _, contexts = server.raw("/_webcortex/contexts", key=ADMIN_KEY)
    assert {c["name"]: c["kind"] for c in contexts["contexts"]}["computed"] == "python"


# --- concurrent leaves ----------------------------------------------------------


def test_gather_runs_tool_calls_concurrently_and_reports_failures(server):
    status, out = server.behaviour("fanout")
    assert status == 200, out
    assert out["ok"] == [{"pong": True}, {"pong2": True}]
    assert out["soft"][0]["ok"] is False and "404" in out["soft"][0]["error"]
    assert out["soft"][1] == {"pong": True}
    assert out["raised"] is True
    assert out["steps"] == 5, "one step per gathered call"


def test_ask_many_returns_structured_answers_in_order(server):
    status, out = server.behaviour("classify_all")
    assert status == 200, out
    assert out["labels"] == [{"label": "a"}, {"label": "b"}, {"label": "c"}]
    assert out["text"] == "classify_all echoes: plain question"
    assert out["usage"]["steps"] == 4
    assert out["usage"]["tree_tokens"] > 0


# --- ledger and models ----------------------------------------------------------


def test_the_usage_ledger_reports_spend_by_caller_and_model(server):
    _, usage = server.raw("/_webcortex/usage", key=ADMIN_KEY)
    assert usage["runs"] > 0, usage
    assert usage["by_caller"]["agent:front_desk"]["calls"] > 0, usage
    assert usage["by_caller"]["behaviour:classify_all"]["calls"] == 4, usage["by_caller"]
    assert usage["by_caller"]["flow:desk"]["calls"] >= 1, usage["by_caller"]
    assert "test-fast" in usage["by_model"], usage["by_model"]
    # Only some models are priced, so the total cost is honestly unknown.
    assert usage["estimated_cost_usd"] is None
    assert usage["priced_cost_usd"] > 0
    # Aliases are resolved before charging, so spend is grouped by real model.
    assert "claude-opus-5" in usage["unpriced_models"]



def test_models_are_described_on_the_control_plane(server):
    _, models = server.raw("/_webcortex/models", key=ADMIN_KEY)
    assert models["aliases"]["fast"] == "test-fast"
    assert models["fake"] is True
    assert "ollama" in models["prefixes"]


def test_health_counts_the_new_surfaces(server):
    _, health = server.raw("/_webcortex/health")
    assert health["flows"] == 6
    assert health["agents"] == 6
    assert health["sessions"] >= 1
