# WebCortex

A Python web framework with a Rust core, built on one idea:

!!! quote ""
    **If you declared it, Rust can run it — and an agent can call it.**

Django and Rails were designed when the only client was a browser. Today the
client is just as likely to be a model. WebCortex treats that as the primary
case rather than something you bolt on with a second, hand-maintained tool
server.

```python title="api.py"
from webcortex import WebCortex

app = WebCortex("bookstore", database="sqlite://./app.db")

app.api_key("WEBCORTEX_API_KEY", id="service", scopes=["read", "write"])

app.resource(
    "books",
    fields={"id": int, "title": str, "author": str, "year": int},
    tools=True,
    read_scopes=["read"],
    write_scopes=["write"],
)
```

```console
$ webcortex dev
```

From those few lines you get:

- a REST API on `:8000`
- an OpenAPI 3.1 document at `/_webcortex/openapi.json`
- **a live MCP server at `/_webcortex/mcp` exposing all five endpoints as tools**
- a typed TypeScript client, on demand, via `webcortex typegen`

No second file, no schema written twice, no drift.

---

## Why a Rust core, specifically

Most Rust-accelerated Python servers put Rust at the socket and call Python for
every request. You get faster parsing; your handler is still interpreted.

WebCortex puts the boundary somewhere more useful. **Python is a declaration
language that compiles to a plan the Rust runtime executes.** A route whose work
is expressible as data — a query, a proxy, a static response, a rendered page, an
agent invocation — is executed entirely in Rust and *never enters the interpreter
at request time*.

In practice, most of a CRUD API is exactly that kind of route.

```console
$ webcortex check
  12 routes, 11 served without touching Python
```

When a route genuinely needs Python, it crosses onto a pool of free-threaded
interpreter workers (CPython 3.13+ with the GIL disabled), each running its own
event loop. Handlers run in real parallel — which was not possible when Django's
execution model was designed.

[How it works :material-arrow-right:](concepts.md){ .md-button }

---

## The three things that are actually different

### 1. Every route is a tool

Mark a route `tool=True` and it appears in `tools/list` over MCP, with an input
schema derived from the handler's own type hints. The route *is* the tool, so
they cannot drift.

```python
@app.get("/books/{id}/blurb", tool=True, scopes=["read"])
def blurb(id: int, style: str = "plain") -> str:
    """One-line pitch for a book."""
    return f"Book {id} — highly recommended."
```

[Tools and MCP :material-arrow-right:](agents-tools.md){ .md-button }

### 2. Agents that are safe to point at production

Agents declared in the app call tools **in-process** — a function call through
the same dispatcher the HTTP server uses, not a loopback request. Their
authority is *delegated*: agent scopes are intersected with the caller's, never
unioned, so an agent can never do more than whoever started it.

Budgets are enforced by the runtime, not trusted to the model. Dangerous tools
stop and wait for a human.

```python
@app.post("/invoices/purge", tool=True, scopes=["billing:write"], approval="required")
def purge() -> dict:
    """Gated: an agent asking for this suspends the run instead of getting it."""
    ...
```

[Agents :material-arrow-right:](agents.md){ .md-button }

### 3. Behaviours: procedures, not prompts

A "skill" written as a prompt is a suggestion — the model may ignore it, and
"if X then Y" fails silently when it does. A Behaviour inverts that. The loops
and branches are real Python that always runs; only the leaves are probabilistic.

```python
@app.behaviour("triage", tools=["list_tickets", "update_tickets"])
def triage(ctx, input):
    for ticket in ctx.call("list_tickets", status="open"):   # a real loop
        verdict = ctx.ask(                                    # a model call
            f"Classify: {ticket['body']}",
            schema={"type": "object",
                    "properties": {"urgency": {"enum": ["low", "high"]}}},
        )
        if verdict["urgency"] == "high":                       # a real branch
            ctx.call("update_tickets", id=ticket["id"], priority=1)
```

[Behaviours :material-arrow-right:](behaviours.md){ .md-button }

---

## Status

!!! warning "Young"
    v0.3.1, on PyPI as `web-cortex-framework`. Working and tested, but young.
    Read [Deployment](deployment.md#is-it-production-ready) for an honest
    assessment of what it is and is not ready for.

**Verified:** 251 tests (66 Rust, 185 Python, including a 54-test adversarial
suite). Clippy clean. `cargo audit` clean. CI builds wheels for Linux
(x86_64/aarch64), macOS (arm64/x86_64), and Windows across Python 3.12, 3.13
and free-threaded 3.14 (`3.14t`). GIL-enabled 3.14 is not supported — see
[Installation](installation.md#from-pypi).

**Measured:** CPU-bound Python handlers scale **4.82x** at concurrency 8 on a
free-threaded build, against **1.38x** on a GIL build. Soak: 1,786,805 requests
over 120 seconds, 0 errors, 0 panics.

**Security:** six issues found by an adversarial review and fixed, each with a
regression test. The [security review](https://github.com/slimboi34/web_cortex_framework/blob/main/SECURITY.md)
reports them and, deliberately, what was *not* tested.

---

## Where to go next

<div class="grid cards" markdown>

- :material-download: **[Installation](installation.md)** — get it running
- :material-school: **[Tutorial](tutorial.md)** — build a real app end to end
- :material-lightbulb: **[Use cases](use-cases.md)** — five worked examples
- :material-shield-lock: **[Security](security.md)** — auth, scopes, hardening

</div>
