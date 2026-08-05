# API reference

```python
from webcortex import WebCortex, Request, Response, HTTPError, free_threaded
```

## `WebCortex(name, **options)`

```python
app = WebCortex(
    "myapp",
    description="",
    version="0.1.0",
    database=None,           # "sqlite://./app.db"
    templates=None,          # "templates"
    host="127.0.0.1",
    port=8000,
    workers=None,            # None = CPU count
    control_prefix="/_webcortex",
    request_timeout=30,
    shutdown_timeout=25,
)
```

---

## Routes

### `app.route(method, path, **opts)` and shorthands

`app.get` · `app.post` · `app.put` · `app.patch` · `app.delete`

```python
@app.get("/items/{id}", tool=True, scopes=["read"])
def get_item(id: int) -> dict: ...
```

Shared options on every route-declaring call:

| Option | Default | |
|---|---|---|
| `tool` | `False` | Expose as an agent tool |
| `tool_name` | derived | Override the generated name |
| `scopes` | `()` | Required to reach the route at all |
| `approval` | `"never"` | `"required"` gates agent invocation |
| `read_only` | by method | MCP hint |
| `idempotent` | by method | MCP hint |
| `summary` | docstring | |

### `app.static(method, path, body, *, status=200, **opts)`

Constant response, serialised at boot.

### `app.query(method, path, sql, *, params=(), returns="many", **opts)`

Bind a route to SQL. `returns` is `"many"` · `"one"` · `"affected"`.

### `app.resource(name, *, fields, **opts)`

```python
app.resource(
    "items",
    fields={"id": int, "name": str},
    table=None,              # defaults to name
    primary_key="id",
    tools=False,
    scopes=(),               # shorthand for both below
    read_scopes=None,
    write_scopes=None,
    create_table=True,
)
```

Returns a `Resource`. Generates five routes.

### `app.page(path, template, **opts)`

```python
app.page("/", "index.html",
         sql=None, params=(), returns="many", bind="data",
         data=None, status=200, scopes=(), method="GET")
```

Pass `sql=` **or** `data=`, never both.

### `app.page_handler(path, template, *, status=200, scopes=())`

Decorator. The function returns a dict used as template context.

### `app.static_files(path, directory, *, index=None, cache_secs=3600)`

### `app.upstream(name, base_url, *, headers=None, bearer_env=None, timeout_ms=30000)`

### `app.proxy(method, path, *, upstream, rewrite=None, **opts)`

---

## Security

### `app.api_key(env_var, *, id, scopes=())`

### `app.jwt(*, secret_env, algorithm="HS256", audience=None, issuer=None, leeway_secs=30)`

### `app.anonymous_scopes(*scopes)`

### `app.cors(*origins, credentials=False, methods=None, headers=None, expose=(), max_age=600)`

### `app.rate_limit(per_second=50.0, *, burst=100)`

### `app.security_headers(*, enabled=True, frame_options="DENY", referrer_policy="strict-origin-when-cross-origin", content_security_policy=None, hsts_max_age=31536000)`

---

## Agents and Behaviours

### `app.agent(name, *, model, **opts)`

```python
app.agent(
    "assistant",
    model="claude-opus-5",
    system="",
    description="",
    tools=(),
    scopes=(),               # what a run MAY DO (intersected with caller)
    expose_scopes=None,      # who may START a run; defaults to scopes
    max_steps=12,
    token_budget=None,
    temperature=1.0,
    max_tokens=4096,
    expose_at=None,
)
```

### `@app.behaviour(name=None, **opts)`

```python
@app.behaviour(
    "triage",
    description="",
    tools=(),
    scopes=(),
    max_steps=50,
    token_budget=None,
    model="claude-opus-5",
    max_tokens=4096,
    temperature=1.0,
    expose_at=None,
    expose_scopes=None,
    tool=True,               # Behaviours are agent tools by default
)
def triage(ctx, input): ...
```

### The `ctx` object

| Member | Signature |
|---|---|
| `ctx.call` | `(tool, **kwargs) -> Any` |
| `ctx.ask` | `(prompt, *, schema=None, system=None, model=None, max_tokens=None, temperature=None)` |
| `ctx.run` | `(name, **kwargs) -> Any` |
| `ctx.log` | `(message) -> None` |
| `ctx.halt` | `(reason) -> NoReturn` |
| `ctx.tools` | `list[str]` |
| `ctx.run_id` | `str` |
| `ctx.depth` | `int` |

---

## Introspection

| Method | Returns |
|---|---|
| `app.check()` | Validated route/tool report; raises on an invalid app |
| `app.security_report()` | Public surface, gated tools, agents |
| `app.openapi()` | OpenAPI 3.1 document |
| `app.typescript_client()` | Generated TS source |
| `app.manifest()` | The raw manifest |
| `app.schema_sql` | Generated DDL |
| `app.run()` | Boot and serve; blocks |

---

## Request

| Member | Type | |
|---|---|---|
| `req.method` | `str` | |
| `req.path` | `str` | |
| `req.params` | `dict` | Path parameters |
| `req.query` | `dict` | Query string |
| `req.headers` | `dict` | Lower-cased keys |
| `req.body` | `bytes` | Raw |
| `req.json()` | `Any` | Parsed, cached |
| `req.user` | `dict` | `{id, authenticated, scopes}` |
| `req.scopes` | `list[str]` | |
| `req.get(name, default=None)` | `Any` | Path → query → body |

## Response

```python
Response(body=None, status=200, headers=None, content_type="application/json")
```

## HTTPError

```python
raise HTTPError(422, "style must be 'short' or 'long'")
```

The message **is** returned to the caller — unlike an unhandled exception, which
is logged and returns a bare `"internal error"`.

## `free_threaded() -> bool`

`True` when running on a GIL-disabled interpreter.
