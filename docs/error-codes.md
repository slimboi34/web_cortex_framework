# Error code reference

Every error WebCortex reports carries a stable code. A code keeps its meaning once released, so
clients, tests and coding agents can branch on `WC402` rather than on a message's wording. The
table lives in the Rust core (`crates/webcortex-core/src/errors.rs`); this page lists the same
entries. `webcortex explain CODE` prints one from the command line, and the built-in MCP tool
`webcortex_explain` answers the same question for an agent.

Where a code appears: the `code` and `hint` fields of an HTTP error body (and the
`x-webcortex-error` response header), `code`/`hint` on an agent run that did not complete and
`code` on its failed steps, `structuredContent.error` of an MCP tool error, the `errors.recent`
ring of [`/_webcortex/status`](diagnostics.md#the-live-status-view), `webcortex doctor` checks,
and CLI failures (`error[WC101]: …`).

## Internal faults (WC0xx)

### WC001 — Internal error {#wc001}

*internal · HTTP 500*

Something failed inside the server. The server log has the full error under this response's x-request-id; set WEBCORTEX_DEBUG_ERRORS=1 locally to return Python tracebacks.

### WC002 — Python handler raised {#wc002}

*internal · HTTP 500*

An unhandled exception escaped a Python handler; its traceback is in the server log. Raise webcortex.HTTPError(status, message) for failures the caller should see.

### WC003 — Behaviour failed {#wc003}

*internal · HTTP 500*

A behaviour raised an exception; its traceback is in the server log. Use ctx.halt(reason) to stop a behaviour on purpose.

### WC004 — Template failed to render {#wc004}

*internal · HTTP 500*

A page template is missing or raised while rendering; check the template name and the data the page binds.

## Configuration and environment (WC1xx)

### WC101 — Invalid application {#wc101}

*config*

The app's declarations do not validate. The message names the declaration; fix it in api.py and run `webcortex check`.

### WC102 — App failed to load {#wc102}

*config*

Importing the app module raised, or it does not define `app`. Run `python api.py` to see the traceback, or pass module:attr.

### WC103 — Core extension mismatch {#wc103}

*config*

The compiled extension (webcortex._core) is missing or from a different version than the Python package. Reinstall the wheel, or run `maturin develop` in a working copy.

### WC104 — Port already in use {#wc104}

*config*

Another process is listening on the app's port. Stop it, or choose another with --port or WEBCORTEX_PORT.

### WC105 — No database configured {#wc105}

*config · HTTP 500*

A route needs SQL but WebCortex(...) was given no database=. Pass database="sqlite://./app.db" or set WEBCORTEX_DATABASE_URL.

### WC106 — Environment variable not set {#wc106}

*config*

A declaration names an environment variable that is unset (an API key, a bearer token, a provider key). Export it or add it to .env; values are never printed.

### WC107 — Path does not exist {#wc107}

*config*

A configured directory or database path does not exist. Create it or correct the path.

### WC108 — Running app not reachable {#wc108}

*config*

Nothing answered at the app's address. Start it with `webcortex dev`, or point the command at it with --url or WEBCORTEX_HOST/WEBCORTEX_PORT.

## Routing, HTTP and request input (WC2xx)

### WC200 — Request failed {#wc200}

*http*

The handler answered with an error status and no more specific code. Read the message; a handler can raise HTTPError(status, message, code=..., hint=...) to say more.

### WC201 — No such route {#wc201}

*http · HTTP 404*

No route matches this path. `webcortex check` lists every route, and GET /_webcortex/routes lists them on a running app.

### WC202 — Method not allowed {#wc202}

*http · HTTP 405*

The path exists but not for this method; the `allow` header lists the methods it takes.

### WC203 — Request timed out {#wc203}

*http · HTTP 504*

The handler ran past request_timeout (or agent_timeout for agents, behaviours and flows). Raise the limit on WebCortex(...) or make the handler faster.

### WC204 — Request body too large {#wc204}

*http · HTTP 413*

The body is over the 32 MB limit. Send large files to a static or device ingest route in pieces, or reference them by URL.

### WC205 — Bad request {#wc205}

*http · HTTP 400*

The request could not be used as sent: check the JSON body and parameter types against the route's schema in /_webcortex/openapi.json.

### WC206 — Missing or invalid parameter {#wc206}

*http · HTTP 422*

A required parameter was absent or could not be read. Send every required field named in the route's input schema (path, query string or JSON body).

### WC207 — Not found {#wc207}

*http · HTTP 404*

The route exists but the thing it looks up does not: check the id or key you sent.

### WC208 — Tool arguments address another route {#wc208}

*http · HTTP 400*

A path argument would have routed this tool call to a different route. Pass a plain value for the path parameter, not a path.

### WC209 — Invalid path parameter {#wc209}

*http · HTTP 400*

A path parameter contained a slash, '..', or a control character. Path parameters are single segments.

### WC210 — Upstream failed {#wc210}

*http · HTTP 502*

A proxied upstream or a pulled camera did not answer properly. Check that it is reachable from the server and that its bearer_env is set.

### WC211 — Database rejected the query {#wc211}

*http · HTTP 400*

SQLite refused the statement or its values (a constraint, a type). The message is SQLite's; check the values you sent.

### WC212 — No such control endpoint {#wc212}

*http · HTTP 404*

That path is not part of the control plane. GET /_webcortex/health, /status, /routes, /tools, /usage and /audit are the common ones.

### WC213 — WebSocket required {#wc213}

*http · HTTP 426*

This route is a WebSocket. Connect with `Upgrade: websocket` (webcortex.client.WebSocket or DeviceConnection).

### WC214 — Conflict {#wc214}

*http · HTTP 409*

The request conflicts with the current state of the resource; re-read it and retry.

### WC215 — Unavailable {#wc215}

*http · HTTP 503*

A subsystem this route needs is not running in this process (for example the agent runtime). Check the server log at boot.

### WC216 — Request body too slow {#wc216}

*http · HTTP 408*

The client announced a body but did not finish sending it within request_timeout (agent_timeout on agent, behaviour and flow routes). Send the whole body promptly, or raise the limit on WebCortex(...).

### WC217 — Upstream response too large {#wc217}

*http · HTTP 502*

A proxied upstream answered with a body over its max_response_bytes (64 MiB by default). Raise it on app.upstream(..., max_response_bytes=...) or page the upstream's response.

### WC218 — Too many connections {#wc218}

*http · HTTP 503*

The server already holds max_connections open connections (10,000 by default), WebSockets included. Retry shortly, close idle connections, or raise max_connections on WebCortex(...).

## Agents, behaviours, flows and providers (WC3xx)

### WC300 — Agent run failed {#wc300}

*agent · HTTP 502*

The run stopped on an error. The last step's `error` says which call failed; GET /_webcortex/status shows recent runs.

### WC301 — Provider key not set {#wc301}

*agent · HTTP 502*

The model's provider needs an API key that is not set. Export ANTHROPIC_API_KEY or OPENAI_API_KEY (or the provider's api_key_env), use a local ollama/<model>, or set WEBCORTEX_FAKE_PROVIDER=1 for tests. `webcortex doctor` checks every model.

