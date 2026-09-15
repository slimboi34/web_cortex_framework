# Tutorial

We will build a **support-desk API** that a human dashboard and an AI agent both
use — with the agent held to strictly less authority than the humans.

By the end you will have a REST API, an OpenAPI document, a live MCP server, a
server-rendered dashboard, a typed TypeScript client, and an agent that must ask
permission before doing anything destructive.

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
  webcortex 0.1.0  ·  supportdesk
  python 3.14.7 (free-threaded)
  7 routes, 7 served without touching Python
  5 agent tools: list_tickets, get_tickets, create_tickets, update_tickets, delete_tickets
  security: auth, rate-limited, headers
```

**7 of 7 routes served without touching Python.** Nothing you wrote runs per
request yet — it is all declaration.

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

Some things are not expressible as SQL. Add a handler:

```python
@app.get("/tickets/{id}/summary", tool=True, scopes=["read"])
def summarize(id: int, style: str = "short") -> dict:
    """Summarise a ticket for a human or an agent."""
    if style not in ("short", "long"):
        raise HTTPError(422, "style must be 'short' or 'long'")
    return {"id": id, "style": style, "summary": f"Ticket #{id}"}
```

Two things happened without extra work:

1. **Parameters are bound by name and coerced from the signature.** `id: int`
   arrives as an `int`; `style` defaults to `"short"`.
2. **That signature became the tool schema.** No separate schema to maintain.

```console
$ curl "localhost:8000/tickets/1/summary?style=long" -H "x-api-key: $WEBCORTEX_API_KEY"
{"id": 1, "style": "long", "summary": "Ticket #1"}
```

Bad input is a clean 422, not a 500:

```console
$ curl -s -o /dev/null -w '%{http_code}\n' "localhost:8000/tickets/1/summary?style=sideways"
422
```

## 4. Look at what an agent sees

The MCP server is already running. It needs admin scope, since it exposes every
tool in the app:

```console
$ curl -s -X POST localhost:8000/_webcortex/mcp \
    -H "x-api-key: $WEBCORTEX_API_KEY" -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | python -m json.tool
```

```json
{
  "result": {
    "tools": [
      {
        "name": "get_tickets_by_id_summary",
        "description": "Summarise a ticket for a human or an agent.",
        "inputSchema": {
          "type": "object",
          "properties": {
            "id": {"type": "integer"},
            "style": {"type": "string", "default": "short"}
          },
          "required": ["id"],
          "additionalProperties": false
        },
        "annotations": {"readOnlyHint": true, "requiresApproval": false}
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
run suspends and records an approval request. It also cannot be called directly
over MCP, because honouring the gate only inside the agent loop would leave an
obvious way around it.

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
    scopes=["read"],          # what a run MAY DO
    expose_scopes=["read"],   # who may START a run
    max_steps=8,
    token_budget=50_000,
    expose_at="/ask",
)
```

Read those two scope lines carefully — they are the heart of the safety model:

- `scopes=["read"]` — the agent may only exercise read authority, **intersected
  with the caller's**. A caller holding nothing gets an agent holding nothing.
- `expose_scopes=["read"]` — who may start a run at all. Without this, `/ask`
  would be an unauthenticated endpoint that spends your tokens.

The agent lists `create_tickets_purge` as a tool but is gated behind approval, so
it can *propose* a purge and never perform one.

Set `ANTHROPIC_API_KEY` and it runs; without it, `/ask` returns a clear 503
rather than failing at boot.

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
automatically.

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
Wrote client/api.ts (7 typed methods, 210 lines)
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
  "rate_limited": true,
  "security_headers": true,
  "public_routes": ["GET /"],
  "gated_tools": ["create_tickets_purge"],
  "agents": [{"name": "assistant", "tools": [...], "scopes": ["read"]}]
}
```

`public_routes` is the list worth staring at: everything reachable **with no
credential at all**. `webcortex dev` warns about it on every boot, because an
unintentionally public route is the failure that actually happens.

## What you built

- A REST API where 8 of 10 routes never touch Python
- An OpenAPI 3.1 document
- A live MCP server, scope-filtered per caller
- A server-rendered dashboard
- A typed TypeScript client
- An agent that cannot exceed its caller's authority and must ask before
  destroying anything

<div class="grid cards" markdown>

- :material-router: **[Routing](routing.md)** — every route kind in depth
- :material-robot: **[Behaviours](behaviours.md)** — when a prompt is not enough
- :material-shield-lock: **[Security](security.md)** — the full model
- :material-rocket-launch: **[Deployment](deployment.md)** — going to production

</div>
