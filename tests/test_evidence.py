"""2.8: structured output, request options, traces and evals.

Declarations are checked in-process. Everything else runs a real server
against the deterministic fake provider (`WEBCORTEX_FAKE_PROVIDER=1`), with a
tiny OTLP collector beside it, so the whole path — HTTP, the agent loop, the
trace store, the exporter, the eval runner and the CLI — is exercised with no
network and no key.
"""

from __future__ import annotations

import http.server
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Literal, TypedDict

import pytest

from webcortex import WebCortex, cli, evals
from webcortex.diagnostics import StatusError, fetch_control, render_trace, render_traces

ADMIN_KEY = "evidence-admin-key"
WEATHER = {
    "type": "object",
    "properties": {"city": {"type": "string"}, "temp_c": {"type": "number"}},
    "required": ["city", "temp_c"],
    "additionalProperties": False,
}


def make_app(**kw) -> WebCortex:
    return WebCortex("t", database="sqlite://:memory:", **kw)


# ---------------------------------------------------------------- declarations


def test_output_accepts_a_schema_a_dataclass_or_a_typeddict():
    @dataclass
    class Triage:
        category: Literal["bug", "billing"]
        urgency: int

    class Verdict(TypedDict):
        ok: bool

    app = make_app()
    app.agent("a", output=WEATHER)
    app.agent("b", output=Triage)
    app.agent("c", output=Verdict)
    app.agent("plain")
    agents = {a["name"]: a for a in app.manifest()["agents"]}
    assert agents["a"]["output_schema"] == WEATHER
    assert agents["b"]["output_schema"]["properties"]["category"]["enum"] == ["bug", "billing"]
    assert agents["b"]["output_schema"]["required"] == ["category", "urgency"]
    assert agents["c"]["output_schema"]["properties"]["ok"]["type"] == "boolean"
    assert "output_schema" not in agents["plain"]
    app.check()
    # The route advertises the typed envelope; an untyped agent advertises none.
    routes = {r["tool"]["name"]: r for r in app.manifest()["routes"] if r["op"]["kind"] == "agent"}
    assert routes["a"]["output_schema"]["properties"]["data"] == WEATHER
    assert routes["plain"]["output_schema"] is None


@pytest.mark.parametrize("bad", [list[int], str, {"type": "array"}, {"properties": {}}])
def test_output_must_describe_an_object(bad):
    with pytest.raises(ValueError, match="JSON object"):
        make_app().agent("a", output=bad)


def test_final_answer_is_reserved_only_for_agents_with_an_output():
    app = make_app()
    app.static("GET", "/f", {"ok": True}, tool=True, tool_name="final_answer")
    app.agent("free", tools=["final_answer"])
    app.check()
    with pytest.raises(ValueError, match="final_answer"):
        app.agent("typed", tools=["final_answer"], output=WEATHER)


def test_options_are_declared_on_agents_behaviours_and_providers():
    app = make_app()
    app.agent("a", options={"seed": 7, "max_tokens": None, "max_completion_tokens": 512})

    @app.behaviour("b", options={"top_p": 0.5})
    def b(ctx, input):
        return {}

    app.provider("vllm", base_url="http://gpu:8000/v1", options={"guided": True})
    app.request_options("ollama", keep_alive="30m")
    app.request_options("ollama", seed=1)
    m = app.manifest()
    assert m["agents"][0]["options"] == {"seed": 7, "max_tokens": None, "max_completion_tokens": 512}
    assert m["behaviours"][0]["options"] == {"top_p": 0.5}
    assert m["models"]["options"] == {"vllm": {"guided": True}, "ollama": {"keep_alive": "30m", "seed": 1}}
    assert "options" not in m["models"]["providers"]["vllm"]
    app.check()


@pytest.mark.parametrize("key", ["model", "messages", "tools", "tool_choice", "system", "stream"])
def test_reserved_options_are_refused_at_declaration(key):
    app = make_app()
    with pytest.raises(ValueError, match=f"option '{key}' is managed by the runtime"):
        app.agent("a", options={key: 1})
    with pytest.raises(ValueError, match="managed by the runtime"):
        app.request_options("openai", **{key: 1})
    with pytest.raises(ValueError, match="managed by the runtime"):
        app.behaviour("b", options={key: 1})


def test_options_must_be_a_json_dict():
    app = make_app()
    with pytest.raises(ValueError, match="dict with string keys"):
        app.agent("a", options=["seed"])
    with pytest.raises(ValueError, match="JSON-serialisable"):
        app.agent("a", options={"x": object()})
    with pytest.raises(ValueError, match="JSON-serialisable"):
        app.agent("a", options={"x": float("nan")})


def test_the_core_refuses_reserved_options_in_a_hand_written_manifest():
    from webcortex import _core

    app = make_app()
    app.agent("a")
    m = app.manifest()
    m["agents"][0]["options"] = {"messages": []}
    with pytest.raises(ValueError, match="managed by the runtime"):
        _core.inspect_manifest(json.dumps(m))
    m["agents"][0]["options"] = {}
    m["agents"][0]["output_schema"] = {"type": "string"}
    with pytest.raises(ValueError, match="output schema"):
        _core.inspect_manifest(json.dumps(m))


