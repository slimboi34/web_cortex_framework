"""The Pylon application object.

A Pylon app is a *description*, not a server. Importing your `api.py` builds a
manifest; the Rust runtime then executes it. That separation is what lets whole
categories of route — queries, proxies, static responses — run without the
interpreter being involved in the request path at all.

The design rule throughout: if something can be declared, declare it, because
declared things are things Rust can execute and agents can read.
"""

from __future__ import annotations

import inspect
import json
import os
from dataclasses import dataclass, field
from typing import Any, Callable, Iterable, Sequence

from . import schema as _schema
from ._bridge import Dispatcher, HTTPError, Request, Response, default_worker_count

__all__ = ["Pylon", "Request", "Response", "HTTPError", "Resource"]

_HTTP_METHODS = ("GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS")

# Parameter names that mean "give me the whole request" rather than "bind this
# by name". Annotating the parameter `Request` works too.
_REQUEST_PARAM_NAMES = {"req", "request"}


@dataclass
class Resource:
    """A table exposed as a set of CRUD endpoints, executed entirely in Rust."""

    name: str
    table: str
    fields: dict[str, type]
    primary_key: str = "id"
    tools: bool = False
    route_ids: list[int] = field(default_factory=list)


class Pylon:
    def __init__(
        self,
        name: str,
        *,
        description: str = "",
        version: str = "0.1.0",
        database: str | None = None,
        host: str = "127.0.0.1",
        port: int = 8000,
        workers: int | None = None,
        control_prefix: str = "/_pylon",
    ) -> None:
        self.name = name
        self.description = description
        self.version = version
        self.database = database
        self.host = host
        self.port = port
        self.workers = workers
        self.control_prefix = control_prefix

        self._routes: list[dict] = []
        self._handlers: list[Callable] = []
        self._upstreams: dict[str, dict] = {}
        self._agents: list[dict] = []
        self._resources: list[Resource] = []
        self._schema_sql: list[str] = []
        self._next_id = 0

        # Always present, so a deployment can be probed before any user route
        # exists and an agent can discover the app's shape.
        self.static("GET", "/", {"app": name, "version": version, "docs": f"{control_prefix}/openapi.json"})

    # ------------------------------------------------------------------
    # Route registration
    # ------------------------------------------------------------------

    def _alloc_id(self) -> int:
        rid = self._next_id
        self._next_id += 1
        return rid

    def _add_route(
        self,
        method: str,
        path: str,
        op: dict,
        *,
        summary: str = "",
        description: str = "",
        input_schema: dict | None = None,
        output_schema: dict | None = None,
        tool: bool = False,
        tool_name: str | None = None,
        read_only: bool | None = None,
        idempotent: bool | None = None,
        scopes: Sequence[str] = (),
    ) -> int:
        method = method.upper()
        if method not in _HTTP_METHODS:
            raise ValueError(f"unsupported HTTP method {method!r}")
        if not path.startswith("/"):
            raise ValueError(f"route path must start with '/': {path!r}")

        if read_only is None:
            read_only = method in ("GET", "HEAD", "OPTIONS")
        if idempotent is None:
            idempotent = method in ("GET", "HEAD", "OPTIONS", "PUT", "DELETE")

        rid = self._alloc_id()
        self._routes.append(
            {
                "id": rid,
                "method": method,
                "path": path,
                "op": op,
                "summary": summary,
                "description": description,
                "input_schema": input_schema,
                "output_schema": output_schema or None,
                "tool": {
                    "expose": bool(tool),
                    "name": tool_name,
                    "read_only": read_only,
                    "idempotent": idempotent,
                    "scopes": list(scopes),
                },
            }
        )
        return rid

    def route(
        self,
        method: str,
        path: str,
        *,
        tool: bool = False,
        tool_name: str | None = None,
        read_only: bool | None = None,
        idempotent: bool | None = None,
        scopes: Sequence[str] = (),
        summary: str = "",
    ) -> Callable:
        """Register a Python handler.

        The handler may take a `Request`, or it may declare its inputs as typed
        parameters and let the framework bind them by name:

            @app.get("/books/{id}")
            def get_book(id: int) -> Book: ...

        The second form is preferred, because the signature then doubles as the
        tool schema an agent sees.
        """

        def decorator(fn: Callable) -> Callable:
            path_params = _path_params(path)
            wants_request = _request_param(fn)
            skip = {wants_request} if wants_request else set()
            input_schema, output_schema = _schema.schema_from_signature(fn, path_params, skip)

            handler_index = len(self._handlers)
            self._handlers.append(_bind_handler(fn, wants_request))

            doc = inspect.getdoc(fn) or ""
            self._add_route(
                method,
                path,
                {"kind": "python", "handler": handler_index},
                summary=summary or doc.split("\n", 1)[0],
                description=doc,
                input_schema=input_schema,
                output_schema=output_schema,
                tool=tool,
                tool_name=tool_name,
                read_only=read_only,
                idempotent=idempotent,
                scopes=scopes,
            )
            return fn

        return decorator

    def get(self, path: str, **kw: Any) -> Callable:
        return self.route("GET", path, **kw)

    def post(self, path: str, **kw: Any) -> Callable:
        return self.route("POST", path, **kw)

    def put(self, path: str, **kw: Any) -> Callable:
        return self.route("PUT", path, **kw)

    def patch(self, path: str, **kw: Any) -> Callable:
        return self.route("PATCH", path, **kw)

    def delete(self, path: str, **kw: Any) -> Callable:
        return self.route("DELETE", path, **kw)

    # ------------------------------------------------------------------
    # Declarative routes — these never enter the interpreter at request time
    # ------------------------------------------------------------------

    def static(self, method: str, path: str, body: Any, *, status: int = 200, **kw: Any) -> int:
        """A constant response, serialised once at boot."""
        return self._add_route(method, path, {"kind": "static", "status": status, "body": body}, **kw)

    def query(
        self,
        method: str,
        path: str,
        sql: str,
        *,
        params: Sequence[str] = (),
        returns: str = "many",
        **kw: Any,
    ) -> int:
        """Bind a route directly to SQL.

        `params` names the values bound to each `?` in order; each is resolved
        from the path, then the query string, then the JSON body. The request
        never reaches Python, so this is the fastest kind of route Pylon has.
        """
        if returns not in ("many", "one", "affected"):
            raise ValueError("returns must be 'many', 'one', or 'affected'")
        if not self.database:
            raise ValueError(
                f"query route {method} {path} needs a database; "
                "pass database=... to Pylon()"
            )
        return self._add_route(
            method,
            path,
            {"kind": "query", "sql": sql, "params": list(params), "returns": returns},
            **kw,
        )

    def upstream(
        self,
        name: str,
        base_url: str,
        *,
        headers: dict[str, str] | None = None,
        bearer_env: str | None = None,
        timeout_ms: int = 30_000,
    ) -> None:
        """Declare an external API this app may call or proxy to.

        Credentials are named, not embedded: `bearer_env` is resolved from the
        process environment at boot, so the manifest stays safe to log, diff,
        and hand to an agent.
        """
        self._upstreams[name] = {
            "base_url": base_url,
            "headers": headers or {},
            "bearer_env": bearer_env,
            "timeout_ms": timeout_ms,
        }

    def proxy(self, method: str, path: str, *, upstream: str, rewrite: str | None = None, **kw: Any) -> int:
        """Forward a route to a declared upstream. The gateway primitive."""
        return self._add_route(
            method, path, {"kind": "proxy", "upstream": upstream, "rewrite": rewrite}, **kw
        )

    def resource(
        self,
        name: str,
        *,
        table: str | None = None,
        fields: dict[str, type],
        primary_key: str = "id",
        tools: bool = False,
        scopes: Sequence[str] = (),
        create_table: bool = True,
    ) -> Resource:
        """Generate a full CRUD surface for a table.

        This is the Django-admin-scale shortcut, except the resulting endpoints
        are executed by Rust rather than by an ORM. Five routes, zero
        interpreter involvement, and — with `tools=True` — five agent tools.
        """
        table = table or name
        if not self.database:
            raise ValueError(f"resource {name!r} needs a database; pass database=... to Pylon()")
        if primary_key not in fields:
            raise ValueError(f"primary key {primary_key!r} is not among fields for {name!r}")

        res = Resource(name=name, table=table, fields=dict(fields), primary_key=primary_key, tools=tools)
        writable = [f for f in fields if f != primary_key]
        cols = ", ".join(writable)
        placeholders = ", ".join("?" for _ in writable)
        assignments = ", ".join(f"{c} = ?" for c in writable)
        props = {f: _schema.json_schema_for(t) for f, t in fields.items()}

        def body_schema(include_pk: bool, required: Iterable[str]) -> dict:
            selected = {k: v for k, v in props.items() if include_pk or k != primary_key}
            return {
                "type": "object",
                "properties": selected,
                "required": list(required),
                "additionalProperties": False,
            }

        list_schema = {
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "default": 50},
                "offset": {"type": "integer", "default": 0},
            },
            "additionalProperties": False,
        }
        pk_schema = {
            "type": "object",
            "properties": {primary_key: props[primary_key]},
            "required": [primary_key],
            "additionalProperties": False,
        }

        common = {"tool": tools, "scopes": scopes}

        res.route_ids.append(
            self.query(
                "GET", f"/{name}",
                f"SELECT * FROM {table} LIMIT COALESCE(?, 50) OFFSET COALESCE(?, 0)",
                params=["limit", "offset"], returns="many",
                summary=f"List {name}",
                description=f"Return a page of {name} rows, newest first by insertion order.",
                input_schema=list_schema,
                output_schema={"type": "array", "items": body_schema(True, [])},
                tool_name=f"list_{name}", **common,
            )
        )
        res.route_ids.append(
            self.query(
                "GET", f"/{name}/{{{primary_key}}}",
                f"SELECT * FROM {table} WHERE {primary_key} = ?",
                params=[primary_key], returns="one",
                summary=f"Get one {name} row by {primary_key}",
                description=f"Fetch a single {name} row. Responds 404 when no row matches.",
                input_schema=pk_schema,
                output_schema=body_schema(True, []),
                tool_name=f"get_{name}", **common,
            )
        )
        res.route_ids.append(
            self.query(
                "POST", f"/{name}",
                f"INSERT INTO {table} ({cols}) VALUES ({placeholders}) RETURNING *",
                params=writable, returns="one",
                summary=f"Create a {name} row",
                description=f"Insert a new {name} row and return it, including its generated {primary_key}.",
                input_schema=body_schema(False, writable),
                output_schema=body_schema(True, []),
                tool_name=f"create_{name}", **common,
            )
        )
        res.route_ids.append(
            self.query(
                "PUT", f"/{name}/{{{primary_key}}}",
                f"UPDATE {table} SET {assignments} WHERE {primary_key} = ? RETURNING *",
                params=[*writable, primary_key], returns="one",
                summary=f"Replace a {name} row",
                description=f"Overwrite every writable field of a {name} row. Responds 404 when no row matches.",
                input_schema=body_schema(True, [primary_key, *writable]),
                output_schema=body_schema(True, []),
                tool_name=f"update_{name}", **common,
            )
        )
        res.route_ids.append(
            self.query(
                "DELETE", f"/{name}/{{{primary_key}}}",
                f"DELETE FROM {table} WHERE {primary_key} = ?",
                params=[primary_key], returns="affected",
                summary=f"Delete a {name} row",
                description=f"Delete a {name} row by {primary_key}. Reports how many rows were removed.",
                input_schema=pk_schema,
                output_schema={"type": "object", "properties": {"affected": {"type": "integer"}}},
                tool_name=f"delete_{name}", **common,
            )
        )

        if create_table:
            self._schema_sql.append(_create_table_sql(table, fields, primary_key))

        self._resources.append(res)
        return res

    # ------------------------------------------------------------------
    # Agents
    # ------------------------------------------------------------------

    def agent(
        self,
        name: str,
        *,
        model: str,
        system: str = "",
        tools: Sequence[str] = (),
        description: str = "",
        max_steps: int | None = 12,
        token_budget: int | None = None,
        expose_at: str | None = None,
    ) -> None:
        """Declare an agent that lives inside the application.

        `tools` names routes exposed with `tool=True`. Because the agent calls
        them through the same dispatcher the HTTP server uses, a tool call is an
        in-process function call — not a loopback request — and it inherits the
        route's declared scopes.

        A typo in `tools` is a boot error, not a runtime surprise.
        """
        self._agents.append(
            {
                "name": name,
                "description": description,
                "model": model,
                "system": system,
                "tools": list(tools),
                "max_steps": max_steps,
                "token_budget": token_budget,
            }
        )
        if expose_at:
            self._add_route(
                "POST", expose_at,
                {"kind": "agent", "agent": name, "stream": False},
                summary=f"Invoke the {name} agent",
                description=description,
                input_schema={
                    "type": "object",
                    "properties": {"input": {"type": "string"}},
                    "required": ["input"],
                },
            )

    # ------------------------------------------------------------------
    # Manifest + run
    # ------------------------------------------------------------------

    def manifest(self) -> dict:
        return {
            "name": self.name,
            "version": self.version,
            "description": self.description,
            "server": {
                "host": os.environ.get("PYLON_HOST", self.host),
                "port": int(os.environ.get("PYLON_PORT", self.port)),
                "python_workers": self.workers,
                "control_prefix": self.control_prefix,
            },
            "database": (
                {"url": os.environ.get("PYLON_DATABASE_URL", self.database), "max_connections": 16}
                if self.database
                else None
            ),
            "routes": self._routes,
            "upstreams": self._upstreams,
            "agents": self._agents,
        }

    def manifest_json(self) -> str:
        return json.dumps(self.manifest())

    @property
    def schema_sql(self) -> str:
        """DDL for every `create_table=True` resource."""
        return "\n".join(self._schema_sql)

    def check(self) -> dict:
        """Validate the app through the Rust runtime without binding a port."""
        from . import _core

        return json.loads(_core.inspect_manifest(self.manifest_json()))

    def openapi(self) -> dict:
        from . import _core

        return json.loads(_core.openapi_for(self.manifest_json()))

    def run(self) -> None:
        """Boot the runtime and serve. Blocks."""
        from . import _core

        self._apply_schema()
        workers = self.workers or default_worker_count()
        dispatcher = Dispatcher(self._handlers, workers)
        try:
            _core.serve(self.manifest_json(), dispatcher, workers)
        except KeyboardInterrupt:
            pass
        finally:
            dispatcher.shutdown()

    def _apply_schema(self) -> None:
        """Create tables for declared resources.

        Deliberately limited to `CREATE TABLE IF NOT EXISTS`. Real schema
        evolution needs versioned, reviewable migrations; silently altering a
        production table because a Python literal changed is exactly the failure
        mode this framework should not ship.
        """
        if not self._schema_sql or not self.database:
            return
        import sqlite3

        url = os.environ.get("PYLON_DATABASE_URL", self.database)
        if not url.startswith("sqlite"):
            return
        filename = url.split("://", 1)[-1] if "://" in url else url.split(":", 1)[-1]
        con = sqlite3.connect(filename)
        try:
            con.executescript(self.schema_sql)
            con.commit()
        finally:
            con.close()


