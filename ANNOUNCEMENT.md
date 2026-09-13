# Launch posts

Two drafts. Every factual claim is verifiable from the repo — notes at the
bottom flag the ones to keep an eye on.

---

## X / Twitter

### Main post

> Every web framework we use was designed for one kind of user: a human with a
> browser.
>
> That assumption broke. Half your traffic is going to be AI agents.
>
> So I built WebCortex — a Python framework with a Rust core where every API
> endpoint is *simultaneously* a REST route, an OpenAPI spec, and a tool an AI
> agent can safely use.
>
> Open source. Apache-2.0. `pip install web-cortex-framework`
>
> 🧵

### Thread

**2/**
> The usual way to give an agent access to your app: build a second "tool
> server" that describes your API to the model.
>
> Now you maintain two descriptions of the same endpoint. They drift. The model
> calls something that no longer exists.
>
> In WebCortex the route *is* the tool. One declaration, three surfaces.

**3/**
> ```python
> app.resource("invoices", fields={...},
>              read_scopes=["read"], write_scopes=["billing:write"])
> ```
>
> That's it. You now have a REST API, an OpenAPI document, and a live MCP
> server your agent can connect to.
>
> No glue code. Nothing written twice.

**4/**
> The part I care most about: agents get **less** power than the person who
> asked.
>
> Agent permissions are *intersected* with the caller's — never unioned. An
> agent can never do something its user couldn't.
>
> That's enforced in the runtime. Not a convention you have to remember.

**5/**
> Destructive actions stop and wait for a human:
>
> ```python
> @app.post("/invoices/purge", tool=True, approval="required")
> ```
>
> An agent asking for that doesn't get it. The run suspends and files an
> approval request. It can propose. A person commits.

**6/**
> Budgets are enforced by the runtime, not trusted to the model. Steps and
> tokens are checked *before* each call, so a looping agent costs a bounded
> amount instead of whatever your provider will happily sell you.
>
> Every tool call is audited — including the ones that got refused.

**7/**
> The Rust part isn't decoration.
>
> Python runs *once*, at startup, to describe your app. Rust executes it. Routes
> that are queries, proxies, static responses or rendered pages never touch the
> Python interpreter at request time.
>
> Most of a CRUD API is exactly those routes.

**8/**
> And when you *do* need Python, it runs on free-threaded CPython 3.14 — real
> parallelism, no GIL.
>
> Measured, same hardware, same client:
> · free-threaded: **4.82x** at concurrency 8
> · GIL: **1.38x**
>
> That wasn't possible three years ago.

**9/**
> I attacked it before shipping it: JWT forgery, SQL injection, path traversal,
> SSRF, privilege escalation, prompt-laundering through agents.
>
> Found 6 real bugs. Fixed all 6. Wrote a regression test for each.
>
> The security doc lists what I *didn't* test too.

**10/**
> Soak test: 1,786,805 requests. 0 errors. 0 panics. Memory flat.
>
> 251 tests. Wheels for Linux, macOS and Windows.
>
> It's v0.3.1 and young — I'd tell you if it weren't.

**11/**
> Why open source matters here:
>
> The infrastructure agents run on is being decided *right now*. If that layer
> ends up owned by three companies, every AI product gets built on rented land.
>
> Apache-2.0. Fork it, ship it, sell it. No permission needed.

**12/**
> Docs: https://slimboi34.github.io/web_cortex_framework/
> Code: https://github.com/slimboi34/web_cortex_framework
>
> ```
> pip install web-cortex-framework
> webcortex new myapp --template agent
> ```
>
> Would genuinely love for people to try and break it.

### Shorter single-post version

> Every web framework assumes your user is a human with a browser.
>
> That's no longer true.
>
> WebCortex: a Python framework with a Rust core where every endpoint is
> automatically a REST API, an OpenAPI spec, *and* a tool an AI agent can use —
> with permissions that are always a subset of the human who asked.
>
> Open source, Apache-2.0.
> `pip install web-cortex-framework`
>
> https://github.com/slimboi34/web_cortex_framework

---

## LinkedIn

> **The frameworks we build software on assume the user is a human with a
> browser. That assumption just broke.**
>
> I've spent the last stretch building **WebCortex**, an open-source Python web
> framework with a Rust core, and I want to explain why — including for the
> people reading this who don't write code.
>
> ---
>
> **The problem, in plain terms**
>
> AI assistants are starting to *do* things, not just talk. Book the meeting.
> Issue the refund. Update the record. Pull the report.
>
> For that to work, the assistant has to reach into real business systems. And
> the moment it does, an uncomfortable question appears:
>
> *What, exactly, is it allowed to touch?*
>
> Today that answer is usually improvised. Teams bolt a separate "tool layer"
> onto an existing API — a second description of the same system, maintained by
> hand. It drifts out of date. Permissions get set generously because tightening
> them is fiddly. Nobody can say with confidence what the AI can and cannot do.
>
> That's a governance problem wearing an engineering costume. It's also the
> reason a lot of promising AI pilots never make it to production: not because
> the model isn't good enough, but because nobody can sign off on the blast
> radius.
>
> ---
>
> **What WebCortex does differently**
>
> You describe your API once. From that single description you automatically get:
>
> • a normal REST API for your apps
> • standard documentation for your developers
> • a live, permission-aware interface an AI agent can use
>
> One source of truth. It can't drift, because there's nothing to keep in sync.
>
> Three things are enforced by the framework itself, not left to whoever wrote
> the feature:
>
> **1. An agent never has more authority than the person who asked.**
> Permissions are intersected with the user's, never added to them. If you can't
> delete an invoice, neither can an assistant acting for you. That's the
> "confused deputy" problem, and it's solved structurally rather than by
> convention.
>
> **2. Dangerous actions stop and wait for a human.** Mark an operation as
> requiring approval and an agent simply cannot perform it. It can propose the
> action and explain why — a person commits it. The AI drafts; the human signs.
>
> **3. Every action is budgeted and audited.** Runs have hard ceilings on steps
> and cost, enforced before each call. Every tool invocation is logged —
> including the ones that were refused, which are usually the interesting ones
> during an incident.
>
> ---
>
> **Why the engineering is unusual**
>
> Python describes the application; Rust runs it. Your Python executes once, at
> startup, to declare what the app *is*. After that, the Rust engine serves
> requests — and routes that are database queries, proxies, static responses or
> rendered pages never touch the Python interpreter at all.
>
> When custom logic genuinely is needed, it runs on free-threaded Python 3.14 —
> a change to the language, years in the making, that finally allows true
> parallel execution. Measured on identical hardware, CPU-bound work scaled
> **4.82x** across 8 concurrent requests, against **1.38x** on a traditional
> build.
>
> Before shipping, I attacked it: authentication bypass, SQL injection, path
> traversal, server-side request forgery, privilege escalation, and attempts to
> launder restricted actions through an agent. It found six real vulnerabilities.
> All six are fixed, each with a permanent regression test. The security write-up
> also documents what I *didn't* test — because a security document that only
> lists successes is marketing.
>
> A sustained load test ran 1,786,805 requests with zero errors and flat memory.
>
> ---
>
> **Why it's open source, and why that matters now**
>
> This could have been a product. I made it Apache-2.0 instead.
>
> The infrastructure layer that AI agents will run on is being decided right now,
> in real time. If that layer ends up owned by a handful of very large companies,
> then every organisation building with AI is building on rented land — subject
> to someone else's pricing, roadmap, and terms.
>
> There is real value in some of that foundation staying open. Not out of
> idealism. Because a business that can read, audit, fork and self-host the layer
> its AI runs on has leverage, and one that can't, doesn't.
>
> Apache-2.0 means you can use it commercially, modify it, and ship it inside
> your own product. No permission, no negotiation, no seat count.
>
> ---
>
> **Where it actually is**
>
> It's version 0.3.1 and it's young. I'd rather say that than oversell it.
>
> It's genuinely ready for internal tools, services behind your own network, and
> AI integrations you control. It's not yet ready for public consumer-facing apps
> — the docs say exactly which parts and why, because you deserve to know that
> before you adopt something, not after.
>
> If you're a developer: `pip install web-cortex-framework`
>
> If you're not: this is the kind of plumbing that decides whether "let the AI
> handle it" is a reasonable sentence in your organisation or a terrifying one.
>
> Code and docs in the comments. I'd honestly love for people to try and break it.
>
> \#OpenSource #AI #SoftwareEngineering #Python #Rust #AIAgents

### First comment (LinkedIn buries links in the post body)

> Docs: https://slimboi34.github.io/web_cortex_framework/
> Code: https://github.com/slimboi34/web_cortex_framework
> Security review: https://github.com/slimboi34/web_cortex_framework/blob/main/SECURITY.md

---

## Notes on the claims

Everything above is verifiable. Specifically:

| Claim | Source |
|---|---|
| 4.82x vs 1.38x scaling | Benchmarked, both servers driven by the same client |
| 1,786,805 requests, 0 errors | 120s soak, recorded in `SECURITY.md` |
| Six vulnerabilities found and fixed | `SECURITY.md`, each with a regression test |
| 251 tests | 66 Rust + 185 Python |
| Apache-2.0, on PyPI | `LICENSE`, pypi.org/project/web-cortex-framework |

**Deliberately not claimed:** that WebCortex is faster than Django, Rails, or
FastAPI. That comparison has never been run, and the internal benchmarks were
limited by the load generator rather than the server. Any cross-framework number
would be marketing, and the first person to benchmark it properly would catch it.

**Two judgement calls for you:**

1. The posts say "half your traffic is going to be AI agents" — that's a
   rhetorical framing, not a measured statistic. Soften it if you'd rather not
   defend it.
2. I left out any comparison to Claude Code or other named AI tools. Inviting
   that comparison sets an expectation of maturity a v0.3.1 project can't meet
   yet, and it reads as borrowing someone else's credibility. The "Django was
   built for the browser era; this is built for the agent era" framing makes the
   same point and is yours to own.

---

# v2 launch post — draft

> WebCortex 2 is out. The first release was about one thing: declare a route
> once and it is a REST endpoint, an OpenAPI operation and an MCP tool, executed
> in Rust.
>
> This one is about what happens when there is more than one agent.
>
> 🧵

**2/**
> Every agent is now a tool. `app.agent("editor", tools=["researcher", "writer"])`
> is the whole supervisor/worker pattern. Workers run as delegates of the
> supervisor — less authority, never more — one nesting level deeper, against
> the supervisor's budget.

**3/**
> Handoffs. `handoffs=["billing", "technical"]` gives the front desk
> `transfer_to_*` tools. The conversation moves to the specialist; the budget
> and the caller's authority carry over; authority can only shrink.

**4/**
> Flows: orchestration as data, executed in Rust.
>
> ```python
> app.flow("briefing", pipeline=["researcher", "writer"], token_budget=150_000)
> app.flow("desk", route={"billing": "billing", "tech": "technical"}, classify_with="fast")
> ```
>
> Every step is a tool. A flow is a tool. One budget bounds the tree.

**5/**
> The budget bit is the one I care most about. In 0.3, each nested run got a
> fresh budget — a per-frame limit is not a per-request limit. Now the outermost
> agent's `token_budget` travels with every in-process call. The bill is what
> you declared.

**6/**
> Tokens are a declaration now. `app.models(default=..., fast="ollama/qwen3.5:9b")`
> — judgement on the big model, classification on a local one, for free.
> Prompt caching on by default. Tool results bounded. Long conversations
> compacted, not truncated. A ledger at `/_webcortex/usage`.

**7/**
> Context is declared: `app.context("policy", sql=...)` is resolved at run start
> and injected, bounded. Memory is four Rust-executed tools keyed by the human
> behind however many agents deep the call is.

**8/**
> Approval gates now *resume*. A gated tool suspends the run; a human decides at
> one endpoint; the run continues — including the rest of the turn it was in.
> Denial is a tool error the model reads.

**9/**
> And the framework describes itself to the model writing it.
> `webcortex context` prints the app's shape in ~2k tokens.
> `webcortex evolve "add reviews and a summariser"` proposes the code, anchored
> on that — with a local model if you like. Then `webcortex check` tells you
> what is wrong, at boot, with a hint.

**10/**
> 326 tests, including an offline end-to-end suite that drives handoffs,
> sessions, approvals, flows and memory over HTTP with no API key.
>
> `pip install web-cortex-framework`
> https://slimboi34.github.io/web_cortex_framework/

### Notes on claims

- "326 tests" — 95 Rust + 231 Python at the 2.0.0 commit; recount before posting.
- "~2k tokens" — the hello example's context pack is ~9 KB; state the KB if
  in doubt.
- Do not claim a benchmark against Django or FastAPI; none has been run.