def test_tracing_is_on_by_default_and_configurable():
    app = make_app()
    assert app.manifest()["tracing"] == {"enabled": True}
    app.tracing(keep=50, otlp_endpoint="http://otel:4318", otlp_headers_env="OTLP_HEADERS", service_name="shop")
    assert app.manifest()["tracing"] == {
        "enabled": True, "keep": 50, "max_spans": 500, "otlp_endpoint": "http://otel:4318",
        "otlp_headers_env": "OTLP_HEADERS", "service_name": "shop",
    }
    app.check()
    with pytest.raises(ValueError, match="http"):
        app.tracing(otlp_endpoint="otel:4317")
    with pytest.raises(ValueError, match="keep"):
        app.tracing(keep=0)
    app.tracing(enabled=False)
    assert app.manifest()["tracing"]["enabled"] is False
    app.check()


def test_evals_are_validated_when_declared():
    app = make_app()
    app.agent("desk")
    app.eval("greets", target="desk", input="hi", expect={"output_contains": "hi"}, scopes=["read"],
             description="says hello")
    case = app._evals[0]
    assert (case.name, case.target, case.args, case.scopes) == ("greets", "desk", {"input": "hi"}, ["read"])
    app.eval("with_args", target="desk", input={"input": "x", "session_id": "s"})
    assert app._evals[1].args == {"input": "x", "session_id": "s"}
    with pytest.raises(ValueError, match="duplicate eval name"):
        app.eval("greets", target="desk", input="hi")
    with pytest.raises(ValueError, match="unknown expect key 'tool_called'"):
        app.eval("typo", target="desk", input="hi", expect={"tool_called": ["x"]})
    with pytest.raises(ValueError, match="names the cassette file"):
        app.eval("../escape", target="desk", input="hi")
    with pytest.raises(ValueError, match="dict of arguments or a string"):
        app.eval("bad_input", target="desk", input=3)
    with pytest.raises(ValueError, match="regular expression"):
        app.eval("bad_re", target="desk", input="hi", expect={"output_matches": "("})
    # Evals are development-time: they are not part of what the runtime executes.
    assert "evals" not in app.manifest()


def test_check_result_reports_every_shortfall():
    case = evals.EvalCase("c", target="desk", args={}, expect={
        "output_contains": ["alpha", "beta"], "output_not_contains": "secret", "output_matches": r"\d+ items",
        "data": {"n": 2, "inner": {"k": "v"}}, "tools_called": ["a", "c"], "tools_not_called": ["danger"],
        "path": ["desk", "billing"], "max_steps": 1, "max_tokens": 10, "max_model_calls": 1,
    })
    good = {
        "status": 200, "error": None, "usage": {"total_tokens": 10},
        "trace": {"model_calls": 1, "spans": [
            {"kind": "tool", "name": "tool a"}, {"kind": "tool", "name": "tool b"},
            {"kind": "tool", "name": "tool c"}, {"kind": "tool", "name": "tool danger", "code": "WC307"},
        ]},
        "body": {"run_id": "r", "status": "completed", "output": "alpha beta 3 items", "path": ["desk", "billing"],
                 "data": {"n": 2, "inner": {"k": "v", "more": 1}}, "usage": {"steps": 1}, "steps": []},
    }
    assert evals.check_result(case, good) == []

    bad = json.loads(json.dumps(good))
    bad["usage"]["total_tokens"] = 11
    bad["trace"]["model_calls"] = 2
    bad["trace"]["spans"] = [{"kind": "tool", "name": "tool c"}, {"kind": "tool", "name": "tool a"},
                             {"kind": "tool", "name": "tool danger"}]
    bad["body"].update(status="step_limit", code="WC404", output="alpha secret", path=["desk"],
                       data={"n": 3}, usage={"steps": 4})
    failures = evals.check_result(case, bad)
    text = "\n".join(failures)
    for expected in ["status is 'step_limit', expected 'completed' (code WC404)", "does not contain 'beta'",
                     "output contains 'secret'", "does not match", "data.n is 3, expected 2",
                     "expected ['a', 'c'] in that order", "tool 'danger' was called", "path is ['desk']",
                     "took 4 steps", "spent 11 tokens", "made 2 model calls"]:
        assert expected in text, f"{expected!r} not in:\n{text}"

    # A function check: raising, False and a string all fail; None and True pass.
    for verdict, message in [(False, "check returned False"), ("too slow", "check: too slow")]:
        c = evals.EvalCase("c", target="desk", args={}, check=lambda body, v=verdict: v)
        assert evals.check_result(c, good) == [message]
    boom = evals.EvalCase("c", target="desk", args={}, check=lambda body: 1 / 0)
    assert evals.check_result(boom, good)[0].startswith("check raised ZeroDivisionError")
    assert evals.check_result(evals.EvalCase("c", target="desk", args={}, check=lambda body: None), good) == []
    # A route that is not a run: success is the HTTP status.
    plain = evals.EvalCase("c", target="ping", args={})
    assert evals.check_result(plain, {"status": 200, "error": None, "body": {"pong": True}, "usage": {}}) == []
    refused = {"status": 403, "error": None, "usage": {},
               "body": {"error": {"code": "WC602", "message": "missing scope"}}}
    assert evals.check_result(plain, refused) == ["the call answered 403 (WC602): missing scope"]
    expects_it = evals.EvalCase("c", target="ping", args={}, expect={"http_status": 403, "code": "WC602"})
    assert evals.check_result(expects_it, refused) == []
    assert evals.check_result(plain, {"error": "unknown tool \"ping\""}) == \
        ['the call could not be made: unknown tool "ping"']


