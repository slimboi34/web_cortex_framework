"""Tests that kill mutants of the lines WebCortex 2.9.0 changed in python/webcortex/.

From the machine-driven testing of 2.9.0 (see the 2.9.1 CHANGELOG): 511
mutants of those lines, 103 of which survived the release's own tests.

Each test names the mutant(s) it kills (ids from mutation/mutants.json, made by
mutation/engine.py from `git diff v2.8.0 v2.9.0 -- python/webcortex/`). Every
one of these mutants survived the release's own Python-layer regression tests
(tests/test_2_9_python_app.py, test_2_9_tooling.py, test_2_9_docs.py).

All tests are in-process, deterministic and fast: no server, no network, no
provider. Run with `pytest -q tests/test_2_9_1_mutation_killers.py`.
"""

from __future__ import annotations

import json
import threading
import time

import pytest

from webcortex import _bridge
from webcortex._bridge import MAX_JSON_DEPTH, InvalidJSON, Request


def _req(body) -> Request:
    return Request({"method": "POST", "path": "/x", "path_params": {}, "query": {},
                    "headers": {}, "body": body, "scopes": []})


def _nest_list(depth: int) -> str:
    return "[" * depth + "1" + "]" * depth


def _nest_obj(depth: int) -> str:
    return '{"a":' * depth + "1" + "}" * depth


# ---------------------------------------------------------------- _bridge: JSON depth


@pytest.mark.parametrize("make", [_nest_list, _nest_obj], ids=["list", "object"])
@pytest.mark.parametrize("as_bytes", [True, False], ids=["bytes", "str"])
def test_exactly_max_depth_is_accepted_and_one_more_refused(make, as_bytes):
    """Kills M048 (`depth > limit` -> `>=`), M070 (walk starts at depth 2) and
    M098 (`depth + 1` -> `+ 2`): the documented limit is 512 levels *allowed*,
    513 refused, whichever path (bracket count or walk) decides."""
    ok = make(MAX_JSON_DEPTH)
    bad = make(MAX_JSON_DEPTH + 1)
    # The same 512 levels plus one bracket inside a string: the bracket count
    # (513) no longer clears the body, so the depth walk itself must allow it.
    ok_walked = ok.replace("1", '"["', 1)
    if as_bytes:
        ok, bad, ok_walked = ok.encode(), bad.encode(), ok_walked.encode()
    assert _req(ok).json() is not None
    assert _req(ok_walked).json() is not None
    with pytest.raises(InvalidJSON) as e:
        _req(bad).json()
    assert e.value.status == 400 and e.value.code == "WC205"


@pytest.mark.parametrize("as_bytes", [True, False], ids=["bytes", "str"])
def test_deep_objects_are_counted_not_only_lists(as_bytes):
    """Kills M045 / M046 (`count('[') + count('{')` -> `-`): a body nested
    through objects alone must hit the limit too."""
    body = _nest_obj(600)
    with pytest.raises(InvalidJSON):
        _req(body.encode() if as_bytes else body).json()
    # Mixed: 300 objects inside 300 lists.
    mixed = "[" * 300 + '{"a":' * 300 + "1" + "}" * 300 + "]" * 300
    with pytest.raises(InvalidJSON):
        _req(mixed.encode() if as_bytes else mixed).json()


def test_brackets_inside_strings_never_refuse_a_shallow_body():
    """Guards the bracket-count shortcut: brackets in string data over the
    limit, real nesting under it."""
    body = json.dumps({"s": "[" * 5000 + "{" * 5000})
    assert _req(body.encode()).json()["s"].startswith("[[[")


# ---------------------------------------------------------------- _bridge: behaviour settlement


class _Completer:
    def __init__(self):
        self.completed = []
        self.failed = []

    def complete(self, value):
        self.completed.append(value)

    def fail(self, message):
        self.failed.append(message)


class BehaviourHalted(Exception):
    """Same name as the runtime's exception, which is matched by name."""


def test_a_halted_behaviour_is_a_structured_result_not_a_failure():
    """Kills M009 / M022 (negated check) and M051 (name changed)."""
    c = _Completer()

    def run():
        raise BehaviourHalted("budget exhausted")

    _bridge._behaviour_task(run, c)
    assert c.failed == []
    assert c.completed == [{"halted": True, "reason": "budget exhausted"}]


def test_any_other_behaviour_exception_is_a_failure():
    """The other side of M009/M022: a plain exception is not a halt."""
    c = _Completer()

    def run():
        raise KeyError("boom")

    _bridge._behaviour_task(run, c)
    assert c.completed == []
    assert len(c.failed) == 1


def test_result_too_large_from_a_behaviour_keeps_its_code():
    """Kills M010 (negated `isinstance(exc, ResultTooLarge)`)."""
    c = _Completer()

    def run():
        raise _bridge.ResultTooLarge("too many items")

    _bridge._behaviour_task(run, c)
    assert c.failed == ["WC406: too many items"]


