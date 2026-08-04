# Pylon

A Python web framework with a Rust core, built on one idea:

> **If you declared it, Rust can run it — and an agent can call it.**

Django and Rails were designed when the only client was a browser. Today the
client is just as likely to be a model. Pylon treats that as the primary case
rather than something you bolt on with a second, hand-maintained tool server.

```python
# api.py
from pylon import Pylon

app = Pylon("bookstore", database="sqlite://./app.db")

app.api_key("PYLON_API_KEY", id="service", scopes=["read", "write"])
app.rate_limit(per_second=50)
app.anonymous_scopes("read")

app.resource(
    "books",
    fields={"id": int, "title": str, "author": str, "year": int},
    tools=True,
    read_scopes=["read"],
    write_scopes=["write"],
)

@app.get("/books/{id}/blurb", tool=True, scopes=["read"])
def blurb(id: int) -> str:
    """One-line pitch for a book."""
    return f"Book {id} — highly recommended."
```

```
$ pylon dev
```

You now have a REST API, an OpenAPI 3.1 document, **a live MCP server exposing
all six endpoints as tools**, authentication, rate limiting, and security
headers. No second file, no schema written twice, no drift.

---

## Start here

```bash
pip install pylon              # or: uv pip install pylon
pylon new myapp                # --template api | fullstack | agent | behaviour
cd myapp
export PYLON_API_KEY=$(pylon keygen)
pylon dev
```

## Why a Rust core, specifically

Most Rust-accelerated Python servers put Rust at the socket and call Python for
every request. You get faster parsing; your handler is still interpreted.

Pylon puts the boundary somewhere more useful. **Python is a declaration
language that compiles to a plan the Rust runtime executes.** A route whose work
is expressible as data — a query, a proxy, a rendered page, a static file, an
agent invocation — runs entirely in Rust and *never enters the interpreter at
request time*. In practice that is most of a CRUD API.

```
$ pylon check
  12 routes, 9 served without touching Python
```

When a route genuinely needs Python, it crosses onto a pool of free-threaded
interpreter workers (CPython 3.13+/3.14, GIL disabled), each running its own
event loop. Handlers run in real parallel — **measured at 4.82× vs 1.38× under
the GIL** ([DESIGN.md](DESIGN.md) has the numbers and their caveats).

Pylon runs correctly on a GIL build too, and tells you which mode it is in.

## Behaviours

A "skill" written as a prompt is a *suggestion*. The model reads it and may
ignore it, and "if X then Y" fails silently when it does.

A **Behaviour** inverts that. The control flow is real Python — a `for` loop is
a loop, an `if` is a branch, and both execute whether or not a model would have
chosen to. Only the *leaves* are probabilistic:

```python
@app.behaviour("triage", tools=["list_tickets", "update_tickets"],
               max_steps=100, token_budget=100_000)
def triage(ctx, input):
    """Classify every open ticket and escalate the urgent ones."""
    tickets = ctx.call("list_tickets", limit=50)
    escalated = []

    for ticket in tickets:                        # a real loop
        verdict = ctx.ask(                        # a model call
            f"Grade this ticket:\n{ticket['body']}",
            schema={
                "type": "object",
                "properties": {
                    "urgency": {"type": "integer"},
                    "category": {"enum": ["bug", "billing", "other"]},
                },
                "required": ["urgency", "category"],
            },
        )
        if verdict["urgency"] >= input.get("threshold", 7):   # a real branch
            escalated.append(ticket["id"])
        ctx.call("update_tickets", id=ticket["id"], body=ticket["body"],
                 urgency=verdict["urgency"],
                 state="escalated" if ticket["id"] in escalated else "triaged")

    return {"escalated": escalated, "usage": ctx.usage}
```

You get a procedure with **deterministic structure and probabilistic steps**,
rather than a probabilistic procedure.

`ctx` is how a behaviour reaches the world:

| | |
|---|---|
| `ctx.call(tool, **kwargs)` | invoke one of the app's tools, in-process |
| `ctx.ask(prompt, schema=...)` | a model call; a schema **forces** the shape, so branches switch on real values |
| `ctx.log(msg)` / `ctx.halt(reason)` | narrate or stop deliberately |
| `ctx.usage` / `ctx.trace` | budget consumed and every leaf executed, readable mid-run |
| `ctx.user` / `ctx.tools` | the delegated principal and what it may call |

**A behaviour is a tool.** It registers as a route, so it is automatically an
MCP tool, an OpenAPI operation, and something an agent — or another behaviour —
can call. Composition is just a tool call, so budgets and scopes still apply.

**The runtime enforces the limits, not your diligence:**

- `max_steps` caps total leaf operations. A loop that would run 100 times with
  `max_steps=3` stops at 3 and returns `{"halted": true, "reason": ...}`.
- `token_budget` caps spend.
- A behaviour cannot call a tool it did not declare.
- An approval-gated tool cannot be laundered through a behaviour — the gate
  halts the run, exactly as it halts an agent.
- Scopes are delegated by intersection, never unioned.

```bash
pylon new myapp --template behaviour
```

## The seven kinds of route

| Kind | Declared with | Runs in |
|---|---|---|
| Static | `app.static(...)` | Rust |
| Query | `app.query(...)`, `app.resource(...)` | Rust |
| Page | `app.page(...)` | Rust (minijinja) |
| Files | `app.static_files(...)` | Rust |
| Proxy | `app.proxy(...)` | Rust |
| Agent | `app.agent(..., expose_at=...)` | Rust |
| Behaviour | `@app.behaviour(...)` | Python worker pool |
| Python | `@app.get(...)` | Python worker pool |

