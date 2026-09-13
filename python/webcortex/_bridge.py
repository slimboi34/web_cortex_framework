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
import inspect
import itertools
import json
import os
import sys
import threading
import traceback
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
            task = self.loop.create_task(coro_fn())
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
        self.worker_count = count

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
            future = self._threads.submit(handler, req)
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
            return handler(ctx, payload)

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
        """Parsed JSON body, cached. ``None`` for an empty body."""
        if self._json is _UNSET:
            raw = self._raw["body"]
            self._json = json.loads(raw) if raw else None
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
    """Raise for an expected failure. Becomes a clean JSON error response."""

    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status
        self.message = message


def _encode(value: Any) -> tuple[int, bytes, str, list]:
    if isinstance(value, Response):
        payload = value.body
        content_type = value.content_type
        status = value.status
        headers = list(value.headers.items())
    else:
        payload, content_type, status, headers = value, "application/json", 200, []

    if payload is None:
        return status if status != 200 else 204, b"", content_type, headers
    if isinstance(payload, bytes):
        return status, payload, content_type, headers
    if isinstance(payload, str) and content_type.startswith("text/"):
        return status, payload.encode("utf-8"), content_type, headers
    return status, json.dumps(payload, default=_fallback).encode("utf-8"), content_type, headers


def _fallback(obj: Any) -> Any:
    """Last-resort JSON encoding for types the stdlib encoder rejects."""
    for attr in ("model_dump", "dict", "_asdict"):
        method = getattr(obj, attr, None)
        if callable(method):
            return method()
    if hasattr(obj, "__dict__"):
        return vars(obj)
    return str(obj)


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
    except Exception:
        _fail(completer, f"handler returned an unserialisable value:\n{traceback.format_exc()}")
        return
    try:
        completer.complete(status, body, content_type, headers)
    except Exception:  # pragma: no cover - the client is already gone
        traceback.print_exc()


def _deliver_exception(exc: BaseException, completer: Any) -> None:
    """Turn a handler exception into a response without leaking internals."""
    if isinstance(exc, HTTPError):
        # Deliberately raised by the application: the message is intended for
        # the caller, so it is safe to pass through.
        _deliver(Response({"error": {"status": exc.status, "message": exc.message}}, exc.status), completer)
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
        completer.complete(future.result())
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
