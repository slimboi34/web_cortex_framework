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
    agent_timeout=600,       # agent, flow and behaviour routes
    shutdown_timeout=25,
    max_connections=10_000,  # open connections incl. WebSockets; past it 503 WC218; 0 = no cap
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

Bind a route to SQL. `returns` is `"many"` · `"one"` · `"affected"`. A param
named `@principal` binds the root principal's id.

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

### `app.upstream(name, base_url, *, headers=None, bearer_env=None, timeout_ms=30000, max_response_bytes=67108864)`

A proxy route buffers the upstream's answer; one over `max_response_bytes` (64 MiB) is a 502 `WC217`.
Repeated query keys are forwarded as sent.

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

## Models

### `app.models(**aliases)`

```python
app.models(default="claude-opus-5", fast="claude-haiku-4-5-20251001", local="ollama/qwen3.5:9b")
```

`default` and `fast` have built-in values. Aliases may chain.

### `app.provider(name, *, base_url, kind="openai", api_key_env=None, options=None)`

Reachable as `<name>/<model>`. `kind` is `"openai"` or `"anthropic"`. `options` are extra
request-body fields for every call to it.

### `app.request_options(provider, **options)`

Extra top-level request-body fields for every call to one provider: a declared one, or the
built-in `"anthropic"`, `"openai"` or `"ollama"`. `None` removes a field the runtime would
send; an agent's or behaviour's own `options` override these key by key. `model`, `messages`,
`tools`, `tool_choice`, `system` and `stream` are reserved (`ValueError`). See
[Request options](request-options.md).

```python
app.request_options("ollama", keep_alive="30m")
app.request_options("openai", max_tokens=None, max_completion_tokens=8000)
```

### `app.pricing(model, *, input_per_mtok, output_per_mtok, cache_read_per_mtok=0.0, cache_write_per_mtok=0.0)`

USD per million tokens. Nothing is built in.

---

## Context and memory

### `app.context(name, *, sql=None, params=(), returns="many", data=None, description="", max_chars=4000)`

Declarative with `sql=` or `data=`; a decorator otherwise:

```python
app.context("policy", data={...})
app.context("mine", sql="SELECT * FROM t WHERE owner = ?", params=["@principal"])

@app.context("account")
def account(req) -> dict: ...
```

### `app.memory(name="memory", *, scopes=(), read_scopes=None, write_scopes=None, max_results=10) -> list[str]`

Creates `wcx_<name>` and four tools: `<name>_remember(key, value)`,
`<name>_recall(key)`, `<name>_search(query, limit)`, `<name>_forget(key)`.
Returns their names.

---

## Agents, Behaviours, Flows

### `app.agent(name, **opts)`

```python
app.agent(
    "assistant",
    model="default",
    system="",
    description="",
    tools=(),
    handoffs=(),
    context=(),
    memory=None,
    scopes=(),               # what a run MAY DO (intersected with caller)
    expose_scopes=None,      # who may START a run; defaults to scopes
    max_steps=12,
    token_budget=None,       # shared with everything the run calls
    cache=True,
    tool_result_limit=16_384,
    context_window=None,
    compact_with=None,       # defaults to the "fast" alias
    keep_recent=6,
    temperature=None,        # sent only when set
    max_tokens=16_000,
    expose_at=None,          # defaults to /agents/<name-with-hyphens>
    tool=True,
    output=None,             # a dataclass, TypedDict or object schema the answer must match
    options=None,            # extra provider request-body fields, e.g. {"seed": 7}
)
```

The route takes `{"input": str, "session_id"?: str, "reset"?: bool, "images"?: [...]}` and
answers `{run_id, agent, path, status, output, steps, usage, trace_id?, data?, code?, hint?,
pending_approval?, session_id?}`. With `output=` the agent answers through a `final_answer`
tool, the runtime validates the answer (two corrections, then `failed` / `WC318`) and `data`
holds it: see [Structured output](structured-output.md). `trace_id` names the run's
[trace](tracing.md).

### `@app.behaviour(name=None, **opts)`

```python
@app.behaviour(
    "triage",
    description="",
    tools=(),
    context=(),
    scopes=(),
    max_steps=50,
    token_budget=None,
    model="default",
    max_tokens=16_000,
    temperature=None,        # sent only when set
    expose_at=None,
    expose_scopes=None,
    tool=True,               # Behaviours are agent tools by default
    options=None,            # extra provider request-body fields for ctx.ask calls
)
def triage(ctx, input): ...
```

### The `ctx` object

