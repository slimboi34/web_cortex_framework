# WebCortex

```bash
pip install web-cortex-framework
```

> Installs as **`web-cortex-framework`**, imports as **`webcortex`** — the same
> split as `djangorestframework` → `import rest_framework`.
>
> **Python 3.12, 3.13 and 3.14**, including the free-threaded build (`3.14t`),
> which is the fast path for Python-backed routes.

**📖 [Documentation](https://slimboi34.github.io/web_cortex_framework/)** ·
[Tutorial](https://slimboi34.github.io/web_cortex_framework/tutorial/) ·
[Orchestration](https://slimboi34.github.io/web_cortex_framework/orchestration/) ·
[Token economy](https://slimboi34.github.io/web_cortex_framework/models/) ·
[Security](https://slimboi34.github.io/web_cortex_framework/security/)

A Python web framework with a Rust core, built on one idea:

> **If you declared it, Rust can run it — and an agent can call it.**

Django and Rails were designed when the only client was a browser and the only
author was a person. Today the client is just as likely to be a model, and so
is the author. WebCortex treats both as the primary case: every declaration is
simultaneously a REST route, an OpenAPI operation and an MCP tool; agents,
behaviours and flows are first-class and compose; and the whole application
can describe itself to the model that is writing it.

```python
# api.py
from webcortex import WebCortex

app = WebCortex("bookstore", database="sqlite://./app.db")

app.api_key("WEBCORTEX_API_KEY", id="service", scopes=["read", "write"])
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
$ webcortex dev
```

You now have a REST API, an OpenAPI 3.1 document, **a live MCP server exposing
all six endpoints as tools**, authentication, rate limiting, and security
headers. No second file, no schema written twice, no drift.

Twenty more lines make it a multi-agent system:

```python
app.models(default="claude-opus-5", fast="ollama/qwen3.5:9b")   # tiers, not models

app.context("policy", data={"max_discount": 0.2})              # what agents know at step one
notes = app.memory("notes", scopes=["read"])                     # a durable, per-user scratchpad

app.agent("librarian", description="Finds and pitches books.",
          tools=["list_books", "get_books_by_id_blurb"], context=["policy"],
          memory="notes", scopes=["read"], token_budget=60_000)

app.agent("front_desk", handoffs=["librarian"], scopes=["read"],
          context_window=40_000, expose_at="/ask")             # hands off; compacts

app.flow("pitch_all", parallel=["librarian", "librarian"])      # orchestration as data
```

Every one of those is a route, a tool, and an MCP entry. The front desk hands
a conversation to the librarian; the librarian remembers what it learned about
the caller; the whole run is bounded by one budget; and `webcortex context`
prints the lot in a form a coding model can extend.

---

## Start here

```bash
uv venv --python 3.13 && uv pip install web-cortex-framework

webcortex new myapp                # --template api | fullstack | agent | behaviour | orchestration
cd myapp
export WEBCORTEX_API_KEY=$(webcortex keygen)
webcortex dev
```

## Why a Rust core, specifically

Most Rust-accelerated Python servers put Rust at the socket and call Python for
every request. You get faster parsing; your handler is still interpreted.

WebCortex puts the boundary somewhere more useful. **Python is a declaration
language that compiles to a plan the Rust runtime executes.** A route whose work
is expressible as data — a query, a proxy, a rendered page, a static file, an
agent invocation, a flow — runs entirely in Rust and *never enters the
interpreter at request time*. In practice that is most of a CRUD API and all of
an orchestration.

```
$ webcortex check
  19 routes, 17 served without touching Python
```

When a route genuinely needs Python, it crosses onto a pool of free-threaded
interpreter workers (free-threaded CPython 3.14, GIL disabled), each running its own
event loop. Handlers run in real parallel — **measured at 4.82× vs 1.38× under
the GIL** ([DESIGN.md](DESIGN.md) has the numbers and their caveats).

WebCortex runs correctly on a GIL build too, and tells you which mode it is in.

## Behaviours

A "skill" written as a prompt is a *suggestion*. The model reads it and may
ignore it, and "if X then Y" fails silently when it does.

A **Behaviour** inverts that. The control flow is real Python — a `for` loop is
a loop, an `if` is a branch, and both execute whether or not a model would have
chosen to. Only the *leaves* are probabilistic, and in v2 the leaves run
concurrently:

```python
@app.behaviour("triage", tools=["list_tickets", "update_tickets"],
               model="fast", max_steps=200, token_budget=100_000)
def triage(ctx, input):
    """Classify every open ticket at once and escalate the urgent ones."""
    tickets = [t for t in ctx.call("list_tickets", limit=50) if t["state"] == "open"]

    verdicts = ctx.ask_many(                                   # fifty model calls, one wait
        [f"Grade this ticket:\n{t['body']}" for t in tickets],
        schema={"type": "object",
                "properties": {"urgency": {"type": "integer"},
                               "category": {"enum": ["bug", "billing", "other"]}},
                "required": ["urgency", "category"]},
    )

    escalated = [t["id"] for t, v in zip(tickets, verdicts)     # a real branch
                 if v["urgency"] >= input.get("threshold", 7)]

    ctx.gather(*[                                                # fifty writes, one wait
        ("update_tickets", {"id": t["id"], "body": t["body"], "urgency": v["urgency"],
                            "state": "escalated" if t["id"] in escalated else "triaged"})
        for t, v in zip(tickets, verdicts)
    ])
    return {"escalated": escalated, "usage": ctx.usage}
```

You get a procedure with **deterministic structure and probabilistic steps**,
rather than a probabilistic procedure.

`ctx` is how a behaviour reaches the world:

| | |
|---|---|
| `ctx.call(tool, **kwargs)` | invoke one of the app's tools, in-process |
| `ctx.gather((tool, kwargs), ...)` | several tool calls, concurrently |
| `ctx.ask(prompt, schema=..., model="fast")` | a model call; a schema **forces** the shape; the model may be a tier |
| `ctx.ask_many([prompts], schema=...)` | the classification loop collapsed into one wait |
| `ctx.context(name)` | a declared context provider, resolved on demand |
| `ctx.log(msg)` / `ctx.halt(reason)` | narrate or stop deliberately |
| `ctx.usage` / `ctx.trace` | budget consumed (this run and the whole tree) and every leaf executed |
| `ctx.user` / `ctx.tools` | the delegated principal and what it may call |

**A behaviour is a tool.** It registers as a route, so it is automatically an
MCP tool, an OpenAPI operation, and something an agent — or another behaviour,
or a flow — can call.

**The runtime enforces the limits, not your diligence:** `max_steps` caps leaf
operations; `token_budget` caps spend for the run *and everything it calls*; a
behaviour cannot call a tool it did not declare; an approval-gated tool cannot
be laundered through a behaviour or a `gather`; scopes are delegated by
intersection, never unioned.

## Orchestration

One agent is a tool loop. Several are a system, and a system needs answers a
loop never asks: who is in control, what may it do, how much may the whole
thing cost, what happens when a human has to decide. Four primitives, all
executed by the runtime:

**Every agent is a tool.** An agent is mounted at `/agents/<name>` (underscores
become hyphens) and exposed
under its own name, so `app.agent("editor", tools=["researcher", "writer"])` is
the entire supervisor/worker pattern. Workers run under a principal delegated
from the supervisor's, one nesting level deeper, against the supervisor's
budget.

**Handoffs.** `app.agent("front_desk", handoffs=["billing", "technical"])`
gives the front desk `transfer_to_*` tools. Calling one moves the *conversation*
to the specialist — its system prompt, tools and context apply from the next
step — while the budget and the caller's authority carry over. Authority can
only shrink along a chain.

**Flows: orchestration as data.**

```python
app.flow("briefing", pipeline=["researcher", "writer"], token_budget=150_000)
app.flow("review",   parallel=["security_review", "style_review"], merge="collect")
app.flow("desk",     route={"billing": "billing", "technical": "technical"},
                     default="front_desk", classify_with="fast")
```

Every step is a tool — an agent, a behaviour, another flow, a plain route — so
composition is uniform, and a flow is itself a tool. Steps map arguments with
`{"tool": "x", "input": {"id": "$.id", "q": "$input.query"}}`.

**Sessions.** Post `{"input": "...", "session_id": "..."}` and the conversation
continues; the key includes the principal, so callers never see each other's
history, and anonymous callers, who share one identity, cannot open one. Long sessions are compacted, not truncated.

**Approvals that resume.** A gated tool suspends the run; a human decides at
`POST /_webcortex/approvals/{id}` with `{"approve": true|false, "note": "..."}`;
the run continues — including the rest of the turn it was interrupted in. A
denial is a tool error the model reads and reacts to.

**Budgets compose.** The outermost run's `token_budget` is shared by every
agent, behaviour and flow it calls. `usage.tree_tokens` reports the total.

## Token economy

The cost of an agent system is which model answers, how much context each call
carries, and how many calls are made. Each has a declaration.

```python
app.models(default="claude-opus-5", fast="claude-haiku-4-5-20251001",
           local="ollama/qwen3.5:9b")
app.provider("groq", base_url="https://api.groq.com/openai/v1", api_key_env="GROQ_API_KEY")
app.pricing("claude-opus-5", input_per_mtok=15, output_per_mtok=75)
```

- **Tiers, not models.** Judgement uses `default`; classification, extraction
  and routing use `fast`. Moving a workload to a cheaper or local model is one
  edit.
- **Two wire formats, chosen by prefix.** Anthropic Messages and OpenAI Chat
  Completions — which is what Ollama, vLLM, LM Studio, Groq and OpenRouter all
  speak. `ollama/qwen3.5:9b` needs no key. Deliberately not a universal LLM
  abstraction.
- **Prompt caching** on by default: the system prompt, context and tool
  definitions are cached across the steps of a run.
- **Bounded context.** `tool_result_limit` caps what the model sees of any
  tool result; `context_window` compacts older turns with the fast model when
  the *measured* input exceeds it.
- **A ledger.** `GET /_webcortex/usage` reports tokens by agent, behaviour,
  flow and model, and dollars where you declared prices — `null`, not zero,
  where you did not.

## Context and memory

```python
app.context("policy", data={"refund_days": 30})
app.context("open_queue", sql="SELECT id, kind FROM tickets WHERE state='open' LIMIT 20")
app.context("my_orders", sql="SELECT * FROM orders WHERE customer = ?", params=["@principal"])

@app.context("account")
def account(req) -> dict: ...

notes = app.memory("notes", read_scopes=["read"], write_scopes=["write"])
app.agent("assistant", context=["policy", "my_orders"], memory="notes", ...)
```

A context provider is resolved when a run starts and injected into the system
prompt, bounded by `max_chars`. `@principal` binds to the human behind however
many agents deep the call is. A memory is four Rust-executed tools —
`remember`, `recall`, `search`, `forget` — keyed by that same principal, so an
agent writing on someone's behalf writes to that someone's memory and can never
read another's.

## The nine kinds of route

| Kind | Declared with | Runs in |
|---|---|---|
| Static | `app.static(...)` | Rust |
| Query | `app.query(...)`, `app.resource(...)`, `app.memory(...)` | Rust |
| Page | `app.page(...)` | Rust (minijinja) |
| Files | `app.static_files(...)` | Rust |
| Proxy | `app.proxy(...)` | Rust |
| Agent | `app.agent(...)` | Rust |
| Flow | `app.flow(...)` | Rust |
| Behaviour | `@app.behaviour(...)` | Python worker pool |
| Python | `@app.get(...)` | Python worker pool |

## Every route is a tool

Mark a route `tool=True` and it appears in MCP `tools/list`, with an input
schema derived from the handler's own type hints. Agents declared in the same
app call those tools **in-process** — a function call through the same
dispatcher the HTTP server uses, not a loopback request. A typo in `tools`,
`handoffs` or `context` fails at boot with a "did you mean" suggestion.

## What makes agents safe to deploy

Enforced by the runtime, not by your diligence:

**Delegated authority.** A run executes as `caller.delegate_to_agent(...)`,
whose scopes are *intersected* with the caller's — never unioned — and a
handoff intersects again. An anonymous caller cannot launch a privileged agent.

**Human approval gates.** Mark a route `approval="required"` and an agent asking
for it does not get it — the run suspends until a human decides, on every path:
the agent loop, MCP, behaviours, `gather`, flows.

**Runtime-enforced budgets that compose.** `max_steps` per run;
`token_budget` for the run and everything under it.

**Scope-filtered tool lists.** `tools/list` shows only what *that caller* can
invoke.

**A full audit trail**, including refused calls, handoffs, compactions and
approval decisions, at `GET /_webcortex/audit`.

## Security defaults

Deny-by-default throughout; relaxing something costs a line, tightening it costs
nothing. API keys are referenced by environment variable, hashed with SHA-256,
and compared in constant time. JWT (HS/RS) with mandatory expiry validation.
Per-principal token-bucket rate limiting. Security headers on every response.
CORS that refuses `*` with credentials *at boot*. Path traversal, symlink
escapes, and dotfiles refused by the static server. The control plane —
including approvals, usage and models — requires `webcortex:admin` once any
authentication is configured.

## The frontend, without the mess

Two clean paths sharing one data layer, chosen per route: **server-rendered
pages** executed in Rust (`app.page("/", "index.html", sql=..., bind="books")`),
where a template receives a data object and nothing else; and **a typed
TypeScript client** for SPA frontends, generated from the same route table by
`webcortex typegen`.

## AI-native development

```
$ webcortex context                     # the app, described for a coding model
$ webcortex evolve "add reviews tied to books and a behaviour that summarises them" --model fast
$ webcortex check                       # boot-time validation, with hints
```

The **context pack** is the app's shape — routes, tools and schemas, agents,
behaviours, flows, context, memory, models, security posture — derived from the
manifest in a few thousand tokens, plus a cheat sheet of the framework's API.
`evolve` feeds it to a model (with the app's own aliases, so `--model fast` can
be a local Ollama model) and prints a proposal to review. The loop is
*describe → propose → check → run*, and boot-time validation is what makes it
safe to repeat.

[`AGENTS.md`](AGENTS.md) is the machine-facing reference for tools that edit
this repository or write apps on it. [`scout/`](scout/) is a local-model
reviewer that leaves suggestions for the framework's own next iteration.

## Commands

```
webcortex new <name>      scaffold a project (api | fullstack | agent | behaviour | orchestration)
webcortex dev             run with a startup report
webcortex check           routes, tools, agents, flows, and the public attack surface
webcortex security        what is reachable without a credential
webcortex tools           the agent tool manifest
webcortex context         the context pack, for an AI coding tool
webcortex evolve "…"      ask a model to propose an extension
webcortex typegen         generate a typed TypeScript client
webcortex openapi         the OpenAPI 3.1 document
webcortex sql             DDL for declared resources and memories
webcortex keygen          mint an API key
```

## Security

WebCortex has been through an adversarial review of its own controls — auth,
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
tested, the known limits, and what v2 added to the surface.

## Status

v2.0.0. Working and tested: the manifest IR, router, native ops (static /
query / proxy / page / files / flow), the free-threaded Python bridge,
authentication and scopes, rate limiting, CORS, security headers, graceful
shutdown, Behaviours with concurrent leaves, the agent runtime with handoffs,
sessions, resumable approval gates, composing budgets, tool-result bounding and
compaction, context providers, memory, flows, two providers (Anthropic and
OpenAI-compatible, which covers local models), prompt caching, the spend
ledger, the audit trail, OpenAPI, the MCP server, TypeScript generation, the
context pack and `evolve`. **326 tests** (95 Rust, 231 Python, including a
54-test adversarial suite and an offline end-to-end suite that drives the whole
agent stack over HTTP), clippy clean.

Not yet: Postgres, token-level SSE streaming, durable agent runs that survive a
restart. See [DESIGN.md](DESIGN.md) for the roadmap, honest risk grading, and —
just as importantly — what is deliberately **not** being built.

## For AI coding tools

[**AGENTS.md**](AGENTS.md) is the machine-facing reference: the complete API
surface with exact signatures and defaults, the binding and scope rules, the
constraints the runtime enforces, and the specific mistakes that are cheap to
make and expensive to debug. Claude Code, Cursor, Codex, Aider and Copilot
Workspace all read it by convention.

It is written to be correct rather than welcoming. Humans should start with the
[docs site](https://slimboi34.github.io/web_cortex_framework/) instead.

## Building from source

```bash
uv venv --python 3.13
uv pip install maturin pytest
.venv/bin/maturin develop --uv
.venv/bin/python -m pytest tests/
```

The free-threaded build is also supported and is the configuration the bridge was
designed around:

```bash
uv venv --python 3.14t
```

## License

Apache-2.0
