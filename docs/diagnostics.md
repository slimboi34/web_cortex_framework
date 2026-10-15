# Diagnostics: codes, doctor and the live view

WebCortex 2.5 answers three questions an operator — or a coding agent — asks when
something is wrong:

1. **Is it set up right?** `webcortex doctor`, before it runs.
2. **What exactly failed, and what do I do?** A stable error code and a hint on every
   failure, wherever it surfaces.
3. **What is the running app doing?** `GET /_webcortex/status`, `webcortex status`, and the
   `webcortex_status` MCP tool.

## Error codes and hints

Every error that reaches a user carries a `code` from one catalogue and a `hint` saying what to
do. The existing fields are unchanged; `code` and `hint` are added beside them, so a client
written for 2.4 keeps working.

```json
{"error": {"status": 423, "message": "actuators are halted (person in the cell); …",
           "code": "WC502",
           "hint": "The emergency stop is engaged, so nothing that moves hardware runs. An operator releases it with `webcortex release` or POST /_webcortex/release (admin scope)."}}
```

The code is also sent as the `x-webcortex-error` response header, so a proxy or a log pipeline
can count failures by kind without parsing bodies.

| Range | Area |
|---|---|
| `WC0xx` | internal faults |
| `WC1xx` | configuration, boot and the environment |
| `WC2xx` | routing, HTTP and request input |
| `WC3xx` | agents, behaviours, flows and model providers |
| `WC4xx` | budgets and limits |
| `WC5xx` | devices, actuators and the emergency stop |
| `WC6xx` | authentication and security |

The full list, with each hint, is the [error code reference](error-codes.md). A code keeps its
meaning once released: branch on `WC402`, not on the wording of a message.

### Where codes appear