def test_a_behaviour_traceback_is_not_sent_unless_debug_errors(monkeypatch):
    """Kills M052 (negated `_EXPOSE_TRACEBACKS`): without
    WEBCORTEX_DEBUG_ERRORS the caller gets "behaviour failed", never the
    traceback; with it, the traceback."""
    monkeypatch.setattr(_bridge, "_EXPOSE_TRACEBACKS", False)
    c = _Completer()

    def run():
        raise RuntimeError("secret-internal-detail")

    _bridge._behaviour_task(run, c)
    assert c.failed == ["behaviour failed"]

    monkeypatch.setattr(_bridge, "_EXPOSE_TRACEBACKS", True)
    c2 = _Completer()
    _bridge._behaviour_task(run, c2)
    assert len(c2.failed) == 1 and "secret-internal-detail" in c2.failed[0]


def test_a_behaviour_result_is_delivered():
    c = _Completer()
    _bridge._behaviour_task(lambda: {"ok": 1}, c)
    assert c.completed == [{"ok": 1}] and c.failed == []


# ---------------------------------------------------------------- _bridge: _Pool


def _wait(pred, secs=5.0):
    deadline = time.monotonic() + secs
    while time.monotonic() < deadline:
        if pred():
            return True
        time.sleep(0.005)
    return pred()


def test_pool_size_and_admitted_are_reported():
    """Kills M011 (`size` returns None), M012 (`admitted` returns None) and
    M053/M054 (`max(1, size)` -> `max(2, …)` / `max(0, …)`)."""
    p = _bridge._Pool(1, "t")
    assert p.size == 1
    assert _bridge._Pool(0, "t").size == 1
    assert _bridge._Pool(3, "t").size == 3
    gate = threading.Event()
    assert p.try_submit(gate.wait)
    assert p.admitted == 1
    gate.set()
    assert _wait(lambda: p.admitted == 0)
    p.shutdown()


def test_a_pool_of_size_zero_still_runs_work():
    """Kills M054: size is at least one thread."""
    p = _bridge._Pool(0, "t")
    done = threading.Event()
    assert p.try_submit(done.set)
    assert done.wait(5)
    p.shutdown()


def test_a_closed_pool_refuses_work_and_its_threads_end():
    """Kills M034 (the `None` sentinel no longer ends a worker: it is called,
    fails, and the thread lives on); a closed pool refuses work."""
    p = _bridge._Pool(2, "t")
    done = threading.Event()
    assert p.try_submit(done.set)
    assert done.wait(5)
    threads = list(p._threads)
    p.shutdown()
    assert p.try_submit(lambda: None) is False
    for t in threads:
        t.join(5)
        assert not t.is_alive()
    assert p.admitted == 0


def test_an_idle_thread_is_reused_rather_than_a_new_one_started():
    """Kills M078 (`_idle > 0` -> `_idle > 1`), M079 (`_idle -= 2`) and M091
    (`_idle += 0`)."""
    p = _bridge._Pool(4, "t")
    for _ in range(3):
        done = threading.Event()
        assert p.try_submit(done.set)
        assert done.wait(5)
        assert _wait(lambda: p.admitted == 0)
    assert len(p._threads) == 1
    p.shutdown()


def test_a_limited_pool_refuses_past_its_limit_and_admits_again_after():
    p = _bridge._Pool(2, "t", limit=2)
    gate = threading.Event()
    assert p.try_submit(gate.wait) and p.try_submit(gate.wait)
    assert p.try_submit(lambda: None) is False
    gate.set()
    assert _wait(lambda: p.admitted == 0)
    assert p.try_submit(lambda: None) is True
    p.shutdown()


# ---------------------------------------------------------------- app: typed agent envelope


from dataclasses import dataclass  # noqa: E402

from webcortex import WebCortex  # noqa: E402
from webcortex.schema import output_schema_for  # noqa: E402


@dataclass
class _Node:
    name: str
    parent: "_Node | None" = None
    kids: "list[_Node] | None" = None


def _route(app, prefix):
    return next(r for r in app.manifest()["routes"] if r["path"].startswith(prefix))


def _walk(schema):
    if isinstance(schema, dict):
        yield schema
        for v in schema.values():
            yield from _walk(v)
    elif isinstance(schema, list):
        for v in schema:
            yield from _walk(v)


def test_a_typed_agents_envelope_is_the_answer_schema_with_refs_inlined():
    """Kills M104 / M117 (lists such as `anyOf` no longer walked: a `$ref`
    leaks into the envelope) and M119 (scalars such as `"type": "object"`
    replaced by None)."""
    app = WebCortex("envelope")
    app.agent("a", output=_Node)
    data = _route(app, "/agents/a")["output_schema"]["properties"]["data"]
    assert not any("$ref" in s or "$defs" in s for s in _walk(data)), data
    assert data["type"] == "object" and data["title"] == "_Node"
    assert data["properties"]["name"] == {"type": "string"}
    assert data["properties"]["parent"] == {"anyOf": [{"type": "object", "title": "_Node"},
                                                      {"type": "null"}]}
    assert data["properties"]["kids"]["items"] == {"type": "object", "title": "_Node"}
    assert data["required"] == ["name"] and data["additionalProperties"] is False
    # The validated schema keeps its refs.
    validated = app.manifest()["agents"][0]["output_schema"]
    assert validated == output_schema_for(_Node)
    assert validated["properties"]["parent"]["anyOf"][0] == {"$ref": "#/$defs/_Node"}


