"""Dispatch of requests from the Rust runtime onto Python handlers.

The Rust side hands us ``(handler_id, request, completer)`` and expects the
completer to be called exactly once, from any thread, whenever the handler is
done. How we get there is entirely Python's business.

Two pools, chosen by handler kind:

* **async handlers** go to one of N worker threads, each running its own
  asyncio event loop. On a free-threaded interpreter those loops execute
  genuinely in parallel; on a GIL build they still overlap on I/O, so the same
  code is correct either way — just slower.
* **sync handlers** go to a thread pool, so a blocking call can never stall an
  event loop that other requests are sharing.
"""

from __future__ import annotations

import asyncio
import base64
import dataclasses
import datetime as _dt
import decimal
import enum
import inspect
import itertools
import json
import math
import os
import re
import sys
import threading
import traceback
import uuid
from collections.abc import Iterable, Mapping
from concurrent.futures import ThreadPoolExecutor
from typing import Any, Callable, Sequence

__all__ = ["Dispatcher", "free_threaded", "default_worker_count"]

# Tracebacks go to the log always, and to the client only when explicitly
# enabled. A stack trace in an HTTP response hands an attacker your file layout,
# dependency versions, and code structure for free.
_EXPOSE_TRACEBACKS = os.environ.get("WEBCORTEX_DEBUG_ERRORS", "").lower() in {"1", "true", "yes"}


def free_threaded() -> bool:
    """True when running on a build with the GIL disabled."""
    # ``sys._is_gil_enabled`` exists only on 3.13+; its absence means a
    # conventional GIL build.
    is_enabled = getattr(sys, "_is_gil_enabled", None)
    return is_enabled is not None and not is_enabled()


def default_worker_count() -> int:
    if not free_threaded():
        # Extra event loops on a GIL build buy concurrency for I/O but cannot
        # buy parallelism, and each one costs a thread. Two is a reasonable
        # compromise that keeps a slow handler from blocking everything.
        return 2
    return max(2, min(32, (os.cpu_count() or 4)))


class _LoopWorker:
    """A thread owning one asyncio event loop."""

    def __init__(self, index: int) -> None:
        self.loop = asyncio.new_event_loop()
        self._ready = threading.Event()
        self._thread = threading.Thread(
            target=self._run, name=f"webcortex-loop-{index}", daemon=True
        )
        self._thread.start()
        self._ready.wait()

    def _run(self) -> None:
        asyncio.set_event_loop(self.loop)
        self.loop.call_soon(self._ready.set)
        self.loop.run_forever()

    def submit(self, coro_fn: Callable[[], Any], completer: Any) -> None:
        self.loop.call_soon_threadsafe(self._start, coro_fn, completer)

    def _start(self, coro_fn: Callable[[], Any], completer: Any) -> None:
        try:
            task = self.loop.create_task(_run_async(coro_fn))
        except Exception:
            _fail(completer, traceback.format_exc())
            return
        task.add_done_callback(lambda t: _settle_task(t, completer))

    def shutdown(self) -> None:
        self.loop.call_soon_threadsafe(self.loop.stop)


class Dispatcher:
    """Routes handler invocations onto the appropriate pool.

    Held by the Rust runtime for the life of the process; ``submit`` is called
    from tokio worker threads.
    """

    def __init__(self, handlers: Sequence[Callable[..., Any]], workers: int = 0) -> None:
        self._handlers = list(handlers)
        self._is_async = [inspect.iscoroutinefunction(h) for h in self._handlers]

        count = workers or default_worker_count()
        self._needs_loops = any(self._is_async)
        self._loops = [_LoopWorker(i) for i in range(count)] if self._needs_loops else []
        self._counter = itertools.count()

        self._threads = ThreadPoolExecutor(
            max_workers=count * 4,
            thread_name_prefix="webcortex-sync",
        )

    # Called from Rust.
    def submit(self, handler_id: int, request: dict, completer: Any) -> None:
        try:
            handler = self._handlers[handler_id]
        except IndexError:
            _fail(completer, f"no handler registered at index {handler_id}")
            return

        req = Request(request)

        if self._is_async[handler_id]:
            worker = self._loops[next(self._counter) % len(self._loops)]
            worker.submit(lambda: handler(req), completer)
        else:
            future = self._threads.submit(_run_sync, handler, req)
            future.add_done_callback(lambda f: _settle_future(f, completer))

    def submit_behaviour(self, handler_id: int, ctx: Any, payload: Any, completer: Any) -> None:
        """Run a Behaviour on a worker thread.

        Behaviours always run on the thread pool, never on an event loop:
        `ctx.call` and `ctx.ask` block while the Rust runtime does the work, and
        blocking an event loop that other requests share would stall them.
        """
        try:
            handler = self._handlers[handler_id]
        except IndexError:
            _fail(completer, f"no behaviour registered at index {handler_id}")
            return

        def run() -> Any:
            result = handler(ctx, payload)
            # `async def` behaviours: run to completion on this worker thread's
            # own loop (ctx.call blocks, so it cannot share a request loop).
            if inspect.isawaitable(result) or hasattr(result, "__aiter__"):
                result = asyncio.run(_drain(result))
            return result

        future = self._threads.submit(run)
        future.add_done_callback(lambda f: _settle_behaviour(f, completer))

    def shutdown(self) -> None:
        for w in self._loops:
            w.shutdown()
        self._threads.shutdown(wait=False)


