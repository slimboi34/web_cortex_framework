# Changelog

Notable changes per release. Versions follow [semantic versioning](https://semver.org); before 2.0, minor
bumps could contain breaking changes.

## [2.0.0] — 2026-09-13

The orchestration generation. The version jumps from 0.3 to 2.0 because this
is a second design rather than a polish of the first: 0.3 established that one
declaration is a route, a tool and a document; 2.0 establishes that agents,
behaviours and flows compose under one budget, and that the application can
describe itself to the model writing it.

### Added — orchestration

- **Every agent is a tool.** An agent is mounted at `/agents/<name-with-hyphens>` (or
  `expose_at`) and exposed under its own name, so
  `app.agent("editor", tools=["researcher", "writer"])` is the whole
  supervisor/worker pattern. Workers run as delegates of the supervisor, one
  nesting level deeper, against the supervisor's budget.
- **Handoffs.** `app.agent(..., handoffs=["billing"])` adds a
  `transfer_to_billing` tool. Calling it moves the conversation to the target
  agent — its system prompt, tools and context apply from the next step —
  while the budget and the caller's authority carry over. Authority is
  re-derived from the original caller and filtered by what the previous agent
  held, so it can only shrink. The result records the `path`. A declared tool
  that collides with a handoff's `transfer_to_<name>` is a boot error.
- **Flows.** `app.flow(name, pipeline=[...] | parallel=[...] | route={...})`
  declares an orchestration as data, executed in Rust. Steps are any tools;
  agent results are unwrapped for the next step; `{"tool": ..., "input": {...}}`
  maps arguments with `$`, `$.path`, `$input` and `$input.path`. Routers
  classify with a forced structured call on the `fast` tier and fall back to
  `default`. A flow is itself a tool and a route.
- **Sessions.** Agent routes accept `session_id` (and `reset`); the
  conversation is kept between requests, keyed by principal as well as id;
  anonymous callers, who all share one identity, are refused a session.
  In memory, bounded (`session_capacity`) and expiring (`session_ttl_secs`).
- **Approvals that resume.** `GET /_webcortex/approvals` lists suspended
  runs; `POST /_webcortex/approvals/{id}` with `{"approve": bool, "note"}`
  continues one — executing or refusing the gated call, then the **rest of
  the interrupted turn**, then the loop. A denial reaches the model as a tool
  error carrying the note. Decisions are consumed; runs expire after
  `approval_ttl_secs`.
- **A shared budget.** The outermost agent, behaviour or flow creates a
  `SharedBudget` from its `token_budget`; it travels with every in-process
  call, and nested runs charge the same counter. `usage.tree_tokens` reports
  the total. This generalises the 0.3 depth counter: a per-frame limit is not
  a per-request limit. Token arithmetic saturates, so an upstream reporting an
  absurd `usage` cannot wrap the counter and reopen an exhausted budget.
- `ctx.gather(...)` and `ctx.ask_many(...)` run tool calls and model calls
  concurrently from a behaviour — one wait instead of a loop of round trips —
  with the same admission, scoping, gating and charging as `call` and `ask`.
- `WebCortex(agent_timeout=600)`: agent, flow and behaviour routes have their
  own ceiling. They used to share the 30-second `request_timeout`, which cut a
  multi-step run off with a 504 and lost it.

### Added — token economy

- `app.models(**aliases)`: name tiers once (`default`, `fast`, `local`, …)
  and use them anywhere a model is named. `default` and `fast` are built in.
- **OpenAI-compatible provider**, selected by prefix (`ollama/…`, `openai/…`,
  `gpt-*`, or a name from `app.provider(...)`). This is how Ollama, vLLM,
  LM Studio, Groq and OpenRouter arrive. `ollama/<model>` needs no key.
  Conversations stay in one canonical shape; the provider translates at its
  boundary, so a session can move between providers.
- **Prompt caching** on Anthropic (`cache=True`, the default): the system
  prompt, context and tool definitions are cached across the steps of a run.
  Usage reports `cache_read_tokens` and `cache_write_tokens`.
- **Tool-result bounding.** `tool_result_limit` (default 16 KB) caps what the
  model sees of any result, with a marker; the step record keeps the whole
  value.
- **Compaction.** `context_window` is checked against the *measured* input of
  the last call; older turns are summarised with `compact_with` (default the
  `fast` alias), keeping `keep_recent` messages verbatim. The cut lands on an
  assistant turn so tool pairs stay intact. A failed compaction is recorded
  as a step naming the model, and the run continues uncompacted; the next
  attempt waits 2, then 4, 8… steps, so a summariser that keeps failing is not
  called on every remaining step.
- **The ledger.** `GET /_webcortex/usage` reports tokens by caller and model,
  and dollars only from prices declared with `app.pricing(...)` — `null`, not
  zero, where none are.
- Structured output is now *forced* (`tool_choice`) on both providers, and
  JSON is recovered from prose or code fences when a small local model answers
  in text.

### Added — context and memory

- `app.context(name, sql=|data=|@decorator)`: named providers resolved at run
  start and injected into the system prompt as delimited blocks, bounded by
  `max_chars`. Agents name them with `context=[...]`; behaviours read them with
  `ctx.context(name)` and may only read what they declared.
- `app.memory(name)`: a per-principal key-value store as four Rust-executed
  tools (`remember`, `recall`, `search`, `forget`). `app.agent(memory=...)`
  adds them plus a usage hint. Memory, and any route that binds `@principal`,
  answers 401 to anonymous callers for the same reason; a context provider
  binds it as NULL for them.
- `@principal` binds the **root** principal — the human behind any chain of
  agent delegation — in `app.query` and `app.context` parameters.

### Added — AI-native development

- `webcortex context`: the context pack — the app described for a coding
  model, derived from the manifest, plus a cheat sheet of the API.
- `webcortex evolve "..."`: a model proposes an extension anchored on the
  context pack, using the app's own aliases and providers (so `--model fast`
  can be a local model). Prints a proposal; never edits files.