# ---------------------------------------------------------------- app: query routes' derived schema


def test_a_query_takes_field_types_only_from_the_tables_its_sql_names():
    """Kills M152 (the table-name filter deleted): a resource whose table the
    SQL does not mention must not type the query's arguments."""
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("tickets", fields={"id": int, "priority": int})
    app.resource("tickets_archive", fields={"id": int, "priority": float, "owner": int})
    app.query("GET", "/open", "SELECT * FROM tickets WHERE priority >= ? AND owner = ?",
              params=["priority", "owner"], tool=True, tool_name="open_tickets")
    route = next(r for r in app.manifest()["routes"] if r["tool"]["name"] == "open_tickets")
    # `tickets_archive` is not named (`tickets` is a prefix of it, not it).
    assert route["input_schema"]["properties"] == {"priority": {"type": "integer"},
                                                   "owner": {"type": "string"}}


# ---------------------------------------------------------------- app: api_key_header


RESERVED_HEADERS = [  # as listed in the 2.9.0 change and app.py's comment
    "authorization", "proxy-authorization", "cookie", "set-cookie", "host",
    "content-type", "content-length", "content-encoding", "transfer-encoding",
    "connection", "keep-alive", "upgrade", "te", "trailer", "expect",
    "origin", "referer", "user-agent", "accept", "accept-encoding",
    "if-none-match", "traceparent", "tracestate", "forwarded",
    "x-forwarded-for", "x-forwarded-host", "x-forwarded-proto", "x-real-ip",
    "sec-fetch-site", "sec-websocket-key", "proxy-connection",
]


@pytest.mark.parametrize("header", RESERVED_HEADERS + [h.upper() for h in RESERVED_HEADERS[:5]])
def test_every_reserved_header_is_refused_as_the_api_key_header(header):
    """Kills M164-M167 and the other entries of `_RESERVED_KEY_HEADERS`
    (each entry renamed): every header with a meaning of its own is refused,
    case-insensitively."""
    with pytest.raises(ValueError, match="already"):
        WebCortex("t", api_key_header=header)


@pytest.mark.parametrize("header", ["x-service-key", "X-Api-Key", "apikey", "x-sec-key", "x_key"])
def test_an_ordinary_header_name_is_accepted_lower_cased(header):
    app = WebCortex("t", api_key_header=header)
    assert app.manifest()["auth"]["api_key_header"] == header.lower()


@pytest.mark.parametrize("header", ["", "x key", "x-key\r\n", "x:key", "ключ", None, 5])
def test_an_invalid_header_name_is_refused(header):
    with pytest.raises(ValueError, match="valid HTTP header name"):
        WebCortex("t", api_key_header=header)


# ---------------------------------------------------------------- _bridge: pool accounting


def test_every_admitted_task_starts_at_once_on_a_warm_pool():
    """Kills M080 (`_idle -= 1` -> `-= 0`) and M090 (`_idle += 1` -> `+= 2`):
    after the pool has run work, idle threads are over-counted and a second
    concurrent task queues behind the first instead of getting a thread, which
    is the wedge 2.9.0 fixed. The release's test only used a fresh pool."""
    p = _bridge._Pool(3, "t", limit=3)
    warm = threading.Event()
    assert p.try_submit(warm.set) and warm.wait(5)
    assert _wait(lambda: p.admitted == 0)
    started = threading.Barrier(3, timeout=5)
    release = threading.Event()
    results = []

    def task():
        try:
            started.wait()
            results.append("ran")
        except threading.BrokenBarrierError:
            results.append("starved")
        release.wait(5)

    for _ in range(3):
        assert p.try_submit(task)
    assert _wait(lambda: len(results) == 3, 10)
    release.set()
    assert results == ["ran"] * 3
    p.shutdown()


def test_a_pool_never_starts_more_threads_than_its_size():
    """Kills M081 (`len(threads) < size` -> `<=`)."""
    p = _bridge._Pool(1, "t")
    gate = threading.Event()
    assert p.try_submit(gate.wait) and p.try_submit(gate.wait) and p.try_submit(gate.wait)
    assert len(p._threads) == 1
    gate.set()
    assert _wait(lambda: p.admitted == 0)
    p.shutdown()


def test_admitted_never_goes_negative_so_the_limit_holds():
    """Kills M088 (`_admitted -= 1` -> `-= 2`)."""
    p = _bridge._Pool(2, "t", limit=2)
    for _ in range(4):
        done = threading.Event()
        assert p.try_submit(done.set) and done.wait(5)
        assert _wait(lambda: p.admitted == 0)
    gate = threading.Event()
    assert p.try_submit(gate.wait) and p.try_submit(gate.wait)
    assert p.try_submit(lambda: None) is False
    gate.set()
    assert _wait(lambda: p.admitted == 0)
    assert len(p._threads) <= 2
    p.shutdown()