# ---------------------------------------------------------------- the live app

APP = '''
from dataclasses import dataclass
from typing import Literal
from webcortex import WebCortex

app = WebCortex("evidence", version="1.2.3", database="sqlite://:memory:", port={port})
app.api_key("ADMIN_KEY", id="admin", scopes=["webcortex:admin", "read"])
app.tracing(otlp_endpoint="http://127.0.0.1:{collector}", otlp_headers_env="OTLP_HEADERS", keep=100)
app.request_options("anthropic", service_tier="auto", seed=1)

@app.get("/invoice/{{id}}", tool=True, tool_name="lookup_invoice")
def lookup_invoice(id: int) -> dict:
    return {{"id": id, "amount": 42}}

@dataclass
class Triage:
    category: Literal["bug", "billing", "other"]
    urgency: int

app.agent("triage", system="Classify the ticket.", output=Triage, max_steps=6)
app.agent("billing", description="Billing questions.", tools=["lookup_invoice"], options={{"seed": 7, "top_p": 0.9}})
app.agent("front_desk", handoffs=["billing"], tools=["triage", "billing", "lookup_invoice"])
app.flow("intake", pipeline=["triage", {{"tool": "lookup_invoice", "input": {{"id": "$.urgency"}}}}])

@app.behaviour("audit", tools=["lookup_invoice", "billing"], options={{"top_k": 5}})
def audit(ctx, input):
    invoice = ctx.call("lookup_invoice", id=3)
    both = ctx.gather(("lookup_invoice", {{"id": 1}}), ("lookup_invoice", {{"id": 2}}))
    said = ctx.ask("options?")
    nested = ctx.call("billing", input="hello")
    return {{"invoice": invoice, "count": len(both), "options": said, "nested": nested["status"]}}
'''


class Collector:
    """An OTLP/HTTP endpoint that remembers what it was sent."""

    def __init__(self) -> None:
        self.requests: list[tuple[str, dict, dict]] = []
        collector = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                body = self.rfile.read(int(self.headers.get("content-length", 0)))
                collector.requests.append((self.path, dict(self.headers.items()), json.loads(body)))
                self.send_response(200)
                self.send_header("content-length", "2")
                self.end_headers()
                self.wfile.write(b"{}")

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def spans(self) -> list[dict]:
        out = []
        for _, _, body in list(self.requests):
            for resource in body["resourceSpans"]:
                for scope in resource["scopeSpans"]:
                    out.extend(scope["spans"])
        return out


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Server:
    def __init__(self, base: str, collector: Collector, workdir: Path, port: int):
        self.base, self.collector, self.workdir, self.port = base, collector, workdir, port

    def raw(self, path, method="GET", body=None, key=ADMIN_KEY, headers=None):
        data = json.dumps(body).encode() if body is not None else None
        hs = {"content-type": "application/json"} if data else {}
        if key:
            hs["x-api-key"] = key
        hs.update(headers or {})
        req = urllib.request.Request(self.base + path, data=data, method=method, headers=hs)
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                raw = r.read()
                return r.status, json.loads(raw) if raw else None, dict(r.headers.items())
        except urllib.error.HTTPError as e:
            raw = e.read()
            return e.code, json.loads(raw) if raw else None, dict(e.headers.items())

    def ask(self, agent, input, **kw):
        return self.raw(f"/agents/{agent.replace('_', '-')}", "POST", {"input": input}, **kw)

    def trace(self, trace_id):
        return self.raw(f"/_webcortex/traces/{trace_id}")[1]


@pytest.fixture(scope="module")
def server():
    port = _free_port()
    collector = Collector()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-evidence-"))
    (workdir / "api.py").write_text(APP.format(port=port, collector=collector.port))
    log_path = workdir / "server.log"
    env = {**os.environ, "WEBCORTEX_LOG": "warn", "WEBCORTEX_FAKE_PROVIDER": "1", "ADMIN_KEY": ADMIN_KEY,
           "OTLP_HEADERS": "authorization=Bearer otlp-secret,x-team=core"}
    for name in ("ANTHROPIC_API_KEY", "WEBCORTEX_API_KEY", "WEBCORTEX_PORT", "WEBCORTEX_HOST"):
        env.pop(name, None)
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
    yield Server(base, collector, workdir, port)
    proc.terminate()
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()
    collector.server.shutdown()


# ---------------------------------------------------------------- structured output over HTTP


def test_a_typed_agent_answers_with_validated_data(server):
    status, body, _ = server.ask("triage", 'tool:final_answer {"category": "billing", "urgency": 3}')
    assert status == 200
    assert body["status"] == "completed"
    assert body["data"] == {"category": "billing", "urgency": 3}
    assert json.loads(body["output"]) == body["data"]
    assert body["steps"][-1]["kind"] == "output"


def test_json_given_as_prose_is_accepted_when_it_matches(server):
    # The fake echoes its input as text; the JSON in it conforms.
    status, body, _ = server.ask("triage", 'json:{"category": "bug", "urgency": 9}')
    assert status == 200 and body["status"] == "completed"
    assert body["data"] == {"category": "bug", "urgency": 9}
    assert body["usage"]["steps"] == 1


