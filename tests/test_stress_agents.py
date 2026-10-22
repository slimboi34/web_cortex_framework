"""Stress and regression tests for the agent runtime, behaviours, flows and MCP.

Two live servers, both on ephemeral ports:

* `wire` — agents whose models are served by a scriptable fake model server
  in this process (`ModelStub`), speaking the OpenAI Chat Completions and
  Anthropic Messages wire formats. It is how malformed, truncated and failing
  provider answers are reproduced: the shapes real local servers and gateways
  send, which the in-process FakeProvider never would.
* `fake` — the deterministic FakeProvider (`WEBCORTEX_FAKE_PROVIDER=1`), for
  behaviours, approvals, sessions, MCP and concurrency.

Every test here was a bug in 2.5.3; each says what used to happen.
"""

from __future__ import annotations

import concurrent.futures
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
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from webcortex import WebCortex
from webcortex.client import AgentSocket

ADMIN_KEY = "stress-admin-key-0123456789abcdef"
USER_KEY = "stress-user-key-0123456789abcdef0"


# --------------------------------------------------------------------------- helpers


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _request(base, path, method="GET", body=None, key=ADMIN_KEY, timeout=60, raw=None):
    data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
    headers = {"content-type": "application/json", "accept": "application/json, text/event-stream"}
    if key:
        headers["x-api-key"] = key
    req = urllib.request.Request(base + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            payload = r.read()
            return r.status, json.loads(payload) if payload else None
    except urllib.error.HTTPError as e:
        payload = e.read()
        return e.code, json.loads(payload) if payload else None


def _start(source: str, env_extra: dict) -> tuple[str, subprocess.Popen, Path]:
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-stress-"))
    (workdir / "api.py").write_text(source.replace("__PORT__", str(port)))
    log_path = workdir / "server.log"
    env = {**os.environ, "WEBCORTEX_LOG": "warn", "STRESS_ADMIN": ADMIN_KEY, "STRESS_USER": USER_KEY,
           **env_extra}
    env.pop("ANTHROPIC_API_KEY", None)
    env.pop("OPENAI_API_KEY", None)
    if env_extra.get("WEBCORTEX_FAKE_PROVIDER") is None:
        env.pop("WEBCORTEX_FAKE_PROVIDER", None)
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
                return base, proc, log_path
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


# --------------------------------------------------------------------------- a fake model server


class ModelStub:
    """Answers model requests from a script, and records what it was sent.

    An entry is `(status, body, headers)`: `body` is a JSON-able object, raw
    bytes, or a callable taking the request body. With the script empty it
    answers 500, which the runtime retries — so a test sets every answer it
    expects to be asked for.
    """

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.script: list = []
        self.seen: list = []
        stub = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                n = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(n)
                try:
                    body = json.loads(raw)
                except ValueError:
                    body = None
                with stub.lock:
                    stub.seen.append({"path": self.path, "body": body})
                    entry = stub.script.pop(0) if stub.script else (500, {"error": {"message": "script empty"}}, {})
                status, payload, headers = entry
                if callable(payload):
                    payload = payload(body)
                data = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
                self.send_response(status)
                for k, v in headers.items():
                    self.send_header(k, v)
                if not any(k.lower() == "content-type" for k in headers):
                    self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.httpd.server_address[1]
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def set(self, *entries) -> None:
        with self.lock:
            self.script = [e if len(e) == 3 else (e[0], e[1], {}) for e in entries]
            self.seen = []

    def requests(self) -> list:
        with self.lock:
            return list(self.seen)


def oa_text(text, finish="stop", usage=None):
    return {"model": "m", "choices": [{"finish_reason": finish,
                                       "message": {"role": "assistant", "content": text}}],
            "usage": usage or {"prompt_tokens": 5, "completion_tokens": 3}}


def oa_calls(calls, finish="tool_calls"):
    return {"model": "m", "choices": [{"finish_reason": finish, "message": {
        "role": "assistant", "content": None,
        "tool_calls": [{**({"id": c["id"]} if "id" in c else {}), "type": "function",
                        "function": {"name": c["name"], "arguments": c.get("arguments", "{}")}}
                       for c in calls]}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}}


def an_text(text, stop="end_turn"):
    return {"model": "m", "content": [{"type": "text", "text": text}], "stop_reason": stop,
            "usage": {"input_tokens": 4, "output_tokens": 2}}


def assert_replayable_openai(messages):
    """Every assistant tool call is answered by a tool message, and no
    assistant message is empty — what Chat Completions requires."""
    for i, m in enumerate(messages):
        if m["role"] != "assistant":
            continue
        calls = m.get("tool_calls") or []
        assert m.get("content") or calls, f"empty assistant message replayed: {messages}"
        answered = {x.get("tool_call_id") for x in messages[i + 1:i + 1 + len(calls)] if x["role"] == "tool"}
        for c in calls:
            assert c["id"] and c["id"] in answered, f"tool call {c['id']!r} has no tool message: {messages}"


WIRE_APP = '''
from webcortex import WebCortex

app = WebCortex("wire", port=__PORT__)
app.api_key("STRESS_ADMIN", id="admin", scopes=["read", "write", "webcortex:admin"])
app.provider("stub", base_url="http://127.0.0.1:__STUB__/v1")
app.provider("astub", base_url="http://127.0.0.1:__STUB__", kind="anthropic", api_key_env="STUB_ANTHROPIC_KEY")

CALLS = []

@app.post("/record", tool=True, tool_name="record")
def record(note: str = "") -> dict:
    CALLS.append(note)
    return {"recorded": note}

@app.get("/recorded")
def recorded() -> dict:
    return {"calls": list(CALLS)}

app.static("GET", "/ping", {"pong": True}, tool=True, tool_name="ping")

app.agent("oa", model="stub/m", tools=["record", "ping"], max_steps=4)
app.agent("an", model="astub/m", tools=["record", "ping"], max_steps=4)

app.static("GET", "/nuke", {"nuked": True}, tool=True, tool_name="nuke", approval="required")
app.agent("oag", model="stub/m", tools=["nuke"], max_steps=4)
'''


@pytest.fixture(scope="module")
def wire():
    stub = ModelStub()
    base, proc, log = _start(WIRE_APP.replace("__STUB__", str(stub.port)),
                             {"STUB_ANTHROPIC_KEY": "not-a-real-key"})
    yield base, stub
    _stop(proc)
    stub.httpd.shutdown()


def _ask(base, agent, input, **extra):
    return _request(base, f"/agents/{agent}", "POST", {"input": input, **extra})


def _recorded(base):
    return _request(base, "/recorded")[1]["calls"]


# --------------------------------------------------------------------------- provider failures


def test_a_rate_limit_is_retried_and_the_run_completes(wire):
    base, stub = wire
    stub.set((429, b"slow down", {"content-type": "text/plain", "retry-after": "0"}), (200, oa_text("hello")))
    status, out = _ask(base, "oa", "hi")
    assert status == 200 and out["status"] == "completed", out
    assert out["output"] == "hello"
    assert len(stub.requests()) == 2, "one retry"


def test_a_failure_that_persists_keeps_its_status_even_with_a_text_body(wire):
    # 2.5.3 parsed the body before looking at the status: a 429 or 502 that
    # answered in text or HTML failed as WC305 "unreadable", status lost.
    base, stub = wire
    page = b"<html><body><h1>502 Bad Gateway</h1></body></html>"
    stub.set(*[(502, page, {"content-type": "text/html"})] * 3)
    status, out = _ask(base, "oa", "hi")
    assert status == 502 and out["code"] == "WC304", out
    error = out["steps"][-1]["error"]
    assert "502" in error and "Bad Gateway" in error, error
    assert len(stub.requests()) == 3, "two retries, then the error"


def test_a_client_error_is_not_retried(wire):
    base, stub = wire
    stub.set((400, {"error": {"message": "model 'm' not found"}}))
    status, out = _ask(base, "oa", "hi")
    assert status == 502 and out["code"] == "WC304"
    assert "not found" in out["steps"][-1]["error"]
    assert len(stub.requests()) == 1


def test_a_success_status_with_a_body_that_is_not_the_wire_format_is_wc305(wire):
    base, stub = wire
    for body in (b"data: {\"x\": 1}\n\ndata: [DONE]\n\n", b"", {"choices": []}, {"choices": [{"message": None}]}):
        stub.set((200, body, {"content-type": "text/event-stream"} if isinstance(body, bytes) else {}))
        status, out = _ask(base, "oa", "hi")
        assert status == 502 and out["code"] == "WC305", (body, out)


def test_an_error_inside_a_200_body_is_reported_with_its_detail(wire):
    base, stub = wire
    stub.set((200, {"error": {"message": "insufficient credits", "code": 402}}))
    status, out = _ask(base, "oa", "hi")
    assert status == 502 and out["code"] == "WC304", out
    assert "insufficient credits" in out["steps"][-1]["error"]


# --------------------------------------------------------------------------- malformed tool calls


def test_tool_calls_under_a_stop_finish_reason_are_run(wire):
    # Ollama answers `finish_reason: "stop"` for a turn of tool calls; 2.5.3
    # ended the run there, "completed" with no output and the tool never run.
    base, stub = wire
    before = len(_recorded(base))
    stub.set((200, oa_calls([{"id": "c1", "name": "record", "arguments": '{"note": "stop"}'}], finish="stop")),
             (200, oa_text("done")))
    status, out = _ask(base, "oa", "hi")
    assert out["status"] == "completed" and out["output"] == "done", out
    assert _recorded(base)[before:] == ["stop"]


@pytest.mark.parametrize("arguments", ['{"note": "half', "[1, 2]", '"just a string"', "7"])
def test_garbled_tool_arguments_are_refused_not_run_with_none(wire, arguments):
    # 2.5.3 ran the tool with {} when the argument string did not parse, and
    # coerced a bare value to {} at dispatch.
    base, stub = wire
    before = len(_recorded(base))
    stub.set((200, oa_calls([{"id": "c1", "name": "record", "arguments": arguments}])), (200, oa_text("ok")))
    status, out = _ask(base, "oa", "hi")
    assert status == 200 and out["status"] == "completed", out
    assert _recorded(base)[before:] == [], "the tool must not have run"
    refused = [s for s in out["steps"] if s["kind"] == "tool_refused"]
    assert refused and refused[0]["code"] == "WC317" and refused[0]["hint"], out["steps"]
    # The model was told, in a tool message paired with its call.
    sent = stub.requests()[-1]["body"]["messages"]
    assert_replayable_openai(sent)
    assert "not a JSON object" in sent[-1]["content"]


def test_missing_empty_and_duplicate_call_ids_are_made_unique_and_paired(wire):
    base, stub = wire
    stub.set((200, oa_calls([{"name": "ping"}, {"id": "", "name": "ping"},
                             {"id": "same", "name": "ping"}, {"id": "same", "name": "ping"}])),
             (200, oa_text("done")))
    status, out = _ask(base, "oa", "hi")
    assert out["status"] == "completed" and out["usage"]["tool_calls"] == 4, out
    sent = stub.requests()[-1]["body"]["messages"]
    assert_replayable_openai(sent)
    ids = [c["id"] for c in sent[1]["tool_calls"]]
    assert len(set(ids)) == 4 and all(ids), ids


def test_usage_reported_as_floats_is_charged(wire):
    base, stub = wire
    stub.set((200, oa_text("x", usage={"prompt_tokens": 12.0, "completion_tokens": 3.0})))
    _, out = _ask(base, "oa", "hi")
    assert (out["usage"]["input_tokens"], out["usage"]["output_tokens"]) == (12, 3), out["usage"]


# --------------------------------------------------------------------------- session hygiene


def test_a_tool_call_cut_off_by_max_tokens_does_not_poison_the_session(wire):
    # 2.5.3 saved the unanswered tool call; every later turn was a 400.
    base, stub = wire
    stub.set((200, oa_calls([{"id": "c1", "name": "record", "arguments": '{"note": "x'}], finish="length")),
             (200, oa_text("second")))
    _, first = _ask(base, "oa", "hi", session_id="cut")
    assert first["status"] == "max_tokens" and first["code"] == "WC312"
    _, second = _ask(base, "oa", "again", session_id="cut")
    assert second["status"] == "completed"
    assert_replayable_openai(stub.requests()[-1]["body"]["messages"])


def test_an_empty_answer_does_not_poison_the_session(wire):
    base, stub = wire
    empty = {"model": "m", "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": ""}}]}
    stub.set((200, empty), (200, oa_text("x")))
    _ask(base, "oa", "hi", session_id="empty")
    _, out = _ask(base, "oa", "again", session_id="empty")
    assert out["status"] == "completed"
    sent = stub.requests()[-1]["body"]["messages"]
    assert_replayable_openai(sent)
    assert [m["role"] for m in sent] == ["user", "user"]


