# Tutorial

We will build a **support-desk API** that a human dashboard and an AI agent both
use — with the agent held to strictly less authority than the humans.

By the end you will have a REST API, an OpenAPI document, a live MCP server, a
server-rendered dashboard, a typed TypeScript client, and an agent that can
propose a destructive action but never perform one without a human's approval.

!!! tip "Follow along"
    Every snippet is complete and runnable. Total time ~20 minutes.

## 1. Scaffold

```console
$ webcortex new supportdesk --template api
$ cd supportdesk
$ export WEBCORTEX_API_KEY=$(webcortex keygen)
```

`keygen` mints a key like `wcx_rU2Y-W1mTd2Jw4Fd...`. The framework only ever
stores a SHA-256 of it and compares in constant time.

## 2. Model the data

Replace `api.py`:

```python title="api.py"
from webcortex import HTTPError, WebCortex

app = WebCortex(
    "supportdesk",
    description="Support tickets, for humans and agents.",
    database="sqlite://./supportdesk.db",
)

# --- Security first. Everything below is deny-by-default. ---
app.api_key("WEBCORTEX_API_KEY", id="service",
            scopes=["read", "write", "admin", "webcortex:admin"])
app.rate_limit(per_second=50, burst=100)

# --- Data: five routes and five tools, all executed in Rust. ---
app.resource(
    "tickets",
    fields={"id": int, "subject": str, "body": str,
            "status": str, "priority": int},
    tools=True,
    read_scopes=["read"],
    write_scopes=["write"],
)
```

Run it:

```console
$ webcortex dev
  webcortex 2.9.1  ·  supportdesk 0.1.0
  python 3.14.7 (free-threaded)
  6 routes, 6 served without touching Python
  5 agent tools: list_tickets, get_tickets, create_tickets, update_tickets, delete_tickets
  security: auth, rate-limited, headers
  ⚠ 1 route(s) need no credential (run `webcortex security` to list them)
```

**6 of 6 routes served without touching Python.** Nothing you wrote runs per
request yet — it is all declaration. The five resource routes are joined by
`GET /`, a small discovery document (the app's name and where its OpenAPI
document lives) that every app gets until it declares its own `/`. It is the
one route that needs no credential, which is what the warning is about; step 7
replaces it.

Try it:

```console
$ curl -X POST localhost:8000/tickets -H "x-api-key: $WEBCORTEX_API_KEY" \
    -H 'content-type: application/json' \
    -d '{"subject":"Login broken","body":"Cannot sign in","status":"open","priority":3}'
{"body":"Cannot sign in","id":1,"priority":3,"status":"open","subject":"Login broken"}

$ curl localhost:8000/tickets -H "x-api-key: $WEBCORTEX_API_KEY"
[{"body":"Cannot sign in","id":1,...}]
```

Without the key:

```console
$ curl -s -o /dev/null -w '%{http_code}\n' localhost:8000/tickets
401
```

## 3. Add logic that needs Python

Some things are not expressible as SQL. Add a handler to the end of `api.py`:

```python
@app.get("/tickets/{id}/summary", tool=True, scopes=["read"])
def summarize(id: int, style: str = "short") -> dict:
    """Summarise a ticket for a human or an agent."""
    if style not in ("short", "long"):
        raise HTTPError(422, "style must be 'short' or 'long'")
    return {"id": id, "style": style, "summary": f"Ticket #{id}"}
```

Then stop `webcortex dev` (Ctrl-C) and start it again. `dev` reads `api.py`
once, at boot, and does not watch it for changes, so restart it after each of
the steps below as well.

Two things happened without extra work:

1. **Parameters are bound by name and converted to their annotations.**
   `/tickets/1/summary` gives `id` the `int` 1, and `style` defaults to
   `"short"`.
2. **That signature became the tool schema.** No separate schema to maintain.

