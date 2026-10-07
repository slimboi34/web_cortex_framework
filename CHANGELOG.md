# Changelog

Notable changes per release. Versions follow [semantic versioning](https://semver.org); before 2.0, minor
bumps could contain breaking changes.

## [2.4.1] — 2026-10-07

A security release: a second adversarial pass over the 2.x surface. Details, and what
was checked and held, are in [SECURITY.md](SECURITY.md). No public API was
removed; one behaviour is new and can refuse requests that used to succeed (the first
item under *Changed*).

### Security

- **A tool argument could steer an in-process tool call onto a different route,
  skipping its approval gate** (high). The call's path was rebuilt from the arguments
  and routed again, while approval had been checked against the tool's name:
  `touch_note(id="purge")` ran a gated `POST /notes/purge`, and a `/` in a value could
  add segments. Scopes and the e-stop still applied. Path values are now percent-encoded
  as one segment, and `dispatch` refuses (400) a tool call whose path routes anywhere
  but the named tool's route.
- **Proxy parameter injection** (medium). Since 2.0.1 path parameters are decoded, and
  the decoded value was spliced into the upstream URL raw, so `/proxy/x%3Fadmin%3D1`
  reached the upstream as `/echo/x?admin=1`. Substituted values are now encoded.
- **Caller-supplied image URLs could aim an OpenAI-compatible server at internal
  addresses** (medium). Those servers fetch `image_url` themselves. An image `url` in a
  request may no longer name a loopback, private, link-local, shared or metadata host
  (`localhost`, `10.x`, `169.254.169.254`, `[::1]`, `0x7f.1`, …). URLs a tool returns are
  unchanged.
- **MCP batches** are capped at 32 messages: the rate limiter admits a batch as one
  request, and it could carry any number of tool calls (medium).
- A **pulled camera** is read only up to `max_frame_bytes`, instead of buffering the whole
  body and checking afterwards (low).
- **Camera and upstream errors no longer echo their URL** to the client — a camera URL
  can carry `user:pass@`. The full error is logged (low).
- **Failed credentials count against the rate limit** for the client's address;
  authentication used to fail before the limiter ran (low).
- **`x-request-id`** is echoed and logged only when it is at most 128 plain characters;
  anything else is replaced with a generated id (low).
- **`webcortex.client.WebSocket`** refuses messages larger than `max_message` (new keyword,
  64 MB by default) instead of trusting a server's 2⁶³-byte frame header, and refuses header
  values containing a line break (low).

### Changed

- **Cross-origin browser requests that change state are refused.** A `POST`, `PUT`,
  `PATCH` or `DELETE`, or a WebSocket upgrade, that a browser marks as cross-origin
  (`Sec-Fetch-Site`, or an `Origin` that is not the `Host`) now gets `403` unless the
  origin is listed in `app.cors(...)`. CORS only stopped a hostile page *reading*
  responses; it could still send a simple POST or open a socket, so any page the operator
  visited could call tools, release the e-stop or watch a camera on an app reachable
  from their browser. API clients, devices, `curl` and `webcortex.client` send neither
  header and are unaffected. To accept a cross-origin browser app, list its origin in
  `app.cors(...)`.

### CI

- Every action is pinned to a full commit SHA (the tag is in a comment); checkouts do
  not persist credentials; Pages permissions are granted to the deploy job only; the
  published wheels are built without sccache; the publish job runs only for a tag;
  `release.yml` has a concurrency group and timeouts. `zizmor` reports no findings.
- The dependency audit job also runs `pip-audit` over the docs toolchain.

## [2.4.0] — 2026-10-06

### Added

- `reasoning=` on `app.agent(...)` and `@app.behaviour(...)`: `none`, `low`, `medium` or
  `high`, sent as `reasoning_effort` on the OpenAI wire format (Ollama, vLLM, LM Studio,
  OpenAI) and ignored by the Anthropic provider. Current local models think before they
  answer by default, which through Ollama meant 20 to 50 seconds per call from a 4B model
  and, often, a reply that was all deliberation; `reasoning="none"` has the same model answer
  a watcher's question about a camera frame in about a second. Validated at declaration.

## [2.3.0] — 2026-10-01

The device hub: WebCortex sits between cameras and sensors on one side and agents
on the other. Devices stream in, agents look, and insights stream out to subscribers and
other systems. The Rust core does the work, and no Python runs per frame.

### Added

- **`app.camera(name, …)` and `app.sensor(name, …)`.** Frames and telemetry live in
  per-device ring buffers in the Rust core. A camera is **pushed** (WebSocket binary
  messages to `/devices/<name>/ws`, or an HTTP POST of the encoded image) or **pulled**
  (`source=` an IP camera's snapshot URL, fetched when a frame is asked for). Media types
  are sniffed from the bytes; `max_fps` and `max_frame_bytes` are enforced per device
  (429 over HTTP, a silent drop over WebSocket). Ingest and read are separately scoped
  (`devices:ingest`, `devices:read` by default).
- **Device tools.** `<name>_snapshot` (the newest frame as an image the agent sees, its
  age, and the latest telemetry), `<name>_telemetry` and `<name>_insights`, served in
  Rust and available over MCP.
- **WebSocket streams.** `/devices/<name>/stream` sends every frame (a JSON header, then
  the image as a binary message; `?frames=meta` for headers only), telemetry reading and
  insight as it happens. Slow subscribers are told how many events they missed instead
  of slowing the device down.
- **Watchers.** `app.watch(name, device=, agent=, input=, every=, max_runs_per_hour=,
  webhook=)` hands the newest frame and telemetry to an agent on a timer, skipping ticks
  where nothing changed (compared by content), and publishes the answer as an insight to
  subscribers, `<device>_insights`, the audit log and an optional webhook. A watcher's
  agent runs with exactly the watcher's `scopes`.
- **Agents over WebSocket.** Every agent route accepts a socket: send `{"input",
  "images"?, "session_id"?}`, receive `{"type": "step"}` as each step happens, then
  `{"type": "result"}`, and keep the socket for the next turn. `RunOptions.events`
  exposes the same step stream to Rust callers.
- **Browser pages.** `/devices/<name>/connect` turns a phone's or laptop's camera into
  the device; `/devices/<name>/view` is a live monitor of frames, telemetry and insights.
  Browsers may authenticate a WebSocket upgrade with `?access_token=`.
- **`webcortex.client`**, standard library only: `DeviceConnection` (a driver pushing
  frames and telemetry), `subscribe()` (a consumer of events), `AgentSocket` (a service
  driving an agent turn by turn), and a small RFC 6455 `WebSocket`.
- `GET /_webcortex/devices`; `devices` and `watchers` in `/health`, the security report,
  the boot banner and the context pack.
- **`webcortex new NAME -t hub`**: a pushed camera, a sensor, an inspector agent and a
  watcher with a webhook, and separate keys for operator, device and reader.
- Docs: [The device hub](docs/device-hub.md).

### Changed

- Connections are served with HTTP upgrades enabled.
- `App::authorize(method, path, principal)` matches a route and checks its scopes
  without running it; WebSocket upgrades use it.

## [2.2.0] — 2026-10-01

Agents that see, and machines they may only move with permission.

### Added — perception

- **`webcortex.Image`.** Return one from any handler, alone or nested anywhere in the
  result, and an agent calling that tool receives the picture itself. Constructors:
  `from_array` (NumPy `uint8` arrays or nested lists, encoded as PNG with the standard
  library; `bgr=True` for OpenCV frames), `from_bytes` (the type is sniffed), `from_path`,
  `from_pil`, `from_url`. Neither NumPy nor Pillow is a dependency. A route annotated
  `-> Image` advertises the shape in OpenAPI.
- **Images in agent runs.** The runtime lifts each `{"$image": …}` marker out of a tool
  result *before* bounding, leaves a placeholder in the text, and attaches the image to
  the `tool_result`. It works in both wire formats: Anthropic image blocks, and for
  OpenAI-compatible servers (local vision models), `image_url` parts in a user message
  after the tool messages. Step records and the audit log keep each image's size, never
  its base64.
- **Images with the input.** Every agent endpoint takes `"images": [...]` (up to 16, each
  5 MB at most, as `{"media_type", "data"}` or `{"url"}`). A bad item is a 400 that names it.
- **`max_images` (default 4).** Only the newest images stay in context as pixels; older
  ones become a line of text, counted in `usage.images_dropped`. A camera tool called in a
  loop no longer resends every frame on every step.
- **MCP image content.** `tools/call` returns tool images as `image` items, so Claude Code
  and other MCP clients see the frame. `structuredContent` holds the redacted value.

### Added — physical tools

- **`actuator=True`** on any route marks it as moving hardware. MCP `tools/list` reports
  it, with `destructiveHint` and `openWorldHint`.
- **An emergency stop.** `POST /_webcortex/halt` (with an optional `reason`) and
  `POST /_webcortex/release`, `GET /_webcortex/halt`, plus `webcortex halt` and
  `webcortex release` on the command line. While halted, every actuator answers 423
  before its handler runs. The check sits in `App::dispatch`, so HTTP, agent tool calls,
  behaviours, flows, MCP and approvals granted after the halt are all refused. Reads keep
  working. Halting and releasing are audited, `/health` reports `halted`, and both routes
  need `webcortex:admin`.
- **`WebCortex(..., start_halted=True)`** boots with the stop engaged, so a crash and
  restart does not re-arm hardware.
- **`security_report()["actuators"]`** and `actuator_warnings`. The boot banner and the
  context pack flag actuators with no authentication, with no scope, or exposed to agents
  without `approval="required"`.
- **`webcortex new NAME -t robotics`.** A simulated two-joint arm, gripper and camera; an
  inspector agent that only looks and an operator agent whose every move needs approval;
  `observe` and `operate` scopes; joint limits enforced in code; boots halted. Replace the
  `Cell` class with your driver.
- Docs: [Vision and robotics](docs/vision-and-robotics.md).

### Changed

- `FakeProvider` reads the text of a message that carries images, and appends
  ` [saw N image(s)]` when the last message held any, so offline tests can see pixels
  arrive.
- The DESIGN roadmap puts perception and physical tools ahead of SSE streaming.

## [2.1.1] — 2026-09-30

### Fixed

- **MCP results for tools that return a list or a scalar.** `tools/call` put the tool's raw
  JSON in `structuredContent`, but the protocol requires an object there, and strict clients
  (Claude Code among them) reject the bare value as malformed — which made every `list_*`
  resource route unusable as a tool even though the call succeeded. A non-object result is
  now wrapped as `{"result": …}`; objects pass through unchanged, and the text content is
  still the raw JSON. Found by recording Claude Code driving a scaffolded app over MCP.

### Corrected

- 2.1.0's note said fat LTO made wheels "a little faster and smaller". Measured on the same
  app and routes with ApacheBench, throughput did not change beyond run-to-run noise; the
  wheels are about 3% smaller (5.43 → 5.26 MB on macOS arm64). The note below now says so.

## [2.1.0] — 2026-09-30

### Added — for the author who is a model

- **Every starter ships `AGENTS.md` and `CLAUDE.md`.** `webcortex new` writes the rules a
  coding agent needs in this project — declare, don't compute; `tool=True` and scopes on
  every route; `approval="required"` on anything destructive; agents cannot exceed their
  caller; keep it to one file — with the commands to run before starting
  (`webcortex context`) and after every change (`webcortex check`). `CLAUDE.md` imports it
  with Claude Code's `@AGENTS.md` syntax; Codex, Cursor and most agents read `AGENTS.md`
  directly.
- **`webcortex mcp-config`** prints the MCP client configuration for the app — streamable
  HTTP at `/_webcortex/mcp` with the API key as the `x-api-key` header, so the client gets
  exactly that key's scopes — and the `claude mcp add --transport http …` line. Plugging an
  app into Claude Code, Claude Desktop or Cursor is one command.

### Changed

- **Release builds use fat LTO** (`lto = "fat"`, was thin): the whole program is optimised as
  one unit. Measured: wheels about 3% smaller; throughput unchanged within noise (see 2.1.1).
- `client/`, where `webcortex typegen` writes by default, is ignored by git.

## [2.0.1] — 2026-09-29

### Fixed

- **Path parameters are percent-decoded.** A captured path segment reached the
  query, the Python handler and the tool arguments exactly as it appeared on the
  wire, so `GET /books/by-author/Frank%20Herbert` bound `author = "Frank%20Herbert"`
  and matched nothing, and `GET /books/%31/blurb` handed an `id: int` handler the
  string `"%31"`. The router now decodes each captured value once, *after*
  matching, so an encoded `%2F` can never become a segment separator and change
  which route is chosen; a value that is not valid UTF-8 once decoded is kept as
  sent. Query-string values were already decoded and are unchanged.

## [2.0.0] — 2026-09-17

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

### Security

- `rustls` 0.23.43 → 0.23.45 (and `rustls-webpki` 0.103.13 → 0.103.15 with
  it) in `Cargo.lock` for [RUSTSEC-2026-0285]: TLS 1.3 handshake messages were
  accepted across encryption-level boundaries. The advisory was published on
  14 September, after this release was reviewed, and the release gate's
  `cargo audit` rightly refused it. `rustls` is transitive, via reqwest's TLS
  for outbound calls to model providers and proxied upstreams; no API is
  involved.

[RUSTSEC-2026-0285]: https://rustsec.org/advisories/RUSTSEC-2026-0285

### Verified

355 tests (106 Rust, 249 Python). Clippy clean. `cargo audit` reports no
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
