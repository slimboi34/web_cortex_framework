"""2.9.0: the Python application layer (app.py, schema.py, _bridge.py).

One section per defect from the 2.9.0 stress test. Declarations are checked
in-process; dispatch is driven directly through ``Dispatcher`` where that is
enough to show the property, and through a real server (the deterministic
fake provider, no network, no key) where only the whole path shows it.

Two tests need a Rust change from another 2.9.0 batch: the runtime's
``/_webcortex/security`` (core runtime) and the validator resolving ``$ref``
into ``$defs`` (core data).
"""

from __future__ import annotations

import importlib.util
import json
import os
import signal
import socket
import sqlite3
import subprocess
import sys
import textwrap
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional, TypedDict

import pytest

from webcortex import WebCortex
from webcortex._bridge import Dispatcher, InvalidJSON, Request
from webcortex.schema import coerce, json_schema_for, output_schema_for

KEY = "batch-c-test-key-0123456789abcdef"


# ---------------------------------------------------------------- helpers


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@contextmanager
def serve(tmp_path: Path, source: str, env: dict | None = None):
    """Run ``source`` as api.py with `webcortex run`; yield (base_url, process)."""
    port = _free_port()
    (tmp_path / "api.py").write_text(textwrap.dedent(source).replace("PORT", str(port)))
    log_path = tmp_path / "server.log"
    full_env = {**os.environ, "WEBCORTEX_FAKE_PROVIDER": "1", "WEBCORTEX_LOG": "warn",
                "WEBCORTEX_API_KEY": KEY, **(env or {})}
    full_env.pop("ANTHROPIC_API_KEY", None)
    with log_path.open("w") as log:
        proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", "run", "api.py"],
                                cwd=tmp_path, stdout=log, stderr=subprocess.STDOUT, env=full_env)
    base = f"http://127.0.0.1:{port}"
    try:
        deadline = time.time() + 45
        while True:
            if proc.poll() is not None:
                pytest.fail(f"server exited early:\n{log_path.read_text()}")
            try:
                with urllib.request.urlopen(base + "/_webcortex/health", timeout=1):
                    break
            except Exception:
                if time.time() > deadline:
                    pytest.fail(f"server never became ready:\n{log_path.read_text()}")
                time.sleep(0.1)
        yield base, proc
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait(10)


def call(base: str, method: str, path: str, body=None, *, headers: dict | None = None,
         timeout: float = 20):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method=method,
                                 headers={"content-type": "application/json", **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return r.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw) if raw else None
        except ValueError:
            return e.code, raw.decode(errors="replace")


def _request(body: bytes = b"") -> dict:
    return {"method": "POST", "path": "/x", "path_params": {}, "query": {}, "headers": {},
            "body": body, "scopes": []}


class _HttpCompleter:
    """What Rust hands `Dispatcher.submit`: complete(status, body, …) / fail(msg)."""

    def __init__(self) -> None:
        self.done = threading.Event()
        self.result: dict = {}

    def complete(self, status, body, content_type="application/json", headers=None):
        self.result = {"status": status, "body": json.loads(body) if body else None}
        self.done.set()

    def fail(self, message):
        self.result = {"failed": message}
        self.done.set()

    def abandoned(self):
        return False


class _ValueCompleter:
    """What Rust hands `Dispatcher.submit_behaviour`: complete(value) / fail(msg)."""

    def __init__(self) -> None:
        self.done = threading.Event()
        self.value = None
        self.failure: str | None = None

    def complete(self, value):
        self.value = value
        self.done.set()

    def fail(self, message):
        self.failure = message
        self.done.set()

    def abandoned(self):
        return False


def _import_source(tmp_path: Path, name: str, source: str):
    path = tmp_path / f"{name}.py"
    path.write_text(textwrap.dedent(source))
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


@pytest.fixture
def fake(monkeypatch):
    monkeypatch.setenv("WEBCORTEX_FAKE_PROVIDER", "1")
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)


# ---------------------------------------------------------------- H3: behaviours vs the sync pool


def test_behaviours_waiting_on_sync_handlers_cannot_starve_them():
    # 2.8.0 ran behaviours and sync handlers on one pool of workers*4 threads.
    # Eight behaviours each waiting on a sync handler held all four threads
    # while the handlers queued behind them: a permanent wedge.
    d = Dispatcher([], workers=1)
    d._handlers = [lambda req: {"ok": 1}]  # 0: a sync route
    d._is_async = [False]

    def behaviour(ctx, payload):
        inner = _HttpCompleter()
        d.submit(0, _request(), inner)          # what ctx.call does, through Rust
        assert inner.done.wait(10), "the nested sync handler never started"
        return inner.result

    d._handlers.append(behaviour)                # 1: the behaviour
    d._is_async.append(False)
    try:
        runs = [_ValueCompleter() for _ in range(8)]   # twice the sync pool's size
        for c in runs:
            d.submit_behaviour(1, None, {}, c)
        for c in runs:
            assert c.done.wait(15), "a behaviour never finished: the pools are shared again"
            assert c.failure is None and c.value == {"status": 200, "body": {"ok": 1}}
    finally:
        d.shutdown()