### WC302 — Undeclared provider {#wc302}

*agent · HTTP 502*

The model name has a prefix no provider answers to. Declare it with app.provider(name, base_url=...), or use ollama/, openai/ or anthropic/.

### WC303 — Provider unreachable {#wc303}

*agent · HTTP 502*

The model provider could not be reached. Check the base URL and network; for Ollama run `ollama serve` (OLLAMA_HOST). `webcortex doctor --online` probes each provider.

### WC304 — Provider returned an error {#wc304}

*agent · HTTP 502*

The model provider answered with an error status (bad model name, overloaded, invalid request). The message carries the provider's own detail.

### WC305 — Unreadable provider response {#wc305}

*agent · HTTP 502*

The provider answered with something that is not its wire format. Check that the base URL points at the API, not a web page.

### WC306 — Malformed agent request {#wc306}

*agent · HTTP 400*

An agent route takes {"input": str, "session_id"?: str, "reset"?: bool, "images"?: [...]}; `input` is required and must be a string.

### WC307 — Tool not available to agent {#wc307}

*agent*

The model asked for a tool this agent was not given. Add it to the agent's tools=[...] if it should have it.

### WC308 — Tool call failed {#wc308}

*agent*

A tool the agent called answered with an error; its code and message are in the step. The model sees the error and may retry.

### WC309 — Approval required {#wc309}

*agent*

This tool is gated with approval="required". A human decides with POST /_webcortex/approvals/{id}; it cannot be called directly over MCP.

### WC310 — Approval not found {#wc310}

*agent · HTTP 404*

No pending approval has that id: it was already decided, or it expired (approval_ttl_secs). GET /_webcortex/approvals lists the pending ones.

### WC311 — Context provider failed {#wc311}

*agent · HTTP 502*

A context provider named in context=[...] failed while the run started. Check its SQL or Python handler.

### WC312 — Response cut off at max_tokens {#wc312}

*agent*

The model ran out of max_tokens mid-answer, and no tool call in that response was run. Raise max_tokens, or reasoning="none" for local thinking models.

### WC313 — Invalid image input {#wc313}

*agent · HTTP 400*

An image in `images` is not a supported {"$image": ...} marker, is too large, or names an internal host. Use webcortex.Image.

### WC314 — Unknown tool {#wc314}

*agent*

No exposed tool has this name. tools/list (MCP) or GET /_webcortex/tools shows the names this caller can use.

### WC315 — Flow step failed {#wc315}

*agent · HTTP 500*

A step of the flow failed or was refused; the message names the step. Run that tool alone to see its error.

### WC316 — Watcher run failed {#wc316}

*agent*

A watcher's agent run failed; the insight carries the error. GET /_webcortex/status shows the watcher's recent runs.

