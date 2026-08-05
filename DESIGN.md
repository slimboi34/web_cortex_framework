# Rango — design and feasibility

This document is the honest version. It states the bet, reports what has been
measured rather than hoped for, and grades every remaining piece by risk. Where
something is hard, it says so; where something should *not* be built, it says
that too.

---

## 1. The bet

Django's execution model assumes the client is a browser and the server is a
single interpreted process. Both assumptions are now shaky. The client is
increasingly a model, and CPython since 3.13 can run without a GIL.

Rango's bet is one sentence:

> **Most of a web API is declarative, and declarative things can be executed by
> Rust and read by agents.**

Two consequences follow, and they are the entire architecture.

**Consequence A — the boundary moves.** Existing Rust-accelerated Python servers
(Granian, Robyn, Socketify) put Rust at the socket and call Python once per
request. The handler is still interpreted, so the ceiling is Python's. Rango
instead treats Python as a *build-time* declaration language that emits a
manifest; Rust compiles that manifest into a router and a set of executable ops.
A route that is a query, a proxy, a static response, or an agent invocation is
executed entirely in Rust and never enters the interpreter at request time.

**Consequence B — the tool surface is not a separate artifact.** Every framework
bolting MCP onto an existing app maintains two descriptions of the same
endpoint: the route, and the tool. They drift. In Rango the route *is* the tool.
`tools/list` is a projection of the route table, and a tool call re-enters the
same dispatcher an HTTP request would, in-process.

**Consequence C — control flow belongs in code, not in a prompt.** A prompt-based
"skill" asks a model to follow a procedure; when it deviates, nothing notices.
A **Behaviour** is Python whose loops and branches always execute, with model
calls only at the leaves. The structure is deterministic and the steps are
probabilistic, which is the opposite of the usual arrangement and the reason a
behaviour can be given a budget, a scope set, and an audit trail that mean
something.

Everything else in this document is downstream of those three ideas.

---

## 2. What exists and has been measured

Working today, with **66 Rust tests and 104 Python tests** passing and clippy
clean:

**Runtime**
- Manifest IR, boot-time validation, method-partitioned radix router
- Native ops: `Static`, `Query` (SQLite via sqlx), `Proxy`, `Page` (minijinja),
  `Files`
- Python bridge over PyO3 onto a free-threaded interpreter worker pool
- Graceful shutdown with connection draining; per-request timeouts

**Security**
- Principals from API keys (SHA-256, constant-time) and JWT (HS/RS, expiry
  enforced); scope enforcement on one code path shared by HTTP, MCP, and agents
- Per-principal token-bucket rate limiting, sharded to avoid a contention point
  under exactly the load that motivates it
- CORS with boot-time rejection of `*`-with-credentials; security headers
- Static server hardened against traversal, symlink escape, and dotfile leaks

**Agents and Behaviours**
- Tool loop with an Anthropic provider and a scripted provider for tests
- Scope delegation by intersection; approval gates; step and token budgets
- Behaviours: Python control flow bridged back into the runtime, exposed as
  tools, composable, with the same budgets, scopes, gates, and audit
- Audit trail including refused calls

**Developer surface**
- OpenAPI 3.1, MCP (`initialize`, `tools/list`, `tools/call`, batching,
  notifications), TypeScript client generation
- `rango` CLI: `new`, `dev`, `run`, `check`, `security`, `openapi`, `tools`,
  `typegen`, `sql`, `keygen`

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
| ~~Auth / scopes~~ | **Done** | Shipped in v0.2. Credential ingestion, principals, per-caller MCP scoping, and scope delegation for agents. |
| ~~TypeScript generation~~ | **Done** | Shipped in v0.2. |
| ~~Templating~~ | **Done** | Shipped in v0.2 via minijinja, executed in Rust. |
| **Postgres support** | Low–Medium | sqlx makes the driver easy. The work is that `Query` assumes `?` placeholders and SQLite's `RETURNING` semantics. Needs a small dialect layer. |
| **Migrations** | Medium | Deliberately *not* auto-applied (see §5). A real versioned migration tool is a week of careful work, and it must produce reviewable SQL. |
| **SSE / WebSocket streaming** | Medium | Required for agent token streaming. The response body is `Bytes`; it needs to become a stream, which touches every op signature. The single most invasive item remaining — better done soon than late. |
| **Session auth for pages** | Low–Medium | Server-rendered apps want cookies and CSRF, not just bearer tokens. The principal abstraction already accommodates it; the cookie/CSRF machinery does not exist yet. |

### Tier 3 — The genuinely hard parts

