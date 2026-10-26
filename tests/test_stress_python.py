"""Stress and regression tests for the Python surface a user touches.

Everything here is a bug that was found by writing an app the way a user
would: handlers returning the values real code returns (sets, datetimes,
enums, slotted dataclasses, NaN, generators, coroutines from a decorator),
signatures real code has (positional-only, callable objects, forward
references, recursive dataclasses), clients sending what clients send (a
broken JSON body), and the CLI run from the places it is run from. Each test
names the wrong behaviour it pins.

The server tests use one module-scoped app on a free port and generous
timeouts, so they hold on a loaded CI runner and on free-threaded builds.
"""

from __future__ import annotations

import ast
import asyncio
import dataclasses
import datetime as dt
import decimal
import enum
import functools
import inspect
import json
import math
import os
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time
import typing
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from typing import Annotated, Literal, NewType, Optional, TypedDict, Union

import pytest

from webcortex import HTTPError, Request, Response, WebCortex, starters
from webcortex import _bridge
from webcortex._bridge import Dispatcher, InvalidJSON, ResultTooLarge, _encode, jsonable
from webcortex.app import _bind_handler, _path_params, _request_param, resolve_port
from webcortex.schema import coerce, json_schema_for, schema_from_signature, type_hints


class Colour(enum.Enum):
    RED = "red"
    BLUE = "blue"


class Level(enum.IntEnum):
    LOW = 1
    HIGH = 2


@dataclasses.dataclass(slots=True)
class Slotted:
    a: int
    b: str


@dataclasses.dataclass
class Item:
    id: int
    name: str


@dataclasses.dataclass
class Order:
    id: int
    items: list[Item]
    placed: dt.date


@dataclasses.dataclass
class Node:
    name: str
    children: list[Node] = dataclasses.field(default_factory=list)


def body(value) -> object:
    status, raw, ctype, headers = _encode(value)
    return json.loads(raw)


# ------------------------------------------------------------- encoding


def test_nan_and_infinity_encode_as_null_not_invalid_json():
    # The stdlib writes the bare tokens NaN/Infinity, which JSON.parse rejects.
    _, raw, _, _ = _encode({"x": math.nan, "y": [math.inf, -math.inf, 1.5]})
    assert json.loads(raw) == {"x": None, "y": [None, None, 1.5]}
    assert b"NaN" not in raw and b"Infinity" not in raw


def test_python_values_encode_by_their_documented_wire_form():
    when = dt.datetime(2024, 1, 2, 3, 4, 5)
    out = body({
        "when": when,                       # ISO 8601, as format: date-time promises
        "day": dt.date(2024, 1, 2),
        "at": dt.time(1, 2, 3),
        "took": dt.timedelta(seconds=90),
        "price": decimal.Decimal("1.10"),
        "id": uuid.UUID(int=1),
        "colour": Colour.RED,
        "level": Level.HIGH,
        "tags": {"b", "a"},
        "frozen": frozenset({3, 1, 2}),
        "raw": b"\x00\xff",
        "range": range(3),
        "slotted": Slotted(1, "x"),
    })
    assert out == {
        "when": "2024-01-02T03:04:05",
        "day": "2024-01-02",
        "at": "01:02:03",
        "took": 90.0,
        "price": "1.10",
        "id": "00000000-0000-0000-0000-000000000001",
        "colour": "red",
        "level": 2,
        "tags": ["a", "b"],
        "frozen": [1, 2, 3],
        "raw": "AP8=",
        "range": [0, 1, 2],
        "slotted": {"a": 1, "b": "x"},
    }


def test_an_enum_never_publishes_its_class_internals():
    # 2.5.3 encoded an Enum member through vars(), which reached __objclass__
    # and wrote the class dict (module name, function addresses) into the body.
    _, raw, _, _ = _encode({"c": Colour.BLUE})
    assert json.loads(raw) == {"c": "blue"}
    assert b"__" not in raw


def test_a_class_or_function_value_is_named_not_dumped():
    out = body({"cls": Item, "fn": test_nan_and_infinity_encode_as_null_not_invalid_json})
    assert out == {"cls": "Item", "fn": "test_nan_and_infinity_encode_as_null_not_invalid_json"}


def test_a_plain_object_encodes_its_public_attributes_only():
    class Account:
        def __init__(self):
            self.name = "ada"
            self._token = "secret"

    assert body(Account()) == {"name": "ada"}


def test_generators_and_iterators_encode_as_arrays():
    def gen():
        yield from (1, 2, 3)

    assert body(gen()) == [1, 2, 3]
    assert body({"k": iter("ab"), "m": {"x": 1}.keys()}) == {"k": ["a", "b"], "m": ["x"]}


def test_non_string_dict_keys_are_converted():
    out = body({1: "a", Colour.RED: "b", dt.date(2024, 1, 2): "c", None: "d", 2.5: math.nan})
    assert out == {"1": "a", "red": "b", "2024-01-02": "c", "null": "d", "2.5": None}


def test_a_reference_cycle_is_refused_not_recursed():
    d: dict = {"a": 1}
    d["self"] = d
    with pytest.raises(ValueError):
        _encode(d)
    with pytest.raises(ValueError, match="circular"):
        jsonable(d)
    # A value seen twice is not a cycle.
    shared = [1]
    assert jsonable({"a": shared, "b": shared}) == {"a": [1], "b": [1]}


def test_tolist_hook_covers_array_types():
    class FakeArray:
        def tolist(self):
            return [[1, 2], [3, 4]]

    assert body({"m": FakeArray()}) == {"m": [[1, 2], [3, 4]]}


def test_nested_dataclasses_and_their_nan_fields():
    @dataclasses.dataclass
    class Reading:
        value: float
        at: dt.datetime

    out = body([Reading(math.nan, dt.datetime(2024, 1, 1)), Reading(2.0, dt.datetime(2024, 1, 2))])
    assert out == [{"value": None, "at": "2024-01-01T00:00:00"}, {"value": 2.0, "at": "2024-01-02T00:00:00"}]


def test_text_bodies_for_non_json_types_are_sent_as_written():
    for ctype in ("application/xml", "image/svg+xml", "text/csv; charset=utf-8", "application/javascript"):
        status, raw, out_type, _ = _encode(Response("<a/>", content_type=ctype))
        assert raw == b"<a/>", ctype
        assert out_type == ctype
    # A string for a JSON type is still a JSON string.
    assert _encode(Response("hi"))[1] == b'"hi"'
    assert _encode(Response("hi", content_type="application/problem+json"))[1] == b'"hi"'


