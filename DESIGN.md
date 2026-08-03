# Pylon — design and feasibility

This document is the honest version. It states the bet, reports what has been
measured rather than hoped for, and grades every remaining piece by risk. Where
something is hard, it says so; where something should *not* be built, it says
that too.

---

## 1. The bet

Django's execution model assumes the client is a browser and the server is a
single interpreted process. Both assumptions are now shaky. The client is
increasingly a model, and CPython since 3.13 can run without a GIL.

Pylon's bet is one sentence:

> **Most of a web API is declarative, and declarative things can be executed by
> Rust and read by agents.**

Two consequences follow, and they are the entire architecture.

**Consequence A — the boundary moves.** Existing Rust-accelerated Python servers
(Granian, Robyn, Socketify) put Rust at the socket and call Python once per
request. The handler is still interpreted, so the ceiling is Python's. Pylon
instead treats Python as a *build-time* declaration language that emits a
manifest; Rust compiles that manifest into a router and a set of executable ops.
A route that is a query, a proxy, a static response, or an agent invocation is
executed entirely in Rust and never enters the interpreter at request time.

**Consequence B — the tool surface is not a separate artifact.** Every framework
bolting MCP onto an existing app maintains two descriptions of the same
endpoint: the route, and the tool. They drift. In Pylon the route *is* the tool.
`tools/list` is a projection of the route table, and a tool call re-enters the
same dispatcher an HTTP request would, in-process.

Everything else in this document is downstream of those two ideas.

---

## 2. What exists and has been measured

Working today, with 8 Rust tests and 54 Python tests passing:

- Manifest IR, boot-time validation, method-partitioned radix router
- Native ops: `Static`, `Query` (SQLite via sqlx), `Proxy` (the gateway)
- Python bridge over PyO3 onto a free-threaded interpreter worker pool
- OpenAPI 3.1 generation and a live MCP server (`initialize`, `tools/list`,
  `tools/call`, batching, notifications)
- `pylon` CLI: `dev`, `run`, `check`, `openapi`, `tools`, `sql`

### Measured numbers

Apples-to-apples, M-series arm64, CPython 3.14.4, **the same free-threaded
client driving both servers** so the load generator is not a confound. Zero
failed requests in every run.

| Route kind | free-threaded server | GIL server |
|---|---:|---:|
| `Static` (Rust) | 29,170 req/s | 33,429 req/s |
| `Query` (Rust + SQLite) | 25,315 req/s | 26,124 req/s |
| Python handler (bridge) | 15,215 req/s | 19,019 req/s |

CPU-bound Python handler, scaling with concurrency:

| Concurrency | free-threaded | GIL |
|---|---:|---:|
| 1 | 1.00x | 1.00x |
| 2 | 2.23x | 1.45x |
| 4 | 3.55x | 1.42x |
| 8 | **4.82x** | **1.38x** |

**Read these carefully.**

- The **CPU-scaling table is the real result.** Free-threading delivers genuine
  parallelism for Python handlers — 4.82x at concurrency 8, against a GIL build
  that plateaus at ~1.4x and then degrades. This is the finding that justifies
  the architecture, and it is not achievable on any pre-3.13 runtime.
- The **throughput table is not yet a meaningful comparison.** Both builds land
  at 25–33k req/s for I/O-shaped routes, and the GIL build is nominally *higher*
  on some rows. That is a strong hint the ~30k ceiling belongs to the stdlib
  load generator, not to either server. The honest claim from this data is
  narrow: *native routes sustain roughly 1.7–2x the throughput of the Python
  bridge path within a build.* The absolute ceiling is unmeasured, and claiming a
  number against Django or FastAPI would require `wrk`/`oha` and a tuned host.
  That benchmark is owed before any performance marketing.
- Free-threading costs single-thread speed (the well-known ~5–10% CPython
  penalty, visible in the concurrency-1 rows). It wins only when you are
  actually concurrent — which a server is.

---

## 3. Feasibility, by component

Graded by risk of *not working well*, not by effort.

### Tier 1 — Solved or straightforward

| Component | Risk | Notes |
|---|---|---|
| Rust HTTP core (hyper/tokio) | **Very low** | Done. Well-trodden. |
| Radix routing | **Very low** | Done, via `matchit`. |
| PyO3 bridge | **Low** | Done. The `Completer` handoff is the one subtle part and it is tested. |
| OpenAPI generation | **Very low** | Done. Derived, not authored. |
| MCP server | **Low** | Done. The protocol is small and stable. |
| Static/Proxy ops | **Low** | Done. |

### Tier 2 — Real engineering, known shape

| Component | Risk | The actual difficulty |
|---|---|---|
| **Postgres support** | Low–Medium | sqlx makes the driver easy. The work is that `Query` currently assumes `?` placeholders and SQLite's `RETURNING` semantics. Needs a small dialect layer. |
| **Migrations** | Medium | Deliberately *not* auto-applied today (see §5). A real versioned migration tool is a week of careful work, and it must be reviewable SQL, not implicit. |
| **Auth / scopes** | Medium | The enforcement point exists and is tested; what's missing is credential *ingestion* — JWT/session/API-key parsing into `req.scopes`, and per-connection scoping for MCP callers. Currently an MCP caller receives each route's declared scopes, which is fine for development and **not** an access-control boundary. This is the most important gap before anything real ships. |
| **SSE / WebSocket streaming** | Medium | Required for agent token streaming. The response type is currently `Bytes`; it needs to become a stream. Touches every op signature — better done soon than late. |
| **TypeScript client generation** | Low | The schemas already exist. This is a code generator over `openapi.json`, not research. |