| Component | Risk | The honest assessment |
|---|---|---|
| **Agent runtime in Rust** | **Medium–High** | The loop itself (call model → parse tool calls → dispatch → repeat) is easy; the runtime already has in-process tool dispatch, which is the valuable half. The hard parts are provider drift (every vendor's streaming tool-call format differs and changes), and cancellation/timeout semantics mid-stream. Mitigation: implement one provider properly rather than a leaky universal abstraction. |
| **Behaviour runtime** | **Shipped, low ongoing risk** | The bridge is `Handle::block_on` from Python worker threads, which are deliberately not tokio contexts. The design constraint that keeps it sound: behaviours run on the thread pool, never on a shared event loop, because `ctx.call` blocks. Budgets, scope checks, and approval gates all sit on the Rust side of the boundary rather than being enforced in Python, so a behaviour cannot talk its way past them. |
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

**Mitigation, and it matters:** Rango runs correctly on a GIL build. The
dispatcher checks `sys._is_gil_enabled()`, sizes its pools accordingly, and
prints which mode it is in at startup. Free-threading is the fast path, not a
hard requirement — so adoption is never blocked on a dependency.

---

## 5. What Rango should deliberately not build

Scope discipline is the difference between this shipping and this becoming
another abandoned framework.

**Do not build an ORM.** This is the single biggest trap. SQLAlchemy is
twenty years of accumulated correctness around identity maps, lazy loading, and
transaction boundaries. Competing with it is a multi-year project that is
orthogonal to everything interesting here. Rango's `Query` op is deliberately
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

**v0.2 — shipped**
1. ✅ Credential ingestion, principals, per-caller MCP scoping
2. ✅ Graceful shutdown, request timeouts, request IDs, rate limiting, CORS,
   security headers
3. ✅ Agent runtime with delegation, approval gates, budgets, audit
4. ✅ Server-rendered pages, static files, TypeScript generation, scaffolding

**v0.3 — shipped**
✅ Behaviours: programmable procedures with deterministic control flow,
   composable, budget-capped, and exposed as tools.

**v0.4 — the streaming release**
5. SSE responses. Agent token streaming depends on it, and it is invasive
   enough that delaying it makes it worse.
6. Postgres dialect.
7. Session cookies + CSRF, so the page layer is usable for real apps.
8. Resume an agent run after an approval is granted. The gate records the
   request today; the resume endpoint is not wired up, so a gated run currently
   ends rather than continuing. **This is the most visible unfinished edge.**

**v0.5 — operational depth**
9. Local model *supervision* (sidecar process management + routing).
10. `rango.toml` for environment/deploy configuration.
11. A real load benchmark against Django and FastAPI (see §2).

**Later, if warranted**
12. Durable agent runs — only with a real design for exactly-once tool
    execution.

---

## 7. What the security review changed

An adversarial review (see [`SECURITY.md`](SECURITY.md)) found six issues. Two
are worth pulling into the design record because they were *design* faults, not
slips:

**Budgets did not compose.** A behaviour's `max_steps` bounded that behaviour,
but each nested invocation received a fresh budget — so recursion was unbounded
even though every individual frame was capped. The lesson generalises: a
per-invocation limit is not a per-request limit, and anything that can re-enter
the dispatcher needs a counter that travels *with the request* rather than with
the frame. `RangoRequest.depth` is that counter.

**A panic was indistinguishable from a network fault.** A dependency panicking
in the auth path severed the connection with no status line. The framework had
no boundary, so the symptom was "connection closed" — one of the hardest
failures to diagnose. The request path now catches unwinds. This is why
`panic = "abort"` was rejected in v0.1: an aborting process would have made the
same bug a crash loop instead of a contained 500.

## 8. The honest summary

The novel, defensible idea here is **"declared once, executed by Rust, callable
by agents"** — and it is demonstrated working end to end today, not sketched.
The free-threading result (4.82x vs 1.38x) is real and measured, and it is the
technical justification for the whole approach.

The performance story against Django/FastAPI is **not yet proven** — the current
benchmark is client-limited and a proper one is owed.

The security model is now real rather than structural: scope enforcement runs on
one code path shared by HTTP, MCP, and agents, and agent authority is a subset of
its caller's *by construction*. Three genuine holes were found and fixed by
tests while building v0.2 — MCP substituting route scopes for caller scopes,
`resource()` leaving writes public while claiming otherwise, and `expose_at`
creating an unauthenticated agent endpoint. That is the argument for the test
suite, not for the absence of remaining bugs.

**Known unfinished edges**, stated plainly: a gated agent run records its
approval request and stops, but cannot yet be resumed; the Anthropic provider is
not exercised against the live API in CI, so `ctx.ask` and the agent loop are
tested against a scripted provider rather than the real one; there is no CSRF or
session support, so the page layer suits internal tools more than public
authenticated apps; and a Behaviour holds a worker thread for its whole run,
which is fine for procedures measured in seconds and wrong for ones measured in
hours.

The largest genuine risk is still not technical, it is scope. A framework that
also tries to be an ORM, a migration tool, a workflow engine, and an inference
server will ship none of them. The pieces marked "do not build" in §5 are the
ones most likely to sink this if they creep back in.