def test_behaviour_runs_past_the_limit_are_refused_not_queued(monkeypatch):
    monkeypatch.setenv("WEBCORTEX_MAX_BEHAVIOURS", "2")
    release = threading.Event()
    d = Dispatcher([lambda ctx, payload: release.wait(10) and {"ran": True}], workers=1)
    try:
        held = [_ValueCompleter(), _ValueCompleter()]
        for c in held:
            d.submit_behaviour(0, None, {}, c)
        refused = _ValueCompleter()
        d.submit_behaviour(0, None, {}, refused)
        assert refused.done.wait(1), "the third run queued instead of being refused"
        assert refused.failure.startswith("WC215:"), refused.failure
        assert "WEBCORTEX_MAX_BEHAVIOURS" in refused.failure

        release.set()
        for c in held:
            assert c.done.wait(10) and c.value == {"ran": True}
        # The slots are free again.
        deadline = time.time() + 5
        while d._behaviour_threads.admitted and time.time() < deadline:
            time.sleep(0.01)
        again = _ValueCompleter()
        d.submit_behaviour(0, None, {}, again)
        assert again.done.wait(10) and again.value == {"ran": True}
    finally:
        release.set()
        d.shutdown()


def test_a_behaviour_calling_a_behaviour_when_every_slot_is_taken_is_refused(monkeypatch):
    # A nested behaviour that queued for a slot its own caller holds would
    # wait forever; refused, the caller gets an error and finishes.
    monkeypatch.setenv("WEBCORTEX_MAX_BEHAVIOURS", "1")
    d = Dispatcher([], workers=1)

    def outer(ctx, payload):
        nested = _ValueCompleter()
        d.submit_behaviour(1, None, {}, nested)
        assert nested.done.wait(5), "the nested behaviour queued behind its caller"
        return {"nested_failure": nested.failure}

    d._handlers = [outer, lambda ctx, payload: {"inner": True}]
    d._is_async = [False, False]
    try:
        c = _ValueCompleter()
        d.submit_behaviour(0, None, {}, c)
        assert c.done.wait(10)
        assert c.value["nested_failure"].startswith("WC215:")
    finally:
        d.shutdown()


def test_every_admitted_behaviour_gets_a_thread_at_once(monkeypatch):
    # Admission below the limit must mean "running", never "queued behind a
    # busy thread", or a behaviour waiting on another could deadlock below it.
    monkeypatch.setenv("WEBCORTEX_MAX_BEHAVIOURS", "6")
    started = threading.Barrier(6, timeout=10)
    d = Dispatcher([lambda ctx, payload: started.wait() is not None], workers=1)
    try:
        runs = [_ValueCompleter() for _ in range(6)]
        for i, c in enumerate(runs):
            d.submit_behaviour(0, None, {}, c)
            if i == 2:
                time.sleep(0.05)  # let some threads finish starting, then add more
        for c in runs:
            assert c.done.wait(15) and c.failure is None, c.failure
    finally:
        d.shutdown()


def test_worker_threads_are_daemons():
    # concurrent.futures joins its threads at interpreter exit, so a handler
    # stuck forever kept the process alive after SIGTERM.
    d = Dispatcher([lambda req: {"ok": 1}, lambda ctx, payload: {"ok": 2}], workers=1)
    try:
        a, b = _HttpCompleter(), _ValueCompleter()
        d.submit(0, _request(), a)
        d.submit_behaviour(1, None, {}, b)
        assert a.done.wait(5) and b.done.wait(5)
        pool_threads = [t for t in threading.enumerate()
                        if t.name.startswith(("webcortex-sync", "webcortex-behaviour"))]
        assert pool_threads and all(t.daemon for t in pool_threads)
    finally:
        d.shutdown()


def test_a_shut_down_dispatcher_refuses_instead_of_raising_into_rust():
    d = Dispatcher([lambda req: {"ok": 1}, lambda ctx, payload: {}], workers=1)
    d.shutdown()
    a, b = _HttpCompleter(), _ValueCompleter()
    d.submit(0, _request(), a)
    d.submit_behaviour(1, None, {}, b)
    assert a.result["status"] == 503 and a.result["body"]["error"]["code"] == "WC215"
    assert b.failure.startswith("WC215:")