- `webcortex new --template orchestration`.
- `scout/`: a stdlib-only local-model reviewer for this repository, with a
  launchd installer, that appends suggestions to a Markdown file.
- `WEBCORTEX_FAKE_PROVIDER=1` selects a deterministic fake provider, and
  `tests/test_orchestration.py` drives handoffs, sessions, approval resume,
  flows, memory, context, `gather`, `ask_many` and the ledger over HTTP with
  no key. Agents were previously untested end to end.
- Control plane: `/flows`, `/contexts`, `/models`, `/usage`, `/approvals`.
  `/health` counts flows, sessions and pending approvals.

### Changed

- **An agent without `expose_at` now has a route** at `/agents/<name-with-hyphens>`,
  guarded by `expose_scopes` (default `scopes`). An agent declared with no
  scopes is therefore a public route; `webcortex security` reports it.
- `app.agent(model=...)` is optional and defaults to the `default` alias;
  `@app.behaviour(model=...)` likewise.
- A missing `input` on an agent route is a 400, not a 500.
- An app with agents but no hosted key boots without the previous warning
  when a local model could serve them; a run that needs a missing key fails
  with a message naming the variable.
- `Request.user` exists (it was documented in 0.3 but not implemented).
- The agent runtime now propagates nesting depth into its tool calls. In 0.3
  it reset depth to zero, so an agent calling a behaviour calling the agent
  bypassed the nesting ceiling that behaviours alone respected.
- Unused dependencies are gone: five Rust crates (`anyhow`, `thiserror`,
  `hmac`, `async-stream`, `pin-project-lite`), four features nothing used
  (sqlx `json` and `macros`, reqwest `stream` and `charset`), and `httpx` from
  the `dev` extra.
- `app.check()` no longer embeds a full OpenAPI document that nothing read
  (every banner and `webcortex check` built one); `app.openapi()` has it.
- The manifest no longer carries fields the runtime never read: `stream` on
  agent ops (results are not streamed yet), `handler` on behaviour ops (it is
  on the behaviour), `validate_body` on routes (body validation was never
  implemented) and `server.python_workers` (workers are passed to `serve`).
- Naming a behaviour declared with `tool=False` in an agent's or a
  behaviour's `tools` is a boot error. It used to pass `check()` and then
  never be callable.
- `temperature` is unset by default and sent only when an agent or behaviour
  sets it: Claude Opus 5, Opus 4.7/4.8 and Sonnet 5 reject sampling parameters
  with a 400. Compaction, flow routing and `evolve` no longer set one.
  `max_tokens` defaults to 16,000 (4,096 left a thinking model little room for
  its answer), and a response cut off at `max_tokens` ends the run with status
  `max_tokens` instead of `completed`, without running any tool call in it.