def test_bytes_like_bodies_are_sent_raw():
    for payload in (b"hi", bytearray(b"hi"), memoryview(b"hi")):
        assert _encode(Response(payload, content_type="application/octet-stream"))[1] == b"hi"


def test_status_must_be_a_real_http_status():
    for bad in (0, 99, 600, 1000, True, "200", 200.0, None):
        with pytest.raises((ValueError, TypeError)):
            _encode(Response({"a": 1}, status=bad))
    assert _encode(Response({"a": 1}, status=201))[0] == 201
    import http

    assert _encode(Response({"a": 1}, status=http.HTTPStatus.ACCEPTED))[0] == 202


def test_none_is_204_unless_a_status_is_given():
    assert _encode(None)[:2] == (204, b"")
    assert _encode(Response(None, status=201))[:2] == (201, b"")


def test_headers_accept_numbers_and_pairs_and_refuse_injection():
    status, _, _, headers = _encode(Response({}, headers={"x-count": 5, "x-ratio": 0.5}))
    assert headers == [("x-count", "5"), ("x-ratio", "0.5")]
    # Two Set-Cookie headers need a list of pairs: a dict cannot hold them.
    _, _, _, headers = _encode(Response({}, headers=[("set-cookie", "a=1"), ("set-cookie", "b=2")]))
    assert headers == [("set-cookie", "a=1"), ("set-cookie", "b=2")]
    for bad in ({"x": "a\r\nset-cookie: evil=1"}, {"x": "a\nb"}, {"x": "nul\0"}, {"bad name": "v"},
                {"": "v"}, {"x": None}, {"x": True}, {"x": b"bytes"}, [("only-one",)]):
        with pytest.raises((ValueError, TypeError)):
            _encode(Response({}, headers=bad))


def test_a_content_type_header_replaces_the_default_instead_of_doubling_it():
    status, raw, ctype, headers = _encode(Response("<p>", headers={"Content-Type": "text/html"}))
    assert ctype == "text/html" and raw == b"<p>"
    assert not any(k.lower() == "content-type" for k, _ in headers)
    with pytest.raises(ValueError):
        _encode(Response({}, content_type="text/html\r\nx-evil: 1"))


# ------------------------------------------------------------- requests


def _request(body: bytes = b"", **extra) -> Request:
    raw = {"method": "POST", "path": "/x", "path_params": {}, "query": {}, "headers": {},
           "body": body, "scopes": []}
    raw.update(extra)
    return Request(raw)


def test_a_body_that_is_not_json_is_a_400_and_still_a_value_error():
    for bad in (b"{bad json", b"\xff\xfe", b"[1,"):
        req = _request(bad)
        with pytest.raises(InvalidJSON) as info:
            req.json()
        assert isinstance(info.value, HTTPError) and isinstance(info.value, ValueError)
        assert info.value.status == 400 and info.value.code == "WC205"
    assert _request(b"").json() is None


# ------------------------------------------------------------- coercion


@pytest.mark.parametrize(
    "raw,annotation,expected",
    [
        ("2024-01-02", dt.date, dt.date(2024, 1, 2)),
        ("2024-01-02T03:04:05", dt.datetime, dt.datetime(2024, 1, 2, 3, 4, 5)),
        ("01:02", dt.time, dt.time(1, 2)),
        ("00000000-0000-0000-0000-000000000001", uuid.UUID, uuid.UUID(int=1)),
        ("1.10", decimal.Decimal, decimal.Decimal("1.10")),
        ("red", Colour, Colour.RED),
        ("RED", Colour, Colour.RED),
        ("2", Level, Level.HIGH),
        (2, Level, Level.HIGH),
        ("2", Literal[1, 2], 2),
        ("b", Literal["a", "b"], "b"),
        ("AP8=", bytes, b"\x00\xff"),
        (["1", "2"], list[int], [1, 2]),
        (["1", "1"], set[int], {1}),
        (["1", "x"], tuple[int, str], (1, "x")),
        ({"a": "1"}, dict[str, int], {"a": 1}),
        ("7", Annotated[int, "doc"], 7),
        ("7", NewType("UserId", int), 7),
        (3.0, int, 3),
        ("42", Optional[int], 42),
    ],
)
def test_coerce_reaches_the_annotated_type(raw, annotation, expected):
    out = coerce(raw, annotation)
    assert out == expected and type(out) is type(expected)


@pytest.mark.parametrize(
    "raw,annotation",
    [
        (3.7, int),                 # truncating to 3 would invent a value
        ({"a": 1}, str),            # repr() of an object is not the caller's string
        ([1], str),
        ("nan", float),             # no JSON Schema number is NaN
        ("inf", float),
        ("not-a-date", dt.date),
        ("purple", Colour),
        ("3", Literal[1, 2]),
        ("not base64!", bytes),
        ("abc", int),               # the documented pass-through
    ],
)
def test_coerce_passes_through_what_it_cannot_convert_faithfully(raw, annotation):
    assert coerce(raw, annotation) == raw


def test_coerce_builds_nested_dataclasses():
    order = coerce({"id": "1", "items": [{"id": "2", "name": "pen"}], "placed": "2024-01-02"}, Order)
    assert order == Order(id=1, items=[Item(2, "pen")], placed=dt.date(2024, 1, 2))
    assert coerce([{"id": 1, "name": "a"}], list[Item]) == [Item(1, "a")]
    # Unknown keys: the dict passes through for the handler to judge.
    assert coerce({"id": 1, "name": "a", "extra": 1}, Item) == {"id": 1, "name": "a", "extra": 1}


def test_coerce_typeddict_fields():
    class Point(TypedDict):
        x: int
        y: int

    assert coerce({"x": "1", "y": 2}, Point) == {"x": 1, "y": 2}


# ------------------------------------------------------------- schemas


def test_enum_schema_lists_its_values():
    assert json_schema_for(Colour) == {"enum": ["red", "blue"], "title": "Colour", "type": "string"}
    assert json_schema_for(Level)["enum"] == [1, 2]
    assert json_schema_for(Literal[Colour.RED]) == {"enum": ["red"]}


def test_fixed_tuples_are_positional():
    assert json_schema_for(tuple[int, str]) == {
        "type": "array", "prefixItems": [{"type": "integer"}, {"type": "string"}],
        "minItems": 2, "maxItems": 2,
    }
    assert json_schema_for(tuple[int, ...]) == {"type": "array", "items": {"type": "integer"}}


def test_bare_containers_and_sets():
    assert json_schema_for(list) == {"type": "array"}
    assert json_schema_for(dict) == {"type": "object"}
    assert json_schema_for(set[int]) == {"type": "array", "items": {"type": "integer"}, "uniqueItems": True}
    assert json_schema_for(typing.Sequence[str]) == {"type": "array", "items": {"type": "string"}}
    assert json_schema_for(typing.Mapping[str, int])["additionalProperties"] == {"type": "integer"}