DEADLOCK_APP = """
import time
from webcortex import WebCortex
app = WebCortex("deadlock", port=PORT, workers=1, shutdown_timeout=3)

@app.get("/inner", tool=True, tool_name="inner")
def inner() -> dict:
    time.sleep(0.5)
    return {"ok": 1}

@app.get("/ping")
def ping() -> dict:
    return {"pong": 1}

@app.behaviour("bh", tools=["inner"], expose_at="/bh")
def bh(ctx, input):
    return ctx.call("inner")
"""


def test_behaviours_calling_sync_tools_at_and_past_the_pool_size(tmp_path):
    # workers=1: four sync threads. 2.8.0 wedged permanently at four
    # concurrent /bh requests; /ping (a sync route) then timed out forever and
    # SIGTERM could not end the process.
    with serve(tmp_path, DEADLOCK_APP, env={"WEBCORTEX_MAX_BEHAVIOURS": "4"}) as (base, proc):
        for n in (4, 12):
            pinged: list = []
            with ThreadPoolExecutor(n + 1) as pool:
                runs = [pool.submit(call, base, "POST", "/bh", {}, timeout=20) for _ in range(n)]
                time.sleep(0.1)
                t0 = time.time()
                pinged.append(call(base, "GET", "/ping", timeout=5))
                pinged.append(time.time() - t0)
                results = [r.result() for r in runs]
            assert pinged[0] == (200, {"pong": 1}), "a sync route starved behind behaviours"
            assert pinged[1] < 3
            ok = [b for s, b in results if s == 200]
            refused = [b for s, b in results if s != 200]
            assert ok and all(b == {"ok": 1} for b in ok), results
            # Past the limit: a coded 503, never a hang or a bare 500.
            for status, body in results:
                if status != 200:
                    assert status == 503 and body["error"]["code"] == "WC215", (status, body)
            if n == 4:
                assert not refused, results   # at the limit: everything runs
            else:
                assert refused, results       # past it: the excess is refused
        # And it still serves afterwards: the wedge is gone, not delayed.
        assert call(base, "POST", "/bh", {}) == (200, {"ok": 1})

        t0 = time.time()
        proc.send_signal(signal.SIGTERM)
        proc.wait(3 + 10)
        assert time.time() - t0 < 3 + 10


STUCK_APP = """
import time
from webcortex import WebCortex
app = WebCortex("stuck", port=PORT, workers=1, request_timeout=1, shutdown_timeout=2)

@app.get("/stuck")
def stuck() -> dict:
    time.sleep(600)
    return {}
"""


def test_sigterm_ends_the_process_while_a_handler_is_stuck(tmp_path):
    with serve(tmp_path, STUCK_APP) as (base, proc):
        status, body = call(base, "GET", "/stuck", timeout=10)
        assert status == 504
        t0 = time.time()
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(15)
        except subprocess.TimeoutExpired:
            pytest.fail("the process outlived SIGTERM: a pool thread is being joined at exit")
        assert time.time() - t0 < 15


# ---------------------------------------------------------------- M6: public_routes and anonymous_scopes


def _scoped_app() -> WebCortex:
    app = WebCortex("t", database="sqlite://:memory:")
    app.api_key("WEBCORTEX_API_KEY", id="service", scopes=["read", "write"])
    app.resource("items", fields={"id": int, "name": str}, tools=True,
                 read_scopes=["read"], write_scopes=["write"])
    app.agent("desk", scopes=["read"], expose_at="/ask")
    return app


def test_routes_reachable_through_anonymous_scopes_are_public():
    app = _scoped_app()
    app.anonymous_scopes("write")
    public = app.security_report()["public_routes"]
    assert "POST /items" in public
    assert "PUT /items/{id}" in public
    assert "DELETE /items/{id}" in public
    assert "GET /items" not in public      # needs read, which anonymous callers lack
    assert "POST /ask" not in public       # expose_scopes default to scopes: read
    assert "GET /" in public               # requires nothing


def test_an_agent_whose_expose_scopes_anonymous_callers_hold_is_public():
    app = _scoped_app()
    app.anonymous_scopes("read")
    public = app.security_report()["public_routes"]
    assert "POST /ask" in public and "GET /items" in public
    assert "POST /items" not in public


def test_a_wildcard_anonymous_scope_makes_every_route_public():
    app = _scoped_app()
    app.anonymous_scopes("*")
    report = app.security_report()
    assert len(report["public_routes"]) == len(app.manifest()["routes"])


def test_without_anonymous_scopes_only_unscoped_routes_are_public():
    app = _scoped_app()
    assert app.security_report()["public_routes"] == ["GET /"]
    # With no authentication at all, a scoped route answers 401 to everyone:
    # unreachable, not public.
    open_app = WebCortex("t")

    @open_app.get("/s", scopes=["read"])
    def s() -> dict:
        return {}

    assert open_app.security_report()["public_routes"] == ["GET /"]