def test_a_prose_answer_is_repaired_through_a_forced_call(server):
    # The echoed text holds JSON with a bad category, so it is refused; the forced
    # call that follows finds nothing better and the answer is refused again.
    status, body, headers = server.ask("triage", 'please classify json:{"category": "nope", "urgency": 1}')
    assert status == 502
    assert body["status"] == "failed" and body["code"] == "WC318"
    assert headers.get("x-webcortex-error") == "WC318"
    assert "schema" in body["hint"]
    rejected = [s for s in body["steps"] if s.get("code") == "WC318"]
    assert len(rejected) == 3, "one answer and two corrections"
    assert "$.category: must be one of" in rejected[0]["error"]
    assert "data" not in body


def test_a_wrong_final_answer_is_reported_with_where_it_is_wrong(server):
    status, body, _ = server.ask("triage", 'tool:final_answer {"category": "billing", "urgency": "high", "x": 1}')
    assert status == 502 and body["code"] == "WC318"
    first = next(s for s in body["steps"] if s.get("code") == "WC318")
    assert "$.urgency: expected integer, got string" in first["error"]
    assert first["kind"] == "output"
    # It is in the live view, like any other failure worth seeing.
    recent = server.raw("/_webcortex/status")[1]["errors"]["recent"]
    assert any(e["code"] == "WC318" for e in recent)


def test_a_flow_passes_the_structured_answer_on_as_an_object(server):
    status, body, _ = server.raw("/flows/intake", "POST",
                                 {"input": 'tool:final_answer {"category": "billing", "urgency": 5}'})
    assert status == 200, body
    # `$.urgency` resolved against the object, not against a JSON string.
    assert body["output"] == {"id": 5, "amount": 42}
    assert [s["tool"] for s in body["steps"]] == ["triage", "lookup_invoice"]


def test_a_supervisor_gets_the_typed_answer_from_its_worker(server):
    inner = json.dumps({"input": 'tool:final_answer {"category": "other", "urgency": 2}'})
    status, body, _ = server.ask("front_desk", f"tool:triage {inner}")
    assert status == 200 and body["status"] == "completed"
    call = next(s for s in body["steps"] if s["kind"] == "tool_call")
    assert call["result"]["data"] == {"category": "other", "urgency": 2}


def test_openapi_describes_the_typed_envelope(server):
    spec = server.raw("/_webcortex/openapi.json")[1]
    op = spec["paths"]["/agents/triage"]["post"]
    schema = op["responses"]["200"]["content"]["application/json"]["schema"]
    assert schema["properties"]["data"]["properties"]["category"]["enum"] == ["bug", "billing", "other"]


# ---------------------------------------------------------------- request options


def test_an_agents_options_are_merged_over_its_providers(server):
    status, body, _ = server.ask("billing", "options?")
    assert status == 200
    # `default` is a bare claude-* name, so the `anthropic` options apply beneath the agent's.
    assert json.loads(body["output"]) == {"service_tier": "auto", "seed": 7, "top_p": 0.9}
    status, body, _ = server.ask("front_desk", "options?")
    assert json.loads(body["output"]) == {"service_tier": "auto", "seed": 1}


def test_a_behaviours_options_reach_its_ask_calls(server):
    status, body, _ = server.raw("/behaviours/audit", "POST", {})
    assert status == 200, body
    assert json.loads(body["options"]) == {"service_tier": "auto", "seed": 1, "top_k": 5}
    assert body["count"] == 2 and body["nested"] == "completed"


def test_the_control_plane_and_context_pack_show_options_and_output_shapes(server):
    from importlib import util

    assert server.raw("/_webcortex/models")[1]["options"] == {"anthropic": {"service_tier": "auto", "seed": 1}}
    agents = {a["name"]: a for a in server.raw("/_webcortex/agents")[1]["agents"]}
    assert agents["billing"]["options"] == {"seed": 7, "top_p": 0.9}
    assert agents["triage"]["output_schema"]["required"] == ["category", "urgency"]
    assert agents["front_desk"]["output_schema"] is None and agents["front_desk"]["options"] == {}

    spec = util.spec_from_file_location("evidence_api", server.workdir / "api.py")
    module = util.module_from_spec(spec)
    spec.loader.exec_module(module)
    pack = module.app.context_pack()
    assert "options: anthropic" in pack
    assert "output={category: " in pack
    assert "options={'seed': 7, 'top_p': 0.9}" in pack


# ---------------------------------------------------------------- traces


