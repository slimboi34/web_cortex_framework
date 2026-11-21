# CLI reference

Every command defaults to `api.py` in the current directory and expects it to
define `app`. Pass a path or `module:attr` to override.

```console
$ webcortex <command> [target] [options]
```

## `webcortex new`

Scaffold a project.

```console
$ webcortex new myapp --template fullstack
```

| Option | Default | |
|---|---|---|
| `--template`, `-t` | `api` | `api` · `fullstack` · `agent` · `behaviour` · `orchestration` · `robotics` · `hub` |
| `--directory`, `-d` | `<name>` | Target directory |
| `--description` | — | Project description |

Every starter boots with authentication, rate limiting, and security headers
enabled.

## `webcortex keygen`

Mint an API key.

```console
$ webcortex keygen
wcx_rU2Y-W1mTd2Jw4FdlYAaAJYFM5P_CE69sCwNhRG4aPk
```

256 bits of entropy. The framework stores only a SHA-256 and compares in
constant time — so this output is the only time you will see it.

## `webcortex dev` / `webcortex run`

Start the server. `dev` additionally prints the startup banner and defaults
logging to `info`.

```console
$ webcortex dev --port 3000
  webcortex 2.9.0  ·  supportdesk 0.1.0
  python 3.14.4 (free-threaded)
  21 routes, 19 served without touching Python
  14 agent tools: list_tickets, get_tickets, create_tickets, … +9
  agents: billing, technical, front_desk
  behaviours: triage
  flows: desk (route), briefing (pipeline)
  memory: notes
  models: default=claude-opus-5, fast=ollama/qwen3.5:9b
  security: auth, rate-limited, headers
  ⚠ 1 route(s) need no credential (run `webcortex security` to list them)
  approval-gated tools: create_tickets_purge
  http://127.0.0.1:8000/_webcortex/openapi.json   ·   MCP: http://127.0.0.1:8000/_webcortex/mcp
  usage: http://127.0.0.1:8000/_webcortex/usage   ·   approvals: http://127.0.0.1:8000/_webcortex/approvals
```

| Option | |
|---|---|
| `--host` | Override bind address |
| `--port` | Override bind port |
| `--workers` | Override interpreter worker count |

A `.env` file in the working directory is loaded automatically. The real
environment always wins.

`dev` does not reload: it reads `api.py` once, at boot. Restart it after an
edit. A boot failure (a template that does not parse, say) prints
`error[WCnnn]` with the file and line rather than a traceback.

## `webcortex check`

Validate the application through the Rust runtime without binding a port. Exits
non-zero on an invalid app — suitable for CI.

```console
$ webcortex check
{
  "name": "supportdesk",
  "routes": 12,
  "native_routes": 10,
  "tools": ["list_tickets", "get_tickets", "..."],
  "agents": ["assistant"],
  "security": { "auth_configured": true, "public_routes": [], ... }
}
```

Since 2.9 it also prints the route table it promises: every route with its
method, path, the engine that serves it (`query`, `python`, `agent`, …) and the
scopes it needs, and the same rows are in the JSON.

Catches: duplicate routes, duplicate tool names, agents referencing tools,
handoffs or context providers that do not exist (with suggestions), flows whose
steps are not tools, that contain themselves, or that contain an
approval-gated tool, approval gates on non-tools, a typed agent that can hand
off to an agent listing a `final_answer` route, a malformed output schema,
queries without a database, page templates that are missing or do not parse,
and invalid CORS.

## `webcortex security`

Report the public attack surface.

```console
$ webcortex security
{
  "auth_configured": true,
  "anonymous_scopes": [],
  "cors_enabled": true,
  "cors_origins": ["https://app.example.com"],
  "rate_limited": true,
  "security_headers": true,
  "public_routes": ["GET /"],
  "gated_tools": ["create_tickets_purge"],
  "agents": [{"name": "assistant", "tools": [...], "scopes": ["read"]}]
}
```

Warns on stderr when no authentication is configured. `public_routes` is the
list worth staring at before every deploy: every route a caller with no
credential can reach, which includes every route whose scopes are all granted
by `app.anonymous_scopes(...)` (since 2.9; before, only unscoped routes were
listed).

## `webcortex openapi`

Print the OpenAPI 3.1 document.

```console
$ webcortex openapi > openapi.json
```

