# AGENTS.md

Instructions for AI coding tools (Claude Code, Cursor, Codex, Aider, Copilot
Workspace) working **on this repository** or **writing applications that use
WebCortex**.

This is a reference, not a tutorial. It states the API surface exactly, names the
constraints the runtime enforces, and lists the mistakes that are easy to make
and expensive to debug. Prose explanation lives in `docs/`; the human entry point
is `README.md`.

Everything here is verified against the source at the commit this file ships
with. If a claim here disagrees with the code, the code is right — fix this file.

---

## 1. What WebCortex is, in one paragraph

WebCortex is a Python web framework with a Rust core. A `WebCortex` app object is
a **description**, not a server: importing `api.py` builds a JSON manifest, and
the Rust runtime executes that manifest. The consequence that matters for code
generation is that **whole categories of route never enter the interpreter at
request time** — SQL-backed routes, proxies, static responses, file serving and
server-rendered pages are all executed by Rust. Only `@app.get`-style Python
handlers, `@app.behaviour` procedures and `@app.page_handler` contexts run
Python per request.

Every route can simultaneously be a REST endpoint, an OpenAPI operation, and an
MCP tool. That is the point of the framework: you declare a thing once and it
shows up in all three surfaces. Agents, behaviours and flows are routes too,
so they are tools too, and they compose: a supervisor lists workers in
`tools=`, a front desk names specialists in `handoffs=`, a flow names any of
them as steps. One budget bounds the whole tree.

### The design rule

> If something can be declared, declare it.

Declared things are executable by Rust and readable by agents. Prefer
`app.query(...)` over a Python handler that runs the same SQL. Prefer
`app.resource(...)` over five hand-written CRUD handlers. Reach for a Python
handler when there is real logic, not as a default.

---

## 2. Repository layout

```
crates/webcortex-core/     Rust runtime: router, server, auth, db, mcp, openapi,
                           templates, typegen, agent loop (agent.rs), providers
                           and model registry (agent/), flows (flow.rs), context
                           providers (context.rs), spend ledger (ledger.rs)
crates/webcortex-py/       PyO3 bindings. Compiles to webcortex._core. The
                           behaviour ctx lives in behaviour.rs
python/webcortex/          The Python package users import
  __init__.py                public exports + __version__
  app.py                     the WebCortex class — the whole declarative API
  _bridge.py                 Request / Response / HTTPError / Dispatcher
  schema.py                  type annotation → JSON Schema
  contextpack.py             `webcortex context`: the app described for a model
  cli.py                     the `webcortex` command
  starters.py                templates used by `webcortex new`
tests/                     pytest. test_pentest.py and test_security.py are
                           adversarial and should not be weakened to pass.
                           test_orchestration.py drives the whole agent stack
                           over HTTP against the offline fake provider
docs/                      MkDocs site (built with --strict; see §10)
examples/hello/            a complete app exercising every subsystem
scout/                     a local-model reviewer for this repository (§13)
```

`_core` is a compiled extension. It does not exist until something builds it —
`maturin develop` for a working copy, or installing a wheel. Any `ImportError`
or `ValueError` from `from . import _core` means the extension is missing or
incompatible, never that the Python source is wrong. See §11.

---

## 3. The application object

```python
from webcortex import WebCortex

app = WebCortex(
    name,                          # str, positional, required
    *,
    description="",                # str
    version="0.1.0",               # str
    database=None,                 # str | None — e.g. "sqlite://./app.db"
    host="127.0.0.1",              # str
    port=8000,                     # int
    workers=None,                  # int | None — None means derive from cores
    control_prefix="/_webcortex",  # str
    templates=None,                # str | None — directory of templates
    request_timeout=30,            # int, seconds
    shutdown_timeout=25,           # int, seconds
)
```

`database` is required before `app.resource(...)`, `app.query(...)`, or
`app.page(..., sql=...)` will work. Each raises `ValueError` at declaration time
if it is missing — a boot error, not a runtime one.

---

## 4. Routes

### 4.1 Python handlers

```python
@app.route(method, path, *, tool=False, tool_name=None, read_only=None,
           idempotent=None, scopes=(), approval="never", summary="")
```