def test_the_runtime_security_endpoint_agrees_with_security_report(tmp_path):
    source = """
from webcortex import WebCortex
app = WebCortex("anon", port=PORT, database="sqlite://:memory:")
app.api_key("WEBCORTEX_API_KEY", id="service", scopes=["read", "write", "webcortex:admin"])
app.anonymous_scopes("write")
app.resource("items", fields={"id": int, "name": str}, read_scopes=["read"], write_scopes=["write"])
"""
    namespace: dict = {}
    exec(source.replace("PORT", "0"), namespace)  # noqa: S102 - the test's own app
    expected = sorted(namespace["app"].security_report()["public_routes"])
    with serve(tmp_path, source) as (base, _):
        status, body = call(base, "GET", "/_webcortex/security", headers={"x-api-key": KEY})
        assert status == 200
        assert sorted(body["public_routes"]) == expected


# ---------------------------------------------------------------- M10: TypedDict qualifiers under future annotations


FUTURE_TYPES = """
from __future__ import annotations
from typing import Annotated, NotRequired, Required, TypedDict


class Ticket(TypedDict):
    title: str
    note: NotRequired[str]


class Partial(TypedDict, total=False):
    id: Required[int]
    label: str


class Base(TypedDict, total=False):
    tag: str
    pinned: Required[bool]


class Child(Base):
    name: str
    extra: NotRequired[Annotated[int, "meta"]]
    wrapped: Annotated[NotRequired[str], "meta"]
"""


def test_typeddict_qualifiers_survive_future_annotations(tmp_path):
    types = _import_source(tmp_path, "batch_c_future_types", FUTURE_TYPES)
    ticket = json_schema_for(types.Ticket)
    assert ticket["required"] == ["title"]
    assert ticket["properties"]["note"] == {"type": "string"}

    partial = json_schema_for(types.Partial)
    assert partial["required"] == ["id"]
    assert partial["properties"]["id"] == {"type": "integer"}

    child = json_schema_for(types.Child)
    # Inherited: tag (Base is total=False) optional, pinned Required.
    # Own: name required (Child is total); extra and wrapped NotRequired,
    # whichever side of Annotated the qualifier is on.
    assert sorted(child["required"]) == ["name", "pinned"]
    assert child["properties"]["extra"] == {"type": "integer"}
    assert child["properties"]["wrapped"] == {"type": "string"}


def test_typed_agent_output_uses_the_right_required_keys(tmp_path, fake):
    types = _import_source(tmp_path, "batch_c_future_types_2", FUTURE_TYPES)
    app = WebCortex("t")
    app.agent("ticket", output=types.Ticket)
    app.agent("partial", output=types.Partial)
    schemas = {a["name"]: a["output_schema"] for a in app.manifest()["agents"]}
    assert schemas["ticket"]["required"] == ["title"]
    assert schemas["partial"]["required"] == ["id"]

    app.eval("ticket_without_note", target="ticket", input='tool:final_answer {"title": "printer on fire"}',
             expect={"data": {"title": "printer on fire"}})
    app.eval("partial_without_id", target="partial", input='tool:final_answer {"label": "x"}',
             expect={"status": "failed", "code": "WC318"})
    report = app.run_evals(mode="live", directory=tmp_path / "evals")
    assert report["ok"], [(r["name"], r["failures"]) for r in report["results"]]


def test_coercion_strips_typeddict_qualifiers(tmp_path):
    types = _import_source(tmp_path, "batch_c_future_types_3", FUTURE_TYPES)
    assert coerce({"id": "7", "label": "x"}, types.Partial) == {"id": 7, "label": "x"}
    assert coerce({"name": "n", "extra": "3"}, types.Child) == {"name": "n", "extra": 3}


@pytest.mark.skipif(sys.version_info < (3, 13), reason="typing.ReadOnly is 3.13+")
def test_readonly_wrapped_qualifiers_are_read(tmp_path):
    types = _import_source(tmp_path, "batch_c_future_readonly", """
        from __future__ import annotations
        from typing import NotRequired, ReadOnly, TypedDict

        class Doc(TypedDict):
            id: ReadOnly[int]
            note: ReadOnly[NotRequired[str]]
    """)
    schema = json_schema_for(types.Doc)
    assert schema["required"] == ["id"]
    assert schema["properties"] == {"id": {"type": "integer"}, "note": {"type": "string"}}


# ---------------------------------------------------------------- M14: query routes describe their arguments


def test_a_query_route_derives_its_tool_schema_from_params():
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("tickets", fields={"id": int, "status": str, "priority": int})
    app.query("GET", "/tickets/by-status/{status}",
              "SELECT * FROM tickets WHERE status = ? AND priority >= ? AND owner = ?",
              params=["status", "priority", "@principal"], tool=True, tool_name="by_status")
    route = next(r for r in app.manifest()["routes"] if r["tool"]["name"] == "by_status")
    assert route["input_schema"] == {
        "type": "object",
        "properties": {"status": {"type": "string"}, "priority": {"type": "integer"}},
        "required": ["status"],
    }
    assert route["description"] == "SELECT * FROM tickets WHERE status = ? AND priority >= ? AND owner = ?"
    # OpenAPI and the MCP schema now name the arguments too.
    op = app.openapi()["paths"]["/tickets/by-status/{status}"]["get"]
    assert {(p["name"], p["in"]) for p in op["parameters"]} == {("status", "path"), ("priority", "query")}