### Tier 3 — The genuinely hard parts

| Component | Risk | The honest assessment |
|---|---|---|
| **Agent runtime in Rust** | **Medium–High** | The loop itself (call model → parse tool calls → dispatch → repeat) is easy; the runtime already has in-process tool dispatch, which is the valuable half. The hard parts are provider drift (every vendor's streaming tool-call format differs and changes), and cancellation/timeout semantics mid-stream. Mitigation: implement one provider properly rather than a leaky universal abstraction. |
| **Durable / resumable agent runs** | **High** | This is what separates a demo from production: checkpointing each step so a run survives a deploy. It is essentially building a small workflow engine, and getting exactly-once tool execution right is genuinely hard. **Recommendation: do not build this in v1.** Make agent runs explicitly ephemeral and say so. |
| **Local model hosting** | **High** | You asked about running local agents in-process. Be precise about what is feasible: *supervising* a llama.cpp or vLLM sidecar and routing to it over an OpenAI-compatible socket is very doable (Tier 2, honestly). *Embedding* inference in the server process, with GPU memory management and continuous batching, is a different project — that is what vLLM is, and it is years of work. **Recommendation: supervise, never embed.** |
| **Own ORM** | **High** | See §5. |

---

## 4. Why free-threaded Python is the enabling condition

Worth stating plainly, because it is the reason this is buildable now:

- On a GIL build, N Python worker threads give you concurrency for I/O and
  nothing for CPU. Measured above: 1.38x at concurrency 8.
- On a free-threaded build, they give real parallelism. Measured: 4.82x.
- PyO3 0.29 supports it directly (`#[pymodule(gil_used = false)]`).

The cost is ecosystem maturity: any C extension without free-threading support
will either fail to import or silently re-enable the GIL. Pure-Python and
Rust-backed packages are fine; the long tail of older C extensions is not.

**Mitigation, and it matters:** Pylon runs correctly on a GIL build. The
dispatcher checks `sys._is_gil_enabled()`, sizes its pools accordingly, and
prints which mode it is in at startup. Free-threading is the fast path, not a
hard requirement — so adoption is never blocked on a dependency.

---

## 5. What Pylon should deliberately not build

Scope discipline is the difference between this shipping and this becoming
another abandoned framework.

**Do not build an ORM.** This is the single biggest trap. SQLAlchemy is
twenty years of accumulated correctness around identity maps, lazy loading, and
transaction boundaries. Competing with it is a multi-year project that is
orthogonal to everything interesting here. Pylon's `Query` op is deliberately
*not* an ORM — it is a way to bind a route to SQL so Rust can execute it. When a
developer needs real object mapping, they should use SQLAlchemy inside a Python
handler. The framework's value is the routing, tool, and agent layer.

**Do not auto-migrate.** `resource(create_table=True)` emits
`CREATE TABLE IF NOT EXISTS` and nothing more. Silently issuing `ALTER TABLE`
because a Python dict changed is how frameworks destroy production data.
Migrations must be versioned, reviewable, and explicit.

**Do not build a universal LLM abstraction.** Every provider's streaming
tool-call format differs and changes underneath you. One provider done properly
beats five done leakily.

**Do not embed inference.** Supervise a sidecar. See Tier 3.

---

## 6. Roadmap

Ordered by what unblocks the most.

**v0.2 — make it safe to deploy**
1. Credential ingestion into `req.scopes` (JWT + API key), per-connection MCP
   scoping. *Until this lands, `scopes` is a structural placeholder, not
   security.*
2. Graceful shutdown, request timeouts, structured request IDs.
3. Postgres dialect.

**v0.3 — make it pleasant**
4. Streaming responses (SSE), which the agent runtime depends on.
5. TypeScript client generation + a dev-mode schema-push websocket.
6. `pylon.toml` for environment/database/deploy configuration.

**v0.4 — make it agentic**
7. Agent runtime, one provider, explicitly ephemeral runs.
8. Local model *supervision* (sidecar process management + routing).
9. Per-run token budgets and a full tool-call audit log.

**Later, if warranted**
10. Durable agent runs — only with a real design for exactly-once tool execution.

---

## 7. The honest summary

The novel, defensible idea here is **"declared once, executed by Rust, callable
by agents"** — and it is demonstrated working end to end today, not sketched.
The free-threading result (4.82x vs 1.38x) is real and measured, and it is the
technical justification for the whole approach.

The performance story against Django/FastAPI is **not yet proven** — the current
benchmark is client-limited and a proper one is owed.

The largest genuine risk is not technical, it is scope. A framework that also
tries to be an ORM, a migration tool, a workflow engine, and an inference server
will ship none of them. The pieces marked "do not build" in §5 are the ones most
likely to sink this if they creep back in.