def test_recursive_dataclasses_do_not_recurse_forever():
    schema = json_schema_for(Node)
    assert schema["properties"]["children"]["items"] == {"type": "object", "title": "Node"}

    @dataclasses.dataclass
    class A:
        b: Optional[B]  # noqa: F821 - resolved below, mutually recursive

    @dataclasses.dataclass
    class B:
        a: A

    # Local classes: give the annotations as objects, as a module-level
    # definition would resolve them.
    A.__annotations__["b"] = Optional[B]
    B.__annotations__["a"] = A
    assert json_schema_for(A)["properties"]["b"]["properties"]["a"] == {"type": "object", "title": "A"}


def test_typeddict_schema_marks_required_keys():
    # Functional syntax: under `from __future__ import annotations` the class
    # syntax hides `Required[...]` from TypedDict itself.
    Movie = TypedDict("Movie", {"title": typing.Required[str], "year": int}, total=False)

    schema = json_schema_for(Movie)
    assert schema["properties"] == {"title": {"type": "string"}, "year": {"type": "integer"}}
    assert schema["required"] == ["title"]


def test_annotated_newtype_and_subclassed_primitives():
    class Name(str):
        pass

    assert json_schema_for(Annotated[int, "meta"]) == {"type": "integer"}
    assert json_schema_for(NewType("UserId", int)) == {"type": "integer"}
    assert json_schema_for(Name) == {"type": "string"}
    assert json_schema_for(uuid.UUID) == {"type": "string", "format": "uuid"}


def test_optional_literal_admits_null():
    assert json_schema_for(Optional[Literal["a", "b"]]) == {"enum": ["a", "b", None], "nullable": True}


def test_exotic_annotations_degrade_instead_of_raising():
    assert json_schema_for([int]) == {}          # unhashable, a common typo for list[int]
    assert json_schema_for("NotAType") == {}
    assert json_schema_for(typing.Callable[[int], int]) == {}


def test_one_unresolvable_forward_reference_does_not_disable_the_others():
    # Under `from __future__ import annotations` every annotation is a string;
    # 2.5.3 gave up on all of them when one did not resolve, so `id: int`
    # silently stopped being coerced.
    def handler(id: "int", when: "dt.date", thing: "OnlyUnderTypeChecking") -> "dict": ...  # noqa: F821

    hints = type_hints(handler)
    assert hints["id"] is int and hints["when"] is dt.date and hints["return"] is dict
    assert hints["thing"] == "OnlyUnderTypeChecking"
    schema, _ = schema_from_signature(handler, [])
    assert schema["properties"]["id"] == {"type": "integer"}


def test_defaults_without_a_json_spelling_are_left_out_of_the_schema():
    sentinel = object()

    def handler(a: int = 1, b: object = sentinel, c: Colour = Colour.RED,
                d: dt.date = dt.date(2024, 1, 2), e: float = math.nan, f: frozenset = frozenset()): ...

    schema, _ = schema_from_signature(handler, [])
    props = schema["properties"]
    assert props["a"]["default"] == 1
    assert "default" not in props["b"] and "default" not in props["e"]
    assert props["c"]["default"] == "red"
    assert props["d"]["default"] == "2024-01-02"
    assert props["f"]["default"] == []
    json.dumps(schema, allow_nan=False)  # the manifest must stay valid JSON


def test_an_app_with_such_defaults_still_builds_its_manifest():
    app = WebCortex("defaults")

    @app.get("/x")
    def x(when: dt.date = dt.date(2024, 1, 1), marker: object = object()) -> dict:
        return {}

    json.loads(app.manifest_json())
    app.check()


# ------------------------------------------------------------- handler binding


def _call(handler, **raw):
    req = _request(**raw)
    out = handler(req)
    if asyncio.iscoroutine(out):
        out = asyncio.run(out)
    return out


def test_positional_only_parameters_bind():
    def f(a: int, /, b: int = 2) -> dict:
        return {"a": a, "b": b}

    assert _call(_bind_handler(f, None), path_params={"a": "3"}) == {"a": 3, "b": 2}


def test_an_object_with_async_call_is_an_async_handler():
    class Handler:
        async def __call__(self, n: int) -> dict:
            return {"n": n}

    bound = _bind_handler(Handler(), None)
    assert inspect.iscoroutinefunction(bound)
    assert _call(bound, query={"n": "4"}) == {"n": 4}


def test_partial_of_an_async_function_is_async():
    async def f(a: int, b: int) -> dict:
        return {"sum": a + b}

    bound = _bind_handler(functools.partial(f, b=1), None)
    assert inspect.iscoroutinefunction(bound)
    assert _call(bound, query={"a": "2"}) == {"sum": 3}


def test_async_generator_handlers_are_collected():
    async def stream(n: int):
        for i in range(n):
            yield i

    assert _call(_bind_handler(stream, None), query={"n": "3"}) == [0, 1, 2]


def test_wildcard_path_params_bind_by_their_name():
    assert _path_params("/files/{*rest}") == ["rest"]
    app = WebCortex("w")

    @app.get("/files/{*rest}")
    def files(rest: str) -> dict:
        return {}

    schema = app._routes[0]["input_schema"]
    assert set(schema["properties"]) == {"rest"} and schema["required"] == ["rest"]


def test_request_param_detection_survives_odd_annotations():
    def f(r: "Request", x: "Undefined") -> None: ...  # noqa: F821

    assert _request_param(f) == "r"


def test_page_handler_summary_is_the_first_docstring_line():
    app = WebCortex("p", templates=".")

    @app.page_handler("/dash", "dash.html")
    def dash() -> dict:
        """Dashboard.

        Lots of detail that is not a summary.
        """
        return {}

    route = app._routes[0]
    assert route["summary"] == "Dashboard."
    assert "Lots of detail" in route["description"]


# ------------------------------------------------------------- the dispatcher


class FakeCompleter:
    def __init__(self) -> None:
        self.done = threading.Event()
        self.calls: list = []
        self.lock = threading.Lock()

    def complete(self, status, body, content_type="application/json", headers=None):
        with self.lock:
            self.calls.append(("complete", status, bytes(body), content_type, headers))
        self.done.set()

    def fail(self, message):
        with self.lock:
            self.calls.append(("fail", message))
        self.done.set()

    def wait(self) -> tuple:
        assert self.done.wait(30), "the completer was never called"
        time.sleep(0.01)
        with self.lock:
            assert len(self.calls) == 1, f"completer called {len(self.calls)} times: {self.calls}"
            return self.calls[0]


