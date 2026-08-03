# Pylon

A Python web framework with a Rust core, built on one idea:

> **If you declared it, Rust can run it — and an agent can call it.**

Pylon is aimed at the gap Django and DRF leave open in 2026: they were designed
for a world where the only client was a browser. Today the client is just as
likely to be a model. Pylon treats that as a first-class case rather than
something you bolt on with a second, hand-maintained tool server.

```python
# api.py
from pylon import Pylon

app = Pylon("bookstore", database="sqlite://./app.db")

app.resource("books", fields={"id": int, "title": str, "author": str, "year": int}, tools=True)

@app.get("/books/{id}/blurb", tool=True)
def blurb(id: int) -> str:
    """One-line pitch for a book."""
    return f"Book {id} — highly recommended."
```

```
$ pylon dev
```

You now have:

- a REST API on `:8000`
- an OpenAPI 3.1 document at `/_pylon/openapi.json`
- **a live MCP server at `/_pylon/mcp` exposing all six endpoints as tools**

No second file, no schema written twice, no drift.

---

## Why a Rust core, specifically

Most Rust-accelerated Python servers put Rust at the socket and call Python for
every request. You get faster parsing; your handler is still interpreted.

Pylon puts the boundary somewhere more useful. **Python is a declaration
language that compiles to a plan the Rust runtime executes.** A route whose work
is expressible as data — a query, a proxy, a static response, an agent
invocation — is executed entirely in Rust and *never enters the interpreter at
request time*.

In practice, most of a CRUD API is exactly that kind of route.

```
$ pylon check
  12 routes, 11 served without touching Python
```

When a route genuinely needs Python, it crosses the bridge onto a pool of
free-threaded interpreter workers (CPython 3.13+/3.14 with the GIL disabled),
each running its own event loop. Handlers run in real parallel — which was not
possible when Django's execution model was designed.

Pylon runs correctly on a GIL build too. It's just slower, and it tells you so:

```
  python 3.14.4 (free-threaded)
```

---

## The convention

```
myapp/
├── api.py          # routes, resources, agents — the whole app surface
├── pylon.toml      # environment, database, deploy target  (optional)
└── client/         # generated TypeScript client            (optional)
```

One file gets you a long way. Split it when it hurts, not before.

## The five kinds of route

| Kind | Declared with | Runs in | Cost |
|---|---|---|---|
| Static | `app.static(...)` | Rust | serialized once at boot |
| Query | `app.query(...)`, `app.resource(...)` | Rust | SQL + JSON encode |
| Proxy | `app.proxy(...)` | Rust | one upstream hop |
| Agent | `app.agent(..., expose_at=...)` | Rust | model latency |
| Python | `@app.get(...)` | Python worker pool | interpreter |

## Every route is a tool

Mark a route `tool=True` and it appears in `tools/list` over MCP, with an input
schema derived from the handler's own type hints:

```python
@app.get("/books/{id}/blurb", tool=True)
def blurb(id: int) -> str: ...
```

becomes

```json
{
  "name": "get_books_by_id_blurb",
  "inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}},
                  "required": ["id"], "additionalProperties": false},
  "annotations": {"readOnlyHint": true}
}
```

Agents declared in the same app call those tools **in-process** — a function
call through the same dispatcher the HTTP server uses, not a loopback request.
Ten tool calls cost ten function calls.

```python
app.agent(
    "librarian",
    model="claude-opus-5",
    system="You help people find books.",
    tools=["list_books", "get_books", "get_books_by_id_blurb"],
    expose_at="/ask",
)
```

A typo in `tools` fails at boot, not in production.

## Scopes

Tool exposure and authorization are declared together, because a tool an agent
can call is an authorization decision:

```python
app.resource("invoices", fields={...}, tools=True, scopes=["billing:write"])
```

The runtime enforces the scope before the op runs, on the HTTP path and the
agent path alike.

---

## Status

Working today (v0.1): the manifest IR, router, native op executor (static /
query / proxy), the free-threaded Python bridge, OpenAPI generation, and the MCP
server. Postgres, the agent runtime, streaming, and TypeScript client generation
are next — see [`DESIGN.md`](DESIGN.md) for the full roadmap, the honest
feasibility assessment of each piece, and what is deliberately *not* being built.

## Building from source

```bash
uv venv --python 3.14.4+freethreaded
uv pip install maturin
.venv/bin/maturin develop --uv
.venv/bin/pylon dev examples/hello/api.py
```

## License

Apache-2.0
