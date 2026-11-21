"""2.9: the Rust leaf modules — output validator, templates, database binding,
router, TypeScript client, traces and OTLP export, cassette fingerprints,
providers and the image-URL filter.

Each test is a regression test for a defect found by the 2.9 stress test.
Declarations and evals run in-process; everything that needs HTTP runs a real
server on 127.0.0.1 against the deterministic fake provider, with a local OTLP
collector or a local stand-in for a model provider beside it where needed.
"""

from __future__ import annotations

import http.server
import json
import os
import random
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path

import pytest

from webcortex import WebCortex


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Recorder:
    """A local HTTP endpoint that remembers every POST. As an OTLP collector it
    answers `{}`; as a model provider it answers in the wire format of the path."""

    def __init__(self) -> None:
        self.requests: list[tuple[str, dict, bytes]] = []
        recorder = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                raw = self.rfile.read(int(self.headers.get("content-length", 0)))
                recorder.requests.append((self.path, {k.lower(): v for k, v in self.headers.items()}, raw))
                if self.path.endswith("/chat/completions"):
                    out = {"id": "x", "model": "m", "choices": [{"index": 0, "finish_reason": "stop",
                           "message": {"role": "assistant", "content": "ok"}}],
                           "usage": {"prompt_tokens": 1, "completion_tokens": 1}}
                elif self.path.endswith("/messages"):
                    out = {"id": "x", "model": "m", "role": "assistant", "stop_reason": "end_turn",
                           "content": [{"type": "text", "text": "ok"}],
                           "usage": {"input_tokens": 1, "output_tokens": 1}}
                else:
                    out = {}
                data = json.dumps(out).encode()
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def spans(self) -> list[dict]:
        out = []
        for path, _, raw in list(self.requests):
            if path == "/v1/traces":
                for resource in json.loads(raw)["resourceSpans"]:
                    for scope in resource["scopeSpans"]:
                        out.extend(scope["spans"])
        return out


class Server:
    """`webcortex run` on a free port, in a temporary directory."""

    def __init__(self, source: str, files: dict[str, str] | None = None, env: dict | None = None,
                 fake: bool = True):
        self.port = _free_port()
        self.base = f"http://127.0.0.1:{self.port}"
        self.dir = Path(tempfile.mkdtemp(prefix="webcortex-2-9-core-"))
        (self.dir / "api.py").write_text(source.replace("{PORT}", str(self.port)))
        for rel, text in (files or {}).items():
            (self.dir / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.dir / rel).write_text(text)
        e = {k: v for k, v in os.environ.items() if not k.startswith(("ANTHROPIC", "OPENAI", "WEBCORTEX_", "OTEL_"))}
        e["WEBCORTEX_LOG"] = "warn"
        if fake:
            e["WEBCORTEX_FAKE_PROVIDER"] = "1"
        e.update(env or {})
        self.log_path = self.dir / "server.log"
        with self.log_path.open("w") as log:
            self.proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", "run", "api.py"],
                                         cwd=self.dir, stdout=log, stderr=subprocess.STDOUT, env=e)
        deadline = time.time() + 45
        while time.time() < deadline:
            if self.proc.poll() is not None:
                pytest.fail(f"server exited early:\n{self.log_path.read_text()}")
            try:
                with urllib.request.urlopen(self.base + "/_webcortex/health", timeout=1):
                    return
            except Exception:
                time.sleep(0.1)
        self.stop()
        pytest.fail(f"server never became ready:\n{self.log_path.read_text()}")

    def request(self, method: str, path: str, body=None, raw: bytes | None = None):
        data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
        req = urllib.request.Request(self.base + path, data=data, method=method,
                                     headers={"content-type": "application/json"} if data else {})
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                return r.status, r.read(), {k.lower(): v for k, v in r.headers.items()}
        except urllib.error.HTTPError as e:
            return e.code, e.read(), {k.lower(): v for k, v in e.headers.items()}

    def json(self, method: str, path: str, body=None, raw: bytes | None = None):
        status, data, headers = self.request(method, path, body, raw)
        return status, (json.loads(data) if data else None), headers

    def ask(self, agent: str, input: str, **extra):
        return self.json("POST", f"/agents/{agent}", {"input": input, **extra})

    def stop(self) -> str:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        return self.log_path.read_text()