## Every route is a tool

Mark a route `tool=True` and it appears in MCP `tools/list`, with an input
schema derived from the handler's own type hints. Agents declared in the same
app call those tools **in-process** — a function call through the same
dispatcher the HTTP server uses, not a loopback request.

```python
app.agent(
    "librarian",
    model="claude-opus-5",
    tools=["list_books", "get_books"],
    scopes=["read"],          # what a run may do
    expose_scopes=["read"],   # who may start one
    max_steps=8,
    token_budget=50_000,
    expose_at="/ask",
)
```

A typo in `tools` fails at boot with a "did you mean" suggestion.

## What makes agents safe to deploy

These are enforced by the runtime, not by your diligence:

**Delegated authority.** An agent run executes as
`caller.delegate_to_agent(...)`, whose scopes are *intersected* with the
caller's — never unioned. An anonymous caller cannot launch a privileged agent.
This is the confused-deputy defence, and it is a property of the type, not a
convention.

**Human approval gates.** Mark a route `approval="required"` and an agent asking
for it does not get it — the run suspends and records an approval request. The
gate also applies to direct MCP calls, so it cannot be stepped around.

```python
@app.delete("/books/all", tool=True, scopes=["write"], approval="required")
def clear_catalogue(confirm: bool = False) -> dict: ...
```

**Runtime-enforced budgets.** `max_steps` and `token_budget` are checked before
each provider call. A looping model costs a bounded amount.

**Scope-filtered tool lists.** `tools/list` shows only what *that caller* can
invoke. A reader sees three tools where an admin sees six.

**A full audit trail**, including refused calls, at `GET /_pylon/audit`.

## Security defaults

Deny-by-default throughout; relaxing something costs a line, tightening it costs
nothing. API keys are referenced by environment variable, hashed with SHA-256,
and compared in constant time. JWT (HS/RS) with mandatory expiry validation.
Per-principal token-bucket rate limiting. Security headers on every response.
CORS that refuses `*` with credentials *at boot*. Path traversal, symlink
escapes, and dotfiles refused by the static server.

`pylon security` prints exactly what is reachable without a credential:

```
$ pylon security
{"auth_configured": true, "public_routes": ["GET /"], "gated_tools": ["DELETE /books/all"]}
```

## The frontend, without the mess

Two clean paths sharing one data layer, chosen per route:

**Server-rendered pages**, executed in Rust:

```python
app.page("/", "index.html", sql="SELECT * FROM books LIMIT 20", bind="books")
app.static_files("/assets", "static")
```

The rule that keeps this clean: **a template receives a data object and nothing
else.** It has no database handle and cannot call Python, so it cannot grow
logic. Data is resolved *before* rendering, from a constant, a query, or a
Python handler. Autoescaping is on and derived from the file extension.

**A typed TypeScript client** for SPA frontends, from the same route table:

```bash
$ pylon typegen          # writes client/api.ts
```

Zero dependencies, `fetch`-based, with auth built in. Pages and static mounts
are excluded — they are not part of the JSON API surface.

## Commands

```
pylon new <name>      scaffold a project (api | fullstack | agent | behaviour)
pylon dev             run with a startup report
pylon check           routes, tools, and the public attack surface
pylon security        what is reachable without a credential
pylon tools           the agent tool manifest
pylon typegen         generate a typed TypeScript client
pylon openapi         the OpenAPI 3.1 document
pylon sql             DDL for declared resources
pylon keygen          mint an API key
```

## Security

Pylon has been through an adversarial review of its own controls — auth,
authorization, injection, traversal, SSRF, exhaustion, disclosure — plus stress
and soak testing. **Six issues were found and fixed**, each with a regression
test in `tests/test_pentest.py`:

| | Severity |
|---|---|
| Remote DoS + total auth failure via a JWT library panic | Critical |
| No panic boundary on the request path | High |
| Proxy path traversal usable as an SSRF primitive | High |
| Unbounded behaviour recursion exhausting the worker pool | High |
| Python tracebacks returned to clients | Medium |
| Client input faults reported as 500s | Low |

Soak: **1,786,805 requests, 0 errors, 0 panics**, memory at steady state.

[`SECURITY.md`](SECURITY.md) has the full report — including what was *not*
tested and the known limits.

## Status

v0.3. Working and tested: the manifest IR, router, native ops (static / query /
proxy / page / files), the free-threaded Python bridge, authentication and
scopes, rate limiting, CORS, security headers, graceful shutdown, Behaviours,
the agent runtime with approval gates and budgets, the audit trail, OpenAPI, the
MCP server, and TypeScript generation. **251 tests** (66 Rust, 185 Python,
including a 54-test adversarial suite), clippy clean.

Not yet: Postgres, SSE streaming, durable agent runs, local model supervision.
See [DESIGN.md](DESIGN.md) for the roadmap, honest risk grading, and — just as
importantly — what is deliberately **not** being built.

## Building from source

```bash
uv venv --python 3.14.4+freethreaded
uv pip install maturin
.venv/bin/maturin develop --uv
.venv/bin/python -m pytest tests/
```

## License

Apache-2.0