RAW = {"method": "GET", "path": "/", "path_params": {}, "query": {}, "headers": {}, "body": b"",
       "scopes": []}


def _dispatch(dispatcher, index) -> tuple:
    c = FakeCompleter()
    dispatcher.submit(index, dict(RAW), c)
    return c.wait()


# ---- endless generators ------------------------------------------------------
# 2.6.0 collected a returned generator with list(): an endless one grew until
# the process ran out of memory, and kept growing after the caller timed out.


class _Tracked:
    """An endless generator that records whether it was closed."""

    def __init__(self, item=0, delay: float = 0.0) -> None:
        self.closed = False
        self.item = item
        self.delay = delay

    def __call__(self, req=None):
        try:
            while True:
                if self.delay:
                    time.sleep(self.delay)
                yield self.item
        finally:
            self.closed = True


def _error(raw: bytes) -> dict:
    return json.loads(raw)["error"]


def test_an_endless_sync_generator_fails_with_wc406_and_is_closed(monkeypatch):
    monkeypatch.setattr(_bridge, "MAX_RESULT_ITEMS", 1000)
    gen = _Tracked()
    d = Dispatcher([gen], workers=1)
    try:
        kind, status, raw, *_ = _dispatch(d, 0)
        assert (kind, status, _error(raw)["code"]) == ("complete", 500, "WC406")
        assert "1000 items" in _error(raw)["message"]
        assert gen.closed
    finally:
        d.shutdown()


def test_an_endless_async_generator_handler_fails_with_wc406_and_is_closed(monkeypatch):
    monkeypatch.setattr(_bridge, "MAX_RESULT_ITEMS", 500)
    state = {"closed": False}

    async def stream():
        try:
            while True:
                yield 1
        finally:
            state["closed"] = True

    d = Dispatcher([_bind_handler(stream, None)], workers=1)
    try:
        kind, status, raw, *_ = _dispatch(d, 0)
        assert (kind, status, _error(raw)["code"]) == ("complete", 500, "WC406")
        assert state["closed"]
    finally:
        d.shutdown()


def test_a_generator_of_big_items_hits_the_byte_cap(monkeypatch):
    monkeypatch.setattr(_bridge, "MAX_RESULT_BYTES", 4 * 1024 * 1024)
    gen = _Tracked(item={"blob": "x" * (1024 * 1024)})
    d = Dispatcher([gen], workers=1)
    try:
        kind, status, raw, *_ = _dispatch(d, 0)
        assert (kind, status, _error(raw)["code"]) == ("complete", 500, "WC406")
        assert "bytes" in _error(raw)["message"] and gen.closed
    finally:
        d.shutdown()


def test_a_generator_nested_in_a_result_is_bounded_too(monkeypatch):
    monkeypatch.setattr(_bridge, "MAX_RESULT_ITEMS", 100)
    with pytest.raises(ResultTooLarge):
        jsonable({"rows": _Tracked()()})
    # Finite lazy iterables are unaffected.
    assert jsonable({"rows": (i for i in range(100)), "r": range(3)}) == {"rows": list(range(100)),
                                                                            "r": [0, 1, 2]}


class _GivesUp(FakeCompleter):
    """A caller that stops waiting after `after` seconds."""

    def __init__(self, after: float) -> None:
        super().__init__()
        self.deadline = time.monotonic() + after

    def abandoned(self) -> bool:
        return time.monotonic() > self.deadline


def test_a_slow_endless_generator_stops_once_the_caller_is_gone():
    # Too slow to reach either cap: only the caller leaving can stop it.
    gen = _Tracked(delay=0.01)
    d = Dispatcher([gen], workers=1)
    try:
        c = _GivesUp(0.3)
        d.submit(0, dict(RAW), c)
        kind, status, raw, *_ = c.wait()
        assert (kind, status, _error(raw)["code"]) == ("complete", 504, "WC203")
        assert gen.closed
    finally:
        d.shutdown()


def test_a_slow_endless_async_generator_stops_once_the_caller_is_gone():
    state = {"closed": False}

    async def stream():
        try:
            while True:
                await asyncio.sleep(0.01)
                yield 1
        finally:
            state["closed"] = True

    d = Dispatcher([_bind_handler(stream, None)], workers=1)
    try:
        c = _GivesUp(0.3)
        d.submit(0, dict(RAW), c)
        kind, status, raw, *_ = c.wait()
        assert (kind, status) == ("complete", 504) and state["closed"]
    finally:
        d.shutdown()


def test_a_finished_calls_caller_check_does_not_leak_to_the_next_call_on_the_thread():
    # The "has the caller gone?" check is set per call and reset after it, so
    # a reused worker never judges a later call by a finished caller.
    seen = []

    def handler(req):
        seen.append(_bridge._ABANDONED.get())
        return {"ok": True}

    d = Dispatcher([handler], workers=1)
    try:
        for _ in range(2):
            c = FakeCompleter()
            d.submit(0, dict(RAW), c)
            assert c.wait()[0] == "complete"
        gone = _GivesUp(-1)
        d.submit(0, dict(RAW), gone)
        gone.wait()
        # The next call on the same (only) worker thread, from a caller with
        # no check of its own, must not inherit the gone caller's.
        later = FakeCompleter()
        d.submit(0, dict(RAW), later)
        later.wait()
        assert seen[-1] is None, "the finished caller's check leaked"
    finally:
        d.shutdown()
    assert _bridge._ABANDONED.get() is None


def test_a_behaviour_result_uses_its_own_caller_check(monkeypatch):
    # A behaviour's result was collected under whatever check the thread
    # last held. A finished caller's check reported "gone", so on a slow
    # machine an endless behaviour failed as "could not be serialised"
    # instead of WC406.
    import concurrent.futures

    token = _bridge._ABANDONED.set(lambda: True)  # a stale, finished caller
    try:
        f = concurrent.futures.Future()
        f.set_result({"rows": (i for i in range(10))})
        got = []

        class Behaviour:  # a behaviour's completer: complete(value) / fail(message)
            complete = got.append
            fail = got.append

            @staticmethod
            def abandoned():
                return False

        monkeypatch.setattr(_bridge, "_CHECK_EVERY", 0)
        _bridge._settle_behaviour(f, Behaviour())
        assert got == [{"rows": list(range(10))}], got
    finally:
        _bridge._ABANDONED.reset(token)