TREE = {
    "type": "object",
    "properties": {"root": {"$ref": "#/$defs/Node"}},
    "required": ["root"],
    "$defs": {"Node": {
        "type": "object",
        "properties": {"value": {"type": "integer"}, "children": {"type": "array", "items": {"$ref": "#/$defs/Node"}}},
        "required": ["value"],
        "additionalProperties": False,
    }},
}

APP = '''
import time
from dataclasses import dataclass
from webcortex import WebCortex

app = WebCortex("coredata", port={PORT}, database="sqlite://:memory:", templates="templates")
app.tracing(otlp_endpoint="http://127.0.0.1:{COLLECTOR}", otlp_headers_env="OTLP_HEADERS", keep=2)

# Templates
@app.page_handler("/doc", "doc.html")
def doc(name: str = "") -> dict:
    return {"name": name}

@app.page_handler("/attr", "attr.html")
def attr(name: str = "") -> dict:
    return {"name": name}

@app.page_handler("/echo.txt", "echo.txt")
def echo_txt(q: str = "") -> dict:
    return {"q": q}

@app.page_handler("/echo.json", "echo.json")
def echo_json(q: str = "") -> dict:
    return {"q": q}

# Database
app.resource("accounts", fields={"id": int, "balance_cents": int})
app.resource("notes", fields={"id": int, "text": str})
app.query("POST", "/accounts/set", "UPDATE accounts SET balance_cents = ? WHERE id = ?",
          params=["balance_cents", "id"], returns="affected")
app.query("POST", "/accounts/add", "INSERT OR IGNORE INTO accounts (id, balance_cents) VALUES (?, ?)",
          params=["id", "balance_cents"], returns="affected")

# Router
@app.get("/café/{n}")
def cafe(n: str) -> dict:
    return {"n": n}

@app.get("/ping")
def ping() -> dict:
    return {"pong": 1}

# Agents
app.agent("looker", system="describe the image")

@dataclass
class Tags:
    tags: set[str]

app.agent("tagger", output=Tags, max_steps=1)
app.agent("labeller", max_steps=1, output={"type": "object", "required": ["labels"], "properties": {"labels": {
    "type": "object", "patternProperties": {"^x-": {"type": "string"}}, "additionalProperties": False}}})
app.agent("triage", max_steps=1, output={
    "type": "object", "required": ["category", "urgency"],
    "properties": {"category": {"enum": ["bug", "billing"]}, "urgency": {"type": "integer"}}})
app.agent("tree", max_steps=1, output=TREE)
app.agent("number", max_steps=1, output={"type": "object", "properties": {"x": {"type": "number"}}, "required": ["x"]})

@app.get("/value", tool=True, tool_name="value")
def value(x: str) -> dict:
    return {"x": float.fromhex(x)}

@app.behaviour("roundtrip", tools=["value"])
def roundtrip(ctx, input):
    return {"equal": [ctx.call("value", x=h)["x"] == float.fromhex(h) for h in input["hex"]]}

# OTLP
@app.get("/slow", tool=True, tool_name="slow")
def slow() -> dict:
    time.sleep(0.4)
    return {"ok": True}

app.agent("worker", tools=["slow"])
'''

TEMPLATES = {
    "templates/doc.html": "<script>const d = {{ data | json }}</script>",
    "templates/attr.html": "<div data-x='{{ data | tojson }}'></div>",
    "templates/echo.txt": "hello {{ data.q }}",
    "templates/echo.json": '{"q": {{ data.q }}}',
}


@pytest.fixture(scope="module")
def collector():
    c = Recorder()
    yield c
    c.server.shutdown()


@pytest.fixture(scope="module")
def server(collector):
    s = Server(APP.replace("{COLLECTOR}", str(collector.port)).replace("output=TREE", f"output={TREE!r}"),
               files=TEMPLATES, env={"OTLP_HEADERS": "authorization=Bearer%20otlp-secret, x-team = core"})
    yield s
    s.stop()