| Surface | Shape |
|---|---|
| HTTP error | `{"error": {"status", "message", "code", "hint"}}` and header `x-webcortex-error` |
| Agent run that did not complete | `"code"` and `"hint"` on the run result (`failed` → the failing step's `WC3xx`, `budget_exhausted` → `WC402`, `step_limit` → `WC404`, `max_tokens` → `WC312`); absent for `completed` and `awaiting_approval` |
| A failed step | `"code"` on the step: a refused tool `WC307`, a tool that answered with an error carries *that* error's code (a missing argument is `WC206`), a provider failure `WC301`–`WC305` |
| MCP tool error | `isError: true`, the hint appended to the text, and `structuredContent: {"error": {"code", "message", "hint"}}`; JSON-RPC errors carry `error.data.code` |
| Watcher insight | `"code"` beside `"error"` when the run failed |
| CLI | `error[WC101]: …` / `hint: …` on stderr, or `{"ok": false, "error": {…}}` with `--json` (and always for `webcortex check`), exit status 1 |

### From your own handlers

`HTTPError` takes a code and a hint:

```python
from webcortex import HTTPError

@app.post("/orders/{id}/ship", tool=True, scopes=["write"])
def ship(id: int) -> dict:
    if not paid(id):
        raise HTTPError(409, f"order {id} is not paid", code="WC214",
                        hint="Take payment with pay_order first, then ship.")
    ...
```

Without `code=`, the catalogued code for the status is used (`409` → `WC214`, `422` → `WC206`,
…). The hint is what an agent reads to recover, so write it as an instruction.

### Explaining a code

```console
$ webcortex explain WC402
WC402  Token budget exhausted  ·  limits

The run hit its token_budget (its own, or the request tree's shared one). Raise token_budget, use a cheaper model alias, or bound context with tool_result_limit / context_window.
$ webcortex explain --list
$ webcortex explain --list --json
```

On a running app, `GET /_webcortex/errors` returns the catalogue, and an MCP client can call
`webcortex_explain` or read `webcortex://errors`.

## `webcortex doctor`

Checks an app before it runs, without starting it. Each check is
`{id, status: ok|warn|fail, message, hint, code?}`; the command exits 1 if any check failed.

```console
$ webcortex doctor
  ok    python: Python 3.13.13 (GIL-enabled)
  ok    core: webcortex 2.5.0, core extension 2.5.0
  ok    app: app 'bookstore' 0.1.0 loaded
  ok    manifest: 20 routes (17 native), 18 tools, 3 agents, 1 behaviours, 1 flows
  FAIL  model:default [WC301]: default → claude-opus-5 (used by agent librarian): model "claude-opus-5" needs ANTHROPIC_API_KEY, which is not set. …
          hint: The model's provider needs an API key that is not set. …
  FAIL  api_key:WEBCORTEX_API_KEY [WC106]: WEBCORTEX_API_KEY is not set: principal 'service' cannot authenticate
          hint: export WEBCORTEX_API_KEY=$(webcortex keygen)
  ok    budgets: every agent and behaviour has a token_budget
  ok    port: 127.0.0.1:8000 is free

  8 ok, 0 warning(s), 2 failure(s)
```

What it checks:

| Check | Fails / warns when |
|---|---|
| `python`, `core` | the interpreter is older than 3.12, or the compiled core is missing or a different version from the Python package (`WC103`) |
| `app`, `manifest` | the module does not import (`WC102`) or a declaration is invalid (`WC101`) |
| `model:<name>` | a model an agent, behaviour, compaction or flow classifier uses does not resolve: a missing key (`WC301`), an undeclared provider prefix (`WC302`). This is the runtime's own routing, so the message is exactly what a run would fail with |
| `online:<url>` | with `--online`: a provider endpoint does not answer within 3 s (`WC303`) |
| `api_key:<ENV>`, `jwt`, `upstream:<name>`, `provider:<name>` | an environment variable the app names is unset (`WC106`). Only presence is read; values never appear in the report |
| `security.*` | no authentication (`WC608`), no rate limit, `cors("*")`, every actuator warning from `security_report()` |
| `estop` | actuators that are armed at boot (`start_halted=False`) |
| `watcher:<name>`, `device:<name>` | a watcher whose agent has no `token_budget`, a plain-http webhook, a camera `bearer_env` that is unset |
| `budgets` | agents or behaviours without `token_budget` |
| `database`, `templates` | a SQLite directory or templates directory that does not exist (`WC107`) |
| `port` / `running` | the port is taken by something that is not this app (`WC104`); when this app is already running, it reports its version and live e-stop state |

`webcortex doctor --json` prints the report as JSON; `app.doctor()` and
`webcortex.doctor(app)` return it.

## The live status view

`GET /_webcortex/status` (admin scope once any auth is configured, exactly like every other
control-plane route) describes what the running app is doing:

| Section | Contents |
|---|---|
| top level | `app`, `app_version`, `webcortex` (core version), `uptime_secs`, `started_at_ms`, `python_workers` |
| `requests` | totals and, per route pattern, request counts by status class, mean / p50 / p95 / p99 / max latency |
| `errors` | total failures and the last 100: `{at_ms, source: http|mcp|agent|watcher, route, status, code, request_id}` |
| `agents` | runs `in_flight` (agent, caller, depth, steps, tokens so far, running time) and the last 50 `recent` (outcome, code, steps, tool calls, tokens, tree tokens, cost, duration); per-agent tallies; sessions; pending approvals |
| `usage` | the spend ledger's totals: tokens, estimated cost (null when a model is unpriced), by model |
| `budgets` | each agent, behaviour and flow: its `token_budget` and `max_steps`, tokens spent, largest run, how often it hit a limit |
| `estop` | halted or armed, the halt state, the actuator tools |
| `devices` | per device: connected, recent fps, frames in and dropped, last frame and telemetry age, subscribers; per watcher: runs, failures, last outcome and code |

Request ids are the `x-request-id` the client sent (or the one the server assigned and returned),
so a failure a user reports can be found here. Nothing in the view holds a body, a header, a
query string or a credential; a failure's route is the route *pattern* (`GET /books/{id}`), not
the concrete path, wherever a route matched.

### Cost of recording

Counting is lock-free: each route has a slot of relaxed atomic counters and a 40-bucket latency
histogram, chosen by route id. Only a failure (into a bounded ring) and an agent run (already
measured in seconds) take a lock. Measured with `ab`, interleaved against 2.4.0: a Rust static
route is within noise (−0.8%), and a flood in which *every* request is a 404 is about 3% slower;
see
[benchmarks/diagnostics/RESULTS.md](https://github.com/slimboi34/web_cortex_framework/blob/main/benchmarks/diagnostics/RESULTS.md).
Percentiles are reported as the upper bound of their bucket, so they are never optimistic and
within 2x of the truth.

### From the command line

Illustrative output:

```console
$ export WEBCORTEX_API_KEY=…   # an admin key
$ webcortex status
  bookstore 0.1.0 · webcortex 2.5.0 · up 0h12m03s
  requests: 1840  2xx 1801  4xx 37  5xx 2  p50 0.256ms  p95 1.024ms  max 812.4ms
     1203  GET /books  (4xx 0, 5xx 0, p95 0.512ms)
  errors: 39 total; most recent:
    WC602  403  POST /books  4c1f…
    WC304  agent  agent librarian  9e2a…
  recent runs:
    failed            librarian  1 steps  0 tokens  WC304
    completed         librarian  3 steps  2210 tokens  $0.0081
$ webcortex status --json
$ webcortex status --url https://api.example.com --key "$ADMIN_KEY"
```

### For a coding agent

The app's own MCP endpoint (the one `webcortex mcp-config` connects) offers, to callers who
may read the control plane:

- the tool **`webcortex_status`** (`section`: `requests | errors | agents | usage | budgets |
  estop | devices`, or omit for everything),
- the tool **`webcortex_explain`** (`code`),
- the resources **`webcortex://status`** and **`webcortex://errors`**.

So "why did that request fail?" is one tool call: find the request id in `errors.recent`, then
explain its code. An app tool with the same name takes precedence over a built-in.

## Testing failure paths offline

With `WEBCORTEX_FAKE_PROVIDER=1`, an agent input of `fail:<message>` makes the fake provider fail
the call with that message, the way a real provider error reads:

```python
status, out = post("/agents/desk", {"input": "fail:anthropic 529: overloaded"})
assert status == 502 and out["code"] == "WC304"
```

The diagnostics benchmark (`benchmarks/diagnostics/`) uses this (and a missing key, and a closed port) to score
eighteen broken scenarios against 2.4.0 and 2.5.0.