def test_a_query_body_parameter_of_unknown_type_accepts_any_value():
    app = WebCortex("t", database="sqlite://:memory:")
    app.query("POST", "/scores/{player}", "INSERT INTO scores (player, points, note) VALUES (?, ?, ?)",
              params=["player", "points", "note", "points"], returns="affected",
              summary="Record a score")
    route = app.manifest()["routes"][0]
    # One property per distinct name; a body value of any type still binds,
    # so nothing a client sent in 2.8.0 is now refused for its type.
    assert route["input_schema"] == {
        "type": "object",
        "properties": {"player": {"type": "string"}, "points": {}, "note": {}},
        "required": ["player"],
    }
    assert "additionalProperties" not in route["input_schema"]
    assert route["description"] == "Record a score"


def test_an_explicit_query_schema_and_description_still_win():
    app = WebCortex("t", database="sqlite://:memory:")
    explicit = {"type": "object", "properties": {"q": {"type": "string", "minLength": 2}}}
    app.query("GET", "/search", "SELECT 1\nWHERE x = ?", params=["q"],
              input_schema=explicit, description="Search")
    app.query("GET", "/multi", "\n  SELECT name\n  FROM t WHERE id = ?\n", params=["id"])
    routes = {r["path"]: r for r in app.manifest()["routes"]}
    assert routes["/search"]["input_schema"] == explicit
    assert routes["/search"]["description"] == "Search"
    assert routes["/multi"]["description"] == "SELECT name"


def test_resource_routes_keep_their_own_schemas():
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("items", fields={"id": int, "name": str})
    route = next(r for r in app.manifest()["routes"] if r["method"] == "POST")
    assert route["input_schema"]["additionalProperties"] is False
    assert route["description"].startswith("Insert a new items row")


# ---------------------------------------------------------------- K1: request_options(provider, /, **options)


def test_the_docs_openrouter_example_works():
    app = WebCortex("t")
    # docs/request-options.md, verbatim
    app.provider("openrouter", base_url="https://openrouter.ai/api/v1",
                 api_key_env="OPENROUTER_API_KEY", options={"provider": {"sort": "price"}})
    assert app.manifest()["models"]["options"]["openrouter"] == {"provider": {"sort": "price"}}
    app.request_options("openrouter", provider={"order": ["a", "b"]}, seed=1)
    assert app.manifest()["models"]["options"]["openrouter"] == {"provider": {"order": ["a", "b"]}, "seed": 1}
    app.request_options("ollama", keep_alive="30m")
    assert app.manifest()["models"]["options"]["ollama"] == {"keep_alive": "30m"}


# ---------------------------------------------------------------- K5: typed output refuses undeclared keys


@dataclass
class Address:
    city: str


@dataclass
class Person:
    name: str
    address: Address
    tags: dict[str, int] = field(default_factory=dict)


class Verdict(TypedDict):
    ok: bool


def test_typed_output_objects_are_closed_at_every_level():
    schema = output_schema_for(Person)
    assert schema["additionalProperties"] is False
    assert schema["properties"]["address"]["additionalProperties"] is False
    # A dict field is a map, not a record: it stays open.
    assert schema["properties"]["tags"] == {"type": "object", "additionalProperties": {"type": "integer"}}
    assert output_schema_for(Verdict)["additionalProperties"] is False


def test_route_schemas_are_unchanged():
    # Advertisement (OpenAPI, the TypeScript client, tool lists) keeps its
    # 2.8.0 shape; only a typed answer's schema is closed.
    assert "additionalProperties" not in json_schema_for(Person)
    assert "additionalProperties" not in json_schema_for(Person)["properties"]["address"]
    app = WebCortex("t")

    @app.post("/people")
    def create(person: Person) -> Person:
        return person

    route = app.manifest()["routes"][0]
    assert route["input_schema"]["additionalProperties"] is False   # as before: the top level
    assert "additionalProperties" not in route["input_schema"]["properties"]["person"]
    assert "additionalProperties" not in route["output_schema"]


def test_an_open_typeddict_stays_open():
    te = pytest.importorskip("typing_extensions")
    try:
        Open = te.TypedDict("Open", {"a": int}, closed=False)
        Extra = te.TypedDict("Extra", {"a": int}, extra_items=str)
    except TypeError:
        pytest.skip("this typing_extensions has no PEP 728")
    assert output_schema_for(Open)["additionalProperties"] is True
    assert output_schema_for(Extra)["additionalProperties"] == {"type": "string"}