def test_the_result_caps_are_read_from_the_environment():
    code = ("import webcortex._bridge as b; print(b.MAX_RESULT_ITEMS, b.MAX_RESULT_BYTES)")
    env = {**os.environ, "WEBCORTEX_MAX_RESULT_ITEMS": "7", "WEBCORTEX_MAX_RESULT_BYTES": "nope"}
    out = subprocess.run([sys.executable, "-c", code], env=env, capture_output=True, text=True, check=True)
    assert out.stdout.split() == ["7", str(64 * 1024 * 1024)]


def test_sys_exit_in_an_async_handler_does_not_kill_its_event_loop():
    # 2.5.3: SystemExit escaped the task into run_forever(), the loop thread
    # died, and every later request queued on that loop timed out (504).
    async def exits(req):
        raise SystemExit(3)

    async def interrupts(req):
        raise KeyboardInterrupt

    async def fine(req):
        return {"ok": True}

    d = Dispatcher([exits, interrupts, fine], workers=2)
    try:
        assert _dispatch(d, 0)[0] == "fail"
        assert _dispatch(d, 1)[0] == "fail"
        for _ in range(8):  # both loops, several times
            kind, status, raw, *_ = _dispatch(d, 2)
            assert (kind, status, json.loads(raw)) == ("complete", 200, {"ok": True})
    finally:
        d.shutdown()


def test_sys_exit_in_a_sync_handler_fails_only_that_request():
    def exits(req):
        raise SystemExit(1)

    def fine(req):
        return {"ok": True}

    d = Dispatcher([exits, fine], workers=1)
    try:
        assert _dispatch(d, 0)[0] == "fail"
        assert _dispatch(d, 1)[:2] == ("complete", 200)
    finally:
        d.shutdown()


def test_a_sync_handler_returning_a_coroutine_is_awaited():
    async def inner():
        await asyncio.sleep(0)
        return {"awaited": True}

    @functools.wraps(inner)
    def decorated(req):  # a decorator that forgot to be async
        return inner()

    d = Dispatcher([decorated], workers=1)
    try:
        kind, status, raw, *_ = _dispatch(d, 0)
        assert json.loads(raw) == {"awaited": True}
    finally:
        d.shutdown()


def test_unsendable_results_fail_without_leaking_a_traceback(monkeypatch):
    import webcortex._bridge as bridge

    monkeypatch.setattr(bridge, "_EXPOSE_TRACEBACKS", False)
    cyclic: dict = {}
    cyclic["self"] = cyclic

    handlers = [
        lambda req: Response({}, status=1000),
        lambda req: Response({}, headers={"x": "a\r\nb"}),
        lambda req: cyclic,
    ]
    d = Dispatcher(handlers, workers=1)
    try:
        for i in range(len(handlers)):
            kind, message = _dispatch(d, i)
            assert kind == "fail"
            assert "Traceback" not in message and "File " not in message, message
    finally:
        d.shutdown()


def test_a_refused_complete_still_resolves_the_request():
    class Refusing(FakeCompleter):
        def complete(self, *a, **k):
            raise ValueError("refused by the runtime")

    d = Dispatcher([lambda req: {"ok": 1}], workers=1)
    try:
        c = Refusing()
        d.submit(0, dict(RAW), c)
        assert c.wait()[0] == "fail"
    finally:
        d.shutdown()


def test_http_error_with_an_odd_status_or_message_still_answers():
    d = Dispatcher([
        lambda req: (_ for _ in ()).throw(HTTPError("404", "gone")),
        lambda req: (_ for _ in ()).throw(HTTPError(9999, "nonsense")),
        lambda req: (_ for _ in ()).throw(HTTPError(409, {"why": "dup"})),
    ], workers=1)
    try:
        assert _dispatch(d, 0)[1] == 404
        assert _dispatch(d, 1)[1] == 500
        kind, status, raw, *_ = _dispatch(d, 2)
        assert status == 409 and "dup" in json.loads(raw)["error"]["message"]
    finally:
        d.shutdown()


def test_concurrent_submissions_resolve_every_request_exactly_once():
    lock = threading.Lock()
    counter = {"n": 0}

    def sync(req):
        with lock:
            counter["n"] += 1
        return {"i": int(req.query["i"])}

    async def asynch(req):
        await asyncio.sleep(0)
        with lock:
            counter["n"] += 1
        return {"i": int(req.query["i"])}

    d = Dispatcher([sync, asynch], workers=4)
    completers: list[tuple[int, FakeCompleter]] = []
    clock = threading.Lock()

    def fire(start: int) -> None:
        for i in range(start, start + 50):
            c = FakeCompleter()
            with clock:
                completers.append((i, c))
            d.submit(i % 2, {**RAW, "query": {"i": str(i)}}, c)

    threads = [threading.Thread(target=fire, args=(k * 50,)) for k in range(16)]
    try:
        for t in threads:
            t.start()
        for t in threads:
            t.join(60)
        assert len(completers) == 800
        for i, c in completers:
            kind, status, raw, *_ = c.wait()
            assert (kind, status) == ("complete", 200)
            assert json.loads(raw) == {"i": i}
        assert counter["n"] == 800
    finally:
        d.shutdown()


def test_behaviour_results_are_made_plain_json_before_crossing_into_rust():
    assert jsonable({"nan": math.nan, "s": {2, 1}, "d": dt.date(2024, 1, 2), "e": Colour.RED}) == {
        "nan": None, "s": [1, 2], "d": "2024-01-02", "e": "red",
    }


# ------------------------------------------------------------- declarations


def test_a_text_primary_key_is_sent_on_create_and_never_null():
    # 2.5.3 left a non-INTEGER key out of the INSERT, so every row got a NULL
    # key that could never be fetched, updated or deleted.
    app = WebCortex("tags", database="sqlite://:memory:")
    app.resource("tags", fields={"slug": str, "label": str}, primary_key="slug")
    create = next(r for r in app._routes if r["method"] == "POST")
    assert create["op"]["params"] == ["slug", "label"]
    assert "slug" in create["input_schema"]["required"]
    assert "slug TEXT PRIMARY KEY NOT NULL" in app.schema_sql

    app2 = WebCortex("notes", database="sqlite://:memory:")
    app2.resource("notes", fields={"id": int, "body": str})
    create2 = next(r for r in app2._routes if r["method"] == "POST")
    assert create2["op"]["params"] == ["body"]  # the INTEGER key is still generated


def test_optional_fields_keep_their_column_type():
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("readings", fields={"id": int, "value": Optional[float], "n": int | None,
                                     "ok": Annotated[bool, "x"]})
    assert "value REAL" in app.schema_sql and "n INTEGER" in app.schema_sql
    assert "ok INTEGER" in app.schema_sql