def test_a_run_names_its_trace_and_the_trace_holds_the_whole_tree(server):
    inner = json.dumps({"input": "tool:lookup_invoice {\"id\": 4}"})
    status, body, headers = server.ask("front_desk", f"tool:billing {inner}")
    assert status == 200
    trace_id = body["trace_id"]
    assert len(trace_id) == 32 and headers["x-webcortex-trace"] == trace_id

    trace = server.trace(trace_id)
    spans = {s["name"]: s for s in trace["spans"]}
    assert set(spans) >= {"agent front_desk", "tool billing", "agent billing", "tool lookup_invoice"}
    assert spans["agent front_desk"]["parent_span_id"] is None
    assert spans["tool billing"]["parent_span_id"] == spans["agent front_desk"]["span_id"]
    assert spans["agent billing"]["parent_span_id"] == spans["tool billing"]["span_id"]
    assert spans["tool lookup_invoice"]["parent_span_id"] == spans["agent billing"]["span_id"]
    assert spans["agent billing"]["attributes"]["webcortex.run.depth"] == 1
    models = [s for s in trace["spans"] if s["kind"] == "model"]
    assert len(models) == trace["model_calls"] == 4
    assert all(s["status"] == "ok" and s["duration_ms"] is not None for s in trace["spans"])
    # The tree's tokens: more than the outer run made itself, equal to what it reports for the tree.
    assert trace["input_tokens"] + trace["output_tokens"] == body["usage"]["tree_tokens"]
    assert trace["input_tokens"] + trace["output_tokens"] > body["usage"]["input_tokens"] + body["usage"]["output_tokens"]
    # Nothing a span holds is content.
    dumped = json.dumps(trace)
    assert "lookup_invoice {" not in dumped and "amount" not in dumped

    listing = server.raw("/_webcortex/traces")[1]
    assert listing["enabled"] is True and listing["capacity"] == 100
    assert listing["traces"][0]["trace_id"] == trace_id
    assert server.trace("latest")["trace_id"] == trace_id
    # The status view links a run to its trace.
    recent = server.raw("/_webcortex/status")[1]["agents"]["recent"]
    assert any(r.get("trace_id") == trace_id for r in recent)


def test_a_behaviour_and_everything_it_calls_are_one_trace(server):
    status, _, headers = server.raw("/behaviours/audit", "POST", {})
    assert status == 200
    trace = server.trace(headers["x-webcortex-trace"])
    root = next(s for s in trace["spans"] if s["kind"] == "behaviour")
    assert root["name"] == "behaviour audit" and root["status"] == "ok" and root["parent_span_id"] is None
    direct = [s for s in trace["spans"] if s["parent_span_id"] == root["span_id"]]
    assert sorted(s["name"] for s in direct) == sorted(
        ["tool lookup_invoice"] * 3 + ["chat default", "tool billing"])
    ask = next(s for s in direct if s["kind"] == "model")
    assert ask["attributes"]["gen_ai.usage.output_tokens"] == 10
    nested = next(s for s in trace["spans"] if s["name"] == "agent billing")
    assert nested["parent_span_id"] == next(s for s in direct if s["name"] == "tool billing")["span_id"]


def test_a_flow_is_traced_and_a_failed_run_is_an_error_span(server):
    status, body, headers = server.raw("/flows/intake", "POST",
                                       {"input": 'tool:final_answer {"category": "bug", "urgency": 1}'})
    assert status == 200 and headers["x-webcortex-trace"] == body["trace_id"]
    names = [s["name"] for s in server.trace(body["trace_id"])["spans"]]
    assert names[0] == "flow intake" and "agent triage" in names and "tool lookup_invoice" in names

    status, body, _ = server.ask("billing", "fail:anthropic 529: overloaded")
    assert status == 502
    trace = server.trace(body["trace_id"])
    assert trace["status"] == "error" and trace["errors"] == ["WC304", "WC304"]
    assert trace["spans"][0]["code"] == "WC304"


def test_an_incoming_traceparent_is_joined(server):
    parent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
    status, body, _ = server.ask("billing", "hello", headers={"traceparent": parent})
    assert status == 200
    assert body["trace_id"] == "0af7651916cd43dd8448eb211c80319c"
    root = server.trace(body["trace_id"])["spans"][0]
    assert root["parent_span_id"] == "b7ad6b7169203331" and root["remote_parent"] is True
    # A malformed header starts a fresh trace instead of failing the request.
    status, body, _ = server.ask("billing", "hello", headers={"traceparent": "not-a-traceparent"})
    assert status == 200 and len(body["trace_id"]) == 32 and body["trace_id"] != "0af7651916cd43dd8448eb211c80319c"


def test_traces_are_admin_only_and_an_unknown_one_is_wc219(server):
    assert server.raw("/_webcortex/traces", key=None)[0] == 401
    assert server.raw("/_webcortex/traces/latest", key=None)[0] == 401
    status, body, _ = server.raw("/_webcortex/traces/feedfacefeedfacefeedfacefeedface")
    assert status == 404 and body["error"]["code"] == "WC219"
    assert "webcortex trace" in body["error"]["hint"]


def test_finished_spans_are_exported_as_otlp_json(server):
    status, body, _ = server.ask("front_desk", "tool:lookup_invoice {\"id\": 9}")
    assert status == 200
    trace_id = body["trace_id"]
    deadline = time.time() + 15
    while time.time() < deadline:
        mine = [s for s in server.collector.spans() if s["traceId"] == trace_id]
        if len(mine) >= 4:
            break
        time.sleep(0.1)
    else:
        pytest.fail(f"spans never arrived: {server.collector.requests[-1:]}")

    path, headers, document = server.collector.requests[-1]
    assert path == "/v1/traces"
    assert headers.get("authorization") == "Bearer otlp-secret" and headers.get("x-team") == "core"
    resource = {a["key"]: a["value"] for a in document["resourceSpans"][0]["resource"]["attributes"]}
    assert resource["service.name"] == {"stringValue": "evidence"}
    assert resource["service.version"] == {"stringValue": "1.2.3"}
    assert document["resourceSpans"][0]["scopeSpans"][0]["scope"]["name"] == "webcortex"

    by_name = {s["name"]: s for s in mine}
    root = by_name["agent front_desk"]
    assert "parentSpanId" not in root and root["status"] == {"code": 1}
    assert by_name["tool lookup_invoice"]["parentSpanId"] == root["spanId"]
    model = next(s for s in mine if s["kind"] == 3)
    attrs = {a["key"]: a["value"] for a in model["attributes"]}
    assert attrs["gen_ai.operation.name"] == {"stringValue": "chat"}
    assert attrs["gen_ai.request.model"] == {"stringValue": "default"}
    assert attrs["gen_ai.response.model"] == {"stringValue": "claude-opus-5"}
    assert int(attrs["gen_ai.usage.input_tokens"]["intValue"]) > 0
    assert int(model["endTimeUnixNano"]) >= int(model["startTimeUnixNano"])
    export = server.raw("/_webcortex/traces")[1]["export"]
    assert export["configured"] is True and export["spans_exported"] >= 4 and export["spans_failed"] == 0