### Fixed

- Percent-decoding panicked on a `%` followed by a multi-byte character (a 500
  through the request path's panic boundary). The three copies of the decoder,
  for query strings, the file server and the proxy path-traversal check, are
  now one that works on bytes.
- A behaviour treated any failed tool call whose error mentioned "508" (say,
  "order 5080 not found") as the nesting ceiling and halted, instead of
  raising an exception it could catch. It now matches the status the runtime
  reports.
- `security_report()["gated_tools"]`, and with it `webcortex check`, the
  startup banner and the context pack, listed a gated route without an
  explicit `tool_name` as `METHOD /path`. It now uses the tool name agents
  call, as `/_webcortex/security` always did.
- A handler parameter annotated `Request` in a module that uses
  `from __future__ import annotations` was bound as an ordinary required
  input instead of receiving the request. Annotations are resolved first now.
- The `webcortex dev` / `run` banner printed the declared host and port even
  when `WEBCORTEX_HOST` or `WEBCORTEX_PORT` moved the server. It prints the
  address that binds.
- `sqlite://:memory:` served no tables: `run()` applied the DDL on its own
  `sqlite3` connection, a separate database the runtime never saw. The runtime
  now applies the DDL through its pool at startup, and holds an in-memory
  database on one long-lived connection so every request sees the same data.
- A body over the 32 MB limit could get a connection reset instead of its
  413: the server stopped reading and closed while the client was still
  sending. It now reads and discards up to 8 MB more (for at most 2 s) before
  answering, so the 413 arrives; a larger overshoot is still cut off.

### Verified

336 tests (99 Rust, 237 Python). Clippy clean. `cargo audit` reports no
vulnerabilities.

## [0.3.2] — 2026-08-31

### Fixed

- **The extension imports on CPython 3.14.7 again.** 0.3.1 and earlier fail with
  `ValueError: module functions cannot set METH_CLASS or METH_STATIC`. The cause
  was pyo3 0.29.1, fixed upstream in 0.29.2 — but `Cargo.lock` still pinned
  0.29.1 while `Cargo.toml` required 0.29.2. Cargo resolves the newer version at
  build time, so builds were correct; `Swatinem/rust-cache` keys off `Cargo.lock`
  though, so CI kept restoring objects compiled against the broken version and
  the fix looked falsified for a day. The lock now pins 0.29.2, and the cache key
  hashes `Cargo.toml` too.

### Added

- **GIL-enabled CPython 3.14 is supported again**, and `cp314` wheels are
  published. 0.3.1 dropped both on the strength of the failure above, which was
  never a real incompatibility. Supported: 3.12, 3.13, 3.14 and free-threaded
  3.14 (`3.14t`).
- The documentation site is deployed to Railway.

### Security

- `h2` 0.4.15 → 0.4.19 in `Cargo.lock` for [RUSTSEC-2026-0258] (unbounded empty
  DATA frames). The advisory landed 17 Aug, between this release being cut and
  it being published, and the release gate's `cargo audit` rightly refused to
  ship it. `h2` is a transitive dependency (via hyper); no API involved.

[RUSTSEC-2026-0258]: https://rustsec.org/advisories/RUSTSEC-2026-0258

## [0.3.1] — 2026-08-06

Packaging and supported-interpreter corrections. No library behaviour changed.

### Removed

- **GIL-enabled CPython 3.14 is no longer supported**, and no `cp314` wheel is
  built or published. The extension cannot be imported on CPython 3.14.7:
  `ValueError: module functions cannot set METH_CLASS or METH_STATIC`. 3.12,
  3.13, 3.14.6 and the free-threaded 3.14 build are all unaffected, so this is
  neither a regression here nor a free-threading problem. Publishing a wheel
  that installs cleanly and then fails at import is worse than publishing none —
  the error arrives later, further from its cause. Use 3.13 or `3.14t`.

  0.3.0 still carries its `cp314` files; PyPI is immutable. The investigation,
  including what has been ruled out, is in AGENTS.md §11.

### Fixed

- The sdist now contains `LICENSE`. Declaring `license = { text = "Apache-2.0" }`
  makes maturin write `License-File: LICENSE` into the metadata, and PyPI
  enforces that claim — 0.3.0's sdist was rejected with
  `400 License-File LICENSE does not exist in distribution file` and that release
  shipped wheels only. Wheels were unaffected because maturin copies the file
  into `.dist-info` itself.
- The README no longer says the project is unpublished. It is the
  `long_description`, so that text was the PyPI landing page.

### Added

- `AGENTS.md`: a machine-facing reference for AI coding tools — the full API
  surface with exact signatures and defaults, the parameter-binding order, the
  scope and delegation model, and the failure modes that are cheap to hit and
  expensive to diagnose.
- CI reports the resolved interpreter and any `_core` import failure as workflow
  annotations. `3.14` is a moving target that `uv` re-resolves per run, so the
  same commit could pass and fail half an hour apart with no way to tell why.

## [0.3.0] — 2026-08-04

The first release intended for other people to install.

### Renamed

The project is now **WebCortex**. It was developed as *Pylon* and briefly carried
the name *Rango*; both are taken on PyPI.

- Import and CLI: `webcortex` (`from webcortex import WebCortex`, `webcortex dev`)
- Distribution on PyPI: **`web-cortex-framework`**. Unlike the earlier names,
  this one is not forced — both `web-cortex-framework` and `webcortex` are free.
  The longer distribution name is the project's chosen identity; the shorter
  import name is for ergonomics, the way `djangorestframework` imports as
  `rest_framework`.
- Crates: `webcortex-core`, `webcortex-py`
- Environment variables: `PYLON_*` → `WEBCORTEX_*`
- Control plane: `/_pylon/*` → `/_webcortex/*`
- Generated API keys: `pyl_…` → `wcx_…`
- OpenAPI extensions: `x-pylon-*` → `x-webcortex-*`

**This is a breaking change for anyone running v0.1–0.2.** No migration path is
provided; nothing was published, so nothing depends on the old names.

### Security

Six issues found by an adversarial review and fixed. Full detail, including what
was *not* tested, in [`SECURITY.md`](SECURITY.md).

- **Critical** — `jsonwebtoken` panicked inside `decode` because no crypto
  provider feature was enabled. Reachable pre-auth by any client sending an
  `Authorization` header; severed the connection with no status line and broke
  every JWT-configured app.
- **High** — no panic boundary on the request path. Now wrapped in
  `catch_unwind`; a panic becomes a 500 for that request, logged, never returned.
- **High** — proxy path traversal usable as an SSRF primitive: a path parameter
  could climb out of the upstream prefix.
- **High** — unbounded behaviour recursion exhausted the worker pool. Each nested
  invocation got a fresh step budget, so `max_steps` never bounded the total.
  Bounded now by `server.max_invocation_depth` (default 8).
- **Medium** — Python tracebacks were returned to clients. Now log-only;
  `WEBCORTEX_DEBUG_ERRORS=1` opts back in.
- **Low** — client input faults surfaced as 500s. `QueryError` now separates
  caller fault (400) from internal fault (500).

Also: JWT expiry leeway was silently inheriting the library's 60-second grace.
Now explicit (`leeway_secs`, default 30) with `exp` required.

### Changed

- `requires-python` is now `>=3.12`. The previous `>=3.11` was never verified;
  3.12 and 3.14 (free-threaded and GIL) are tested.
- Incremental compilation disabled for the dev profile — artifacts had reached
  several GB across rebuild cycles.

### Added

- 54-test adversarial suite (`tests/test_pentest.py`) covering auth bypass,
  injection, traversal, SSRF, privilege escalation, exhaustion, and disclosure.
- `LICENSE` (Apache-2.0), matching the declared license.
- CI: test matrix and multi-platform wheel builds.

### Verified

251 tests (66 Rust, 185 Python). Clippy clean. Soak: 1,786,805 requests over
120 seconds, 0 errors, 0 panics, memory at steady state.

## [0.2.0] — unreleased

Production hardening and the frontend story: authentication (API keys + JWT) with
scope enforcement, CORS, rate limiting, security headers, request IDs, graceful
shutdown, server-rendered pages via minijinja, static file serving, the agent
runtime with approval gates and runtime-enforced budgets, an audit trail, and
TypeScript client generation.

## [0.1.0] — unreleased

Initial proof of the core idea: a Python declaration layer compiled to a manifest
and executed by a Rust runtime, where routes that are queries, proxies, or static
responses never enter the interpreter — and every route marked `tool=True` is
simultaneously a REST endpoint, an OpenAPI operation, and a live MCP tool.
