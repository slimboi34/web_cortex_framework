"""The WebCortex application object.

A WebCortex app is a *description*, not a server. Importing your `api.py` builds a
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

__all__ = ["WebCortex", "Request", "Response", "HTTPError", "Resource"]

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


class WebCortex:
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
        control_prefix: str = "/_webcortex",
        templates: str | None = None,
        request_timeout: int = 30,
        shutdown_timeout: int = 25,
    ) -> None:
        self.name = name
        self.description = description
        self.version = version
        self.database = database
        self.host = host
        self.port = port
        self.workers = workers
        self.control_prefix = control_prefix
        self.templates_dir = templates
        self.request_timeout = request_timeout
        self.shutdown_timeout = shutdown_timeout

        # Security posture. Every one of these defaults to the safe setting;
        # relaxing it is what costs a line of code, not tightening it.
        self._auth: dict = {
            "api_keys": {},
            "api_key_header": "x-api-key",
            "jwt": None,
            "anonymous_scopes": [],
        }
        self._cors: dict | None = None
        self._rate_limit: dict | None = None
        self._security_headers: dict = {"enabled": True}

        self._routes: list[dict] = []
        self._handlers: list[Callable] = []
        self._upstreams: dict[str, dict] = {}
        self._agents: list[dict] = []
        self._behaviours: list[dict] = []
        self._resources: list[Resource] = []
        self._schema_sql: list[str] = []
        self._templates_used: set[str] = set()
        self._next_id = 0

        # A default root route is added at manifest time *only* if the app did
        # not define its own. Registering it here instead would make `/` the one
        # path a user could never claim.

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
        approval: str = "never",
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
                    "scopes": [],
                },
                "scopes": list(scopes),
                "approval": approval,
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
        approval: str = "never",
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
                approval=approval,
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
        never reaches Python, so this is the fastest kind of route WebCortex has.
        """
        if returns not in ("many", "one", "affected"):
            raise ValueError("returns must be 'many', 'one', or 'affected'")
        if not self.database:
            raise ValueError(
                f"query route {method} {path} needs a database; "
                "pass database=... to WebCortex()"
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
        read_scopes: Sequence[str] | None = None,
        write_scopes: Sequence[str] | None = None,
        create_table: bool = True,
    ) -> Resource:
        """Generate a full CRUD surface for a table.

        This is the Django-admin-scale shortcut, except the resulting endpoints
        are executed by Rust rather than by an ORM. Five routes, zero
        interpreter involvement, and — with `tools=True` — five agent tools.

        Authorization is split, because reads and writes almost never warrant
        the same scope: `read_scopes` guards list/get, `write_scopes` guards
        create/update/delete. `scopes` sets both at once. Leaving all three
        unset makes the resource fully public, which `webcortex security` reports.
        """
        table = table or name
        if not self.database:
            raise ValueError(f"resource {name!r} needs a database; pass database=... to WebCortex()")
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

        reads = list(read_scopes if read_scopes is not None else scopes)
        writes = list(write_scopes if write_scopes is not None else scopes)
        common = {"tool": tools}

        res.route_ids.append(
            self.query(
                "GET", f"/{name}",
                f"SELECT * FROM {table} LIMIT COALESCE(?, 50) OFFSET COALESCE(?, 0)",
                params=["limit", "offset"], returns="many",
                summary=f"List {name}",
                description=f"Return a page of {name} rows, newest first by insertion order.",
                input_schema=list_schema,
                output_schema={"type": "array", "items": body_schema(True, [])},
                tool_name=f"list_{name}", scopes=reads, **common,
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
                tool_name=f"get_{name}", scopes=reads, **common,
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
                tool_name=f"create_{name}", scopes=writes, **common,
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
                tool_name=f"update_{name}", scopes=writes, **common,
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
                tool_name=f"delete_{name}", scopes=writes, **common,
            )
        )

        if create_table:
            self._schema_sql.append(_create_table_sql(table, fields, primary_key))

        self._resources.append(res)
        return res

    # ------------------------------------------------------------------
    # Security
    # ------------------------------------------------------------------

    def api_key(self, env_var: str, *, id: str, scopes: Sequence[str] = ()) -> None:
        """Accept an API key read from `env_var`, granting `scopes`.

        The key itself never appears in your source or in the manifest — only
        the name of the variable holding it. The runtime stores a SHA-256 of the
        secret and compares in constant time.
        """
        self._auth["api_keys"][env_var] = {"id": id, "scopes": list(scopes)}

    def jwt(
        self,
        *,
        secret_env: str,
        algorithm: str = "HS256",
        audience: str | None = None,
        issuer: str | None = None,
        leeway_secs: int = 30,
    ) -> None:
        """Accept bearer JWTs verified with the secret in `secret_env`.

        Scopes are read from the standard `scope` (space-delimited) or `scopes`
        (array) claims. `exp` is required and always validated.

        `leeway_secs` is the clock skew tolerated on `exp`/`nbf`. It is stated
        here rather than inherited because the underlying library defaults to
        60 seconds, and an expired token staying valid for a further minute
        should be a decision, not a surprise.
        """
        self._auth["jwt"] = {
            "secret_env": secret_env,
            "algorithm": algorithm,
            "audience": audience,
            "issuer": issuer,
            "leeway_secs": leeway_secs,
        }

    def anonymous_scopes(self, *scopes: str) -> None:
        """Grant scopes to callers presenting no credential.

        Empty by default. Use sparingly: this is how a route you believed was
        protected becomes public.
        """
        self._auth["anonymous_scopes"] = list(scopes)

    def cors(
        self,
        *origins: str,
        credentials: bool = False,
        methods: Sequence[str] | None = None,
        headers: Sequence[str] | None = None,
        expose: Sequence[str] = (),
        max_age: int = 600,
    ) -> None:
        """Enable CORS for explicit origins.

        `credentials=True` together with a `"*"` origin is rejected at boot: the
        combination is forbidden by the CORS spec, and browsers fail it in ways
        that are maddening to debug.
        """
        self._cors = {
            "enabled": True,
            "allow_origins": list(origins),
            "allow_credentials": credentials,
            "expose_headers": list(expose),
            "max_age_secs": max_age,
        }
        if methods:
            self._cors["allow_methods"] = list(methods)
        if headers:
            self._cors["allow_headers"] = list(headers)

    def rate_limit(self, per_second: float = 50.0, *, burst: int = 100) -> None:
        """Token-bucket rate limiting, keyed per principal (IP when anonymous)."""
        self._rate_limit = {
            "enabled": True,
            "per_second": per_second,
            "burst": burst,
            "idle_eviction_secs": 300,
        }

    def security_headers(
        self,
        *,
        enabled: bool = True,
        frame_options: str = "DENY",
        referrer_policy: str = "strict-origin-when-cross-origin",
        content_security_policy: str | None = None,
        hsts_max_age: int = 31_536_000,
    ) -> None:
        """Tune the always-on security headers. Sensible without calling this."""
        self._security_headers = {
            "enabled": enabled,
            "frame_options": frame_options,
            "referrer_policy": referrer_policy,
            "content_security_policy": content_security_policy,
            "hsts_max_age_secs": hsts_max_age,
        }

    # ------------------------------------------------------------------
    # Server-rendered pages and static assets
    # ------------------------------------------------------------------

    def page(
        self,
        path: str,
        template: str,
        *,
        sql: str | None = None,
        params: Sequence[str] = (),
        returns: str = "many",
        bind: str = "data",
        data: Any = None,
        status: int = 200,
        scopes: Sequence[str] = (),
        method: str = "GET",
    ) -> int:
        """Render a server-side template.

        Data comes from exactly one declared source and is resolved *before*
        rendering: `sql=` for a query, `data=` for a constant, or neither. The
        template itself can never fetch anything — it has no handle to the
        database and no way to call Python — which is the constraint that keeps
        this layer from turning into a second, worse view layer.

        For a template whose data needs real logic, use `@app.page_handler`.
        """
        if sql and data is not None:
            raise ValueError("page(): pass either sql= or data=, not both")

        if sql:
            if not self.database:
                raise ValueError(f"page {path!r} uses sql= but no database is configured")
            page_data: dict = {
                "kind": "query", "sql": sql, "params": list(params),
                "returns": returns, "bind": bind,
            }
        elif data is not None:
            page_data = {"kind": "static", "value": data}
        else:
            page_data = {"kind": "none"}

        self._templates_used.add(template)
        return self._add_route(
            method, path,
            {"kind": "page", "template": template, "data": page_data, "status": status},
            summary=f"Page {path}",
            scopes=scopes,
        )

    def page_handler(
        self, path: str, template: str, *, status: int = 200, scopes: Sequence[str] = ()
    ) -> Callable:
        """A page whose context comes from a Python function returning a dict."""

        def decorator(fn: Callable) -> Callable:
            wants_request = _request_param(fn)
            handler_index = len(self._handlers)
            self._handlers.append(_bind_handler(fn, wants_request))
            self._templates_used.add(template)
            self._add_route(
                "GET", path,
                {
                    "kind": "page", "template": template,
                    "data": {"kind": "python", "handler": handler_index},
                    "status": status,
                },
                summary=inspect.getdoc(fn) or f"Page {path}",
                scopes=scopes,
            )
            return fn

        return decorator

    def static_files(
        self, path: str, directory: str, *, index: str | None = None, cache_secs: int = 3600
    ) -> int:
        """Serve a directory. `path` must end in a wildcard segment.

        Traversal, symlink escapes, and dotfiles are refused by the runtime.
        """
        if not path.endswith("}"):
            path = path.rstrip("/") + "/{*file}"
        if not os.path.isdir(directory):
            raise ValueError(f"static_files(): {directory!r} is not a directory")
        return self._add_route(
            "GET",
            path,
            {"kind": "files", "dir": directory, "index": index, "cache_secs": cache_secs},
            summary=f"Static files from {directory}",
        )

    # ------------------------------------------------------------------
    # Behaviours
    # ------------------------------------------------------------------

    def behaviour(
        self,
        name: str | None = None,
        *,
        description: str = "",
        tools: Sequence[str] = (),
        scopes: Sequence[str] = (),
        max_steps: int = 50,
        token_budget: int | None = None,
        model: str = "claude-opus-5",
        max_tokens: int = 4096,
        temperature: float = 1.0,
        expose_at: str | None = None,
        expose_scopes: Sequence[str] | None = None,
        tool: bool = True,
    ) -> Callable:
        """Declare a Behaviour: a procedure whose control flow is real Python.

        A "skill" written as a prompt is a suggestion — the model reads it and
        may ignore it, and "if X then Y" fails silently when it does. A Behaviour
        inverts that. The loops and branches are code that always runs; only the
        leaves are probabilistic:

            @app.behaviour("triage", tools=["list_tickets", "update_tickets"])
            def triage(ctx, input):
                tickets = ctx.call("list_tickets", status="open")

                urgent = 0
                for ticket in tickets:                      # a real loop
                    verdict = ctx.ask(                      # a model call
                        f"Classify this ticket: {ticket['body']}",
                        schema={
                            "type": "object",
                            "properties": {
                                "category": {"enum": ["bug", "billing", "other"]},
                                "urgency": {"type": "integer"},
                            },
                            "required": ["category", "urgency"],
                        },
                    )
                    if verdict["urgency"] > 7:              # a real branch
                        urgent += 1
                        ctx.call("page_oncall", ticket=ticket["id"])
                    ctx.call("update_tickets", id=ticket["id"],
                             category=verdict["category"])

                return {"triaged": len(tickets), "urgent": urgent}

        The handler takes `(ctx, input)`. `input` is the JSON payload the caller
        sent; `ctx` is how the behaviour reaches the outside world:

        - `ctx.call(tool, **kwargs)` — invoke one of the app's tools, in-process,
          under this behaviour's delegated principal
        - `ctx.ask(prompt, schema=...)` — a model call; with a schema the model
          is *forced* into that shape, so branches switch on real values
        - `ctx.log(msg)`, `ctx.halt(reason)`, `ctx.usage`, `ctx.trace`, `ctx.user`

        A behaviour is exposed as a tool by default, so agents can invoke
        behaviours and behaviours can compose with each other. `max_steps` caps
        total leaf operations and `token_budget` caps spend — both enforced by
        the runtime, so a runaway loop costs a bounded amount.
        """

        def decorator(fn: Callable) -> Callable:
            behaviour_name = name or fn.__name__
            doc = inspect.getdoc(fn) or ""
            handler_index = len(self._handlers)
            self._handlers.append(fn)

            input_schema = _behaviour_input_schema(fn)

            self._behaviours.append(
                {
                    "name": behaviour_name,
                    "description": description or doc,
                    "handler": handler_index,
                    "tools": list(tools),
                    "scopes": list(scopes),
                    "max_steps": max_steps,
                    "token_budget": token_budget,
                    "model": model,
                    "max_tokens": max_tokens,
                    "temperature": temperature,
                    "input_schema": input_schema,
                }
            )

            # A behaviour becomes a route, which is what makes it a tool, an
            # OpenAPI operation, and an MCP entry — with no separate plumbing.
            path = expose_at or f"/behaviours/{behaviour_name.replace('_', '-')}"
            guard = list(expose_scopes if expose_scopes is not None else scopes)
            self._add_route(
                "POST",
                path,
                {"kind": "behaviour", "behaviour": behaviour_name, "handler": handler_index},
                summary=(description or doc).split("\n", 1)[0] or f"Run the {behaviour_name} behaviour",
                description=description or doc,
                input_schema=input_schema,
                tool=tool,
                tool_name=behaviour_name,
                read_only=False,
                idempotent=False,
                scopes=guard,
            )
            return fn

        # Allow both @app.behaviour and @app.behaviour("name").
        if callable(name):
            fn, name = name, None
            return decorator(fn)
        return decorator

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
        scopes: Sequence[str] = (),
        expose_scopes: Sequence[str] | None = None,
        temperature: float = 1.0,
        max_tokens: int = 4096,
    ) -> None:
        """Declare an agent that lives inside the application.

        `tools` names routes exposed with `tool=True`. Because the agent calls
        them through the same dispatcher the HTTP server uses, a tool call is an
        in-process function call — not a loopback request — and it inherits the
        route's declared scopes.

        `scopes` is what the agent may *use*; `expose_scopes` is who may *start*
        a run. They default to the same set, because an endpoint that spends
        tokens and exercises tools should not be less guarded than the tools
        themselves. Passing `expose_scopes=[]` makes the endpoint public — which
        `webcortex security` will report.

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
                "scopes": list(scopes),
                "temperature": temperature,
                "max_tokens": max_tokens,
            }
        )
        if expose_at:
            guard = list(expose_scopes if expose_scopes is not None else scopes)
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
                scopes=guard,
            )

    # ------------------------------------------------------------------
    # Manifest + run
    # ------------------------------------------------------------------

    def _routes_with_default_root(self) -> list[dict]:
        """Routes, plus a discovery route at `/` when the app leaves it free."""
        if any(r["path"] == "/" and r["method"] == "GET" for r in self._routes):
            return self._routes
        default = {
            "id": self._next_id,
            "method": "GET",
            "path": "/",
            "op": {
                "kind": "static",
                "status": 200,
                "body": {
                    "app": self.name,
                    "version": self.version,
                    "docs": f"{self.control_prefix}/openapi.json",
                },
            },
            "summary": "Service discovery",
            "description": "Identifies the application and points at its OpenAPI document.",
            "input_schema": None,
            "output_schema": None,
            "tool": {"expose": False, "name": None, "read_only": True,
                     "idempotent": True, "scopes": []},
            "scopes": [],
            "approval": "never",
        }
        return [*self._routes, default]

    def manifest(self) -> dict:
        return {
            "name": self.name,
            "version": self.version,
            "description": self.description,
            "server": {
                "host": os.environ.get("WEBCORTEX_HOST", self.host),
                "port": int(os.environ.get("WEBCORTEX_PORT", self.port)),
                "python_workers": self.workers,
                "control_prefix": self.control_prefix,
                "request_timeout_secs": self.request_timeout,
                "shutdown_timeout_secs": self.shutdown_timeout,
            },
            "auth": self._auth,
            "cors": self._cors or {"enabled": False},
            "rate_limit": self._rate_limit or {"enabled": False},
            "security_headers": self._security_headers,
            "templates": (
                {"dir": self.templates_dir, "autoescape": True}
                if self.templates_dir
                else None
            ),
            "database": (
                {"url": os.environ.get("WEBCORTEX_DATABASE_URL", self.database), "max_connections": 16}
                if self.database
                else None
            ),
            "routes": self._routes_with_default_root(),
            "upstreams": self._upstreams,
            "agents": self._agents,
            "behaviours": self._behaviours,
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

    def typescript_client(self) -> str:
        """Generate a dependency-free typed TypeScript client."""
        from . import _core

        return _core.typescript_client(self.manifest_json())

    def security_report(self) -> dict:
        """What is reachable without a credential, and what is gated.

        Printed by `webcortex check` so the public attack surface is something you
        read on every run rather than something you audit once.
        """
        auth_on = bool(self._auth["api_keys"]) or self._auth["jwt"] is not None
        public = [
            f"{r['method']} {r['path']}"
            for r in self._routes_with_default_root()
            if not r["scopes"] and not r["tool"]["scopes"]
        ]
        return {
            "auth_configured": auth_on,
            "anonymous_scopes": self._auth["anonymous_scopes"],
            "cors_enabled": bool(self._cors),
            "cors_origins": (self._cors or {}).get("allow_origins", []),
            "rate_limited": bool(self._rate_limit),
            "security_headers": self._security_headers.get("enabled", True),
            "public_routes": public,
            "gated_tools": [
                r["tool"]["name"] or f"{r['method']} {r['path']}"
                for r in self._routes_with_default_root()
                if r.get("approval") == "required"
            ],
            "agents": [
                {"name": a["name"], "tools": a["tools"], "scopes": a.get("scopes", [])}
                for a in self._agents
            ],
            "behaviours": [
                {
                    "name": b["name"],
                    "tools": b["tools"],
                    "scopes": b.get("scopes", []),
                    "max_steps": b["max_steps"],
                    "token_budget": b.get("token_budget"),
                }
                for b in self._behaviours
            ],
        }

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

        url = os.environ.get("WEBCORTEX_DATABASE_URL", self.database)
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


def _behaviour_input_schema(fn: Callable) -> dict:
    """Derive the payload schema from the handler's `input` annotation.

    A behaviour signature is `(ctx, input)`. Annotating `input` with a dataclass
    gives agents a precise tool schema for free; leaving it bare accepts any
    object.
    """
    try:
        hints = __import__("typing").get_type_hints(fn)
    except Exception:
        hints = getattr(fn, "__annotations__", {})

    params = [p for p in inspect.signature(fn).parameters]
    if len(params) < 2:
        return {"type": "object", "properties": {}}

    annotation = hints.get(params[1], inspect.Parameter.empty)
    schema = _schema.json_schema_for(annotation)
    if schema.get("type") == "object" and "properties" in schema:
        return schema
    return {"type": "object", "properties": {}, "additionalProperties": True}


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