def test_a_typed_answer_with_an_extra_key_is_refused(tmp_path, fake):
    app = WebCortex("t")
    app.agent("who", output=Person)
    app.eval("extra_key", target="who",
             input='tool:final_answer {"name": "a", "address": {"city": "b"}, "notes": "x"}',
             expect={"status": "failed", "code": "WC318"})
    app.eval("nested_extra_key", target="who",
             input='tool:final_answer {"name": "a", "address": {"city": "b", "zip": "1"}}',
             expect={"status": "failed", "code": "WC318"})
    app.eval("exact", target="who",
             input='tool:final_answer {"name": "a", "address": {"city": "b"}, "tags": {"x": 1}}',
             expect={"data": {"name": "a", "address": {"city": "b"}, "tags": {"x": 1}}})
    report = app.run_evals(mode="live", directory=tmp_path / "evals")
    assert report["ok"], [(r["name"], r["failures"]) for r in report["results"]]


# ---------------------------------------------------------------- K6: recursive types via $defs/$ref


@dataclass
class Node:
    value: int
    children: list[Node] = field(default_factory=list)


@dataclass
class Tree:
    name: str
    root: Node
    parent: Optional[Tree] = None


@dataclass
class Plain:
    a: int


def test_a_recursive_output_type_is_described_once_under_defs():
    schema = output_schema_for(Node)
    assert schema["type"] == "object" and schema["title"] == "Node"
    assert schema["properties"]["children"] == {"type": "array", "items": {"$ref": "#/$defs/Node"}}
    node = schema["$defs"]["Node"]
    assert node["properties"]["value"] == {"type": "integer"}
    assert node["properties"]["children"]["items"] == {"$ref": "#/$defs/Node"}
    assert node["additionalProperties"] is False


def test_recursion_below_the_root_and_through_optional():
    schema = output_schema_for(Tree)
    assert set(schema["$defs"]) == {"Node", "Tree"}
    # An Optional recursion point: anyOf, so whatever resolves the $ref need
    # not also honour a sibling keyword.
    assert schema["properties"]["parent"] == {"anyOf": [{"$ref": "#/$defs/Tree"}, {"type": "null"}]}
    assert schema["properties"]["root"]["properties"]["children"]["items"] == {"$ref": "#/$defs/Node"}


def test_a_type_that_does_not_recur_has_no_defs():
    assert "$defs" not in output_schema_for(Plain)
    assert "$defs" not in output_schema_for(Person)
    # Route schemas keep the loose 2.8.0 recursion point.
    assert json_schema_for(Node)["properties"]["children"]["items"] == {"type": "object", "title": "Node"}


class TNode(TypedDict):
    value: int
    children: list[TNode]


def test_a_recursive_typeddict_uses_defs_too():
    schema = output_schema_for(TNode)
    assert schema["properties"]["children"]["items"] == {"$ref": "#/$defs/TNode"}
    assert schema["$defs"]["TNode"]["additionalProperties"] is False


class TA(TypedDict):
    b: TB


class TB(TypedDict):
    a: TA
    kids: list[TB]


def test_a_class_whose_schema_failed_leaves_no_dangling_ref(monkeypatch):
    # TB finishes (and is kept under $defs) holding a $ref to TA; TA then
    # fails part-way and degrades to {}, so it never gets an entry.
    from webcortex import schema as schema_module

    real = schema_module._typeddict_extra_items

    def broken(cls):
        if cls is TA:
            raise RuntimeError("an exotic annotation")
        return real(cls)

    monkeypatch.setattr(schema_module, "_typeddict_extra_items", broken)
    out = output_schema_for(dict[str, TA])
    assert set(out["$defs"]) == {"TB"}
    assert out["$defs"]["TB"]["properties"]["a"] == {"type": "object", "title": "TA"}
    assert "#/$defs/TA" not in json.dumps(out)


def test_the_agent_route_envelope_carries_no_dangling_refs():
    app = WebCortex("t")
    app.agent("tree", output=Node)
    agent = app.manifest()["agents"][0]
    assert agent["output_schema"]["$defs"]["Node"]
    route = next(r for r in app.manifest()["routes"] if r["op"].get("agent") == "tree")
    data = route["output_schema"]["properties"]["data"]
    text = json.dumps(data)
    assert "$ref" not in text and "$defs" not in text
    assert data["properties"]["children"]["items"] == {"type": "object", "title": "Node"}
    # And the OpenAPI document built from it resolves.
    assert "#/$defs" not in json.dumps(app.openapi())