Shorthands: `@app.get`, `@app.post`, `@app.put`, `@app.patch`, `@app.delete` —
all take `(path, **kw)` with the same keywords.

Two handler signature styles:

```python
# Preferred — parameters bind by name, and the signature doubles as the
# tool schema an agent sees.
@app.get("/books/{id}", tool=True)
def get_book(id: int) -> Book: ...

# Escape hatch — the whole request. Use only when you need headers,
# raw body, or the caller's scopes.
@app.post("/webhook")
def webhook(req: Request) -> dict: ...
```

Binding rules, exactly:

- A parameter named `req` or `request`, **or** annotated `Request`, receives the
  whole `Request` and is excluded from the schema.
- Every other parameter is resolved **path → query string → JSON body**, in that
  order, and coerced to its annotation.
- Path parameters are taken from `{name}` segments in `path`.
- Parameters with defaults are optional; parameters without are required.
- The return annotation becomes the output schema.
- `inspect.getdoc(fn)` becomes the route description; its first line becomes the
  summary unless `summary=` is passed.

Defaults for the tool metadata are derived from the method, not guessed:

| keyword | default |
| --- | --- |
| `read_only` | `True` for GET, HEAD, OPTIONS |
| `idempotent` | `True` for GET, HEAD, OPTIONS, PUT, DELETE |

Override them only when the method lies about the semantics — e.g. a POST that
is genuinely read-only.

`path` **must** start with `/` and `method` must be one of GET, POST, PUT,
PATCH, DELETE, HEAD, OPTIONS. Both raise `ValueError` otherwise.

### 4.2 Declarative routes — no interpreter in the request path

```python
app.static(method, path, body, *, status=200, **kw) -> int
```
A constant response, serialised once at boot.

```python
app.query(method, path, sql, *, params=(), returns="many", **kw) -> int
```
Binds a route directly to SQL. `params` names the values bound to each `?` **in
order**; each is resolved path → query → body. `returns` is `"many"`, `"one"`,
or `"affected"` — any other value raises `ValueError`. This is the fastest kind
of route WebCortex has.

```python
app.upstream(name, base_url, *, headers=None, bearer_env=None, timeout_ms=30_000) -> None
app.proxy(method, path, *, upstream, rewrite=None, **kw) -> int
```
Declare an external API, then forward routes to it. **Credentials are named, not
embedded**: `bearer_env` holds the *name* of an environment variable, resolved at
boot. This keeps the manifest safe to log, diff, and hand to an agent. Never
inline a token here.

```python
app.static_files(path, directory, *, index=None, cache_secs=3600) -> int
```
Serves a directory. If `path` does not already end in a wildcard segment,
`/{*file}` is appended. `directory` must exist at declaration time or it raises
`ValueError`. The runtime refuses traversal, symlink escapes and dotfiles — do
not add your own checks on top, and do not weaken the tests that prove it.

All of these return an `int` route id.

### 4.3 CRUD resources

```python
app.resource(
    name, *, table=None, fields, primary_key="id", tools=False,
    scopes=(), read_scopes=None, write_scopes=None, create_table=True,
) -> Resource
```

Generates the full five-route CRUD surface (list, get, create, update, delete),
executed entirely in Rust. With `tools=True` those five become five agent tools.

Authorization is deliberately split, because reads and writes rarely warrant the
same scope:

- `read_scopes` guards list and get
- `write_scopes` guards create, update and delete
- `scopes` sets both at once

Leaving all three unset makes the resource **fully public**. That is legal, and
`webcortex security` reports it. Do not silently leave it unset in generated
code — either set scopes or say in a comment that public is intended.

`primary_key` must appear in `fields`, else `ValueError`.

---

## 5. Security

Every setting below defaults to the safe value. **Relaxing costs a line of code;
tightening does not.** Preserve that property in any change.

```python
app.api_key(env_var, *, id, scopes=())
```
Accepts an API key read from `env_var`. The secret never appears in source or in
the manifest — only the variable name. The runtime stores a SHA-256 and compares
in constant time.