def test_the_trace_command_lists_and_prints_a_tree(server, capsys, monkeypatch):
    status, body, _ = server.ask("front_desk", "handoff:billing")
    assert status == 200
    monkeypatch.setenv("WEBCORTEX_API_KEY", ADMIN_KEY)
    monkeypatch.chdir(server.workdir)

    assert cli.main(["trace", "--url", server.base]) == 0
    out = capsys.readouterr().out
    assert body["trace_id"] in out and "agent front_desk" in out and "model" in out

    assert cli.main(["trace", body["trace_id"], "--url", server.base]) == 0
    tree = capsys.readouterr().out
    lines = tree.splitlines()
    assert lines[0].strip().startswith(f"trace {body['trace_id']}")
    assert lines[1].strip().startswith("agent front_desk")
    assert any(line.startswith("    └ handoff billing") for line in lines)
    assert any("└ chat default" in line and " in / " in line for line in lines)

    assert cli.main(["trace", "latest", "--json", "--url", server.base]) == 0
    assert json.loads(capsys.readouterr().out)["trace_id"] == body["trace_id"]

    assert cli.main(["trace", "0" * 32, "--json", "--url", server.base]) == 1
    assert json.loads(capsys.readouterr().out)["error"]["code"] == "WC219"

    monkeypatch.setenv("WEBCORTEX_API_KEY", "wrong")
    assert cli.main(["trace", "--json", "--url", server.base]) == 1
    assert json.loads(capsys.readouterr().out)["error"]["code"] in ("WC603", "WC604")


def test_the_python_trace_client_and_renderers(server, monkeypatch):
    monkeypatch.setenv("WEBCORTEX_API_KEY", ADMIN_KEY)
    listing = fetch_control(None, "/traces", url=server.base)
    assert "traces" in listing and "newest first" in render_traces(listing)
    assert "sent" in render_traces(listing), "the export line shows when an exporter is configured"
    trace = fetch_control(None, "/traces/latest", url=server.base)
    assert render_trace(trace).splitlines()[0].strip().startswith("trace ")
    with pytest.raises(StatusError) as e:
        fetch_control(None, "/traces/nope", url=server.base)
    assert e.value.code == "WC219"
    with pytest.raises(StatusError) as e:
        fetch_control(None, "/traces", url=f"http://127.0.0.1:{_free_port()}")
    assert e.value.code == "WC108"
    assert "tracing is off" in render_traces({"enabled": False})
    assert "no traces yet" in render_traces({"enabled": True, "traces": []})


def test_mcp_offers_the_trace_tool(server):
    def rpc(method, params=None):
        return server.raw("/_webcortex/mcp", "POST",
                          {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}})[1]["result"]

    assert "webcortex_trace" in [t["name"] for t in rpc("tools/list")["tools"]]
    listed = rpc("tools/call", {"name": "webcortex_trace", "arguments": {}})
    assert listed["isError"] is False and listed["structuredContent"]["traces"]
    one = rpc("tools/call", {"name": "webcortex_trace", "arguments": {"trace_id": "latest"}})
    assert one["structuredContent"]["spans"]
    missing = rpc("tools/call", {"name": "webcortex_trace", "arguments": {"trace_id": "0" * 32}})
    assert missing["isError"] is True and missing["structuredContent"]["error"]["code"] == "WC219"


# ---------------------------------------------------------------- evals