A conversion is not a validation. A value that cannot be converted reaches the
handler **unchanged**: `/tickets/abc/summary` calls `summarize` with
`id == "abc"`, a string. That is why the handler checks `style` itself. Check
whatever you rely on, and never put a parameter into SQL, a shell command or a
file path on the strength of its annotation.

```console
$ curl "localhost:8000/tickets/1/summary?style=long" -H "x-api-key: $WEBCORTEX_API_KEY"
{"id": 1, "style": "long", "summary": "Ticket #1"}
```

Bad input is a clean 422, not a 500:

```console
$ curl -s -o /dev/null -w '%{http_code}\n' "localhost:8000/tickets/1/summary?style=sideways" \
    -H "x-api-key: $WEBCORTEX_API_KEY"
422
```

Leave the key off and the same request is a 401: authentication is checked
before your handler runs.

## 4. Look at what an agent sees

The MCP server is already running. It needs admin scope, since it exposes every
tool in the app:

```console
$ curl -s -X POST localhost:8000/_webcortex/mcp \
    -H "x-api-key: $WEBCORTEX_API_KEY" -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | python -m json.tool
```

The response lists every tool the key may call: the five resource tools, the
one you just wrote, and the `webcortex_*` diagnostics tools that an admin key
also sees. This excerpt keeps only the new one:

```json
{
  "id": 1,
  "jsonrpc": "2.0",
  "result": {
    "tools": [
      {
        "annotations": {
          "actuator": false,
          "destructiveHint": false,
          "idempotentHint": true,
          "openWorldHint": false,
          "readOnlyHint": true,
          "requiresApproval": false
        },
        "description": "Summarise a ticket for a human or an agent.",
        "inputSchema": {
          "additionalProperties": false,
          "properties": {
            "id": {"type": "integer"},
            "style": {"default": "short", "type": "string"}
          },
          "required": ["id"],
          "type": "object"
        },
        "name": "get_tickets_by_id_summary"
      }
    ]
  }
}
```

That schema came from the Python signature. The docstring became the
description. Nothing was written twice.

Point any MCP client at `http://127.0.0.1:8000/_webcortex/mcp`.

## 5. Add a gated, destructive tool

Some operations should never happen unattended:

```python
@app.post("/tickets/purge", tool=True, scopes=["admin"], approval="required")
def purge(confirm: bool = False) -> dict:
    """Delete every closed ticket. Requires human approval."""
    if not confirm:
        raise HTTPError(422, "purge requires confirm=true")
    return {"purged": True}
```

`approval="required"` means an agent asking for this **does not get it** — the
run suspends and records an approval request, and the tool runs only if a human
approves it. It also cannot be called directly over MCP (`WC309`), because
honouring the gate only inside the agent loop would leave an obvious way around
it.

The gate is about models, not people. A caller holding the `admin` scope can
still `POST /tickets/purge` over plain HTTP, and it runs at once like any other
route: `scopes` decide who may call it at all, the gate decides that a model
may not call it unattended.

## 6. Add the agent

```python
app.agent(
    "assistant",
    model="claude-opus-5",
    description="Answers questions about tickets.",
    system=(
        "You help support staff triage tickets. Prefer read-only tools. "
        "Never purge anything unless explicitly asked."
    ),
    tools=[
        "list_tickets", "get_tickets",
        "get_tickets_by_id_summary", "create_tickets_purge",
    ],
    scopes=["read", "admin"],  # the most a run MAY DO
    expose_scopes=["read"],    # who may START a run
    max_steps=8,
    token_budget=50_000,
    expose_at="/ask",
)
```

Read those two scope lines carefully — they are the heart of the safety model:

- `scopes=["read", "admin"]` — the most authority a run may exercise,
  **intersected with the caller's**. A caller holding only `read` gets an agent
  that can read and nothing more; a caller holding nothing gets an agent holding
  nothing.
- `expose_scopes=["read"]` — who may start a run at all. It defaults to
  `scopes`; it is set here so that staff holding only `read` can ask questions
  too. `expose_scopes=[]` would make `/ask` an unauthenticated endpoint that
  spends your tokens.