def _steps_with(body: dict, code: str) -> list[str]:
    return [s.get("error") or "" for s in body.get("steps", []) if s.get("code") == code]


# ---------------------------------------------------------------- M7: the json filter


def test_the_json_filter_is_valid_javascript_and_script_safe(server):
    payload = "</script><script>alert(document.cookie)</script><!-- ' &"
    status, body, headers = server.request("GET", "/doc?" + urllib.parse.urlencode({"name": payload}))
    assert status == 200 and headers["content-type"] == "text/html; charset=utf-8"
    text = body.decode()
    inner = text.removeprefix("<script>const d = ").removesuffix("</script>")
    assert inner != text, text
    assert "&quot;" not in inner, "the documented form must be JavaScript, not HTML-escaped text"
    assert not any(c in inner for c in "<>&'"), inner
    assert json.loads(inner) == {"name": payload}


def test_tojson_is_an_alias_and_safe_in_a_single_quoted_attribute(server):
    payload = "'><script>alert(1)</script>"
    status, body, _ = server.request("GET", "/attr?" + urllib.parse.urlencode({"name": payload}))
    text = body.decode()
    assert status == 200 and "<script>" not in text and text.count("'") == 2, text
    assert json.loads(text.removeprefix("<div data-x='").removesuffix("'></div>")) == {"name": payload}


# ---------------------------------------------------------------- M8: content type by extension


def test_a_txt_page_is_plain_text_not_html(server):
    status, body, headers = server.request("GET", "/echo.txt?q=%3Cscript%3Ealert(1)%3C/script%3E")
    assert status == 200
    assert headers["content-type"] == "text/plain; charset=utf-8"
    assert headers.get("x-content-type-options") == "nosniff"
    assert body == b"hello <script>alert(1)</script>"


def test_a_json_page_is_json_and_writes_values_as_json(server):
    q = 'x", "admin": true, "y": "<b>'
    status, body, headers = server.request("GET", "/echo.json?" + urllib.parse.urlencode({"q": q}))
    assert status == 200 and headers["content-type"] == "application/json"
    assert json.loads(body) == {"q": q}, "the value cannot break out of its string"


# ---------------------------------------------------------------- L8, L33: database binding


@pytest.mark.parametrize("raw", ["18446744073709551615", "9223372036854775808", "123456789012345678901234567890"])
def test_an_integer_outside_64_bits_is_refused_not_rounded(server, raw):
    status, body, _ = server.json("POST", "/accounts", raw=('{"balance_cents": %s}' % raw).encode())
    assert status == 400 and body["error"]["code"] == "WC211", body


def test_integers_that_fit_are_stored_exactly(server):
    status, body, _ = server.json("POST", "/accounts", raw=b'{"balance_cents": 9223372036854775807}')
    assert status in (200, 201) and body["balance_cents"] == 9223372036854775807, body


def test_affected_reports_no_stale_insert_id(server):
    status, created, _ = server.json("POST", "/accounts", {"balance_cents": 1})
    assert status in (200, 201)
    for i in range(3):
        server.json("POST", "/notes", {"text": f"n{i}"})
    status, body, _ = server.json("POST", "/accounts/set", {"balance_cents": 9, "id": created["id"]})
    assert status == 200 and body == {"affected": 1, "last_insert_id": None}
    status, body, _ = server.json("POST", "/accounts/add", {"id": created["id"], "balance_cents": 5})
    assert body == {"affected": 0, "last_insert_id": None}
    status, body, _ = server.json("POST", "/accounts/add", {"id": 777, "balance_cents": 5})
    assert body == {"affected": 1, "last_insert_id": 777}


# ---------------------------------------------------------------- L30: non-ASCII routes


@pytest.mark.parametrize("path", ["/caf%C3%A9/x", "/caf%c3%a9/x"])
def test_a_non_ascii_route_answers_a_percent_encoded_path(server, path):
    status, body, _ = server.json("GET", path)
    assert status == 200 and body == {"n": "x"}