EVAL_APP = '''
from webcortex import WebCortex

app = WebCortex("evalapp", database="sqlite://:memory:")
app.api_key("ADMIN_KEY", id="admin", scopes=["webcortex:admin"])

CALLS = []

@app.get("/invoice/{id}", tool=True, tool_name="lookup_invoice", scopes=["read"])
def lookup_invoice(id: int) -> dict:
    CALLS.append(id)
    return {"id": id, "amount": 42}

app.agent("billing", system="SYSTEM_PROMPT", description="Billing questions.",
          tools=["lookup_invoice"], scopes=["read"])
app.agent("front_desk", handoffs=["billing"], tools=["billing"], scopes=["read"])
app.agent("typed", output={"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]})

@app.behaviour("audit", tools=["lookup_invoice"], scopes=["read"])
def audit(ctx, input):
    return {"invoice": ctx.call("lookup_invoice", id=input.get("id", 1)), "said": ctx.ask("hello")}

app.eval("handoff", target="front_desk", input="handoff:billing", scopes=["read"],
         expect={"path": ["front_desk", "billing"], "output_contains": "billing", "max_model_calls": 2},
         description="a billing question reaches billing")
app.eval("lookup", target="billing", input='tool:lookup_invoice {"id": 7}', scopes=["read"],
         expect={"tools_called": ["lookup_invoice"], "output_matches": r"amount.*42", "max_tokens": 5000})
app.eval("nested", target="front_desk", scopes=["read"],
         input='tool:billing {"input": "tool:lookup_invoice {\\\\"id\\\\": 2}"}',
         expect={"tools_called": ["billing", "lookup_invoice"]})
app.eval("typed", target="typed", input='tool:final_answer {"n": 3}', expect={"data": {"n": 3}})
app.eval("behaviour", target="audit", input={"id": 5}, scopes=["read"],
         expect={"data": {"invoice": {"id": 5}}, "tools_called": ["lookup_invoice"]},
         check=lambda result: "audit echoes" in result["said"] or "said was " + result["said"])
app.eval("no_scope", target="billing", input='tool:lookup_invoice {"id": 7}',
         expect={"http_status": 403, "code": "WC602", "tools_not_called": ["lookup_invoice"]})
app.eval("refused_route", target="lookup_invoice", input={"id": 1}, expect={"http_status": 403})
'''


def _load(tmp_path: Path, source: str = EVAL_APP, **replace: str) -> WebCortex:
    for old, new in replace.items():
        source = source.replace(old, new)
    path = tmp_path / "api.py"
    path.write_text(source)
    namespace: dict = {}
    exec(compile(source, str(path), "exec"), namespace)  # noqa: S102 - the test's own app
    return namespace["app"]


@pytest.fixture
def fake(monkeypatch):
    monkeypatch.setenv("WEBCORTEX_FAKE_PROVIDER", "1")
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)


def test_evals_run_in_process_against_the_real_tools(tmp_path, fake):
    app = _load(tmp_path)
    report = app.run_evals(mode="live", directory=tmp_path / "evals")
    by_name = {r["name"]: r for r in report["results"]}
    assert report["ok"], {n: r["failures"] for n, r in by_name.items() if not r["ok"]}
    assert (report["total"], report["passed"], report["failed"]) == (7, 7, 0)
    assert all(r["mode"] == "live" and r["cassette"] is None for r in report["results"])
    assert by_name["handoff"]["status"] == "completed" and by_name["handoff"]["model_calls"] == 2
    assert by_name["handoff"]["tokens"] > 0 and len(by_name["handoff"]["trace_id"]) == 32
    # The eval's caller holds only the scopes the eval grants it.
    assert by_name["refused_route"]["status"] == 403 and by_name["refused_route"]["code"] == "WC602"
    assert by_name["typed"]["result"]["data"] == {"n": 3}
    assert report["tokens"] == sum(r["tokens"] for r in report["results"])
    assert not (tmp_path / "evals").exists(), "a live run writes nothing"


def test_a_failing_eval_says_what_fell_short(tmp_path, fake):
    app = _load(tmp_path, **{'expect={"path": ["front_desk", "billing"]': 'expect={"path": ["front_desk"]',
                             '"max_tokens": 5000': '"max_tokens": 5'})
    report = app.run_evals(mode="live", directory=tmp_path / "evals", only=["handoff", "lookup"])
    assert not report["ok"] and report["failed"] == 2 and report["total"] == 2
    handoff, lookup = report["results"]
    assert handoff["failures"] == ["path is ['front_desk', 'billing'], expected ['front_desk']"]
    assert lookup["failures"][0].startswith("spent ") and "max_tokens=5" in lookup["failures"][0]
    text = evals.render_report(report)
    assert "FAIL handoff" in text and "0/2 passed" in text
    with pytest.raises(ValueError, match="no eval named 'nope'"):
        app.run_evals(only=["nope"])
    with pytest.raises(ValueError, match="mode must be one of"):
        app.run_evals(mode="sometimes")


def test_recorded_evals_replay_without_any_provider(tmp_path, fake, monkeypatch):
    app = _load(tmp_path)
    directory = tmp_path / "evals"
    recorded = app.run_evals(mode="record", directory=directory)
    assert recorded["ok"]
    assert sorted(p.name for p in directory.iterdir()) == sorted(f"{c.name}.json" for c in app._evals)
    cassette = json.loads((directory / "nested.json").read_text())
    assert cassette["version"] == 1 and cassette["eval"] == "nested" and cassette["target"] == "front_desk"
    assert [c["agent"] for c in cassette["calls"]] == ["front_desk", "billing", "billing", "front_desk"]
    assert all(len(c["fingerprint"]) == 16 for c in cassette["calls"])
    # A route that never reaches a model records an empty, but present, cassette.
    assert json.loads((directory / "refused_route.json").read_text())["calls"] == []

    # No fake provider, no key: only the cassettes can be answering.
    monkeypatch.delenv("WEBCORTEX_FAKE_PROVIDER")
    app = _load(tmp_path)
    replayed = app.run_evals(mode="replay", directory=directory)
    assert replayed["ok"], [r["failures"] for r in replayed["results"] if not r["ok"]]
    assert all(r["mode"] == "replay" and r["cassette"] for r in replayed["results"])
    for live, again in zip(recorded["results"], replayed["results"]):
        assert again["tokens"] == live["tokens"], "replay reports the recorded spend"
        assert again["model_calls"] == live["model_calls"]
        strip = lambda v: json.loads(json.dumps(v), object_hook=lambda d: {  # noqa: E731
            k: x for k, x in d.items() if k not in ("run_id", "trace_id", "duration_ms")})
        assert strip(again["result"]) == strip(live["result"])
    nested = next(r for r in replayed["results"] if r["name"] == "nested")
    assert nested["replay"] == {"recorded": 4, "used": 4, "unused": 0}
    # `auto` picks the cassette up, and the same run without one would need a key.
    assert app.run_evals(mode="auto", directory=directory)["ok"]
    live = app.run_evals(mode="live", directory=directory, only=["lookup"])
    assert not live["ok"] and live["results"][0]["code"] == "WC301"