class Request:
    """What a handler receives.

    Intentionally small. Anything that can be declared instead of computed
    belongs in the manifest, where Rust can act on it without waking Python.
    """

    __slots__ = ("_raw", "_json")

    def __init__(self, raw: dict) -> None:
        self._raw = raw
        self._json: Any = _UNSET

    @property
    def method(self) -> str:
        return self._raw["method"]

    @property
    def path(self) -> str:
        return self._raw["path"]

    @property
    def params(self) -> dict:
        return self._raw["path_params"]

    @property
    def query(self) -> dict:
        return self._raw["query"]

    @property
    def headers(self) -> dict:
        return self._raw["headers"]

    @property
    def body(self) -> bytes:
        return self._raw["body"]

    @property
    def scopes(self) -> list:
        return self._raw["scopes"]

    @property
    def user(self) -> dict:
        """The authenticated caller: ``{id, authenticated, scopes}``."""
        return self._raw.get("user", {"id": "anonymous", "authenticated": False, "scopes": []})


    def json(self) -> Any:
        """Parsed JSON body, cached. ``None`` for an empty body.

        A body that is not JSON raises :class:`InvalidJSON`, which is both an
        ``HTTPError(400)`` (so an unhandled one answers the caller with a 400
        rather than a 500: the client sent it) and a ``ValueError`` (so code
        that catches ``json.JSONDecodeError``'s base class keeps working).
        """
        if self._json is _UNSET:
            raw = self._raw["body"]
            try:
                self._json = json.loads(raw) if raw else None
            except ValueError as e:  # JSONDecodeError and UnicodeDecodeError
                raise InvalidJSON(f"request body is not valid JSON: {e}") from None
        return self._json

    def get(self, name: str, default: Any = None) -> Any:
        """Look a value up across path params, query string, then JSON body.

        Mirrors the Rust-side resolution order so a handler and a declarative
        route read their inputs the same way.
        """
        if name in self.params:
            return self.params[name]
        if name in self.query:
            return self.query[name]
        body = self.json()
        if isinstance(body, dict) and name in body:
            return body[name]
        return default

    def __repr__(self) -> str:
        return f"<Request {self.method} {self.path}>"


class _Unset:
    __slots__ = ()


_UNSET = _Unset()


class Response:
    """Return one of these from a handler for control over status and headers.

    Returning a plain dict, list, or ``None`` is the common case and does not
    require this type.

    ``headers`` is a mapping, or a list of ``(name, value)`` pairs when a
    name repeats (two ``set-cookie`` headers). A ``content-type`` header
    replaces ``content_type``. A ``str`` body for a type that is not JSON
    (HTML, XML, CSV …) is sent as written; anything else is encoded as JSON.
    ``status`` must be an int from 100 to 599.
    """

    __slots__ = ("body", "status", "headers", "content_type")

    def __init__(
        self,
        body: Any = None,
        status: int = 200,
        headers: dict | None = None,
        content_type: str = "application/json",
    ) -> None:
        self.body = body
        self.status = status
        self.headers = headers or {}
        self.content_type = content_type


class HTTPError(Exception):
    """Raise for an expected failure. Becomes a clean JSON error response.

    The body is ``{"error": {"status", "message", "code", "hint"}}``. ``code``
    defaults to the catalogued code for ``status`` (``webcortex explain
    --list``); pass your own ``code`` and ``hint`` to say more. A hint is
    what the caller should do next, written for a person or a model.
    """

    def __init__(self, status: int, message: str, *, code: str | None = None,
                 hint: str | None = None) -> None:
        super().__init__(message)
        self.status = status
        self.message = message
        self.code = code
        self.hint = hint