def test_sync_pool_has_four_threads_per_worker_and_behaviours_their_own(monkeypatch):
    """Kills M084 / M085 (`count * 4` -> 5 / 3). The behaviour pool is
    max(32, workers*8) unless WEBCORTEX_MAX_BEHAVIOURS says otherwise."""
    monkeypatch.delenv("WEBCORTEX_MAX_BEHAVIOURS", raising=False)
    d = _bridge.Dispatcher([lambda req: None], workers=3)
    try:
        assert d._threads.size == 12
        assert d._behaviour_threads.size == 32
    finally:
        d.shutdown()
    d = _bridge.Dispatcher([lambda req: None], workers=5)
    try:
        assert d._behaviour_threads.size == 40
    finally:
        d.shutdown()
    monkeypatch.setenv("WEBCORTEX_MAX_BEHAVIOURS", "3")
    assert _bridge.behaviour_limit(5) == 3
    monkeypatch.setenv("WEBCORTEX_MAX_BEHAVIOURS", "0")
    assert _bridge.behaviour_limit(5) == 40


def test_a_full_behaviour_pool_refuses_with_wc215_naming_its_size(monkeypatch):
    """Kills M011 through the public path: the refusal names the pool size."""
    monkeypatch.setenv("WEBCORTEX_MAX_BEHAVIOURS", "1")
    gate = threading.Event()
    d = _bridge.Dispatcher([lambda ctx, payload=None: gate.wait(5)], workers=1)
    try:
        first, second = _Completer(), _Completer()
        d.submit_behaviour(0, object(), {}, first)
        d.submit_behaviour(0, object(), {}, second)
        assert len(second.failed) == 1
        assert second.failed[0].startswith("WC215: 1 behaviour runs are already in progress")
    finally:
        gate.set()
        d.shutdown()


# ---------------------------------------------------------------- app: envelope data keywords


def test_enum_and_const_values_are_data_not_schemas():
    """Kills M184 (`const` no longer treated as data): an object-valued const
    with `$defs`/`$ref` keys must reach the envelope unchanged."""
    const = {"$defs": {"x": 1}, "$ref": "#/$defs/x", "k": [1, 2]}
    out = {"type": "object", "properties": {
        "c": {"const": const},
        "e": {"enum": [{"$ref": "#/$defs/y"}, None]},
        "d": {"type": "object", "default": {"$defs": 2}},
    }, "required": ["c"]}
    app = WebCortex("t")
    app.agent("a", output=out)
    data = _route(app, "/agents/a")["output_schema"]["properties"]["data"]
    assert data["properties"]["c"]["const"] == const
    assert data["properties"]["e"]["enum"] == [{"$ref": "#/$defs/y"}, None]
    assert data["properties"]["d"]["default"] == {"$defs": 2}


def test_property_names_that_look_like_keywords_are_kept():
    """Kills M185 (the `properties` map walked as if it were a schema): a
    property literally named `$defs` or `enum` is a property."""
    out = {"type": "object", "properties": {
        "$defs": {"type": "string"},
        "enum": {"$ref": "#/$defs/T"},
    }, "$defs": {"T": {"type": "integer"}}}
    app = WebCortex("t")
    app.agent("a", output=out)
    data = _route(app, "/agents/a")["output_schema"]["properties"]["data"]
    assert data["properties"]["$defs"] == {"type": "string"}
    assert data["properties"]["enum"] == {"type": "object", "title": "T"}
    assert "$defs" not in data


def test_a_query_over_two_tables_types_a_shared_name_only_when_they_agree():
    """Kills M205 (the clash test inverted): a name both tables declare with
    the same type keeps that type; with different types it is a string."""
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("orders", fields={"id": int, "total": float, "note": str})
    app.resource("refunds", fields={"id": int, "total": int, "note": str})
    app.query("GET", "/joined",
              "SELECT * FROM orders JOIN refunds ON orders.id = refunds.id "
              "WHERE orders.id = ? AND refunds.total > ? AND orders.note = ?",
              params=["id", "total", "note"], tool=True, tool_name="joined")
    route = next(r for r in app.manifest()["routes"] if r["tool"]["name"] == "joined")
    assert route["input_schema"]["properties"] == {
        "id": {"type": "integer"},     # both say int
        "total": {"type": "string"},   # float vs int: a clash
        "note": {"type": "string"},
    }


# ---------------------------------------------------------------- cli: redaction


def test_redaction_passes_absent_urls_through():
    """Kills M219 (the `isinstance(url, str)` guard deleted: `re.sub` on None
    raises, so `webcortex context --json` crashed on a camera with no URL)."""
    from webcortex.cli import _redact_url, redacted_manifest

    assert _redact_url(None) is None
    m = redacted_manifest({"database": {"url": None},
                           "devices": [{"name": "cam", "source": {"headers": {"k": "v"}}}],
                           "watchers": [{"webhook": {}}],
                           "upstreams": {"u": {"base_url": "https://a:b@h/x", "headers": {"x": "s"}}}})
    assert m["database"]["url"] is None
    assert m["devices"][0]["source"] == {"headers": {"k": "<redacted>"}, "url": None}
    assert m["upstreams"]["u"] == {"base_url": "https://<redacted>@h/x", "headers": {"x": "<redacted>"}}


# ---------------------------------------------------------------- cli: boot failures and the control plane


from webcortex import cli as _cli  # noqa: E402