def test_a_cassette_goes_stale_when_the_agent_changes(tmp_path, fake):
    directory = tmp_path / "evals"
    assert _load(tmp_path).run_evals(mode="record", directory=directory, only=["lookup"])["ok"]
    changed = _load(tmp_path, SYSTEM_PROMPT="A different system prompt.")
    report = changed.run_evals(mode="replay", directory=directory, only=["lookup"])
    result = report["results"][0]
    assert not result["ok"] and result["code"] == "WC319"
    assert result["failures"][0].startswith("WC319: the cassette is stale") and "--record" in result["failures"][0]
    assert len(result["failures"]) == 1, "the unused responses are not reported on top of the cause"
    # Recording again repairs it.
    assert changed.run_evals(mode="record", directory=directory, only=["lookup"])["ok"]
    assert changed.run_evals(mode="replay", directory=directory, only=["lookup"])["ok"]


def test_replay_needs_a_readable_cassette(tmp_path, fake):
    app = _load(tmp_path)
    directory = tmp_path / "evals"
    missing = app.run_evals(mode="replay", directory=directory, only=["lookup"])["results"][0]
    assert not missing["ok"] and missing["code"] == "WC320" and "--record" in missing["failures"][0]
    directory.mkdir()
    (directory / "lookup.json").write_text("{not json")
    broken = app.run_evals(mode="replay", directory=directory, only=["lookup"])["results"][0]
    assert broken["code"] == "WC320" and "not valid JSON" in broken["failures"][0]
    (directory / "lookup.json").write_text(json.dumps({"version": 99, "calls": []}))
    newer = app.run_evals(mode="replay", directory=directory, only=["lookup"])["results"][0]
    assert newer["code"] == "WC320" and "version 99" in newer["failures"][0]
    # A run that needs more responses than were recorded.
    (directory / "lookup.json").write_text(json.dumps({"version": 1, "calls": []}))
    short = app.run_evals(mode="replay", directory=directory, only=["lookup"])["results"][0]
    assert short["code"] == "WC319" and "no more recorded responses" in short["failures"][0]


def test_an_eval_for_an_unknown_tool_fails_cleanly(tmp_path, fake):
    app = _load(tmp_path)
    app.eval("ghost", target="no_such_tool", input={})
    result = app.run_evals(mode="live", only=["ghost"], directory=tmp_path / "evals")["results"][0]
    assert not result["ok"] and 'unknown tool "no_such_tool"' in result["failures"][0]


def test_the_eval_command(tmp_path, fake, capsys, monkeypatch):
    _load(tmp_path)
    monkeypatch.chdir(tmp_path)

    assert cli.main(["eval", "--list"]) == 0
    listed = capsys.readouterr().out
    assert "handoff" in listed and "no cassette" in listed and "a billing question reaches billing" in listed

    assert cli.main(["eval", "--replay", "--only", "handoff", "--json"]) == 1
    assert json.loads(capsys.readouterr().out)["results"][0]["code"] == "WC320"

    assert cli.main(["eval", "--record"]) == 0
    out = capsys.readouterr().out
    assert "recorded evals/handoff.json" in out and "7/7 passed" in out

    assert cli.main(["eval"]) == 0
    assert "7 replayed from cassettes" in capsys.readouterr().out

    assert cli.main(["eval", "--only", "lookup", "--only", "typed", "--json"]) == 0
    report = json.loads(capsys.readouterr().out)
    assert [r["name"] for r in report["results"]] == ["lookup", "typed"] and report["ok"] is True
    assert cli.main(["eval", "--list", "--json"]) == 0
    assert all(e["cassette"] for e in json.loads(capsys.readouterr().out)["evals"])

    assert cli.main(["eval", "--only", "nope", "--json"]) == 1
    assert json.loads(capsys.readouterr().out)["error"]["code"] == "WC320"

    # A changed prompt turns the replay red, with the code that says why.
    (tmp_path / "api.py").write_text(EVAL_APP.replace("SYSTEM_PROMPT", "Something else."))
    assert cli.main(["eval", "--replay", "--only", "lookup"]) == 1
    assert "WC319" in capsys.readouterr().out


def test_an_app_without_evals_reports_none(tmp_path, capsys, monkeypatch):
    (tmp_path / "api.py").write_text('from webcortex import WebCortex\napp = WebCortex("empty")\n')
    monkeypatch.chdir(tmp_path)
    assert cli.main(["eval"]) == 0
    assert "no evals declared" in capsys.readouterr().out
    assert make_app().run_evals() == {"ok": True, "total": 0, "passed": 0, "failed": 0, "tokens": 0,
                                      "mode": "auto", "directory": "evals", "results": []}