The agent lists `create_tickets_purge`, and `admin` is among its scopes, so when
someone holding `admin` asks for a purge the agent can *propose* one: the run
suspends until a human approves it at `/_webcortex/approvals`. When a caller
holding only `read` asks, the run holds no `admin`, so the call is refused at
once (`WC602`) and no approval is created. Either way the agent never performs a
purge on its own.

Set `ANTHROPIC_API_KEY` and it runs. Without it the app still boots, and `/ask`
answers **502** with code `WC301` and a hint naming the missing variable. (For
tests, `WEBCORTEX_FAKE_PROVIDER=1` swaps in a deterministic stand-in model that
needs no key.)

```console
$ curl -X POST localhost:8000/ask -H "x-api-key: $WEBCORTEX_API_KEY" \
    -H 'content-type: application/json' -d '{"input":"What tickets are open?"}'
```

## 7. Add a dashboard

Pages are rendered **in Rust**. Add `templates="templates"` to the constructor,
then:

```python
app.page(
    "/",
    "index.html",
    sql="SELECT * FROM tickets ORDER BY priority DESC LIMIT 20",
    bind="tickets",
    scopes=["read"],
)
```

```html title="templates/index.html"
<!doctype html>
<html><body>
  <h1>Open tickets</h1>
  <ul>
    {% for t in tickets %}
      <li>#{{ t.id }} — {{ t.subject }} (priority {{ t.priority }})</li>
    {% endfor %}
  </ul>
</body></html>
```

Zero Python in the request path, and `{{ t.subject }}` is HTML-escaped
automatically. The page now owns `GET /`, so the discovery route from step 2 is
gone, and with it the app's only public route.

!!! info "The rule that keeps templates clean"
    A template receives a data object and nothing else. It has no database
    handle and cannot call Python. Django's template layer grew unmaintainable
    because template tags could reach back into application code; here that is
    structurally impossible.

    When a page needs real logic, use `@app.page_handler` — the logic lives in
    Python and the template still only renders.

## 8. Generate a typed frontend client

```console
$ webcortex typegen
Wrote client/api.ts (10 typed methods, … lines)
```

```typescript
import { createClient } from "./client/api";

const api = createClient({ baseUrl: "http://localhost:8000", apiKey: KEY });

const tickets = await api.listTickets({ limit: 20 });  // fully typed
const summary = await api.getTicketsByIdSummary({ id: 1, style: "long" });
```

Same route table, third consumer. Regenerate after changing routes and the
compiler tells you what broke.

## 9. Audit before shipping

```console
$ webcortex security
{
  "auth_configured": true,
  "anonymous_scopes": [],
  "rate_limited": true,
  "security_headers": true,
  "public_routes": [],
  "gated_tools": ["create_tickets_purge"],
  "agents": [{"name": "assistant", "tools": [...], "scopes": ["read", "admin"], ...}],
  ...
}
```

`public_routes` is the list worth staring at: everything reachable **with no
credential at all**, including anything `app.anonymous_scopes(...)` opens up.
It is empty here because the dashboard replaced the discovery route. `webcortex
dev` warns about it on every boot, because an unintentionally public route is
the failure that actually happens.

## What you built

- A REST API where 7 of 9 routes never touch Python
- An OpenAPI 3.1 document
- A live MCP server, scope-filtered per caller
- A server-rendered dashboard
- A typed TypeScript client
- An agent that cannot exceed its caller's authority and can only propose a
  purge, which a human then approves or refuses

<div class="grid cards" markdown>

- :material-router: **[Routing](routing.md)** — every route kind in depth
- :material-robot: **[Behaviours](behaviours.md)** — when a prompt is not enough
- :material-shield-lock: **[Security](security.md)** — the full model
- :material-rocket-launch: **[Deployment](deployment.md)** — going to production

</div>
