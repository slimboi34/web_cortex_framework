# Use cases

Five worked examples, each showing a different shape of application. All are
complete and runnable.

---

## 1. Internal tool your team and an agent both use

**The situation.** An ops team manages deploy approvals through a small web
tool. You want an assistant that can answer questions about pending deploys and
prepare approvals — but never actually approve one.

```python title="api.py"
from webcortex import HTTPError, WebCortex

app = WebCortex("deployhub", database="sqlite://./deployhub.db",
                templates="templates")

app.api_key("OPS_KEY",   id="ops",   scopes=["read", "write", "webcortex:admin"])
app.api_key("VIEWER_KEY", id="viewer", scopes=["read"])
app.rate_limit(per_second=20, burst=40)

app.resource(
    "deploys",
    fields={"id": int, "service": str, "version": str,
            "status": str, "requested_by": str},
    tools=True,
    read_scopes=["read"],
    write_scopes=["write"],
)

# Approving a deploy is destructive. An agent may propose; only a human commits.
@app.post("/deploys/{id}/approve", tool=True,
          scopes=["write"], approval="required")
def approve(id: int, approver: str) -> dict:
    """Approve a pending deploy. Gated behind human approval."""
    return {"id": id, "status": "approved", "approver": approver}

@app.get("/deploys/pending", tool=True, scopes=["read"])
def pending() -> list:
    """Deploys waiting on a decision."""
    return []

app.agent(
    "deploybot",
    model="claude-opus-5",
    system="You help ops staff understand pending deploys. Never approve anything.",
    tools=["list_deploys", "get_deploys", "get_deploys_pending",
           "create_deploys_by_id_approve"],
    scopes=["read"],
    expose_scopes=["read"],
    max_steps=10,
    token_budget=40_000,
    expose_at="/ask",
)

# A dashboard for the humans, rendered in Rust.
app.page("/", "index.html",
         sql="SELECT * FROM deploys WHERE status='pending' ORDER BY id DESC",
         bind="deploys", scopes=["read"])
```

**Why it is safe.** The agent lists the approve tool, so it can *reason* about
approvals and propose one — but `approval="required"` means the run suspends
instead of executing, and `scopes=["read"]` means even without the gate it could
not reach a write route. Two independent controls.

---

## 2. Public API that is also an MCP server

**The situation.** You publish a data API. Customers integrate over REST; their
AI tooling integrates over MCP. You maintain one thing.

```python title="api.py"
from webcortex import WebCortex

app = WebCortex("marketdata", description="Market data API.",
                database="sqlite://./market.db")

app.api_key("CUSTOMER_KEY", id="customer", scopes=["read"])
app.api_key("ADMIN_KEY", id="admin", scopes=["read", "write", "webcortex:admin"])
app.rate_limit(per_second=10, burst=20)
app.cors("https://app.customer.com", credentials=False)

app.resource("instruments",
             fields={"id": int, "symbol": str, "name": str, "sector": str},
             tools=True, read_scopes=["read"], write_scopes=["write"])

app.query(
    "GET", "/prices/{symbol}",
    "SELECT * FROM prices WHERE symbol = ? ORDER BY ts DESC LIMIT COALESCE(?, 100)",
    params=["symbol", "limit"],
    returns="many",
    summary="Recent prices for a symbol",
    description="Most recent price ticks for a symbol, newest first.",
    input_schema={"type": "object",
                  "properties": {"symbol": {"type": "string"},
                                 "limit": {"type": "integer", "default": 100}},
                  "required": ["symbol"]},
    tool=True, tool_name="get_prices", scopes=["read"],
)
```

Customers get REST, `/_webcortex/openapi.json`, a typed TS client via
`webcortex typegen`, and an MCP endpoint — from one declaration.

Every one of these routes is served **entirely in Rust**.

!!! tip "Write descriptions for the model, not just the docs"
    `description` becomes the MCP tool description. "Most recent price ticks for
    a symbol, newest first" tells a model when to reach for it; "get prices"
    does not.

---

## 3. Agentic workflow with a hard policy boundary

**The situation.** Expense reports need reviewing. Judgment is genuinely
model-shaped; the approval limit absolutely is not.

