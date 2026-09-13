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
        self._flows: list[dict] = []
        self._contexts: list[dict] = []
        self._memories: list[dict] = []
        self._models: dict = {"aliases": {}, "providers": {}, "pricing": {}}
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
    # Models: tiers, providers, prices
    # ------------------------------------------------------------------

    def models(self, **aliases: str) -> None:
        """Name model tiers once, and change them in one place.

            app.models(
                default="claude-opus-5",
                fast="claude-haiku-4-5-20251001",
                local="ollama/qwen3.5:9b",
            )

        Anywhere a model is named — `app.agent(model=...)`, `@app.behaviour`,
        `ctx.ask(model=...)`, a flow's classifier — an alias resolves here. The
        token-economy habit this enables is simple: classification and
        extraction leaves use `"fast"`, judgement uses `"default"`, and moving
        a workload to a cheaper or local model is one edit.

        A model name selects its provider by prefix: `ollama/…` for a local
        Ollama server, `openai/…`, `anthropic/…`, or the name of a provider
        declared with `app.provider(...)`. A bare `claude-*` name is Anthropic.
        `default` and `fast` have built-in values.
        """
        for alias, target in aliases.items():
            if not isinstance(target, str) or not target:
                raise ValueError(f"models(): alias {alias!r} must name a model")
            self._models["aliases"][alias] = target

    def provider(
        self,
        name: str,
        *,
        base_url: str,
        kind: str = "openai",
        api_key_env: str | None = None,
    ) -> None:
        """Declare a model endpoint reachable as `<name>/<model>`.

        `kind="openai"` is any Chat-Completions-compatible server — vLLM,
        LM Studio, Groq, OpenRouter, a second Ollama host. `kind="anthropic"`
        is a Messages-API gateway. As everywhere else, the credential is named
        by environment variable, never embedded.
        """
        if kind not in ("openai", "anthropic"):
            raise ValueError("provider(): kind must be 'openai' or 'anthropic'")
        if not base_url.startswith(("http://", "https://")):
            raise ValueError(f"provider {name!r}: base_url must start with http:// or https://")
        self._models["providers"][name] = {
            "kind": kind,
            "base_url": base_url,
            "api_key_env": api_key_env,
        }

    def pricing(
        self,
        model: str,
        *,
        input_per_mtok: float,
        output_per_mtok: float,
        cache_read_per_mtok: float = 0.0,
        cache_write_per_mtok: float = 0.0,
    ) -> None:
        """Declare what a model costs, in USD per million tokens.

        Nothing is built in: prices change, and a stale number is worse than
        none. With prices declared, `GET /_webcortex/usage` reports an
        estimated cost alongside the token counts it always reports.
        """
        self._models["pricing"][model] = {
            "input_per_mtok": float(input_per_mtok),
            "output_per_mtok": float(output_per_mtok),
            "cache_read_per_mtok": float(cache_read_per_mtok),
            "cache_write_per_mtok": float(cache_write_per_mtok),
        }

    # ------------------------------------------------------------------
    # Context providers
    # ------------------------------------------------------------------

    def context(
        self,
        name: str,
        *,
        sql: str | None = None,
        params: Sequence[str] = (),
        returns: str = "many",
        data: Any = None,
        description: str = "",
        max_chars: int = 4000,
    ) -> Any:
        """Declare a named source of context for agents and behaviours.

        A run is only as good as what it knows at step one. A context provider
        is resolved when a run starts and handed to the model as a delimited
        block in the system prompt — declared once, reused by anything that
        names it, and bounded in size because it is re-sent on every step.

        Three sources, mirroring `app.page`:

            app.context("catalogue", sql="SELECT title, author FROM books LIMIT 50")
            app.context("policy", data={"refund_days": 30, "max_auto_refund": 500})

            @app.context("account")
            def account(req) -> dict:
                return lookup(req.user["id"])

        A SQL provider may bind `@principal`, the identity of whoever started
        the run, and any key of the run's input. Agents name providers with
        `context=[...]`; behaviours declare them the same way and read them on
        demand with `ctx.context(name)`.
        """
        if sql is not None and data is not None:
            raise ValueError("context(): pass either sql= or data=, not both")
        if max_chars <= 0:
            raise ValueError("context(): max_chars must be positive")

        if sql is not None:
            if not self.database:
                raise ValueError(f"context {name!r} uses sql= but no database is configured")
            if returns not in ("many", "one", "affected"):
                raise ValueError("returns must be 'many', 'one', or 'affected'")
            self._contexts.append({
                "name": name, "description": description, "max_chars": max_chars,
                "source": {"kind": "query", "sql": sql, "params": list(params), "returns": returns},
            })
            return None

        if data is not None:
            self._contexts.append({
                "name": name, "description": description, "max_chars": max_chars,
                "source": {"kind": "static", "value": data},
            })
            return None

        def decorator(fn: Callable) -> Callable:
            wants_request = _request_param(fn)
            handler_index = len(self._handlers)
            self._handlers.append(_bind_handler(fn, wants_request))
            self._contexts.append({
                "name": name,
                "description": description or (inspect.getdoc(fn) or ""),
                "max_chars": max_chars,
                "source": {"kind": "python", "handler": handler_index},
            })
            return fn

        return decorator

    # ------------------------------------------------------------------
    # Memory
    # ------------------------------------------------------------------

    def memory(
        self,
        name: str = "memory",
        *,
        scopes: Sequence[str] = (),
        read_scopes: Sequence[str] | None = None,
        write_scopes: Sequence[str] | None = None,
        max_results: int = 10,
    ) -> list[str]:
        """Give agents a persistent, per-user memory — four tools, zero Python.

            notes = app.memory("notes", scopes=["read"])
            app.agent("assistant", memory="notes", ...)

        Creates a table and four routes, all executed in Rust and all exposed
        as tools: `notes_remember(key, value)`, `notes_recall(key)`,
        `notes_search(query, limit)` and `notes_forget(key)`. Every row is
        keyed by the *root* principal — the human or service behind however
        many agents deep the call is — so an agent writing on someone's behalf
        writes to that someone's memory and can never read another's.

        Search is substring match, on purpose. Semantic retrieval belongs in a
        Python handler over the embedding store you already run; this is the
        durable scratchpad that most assistants need and few frameworks ship.

        Returns the tool names, so they can be given to agents that do not
        use the `memory=` shorthand.
        """
        if not self.database:
            raise ValueError(f"memory {name!r} needs a database; pass database=... to WebCortex()")
        if not name.isidentifier():
            raise ValueError(f"memory name {name!r} must be a valid identifier")
        if any(m["name"] == name for m in self._memories):
            raise ValueError(f"memory {name!r} is already declared")

        table = f"wcx_{name}"
        reads = list(read_scopes if read_scopes is not None else scopes)
        writes = list(write_scopes if write_scopes is not None else scopes)
        row = {
            "type": "object",
            "properties": {
                "key": {"type": "string"},
                "value": {"type": "string"},
                "updated_at": {"type": "string"},
            },
        }
        tools = [f"{name}_remember", f"{name}_recall", f"{name}_search", f"{name}_forget"]

        self.query(
            "POST", f"/{name}/remember",
            f"INSERT INTO {table} (principal, key, value, updated_at) "
            f"VALUES (?, ?, ?, datetime('now')) "
            f"ON CONFLICT(principal, key) DO UPDATE SET value = excluded.value, "
            f"updated_at = excluded.updated_at RETURNING key, value, updated_at",
            params=["@principal", "key", "value"], returns="one",
            summary=f"Remember something under a key in the {name} memory",
            description=(f"Store or overwrite a fact in the caller's {name} memory. "
                         "Use a short, stable key so it can be recalled later."),
            input_schema={"type": "object",
                          "properties": {"key": {"type": "string"}, "value": {"type": "string"}},
                          "required": ["key", "value"], "additionalProperties": False},
            output_schema=row, tool=True, tool_name=tools[0], scopes=writes,
            read_only=False, idempotent=True,
        )
        self.query(
            "GET", f"/{name}/recall/{{key}}",
            f"SELECT key, value, updated_at FROM {table} WHERE principal = ? AND key = ?",
            params=["@principal", "key"], returns="one",
            summary=f"Recall one fact from the {name} memory by key",
            description=f"Fetch the value stored under a key in the caller's {name} memory. 404 when nothing is stored.",
            input_schema={"type": "object", "properties": {"key": {"type": "string"}},
                          "required": ["key"], "additionalProperties": False},
            output_schema=row, tool=True, tool_name=tools[1], scopes=reads,
        )
        self.query(
            "GET", f"/{name}/search",
            f"SELECT key, value, updated_at FROM {table} WHERE principal = ? "
            f"AND (key LIKE '%' || ? || '%' OR value LIKE '%' || ? || '%') "
            f"ORDER BY updated_at DESC LIMIT COALESCE(?, {int(max_results)})",
            params=["@principal", "query", "query", "limit"], returns="many",
            summary=f"Search the {name} memory",
            description=f"Find facts in the caller's {name} memory whose key or value contains the query, newest first.",
            input_schema={"type": "object",
                          "properties": {"query": {"type": "string"},
                                         "limit": {"type": "integer", "default": int(max_results)}},
                          "required": ["query"], "additionalProperties": False},
            output_schema={"type": "array", "items": row}, tool=True, tool_name=tools[2], scopes=reads,
        )
        self.query(
            "DELETE", f"/{name}/forget/{{key}}",
            f"DELETE FROM {table} WHERE principal = ? AND key = ?",
            params=["@principal", "key"], returns="affected",
            summary=f"Forget one fact in the {name} memory",
            description=f"Delete the value stored under a key in the caller's {name} memory.",
            input_schema={"type": "object", "properties": {"key": {"type": "string"}},
                          "required": ["key"], "additionalProperties": False},
            output_schema={"type": "object", "properties": {"affected": {"type": "integer"}}},
            tool=True, tool_name=tools[3], scopes=writes,
        )

        self._schema_sql.append(
            f"CREATE TABLE IF NOT EXISTS {table} (\n"
            f"  principal TEXT NOT NULL,\n"
            f"  key TEXT NOT NULL,\n"
            f"  value TEXT NOT NULL,\n"
            f"  updated_at TEXT NOT NULL,\n"
            f"  PRIMARY KEY (principal, key)\n"
            f");"
        )
        self._memories.append({"name": name, "table": table, "tools": tools})
        return tools

    # ------------------------------------------------------------------
    # Flows
    # ------------------------------------------------------------------

    def flow(
        self,
        name: str,
        *,
        pipeline: Sequence[Any] | None = None,
        parallel: Sequence[Any] | None = None,
        merge: str = "collect",
        route: dict[str, Any] | None = None,
        default: Any = None,
        classify_with: str | None = None,
        classify_prompt: str | None = None,
        description: str = "",
        scopes: Sequence[str] = (),
        expose_scopes: Sequence[str] | None = None,
        token_budget: int | None = None,
        expose_at: str | None = None,
        tool: bool = True,
        input_schema: dict | None = None,
    ) -> None:
        """Declare an orchestration as data, executed in Rust.

        Three shapes cover most multi-agent arrangements:

            # each step receives the previous step's output
            app.flow("report", pipeline=["researcher", "writer"], token_budget=200_000)

            # every branch receives the same input, concurrently
            app.flow("audit", parallel=["security_review", "style_review"], merge="collect")

            # a cheap model picks a branch
            app.flow("front_desk",
                     route={"billing": "billing_agent", "technical": "tech_agent"},
                     default="general_agent", classify_with="fast")

        A step is a tool name — an agent, a behaviour, another flow, or any
        route marked `tool=True` — or a dict `{"tool": name, "input": {...}}`
        whose `input` maps arguments: `"$"` is the incoming value, `"$.a.b"` a
        path into it, `"$input"` the flow's original input. Without a mapping,
        an agent step receives `{"input": <previous output>}` and any other
        step receives the previous output as its arguments.

        Every step runs under the flow's delegated principal, one nesting level
        deeper, against one shared `token_budget` — so a pipeline of three
        agents costs at most what you said, not three times what each said.
        A flow is itself a tool, so flows nest and agents can invoke them.
        """
        given = [k for k, v in (("pipeline", pipeline), ("parallel", parallel), ("route", route)) if v is not None]
        if len(given) != 1:
            raise ValueError("flow(): pass exactly one of pipeline=, parallel=, or route=")
        if any(f["name"] == name for f in self._flows):
            raise ValueError(f"flow {name!r} is already declared")

        def step(s: Any) -> dict:
            if isinstance(s, str):
                return {"tool": s, "input": None}
            if isinstance(s, dict) and isinstance(s.get("tool"), str):
                return {"tool": s["tool"], "input": s.get("input")}
            raise ValueError(
                f"flow {name!r}: each step must be a tool name or a dict with a 'tool' key, got {s!r}"
            )

        if pipeline is not None:
            steps = [step(s) for s in pipeline]
            if not steps:
                raise ValueError(f"flow {name!r}: a pipeline needs at least one step")
            kind: dict = {"kind": "pipeline", "steps": steps}
        elif parallel is not None:
            if merge not in ("collect", "merge"):
                raise ValueError("flow(): merge must be 'collect' or 'merge'")
            branches = [step(s) for s in parallel]
            if not branches:
                raise ValueError(f"flow {name!r}: a parallel needs at least one branch")
            kind = {"kind": "parallel", "branches": branches, "merge": merge}
        else:
            assert route is not None
            if not route:
                raise ValueError(f"flow {name!r}: a router needs at least one route")
            kind = {
                "kind": "route",
                "routes": {str(label): step(s) for label, s in route.items()},
                "default": step(default) if default is not None else None,
                "classify_with": classify_with,
                "classify_prompt": classify_prompt,
            }

        schema = input_schema or {
            "type": "object",
            "properties": {"input": {"type": "string", "description": "The flow's input."}},
            "additionalProperties": True,
        }
        self._flows.append({
            "name": name,
            "description": description,
            "kind": kind,
            "scopes": list(scopes),
            "token_budget": token_budget,
            "input_schema": schema,
        })

        path = expose_at or f"/flows/{name.replace('_', '-')}"
        guard = list(expose_scopes if expose_scopes is not None else scopes)
        self._add_route(
            "POST", path,
            {"kind": "flow", "flow": name},
            summary=description.split("\n", 1)[0] or f"Run the {name} flow",
            description=description,
            input_schema=schema,
            tool=tool, tool_name=name,
            read_only=False, idempotent=False,
            scopes=guard,
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
        context: Sequence[str] = (),
        scopes: Sequence[str] = (),
        max_steps: int = 50,
        token_budget: int | None = None,
        model: str = "default",
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
        - `ctx.gather((tool, kwargs), ...)` — several tool calls at once, run
          concurrently on the Rust runtime
        - `ctx.ask(prompt, schema=..., model="fast")` — a model call; with a
          schema the model is *forced* into that shape, so branches switch on
          real values; `model` may be an alias from `app.models(...)`
        - `ctx.ask_many([prompts], schema=...)` — the classification loop
          collapsed into one concurrent wait
        - `ctx.context(name)` — resolve a declared context provider
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
                    "context": list(context),
                }
            )

            # A behaviour becomes a route, which is what makes it a tool, an
            # OpenAPI operation, and an MCP entry — with no separate plumbing.
            path = expose_at or f"/behaviours/{behaviour_name.replace('_', '-')}"
            guard = list(expose_scopes if expose_scopes is not None else scopes)
            self._add_route(
                "POST",
                path,
                {"kind": "behaviour", "behaviour": behaviour_name},
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
        model: str = "default",
        system: str = "",
        tools: Sequence[str] = (),
        handoffs: Sequence[str] = (),
        context: Sequence[str] = (),
        memory: str | None = None,
        description: str = "",
        max_steps: int | None = 12,
        token_budget: int | None = None,
        expose_at: str | None = None,
        scopes: Sequence[str] = (),
        expose_scopes: Sequence[str] | None = None,
        temperature: float = 1.0,
        max_tokens: int = 4096,
        cache: bool = True,
        context_window: int | None = None,
        tool_result_limit: int = 16_384,
        compact_with: str | None = None,
        keep_recent: int = 6,
        tool: bool = True,
    ) -> None:
        """Declare an agent that lives inside the application.

        `tools` names routes exposed with `tool=True` — including other
        agents, behaviours and flows, which is all a supervisor needs. Because
        the agent calls them through the same dispatcher the HTTP server uses,
        a tool call is an in-process function call, not a loopback request,
        and it inherits the route's declared scopes.

        **Every agent is a tool.** It is mounted at `expose_at` (default
        `/agents/<name>`) and exposed under its own name, so another agent can
        list it in `tools=[...]`. The endpoint takes `{"input": "...",
        "session_id": "..."}`; a `session_id` continues a conversation.

        `handoffs` names agents this one may transfer the conversation to.
        Each becomes a `transfer_to_<name>` tool; calling it swaps the agent in
        control while the conversation, the budget and the caller's authority
        carry over — and authority can only shrink along the chain.

        `context` names providers declared with `app.context(...)`, resolved
        at run start into the system prompt. `memory` names a store declared
        with `app.memory(...)` and adds its four tools plus a hint on how to
        use them.

        **Token economy.** `cache=True` asks the provider to cache the system
        prompt and tool definitions across steps. `tool_result_limit` caps
        what the model sees of any one tool result. `context_window` sets the
        measured input size beyond which older turns are summarised with
        `compact_with` (default: the `fast` alias), keeping the last
        `keep_recent` messages intact. `token_budget` caps the whole run,
        including every nested agent, behaviour and flow it calls.

        `scopes` is what the agent may *use*; `expose_scopes` is who may *start*
        a run. They default to the same set, because an endpoint that spends
        tokens and exercises tools should not be less guarded than the tools
        themselves. Passing `expose_scopes=[]` makes the endpoint public — which
        `webcortex security` will report.

        A typo in `tools`, `handoffs` or `context` is a boot error, not a
        runtime surprise.
        """
        if keep_recent < 1:

            raise ValueError("agent(): keep_recent must be at least 1")

        tool_list = list(tools)
        system_text = system
        if memory is not None:
            mem = next((m for m in self._memories if m["name"] == memory), None)
            if mem is None:
                raise ValueError(
                    f"agent {name!r} names memory {memory!r}, which is not declared; "
                    f"call app.memory({memory!r}) first"
                )
            for t in mem["tools"]:
                if t not in tool_list:
                    tool_list.append(t)
            hint = (
                f"You have a persistent memory. Use {memory}_remember to store facts worth "
                f"keeping across conversations, {memory}_recall or {memory}_search to "
                f"retrieve them, and {memory}_forget to drop ones that are no longer true."
            )
            system_text = f"{system_text}\n\n{hint}".strip()

        self._agents.append(
            {
                "name": name,
                "description": description,
                "model": model,
                "system": system_text,
                "tools": tool_list,
                "handoffs": list(handoffs),
                "context": list(context),
                "max_steps": max_steps,
                "token_budget": token_budget,
                "scopes": list(scopes),
                "temperature": temperature,
                "max_tokens": max_tokens,
                "cache": bool(cache),
                "policy": {
                    "max_tool_result_bytes": int(tool_result_limit),
                    "max_context_tokens": context_window,
                    "compact_with": compact_with,
                    "keep_recent": int(keep_recent),
                },
            }
        )

        path = expose_at or f"/agents/{name.replace('_', '-')}"
        guard = list(expose_scopes if expose_scopes is not None else scopes)
        self._add_route(
            "POST", path,
            {"kind": "agent", "agent": name},
            summary=description.split("\n", 1)[0] or f"Ask the {name} agent",
            description=description or f"Ask the {name} agent.",
            input_schema={
                "type": "object",
                "properties": {
                    "input": {"type": "string", "description": "What to ask the agent."},
                    "session_id": {
                        "type": "string",
                        "description": "Continue an earlier conversation with this id.",
                    },
                },
                "required": ["input"],
                "additionalProperties": False,
            },
            tool=tool, tool_name=name,
            read_only=False, idempotent=False,
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
            "flows": self._flows,
            "contexts": self._contexts,
            "models": self._models,
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

    def context_pack(self) -> str:
        """A compact description of this app for an AI coding tool.

        Everything a model needs to extend the app correctly — routes, tools
        and their schemas, agents, behaviours, flows, context providers,
        memory, model aliases, security posture — in a few thousand tokens
        instead of the whole codebase. `webcortex context` prints it.
        """
        from . import contextpack

        return contextpack.build(self)


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
                {
                    "name": a["name"],
                    "tools": a["tools"],
                    "handoffs": a.get("handoffs", []),
                    "scopes": a.get("scopes", []),
                    "token_budget": a.get("token_budget"),
                }
                for a in self._agents
            ],
            "flows": [
                {
                    "name": f["name"],
                    "kind": f["kind"]["kind"],
                    "scopes": f["scopes"],
                    "token_budget": f.get("token_budget"),
                }
                for f in self._flows
            ],
            "memories": [m["name"] for m in self._memories],
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
