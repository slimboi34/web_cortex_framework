"""Behaviours: Python control flow with model and tool calls at the leaves.

The property under test throughout is that the *structure* is deterministic.
A loop loops, a branch branches, and the runtime — not the model, and not the
author's diligence — enforces the limits.
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
from dataclasses import dataclass
from pathlib import Path

import pytest

from webcortex import WebCortex


# ---------------------------------------------------------------- declarations


def test_behaviour_is_exposed_as_a_tool_by_default():
    app = WebCortex("t")

    @app.behaviour("summarise")
    def summarise(ctx, input):
        return {}

    assert "summarise" in app.check()["tools"]


def test_behaviour_name_defaults_to_the_function_name():
    app = WebCortex("t")

    @app.behaviour
    def my_procedure(ctx, input):
        return {}

    assert "my_procedure" in app.check()["tools"]


def test_behaviour_registers_a_route_so_it_gets_the_whole_stack():
    app = WebCortex("t")

    @app.behaviour("triage")
    def triage(ctx, input):
        return {}

    route = next(r for r in app.manifest()["routes"] if r["op"]["kind"] == "behaviour")
    assert route["method"] == "POST"
    assert route["path"] == "/behaviours/triage"
    assert route["op"]["behaviour"] == "triage"


def test_behaviour_path_can_be_overridden():
    app = WebCortex("t")

    @app.behaviour("triage", expose_at="/ops/triage")
    def triage(ctx, input):
        return {}

    assert any(r["path"] == "/ops/triage" for r in app.manifest()["routes"])


def test_behaviour_docstring_becomes_the_tool_description():
    app = WebCortex("t")

    @app.behaviour("triage")
    def triage(ctx, input):
        """Escalate urgent tickets."""
        return {}

    b = app.manifest()["behaviours"][0]
    assert "Escalate urgent tickets." in b["description"]


@dataclass
class TriageInput:
    threshold: int
    dry_run: bool = False


def test_behaviour_input_schema_comes_from_the_annotation():
    app = WebCortex("t")

    @app.behaviour("triage")
    def triage(ctx, input: TriageInput):
        return {}

    schema = app.manifest()["behaviours"][0]["input_schema"]
    assert schema["properties"]["threshold"] == {"type": "integer"}
    assert schema["required"] == ["threshold"]


def test_unannotated_behaviour_accepts_any_object():
    app = WebCortex("t")

    @app.behaviour("loose")
    def loose(ctx, input):
        return {}

    schema = app.manifest()["behaviours"][0]["input_schema"]
    assert schema["additionalProperties"] is True


def test_behaviour_declaring_an_unknown_tool_is_a_boot_error():
    app = WebCortex("t")

    @app.behaviour("bad", tools=["no_such_tool"])
    def bad(ctx, input):
        return {}

    with pytest.raises(ValueError, match="no_such_tool"):
        app.check()


def test_a_behaviour_may_declare_another_behaviour_as_a_tool():
    app = WebCortex("t")

    @app.behaviour("inner")
    def inner(ctx, input):
        return {}

    @app.behaviour("outer", tools=["inner"])
    def outer(ctx, input):
        return {}

    app.check()  # must not raise


def test_an_agent_may_use_a_behaviour_as_a_tool():
    app = WebCortex("t")

    @app.behaviour("triage")
    def triage(ctx, input):
        return {}

    app.agent("boss", model="m", tools=["triage"])
    assert app.check()["agents"] == ["boss"]


def test_a_behaviour_declared_with_tool_false_cannot_be_named_as_a_tool():
    app = WebCortex("t")

    @app.behaviour("hidden", tool=False)
    def hidden(ctx, input):
        return {}

    app.agent("boss", model="m", tools=["hidden"])
    with pytest.raises(ValueError, match="tool=False"):
        app.check()

    app = WebCortex("t")

    @app.behaviour("hidden", tool=False)
    def hidden_again(ctx, input):
        return {}

    @app.behaviour("outer", tools=["hidden"])
    def outer(ctx, input):
        return {}

    with pytest.raises(ValueError, match="tool=False"):
        app.check()


def test_duplicate_behaviour_names_are_rejected():
    app = WebCortex("t")

    @app.behaviour("dup")
    def a(ctx, input):
        return {}

    @app.behaviour("dup", expose_at="/other")
    def b(ctx, input):
        return {}

    with pytest.raises(ValueError, match="duplicate behaviour"):
        app.check()


def test_zero_step_behaviour_is_rejected():
    app = WebCortex("t")

    @app.behaviour("useless", max_steps=0)
    def useless(ctx, input):
        return {}

    with pytest.raises(ValueError, match="never do anything"):
        app.check()


def test_behaviours_appear_in_the_security_report():
    app = WebCortex("t")

    @app.behaviour("triage", scopes=["ops"], max_steps=10, token_budget=1000)
    def triage(ctx, input):
        return {}

    b = app.security_report()["behaviours"][0]
    assert b["name"] == "triage"
    assert b["scopes"] == ["ops"]
    assert b["max_steps"] == 10


def test_behaviour_endpoint_inherits_its_scopes_as_a_guard():
    app = WebCortex("t")

    @app.behaviour("triage", scopes=["ops"])
    def triage(ctx, input):
        return {}

    route = next(r for r in app.manifest()["routes"] if r["op"]["kind"] == "behaviour")
    assert route["scopes"] == ["ops"], "a behaviour endpoint must not be more open than the run"


# ---------------------------------------------------------------- live


APP = '''
from webcortex import WebCortex

app = WebCortex("beh", database="sqlite://./beh.db", port={port})

app.resource(
    "tickets",
    fields={{"id": int, "body": str, "urgency": int, "state": str}},
    tools=True,
)
app.static("GET", "/oncall", {{"paged": True}}, tool=True, tool_name="page_oncall")
app.static("GET", "/nuke", {{"nuked": True}}, tool=True, tool_name="nuke",
           approval="required")


@app.behaviour("triage", tools=["list_tickets", "update_tickets", "page_oncall"])
def triage(ctx, input):
    """Walk every open ticket and escalate the urgent ones."""
    threshold = input.get("threshold", 7)
    tickets = ctx.call("list_tickets", limit=100)
    escalated = []
    for t in tickets:
        if t["state"] != "open":
            continue
        if t["urgency"] >= threshold:
            ctx.call("page_oncall")
            escalated.append(t["id"])
            state = "escalated"
        else:
            state = "triaged"
        ctx.call("update_tickets", id=t["id"], body=t["body"],
                 urgency=t["urgency"], state=state)
    ctx.log(f"escalated {{len(escalated)}}")
    return {{"seen": len(tickets), "escalated": escalated,
            "steps": ctx.usage["steps"], "trace": len(ctx.trace)}}


@app.behaviour("burner", tools=["list_tickets"], max_steps=3)
def burner(ctx, input):
    for _ in range(100):
        ctx.call("list_tickets")
    return {{"unreachable": True}}


@app.behaviour("composer", tools=["triage"])
def composer(ctx, input):
    return {{"inner": ctx.call("triage", threshold=input.get("threshold", 7))}}


@app.behaviour("prober", tools=["list_tickets"])
def prober(ctx, input):
    try:
        ctx.call("page_oncall")
        return {{"blocked": False}}
    except PermissionError as e:
        return {{"blocked": True, "reason": str(e)}}


@app.behaviour("gate_probe", tools=["nuke"])
def gate_probe(ctx, input):
    ctx.call("nuke")
    return {{"ran": True}}


@app.behaviour("boom", tools=[])
def boom(ctx, input):
    raise ValueError("intentional behaviour failure")


@app.behaviour("halter", tools=[])
def halter(ctx, input):
    ctx.halt("stopped on purpose")
    return {{"unreachable": True}}


@app.behaviour("introspect", tools=["list_tickets"])
def introspect(ctx, input):
    return {{"user": ctx.user["id"], "tools": ctx.tools,
             "run_id_len": len(ctx.run_id)}}
'''


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture(scope="module")
def beh_server():
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-beh-"))
    (workdir / "api.py").write_text(APP.format(port=port))
    log_path = workdir / "server.log"

    env = {**os.environ, "WEBCORTEX_LOG": "warn"}
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

    # Seed a mix of urgencies so the branch has both paths to take.
    for urgency in (9, 3, 8, 2):
        req = urllib.request.Request(
            base + "/tickets",
            data=json.dumps({"body": f"u{urgency}", "urgency": urgency, "state": "open"}).encode(),
            method="POST",
            headers={"content-type": "application/json"},
        )
        urllib.request.urlopen(req, timeout=10).read()

    yield base
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()


def run_behaviour(base, name, payload=None):
    req = urllib.request.Request(
        f"{base}/behaviours/{name.replace('_', '-')}",
        data=json.dumps(payload or {}).encode(),
        method="POST",
        headers={"content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"null")


def get(base, path):
    with urllib.request.urlopen(base + path, timeout=15) as r:
        return json.loads(r.read())


def test_loop_and_branch_both_actually_execute(beh_server):
    status, out = run_behaviour(beh_server, "triage", {"threshold": 7})
    assert status == 200
    assert out["seen"] == 4
    # Exactly the two tickets at or above the threshold.
    assert len(out["escalated"]) == 2

    states = {t["urgency"]: t["state"] for t in get(beh_server, "/tickets")}
    assert states[9] == "escalated"
    assert states[8] == "escalated"
    assert states[3] == "triaged"
    assert states[2] == "triaged"


def test_the_threshold_actually_changes_the_branch_taken(beh_server):
    _, out = run_behaviour(beh_server, "triage", {"threshold": 100})
    assert out["escalated"] == [], "nothing should clear a threshold of 100"


def test_every_leaf_is_recorded_in_the_trace(beh_server):
    _, out = run_behaviour(beh_server, "triage", {"threshold": 7})
    assert out["trace"] >= out["steps"], "trace must cover at least every charged step"


def test_step_budget_halts_a_runaway_loop(beh_server):
    """The whole point: a loop that would run 100 times is stopped at 3."""
    status, out = run_behaviour(beh_server, "burner")
    assert status == 200
    assert out["halted"] is True
    assert "step budget of 3" in out["reason"]
    assert "unreachable" not in out


def test_behaviours_compose(beh_server):
    status, out = run_behaviour(beh_server, "composer", {"threshold": 7})
    assert status == 200
    assert out["inner"]["seen"] == 4


def test_a_behaviour_cannot_call_a_tool_it_did_not_declare(beh_server):
    _, out = run_behaviour(beh_server, "prober")
    assert out["blocked"] is True
    assert "page_oncall" in out["reason"]


def test_an_approval_gate_cannot_be_laundered_through_a_behaviour(beh_server):
    """Wrapping a gated tool in a behaviour must not bypass the gate."""
    status, out = run_behaviour(beh_server, "gate_probe")
    assert status == 200
    assert out["halted"] is True
    assert "approval" in out["reason"]
    assert "ran" not in out


def test_a_failing_behaviour_is_a_500_and_does_not_kill_the_server(beh_server):
    status, _ = run_behaviour(beh_server, "boom")
    assert status == 500
    assert run_behaviour(beh_server, "prober")[0] == 200


def test_explicit_halt_is_reported_structurally(beh_server):
    status, out = run_behaviour(beh_server, "halter")
    assert status == 200
    assert out["halted"] is True
    assert "on purpose" in out["reason"]


def test_context_exposes_identity_and_declared_tools(beh_server):
    _, out = run_behaviour(beh_server, "introspect")
    assert out["tools"] == ["list_tickets"]
    assert out["run_id_len"] > 10


def test_behaviours_are_mcp_tools(beh_server):
    req = urllib.request.Request(
        beh_server + "/_webcortex/mcp",
        data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).encode(),
        method="POST",
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=15) as r:
        tools = {t["name"] for t in json.loads(r.read())["result"]["tools"]}
    assert {"triage", "composer", "prober"} <= tools


def test_a_behaviour_is_invocable_as_an_mcp_tool(beh_server):
    req = urllib.request.Request(
        beh_server + "/_webcortex/mcp",
        data=json.dumps({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "triage", "arguments": {"threshold": 100}},
        }).encode(),
        method="POST",
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=60) as r:
        result = json.loads(r.read())["result"]
    assert result["isError"] is False
    assert result["structuredContent"]["seen"] == 4


def test_control_plane_lists_behaviours(beh_server):
    names = {b["name"] for b in get(beh_server, "/_webcortex/behaviours")["behaviours"]}
    assert {"triage", "burner", "composer"} <= names