def test_a_correct_deep_recursive_answer_is_accepted(tmp_path, fake):
    app = WebCortex("t")
    app.agent("tree", output=Node)
    answer = {"value": 1, "children": [{"value": 2, "children": [{"value": 3, "children": []}]}]}
    app.eval("deep", target="tree", input=f"tool:final_answer {json.dumps(answer)}",
             expect={"data": answer})
    report = app.run_evals(mode="live", directory=tmp_path / "evals")
    assert report["ok"], [(r["name"], r["failures"]) for r in report["results"]]


def test_a_wrong_type_three_levels_down_is_refused(tmp_path, fake):
    app = WebCortex("t")
    app.agent("tree", output=Node)
    bad = {"value": 1, "children": [{"value": 2, "children": [{"value": "three", "children": []}]}]}
    app.eval("deep_bad", target="tree", input=f"tool:final_answer {json.dumps(bad)}",
             expect={"status": "failed", "code": "WC318"})
    report = app.run_evals(mode="live", directory=tmp_path / "evals")
    assert report["ok"], [(r["name"], r["failures"]) for r in report["results"]]


# ---------------------------------------------------------------- L4: jwt() algorithms


@pytest.mark.parametrize("alg", ["HS256", "HS384", "HS512", "RS256", "RS384", "RS512",
                                 "ES256", "ES384", "EdDSA"])
def test_jwt_accepts_the_supported_algorithms(alg):
    app = WebCortex("t")
    app.jwt(secret_env="JWT_SECRET", algorithm=alg)
    assert app.manifest()["auth"]["jwt"]["algorithm"] == alg


@pytest.mark.parametrize("alg", ["none", "PS256", "ES512", "HS1", "", None, 256])
def test_jwt_refuses_anything_else_at_declaration(alg):
    with pytest.raises(ValueError, match="not supported"):
        WebCortex("t").jwt(secret_env="JWT_SECRET", algorithm=alg)


def test_jwt_suggests_the_right_spelling():
    with pytest.raises(ValueError, match="did you mean 'HS256'"):
        WebCortex("t").jwt(secret_env="JWT_SECRET", algorithm="hs256")
    with pytest.raises(ValueError, match="did you mean 'EdDSA'"):
        WebCortex("t").jwt(secret_env="JWT_SECRET", algorithm="EDDSA")


# ---------------------------------------------------------------- L6: resource list order and page bounds


def test_the_list_route_is_newest_first_and_clamped_in_sql():
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("events", fields={"id": int, "msg": str})
    route = next(r for r in app.manifest()["routes"] if r["tool"]["name"] == "list_events")
    sql = route["op"]["sql"]
    assert "ORDER BY id DESC" in sql
    assert "MIN(MAX(COALESCE(CAST(? AS INTEGER), 50), 1), 1000)" in sql
    assert "OFFSET MAX(COALESCE(CAST(? AS INTEGER), 0), 0)" in sql
    props = route["input_schema"]["properties"]
    assert props["limit"] == {"type": "integer", "default": 50, "minimum": 1, "maximum": 1000}
    assert props["offset"] == {"type": "integer", "default": 0, "minimum": 0}
    assert "newest first" in route["description"]


def test_a_text_keyed_resource_orders_by_insertion_or_says_what_it_does():
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("pages", fields={"slug": str, "title": str}, primary_key="slug")
    app.resource("legacy", fields={"slug": str}, primary_key="slug", create_table=False)
    routes = {r["tool"]["name"]: r for r in app.manifest()["routes"]}
    assert "ORDER BY rowid DESC" in routes["list_pages"]["op"]["sql"]
    assert "newest first" in routes["list_pages"]["description"]
    # A table the resource did not create may have no rowid.
    assert "ORDER BY slug DESC" in routes["list_legacy"]["op"]["sql"]
    assert "newest first" not in routes["list_legacy"]["description"]


def test_the_list_route_bounds_hold_on_a_real_table(tmp_path):
    db = tmp_path / "events.db"
    with sqlite3.connect(db) as conn:
        conn.execute("CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, msg TEXT)")
        conn.executemany("INSERT INTO events (msg) VALUES (?)", [(f"e{i}",) for i in range(1, 1201)])
    source = f"""
from webcortex import WebCortex
app = WebCortex("l", port=PORT, database="sqlite://{db}")
app.resource("events", fields={{"id": int, "msg": str}})
"""
    with serve(tmp_path, source) as (base, _):
        status, rows = call(base, "GET", "/events")
        assert status == 200 and len(rows) == 50
        assert [r["id"] for r in rows[:3]] == [1200, 1199, 1198]
        assert len(call(base, "GET", "/events?limit=-1")[1]) == 1
        assert len(call(base, "GET", "/events?limit=5000")[1]) == 1000
        assert len(call(base, "GET", "/events?limit=abc")[1]) == 1
        assert [r["id"] for r in call(base, "GET", "/events?limit=2&offset=-5")[1]] == [1200, 1199]
        assert [r["id"] for r in call(base, "GET", "/events?limit=2&offset=3")[1]] == [1197, 1196]