```python
app.jwt(*, secret_env, algorithm="HS256", audience=None, issuer=None, leeway_secs=30)
```
Bearer JWTs verified with the secret in `secret_env`. Scopes come from the
standard `scope` (space-delimited) or `scopes` (array) claims. `exp` is
**required and always validated**. `leeway_secs` is stated explicitly rather than
inherited because the underlying library defaults to 60s, and an expired token
staying valid for another minute should be a decision.

```python
app.anonymous_scopes(*scopes)
```
Grants scopes to callers with no credential. Empty by default. This is the single
most common way a route someone believed was protected becomes public.

```python
app.cors(*origins, credentials=False, methods=None, headers=None, expose=(), max_age=600)
```
Explicit origins only. `credentials=True` together with a `"*"` origin is
**rejected at boot** — the combination is forbidden by the CORS spec.

```python
app.rate_limit(per_second=50.0, *, burst=100)
```
Token bucket, keyed per principal (IP when anonymous).

```python
app.security_headers(*, enabled=True, frame_options="DENY",
                     referrer_policy="strict-origin-when-cross-origin",
                     content_security_policy=None, hsts_max_age=31_536_000)
```
Already sensible without calling this. Call it to tune, not to enable.

### Scope model

- Route `scopes=` is what a caller must hold to reach the route.
- Agent `scopes=` is what the agent may **use**; `expose_scopes=` is who may
  **start** a run. They default to the same set.
- Behaviour tool calls run under a **delegated principal** and cannot exceed the
  caller's scopes. `tests/test_pentest.py` proves this; do not regress it.

---

## 6. Pages and templates

```python
app.page(path, template, *, sql=None, params=(), returns="many", bind="data",
         data=None, status=200, scopes=(), method="GET") -> int
```

Data comes from **exactly one** declared source and is resolved *before*
rendering: `sql=` for a query, `data=` for a constant, or neither. Passing both
raises `ValueError`. `bind` names the template variable the rows land in.

The template has no handle to the database and no way to call Python. That
constraint is deliberate — it stops this layer becoming a second, worse view
layer. If a page needs real logic:

```python
@app.page_handler(path, template, *, status=200, scopes=())
def ctx(...) -> dict: ...
```

Template output is HTML-escaped. `tests/test_fullstack.py` asserts this.

---

## 6a. Models, context providers, memory

```python
app.models(**aliases)                      # e.g. default=..., fast=..., local="ollama/qwen3.5:9b"
app.provider(name, *, base_url, kind="openai", api_key_env=None)
app.pricing(model, *, input_per_mtok, output_per_mtok,
            cache_read_per_mtok=0.0, cache_write_per_mtok=0.0)
```

A model name resolves aliases first (`fast` → concrete name; `default` and
`fast` are built in), then picks a provider by prefix: `ollama/…` (built in,
no key, `OLLAMA_HOST`), `openai/…` or `gpt-*` (`OPENAI_API_KEY`),
`anthropic/…` or bare `claude-*` (`ANTHROPIC_API_KEY`), `<declared>/…`. Two
wire formats exist — Anthropic Messages and OpenAI Chat Completions — and
`kind` selects one for a declared provider. **Do not add a third abstraction
layer.** The spend ledger groups by the resolved name, so `fast` shows up as
whatever it pointed at.

Prices are never built in. `GET /_webcortex/usage` reports
`estimated_cost_usd: null` when any model that spent tokens is unpriced.

```python
app.context(name, *, sql=None, params=(), returns="many", data=None,
            description="", max_chars=4000)        # declarative, or a decorator
```