| Member | Signature |
|---|---|
| `ctx.call` | `(tool, **kwargs) -> Any` |
| `ctx.gather` | `(*items, return_exceptions=False) -> list` — items are `(tool, kwargs)`, a tool name, or `{"tool": ..., **kwargs}` |
| `ctx.ask` | `(prompt, *, schema=None, system=None, model=None, max_tokens=None, temperature=None) -> Any` |
| `ctx.ask_many` | `(prompts, *, schema=None, system=None, model=None, max_tokens=None, temperature=None, concurrency=8) -> list` |
| `ctx.context` | `(name) -> Any` — only declared providers |
| `ctx.run` | `(name, **kwargs) -> Any` |
| `ctx.log` | `(message) -> None` |
| `ctx.halt` | `(reason) -> NoReturn` |
| `ctx.tools` | `list[str]` |
| `ctx.contexts` | `list[str]` |
| `ctx.user` | `dict` — `{id, root_id, authenticated, scopes}` |
| `ctx.usage` | `dict` — `steps, max_steps, input_tokens, output_tokens, cache_tokens, token_budget, tree_tokens, tree_budget, depth` |
| `ctx.trace` | `list[dict]` |
| `ctx.run_id` | `str` |
| `ctx.depth` | `int` |

### `app.flow(name, **opts)`

```python
app.flow(
    "name",
    pipeline=None,           # exactly one of pipeline / parallel / route
    parallel=None,
    merge="collect",         # or "merge"
    route=None,              # {label: step}
    default=None,
    classify_with=None,      # defaults to "fast"
    classify_prompt=None,    # may use {labels} and {input}
    description="",
    scopes=(),
    expose_scopes=None,
    token_budget=None,
    expose_at=None,          # defaults to /flows/<name-with-hyphens>
    tool=True,
    input_schema=None,
)
```

A step is a tool name or `{"tool": name, "input": {...}}`; template strings
`"$"`, `"$.path"`, `"$input"`, `"$input.path"` resolve against the incoming
value and the original input.

---

## Traces and evals

### `app.tracing(*, enabled=True, keep=200, max_spans=500, otlp_endpoint=None, otlp_headers_env=None, service_name=None)`

Run traces are on without calling this. `keep` and `max_spans` bound what is held in memory;
`otlp_endpoint` exports finished spans as OTLP/HTTP JSON (`/v1/traces` is appended), with
headers from the environment variable `otlp_headers_env` names (`k=v,k2=v2`). When no endpoint
is declared, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` and `OTEL_EXPORTER_OTLP_ENDPOINT` are read.
See [Traces](tracing.md).

### `app.traces(*, url=None, key=None) -> dict` · `app.trace(trace_id="latest", *, url=None, key=None) -> dict`

The running app's recent traces, and one trace with every span, from the control plane.

### `app.eval(name, *, target, input, expect=None, check=None, scopes=(), description="")`

Declare an eval: one call to a tool and what must be true of the result. `input` is a dict of
arguments, or a string as shorthand for `{"input": ...}`. `expect` keys: `status`,
`http_status`, `code`, `output_contains`, `output_not_contains`, `output_matches`, `data`,
`tools_called`, `tools_not_called`, `path`, `max_steps`, `max_tokens`, `max_model_calls`;
an unknown key is a `ValueError`. `check` is a function of the result body. See
[Evals and cassettes](evals.md).

### `app.run_evals(*, mode="auto", directory="evals", only=None) -> dict`

Run the declared evals in-process. `mode` is `auto` (replay a cassette when there is one),
`replay`, `live` or `record`. Returns `{ok, total, passed, failed, tokens, mode, directory,
results: [{name, target, mode, ok, failures, status, code, tokens, model_calls, duration_ms,
trace_id, cassette, result}]}`.

---

## Introspection

| Method | Returns |
|---|---|
| `app.check()` | Validated report: routes, tools, agents, behaviours, flows, contexts; raises on an invalid app |
| `app.security_report()` | Public surface, gated tools, agents, behaviours, flows, memories |
| `app.context_pack()` | The app described for an AI coding tool, as Markdown |
| `app.openapi()` | OpenAPI 3.1 document |
| `app.typescript_client()` | Generated TS source |
| `app.manifest()` | The raw manifest |
| `app.schema_sql` | Generated DDL |
| `app.run()` | Boot and serve; blocks |

---

## Control plane

Mounted under `control_prefix`. Everything but `/health` needs
`webcortex:admin` once any authentication is configured.

| Endpoint | |
|---|---|
| `GET /health` | Liveness, counts, sessions, pending approvals |
| `GET /openapi.json` · `GET /tools` · `GET /routes` | The surfaces |
| `GET /agents` · `GET /behaviours` · `GET /flows` · `GET /contexts` | Declarations |
| `GET /models` | Aliases, live providers, priced models |
| `GET /usage` | The spend ledger |
| `GET /audit` | Recent events |
| `GET /status` | The live view: requests, recent errors, runs, spend, devices |
| `GET /traces` · `GET /traces/{id}` | Recent run traces, and one with every span (`latest` works as an id) |
| `GET /approvals` | Runs waiting on a human |
| `POST /approvals/{id}` | `{"approve": bool, "note": str}` — continue a run |
| `GET /security` | Public attack surface |
| `POST /mcp` | MCP JSON-RPC |

---

## Request

| Member | Type | |
|---|---|---|
| `req.method` | `str` | |
| `req.path` | `str` | |
| `req.params` | `dict` | Path parameters |
| `req.query` | `dict` | Query string; a repeated key keeps its last value |
| `req.query_list(name)` | `list[str]` | Every value of a query key, in order (`?tag=a&tag=b`) |
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