# --------------------------------------------------------------------------- the Anthropic wire


def test_anthropic_overload_is_retried_and_garbled_input_is_refused(wire):
    base, stub = wire
    before = len(_recorded(base))
    garbled = {"model": "m", "stop_reason": "tool_use", "usage": {"input_tokens": 3, "output_tokens": 3},
               "content": [{"type": "tool_use", "id": "t1", "name": "record", "input": "note=x"},
                           {"type": "tool_use", "name": "ping", "input": {}}]}
    stub.set((529, {"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
             (200, garbled), (200, an_text("fine")))
    status, out = _ask(base, "an", "hi")
    assert status == 200 and out["status"] == "completed", out
    assert _recorded(base)[before:] == []
    kinds = [(s["kind"], s.get("code")) for s in out["steps"]]
    assert ("tool_refused", "WC317") in kinds and ("tool_call", None) in kinds, kinds
    sent = stub.requests()[-1]
    assert sent["path"] == "/v1/messages"
    assistant = sent["body"]["messages"][1]["content"]
    assert assistant[0]["input"] == {}, "Anthropic replays only an object"
    assert assistant[1]["id"], "a missing tool_use id is filled in"
    results = sent["body"]["messages"][2]["content"]
    assert [r["tool_use_id"] for r in results] == [b["id"] for b in assistant]


def test_anthropic_without_a_content_array_is_wc305(wire):
    base, stub = wire
    stub.set((200, {"type": "message", "stop_reason": "end_turn"}))
    status, out = _ask(base, "an", "hi")
    assert status == 502 and out["code"] == "WC305", out


# --------------------------------------------------------------------------- concurrency on the wire


def test_concurrent_runs_are_charged_exactly(wire):
    base, stub = wire
    n = 24
    stub.set(*[(200, lambda body: oa_text("ok", usage={"prompt_tokens": 7, "completion_tokens": 3}))] * n)
    before = _request(base, "/_webcortex/usage")[1]
    with concurrent.futures.ThreadPoolExecutor(8) as pool:
        results = list(pool.map(lambda i: _ask(base, "oa", f"q{i}"), range(n)))
    assert all(s == 200 and o["status"] == "completed" for s, o in results), results
    after = _request(base, "/_webcortex/usage")[1]
    assert after["totals"]["calls"] - before["totals"]["calls"] == n
    assert after["total_tokens"] - before["total_tokens"] == n * 10
    status = _request(base, "/_webcortex/status")[1]
    assert status["agents"]["in_flight"] == [], "nothing left in flight"


# =========================================================================== FakeProvider server

FAKE_APP = '''
from webcortex import WebCortex

app = WebCortex("fake", port=__PORT__)
app.api_key("STRESS_ADMIN", id="admin", scopes=["read", "write", "webcortex:admin"])
app.api_key("STRESS_USER", id="user", scopes=["read", "write"])

app.static("GET", "/ping", {"pong": True}, tool=True, tool_name="ping")
app.static("GET", "/nuke", {"nuked": True}, tool=True, tool_name="nuke", approval="required")

@app.get("/huge_int", tool=True, tool_name="huge_int")
def huge_int() -> dict:
    return {"v": 2**64 - 1, "neg": -(2**63)}

app.agent("talker", tools=["ping"], max_steps=3)
app.agent("gated", tools=["nuke"], max_steps=3)

@app.behaviour("stubborn", tools=[], token_budget=25, max_steps=200)
def stubborn(ctx, input):
    # Catches every halt and asks again: the budget must still bound it.
    answered = halted = 0
    for _ in range(50):
        try:
            ctx.ask("again")
            answered += 1
        except Exception as e:
            if type(e).__name__ != "BehaviourHalted":
                raise
            halted += 1
    return {"answered": answered, "halted": halted, "usage": ctx.usage}

@app.behaviour("batch", tools=[], token_budget=15, max_steps=50)
def batch(ctx, input):
    ctx.ask_many(["a", "b", "c", "d", "e"])
    return {"unreachable": True}

@app.behaviour("ints", tools=["huge_int"])
def ints(ctx, input):
    got = ctx.call("huge_int")
    return {"is_int": isinstance(got["v"], int), "v": got["v"], "neg": got["neg"], "back": 2**64 - 1}
'''


@pytest.fixture(scope="module")
def fake():
    base, proc, log = _start(FAKE_APP, {"WEBCORTEX_FAKE_PROVIDER": "1"})
    yield base
    _stop(proc)


def _usage_calls(base, caller):
    usage = _request(base, "/_webcortex/usage")[1]
    return usage["by_caller"].get(caller, {}).get("calls", 0)


# --------------------------------------------------------------------------- behaviours


def test_a_behaviour_that_swallows_its_halt_cannot_keep_spending(fake):
    # 2.5.3 checked the budget only after each call, so `except: continue`
    # made — and paid for — every one of the fifty.
    status, out = _request(fake, "/behaviours/stubborn", "POST", {})
    assert status == 200, out
    assert out["answered"] + out["halted"] == 50
    calls = _usage_calls(fake, "behaviour:stubborn")
    assert calls <= 3, f"{calls} model calls on a 25-token budget"
    assert out["usage"]["input_tokens"] + out["usage"]["output_tokens"] <= 25 + 15


def test_every_answer_of_an_ask_many_batch_is_charged(fake):
    # 2.5.3 stopped charging at the first answer over budget: the rest of the
    # batch was spent but never reached the ledger or the shared budget.
    status, out = _request(fake, "/behaviours/batch", "POST", {})
    assert status == 200 and out.get("halted") is True, out
    assert _usage_calls(fake, "behaviour:batch") == 5


def test_integers_beyond_i64_cross_the_bridge_as_integers(fake):
    status, out = _request(fake, "/behaviours/ints", "POST", {})
    assert status == 200, out
    assert out == {"is_int": True, "v": 2**64 - 1, "neg": -(2**63), "back": 2**64 - 1}


@pytest.mark.parametrize("kind", ["agent", "behaviour"])
def test_max_tokens_zero_is_a_boot_error(kind):
    app = WebCortex("t")
    if kind == "agent":
        app.agent("a", max_tokens=0)
    else:
        app.behaviour("b", max_tokens=0)(lambda ctx, input: {})
    with pytest.raises(ValueError, match="max_tokens=0"):
        app.check()


# --------------------------------------------------------------------------- approvals


def test_concurrent_decisions_resume_an_approval_exactly_once(fake):
    status, out = _request(fake, "/agents/gated", "POST", {"input": "tool:nuke {}"})
    assert status == 202 and out["status"] == "awaiting_approval", out
    approval = out["pending_approval"]["approval_id"]

    def decide(_):
        return _request(fake, f"/_webcortex/approvals/{approval}", "POST", {"approve": True})

    with concurrent.futures.ThreadPoolExecutor(8) as pool:
        results = list(pool.map(decide, range(8)))
    codes = sorted(s for s, _ in results)
    assert codes.count(200) == 1, codes
    assert all(o["error"]["code"] == "WC310" for s, o in results if s != 200)


def test_approval_endpoint_edge_cases(fake):
    s, out = _request(fake, "/_webcortex/approvals/not-a-real-id", "POST", {"approve": True})
    assert s == 404 and out["error"]["code"] == "WC310"
    s, out = _request(fake, "/agents/gated", "POST", {"input": "tool:nuke {}"})
    approval = out["pending_approval"]["approval_id"]
    for bad in ({"approve": "yes"}, {}, None):
        s, out = _request(fake, f"/_webcortex/approvals/{approval}", "POST", bad)
        assert s == 400 and out["error"]["code"] == "WC205", (bad, out)
    s, out = _request(fake, f"/_webcortex/approvals/{approval}", "POST", {"approve": False, "note": 5})
    assert s == 200 and any(st["kind"] == "approval_denied" for st in out["steps"])
    s, _ = _request(fake, f"/_webcortex/approvals/{approval}", "POST", {"approve": True})
    assert s == 404, "a decision is consumed"


# --------------------------------------------------------------------------- sessions


def test_a_long_session_stays_bounded(fake):
    # 2.5.3 kept every turn of a session: memory grew without limit, and so
    # did every later request. The FakeProvider's input tokens scale with the
    # conversation, so a plateau shows the bound.
    tokens = []
    for i in range(260):
        s, out = _request(fake, "/agents/talker", "POST", {"input": f"turn {i:04d}", "session_id": "long"},
                          key=USER_KEY)
        assert s == 200, out
        tokens.append(out["usage"]["input_tokens"])
    assert tokens[150] > tokens[10] * 5, "the history did grow at first"
    assert max(tokens[230:]) <= tokens[205] * 1.05, (tokens[205], max(tokens[230:]))


# --------------------------------------------------------------------------- concurrency and the status view


def test_many_concurrent_runs_are_all_accounted_for(fake):
    before = _request(fake, "/_webcortex/status")[1]
    tally = next((b for b in before["budgets"] if b["name"] == "talker"), {})
    runs_before = tally.get("runs", 0)
    n = 40

    def one(i):
        body = {"input": "tool:ping {}" if i % 2 else f"hello {i}"}
        return _request(fake, "/agents/talker", "POST", body, key=USER_KEY)

    with concurrent.futures.ThreadPoolExecutor(10) as pool:
        results = list(pool.map(one, range(n)))
    assert all(s == 200 and o["status"] == "completed" for s, o in results), results
    after = _request(fake, "/_webcortex/status")[1]
    assert after["agents"]["in_flight"] == []
    tally = next(b for b in after["budgets"] if b["name"] == "talker")
    assert tally["runs"] - runs_before == n


# --------------------------------------------------------------------------- MCP


def _mcp(base, body=None, raw=None):
    return _request(base, "/_webcortex/mcp", "POST", body, raw=raw)


def test_mcp_negotiates_the_protocol_version(fake):
    for asked, answered in (("2024-11-05", "2024-11-05"), ("2025-03-26", "2025-03-26"),
                            ("2025-06-18", "2025-06-18"), ("2031-01-01", "2025-06-18")):
        _, out = _mcp(fake, {"jsonrpc": "2.0", "id": 1, "method": "initialize",
                             "params": {"protocolVersion": asked, "capabilities": {},
                                        "clientInfo": {"name": "t", "version": "1"}}})
        assert out["result"]["protocolVersion"] == answered, asked


def test_mcp_answers_invalid_messages_instead_of_dropping_them(fake):
    s, out = _mcp(fake, [1, "x", {"jsonrpc": "2.0", "id": 2, "method": "ping"}])
    assert s == 200 and len(out) == 3, out
    assert [o.get("error", {}).get("code") for o in out[:2]] == [-32600, -32600]
    assert out[2]["result"] == {}
    s, out = _mcp(fake, [])
    assert s == 200 and out["error"]["code"] == -32600
    s, out = _mcp(fake, {"jsonrpc": "2.0", "id": {"nested": 1}, "method": "ping"})
    assert out["error"]["code"] == -32600 and out["id"] is None
    s, out = _mcp(fake, {"jsonrpc": "2.0", "id": 9, "result": {}})
    assert s == 202 and out is None, "a client's response is never answered"
    s, out = _mcp(fake, raw=b"[" + b"{" * 64)
    assert s == 200 and out["error"]["code"] == -32700


def test_mcp_refuses_tool_arguments_that_are_not_an_object(fake):
    for bad in ([1], "x", 3):
        _, out = _mcp(fake, {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                             "params": {"name": "ping", "arguments": bad}})
        assert out["error"]["code"] == -32602 and out["error"]["data"]["code"] == "WC206", out


def test_mcp_survives_a_full_batch_of_concurrent_callers(fake):
    batch = [{"jsonrpc": "2.0", "id": i, "method": "tools/call",
              "params": {"name": "huge_int" if i % 2 else "ping", "arguments": {}}} for i in range(32)]

    def one(_):
        return _mcp(fake, batch)

    with concurrent.futures.ThreadPoolExecutor(6) as pool:
        results = list(pool.map(one, range(12)))
    for s, out in results:
        assert s == 200 and [o["id"] for o in out] == list(range(32))
        assert all(isinstance(o["result"]["structuredContent"], dict) for o in out)
        assert out[1]["result"]["structuredContent"]["v"] == 2**64 - 1


# --------------------------------------------------------------------------- 2.6.1: sessions, sockets, MCP


def _user_inputs(messages):
    return [m["content"] for m in messages if m["role"] == "user" and isinstance(m["content"], str)]


def _slow(text, secs):
    def answer(_body):
        time.sleep(secs)
        return oa_text(text)
    return answer


def test_concurrent_turns_on_one_session_all_land(wire):
    # 2.6.0: each turn loaded the same history and the last to finish saved
    # over the others, so three of these four turns vanished from the session.
    base, stub = wire
    stub.set(*[(200, _slow(f"a{i}", 0.3)) for i in range(4)], (200, oa_text("last")))
    with concurrent.futures.ThreadPoolExecutor(4) as pool:
        results = list(pool.map(lambda i: _ask(base, "oa", f"turn {i}", session_id="race"), range(4)))
    assert all(s == 200 and o["status"] == "completed" for s, o in results), results
    _, out = _ask(base, "oa", "what did I say?", session_id="race")
    assert out["status"] == "completed"
    said = _user_inputs(stub.requests()[-1]["body"]["messages"])
    assert sorted(said[:-1]) == [f"turn {i}" for i in range(4)], said
    assert_replayable_openai(stub.requests()[-1]["body"]["messages"])


def test_a_resumed_approval_does_not_overwrite_turns_taken_while_it_waited(wire):
    # 2.6.0: the resumed run saved the history it had when it suspended.
    base, stub = wire
    stub.set((200, oa_calls([{"id": "n1", "name": "nuke"}])), (200, oa_text("two")),
             (200, oa_text("approved")), (200, oa_text("three")))
    s, first = _ask(base, "oag", "first", session_id="gate")
    assert s == 202 and first["status"] == "awaiting_approval", first
    _, second = _ask(base, "oag", "second", session_id="gate")
    assert second["output"] == "two"
    s, resumed = _request(base, f"/_webcortex/approvals/{first['pending_approval']['approval_id']}",
                          "POST", {"approve": True})
    assert s == 200 and resumed["output"] == "approved", resumed
    _ask(base, "oag", "third", session_id="gate")
    messages = stub.requests()[-1]["body"]["messages"]
    assert _user_inputs(messages) == ["second", "first", "third"], messages
    assert_replayable_openai(messages)


def _in_flight(base):
    return _request(base, "/_webcortex/status")[1]["agents"]["in_flight"]


def test_closing_an_agent_socket_mid_turn_cancels_the_run(wire):
    # 2.6.0 did not read the socket during a turn: the run kept going (and
    # paying for model calls) until the model's step came back.
    base, stub = wire
    stub.set((200, _slow("too late", 6)))
    sock = AgentSocket(base, "/agents/oa", key=ADMIN_KEY, timeout=30)
    sock.ws.send_json({"input": "take your time"})
    deadline = time.time() + 5
    while not _in_flight(base) and time.time() < deadline:
        time.sleep(0.05)
    assert _in_flight(base), "the turn never started"
    sock.close()
    closed = time.time()
    while _in_flight(base) and time.time() - closed < 3:
        time.sleep(0.05)
    assert _in_flight(base) == [], "the run outlived its client"
    assert time.time() - closed < 3


def test_turns_sent_ahead_on_an_agent_socket_are_answered_in_order(wire):
    base, stub = wire
    stub.set((200, _slow("one", 0.3)), (200, oa_text("two")), (200, oa_text("three")))
    with AgentSocket(base, "/agents/oa", key=ADMIN_KEY, timeout=30) as sock:
        for text in ("a", "b", "c"):
            sock.ws.send_json({"input": text})
        outputs = []
        while len(outputs) < 3:
            event = sock.ws.recv_json()
            if event["type"] == "result":
                outputs.append(event["result"]["output"])
    assert outputs == ["one", "two", "three"]


def test_mcp_unknown_tool_is_a_protocol_error(fake):
    # MCP 2025-06-18 lists an unknown tool as a JSON-RPC error; 2.6.0 answered
    # with a tool result flagged isError.
    _, out = _mcp(fake, {"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                         "params": {"name": "no_such_tool", "arguments": {}}})
    assert out["id"] == 7 and "result" not in out, out
    assert out["error"]["code"] == -32602 and out["error"]["data"]["code"] == "WC314", out