```python
@app.behaviour(
    "review_expenses",
    tools=["list_expenses", "approve_expense", "flag_expense"],
    scopes=["expenses:read", "expenses:write"],
    max_steps=100,
    token_budget=300_000,
    expose_at="/behaviours/review",
    expose_scopes=["finance"],
)
def review_expenses(ctx, input):
    expenses = ctx.call("list_expenses", status="pending")
    if not expenses:
        ctx.halt("nothing pending")

    approved = flagged = 0
    for expense in expenses:
        # Hard policy. Code, not a prompt.
        if expense["amount"] > 1_000_00:
            ctx.call("flag_expense", id=expense["id"], reason="over limit")
            flagged += 1
            continue

        verdict = ctx.ask(
            f"Is this expense compliant?\n\n{expense['description']}\n"
            f"Amount: ${expense['amount']/100:.2f}\nCategory: {expense['category']}",
            schema={"type": "object",
                    "properties": {"compliant": {"type": "boolean"},
                                   "confidence": {"type": "number"},
                                   "reason": {"type": "string"}},
                    "required": ["compliant", "confidence", "reason"]},
        )

        if verdict["compliant"] and verdict["confidence"] >= 0.85:
            ctx.call("approve_expense", id=expense["id"])
            approved += 1
        else:
            ctx.call("flag_expense", id=expense["id"], reason=verdict["reason"])
            flagged += 1

    ctx.log(f"approved {approved}, flagged {flagged}")
    return {"approved": approved, "flagged": flagged, "total": len(expenses)}
```

**Why a Behaviour rather than an agent.** The $1,000 limit and the 0.85
confidence floor are branches in Python. Prompt injection in a description
cannot move them, because the model never sees them as instructions — they are
evaluated after it returns. The loop provably visits every expense.

---

## 4. API gateway in front of internal services

**The situation.** Several internal services, one authenticated public edge,
with the internal credentials never leaving the gateway.

```python title="api.py"
from webcortex import WebCortex

app = WebCortex("gateway")

app.api_key("PARTNER_KEY", id="partner", scopes=["billing:read", "catalog:read"])
app.jwt(secret_env="JWT_SECRET", audience="gateway.example.com")
app.rate_limit(per_second=100, burst=200)
app.cors("https://partner.example.com")

app.upstream("billing", base_url="https://billing.internal",
             bearer_env="BILLING_TOKEN", timeout_ms=5000)
app.upstream("catalog", base_url="https://catalog.internal",
             bearer_env="CATALOG_TOKEN", timeout_ms=3000)

app.proxy("GET", "/billing/invoices/{id}",
          upstream="billing", rewrite="/v2/invoices/{id}",
          scopes=["billing:read"], tool=True, tool_name="get_invoice")

app.proxy("GET", "/catalog/products",
          upstream="catalog", rewrite="/products",
          scopes=["catalog:read"], tool=True, tool_name="list_products")
```

**What the gateway guarantees.**

- Internal tokens are named (`bearer_env`), resolved at boot, never in source
- The caller's own `Authorization` header is **never** forwarded — the upstream
  sees the gateway's credentials
- Path parameters containing traversal are rejected with 400 before any request
  is issued
- An undeclared upstream is a **boot error**

Every proxied route is also an MCP tool, so an agent can reach internal services
through exactly the same authorization checks a partner does.

---

## 5. Server-rendered admin panel

**The situation.** An internal admin panel. No SPA, no build step, no npm.

```python title="api.py"
from webcortex import WebCortex

app = WebCortex("admin", database="sqlite://./app.db", templates="templates")

app.api_key("ADMIN_KEY", id="admin", scopes=["admin", "webcortex:admin"])
app.security_headers(content_security_policy="default-src 'self'")

app.resource("users", fields={"id": int, "email": str, "role": str},
             scopes=["admin"])

app.page("/", "users.html",
         sql="SELECT * FROM users ORDER BY id DESC LIMIT 100",
         bind="users", scopes=["admin"])

app.page("/users/{id}", "user_detail.html",
         sql="SELECT * FROM users WHERE id = ?",
         params=["id"], returns="one", bind="user", scopes=["admin"])

@app.page_handler("/stats", "stats.html", scopes=["admin"])
def stats() -> dict:
    """A page whose context needs real computation."""
    return {"active": 128, "churn_rate": 0.03}

app.static_files("/assets", "static")
```

```html title="templates/users.html"
{% extends "base.html" %}
{% block content %}
  <h1>Users</h1>
  <table>
    {% for u in users %}
      <tr>
        <td><a href="/users/{{ u.id }}">{{ u.email }}</a></td>
        <td>{{ u.role }}</td>
      </tr>
    {% endfor %}
  </table>
{% endblock %}
```

Two of the three pages run **zero Python per request**: SQL to template, in
Rust, at ~22,000 requests/second. `{{ u.email }}` is HTML-escaped automatically.

---

## Choosing a shape

| If you need | Reach for |
|---|---|
| CRUD over a table | `app.resource(...)` |
| A specific query | `app.query(...)` |
| Genuine computation | `@app.get(...)` handler |
| HTML for humans | `app.page(...)` |
| To front an internal service | `app.upstream` + `app.proxy` |
| A known procedure with judgment in it | `@app.behaviour(...)` |
| Open-ended assistance | `app.agent(...)` |

[Deployment :material-arrow-right:](deployment.md){ .md-button .md-button--primary }