def test_a_bind_failure_hint_is_given_only_when_the_port_is_not_merely_in_use():
    """Kills M234 (`and` -> `or`), M251 (`==` -> `!=`) and M257 (code renamed)."""
    denied = _cli._boot_failure(None, "bind 127.0.0.1:80: Permission denied (os error 13)")
    assert denied.code == "WC104" and denied.hint and "--host/--port" in denied.hint
    in_use = _cli._boot_failure(None, "bind 127.0.0.1:11150: Address already in use (os error 48)")
    assert in_use.code == "WC104" and in_use.hint is None
    other = _cli._boot_failure(None, "invalid application: something")
    assert other.code == "WC101" and other.hint is None


def test_a_template_location_needs_a_templates_dir():
    """Kills M222 (the `if not directory` guard deleted: `Path(None)` raises)."""
    msg = 'template "p.html" failed to load: syntax error (in p.html:3)'
    assert _cli._template_location(object(), msg) == msg
    import types as _types
    app = _types.SimpleNamespace(templates_dir="tpl")
    assert _cli._template_location(app, msg).endswith("(at tpl/p.html:3)")


class _FakeResponse:
    def __init__(self, body: bytes):
        self._body = body

    def read(self):
        return self._body

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


def _capture_urlopen(monkeypatch, *, raise_exc=None, body=b'{"was_halted": true}'):
    import urllib.request as _ur
    seen = []

    def fake(req, timeout=None):
        seen.append(req.full_url)
        if raise_exc is not None:
            raise raise_exc
        return _FakeResponse(body)

    monkeypatch.setattr(_ur, "urlopen", fake)
    return seen


def test_control_uses_the_apps_own_address_and_prefix(monkeypatch):
    """Kills M244 (app and --url branches swapped) and M230 (prefix taken
    from the default when an app is given)."""
    import types as _types
    monkeypatch.delenv("WEBCORTEX_HOST", raising=False)
    monkeypatch.delenv("WEBCORTEX_PORT", raising=False)
    monkeypatch.delenv("PORT", raising=False)
    app = _types.SimpleNamespace(host="0.0.0.0", port=11150, control_prefix="/ops")
    seen = _capture_urlopen(monkeypatch)
    assert _cli._control(app, "GET", "/halt") == {"was_halted": True}
    assert seen == ["http://127.0.0.1:11150/ops/halt"]
    seen.clear()
    _cli._control(None, "POST", "/release", url="http://127.0.0.1:11151/")
    assert seen == ["http://127.0.0.1:11151/_webcortex/release"]


def test_control_reports_an_unreachable_app_as_wc108(monkeypatch):
    """Kills M247 / M248 (the URLError / OSError raise deleted: an
    UnboundLocalError traceback instead) and M263 (code renamed)."""
    import urllib.error
    _capture_urlopen(monkeypatch, raise_exc=urllib.error.URLError("refused"))
    with pytest.raises(_cli.CLIError) as e:
        _cli._control(None, "GET", "/halt", url="http://127.0.0.1:11152")
    assert e.value.code == "WC108" and "cannot reach" in e.value.message
    _capture_urlopen(monkeypatch, raise_exc=TimeoutError("timed out"))
    with pytest.raises(_cli.CLIError) as e:
        _cli._control(None, "GET", "/halt", url="http://127.0.0.1:11152")
    assert e.value.code == "WC108" and "timed out" in e.value.message


def test_control_tells_a_non_webcortex_404_from_other_errors(monkeypatch):
    """Kills M253 (`e.code == 404` negated)."""
    import io
    import urllib.error

    def http_error(code):
        return urllib.error.HTTPError("http://x", code, "err", {}, io.BytesIO(b"<html>nope</html>"))

    _capture_urlopen(monkeypatch, raise_exc=http_error(404))
    with pytest.raises(_cli.CLIError) as e:
        _cli._control(None, "GET", "/halt", url="http://127.0.0.1:11153")
    assert e.value.code == "WC108" and "not with a WebCortex error" in e.value.message
    _capture_urlopen(monkeypatch, raise_exc=http_error(500))
    with pytest.raises(_cli.CLIError) as e:
        _cli._control(None, "GET", "/halt", url="http://127.0.0.1:11153")
    from webcortex.errors import code_for_status
    assert e.value.code == code_for_status(500) and "answered 500" in e.value.message


def test_halt_loads_the_app_unless_only_a_url_is_given(monkeypatch, tmp_path):
    """Kills M252 (the `--url` without api.py condition's and/or swapped)."""
    import argparse
    monkeypatch.chdir(tmp_path)
    loaded = []
    monkeypatch.setattr(_cli, "_load_app", lambda target: loaded.append(target) or object())
    monkeypatch.setattr(_cli, "_control", lambda app, *a, **k: {"was_halted": True})

    def run(url, target=_cli.DEFAULT_MODULE):
        loaded.clear()
        _cli._cmd_estop(argparse.Namespace(url=url, target=target, command="release", key=None,
                                           status=False, reason="r"))
        return list(loaded)

    assert run("http://127.0.0.1:11154") == []              # --url, no api.py here
    assert run(None) == [_cli.DEFAULT_MODULE]                 # no --url: load it
    assert run("http://127.0.0.1:11154", "other.py") == ["other.py"]
    (tmp_path / _cli.DEFAULT_MODULE).write_text("")
    assert run("http://127.0.0.1:11154") == [_cli.DEFAULT_MODULE]  # api.py exists