Includes `x-webcortex-op` on each operation showing which engine serves it
(`static`, `query`, `python`, `proxy`, `page`, `files`, `agent`, `behaviour`,
`flow`) and
`x-webcortex-tool` for tool exposure.

## `webcortex tools`

Print the agent tool manifest.

```console
$ webcortex tools
{
  "tools": [
    {"name": "list_tickets", "method": "GET", "path": "/tickets",
     "description": "Return a page of tickets rows.", "executed_by": "query"}
  ],
  "agents": ["assistant"]
}
```

## `webcortex typegen`

Generate a typed TypeScript client.

```console
$ webcortex typegen --out src/api.ts
Wrote src/api.ts (12 typed methods, 284 lines)
```

| Option | Default |
|---|---|
| `--out`, `-o` | `client/api.ts` |

## `webcortex sql`

Print the DDL for declared resources and memories.

```console
$ webcortex sql > schema.sql
```

Nothing is applied — this is for review and for feeding a migration tool.

## `webcortex context`

Print the [context pack](ai-development.md#the-context-pack): the application
described for an AI coding tool, derived from the manifest.

```console
$ webcortex context > CONTEXT.md
$ webcortex context --json        # the raw manifest instead
```

`--json` replaces the value of every upstream header (`app.upstream(...,
headers=...)`) with `"<redacted>"`, so the output is safe to paste into a chat
or a ticket; `--show-secrets` prints them as declared.

## `webcortex mcp-config`

Print what an MCP client needs to reach this app: the `mcpServers` entry that
Claude Desktop, Cursor and other clients read (Streamable HTTP at
`{control_prefix}/mcp`), and the `claude mcp add` command for Claude Code.

```console
$ webcortex mcp-config
{
  "mcpServers": {
    "supportdesk": {
      "type": "http",
      "url": "http://127.0.0.1:8000/_webcortex/mcp",
      "headers": {}
    }
  }
}

# Claude Code:
claude mcp add --transport http supportdesk http://127.0.0.1:8000/_webcortex/mcp --header "x-api-key: $WEBCORTEX_API_KEY"
```

With `WEBCORTEX_API_KEY` unset, as here, the header is left for your shell to
fill in and a note goes to stderr. With it set, the key itself is written into
both, so treat the output as a secret. The URL is built from `WEBCORTEX_HOST`
and `WEBCORTEX_PORT` (or the app's own host and port) over plain HTTP; edit it
for a deployment behind TLS. The server name is the app's name reduced to
letters, digits, `_` and `-`, and the command is shell-quoted.

!!! warning "The MCP endpoint is a control-plane route"
    Once authentication is configured, `/_webcortex/mcp` needs
    `webcortex:admin`, so the key you give an MCP client is an admin key: it can
    also decide approvals, halt and release actuators, and read the audit log.
    Give it to tools you would trust with that.

## `webcortex evolve`

Ask a model to propose an extension, anchored on the context pack.

```console
$ webcortex evolve "add a reviews resource tied to books" --model fast
$ webcortex evolve "..." --out proposal.py
$ webcortex evolve "..." --json    # {summary, code, notes}
```

| Option | Default | |
|---|---|---|
| `--model`, `-m` | `default` | A model or an alias from the app's `app.models(...)` |
| `--out`, `-o` | stdout | Write the proposal to a file |
| `--json` | off | Structured output |

It prints a proposal; it never edits `api.py`. Needs the relevant key in the
environment, or an `ollama/…` model, which needs none.

## `webcortex halt` / `webcortex release`

The emergency stop for a running app. While halted, every `actuator=True`
route refuses with 423, whether HTTP, agent, behaviour, flow or MCP calls it.

```console
$ webcortex halt --reason "person in the cell"
HALTED: person in the cell. Actuators refuse to run until `webcortex release`.
$ webcortex halt --status
$ webcortex release
released: actuators enabled
```

| Option | Default | |
|---|---|---|
| `--reason`, `-r` | `emergency stop` | Recorded in the audit log |
| `--status` | off | Report the state without changing it |
| `--key` | `$WEBCORTEX_API_KEY`, then `$WEBCORTEX_TOKEN` | A key or token with `webcortex:admin` |
| `--url` | the app's host and port | The running app's base URL, e.g. `https://cell.example.com` (since 2.9) |

Without `--url`, both commands call the control plane of the app at
`WEBCORTEX_HOST` and `WEBCORTEX_PORT`, or at the app's own host and port, over
plain HTTP. Pass `--url` to reach a deployment behind TLS. See
[Vision and robotics](vision-and-robotics.md).


---

## In CI

```yaml
- run: webcortex check          # fails on an invalid application
- run: webcortex security       # review the public surface
- run: webcortex typegen --out src/api.ts
- run: git diff --exit-code src/api.ts   # fails if the client drifted
```

That last line is worth having: it fails the build when someone changes a route
without regenerating the client.

## `webcortex doctor`

Checks the app is ready to run without starting it: Python and core versions, the manifest,
every model the app uses (resolved exactly as the runtime would; keys are checked for presence,
never printed), the environment variables it names, security posture, the e-stop, watchers,
budgets, storage paths and the port. Exits 1 if any check fails. See
[Diagnostics](diagnostics.md#webcortex-doctor).

```console
$ webcortex doctor
$ webcortex doctor --online      # also probe each model provider (3 s timeout)
$ webcortex doctor --json
```

| Option | Default | |
|---|---|---|
| `--online` | off | Probe each provider endpoint |
| `--json` | off | `{"ok", "summary", "checks": [{id, status, message, hint, code?}]}` |
| `--url` | the app's host and port | Where a running instance would be |
| `--key` | `$WEBCORTEX_API_KEY` | Used when talking to a running instance |

## `webcortex status`

What the running app is doing: requests per route by status class and latency, the most recent
errors with their codes and request ids, agent runs in flight and recent, spend, budgets, the
e-stop and devices. Reads `GET {control_prefix}/status`, which needs `webcortex:admin` once auth
is configured.

```console
$ webcortex status
$ webcortex status --json
$ webcortex status --url https://api.example.com --key "$ADMIN_KEY"
```

## `webcortex trace`

What a recent agent, flow or behaviour run did, as a tree of spans: the run, each model call
with its tokens, each tool call, each handoff, and nested runs under the call that started
them. Reads `GET {control_prefix}/traces` (admin scope once auth is configured). See
[Traces](tracing.md).

```console
$ webcortex trace                      # the recent traces, newest first
$ webcortex trace latest               # the newest, as a tree
$ webcortex trace 7d1c0b…e02a --json   # one trace, as the JSON document
$ webcortex trace --url https://api.example.com --key "$ADMIN_KEY"
```

| Flag | Default | |
|---|---|---|
| `trace_id` | none | A trace id (from a run result's `trace_id` or the `x-webcortex-trace` header) or `latest`; omit to list |
| `--app` | `api.py` | The app module, for its host, port and control prefix |
| `--json` | off | The document instead of the table or tree |
| `--url` | the app's host and port | Where the running instance is |
| `--key` | `$WEBCORTEX_API_KEY`, then `$WEBCORTEX_TOKEN` | An admin credential |

A trace that is not held exits 1 with `WC219`.

## `webcortex eval`

Run the evals the app declares with `app.eval(...)`, in-process, and exit 1 when any fails.
With a recorded cassette an eval replays the model's responses: no key, no tokens, the same
result every time, with the tools running for real. See [Evals and cassettes](evals.md).

```console
$ webcortex eval                    # replay where a cassette exists, otherwise call the providers
$ webcortex eval --record           # call the providers, write evals/<name>.json
$ webcortex eval --replay           # cassettes only; an eval without one fails (use in CI)
$ webcortex eval --live             # call the providers, ignore cassettes
$ webcortex eval --only refunds --json
$ webcortex eval --list
```

| Flag | Default | |
|---|---|---|
| `--record` / `--replay` / `--live` | auto | Where model responses come from; mutually exclusive |
| `--dir` | `evals` | Where cassettes are read and written |
| `--only NAME` | all | Run one eval; repeatable |
| `--list` | off | List the declared evals and whether each has a cassette |
| `--json` | off | The full report, including each eval's result body |

## `webcortex explain`

What an error code means and what to do about it. Every error the runtime, the MCP endpoint or
the CLI reports carries one; see the [error code reference](error-codes.md).

```console
$ webcortex explain WC301
$ webcortex explain --list [--json]
```

## Errors from the CLI

A failing command prints `error[WCnnn]: …`, a `hint:` line and `see: webcortex explain WCnnn`
to stderr and exits 1. With `--json` (and always for `webcortex check`, whose output is JSON)
the failure is `{"ok": false, "error": {"code", "message", "hint"}}` on stdout.