def test_a_non_ascii_route_answers_raw_utf8_and_encoded_letters_match(server):
    with socket.create_connection(("127.0.0.1", server.port), timeout=10) as s:
        s.sendall("GET /café/x HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".encode())
        assert s.recv(200).split(b"\r\n")[0] == b"HTTP/1.1 200 OK"
    assert server.json("GET", "/%70ing")[:2] == (200, {"pong": 1})
    assert server.json("GET", "/caf%C3%A9/caf%c3%a9")[1] == {"n": "café"}, "a parameter is decoded once"


# ---------------------------------------------------------------- L13: image URL filter


@pytest.mark.parametrize("url", ["http://[::7f00:1]/", "http://[64:ff9b::a9fe:a9fe]/", "http://[2002:a9fe:a9fe::]/",
                                 "http://[fec0::1]/", "http://[64:ff9b:1::a00:1]/"])
def test_other_spellings_of_internal_addresses_are_refused(server, url):
    status, body, _ = server.json("POST", "/agents/looker", {"input": "hi", "images": [{"$image": {"url": url}}]})
    assert status == 400 and "may not point at" in json.dumps(body), body


def test_a_public_ipv6_image_url_is_still_accepted(server):
    status, body, _ = server.json("POST", "/agents/looker",
                                  {"input": "hi", "images": [{"$image": {"url": "https://[2002:808:808::]/a.png"}}]})
    assert status == 200, body


# ---------------------------------------------------------------- the output validator


def test_set_fields_refuse_duplicates(server):
    status, body, _ = server.ask("tagger", 'tool:final_answer {"tags": ["urgent", "urgent"]}')
    assert body["status"] != "completed" and "items 0 and 1 are equal" in " ".join(_steps_with(body, "WC318")), body
    status, body, _ = server.ask("tagger", 'tool:final_answer {"tags": ["urgent", "later"]}')
    assert body["status"] == "completed" and sorted(body["data"]["tags"]) == ["later", "urgent"]


def test_pattern_properties_do_not_make_a_valid_answer_fail(server):
    status, body, _ = server.ask("labeller", 'tool:final_answer {"labels": {"x-team": "core"}}')
    assert body["status"] == "completed" and body["data"] == {"labels": {"x-team": "core"}}, body


def test_a_recursive_schema_is_checked_at_every_depth(server):
    good = {"root": {"value": 1, "children": [{"value": 2, "children": [{"value": 3}]}]}}
    status, body, _ = server.ask("tree", "tool:final_answer " + json.dumps(good))
    assert body["status"] == "completed" and body["data"] == good, body
    bad = {"root": {"value": 1, "children": [{"value": 2, "children": [{"value": "three"}]}]}}
    status, body, _ = server.ask("tree", "tool:final_answer " + json.dumps(bad))
    assert body["status"] != "completed"
    assert "$.root.children[0].children[0].value: expected integer, got string" in " ".join(_steps_with(body, "WC318"))


@pytest.mark.parametrize("text", [
    'Here you go: {"category": "billing", "urgency": 3}',
    'The fields are {category, urgency}. Answer: {"category": "billing", "urgency": 3}',
    'First guess {"category": "other"}, final answer {"category": "billing", "urgency": 3}',
])
def test_the_last_json_in_prose_is_recovered(server, text):
    status, body, _ = server.ask("triage", text)
    assert body["status"] == "completed" and body["data"] == {"category": "billing", "urgency": 3}, body


def _judge(cases: list[tuple[dict, object]]) -> list[bool]:
    """Ask the runtime validator, through typed agents, whether each value matches."""
    app = WebCortex("judge")
    for i, (schema, value) in enumerate(cases):
        app.agent(f"a{i}", output={"type": "object", "properties": {"v": schema}, "required": ["v"]}, max_steps=1)
        app.eval(f"e{i}", target=f"a{i}", input="tool:final_answer " + json.dumps({"v": value}),
                 expect={"status": "completed"})
    return [r["ok"] for r in app.run_evals(mode="live")["results"]]


@pytest.fixture
def fake(monkeypatch):
    monkeypatch.setenv("WEBCORTEX_FAKE_PROVIDER", "1")
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)