@pytest.mark.parametrize("kwargs", [
    {"name": "my-items"},                                     # table defaults to name
    {"name": "items", "table": "items; DROP TABLE x"},
    {"name": "items", "fields": {"id": int, "full name": str}},
    {"name": "a/b", "table": "ab"},
    {"name": "items", "fields": {}},
])
def test_resource_names_that_cannot_be_sql_are_refused_at_declaration(kwargs):
    app = WebCortex("t", database="sqlite://:memory:")
    kwargs = {"fields": {"id": int, "name": str}, **kwargs}
    with pytest.raises(ValueError):
        app.resource(**kwargs)


def test_a_hyphenated_resource_works_with_an_explicit_table():
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("my-items", table="my_items", fields={"id": int, "name": str})
    assert "CREATE TABLE IF NOT EXISTS my_items" in app.schema_sql
    app.check()


def test_static_page_and_context_data_use_the_handler_encoding():
    app = WebCortex("s", templates=".")
    app.static("GET", "/s", {"tags": {"b", "a"}, "at": dt.date(2024, 1, 2), "x": math.nan})
    app.page("/p", "p.html", data={"when": dt.datetime(2024, 1, 2)})
    app.context("c", data={"colour": Colour.RED})
    m = json.loads(app.manifest_json())
    assert m["routes"][0]["op"]["body"] == {"tags": ["a", "b"], "at": "2024-01-02", "x": None}
    assert m["routes"][1]["op"]["data"]["value"] == {"when": "2024-01-02T00:00:00"}
    assert m["contexts"][0]["source"]["value"] == {"colour": "red"}


def test_port_and_workers_are_validated_with_a_message_that_names_them(monkeypatch):
    for bad in ("abc", "70000", "-1", ""):
        monkeypatch.setenv("WEBCORTEX_PORT", bad)
        with pytest.raises(ValueError, match="WEBCORTEX_PORT"):
            WebCortex("p").manifest()
    monkeypatch.setenv("WEBCORTEX_PORT", " 8123 ")
    assert resolve_port(8000) == 8123
    monkeypatch.delenv("WEBCORTEX_PORT")
    with pytest.raises(ValueError, match="port"):
        WebCortex("p", port=99999).manifest()
    for bad in (-1, 1.5, "4", True):
        with pytest.raises(ValueError, match="workers"):
            WebCortex("p", workers=bad)
    assert WebCortex("p", workers=0).workers == 0


def test_wildcard_and_dotted_paths_derive_valid_tool_names():
    app = WebCortex("t")

    @app.get("/files/{*rest}", tool=True)
    def files(rest: str) -> dict:
        return {}

    @app.get("/v1.0/items", tool=True)
    def items() -> dict:
        return {}

    tools = app.check()["tools"]
    assert "get_files_by_rest" in tools and "get_v1_0_items" in tools
    for name in tools:
        assert re.fullmatch(r"[A-Za-z0-9_-]+", name), name
    # The Python mirror (context pack, security report) agrees with Rust.
    assert "get_files_by_rest" in app.context_pack()


def test_an_explicit_tool_name_no_provider_accepts_is_refused():
    app = WebCortex("t")

    @app.get("/x", tool=True, tool_name="search books")
    def x() -> dict:
        return {}

    with pytest.raises(ValueError, match="search books"):
        app.check()


def test_context_pack_shows_unions_tuples_and_nulls():
    app = WebCortex("t")

    @app.get("/x", tool=True)
    def x(a: Union[int, str], b: Optional[int] = None, c: tuple[int, str] = (1, "a")) -> dict:
        return {}

    pack = app.context_pack()
    assert "get_x(a: integer|string, b?: integer|null, c?: [integer, string])" in pack


# ------------------------------------------------------------- CLI


def _cli(args: list[str], cwd: Path, **env) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, "-m", "webcortex.cli", *args], cwd=cwd, capture_output=True,
                          text=True, env={**os.environ, **env}, timeout=120)


def test_cli_reports_a_bad_port_as_a_coded_error_not_a_traceback(tmp_path):
    (tmp_path / "api.py").write_text("from webcortex import WebCortex\napp = WebCortex('x')\n")
    for command in (["check"], ["openapi"], ["mcp-config"], ["typegen", "-o", "c.ts"]):
        out = _cli(command, tmp_path, WEBCORTEX_PORT="abc")
        assert out.returncode == 1, (command, out.stdout, out.stderr)
        text = out.stdout + out.stderr
        assert "WC101" in text and "WEBCORTEX_PORT" in text and "Traceback" not in text, (command, text)


def test_cli_run_refuses_negative_workers_cleanly(tmp_path):
    (tmp_path / "api.py").write_text("from webcortex import WebCortex\napp = WebCortex('x')\n")
    out = _cli(["run", "--workers", "-1"], tmp_path, WEBCORTEX_PORT=str(_free_port()))
    assert out.returncode == 1 and "WC101" in out.stderr and "Traceback" not in out.stderr


def test_mcp_config_follows_a_custom_control_prefix():
    from webcortex.cli import mcp_config

    app = WebCortex("x", control_prefix="/ops")
    assert mcp_config(app)["mcpServers"]["x"]["url"].endswith("/ops/mcp")


def test_cli_targets_split_like_a_user_types_them():
    from webcortex.cli import _split_target

    assert _split_target("api.py") == ("api.py", "app")
    assert _split_target("api.py:service") == ("api.py", "service")
    assert _split_target("pkg.api:app") == ("pkg.api", "app")
    # A Windows path is not module "C", attribute "\\proj\\api.py".
    assert _split_target("C:\\proj\\api.py") == ("C:\\proj\\api.py", "app")
    assert _split_target("C:\\proj\\api.py:service") == ("C:\\proj\\api.py", "service")


def test_a_package_module_target_imports_from_the_working_directory(tmp_path):
    pkg = tmp_path / "svc"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "web.py").write_text("from webcortex import WebCortex\nservice = WebCortex('pkgapp')\n")
    # Run as the console script would: a sys.path that does not include cwd.
    code = ("import sys, os; sys.path = [p for p in sys.path if p not in ('', os.getcwd())]; "
            "from webcortex.cli import main; raise SystemExit(main(['check', 'svc.web:service']))")
    out = subprocess.run([sys.executable, "-c", code], cwd=tmp_path, capture_output=True, text=True,
                         timeout=120)
    assert out.returncode == 0, out.stdout + out.stderr
    assert json.loads(out.stdout)["name"] == "pkgapp"