Exactly one of `sql=` / `data=` / decorated function. Resolved at run start
for agents (`context=[...]`, injected into the system prompt as
`<context name="…">…</context>` blocks) and on demand for behaviours
(`ctx.context(name)`, only for names in the behaviour's `context=[...]`).
SQL providers may bind `@principal` and keys of the run's input
(`{"input": "..."}` for agents; the payload for behaviours). A Python provider
is a normal handler called with a synthetic POST whose body is that input.
`max_chars` is a hard cap and is re-sent every step — keep it small.

```python
app.memory(name="memory", *, scopes=(), read_scopes=None, write_scopes=None,
           max_results=10) -> list[str]
```

Creates table `wcx_<name>` (DDL via `app.schema_sql`; applied by `app.run()`
for SQLite) and four **Query** routes exposed as tools:
`<name>_remember(key, value)` (upsert), `<name>_recall(key)` (404 when
absent), `<name>_search(query, limit)` (substring, newest first),
`<name>_forget(key)`. Every row is keyed by `@principal`, the **root**
principal — the human or service behind any chain of agent delegation —
enforced in the SQL, not by the model. Search is substring on purpose; do not
add embeddings here.

### `@principal`

`WebCortexRequest::lookup("@principal")` returns `principal.root_id()`. A
delegated principal carries `claims.root`, set on the first delegation and
preserved through every subsequent one. Use it in `app.query(params=[...])`
and `app.context(params=[...])` to scope data to the caller without a Python
handler.

---

## 7. Behaviours

```python
@app.behaviour(name=None, *, description="", tools=(), context=(), scopes=(),
               max_steps=50, token_budget=None, model="default", max_tokens=4096,
               temperature=1.0, expose_at=None, expose_scopes=None, tool=True)
def handler(ctx, input): ...
```

Both `@app.behaviour` and `@app.behaviour("name")` work; bare usage takes the
function name.

**A Behaviour is the answer to "a prompt-as-skill is only a suggestion."** The
loops and branches are Python that always runs; only the leaves are
probabilistic.

```python
@app.behaviour("triage", tools=["list_tickets", "update_tickets"])
def triage(ctx, input):
    tickets = ctx.call("list_tickets", status="open")
    urgent = 0
    for ticket in tickets:                       # a real loop
        verdict = ctx.ask(                       # a model call
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
        if verdict["urgency"] > 7:                # a real branch
            urgent += 1
            ctx.call("page_oncall", ticket=ticket["id"])
        ctx.call("update_tickets", id=ticket["id"], category=verdict["category"])
    return {"triaged": len(tickets), "urgent": urgent}
```

The `ctx` surface:

| call | meaning |
| --- | --- |
| `ctx.call(tool, **kwargs)` | invoke one of the app's tools, in-process, under the delegated principal |
| `ctx.gather(*items, return_exceptions=False)` | several tool calls concurrently; items are `(tool, kwargs)`, a name, or `{"tool": …, **kwargs}`; one step each; admitted/scoped/gated exactly like `call` |
| `ctx.ask(prompt, *, schema=None, system=None, model=None, max_tokens=None, temperature=None)` | a model call; with a schema the model is **forced** into that shape (tool_choice), and JSON is recovered from prose if a local model answers in text; `model` may be an alias |
| `ctx.ask_many(prompts, *, schema=None, ..., concurrency=8)` | concurrent `ask`; answers in order; one step per prompt |
| `ctx.context(name)` | a declared context provider's raw value; `PermissionError` if not declared on the behaviour |
| `ctx.log(msg)` | structured log line |
| `ctx.halt(reason)` | stop the behaviour |
| `ctx.usage` | `steps, max_steps, input_tokens, output_tokens, cache_tokens, token_budget, tree_tokens, tree_budget, depth` |
| `ctx.trace` | the step trace |
| `ctx.user` | `{id, root_id, authenticated, scopes}` |
| `ctx.tools` / `ctx.contexts` | what the behaviour may call / read |

`input` is the JSON payload the caller sent.

Behaviours are exposed as tools by default (`tool=True`), so agents can invoke
behaviours and behaviours compose with each other. Default route is
`/behaviours/<name-with-hyphens>`; override with `expose_at`.

`max_steps` caps total leaf operations and `token_budget` caps spend — **both
enforced by the runtime**, so a runaway loop costs a bounded amount. Tokens are
also charged to the request tree's shared budget (§8a), so a behaviour started
by an agent cannot outspend the agent. Do not remove these bounds in generated
code.

Behaviours run on the sync thread pool and block on `Handle::block_on`;
`gather`/`ask_many` block once on a `join_all`. Never run a behaviour on an
event loop.

---

## 8. Agents

```python
app.agent(name, *, model="default", system="", tools=(), handoffs=(), context=(),
          memory=None, description="", max_steps=12, token_budget=None,
          expose_at=None, scopes=(), expose_scopes=None, temperature=1.0,
          max_tokens=4096, cache=True, context_window=None,
          tool_result_limit=16_384, compact_with=None, keep_recent=6,
          tool=True) -> None
```

`tools` names routes that were declared with `tool=True` — including other
agents, behaviours and flows. The agent calls them through the same dispatcher
the HTTP server uses, so **a tool call is an in-process function call, not a
loopback HTTP request**, and it inherits the route's declared scopes.

**Every agent is a route and a tool.** It is mounted at `expose_at` (default
`/agents/<name-with-hyphens>`) with tool name `<name>`, guarded by
`expose_scopes` (default `scopes`). The route takes
`{"input": str, "session_id"?: str, "reset"?: bool}`. This is a change from
0.3, where an agent without `expose_at` had no route: an agent declared with
`scopes=()` is now a public route, and `webcortex security` reports it.

`handoffs` names agents; each becomes a `transfer_to_<name>` tool with the
target's `description`. On a handoff the active `AgentDef` is swapped, the
conversation and budget carry over, and the actor is re-delegated from the
original caller then filtered by the previous actor's scopes. Limits
(`max_steps`, `token_budget`, `policy`) are always the **entry** agent's.
`path` in the result lists every agent visited.

`memory="notes"` appends the four memory tools and a one-sentence usage hint
to `system`. `context=[...]` names providers (§6a).

`cache=True` sends Anthropic `cache_control` breakpoints on the system block
and the last tool. `tool_result_limit` bounds what the model sees of a tool
result (the step record keeps the full value). `context_window` compares
against the **measured** input tokens of the last call; when exceeded, turns
before the last `keep_recent` are summarised with `compact_with` (default
`fast`) and replaced by one user message. The cut always lands on an
assistant message so `tool_use`/`tool_result` pairs stay together. A failed
compaction is a `compaction` step with `error` set, naming the model; the run
continues uncompacted.

A typo in `tools`, `handoffs` or `context` is a **boot error** with a "did you
mean" hint. An agent that lists itself in `handoffs` is rejected, and so is one
whose `tools` include `transfer_to_<h>` for one of its own handoffs `h`.

Result shape: `run_id, agent, path, status, output, steps, usage,
pending_approval?, session_id?`. `status` ∈ `completed | step_limit |
budget_exhausted | awaiting_approval | failed`. Step kinds: `model, tool_call,
tool_refused, approval_requested, approval_denied, handoff, compaction`.
HTTP status: 202 for `awaiting_approval`, 502 for `failed`, 200 otherwise.

### 8a. The shared budget, sessions, approvals

`WebCortexRequest.budget: Option<Arc<SharedBudget>>` travels with every
in-process call (`App::call_tool_in_tree`). The outermost agent, behaviour or
flow creates it from its own `token_budget`; nested runs charge the same
counter and stop with `budget_exhausted` when it is spent. `usage.tree_tokens`
reports the total. A nested agent's or behaviour's own `token_budget` still
caps the model calls it makes itself, but sets no smaller ceiling for what it
starts; a nested flow's `token_budget` is not consulted. Steps stay per-run. **Any new op that can re-enter the
dispatcher must pass `depth + 1` and the budget through**; the 0.3 agent loop
reset depth to zero on tool calls, which bypassed the nesting ceiling.

Sessions: `SessionStore` keyed by `(agent, principal.id, session_id)`,
in-memory, `server.session_capacity` (1000) / `session_ttl_secs` (3600),
LRU-evicted. Saved after any terminal status except `awaiting_approval`.

Approvals: a gated call suspends the run into `App.approvals` with the whole
`RunState`, the turn's tool calls, the index of the gated one, and the results
already produced. `POST /_webcortex/approvals/{id}` with
`{"approve": bool, "note": str}` (admin scope) executes or refuses the gated
call, runs the **rest of the turn**, and continues the loop. A decision is
consumed; expiry is `server.approval_ttl_secs` (3600). Denial pushes a
`tool_result` error carrying the note.

### 8b. Flows

```python
app.flow(name, *, pipeline=None, parallel=None, merge="collect", route=None,
         default=None, classify_with=None, classify_prompt=None, description="",
         scopes=(), expose_scopes=None, token_budget=None, expose_at=None,
         tool=True, input_schema=None) -> None
```

Exactly one of `pipeline` / `parallel` / `route`. A step is a tool name or
`{"tool": name, "input": template}`; every step must be an exposed tool, and a
flow may not contain itself. Executed in Rust (`flow.rs`) under a delegated
principal, `depth + 1`, and one shared budget. Mounted at `/flows/<name>` as a
tool named `<name>`.

Argument rules without a template: agent target → `{"input": <previous>}`
(an object with an `input` key passes through; anything else is stringified);
other targets → an object passes through, a scalar becomes `{"input": …}`.
Agent and flow results are envelopes; the next step receives `output`. A step
whose run is `awaiting_approval` or `failed` fails the flow. Templates:
`"$"` incoming, `"$.a.b"` path, `"$input"` original, `"$input.a"` path.

Route flows classify with a forced `choose` tool whose schema enumerates the
labels; an unknown label falls to `default`, else the flow fails naming the
label. The classifier is charged to the ledger as `flow:<name>`.

Result: `{flow, run_id, status: completed|budget_exhausted, output, steps:
[{tool, duration_ms, ok, error?, label?}], usage: {tree_tokens}, duration_ms}`.

---

## 9. Introspection and output

| call | returns |
| --- | --- |
| `app.check()` | validated report: routes, tools, agents, behaviours, flows, contexts. Raises on any invalid declaration |
| `app.manifest()` / `app.manifest_json()` | the manifest the Rust runtime executes |
| `app.openapi()` | OpenAPI document, with `x-webcortex-tool` and `x-webcortex-op` extensions |
| `app.schema_sql` | property — DDL for declared resources and memories |
| `app.typescript_client()` | dependency-free typed TS client source |
| `app.security_report()` | public attack surface, `auth_configured` flag, agents, behaviours, flows, memories |
| `app.context_pack()` | the app described for a coding model (Markdown); `webcortex context` |
| `app.run()` | hand the manifest to Rust and serve |

Control plane (admin scope once auth is configured): `/health`,
`/openapi.json`, `/tools`, `/routes`, `/agents`, `/behaviours`, `/flows`,
`/contexts`, `/models`, `/usage`, `/audit`, `/approvals`,
`POST /approvals/{id}`, `/security`, `POST /mcp`.

`check()`, `openapi()`, `typescript_client()` and `security_report()` all
`from . import _core`. They are the first things to fail when the extension is
missing — which is exactly the failure described in §11.

### Type annotation → JSON Schema

`webcortex.schema.json_schema_for` handles: `str`, `int`, `float`, `bool`,
`bytes` (base64), `datetime.date`, `datetime.datetime`, `None`, `Any`,
`Optional[X]` / `X | None` (as `nullable`), `Union` (as `anyOf`), `list`, `set`,
`frozenset`, `tuple`, `dict`, `Literal` (as `enum`), and dataclasses.

**Unknown types degrade to `{}` — accept anything — rather than raising.** A
handler with an exotic signature still serves traffic; it just advertises a less
precise tool schema. Do not "fix" this by making it raise.

---

## 10. The CLI

```
webcortex new NAME [-t api|fullstack|agent|behaviour|orchestration] [-d DIR] [--description D]
webcortex keygen
webcortex dev      [target] [--host H] [--port P] [--workers N]
webcortex run      [target] [--host H] [--port P] [--workers N]
webcortex check    [target]
webcortex openapi  [target]
webcortex tools    [target]
webcortex sql      [target]
webcortex security [target]
webcortex typegen  [target] [-o client/api.ts]
webcortex context  [target] [--json]
webcortex evolve   REQUEST [target] [-m MODEL] [-o FILE] [--json]
```

`evolve` calls `_core.ask_model(manifest_json, model, system, prompt, schema,
max_tokens)`, which builds a `ProviderRegistry` from the app's `models`
config and the environment, so aliases and declared providers work on the
command line. It prints a proposal and never edits files.

`target` defaults to `api.py` and accepts `module` or `module:attr`. A `.env`
file is loaded before the app is imported, for every command except `new` and
`keygen`.

`webcortex security` exits 0 even when nothing is authenticated, but prints a
warning to **stderr**. If you are scripting a gate, check
`security_report()["auth_configured"]`, not the exit code.

---

## 11. Resolved: the CPython 3.14 import failure

**Status: fixed in 0.3.2.** 3.12, 3.13 and 3.14 — GIL-enabled and free-threaded
— are all supported, tested and shipped. Kept here because the *reason* it took
a day to find is a trap worth not repeating.

On CPython **3.14.7 with the GIL enabled**, importing the compiled extension
fails:

```
ValueError: module functions cannot set METH_CLASS or METH_STATIC
```

Established by controlled runs, not inference:

| interpreter | pyo3 0.29.1 | pyo3 0.29.2 |
| --- | --- | --- |
| 3.12, 3.13 | passes | passes |
| 3.14.6, GIL-enabled | passes | passes |
| **3.14.7, GIL-enabled** | **fails** | **passes** |
| 3.14.7 free-threaded (`3.14t`) | passes | passes |

### The cause

pyo3 **0.29.1**, fixed upstream in **0.29.2**.

### Why that took a day to establish

`crates/webcortex-py/Cargo.toml` was changed to require `0.29.2` early on. The
failure continued, so the fix looked falsified — and on that basis 3.14 was
dropped from the matrix and the wheel set entirely.

It had never been built. `Cargo.lock` still pinned `0.29.1`. Cargo resolves the
newer version at build time, so any *fresh* build was correct, but
`Swatinem/rust-cache` keys its cache off `Cargo.lock` — which had not changed —
so every CI job restored objects compiled against the broken version.

The tell was available and went unread for hours: a dispatch-only probe on the
same commit and the same 3.14.7 build imported cleanly, and its only material
difference from the failing job was a cold cache.

### The second cause, found while shipping 2.0

The same error came back on the macOS 3.14 job of the 2.0.0 branch, with
pyo3 0.29.2 correctly pinned and Homebrew's CPython 3.14.7 — a combination
the table above says passes. The log showed the tell: the job compiled only
`webcortex-core` and `webcortex-py`; pyo3 came from a cache whose key was
**identical to the one the passing 3.14t job restored**.

`Swatinem/rust-cache` keys on the job id, and every entry of the Python
matrix has the id `python`, so 3.12, 3.13, 3.14 and 3.14t on one OS shared
one cache. `pyo3-ffi`'s build script configures itself against the interpreter
it is built with (`Py_GIL_DISABLED` among other things), and cargo's
fingerprint does not include the interpreter. A GIL-enabled 3.14 job that
restored objects built by the free-threaded job therefore imported a module
compiled for the wrong ABI — and whichever matrix job won the race to save
the cache decided whether the *next* run passed. That is what "passed at
09:54 and failed at 10:17" was.

The cache key now includes `matrix.python`. The pyo3 0.29.1 → 0.29.2 upgrade
was still correct; it was just not the whole story.

### What now prevents a repeat

- `Cargo.lock` pins 0.29.2 explicitly.
- The rust-cache key hashes `Cargo.toml` as well as `Cargo.lock`, so a manifest
  change invalidates the cache even when the lock lags behind it.
- The rust-cache key includes the matrix interpreter, so builds for different
  ABIs never share compiled objects.
- CI prints the resolved interpreter as a `::notice::` on every job, and raises
  `_core` import failures as `::error::` annotations.


**If you are ever debugging a dependency fix that appears not to work: build once
with a cold cache before concluding anything.** A cache that silently serves
objects from before your change will falsify a correct hypothesis.

**Symptom to recognise:** a wall of ~100 pytest fixture errors that all bottom
out in `from . import _core`. That is one binding failure, not a hundred test
failures. CI imports `_core` explicitly right after the build so this surfaces in
three seconds rather than several minutes.

---

## 12. Working on this repository

### Build a working copy

```bash
uv venv --python 3.14t         # or 3.12 / 3.13 / 3.14
uv pip install maturin pytest
source .venv/bin/activate
maturin develop --uv
pytest tests/ -q
```

`maturin develop` is what produces `_core`. Nothing works before it.

### Rust side

```bash
cargo test -p webcortex-core --all-features
cargo clippy -p webcortex-core --all-targets -- -D warnings
cargo audit
```

Clippy warnings are errors in CI. A warning nobody fails on is a warning nobody
reads.

### Rules that CI enforces, so you may as well know them up front

- **Docs build with `--strict`.** Every page listed in `mkdocs.yml` nav must
  exist, and every internal link must resolve. Adding a nav entry without the
  file turns a warning into a build failure. Relative links out of `docs/` (e.g.
  `../examples/`) do not resolve on the published site — use absolute GitHub
  URLs.
- **The Python matrix is 3.12, 3.13, 3.14, 3.14t** across ubuntu and macOS, and
  wheels build for the same four on Linux x86_64/aarch64, macOS x86_64/aarch64,
  and Windows x64. Change those two lists together: a tested target that ships no
  wheel, or a shipped wheel nothing tests, is worse than either.
- **PyO3 is used without `abi3`**, because the free-threaded builds expose a
  distinct ABI that cannot be combined with the stable-ABI feature. That is why
  there is one wheel per interpreter version rather than one abi3 wheel.
- **Linux aarch64 builds natively** on `ubuntu-24.04-arm` rather than
  cross-compiling: `ring`'s ARM assembly fails to cross-build with "ARM assembler
  must define `__ARM_ARCH`".
- **Intel macOS is cross-compiled** from the ARM runner, since GitHub retired the
  macos-13 Intel runners.
- **Releases publish via PyPI Trusted Publishing (OIDC)**, environment `pypi`.
  There is no API token to find, and none should be added.

### Things not to do

- Do not weaken `tests/test_pentest.py` or `tests/test_security.py` to make a
  build pass. They encode the security guarantees the README makes.
- Do not embed a credential anywhere in a manifest, an example, or a test
  fixture. Use `*_env` parameters that name a variable.
- Do not add a smoke check that imports `webcortex` without touching `_core`.
  `webcortex.free_threaded()` lives in `_bridge.py` and only reads
  `sys._is_gil_enabled`, so it passes cleanly on a build whose native module
  cannot load at all. That false negative cost several hours; the explicit
  `from webcortex import _core` in `ci.yml` exists to prevent a repeat.
- Do not make `json_schema_for` raise on unknown types (§9).
- Do not remove `max_steps` / `token_budget` bounds from behaviours (§7).
- Do not add a route kind that re-enters the dispatcher without passing
  `depth + 1` and the shared budget (§8a).
- Do not build in model prices (§6a), a third provider abstraction, an ORM, or
  a vector store. Memory search is substring on purpose.
- Do not let a session or approval store grow unbounded; both are capped and
  expire, and both are documented as ephemeral.
- Do not make the control plane's new endpoints (`/usage`, `/approvals`,
  `/models`) reachable without `webcortex:admin` once auth is configured.
- Do not select `FakeProvider` by anything other than
  `WEBCORTEX_FAKE_PROVIDER`; it is a test double and logs a warning when on.

---

## 13. The fake provider and the scout

`WEBCORTEX_FAKE_PROVIDER=1` replaces every model call with `FakeProvider`
(`agent/provider.rs`), which answers from a tiny command language in the last
user message: `tool:<name> <json>`, `handoff:<agent>`, `json:<object>` (for a
forced tool), `context?` (echoes the system suffix), anything else is echoed.
After tool results it summarises them and stops. Input tokens scale with the
conversation, so compaction is observable. `tests/test_orchestration.py` is
built on it; extend the DSL rather than mocking around it.

`scout/scout.py` is a stdlib-only script that runs a local Ollama model over
recent commits and a rotating focus area and appends structured suggestions to
`scout/suggestions.md` (git-ignored). `scout/README.md` explains the loop:
the scout proposes, a human or a coding agent decides. It is not part of the
package and must not be imported by it.