### WC317 — Malformed tool call {#wc317}

*agent*

The model asked for a tool with arguments that are not a JSON object (cut off, garbled, or a bare value), so the tool was not run. The model is told and may retry; a small local model that keeps doing it needs a larger max_tokens or a simpler input schema.

## Budgets and limits (WC4xx)

### WC401 — Rate limited {#wc401}

*limits · HTTP 429*

Too many requests from this caller. Wait for retry-after seconds, or raise app.rate_limit(per_second=..., burst=...).

### WC402 — Token budget exhausted {#wc402}

*limits*

The run hit its token_budget (its own, or the request tree's shared one). Raise token_budget, use a cheaper model alias, or bound context with tool_result_limit / context_window.

### WC403 — Nesting too deep {#wc403}

*limits · HTTP 508*

Agents, behaviours or flows called each other past max_invocation_depth: usually something calling itself. Break the cycle.

### WC404 — Step limit reached {#wc404}

*limits*

The run used max_steps before the model stopped asking for tools. Raise max_steps, or give the agent fewer, sharper tools.

### WC405 — MCP batch too large {#wc405}

*limits*

A JSON-RPC batch may hold at most 32 messages. Split it.

### WC406 — Returned generator too large {#wc406}

*limits · HTTP 500*

A handler, behaviour or tool returned a generator (or other lazy iterable) that went past WEBCORTEX_MAX_RESULT_ITEMS (default 100000) or WEBCORTEX_MAX_RESULT_BYTES (default 64 MiB). A response is sent whole, so return a bounded result: page it, or raise the limits.

## Devices, actuators and the emergency stop (WC5xx)

### WC501 — Unknown device {#wc501}

*devices · HTTP 404*

No device has that name. Declare it with app.camera(name) or app.sensor(name); GET /_webcortex/devices lists them.

### WC502 — Actuators halted {#wc502}

*devices · HTTP 423*

The emergency stop is engaged, so nothing that moves hardware runs. An operator releases it with `webcortex release` or POST /_webcortex/release (admin scope).

### WC503 — No frame yet {#wc503}

*devices · HTTP 404*

The device has not sent a frame. Open /devices/<name>/connect on the camera, or push frames to its ingest route.

### WC504 — Frame faster than max_fps {#wc504}

*devices · HTTP 429*

The device is sending faster than its max_fps; the frame was dropped. Slow the sender or raise max_fps.

### WC505 — Invalid frame or telemetry {#wc505}

*devices · HTTP 422*

The frame is not a PNG/JPEG/GIF/WebP, is over max_frame_bytes, or the telemetry is not a JSON body (a body that is not JSON at all is answered 400; the rest 422).

### WC506 — Camera unreachable {#wc506}

*devices · HTTP 502*

A pulled IP camera did not answer with a snapshot. Check its URL, its bearer_env, and that it is reachable from the server.

## Authentication and security (WC6xx)

### WC601 — Authentication required {#wc601}

*auth · HTTP 401*

This route needs a credential. Send the API key in the x-api-key header (or Authorization: Bearer <jwt>).

### WC602 — Missing scope {#wc602}

*auth · HTTP 403*

The caller is authenticated but lacks a scope this route requires; the message names it. Use a key whose app.api_key(..., scopes=[...]) includes it.

### WC603 — Invalid credential {#wc603}

*auth · HTTP 401*

The API key or token was not accepted (unknown, expired or malformed). Check the key, or mint one with `webcortex keygen` and register it with app.api_key(env_var, ...).

### WC604 — Admin scope required {#wc604}

*auth · HTTP 401*

The /_webcortex control plane needs the 'webcortex:admin' scope once any auth is configured. Use an admin key: app.api_key(env, id=..., scopes=['webcortex:admin']).

### WC605 — Cross-origin request refused {#wc605}

*auth · HTTP 403*

A browser on another origin sent a state-changing request or a WebSocket upgrade. List that origin in app.cors(...) if it should be allowed.

### WC606 — CORS origin not allowed {#wc606}

*auth · HTTP 403*

The preflight's Origin is not in app.cors(...). Add it there.

### WC607 — Needs an authenticated caller {#wc607}

*auth · HTTP 401*

Anonymous callers share one identity, so @principal-scoped data and agent sessions need a real credential.

### WC608 — No authentication configured {#wc608}

*auth*

Every route is public. Add app.api_key('WEBCORTEX_API_KEY', id='service', scopes=[...]) before exposing the app beyond localhost.

### WC609 — Unauthorized (raised by the app) {#wc609}

*auth · HTTP 401*

The app's own code answered 401 (for example a wrong password at login); its message says why. WebCortex's own credential checks answer WC601, WC603 or WC604 instead.

### WC610 — Forbidden (raised by the app) {#wc610}

*auth · HTTP 403*

The app's own code refused this caller with 403; its message says why. A missing WebCortex scope answers WC602 and a cross-origin refusal WC605 instead.