@pytest.mark.parametrize("line,expected", [
    ("KEY=value", ("KEY", "value")),
    ("export KEY=value", ("KEY", "value")),
    ("KEY = value", ("KEY", "value")),
    ('KEY="quoted # not a comment"', ("KEY", "quoted # not a comment")),
    ("KEY='single'", ("KEY", "single")),
    ("KEY=value # a comment", ("KEY", "value")),
    ("KEY=a#b", ("KEY", "a#b")),
    ("KEY=", ("KEY", "")),
    ("# comment", None),
    ("not an assignment", None),
    ("bad key=1", None),
])
def test_dotenv_lines(line, expected):
    from webcortex.cli import _dotenv_line

    assert _dotenv_line(line) == expected


def test_new_refuses_a_target_that_is_a_file(tmp_path):
    (tmp_path / "taken").write_text("x")
    out = _cli(["new", "demo", "-d", "taken"], tmp_path)
    assert out.returncode != 0 and "Traceback" not in out.stderr


@pytest.mark.parametrize("template", starters.TEMPLATES)
def test_every_starter_survives_a_name_and_description_with_quotes(template, tmp_path, monkeypatch):
    name = 'my "best" app\\'
    description = 'It\'s "great"\nand """triple""" quoted'
    files = starters.files_for(template, name, description)
    ast.parse(files["api.py"])  # a SyntaxError in a fresh project is the worst first impression
    for relative, contents in files.items():
        path = tmp_path / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)
    monkeypatch.chdir(tmp_path)
    for var in ("WEBCORTEX_API_KEY", "WEBCORTEX_VIEWER_KEY", "WEBCORTEX_DEVICE_KEY", "WEBCORTEX_READER_KEY"):
        monkeypatch.setenv(var, "test-key-abcdefghijklmnop")
    from webcortex.cli import _load_app

    app = _load_app("api.py")
    assert app.name == name and app.description == description
    app.check()
    if app.database:
        assert re.fullmatch(r"sqlite://\./[A-Za-z0-9_.-]+\.db", app.database), app.database


# ------------------------------------------------------------- typegen


def test_typescript_client_names_are_valid_and_unique():
    app = WebCortex("ts", database="sqlite://:memory:")
    app.resource("items", fields={"id": int, "name": str})

    @app.put("/things/{id}")
    def put_thing(id: int) -> dict:
        return {}

    @app.patch("/things/{id}")
    def patch_thing(id: int) -> dict:
        return {}

    @app.get("/v1.0/thing")
    def dotted() -> dict:
        """Ends the doc comment */ early."""
        return {}

    @app.get("/raw/{*rest}")
    def raw(rest: str) -> dict:
        return {}

    @app.get("/u", tool=True, tool_name="request")
    def shadow() -> dict:
        return {}

    @app.get("/tuple")
    def tup() -> tuple[int, str]:
        return (1, "a")

    @app.get("/union")
    def union(x: Union[int, str] = 1, c: Optional[Colour] = None) -> dict:
        return {}

    ts = app.typescript_client()
    methods = re.findall(r"^  (\w+)\((?:params: \w+(?: = \{\})?)?\): Promise<", ts, re.M)
    assert len(methods) == len(set(methods)), methods
    assert len(methods) == len([r for r in app.manifest()["routes"]]), (methods, ts)
    assert "request" not in methods and "requestRoute" in methods
    assert "updateThingsById" in methods and "updateThingsByIdPatch" in methods
    for line in ts.splitlines():
        if line.strip().startswith("/**"):
            assert line.strip().endswith("*/") and line.count("*/") == 1, line
    assert "Promise<[number, string]>" in ts
    assert "x?: number | string;" in ts
    assert 'c?: "red" | "blue" | null;' in ts
    assert "{*${k}}" in ts  # wildcard path parameters are substituted, not sent as a query
    assert "as Record<string, unknown>" not in ts  # TS2352 for interfaces with required keys
    assert "getUnion(params: GetUnionParams = {})" in ts


# ------------------------------------------------------------- over HTTP


APP_SOURCE = r'''
import asyncio, dataclasses, datetime, decimal, enum, itertools, math, threading
from webcortex import WebCortex, Response, HTTPError

app = WebCortex("stress", port={port}, request_timeout=60)

class Colour(enum.Enum):
    RED = "red"

@dataclasses.dataclass(slots=True)
class Point:
    x: float
    y: float

LOCK = threading.Lock()
COUNT = {{"n": 0}}

@app.get("/odd")
def odd() -> dict:
    return {{"nan": math.nan, "set": {{2, 1}}, "when": datetime.datetime(2024, 1, 2, 3, 4),
            "colour": Colour.RED, "point": Point(1.5, math.inf), "money": decimal.Decimal("9.99")}}

@app.post("/echo")
def echo(text: str, n: int = 0) -> dict:
    return {{"text": text, "n": n, "len": len(text)}}

@app.get("/greet/{{name}}")
def greet(name: str) -> dict:
    return {{"name": name}}

@app.get("/status")
def status() -> Response:
    return Response({{"x": 1}}, status=1000)

@app.get("/inject")
def inject() -> Response:
    return Response({{}}, headers={{"x-a": "1\r\nset-cookie: evil=1"}})

@app.get("/cookies")
def cookies() -> Response:
    return Response({{"ok": True}}, headers=[("set-cookie", "a=1"), ("set-cookie", "b=2"), ("x-n", 3)])

@app.get("/xml")
def xml() -> Response:
    return Response("<a>é</a>", content_type="application/xml")

@app.get("/exit")
async def exits() -> dict:
    raise SystemExit(2)

@app.get("/async")
async def asynch(i: int = 0) -> dict:
    await asyncio.sleep(0.001)
    with LOCK:
        COUNT["n"] += 1
    return {{"i": i}}

@app.get("/sync")
def sync(i: int = 0) -> dict:
    with LOCK:
        COUNT["n"] += 1
    return {{"i": i}}

@app.get("/count")
def count() -> dict:
    return dict(COUNT)

@app.get("/big")
def big(mb: int = 1) -> dict:
    return {{"blob": "x" * (mb * 1024 * 1024)}}

@app.get("/gen")
def gen(n: int = 3):
    for i in range(n):
        yield i

@app.get("/endless")
def endless():
    i = 0
    while True:
        yield i
        i += 1

@app.get("/aendless")
async def aendless():
    while True:
        yield "x" * 4096

@app.behaviour("endless_result")
def endless_result(ctx, input):
    return {{"rows": itertools.count()}}

class Callable:
    async def __call__(self, x: int = 1) -> dict:
        return {{"x": x}}

app.get("/callable")(Callable())

@app.behaviour("odd_result")
def odd_result(ctx, input):
    return {{"nan": math.nan, "set": {{3, 1}}, "day": datetime.date(2024, 1, 2)}}

@app.behaviour("async_result")
async def async_result(ctx, input):
    await asyncio.sleep(0)
    return {{"async": True}}

@app.behaviour("cyclic_result")
def cyclic_result(ctx, input):
    d = {{"a": 1}}
    d["self"] = d
    return d
'''


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Client:
    def __init__(self, base: str) -> None:
        self.base = base

    def call(self, path: str, method: str = "GET", data: bytes | None = None, timeout: float = 60):
        req = urllib.request.Request(self.base + path, data=data, method=method,
                                     headers={"content-type": "application/json"} if data is not None else {})
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return r.status, r.headers, r.read()
        except urllib.error.HTTPError as e:
            return e.code, e.headers, e.read()

    def json(self, path: str, method: str = "GET", payload=None):
        data = json.dumps(payload).encode() if payload is not None else None
        status, headers, raw = self.call(path, method, data)
        return status, (json.loads(raw) if raw else None)