# ---------------------------------------------------------------- L7: deep JSON is a 400


def test_deeply_nested_json_is_invalid_json_not_a_crash():
    req = Request(_request(b"[" * 100_000 + b"]" * 100_000))
    with pytest.raises(InvalidJSON) as caught:
        req.json()
    assert caught.value.status == 400 and caught.value.code == "WC205"
    assert isinstance(caught.value, ValueError)


def test_deep_json_to_a_python_handler_answers_400(tmp_path):
    source = """
from webcortex import WebCortex
app = WebCortex("deep", port=PORT)

@app.post("/notes")
def create(body: str) -> dict:
    return {"body": body}
"""
    with serve(tmp_path, source) as (base, _):
        payload = b"[" * 20_000 + b"]" * 20_000
        req = urllib.request.Request(base + "/notes", data=payload, method="POST",
                                     headers={"content-type": "application/json"})
        with pytest.raises(urllib.error.HTTPError) as caught:
            urllib.request.urlopen(req, timeout=20)
        assert caught.value.code == 400
        assert json.loads(caught.value.read())["error"]["code"] == "WC205"
        assert "RecursionError" not in (tmp_path / "server.log").read_text()


# ---------------------------------------------------------------- L16: typed agents and handoff targets


def _typed(app: WebCortex, name: str, **kw) -> None:
    app.agent(name, output={"type": "object", "properties": {"n": {"type": "integer"}}}, **kw)


def test_a_handoff_target_of_a_typed_agent_may_not_list_final_answer():
    app = WebCortex("t")

    @app.post("/file", tool=True, tool_name="final_answer")
    def file_it(text: str = "") -> dict:
        return {}

    _typed(app, "desk", handoffs=["specialist"])        # declared before its target
    app.agent("specialist", tools=["final_answer"])
    with pytest.raises(ValueError, match=r"'specialist'.*\(desk -> specialist\)"):
        app.manifest()


def test_the_check_follows_handoffs_transitively():
    app = WebCortex("t")

    @app.post("/file", tool=True, tool_name="final_answer")
    def file_it(text: str = "") -> dict:
        return {}

    app.agent("specialist", tools=["final_answer"])
    app.agent("middle", handoffs=["specialist"])
    _typed(app, "desk", handoffs=["middle"])
    with pytest.raises(ValueError, match=r"desk -> middle -> specialist"):
        app.check()


def test_untyped_agents_may_still_hand_off_to_one_with_a_final_answer_route():
    app = WebCortex("t")

    @app.post("/file", tool=True, tool_name="final_answer")
    def file_it(text: str = "") -> dict:
        return {}

    app.agent("specialist", tools=["final_answer"])
    app.agent("desk", handoffs=["specialist"])
    _typed(app, "typed")  # typed, but it hands off to nobody
    assert {"desk", "specialist", "typed"} <= set(app.check()["agents"])


def test_handoff_cycles_terminate():
    app = WebCortex("t")
    _typed(app, "a", handoffs=["b"])
    app.agent("b", handoffs=["a", "c"])
    app.agent("c", handoffs=["b"])
    app.manifest()


# ---------------------------------------------------------------- L32: api_key_header


def test_api_key_header_is_settable_and_lower_cased():
    assert WebCortex("t").manifest()["auth"]["api_key_header"] == "x-api-key"
    app = WebCortex("t", api_key_header="X-Service-Key")
    assert app.manifest()["auth"]["api_key_header"] == "x-service-key"


@pytest.mark.parametrize("bad", ["authorization", "Cookie", "set-cookie", "host", "content-type",
                                 "x-forwarded-for", "sec-fetch-site", "proxy-token"])
def test_api_key_header_refuses_headers_with_another_meaning(bad):
    with pytest.raises(ValueError, match="already use"):
        WebCortex("t", api_key_header=bad)


@pytest.mark.parametrize("bad", ["", "x api key", "x-key:", "x-kéy", "x-api-key\n", None, 3])
def test_api_key_header_must_be_a_header_name(bad):
    with pytest.raises(ValueError, match="not a valid HTTP header name"):
        WebCortex("t", api_key_header=bad)


def test_the_runtime_reads_the_key_from_the_configured_header(tmp_path):
    source = """
from webcortex import WebCortex
app = WebCortex("hdr", port=PORT, api_key_header="X-Service-Key")
app.api_key("WEBCORTEX_API_KEY", id="service", scopes=["read"])

@app.get("/p", scopes=["read"])
def p() -> dict:
    return {"ok": True}
"""
    with serve(tmp_path, source) as (base, _):
        assert call(base, "GET", "/p", headers={"X-Service-Key": KEY}) == (200, {"ok": True})
        assert call(base, "GET", "/p", headers={"x-api-key": KEY})[0] == 401