class InvalidJSON(HTTPError, ValueError):
    """The request body is not JSON: a 400 (``WC205``), and a ``ValueError``."""

    def __init__(self, message: str) -> None:
        super().__init__(400, message, code="WC205")


# RFC 9110 `token`: what a header name may contain.
_HEADER_NAME = re.compile(r"^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$")


def _check_status(status: Any) -> int:
    # bool is an int, and `Response(x, True)` is a mistake, not a 1.
    if isinstance(status, bool) or not isinstance(status, int) or not 100 <= status <= 599:
        raise ValueError(f"invalid HTTP status {status!r}: a response status is an int from 100 to 599")
    return int(status)


def _check_header_value(name: str, value: str) -> str:
    # CR/LF would split the header (response splitting); other control
    # characters are refused by the HTTP layer. Either way, say which header.
    if any((ord(c) < 32 and c != "\t") or ord(c) == 127 for c in value):
        raise ValueError(f"header {name!r} contains a control character (CR, LF, NUL …); refusing to send it")
    return value


def _header_pairs(headers: Any) -> list[tuple[str, str]]:
    """Normalise ``Response.headers``: a mapping, or a list of pairs (which is
    how one response carries two ``set-cookie`` headers)."""
    if not headers:
        return []
    items = headers.items() if isinstance(headers, Mapping) else headers
    out: list[tuple[str, str]] = []
    for item in items:
        try:
            name, value = item
        except (TypeError, ValueError):
            raise ValueError(f"response header {item!r} is not a (name, value) pair") from None
        if not isinstance(name, str) or not _HEADER_NAME.match(name):
            raise ValueError(f"invalid response header name {name!r}")
        if isinstance(value, bool) or not isinstance(value, (str, int, float)):
            raise TypeError(f"response header {name!r} must be a string, not {type(value).__name__}")
        out.append((name, _check_header_value(name, str(value))))
    return out


def _is_json_type(content_type: str) -> bool:
    media = content_type.split(";", 1)[0].strip().lower()
    return media == "application/json" or media.endswith("+json")


def _encode(value: Any) -> tuple[int, bytes, str, list]:
    if isinstance(value, Response):
        payload = value.body
        content_type = value.content_type
        status = value.status
        headers = _header_pairs(value.headers)
    else:
        payload, content_type, status, headers = value, "application/json", 200, []

    status = _check_status(status)
    # A content type passed as a header *is* the content type; sending it as
    # well would put two different content-type headers on one response.
    for name, v in headers:
        if name.lower() == "content-type":
            content_type = v
    headers = [(k, v) for k, v in headers if k.lower() != "content-type"]
    if not isinstance(content_type, str):
        raise TypeError(f"content_type must be a string, not {type(content_type).__name__}")
    _check_header_value("content-type", content_type)

    if payload is None:
        return status if status != 200 else 204, b"", content_type, headers
    if isinstance(payload, (bytes, bytearray, memoryview)):
        return status, bytes(payload), content_type, headers
    if isinstance(payload, str) and not _is_json_type(content_type):
        # Text for a non-JSON type (HTML, XML, CSV, SVG …) is sent as written.
        return status, payload.encode("utf-8"), content_type, headers
    return status, _dumps(payload), content_type, headers


def _dumps(payload: Any) -> bytes:
    """JSON that every client can parse.

    The fast path is the stdlib encoder. It is redone through
    :func:`jsonable` only when the payload holds something strict JSON has no
    spelling for — NaN or an infinity (which the stdlib would write as the
    bare, invalid token ``NaN``) or a dict key that is not a string.
    """
    try:
        return json.dumps(payload, default=_fallback, allow_nan=False).encode("utf-8")
    except (ValueError, TypeError) as e:
        text = str(e)
        if "Out of range float" not in text and "keys must be" not in text:
            raise
    return json.dumps(jsonable(payload), allow_nan=False).encode("utf-8")


