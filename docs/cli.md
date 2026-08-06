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
| `--template`, `-t` | `api` | `api` · `fullstack` · `agent` · `behaviour` |
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
  webcortex 0.3.1  ·  supportdesk
  python 3.14.7 (free-threaded)
  12 routes, 10 served without touching Python
  8 agent tools: list_tickets, get_tickets, create_tickets, … +3
  agents: assistant
  security: auth, rate-limited, headers
  ⚠ 1 route(s) need no credential (run `webcortex security` to list them)
  approval-gated tools: create_tickets_purge
  http://127.0.0.1:8000/_webcortex/openapi.json   ·   MCP: http://127.0.0.1:8000/_webcortex/mcp
```

| Option | |
|---|---|
| `--host` | Override bind address |
| `--port` | Override bind port |
| `--workers` | Override interpreter worker count |

A `.env` file in the working directory is loaded automatically. The real
environment always wins.

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
  "security": { "auth_configured": true, "public_routes": ["GET /"], ... }
}
```

Catches: duplicate routes, duplicate tool names, agents referencing tools that
do not exist (with suggestions), approval gates on non-tools, queries without a
database, templates that do not parse, and invalid CORS.

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
list worth staring at before every deploy.

## `webcortex openapi`

Print the OpenAPI 3.1 document.

```console
$ webcortex openapi > openapi.json
```

Includes `x-webcortex-op` on each operation showing which engine serves it
(`static`, `query`, `python`, `proxy`, `page`, `files`, `agent`) and
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

Print the DDL for declared resources.

```console
$ webcortex sql > schema.sql
```

Nothing is applied — this is for review and for feeding a migration tool.

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