def test_count_keywords_written_as_integral_floats_are_enforced(fake):
    assert _judge([
        ({"type": "array", "minItems": 2.0}, [1]),
        ({"type": "array", "maxItems": 1.0}, [1, 2, 3]),
        ({"type": "string", "maxLength": 2.0}, "abcdef"),
        ({"type": "string", "minLength": 3.0}, ""),
        ({"type": "string", "minLength": 3.0}, "abc"),
    ]) == [False, False, False, False, True]


def test_enum_and_const_compare_numbers_at_any_depth_and_big_integers_exactly(fake):
    two53 = 2 ** 53
    assert _judge([
        ({"const": [1]}, [1.0]),
        ({"const": {"k": 1}}, {"k": 1.0}),
        ({"enum": [[1, 2], "x"]}, [1.0, 2]),
        ({"enum": [{"a": 2.0}]}, {"a": 2}),
        ({"type": "integer", "maximum": two53}, two53),
        ({"type": "integer", "maximum": two53}, two53 + 1),
        ({"type": "integer", "exclusiveMaximum": two53 + 1}, two53 + 1),
        ({"type": "integer", "minimum": 2 ** 63}, 2 ** 63 - 1),
        ({"enum": [two53 + 1]}, two53),
        ({"const": 2 ** 54 + 1}, 2 ** 54),
    ]) == [True, True, True, True, True, False, False, False, False, False]


@pytest.mark.parametrize("schema, why", [
    ({"type": "object", "properties": {"v": {"type": "array", "maxItems": -1}}}, "maxItems must be a non-negative integer"),
    ({"type": "object", "properties": {"v": {"type": "string", "minLength": 1.5}}}, "minLength must be a non-negative integer"),
    ({"type": "object", "required": "v"}, "required must be a list of property names"),
    ({"type": "object", "properties": []}, "properties must be an object"),
    ({"type": "object", "properties": {"v": {"type": "str"}}}, "is not a JSON Schema type"),
    ({"type": "object", "properties": {"v": {"enum": "a"}}}, "enum must be a list"),
    ({"type": "object", "properties": {"v": {"$ref": "#/$defs/Missing"}}}, "names no definition"),
])
def test_a_malformed_output_schema_fails_at_boot(schema, why):
    app = WebCortex("t")
    app.agent("a", output=schema)
    with pytest.raises(ValueError, match=why):
        app.check()


def test_a_recursive_schema_with_defs_boots(fake):
    app = WebCortex("t")
    app.agent("a", output=TREE)
    app.check()


# ---------------------------------------------------------------- L17: floats round-trip


def test_floats_survive_the_core_exactly(server):
    rnd = random.Random(9)
    doubles = [-943305.0469559873] + [rnd.uniform(-1e6, 1e6) for _ in range(60)] + [rnd.random() * 10 ** rnd.randint(-300, 300)
                                                                                      for _ in range(20)]
    for v in doubles:
        status, body, _ = server.ask("number", "tool:final_answer " + json.dumps({"x": v}))
        assert body["status"] == "completed" and body["data"]["x"] == v, (v, body.get("data"))
    for i in range(0, len(doubles), 40):  # a behaviour run has a budget of 50 steps
        chunk = doubles[i:i + 40]
        status, body, _ = server.json("POST", "/behaviours/roundtrip", {"hex": [v.hex() for v in chunk]})
        assert status == 200 and body.get("equal") == [True] * len(chunk), body


# ---------------------------------------------------------------- L24: temperature on the wire


def test_temperature_goes_on_the_wire_as_written():
    stub = Recorder()
    source = f'''
from webcortex import WebCortex
app = WebCortex("temp", port={{PORT}})
app.provider("stub", base_url="http://127.0.0.1:{stub.port}/v1", kind="openai")
app.provider("astub", base_url="http://127.0.0.1:{stub.port}", kind="anthropic", api_key_env="STUB_KEY")
app.agent("openai_wire", model="stub/m", temperature=0.9)
app.agent("anthropic_wire", model="astub/m", temperature=0.7)
'''
    s = Server(source, env={"STUB_KEY": "local-dummy"}, fake=False)
    try:
        sent = {}
        for agent, path in (("openai-wire", "/v1/chat/completions"), ("anthropic-wire", "/v1/messages")):
            stub.requests.clear()
            status, body, _ = s.ask(agent, "hi")
            assert status == 200, body
            (got_path, _, raw), = stub.requests
            assert got_path == path
            sent[agent] = raw.decode()
        assert '"temperature":0.9' in sent["openai-wire"], sent["openai-wire"]
        assert '"temperature":0.7' in sent["anthropic-wire"], sent["anthropic-wire"]
    finally:
        s.stop()
        stub.server.shutdown()