# ----------------------------------------------------------------------
# Handler binding
# ----------------------------------------------------------------------


def _path_params(path: str) -> list[str]:
    return [seg[1:-1] for seg in path.split("/") if seg.startswith("{") and seg.endswith("}")]


def _request_param(fn: Callable) -> str | None:
    """Name of the parameter that should receive the Request, if any."""
    try:
        hints = fn.__annotations__
    except AttributeError:
        hints = {}
    for name, param in inspect.signature(fn).parameters.items():
        if hints.get(name) is Request or name in _REQUEST_PARAM_NAMES:
            return name
        if param.annotation is Request:
            return name
    return None


def _bind_handler(fn: Callable, request_param: str | None) -> Callable:
    """Wrap a user handler in the ``(Request) -> Any`` shape Rust dispatches to.

    Async-ness is preserved rather than papered over, because the dispatcher
    routes coroutine handlers and blocking handlers to different pools.
    """
    sig = inspect.signature(fn)
    try:
        hints = __import__("typing").get_type_hints(fn)
    except Exception:
        hints = getattr(fn, "__annotations__", {})

    binder = _make_binder(sig, hints, request_param)

    if inspect.iscoroutinefunction(fn):

        async def async_handler(req: Request) -> Any:
            return await fn(**binder(req))

        async_handler.__name__ = getattr(fn, "__name__", "handler")
        return async_handler

    def sync_handler(req: Request) -> Any:
        return fn(**binder(req))

    sync_handler.__name__ = getattr(fn, "__name__", "handler")
    return sync_handler