# ---------------------------------------------------------------- diagnostics: the JWT secret check


@pytest.mark.parametrize("raw,expect", [
    (None, "is not set"),
    ("", "is empty"),
    ("   ", "is empty"),
    ("replace-me-run-webcortex-keygen", "is still the placeholder from .env.example"),
    ("REPLACE-ME", "is still the placeholder from .env.example"),
])
def test_the_doctor_says_why_the_jwt_secret_is_unusable(monkeypatch, raw, expect):
    """Kills M295 / M297 ("is empty" and "is not set" swapped) and M301 (the
    placeholder branch unreachable)."""
    from webcortex.diagnostics import doctor
    env = "MK_JWT_SECRET_UNDER_TEST"
    if raw is None:
        monkeypatch.delenv(env, raising=False)
    else:
        monkeypatch.setenv(env, raw)
    app = WebCortex("t")
    app.jwt(secret_env=env)
    check = next(c for c in doctor(app)["checks"] if c["id"] == "jwt")
    assert check["status"] == "fail" and check.get("code", "WC106") == "WC106"
    assert check["message"] == f"{env} (the JWT secret) {expect}"
    if raw:
        assert raw.strip() not in check["message"] or raw.strip() == ""


def test_the_doctor_passes_a_real_jwt_secret(monkeypatch):
    from webcortex.diagnostics import doctor
    monkeypatch.setenv("MK_JWT_SECRET_UNDER_TEST", "k" * 40)
    app = WebCortex("t")
    app.jwt(secret_env="MK_JWT_SECRET_UNDER_TEST")
    assert not [c for c in doctor(app)["checks"] if c["id"] == "jwt" and c["status"] == "fail"]


# ---------------------------------------------------------------- evals


from webcortex import evals as _evals  # noqa: E402


def _case(expect):
    return _evals.EvalCase(name="c", target="t", args={}, expect=expect, check=None, scopes=[],
                           description="")


def test_expect_data_compares_objects_inside_lists_exactly():
    """Kills M398 (`_same` on objects: `and` -> `or`, so any two objects
    matched): objects inside a list compare by JSON equality."""
    run = {"run_id": "r", "status": "completed", "output": "", "steps": []}
    body = {**run, "data": {"items": [{"a": 1, "b": 2}], "flag": True}}
    outcome = {"status": 200, "error": None, "usage": {}, "body": body}
    assert _evals.check_result(_case({"data": {"items": [{"a": 1, "b": 2}]}}), outcome) == []
    assert _evals.check_result(_case({"data": {"items": [{"a": 1}]}}), outcome)
    assert _evals.check_result(_case({"data": {"items": [{"a": 2, "b": 2}]}}), outcome)
    assert _evals.check_result(_case({"data": {"items": [{"a": 1, "b": 2, "c": 3}]}}), outcome)
    assert _evals.check_result(_case({"data": {"items": [{"a": True, "b": 2}]}}), outcome)
    assert _evals._same([{"a": 1}], [{"a": 1}]) and not _evals._same([{"a": 1}], [{"a": 1, "b": 1}])


@pytest.mark.parametrize("status,ok", [(100, True), (599, True), (99, False), (600, False)])
def test_expect_http_status_accepts_exactly_100_to_599(status, ok):
    """Kills M478 (`<= 599` -> `< 599`), M486 (`100 <=` -> `101 <=`) and
    M487 (`<= 600`)."""
    app = WebCortex("t")
    if ok:
        _evals.declare(app, "e", target="t", input={}, expect={"http_status": status})
    else:
        with pytest.raises(ValueError, match="HTTP status code"):
            _evals.declare(app, "e", target="t", input={}, expect={"http_status": status})


def test_a_provider_failure_is_found_in_the_body_code_and_in_the_trace():
    """Kills M442 (the body's own `code` never collected) and M443 (the
    trace's `errors` never collected)."""
    run = {"run_id": "r", "status": "failed", "steps": [{"error": "no key"}]}
    assert _evals._provider_failure({"body": {**run, "code": "WC301"}}) == ("WC301", "no key (WC301)")
    assert _evals._provider_failure({"body": {"x": 1}, "trace": {"errors": ["WC304"]}}) == \
        ("WC304", "a model call failed with WC304")
    assert _evals._provider_failure({"body": {"error": {"code": "WC302"}}})[0] == "WC302"
    assert _evals._provider_failure({"body": {**run, "code": "WC404"}}) is None


def test_reading_a_cassette_that_fails_with_another_oserror_names_it(tmp_path, monkeypatch):
    """Kills M411 (the generic OSError branch returns a bare None, which the
    caller cannot unpack) and M440 (the same for a directory that raised a
    plain OSError, as some platforms do)."""
    loop = tmp_path / "loop.json"
    loop.symlink_to(loop)  # ELOOP: neither missing, a directory nor permission
    text, problem = _evals._read_cassette(loop)
    assert text is None and str(loop) in problem and problem.startswith("cannot read the cassette")

    import errno
    import pathlib
    d = tmp_path / "dir.json"
    d.mkdir()
    real = pathlib.Path.read_text

    def read_text(self, *a, **k):
        if self == d:
            raise OSError(errno.EIO, "I/O error")
        return real(self, *a, **k)

    monkeypatch.setattr(pathlib.Path, "read_text", read_text)
    text, problem = _evals._read_cassette(d)
    assert text is None and "is a directory, not a file" in problem