# ---------------------------------------------------------------- M12 + K3: cassette fingerprints


def _desk(fast: str = "claude-haiku-4-5", context: bool = False, rule: str = "no refunds over 50 GBP",
          provider_options: dict | None = None) -> WebCortex:
    app = WebCortex("fp", database="sqlite://:memory:")
    app.models(fast=fast)
    kw = {}
    if context:
        app.context("policy", data={"rule": rule})
        kw["context"] = ["policy"]
    if provider_options:
        app.request_options("anthropic", **provider_options)
    app.agent("desk", model="fast", system="Answer billing questions.", **kw)
    app.eval("desk_answers", target="desk", input="what is my balance?")
    return app


def test_a_cassette_goes_stale_when_the_alias_context_or_provider_options_change(tmp_path, fake):
    directory = tmp_path / "evals"
    assert _desk().run_evals(mode="record", directory=directory)["ok"]
    cassette = json.loads((directory / "desk_answers.json").read_text())
    assert all(c["fingerprint_version"] == 2 for c in cassette["calls"])
    assert _desk().run_evals(mode="replay", directory=directory)["ok"], "unchanged replays"
    for changed in (_desk(fast="claude-opus-5"), _desk(context=True), _desk(provider_options={"top_k": 5})):
        result = changed.run_evals(mode="replay", directory=directory)["results"][0]
        assert not result["ok"] and result["code"] == "WC319", result
        assert "alias points to" in result["failures"][0] and "context providers" in result["failures"][0]
    # Options for a provider the alias does not resolve to do not matter.
    app = _desk()
    app.request_options("openai", seed=3)
    assert app.run_evals(mode="replay", directory=directory)["ok"]
    # A change to a context provider's definition is a change too.
    assert _desk(context=True).run_evals(mode="record", directory=directory)["ok"]
    assert _desk(context=True).run_evals(mode="replay", directory=directory)["ok"]
    result = _desk(context=True, rule="no refunds at all").run_evals(mode="replay", directory=directory)["results"][0]
    assert result["code"] == "WC319"


def test_a_cassette_from_2_8_says_why_it_is_stale(tmp_path, fake):
    directory = tmp_path / "evals"
    assert _desk().run_evals(mode="record", directory=directory)["ok"]
    path = directory / "desk_answers.json"
    cassette = json.loads(path.read_text())
    for call in cassette["calls"]:
        del call["fingerprint_version"]
        call["fingerprint"] = "5d98eea00be35bdd"
    path.write_text(json.dumps(cassette))
    result = _desk().run_evals(mode="replay", directory=directory)["results"][0]
    assert result["code"] == "WC319" and "recorded by WebCortex 2.8 or earlier" in result["failures"][0]


# ---------------------------------------------------------------- L11: TypeScript client


def _node() -> str | None:
    return shutil.which("node") or (str(Path.home() / ".local/bin/node") if (Path.home() / ".local/bin/node").exists() else None)