def _fallback(obj: Any) -> Any:
    """Last-resort JSON encoding for types the stdlib encoder rejects.

    The rules, in order: an object's own hook (``__webcortex_json__``,
    pydantic's ``model_dump``, ``_asdict``, numpy's ``tolist``); enum members
    as their value; dataclasses as their fields; dates and times as ISO 8601
    (what the ``date-time`` schema format promises); ``timedelta`` as seconds;
    ``Decimal`` and ``UUID`` as strings; bytes as base64 (what the ``bytes``
    schema promises); sets, generators and other iterables as arrays; any
    other mapping as an object; then an object's public attributes; then
    ``str``.
    """
    # `webcortex.Image`, and anything else that knows its own wire form.
    hook = getattr(obj, "__webcortex_json__", None)
    if callable(hook):
        return hook()
    if isinstance(obj, type) or inspect.isroutine(obj) or inspect.ismodule(obj):
        # A class has `model_dump` and `__dict__` too, unbound; neither is
        # what the caller meant, and `vars(cls)` would publish its internals.
        return getattr(obj, "__qualname__", None) or str(obj)
    if isinstance(obj, enum.Enum):
        return obj.value
    for attr in ("model_dump", "dict", "_asdict", "tolist"):
        method = getattr(obj, attr, None)
        if callable(method):
            return method()
    if dataclasses.is_dataclass(obj):
        # Field by field, so slotted dataclasses (no `__dict__`) work, and
        # nested values go back through the encoder.
        return {f.name: getattr(obj, f.name) for f in dataclasses.fields(obj)}
    if isinstance(obj, (_dt.datetime, _dt.date, _dt.time)):
        return obj.isoformat()
    if isinstance(obj, _dt.timedelta):
        return obj.total_seconds()
    if isinstance(obj, (decimal.Decimal, uuid.UUID)):
        return str(obj)
    if isinstance(obj, (bytes, bytearray, memoryview)):
        return base64.b64encode(bytes(obj)).decode("ascii")
    if isinstance(obj, (set, frozenset)):
        try:
            return sorted(obj)
        except TypeError:
            return list(obj)
    if isinstance(obj, Mapping):
        return dict(obj)
    if isinstance(obj, Iterable) and not isinstance(obj, (str, bytes)):
        return list(obj)
    if hasattr(obj, "__dict__"):
        return {k: v for k, v in vars(obj).items() if not k.startswith("_")}
    return str(obj)


def jsonable(value: Any) -> Any:
    """``value`` as plain JSON data: dicts with string keys, lists, strings,
    finite numbers, booleans and ``None``.

    Applies the :func:`_fallback` rules everywhere, maps NaN and the
    infinities to ``None`` (as JavaScript's ``JSON.stringify`` does), and
    refuses a reference cycle with ``ValueError`` rather than recursing until
    the stack runs out. Behaviour results go through this before they cross
    into Rust.
    """
    return _jsonable(value, set())


def _jsonable(value: Any, active: set[int]) -> Any:
    if value is None or isinstance(value, (str, bool)):
        return value
    if isinstance(value, int):
        return int(value)
    if isinstance(value, float):
        return float(value) if math.isfinite(value) else None
    key = id(value)
    if key in active:
        raise ValueError("circular reference detected")
    active.add(key)
    try:
        if isinstance(value, dict):
            return {_json_key(k): _jsonable(v, active) for k, v in value.items()}
        if isinstance(value, (list, tuple)):
            return [_jsonable(v, active) for v in value]
        converted = _fallback(value)
        if converted is value:
            return str(value)
        return _jsonable(converted, active)
    finally:
        active.discard(key)


def _json_key(key: Any) -> str:
    if isinstance(key, str):
        return key
    if key is None or isinstance(key, bool):
        return json.dumps(key)
    if isinstance(key, float) and not math.isfinite(key):
        return str(key)
    if isinstance(key, (int, float)):
        return json.dumps(key)
    plain = _jsonable(key, set())
    return plain if isinstance(plain, str) else json.dumps(plain)


class _HandlerExit(Exception):
    """A handler called ``sys.exit()`` or raised ``KeyboardInterrupt``.

    Re-raised as an ordinary exception so it fails that one request. Left as
    it is, it would escape the event loop and kill the worker thread that
    every later async request on that loop is queued on.
    """


async def _drain(value: Any) -> Any:
    """Await an awaitable, and collect an async iterator into a list."""
    if inspect.isawaitable(value):
        value = await value
    if hasattr(value, "__aiter__") and hasattr(value, "__anext__"):
        value = [item async for item in value]
    return value


async def _run_async(coro_fn: Callable[[], Any]) -> Any:
    try:
        return await _drain(coro_fn())
    except (SystemExit, KeyboardInterrupt) as e:
        raise _HandlerExit(f"handler raised {type(e).__name__}({e})") from e