def test_writing_a_cassette_makes_missing_parent_directories(tmp_path):
    """Kills M464 (`parents=True` -> `False`)."""
    path = tmp_path / "a" / "b" / "c.json"
    assert _evals._write_cassette(path, {"version": 1, "calls": []}) is None
    assert json.loads(path.read_text()) == {"calls": [], "version": 1}


def test_a_cassette_write_failure_names_the_os_reason_not_the_exception(tmp_path):
    """Kills M480 (`e.strerror or e` -> `and`: the message carried the whole
    exception text)."""
    import os as _os
    ro = tmp_path / "ro"
    ro.mkdir()
    ro.chmod(0o500)
    try:
        if _os.access(ro, _os.W_OK):
            pytest.skip("running with privileges that ignore directory permissions")
        path = ro / "c.json"
        assert _evals._write_cassette(path, {"version": 1, "calls": []}) == \
            f"cannot write the cassette {path}: Permission denied"
    finally:
        ro.chmod(0o700)


def test_an_eval_result_carries_the_routes_error_code(tmp_path, monkeypatch):
    """Kills M379 (`body.code or error.code` -> `and`: a route's coded error
    left the result's `code` empty)."""
    from webcortex import HTTPError
    monkeypatch.setenv("WEBCORTEX_FAKE_PROVIDER", "1")
    app = WebCortex("ev")

    @app.get("/missing", tool=True, tool_name="missing")
    def missing():
        raise HTTPError(404, "no such thing", code="WC404", hint="look elsewhere")

    app.eval("m1", target="missing", input={}, expect={"http_status": 404})
    result = app.run_evals(directory=str(tmp_path))["results"][0]
    assert result["ok"] is True and result["status"] == 404 and result["code"] == "WC404"


def test_a_non_cassette_error_in_replay_does_not_name_the_cassette(tmp_path, monkeypatch):
    """Kills M446 (`code == "WC320" and cassette` -> `or`): only a cassette
    problem gets the cassette's path appended."""
    monkeypatch.setenv("WEBCORTEX_FAKE_PROVIDER", "1")
    app = WebCortex("ev2")
    app.agent("bot", tools=["nope"])
    app.eval("e2", target="bot", input="hi")
    cassette = tmp_path / "e2.json"
    cassette.write_text('{"version": 1, "calls": []}')
    result = app.run_evals(directory=str(tmp_path))["results"][0]
    assert result["ok"] is False and result["mode"] == "replay"
    assert result["code"] != "WC320"
    assert str(cassette) not in result["failures"][0]


# ---------------------------------------------------------------- schema


from typing import NotRequired, TypedDict  # noqa: E402

from webcortex import schema as _schema_mod  # noqa: E402


class _PlainTD(TypedDict):
    a: int
    b: NotRequired[str]


def test_dangling_refs_are_loosened_inside_lists_too():
    """Kills M507 (`_loosen_refs` no longer walks lists: a `$ref` inside
    `anyOf` to a `$defs` entry that was never written stays dangling)."""
    s = {"anyOf": [{"$ref": "#/$defs/N"}, {"type": "null"}],
         "items": [{"$ref": "#/$defs/N"}], "keep": {"$ref": "#/$defs/Other"}}
    assert _schema_mod._loosen_refs(s, {"N"}) == {
        "anyOf": [{"type": "object", "title": "N"}, {"type": "null"}],
        "items": [{"type": "object", "title": "N"}], "keep": {"$ref": "#/$defs/Other"}}


def test_an_old_typing_extensions_without_readonly_does_not_break_plain_fields(monkeypatch):
    """Kills M521 (`_qualifier`'s `origin is None` guard deleted). With a
    typing_extensions that predates ReadOnly (< 4.9) loaded, a plain `int`
    field (origin None) matched the missing attribute (`None is None`) and was
    read as a qualifier: the schema generator then failed on it."""
    import sys as _sys
    import types as _types

    old = _types.ModuleType("typing_extensions")
    old.Required = None  # spelled as absent: getattr(old, "ReadOnly", None) is None
    old.is_typeddict = lambda tp: False
    monkeypatch.setitem(_sys.modules, "typing_extensions", old)

    assert _schema_mod._qualifier(None) is None
    assert _schema_mod.typeddict_fields(_PlainTD) == {"a": (int, True), "b": (str, False)}
    s = output_schema_for(_PlainTD)
    assert s["properties"] == {"a": {"type": "integer"}, "b": {"type": "string"}}
    assert s["required"] == ["a"]


import dataclasses as _dc  # noqa: E402
import typing as _typing  # noqa: E402
from typing import Annotated, Any, NewType  # noqa: E402

try:
    import typing_extensions as _te  # noqa: E402
except ImportError:  # CI installs it; without it the tests that need it skip
    _te = None
needs_te = pytest.mark.skipif(_te is None, reason="needs typing_extensions")

from webcortex.schema import coerce, json_schema_for  # noqa: E402

_UserId = NewType("_UserId", int)