def test_the_typescript_client_refuses_dot_segment_path_arguments():
    node = _node()
    if node is None:
        pytest.skip("node is not installed")
    app = WebCortex("ts")

    @app.post("/users/{id}/disable")
    def disable_user(id: str) -> dict:
        return {"id": id}

    @app.get("/files/{*rest}")
    def files(rest: str) -> dict:
        return {"rest": rest}

    ts = app.typescript_client()
    head = "for (const [k, v] of Object.entries(params ?? {}))"
    start = ts.index(head)
    i, depth = ts.index("{", start + len(head)), 0
    for j in range(i, len(ts)):
        depth += {"{": 1, "}": -1}.get(ts[j], 0)
        if depth == 0:
            loop = ts[start:j + 1]
            break
    js = """
class WebCortexError extends Error { constructor(status, body, message) { super(message); this.status = status; } }
function substitute(path, params) {
  let url = "http://127.0.0.1" + path; const query = new URLSearchParams(); const body = {}; const hasBody = true;
  LOOP
  return url;
}
const cases = JSON.parse(process.argv[1]);
console.log(JSON.stringify(cases.map(([path, params]) => {
  try { return {url: substitute(path, params)}; } catch (e) { return {error: e.message, status: e.status, type: e.constructor.name}; }
})));
""".replace("LOOP", loop)
    cases = [
        ["/users/{id}/disable", {"id": ".."}],
        ["/users/{id}/disable", {"id": "."}],
        ["/users/{id}/disable", {"id": ""}],
        ["/users/{id}/disable", {"id": "42"}],
        ["/users/{id}/disable", {"id": "..."}],
        ["/users/{id}/disable", {"id": "%2E%2E"}],
        ["/files/{*rest}", {"rest": "a/../b"}],
        ["/files/{*rest}", {"rest": "./a"}],
        ["/files/{*rest}", {"rest": "a/b c/d"}],
    ]
    out = subprocess.run([node, "-e", js, json.dumps(cases)], capture_output=True, text=True, timeout=30)
    assert out.returncode == 0, out.stderr
    results = json.loads(out.stdout)
    for r in results[:3] + results[6:8]:
        assert r.get("type") == "WebCortexError" and r["status"] == 0 and "different route" in r["error"], r
    assert results[3] == {"url": "http://127.0.0.1/users/42/disable"}
    assert results[4] == {"url": "http://127.0.0.1/users/.../disable"}
    assert results[5] == {"url": "http://127.0.0.1/users/%252E%252E/disable"}
    assert results[8] == {"url": "http://127.0.0.1/files/a/b%20c/d"}


def test_the_typescript_header_cannot_be_broken_out_of():
    app = WebCortex("billing\nglobalThis.PWNED = 'from app name';\n//", version="1.0 x")
    app.static("GET", "/ping", {"ok": True})
    ts = app.typescript_client()
    first, second = ts.splitlines()[:2]
    assert first.startswith("// Generated by `webcortex typegen` from billing globalThis.PWNED")
    assert second.startswith("// Do not edit")
    assert " " not in ts


# ---------------------------------------------------------------- K2, K4: OTLP export


def test_runs_beyond_keep_are_all_exported_with_decoded_headers(server, collector):
    with ThreadPoolExecutor(10) as pool:
        replies = list(pool.map(lambda _: server.ask("worker", "tool:slow {}"), range(10)))
    assert all(status == 200 and body["status"] == "completed" for status, body, _ in replies), replies
    trace_ids = {body["trace_id"] for _, body, _ in replies}
    assert len(trace_ids) == 10
    deadline = time.time() + 20
    while time.time() < deadline:
        roots = {s["traceId"] for s in collector.spans() if s["name"] == "agent worker" and "parentSpanId" not in s}
        if trace_ids <= roots:
            break
        time.sleep(0.2)
    else:
        pytest.fail(f"root spans exported for {len(trace_ids & roots)} of 10 runs")
    # Every span of every run arrived, not only the ones still held in memory.
    for trace_id in trace_ids:
        names = sorted(s["name"] for s in collector.spans() if s["traceId"] == trace_id)
        assert "tool slow" in names and "agent worker" in names, names
    _, headers, _ = next(r for r in collector.requests if r[0] == "/v1/traces")
    assert headers.get("authorization") == "Bearer otlp-secret" and headers.get("x-team") == "core"


def test_an_unusable_otlp_endpoint_is_a_boot_warning():
    s = Server('from webcortex import WebCortex\napp = WebCortex("w", port={PORT})\n',
               env={"OTEL_EXPORTER_OTLP_ENDPOINT": "collector:4317"})
    log = s.stop()
    assert "OTEL_EXPORTER_OTLP_ENDPOINT" in log and "cannot be used" in log and "http://" in log, log