@pytest.fixture(scope="module")
def stress():
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-stress-"))
    (workdir / "api.py").write_text(APP_SOURCE.format(port=port))
    log_path = workdir / "server.log"
    env = {**os.environ, "WEBCORTEX_LOG": "warn"}
    env.pop("WEBCORTEX_DEBUG_ERRORS", None)
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
                break
        except Exception:
            time.sleep(0.1)
    else:
        proc.kill()
        pytest.fail(f"server never became ready:\n{log_path.read_text()}")
    client = Client(base)
    client.proc = proc
    client.log = log_path
    yield client
    proc.terminate()
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()


def test_http_odd_values_are_valid_json(stress):
    status, headers, raw = stress.call("/odd")
    assert status == 200
    assert b"NaN" not in raw and b"Infinity" not in raw
    assert json.loads(raw) == {"nan": None, "set": [1, 2], "when": "2024-01-02T03:04:00", "colour": "red",
                               "point": {"x": 1.5, "y": None}, "money": "9.99"}


def test_http_broken_json_body_is_a_400(stress):
    for bad in (b"{bad", b"\xff", b'{"text": '):
        status, headers, raw = stress.call("/echo", "POST", bad)
        assert status == 400, raw
        assert json.loads(raw)["error"]["code"] == "WC205"


def test_http_unicode_round_trips(stress):
    text = "héllo wörld — 日本語 — مرحبا — 🚀\u200d"
    assert stress.json("/echo", "POST", {"text": text}) == (200, {"text": text, "n": 0, "len": len(text)})
    status, body = stress.json("/greet/" + urllib.request.quote("Zoë 🚀"))
    assert (status, body) == (200, {"name": "Zoë 🚀"})


def test_http_object_for_a_string_param_is_not_stringified(stress):
    status, body = stress.json("/echo", "POST", {"text": "x", "n": 3.7})
    assert status == 200 and body["n"] == 3.7  # not silently truncated to 3


def test_http_invalid_status_and_header_injection_fail_with_a_coded_json_500(stress):
    for path in ("/status", "/inject"):
        status, headers, raw = stress.call(path)
        assert status == 500
        err = json.loads(raw)["error"]  # JSON with a code, not bare text
        assert err["code"] == "WC002"
        assert "Traceback" not in err["message"]
        assert headers.get("set-cookie") is None


def test_http_multiple_set_cookie_headers(stress):
    status, headers, raw = stress.call("/cookies")
    assert status == 200
    assert headers.get_all("set-cookie") == ["a=1", "b=2"]
    assert headers.get("x-n") == "3"


def test_http_text_body_for_xml_is_sent_as_written(stress):
    status, headers, raw = stress.call("/xml")
    assert status == 200 and raw.decode() == "<a>é</a>"
    assert headers.get("content-type").startswith("application/xml")


def test_http_sys_exit_does_not_take_an_event_loop_down(stress):
    status, _, _ = stress.call("/exit")
    assert status == 500
    started = time.time()
    for i in range(20):
        status, body = stress.json(f"/async?i={i}")
        assert (status, body) == (200, {"i": i})
    # A dead loop answered every other request with a 504 after request_timeout.
    assert time.time() - started < 45


def test_http_generators_and_callable_objects(stress):
    assert stress.json("/gen?n=4") == (200, [0, 1, 2, 3])
    assert stress.json("/callable?x=5") == (200, {"x": 5})


def test_http_large_response(stress):
    status, headers, raw = stress.call("/big?mb=8", timeout=120)
    assert status == 200 and len(json.loads(raw)["blob"]) == 8 * 1024 * 1024


def test_http_concurrent_sync_and_async_handlers(stress):
    _, before = stress.json("/count")
    errors: list = []

    def worker(k: int) -> None:
        for j in range(15):
            i = k * 100 + j
            path = f"/{'async' if (i % 2) else 'sync'}?i={i}"
            try:
                status, body = stress.json(path)
                if (status, body) != (200, {"i": i}):
                    errors.append((path, status, body))
            except Exception as e:  # noqa: BLE001 - report every failure
                errors.append((path, repr(e)))

    threads = [threading.Thread(target=worker, args=(k,)) for k in range(24)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(180)
    assert not errors, errors[:5]
    _, after = stress.json("/count")
    assert after["n"] - before["n"] == 24 * 15


def test_http_behaviour_results_are_encoded_and_a_cycle_fails_cleanly(stress):
    status, body = stress.json("/behaviours/odd-result", "POST", {})
    assert (status, body) == (200, {"nan": None, "set": [1, 3], "day": "2024-01-02"})
    # An `async def` behaviour returned "<coroutine object …>" in 2.5.3.
    assert stress.json("/behaviours/async-result", "POST", {}) == (200, {"async": True})
    # 2.5.3: the cycle recursed in the Rust converter until the stack overflowed
    # and the whole server process died.
    status, body = stress.json("/behaviours/cyclic-result", "POST", {})
    assert status == 500 and body["error"]["code"] == "WC003"
    assert stress.proc.poll() is None, "the server process died"
    assert stress.json("/sync?i=1") == (200, {"i": 1})


def test_http_endless_generators_fail_with_wc406_and_the_server_lives(stress):
    # 2.6.0: the worker collected forever and the process's memory grew
    # until it was killed; the client got a 504 after request_timeout.
    for path in ("/endless", "/aendless"):
        started = time.time()
        status, body = stress.json(path)
        assert status == 500 and body["error"]["code"] == "WC406", (path, body)
        assert time.time() - started < 30
    status, body = stress.json("/behaviours/endless-result", "POST", {})
    assert status == 500 and body["error"]["code"] == "WC406", body
    assert stress.proc.poll() is None
    assert stress.json("/gen?n=4") == (200, [0, 1, 2, 3])