def _make_binder(
    sig: inspect.Signature, hints: dict, request_param: str | None
) -> Callable[[Request], dict]:
    params = [
        (name, hints.get(name, p.annotation), p.default)
        for name, p in sig.parameters.items()
        if p.kind not in (inspect.Parameter.VAR_POSITIONAL, inspect.Parameter.VAR_KEYWORD)
    ]

    def bind(req: Request) -> dict:
        kwargs: dict[str, Any] = {}
        for name, annotation, default in params:
            if name == request_param:
                kwargs[name] = req
                continue
            raw = req.get(name, _MISSING)
            if raw is _MISSING:
                if default is inspect.Parameter.empty:
                    raise HTTPError(422, f"missing required parameter {name!r}")
                kwargs[name] = default
                continue
            kwargs[name] = _schema.coerce(raw, annotation)
        return kwargs

    return bind


class _Missing:
    __slots__ = ()


_MISSING = _Missing()


_SQL_TYPES = {
    int: "INTEGER",
    float: "REAL",
    str: "TEXT",
    bool: "INTEGER",
    bytes: "BLOB",
}


def _create_table_sql(table: str, fields: dict[str, type], primary_key: str) -> str:
    cols = []
    for name, typ in fields.items():
        sql_type = _SQL_TYPES.get(typ, "TEXT")
        if name == primary_key:
            cols.append(
                f"  {name} {sql_type} PRIMARY KEY"
                + (" AUTOINCREMENT" if sql_type == "INTEGER" else "")
            )
        else:
            cols.append(f"  {name} {sql_type}")
    body = ",\n".join(cols)
    return f"CREATE TABLE IF NOT EXISTS {table} (\n{body}\n);"