def _run_sync(handler: Callable[[Any], Any], req: Any) -> Any:
    try:
        result = handler(req)
    except (SystemExit, KeyboardInterrupt) as e:
        raise _HandlerExit(f"handler raised {type(e).__name__}({e})") from e
    # A sync callable can still hand back a coroutine — a decorator that
    # forgot to be async, a callable object with `async def __call__`. Run it
    # to completion here rather than serialising "<coroutine object …>".
    if inspect.isawaitable(result) or hasattr(result, "__aiter__"):
        result = asyncio.run(_drain(result))
    return result


def _settle_task(task: asyncio.Task, completer: Any) -> None:
    if task.cancelled():
        _fail(completer, "handler task was cancelled")
        return
    exc = task.exception()
    if exc is not None:
        _deliver_exception(exc, completer)
        return
    _deliver(task.result(), completer)


def _settle_future(future: Any, completer: Any) -> None:
    exc = future.exception()
    if exc is not None:
        _deliver_exception(exc, completer)
        return
    _deliver(future.result(), completer)


def _deliver(value: Any, completer: Any) -> None:
    try:
        status, body, content_type, headers = _encode(value)
    except Exception as e:
        # Logged in full; the client sees the traceback only when tracebacks
        # are explicitly exposed, as for an exception the handler raised.
        detail = traceback.format_exc()
        print(f"[webcortex] handler returned a value that cannot be sent:\n{detail}",
              file=sys.stderr, flush=True)
        _fail(completer, f"handler returned an unserialisable value:\n{detail}" if _EXPOSE_TRACEBACKS
              else f"handler returned a value that cannot be sent ({type(e).__name__})")
        return
    try:
        completer.complete(status, body, content_type, headers)
    except Exception:
        # The runtime refused the response (it re-checks status and headers).
        # Fail the request with the reason instead of leaving it to time out.
        detail = traceback.format_exc()
        print(f"[webcortex] response refused:\n{detail}", file=sys.stderr, flush=True)
        _fail(completer, detail if _EXPOSE_TRACEBACKS else "handler returned a response the server could not send")


def _deliver_exception(exc: BaseException, completer: Any) -> None:
    """Turn a handler exception into a response without leaking internals."""
    if isinstance(exc, HTTPError):
        # Deliberately raised by the application: the message is intended for
        # the caller, so it is safe to pass through.
        from .errors import error_body

        try:
            status = _check_status(exc.status if not isinstance(exc.status, str) else int(exc.status))
        except (TypeError, ValueError):
            print(f"[webcortex] HTTPError with invalid status {exc.status!r}; answering 500",
                  file=sys.stderr, flush=True)
            status = 500
        message = exc.message if isinstance(exc.message, str) else str(exc.message)
        body = error_body(status, message, exc.code, exc.hint)
        _deliver(Response(body, status, headers={"x-webcortex-error": body["error"]["code"]}),
                 completer)
        return

    detail = "".join(traceback.format_exception(type(exc), exc, exc.__traceback__))
    # Always logged in full, so nothing is lost by not returning it.
    print(f"[webcortex] unhandled handler exception:\n{detail}", file=sys.stderr, flush=True)
    _fail(completer, detail if _EXPOSE_TRACEBACKS else "internal error")


def _settle_behaviour(future: Any, completer: Any) -> None:
    """Deliver a behaviour's return value, or its failure."""
    exc = future.exception()
    if exc is not None:
        # A halt is the runtime stopping the behaviour on purpose (budget
        # exhausted, approval required). It is reported as a structured result
        # rather than a crash, because the caller usually wants to act on it.
        if type(exc).__name__ == "BehaviourHalted":
            try:
                completer.complete({"halted": True, "reason": str(exc)})
                return
            except Exception:
                traceback.print_exc()
                return
        detail = "".join(traceback.format_exception(type(exc), exc, exc.__traceback__))
        print(f"[webcortex] behaviour failed:\n{detail}", file=sys.stderr, flush=True)
        _fail(completer, detail if _EXPOSE_TRACEBACKS else "behaviour failed")
        return
    try:
        completer.complete(jsonable(future.result()))
    except Exception:
        detail = traceback.format_exc()
        print(f"[webcortex] behaviour returned an unconvertible value:\n{detail}",
              file=sys.stderr, flush=True)
        _fail(completer, detail if _EXPOSE_TRACEBACKS else
              "behaviour returned a value that could not be serialised")


def _fail(completer: Any, message: str) -> None:
    try:
        completer.fail(message)
    except Exception:  # pragma: no cover
        traceback.print_exc()
