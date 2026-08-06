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
shows up in all three surfaces.

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
                           templates, typegen, agent loop, behaviour executor
crates/webcortex-py/       PyO3 bindings. Compiles to webcortex._core
python/webcortex/          The Python package users import
  __init__.py                public exports + __version__
  app.py                     the WebCortex class — the whole declarative API
  _bridge.py                 Request / Response / HTTPError / Dispatcher
  schema.py                  type annotation → JSON Schema
  cli.py                     the `webcortex` command
  starters.py                templates used by `webcortex new`
tests/                     pytest. test_pentest.py and test_security.py are
                           adversarial and should not be weakened to pass
docs/                      MkDocs site (built with --strict; see §10)
examples/hello/            minimal app
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

## 7. Behaviours

```python
@app.behaviour(name=None, *, description="", tools=(), scopes=(), max_steps=50,
               token_budget=None, model="claude-opus-5", max_tokens=4096,
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
| `ctx.ask(prompt, schema=...)` | a model call; with a schema the model is **forced** into that shape, so branches switch on real values |
| `ctx.log(msg)` | structured log line |
| `ctx.halt(reason)` | stop the behaviour |
| `ctx.usage` | tokens and steps consumed so far |
| `ctx.trace` | the step trace |
| `ctx.user` | the calling principal |

`input` is the JSON payload the caller sent.

Behaviours are exposed as tools by default (`tool=True`), so agents can invoke
behaviours and behaviours compose with each other. Default route is
`/behaviours/<name-with-hyphens>`; override with `expose_at`.

`max_steps` caps total leaf operations and `token_budget` caps spend — **both
enforced by the runtime**, so a runaway loop costs a bounded amount. Do not
remove these bounds in generated code.

---

## 8. Agents

```python
app.agent(name, *, model, system="", tools=(), description="", max_steps=12,
          token_budget=None, expose_at=None, scopes=(), expose_scopes=None,
          temperature=1.0, max_tokens=4096) -> None
```

`tools` names routes that were declared with `tool=True`. The agent calls them
through the same dispatcher the HTTP server uses, so **a tool call is an
in-process function call, not a loopback HTTP request**, and it inherits the
route's declared scopes.

A typo in `tools` is a **boot error**, not a runtime surprise. `app.check()`
raises with the offending name.

`expose_at` adds a `POST` route taking `{"input": "..."}`. Passing
`expose_scopes=[]` makes that endpoint public; `webcortex security` reports it.

---

## 9. Introspection and output

| call | returns |
| --- | --- |
| `app.check()` | validated report: routes, tools, agents, behaviours. Raises on any invalid declaration |
| `app.manifest()` / `app.manifest_json()` | the manifest the Rust runtime executes |
| `app.openapi()` | OpenAPI document, with `x-webcortex-tool` and `x-webcortex-op` extensions |
| `app.schema_sql` | property — DDL for declared resources |
| `app.typescript_client()` | dependency-free typed TS client source |
| `app.security_report()` | public attack surface, `auth_configured` flag |
| `app.run()` | hand the manifest to Rust and serve |

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
webcortex new NAME [-t api|fullstack|agent|behaviour] [-d DIR] [--description D]
webcortex keygen
webcortex dev      [target] [--host H] [--port P] [--workers N]
webcortex run      [target] [--host H] [--port P] [--workers N]
webcortex check    [target]
webcortex openapi  [target]
webcortex tools    [target]
webcortex sql      [target]
webcortex security [target]
webcortex typegen  [target] [-o client/api.ts]
```

`target` defaults to `api.py` and accepts `module` or `module:attr`. A `.env`
file is loaded before the app is imported, for every command except `new` and
`keygen`.

`webcortex security` exits 0 even when nothing is authenticated, but prints a
warning to **stderr**. If you are scripting a gate, check
`security_report()["auth_configured"]`, not the exit code.

---

## 11. Known issue: CPython 3.14.7 with the GIL enabled

**Status: open. Read this before diagnosing any `_core` import failure.**

On CPython **3.14.7 with the GIL enabled**, importing the compiled extension
fails:

```
ValueError: module functions cannot set METH_CLASS or METH_STATIC
```

Established by controlled runs, not inference:

| interpreter | build | result |
| --- | --- | --- |
| 3.12, 3.13 | GIL-enabled | passes |
| 3.14.6 (10 Jun 2026) | GIL-enabled | passes |
| **3.14.7 (5 Aug 2026)** | **GIL-enabled** | **fails** |
| 3.14.7 (5 Aug 2026) | free-threaded (`3.14t`) | passes |

Notes for anyone picking this up:

- It is **not** intermittent, despite looking that way. `uv` resolves `3.14` to
  whatever patch build is newest when a runner starts, so the same commit changed
  verdict inside half an hour and platforms flipped one at a time as they rolled
  forward. CI now prints the interpreter as a `::notice::` annotation on every
  job for exactly this reason.
- It is **not** the pyo3 version. `crates/webcortex-py/Cargo.toml` pins pyo3
  `0.29.2` and the failure persists. The pin was a hypothesis and is now
  falsified; it can be relaxed back to `"0.29"` once the real cause is known.
- Nothing in `crates/` declares `#[staticmethod]` or `#[classmethod]`. The module
  is three `add_class` calls, an exception type, and five plain `#[pyfunction]`s.
  The flags come from generated code.
- CPython raises this when a method definition carrying `METH_CLASS` or
  `METH_STATIC` is used to build a **module-level** function.

Next steps if you are investigating: pin the CPython patch version to reproduce
deliberately, then reduce the module to a single function to find which construct
emits the flags. Check for an upstream pyo3 issue first — this is very likely
theirs, not ours.

**Symptom to recognise:** a wall of ~100 pytest fixture errors that all bottom
out in `from . import _core`. That is one binding failure, not a hundred test
failures. CI imports `_core` explicitly right after the build so this surfaces in
three seconds rather than several minutes.

---

## 12. Working on this repository

### Build a working copy

```bash
uv venv --python 3.13          # 3.12 or 3.13; avoid 3.14 until §11 is fixed
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
- **The Python matrix is 3.12, 3.13, 3.14, 3.14t** across ubuntu and macOS.
  Wheels build for 3.12, 3.13, 3.14 and 3.14t on Linux x86_64/aarch64, macOS
  x86_64/aarch64, and Windows x64.
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