@pytest.mark.parametrize("annotation,expected", [
    (Annotated[int, "meta"], {"type": "integer"}),
    (_UserId, {"type": "integer"}),
    (int | str, {"anyOf": [{"type": "integer"}, {"type": "string"}]}),
    (tuple[int, ...], {"type": "array", "items": {"type": "integer"}}),
    (tuple[int, str], {"type": "array", "prefixItems": [{"type": "integer"}, {"type": "string"}],
                       "minItems": 2, "maxItems": 2}),
])
def test_wrapper_and_tuple_annotations_have_their_schemas(annotation, expected):
    """Kills M542 (Annotated), M543 (NewType), M544 (Union), M572
    (`tuple[X, ...]`) and M573 (fixed tuple) returning None."""
    assert json_schema_for(annotation) == expected
    assert output_schema_for(annotation) == expected


class _NotATypedDict:
    x: int


@needs_te
def test_only_typeddicts_are_typeddicts():
    """Kills M547 / M576 (`is_td and is_td(x)` -> `or`: with the checker
    present, every annotation counted as a TypedDict)."""
    assert "typing_extensions" in __import__("sys").modules
    for ann in (int, dict, _NotATypedDict, list[int]):
        assert _schema_mod._is_typeddict(ann) is False
    assert _schema_mod._is_typeddict(_PlainTD) is True
    assert json_schema_for(_NotATypedDict) == {}
    assert coerce({"x": "1"}, _NotATypedDict) == {"x": "1"}


@needs_te
def test_typing_extensions_typeddicts_are_closed_unless_declared_open():
    """Kills M559 (`extra is not None and ...` -> `or`: a typing_extensions
    TypedDict that says nothing became open) and M618 (`extra_items=Any`
    whose schema is `{}` became closed: `or True` -> `or False`)."""
    try:
        _TEPlain = _te.TypedDict("_TEPlain", {"a": int})
        _TEExtraAny = _te.TypedDict("_TEExtraAny", {"a": int}, extra_items=Any)
        _TEOpen = _te.TypedDict("_TEOpen", {"a": int}, closed=False)
    except TypeError:
        pytest.skip("this typing_extensions has no PEP 728")
    assert output_schema_for(_TEPlain)["additionalProperties"] is False
    assert output_schema_for(_TEExtraAny)["additionalProperties"] is True
    assert output_schema_for(_TEOpen)["additionalProperties"] is True


class _Unresolved(TypedDict):
    a: int
    b: NotRequired[DoesNotExist]  # noqa: F821 - deliberately unresolvable
    c: _typing.NotRequired[AlsoMissing]  # noqa: F821


def test_an_unresolved_qualified_field_keeps_its_qualifier_and_its_text():
    """Kills M620 (the string-qualifier test negated), M630 / M631 (the dotted
    `typing.NotRequired[...]` head read wrongly) and M633 / M634 (the inner
    annotation text cut short)."""
    assert _schema_mod.typeddict_fields(_Unresolved) == {
        "a": (int, True), "b": ("DoesNotExist", False), "c": ("AlsoMissing", False)}
    assert output_schema_for(_Unresolved)["required"] == ["a"]


def _recursive_dataclass(name: str):
    cls = _dc.make_dataclass("Tmp", [("kids", list, _dc.field(default_factory=list))])
    cls.__name__ = cls.__qualname__ = name
    cls.__annotations__["kids"] = list[cls]
    for f in _dc.fields(cls):
        f.type = list[cls]
    return cls


def test_defs_names_are_sanitised_and_unique_per_schema():
    """Kills M629 (non-identifier characters kept in a `$defs` key) and M616
    (the uniqueness counter counting down: a third class of the same name got
    `Node_1`)."""
    a, b, c = (_recursive_dataclass("Node") for _ in range(3))
    odd = _recursive_dataclass("My Node-x")

    # make_dataclass with real types: this module's `from __future__ import
    # annotations` would leave a locally defined class's annotations unresolvable.
    Holder = _dc.make_dataclass("Holder", [("a", a), ("b", b), ("c", c), ("d", odd)])
    s = output_schema_for(Holder)
    assert set(s["$defs"]) == {"Node", "Node_2", "Node_3", "My_Node_x"}
    assert s["properties"]["d"]["properties"]["kids"]["items"] == {"$ref": "#/$defs/My_Node_x"}


class _Tree(TypedDict, total=False):
    n: int
    child: _Tree


def _tree(depth: int) -> dict:
    node: dict = {"n": str(depth)}
    for d in range(depth - 1, 0, -1):
        node = {"n": str(d), "child": node}
    return node


def _ns(value: dict) -> list:
    out = []
    while value is not None:
        out.append(value["n"])
        value = value.get("child")
    return out


def test_typeddict_coercion_goes_32_levels_deep_and_stops_there():
    """Kills M624 (`depth + 1` -> `+ 2`: coercion stopped at 16 levels) and
    M621 (`depth - 1`: the 32-level bound never reached)."""
    shallow = _ns(coerce(_tree(20), _Tree))
    assert shallow == list(range(1, 21))
    deep = _ns(coerce(_tree(40), _Tree))
    assert deep[:32] == list(range(1, 33))             # 32 levels converted
    assert all(isinstance(v, str) for v in deep[32:])  # past the bound: as sent
