"""Project starters for `webcortex new`.

Three shapes, because they are genuinely different applications rather than the
same one with features toggled:

* **api**       — a JSON API plus an MCP tool surface
* **fullstack** — the above, plus server-rendered pages and static assets
* **agent**     — the above, plus an agent with a gated tool and an approval flow
* **behaviour** — the above, plus Behaviours: procedures whose control flow is
  real Python and whose leaves are model and tool calls

Every starter boots with authentication, rate limiting, and security headers
already on. A starter that generates an insecure app teaches an insecure habit.
"""

from __future__ import annotations

BASE_GITIGNORE = """\
__pycache__/
*.db
.env
.venv/
client/
"""

ENV_EXAMPLE = """\
# Copy to .env and fill in. Never commit the real file.
WEBCORTEX_API_KEY=replace-me-run-webcortex-keygen
WEBCORTEX_JWT_SECRET=replace-me-at-least-32-bytes-long-abcdefgh
# Hosted models. Either is enough; a model name picks its provider by prefix.
# ANTHROPIC_API_KEY=sk-ant-...
# OPENAI_API_KEY=sk-...
# Local models need no key: ollama/<model> talks to OLLAMA_HOST.
# OLLAMA_HOST=http://127.0.0.1:11434
"""

README = """\
# {name}

Built with [WebCortex](https://github.com/slimboi34/web_cortex_framework).

## Run

```bash
export WEBCORTEX_API_KEY=$(webcortex keygen)
webcortex dev
```

- API: http://127.0.0.1:8000
- OpenAPI: http://127.0.0.1:8000/_webcortex/openapi.json
- MCP endpoint: http://127.0.0.1:8000/_webcortex/mcp

## Inspect

```bash
webcortex check      # routes, tools, and the public attack surface
webcortex security   # what is reachable without a credential
webcortex tools      # the agent tool manifest
webcortex typegen    # generate client/api.ts
```
"""

API = '''\
"""{name} — a JSON API that is also an MCP server."""

from webcortex import HTTPError, WebCortex

app = WebCortex(
    "{name}",
    description="{description}",
    database="sqlite://./{name}.db",
)

# --- Security -------------------------------------------------------------
# Configured up front deliberately: everything below is deny-by-default, and
# opening a route up is an explicit act.

app.api_key("WEBCORTEX_API_KEY", id="service", scopes=["read", "write", "webcortex:admin"])
app.rate_limit(per_second=50, burst=100)

# Reads are public; writes require the "write" scope.
app.anonymous_scopes("read")


# --- Data -----------------------------------------------------------------
# Five CRUD routes and five agent tools, all executed in Rust.

app.resource(
    "items",
    fields={{"id": int, "name": str, "note": str}},
    tools=True,
    read_scopes=["read"],
    write_scopes=["write"],
)


# --- Custom logic ---------------------------------------------------------


@app.get("/items/{{id}}/summary", tool=True, scopes=["read"])
def summarize(id: int, style: str = "short") -> dict:
    """Summarise an item. Demonstrates a typed Python handler."""
    if style not in ("short", "long"):
        raise HTTPError(422, "style must be 'short' or 'long'")
    return {{"id": id, "style": style, "summary": f"Item {{id}}"}}
'''

FULLSTACK_API = '''\
"""{name} — API, server-rendered pages, and an MCP tool surface."""

from webcortex import HTTPError, WebCortex

app = WebCortex(
    "{name}",
    description="{description}",
    database="sqlite://./{name}.db",
    templates="templates",
)

app.api_key("WEBCORTEX_API_KEY", id="service", scopes=["read", "write", "webcortex:admin"])
app.rate_limit(per_second=50, burst=100)
app.anonymous_scopes("read")

app.resource(
    "items",
    fields={{"id": int, "name": str, "note": str}},
    tools=True,
    read_scopes=["read"],
    write_scopes=["write"],
)

# --- Pages ----------------------------------------------------------------
# Rendered in Rust. A template receives a data object and nothing else — it has
# no database handle and cannot call Python — which is what stops this layer
# from turning into a second view layer.

app.page(
    "/",
    "index.html",
    sql="SELECT * FROM items ORDER BY id DESC LIMIT 20",
    bind="items",
)

app.page("/about", "about.html", data={{"title": "About"}})


@app.page_handler("/dashboard", "dashboard.html")
def dashboard() -> dict:
    """A page whose context needs real logic."""
    return {{"stats": {{"greeting": "Hello", "healthy": True}}}}


app.static_files("/assets", "static")


@app.get("/items/{{id}}/summary", tool=True, scopes=["read"])
def summarize(id: int, style: str = "short") -> dict:
    """Summarise an item."""
    if style not in ("short", "long"):
        raise HTTPError(422, "style must be 'short' or 'long'")
    return {{"id": id, "style": style, "summary": f"Item {{id}}"}}
'''

AGENT_API = '''\
"""{name} — an agent-native application.

Shows the three things that make WebCortex agents safe to point at production:
delegated authority, runtime-enforced budgets, and human approval gates.
"""

from webcortex import HTTPError, WebCortex

app = WebCortex(
    "{name}",
    description="{description}",
    database="sqlite://./{name}.db",
)

app.api_key("WEBCORTEX_API_KEY", id="service", scopes=["read", "write", "webcortex:admin"])
app.rate_limit(per_second=50, burst=100)
app.anonymous_scopes("read")

app.resource(
    "items",
    fields={{"id": int, "name": str, "note": str}},
    tools=True,
    read_scopes=["read"],
    write_scopes=["write"],
)


@app.get("/items/{{id}}/summary", tool=True, scopes=["read"])
def summarize(id: int) -> dict:
    """Summarise an item."""
    return {{"id": id, "summary": f"Item {{id}}"}}


# A destructive tool. `approval="required"` means an agent asking for this does
# not get it: the run suspends and records an approval request for a human.
@app.post("/items/purge", tool=True, scopes=["write"], approval="required")
def purge(confirm: bool = False) -> dict:
    """Delete every item. Gated behind human approval."""
    if not confirm:
        raise HTTPError(422, "purge requires confirm=true")
    return {{"purged": True}}


# The agent. Its `scopes` are intersected with whoever starts the run, so it can
# never hold more authority than its caller. `max_steps` and `token_budget` are
# enforced by the runtime rather than trusted to the model.
app.agent(
    "assistant",
    model="claude-opus-5",
    description="Answers questions about the catalogue.",
    system=(
        "You help users manage their items. Prefer read-only tools. "
        "Never purge without being explicitly asked."
    ),
    tools=["list_items", "get_items", "get_items_by_id_summary", "create_items_purge"],
    scopes=["read"],
    # Who may start a run. Separate from `scopes`, which is what the run may do.
    expose_scopes=["read"],
    max_steps=8,
    token_budget=50_000,
    expose_at="/ask",
)
'''

INDEX_HTML = """\
{% extends "base.html" %}
{% block content %}
  <h1>Items</h1>
  {% if items %}
    <ul class="items">
      {% for item in items %}
        <li><strong>{{ item.name }}</strong> — {{ item.note }}</li>
      {% endfor %}
    </ul>
  {% else %}
    <p class="empty">No items yet. Create one:</p>
    <pre>curl -X POST localhost:8000/items \\
  -H 'x-api-key: $WEBCORTEX_API_KEY' \\
  -H 'content-type: application/json' \\
  -d '{"name":"First","note":"hello"}'</pre>
  {% endif %}
{% endblock %}
"""

BASE_HTML = """\
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>{% block title %}WebCortex{% endblock %}</title>
    <link rel="stylesheet" href="/assets/style.css" />
  </head>
  <body>
    <nav>
      <a href="/">Home</a>
      <a href="/dashboard">Dashboard</a>
      <a href="/about">About</a>
      {% if user.authenticated %}<span class="user">{{ user.id }}</span>{% endif %}
    </nav>
    <main>{% block content %}{% endblock %}</main>
  </body>
</html>
"""

ABOUT_HTML = """\
{% extends "base.html" %}
{% block title %}{{ data.title }}{% endblock %}
{% block content %}
  <h1>{{ data.title }}</h1>
  <p>Rendered in Rust, from a constant declared in <code>api.py</code>.</p>
{% endblock %}
"""

DASHBOARD_HTML = """\
{% extends "base.html" %}
{% block content %}
  <h1>{{ data.stats.greeting }}</h1>
  <p>Status: {% if data.stats.healthy %}healthy{% else %}degraded{% endif %}</p>
{% endblock %}
"""

STYLE_CSS = """\
:root { color-scheme: light dark; --fg: #111; --bg: #fff; --muted: #666; }
@media (prefers-color-scheme: dark) {
  :root { --fg: #eee; --bg: #111; --muted: #999; }
}
* { box-sizing: border-box; }
body {
  margin: 0; padding: 0; background: var(--bg); color: var(--fg);
  font: 16px/1.6 ui-sans-serif, system-ui, -apple-system, sans-serif;
}
nav {
  display: flex; gap: 1rem; align-items: center;
  padding: 1rem 2rem; border-bottom: 1px solid color-mix(in srgb, var(--fg) 15%, transparent);
}
nav a { color: inherit; text-decoration: none; font-weight: 500; }
nav a:hover { text-decoration: underline; }
nav .user { margin-left: auto; color: var(--muted); font-size: 0.875rem; }
main { max-width: 52rem; margin: 0 auto; padding: 2rem; }
h1 { font-size: 1.75rem; margin-top: 0; }
.items { list-style: none; padding: 0; }
.items li { padding: 0.75rem 0; border-bottom: 1px solid color-mix(in srgb, var(--fg) 10%, transparent); }
.empty { color: var(--muted); }
pre {
  background: color-mix(in srgb, var(--fg) 6%, transparent);
  padding: 1rem; border-radius: 8px; overflow-x: auto; font-size: 0.875rem;
}
"""


def files_for(template: str, name: str, description: str) -> dict[str, str]:
    """Return `{relative_path: contents}` for a starter."""
    if template not in TEMPLATES:
        raise ValueError(
            f"unknown starter {template!r}; choose one of: {', '.join(TEMPLATES)}"
        )

    common = {
        ".gitignore": BASE_GITIGNORE,
        ".env.example": ENV_EXAMPLE,
        "README.md": README.format(name=name),
    }

    if template == "api":
        return {**common, "api.py": API.format(name=name, description=description)}

    if template == "agent":
        return {**common, "api.py": AGENT_API.format(name=name, description=description)}

    if template == "behaviour":
        return {**common, "api.py": BEHAVIOUR_API.format(name=name, description=description)}

    if template == "orchestration":
        return {**common, "api.py": ORCHESTRATION_API.format(name=name, description=description)}

    return {
        **common,
        "api.py": FULLSTACK_API.format(name=name, description=description),
        "templates/base.html": BASE_HTML,
        "templates/index.html": INDEX_HTML,
        "templates/about.html": ABOUT_HTML,
        "templates/dashboard.html": DASHBOARD_HTML,
        "static/style.css": STYLE_CSS,
    }


BEHAVIOUR_API = '''\
"""{name} — an application built around Behaviours.

A "skill" written as a prompt is a suggestion: the model reads it and may
ignore it, and "if X then Y" fails silently when it does.

A Behaviour inverts that. The loops and branches below are real Python that
always runs; only the leaves — `ctx.ask(...)` — are probabilistic. You get a
procedure with deterministic structure and probabilistic steps, rather than a
probabilistic procedure.
"""

from webcortex import WebCortex

app = WebCortex(
    "{name}",
    description="{description}",
    database="sqlite://./{name}.db",
)

app.api_key("WEBCORTEX_API_KEY", id="service", scopes=["read", "write", "webcortex:admin"])
app.rate_limit(per_second=50, burst=100)
app.anonymous_scopes("read")

app.resource(
    "tickets",
    fields={{"id": int, "body": str, "urgency": int, "state": str}},
    tools=True,
    read_scopes=["read"],
    write_scopes=["write"],
)


@app.behaviour(
    "triage",
    description="Classify every open ticket and escalate the urgent ones.",
    tools=["list_tickets", "update_tickets"],
    scopes=["read", "write"],
    max_steps=100,
    token_budget=100_000,
)
def triage(ctx, input):
    """Walk the open tickets, ask the model to grade each, act on the grade."""
    threshold = input.get("threshold", 7)
    tickets = ctx.call("list_tickets", limit=50)

    escalated, routine = [], []

    for ticket in tickets:                          # a real loop
        if ticket["state"] != "open":               # a real branch
            continue

        # A leaf. `schema` forces the shape, so the branch below switches on a
        # real value rather than on parsed prose.
        verdict = ctx.ask(
            f"Grade this support ticket.\\n\\n{{ticket['body']}}",
            schema={{
                "type": "object",
                "properties": {{
                    "urgency": {{"type": "integer", "minimum": 1, "maximum": 10}},
                    "category": {{"enum": ["bug", "billing", "question", "other"]}},
                    "reason": {{"type": "string"}},
                }},
                "required": ["urgency", "category", "reason"],
            }},
        )

        if verdict["urgency"] >= threshold:
            escalated.append(ticket["id"])
            state = "escalated"
        else:
            routine.append(ticket["id"])
            state = "triaged"

        ctx.call(
            "update_tickets",
            id=ticket["id"],
            body=ticket["body"],
            urgency=verdict["urgency"],
            state=state,
        )

    ctx.log(f"escalated {{len(escalated)}}, routed {{len(routine)}}")
    return {{
        "escalated": escalated,
        "routine": routine,
        "usage": ctx.usage,
    }}


@app.behaviour(
    "daily_report",
    description="Summarise the queue. Composes with triage.",
    tools=["triage", "list_tickets"],
    scopes=["read", "write"],
)
def daily_report(ctx, input):
    """Behaviours compose: one can call another, budgets and scopes intact."""
    result = ctx.call("triage", threshold=input.get("threshold", 7))
    remaining = ctx.call("list_tickets", limit=50)

    summary = ctx.ask(
        "Write two sentences summarising this queue for a standup: "
        f"{{len(remaining)}} tickets, {{len(result['escalated'])}} escalated."
    )
    return {{"summary": summary, "triage": result}}


# An agent can invoke a Behaviour like any other tool, which is how you give a
# conversational agent a procedure it is not free to improvise around.
app.agent(
    "supervisor",
    model="claude-opus-5",
    description="Runs the queue.",
    system="You supervise a support queue. Use the triage behaviour rather than "
           "grading tickets yourself.",
    tools=["triage", "daily_report", "list_tickets"],
    scopes=["read", "write"],
    expose_scopes=["write"],
    max_steps=8,
    token_budget=100_000,
    expose_at="/ask",
)
'''


ORCHESTRATION_API = '''\
"""{name} — a multi-agent application.

Everything an orchestration needs, declared in one file and executed by the
runtime: a front desk that hands conversations to specialists, a memory each
caller owns, context the agents start with, a pipeline and a router built from
agents as steps, and a behaviour that fans work out concurrently. Budgets are
shared across the whole tree, so the bill is bounded by what you declare.
"""

from webcortex import WebCortex

app = WebCortex(
    "{name}",
    description="{description}",
    database="sqlite://./{name}.db",
)

app.api_key("WEBCORTEX_API_KEY", id="service", scopes=["read", "write", "webcortex:admin"])
app.rate_limit(per_second=50, burst=100)
app.anonymous_scopes("read")

# --- Models -----------------------------------------------------------------
# Name tiers once. Classification leaves use "fast"; judgement uses "default".
# Point "fast" at a local model to do the cheap work for free:
#     app.models(fast="ollama/qwen3.5:9b")

app.models(default="claude-opus-5", fast="claude-haiku-4-5-20251001")


# --- Data -------------------------------------------------------------------

app.resource(
    "tickets",
    fields={{"id": int, "customer": str, "body": str, "kind": str, "state": str}},
    tools=True,
    read_scopes=["read"],
    write_scopes=["write"],
)


# --- Context: what an agent knows at step one --------------------------------
# Resolved when a run starts, injected into the system prompt, bounded in size.

app.context(
    "policy",
    data={{"refund_window_days": 30, "auto_refund_limit_usd": 500, "escalate_to": "ops@example.com"}},
    description="Support policy the agents must follow.",
)
app.context(
    "open_queue",
    sql="SELECT id, customer, kind, state FROM tickets WHERE state = 'open' ORDER BY id DESC LIMIT 20",
    description="The newest open tickets.",
)


# --- Memory: a durable, per-caller scratchpad -------------------------------
# Four tools, executed in Rust, keyed by whoever is really asking.

app.memory("notes", read_scopes=["read"], write_scopes=["write"])


# --- Specialists ------------------------------------------------------------

app.agent(
    "billing",
    description="Handles invoices, refunds and payment questions.",
    system="You are the billing specialist. Apply the policy exactly.",
    tools=["list_tickets", "get_tickets", "update_tickets"],
    context=["policy"],
    scopes=["read", "write"],
    max_steps=10,
    token_budget=60_000,
)

app.agent(
    "technical",
    description="Handles bugs, outages and how-to questions.",
    system="You are the technical specialist. Be precise and cite ticket ids.",
    tools=["list_tickets", "get_tickets", "update_tickets"],
    context=["open_queue"],
    scopes=["read", "write"],
    max_steps=10,
    token_budget=60_000,
)


# --- The front desk: hands off, remembers, stays inside one budget -----------
# `handoffs` become transfer_to_* tools. The conversation, the budget and the
# caller's authority carry over — and authority can only shrink.

app.agent(
    "front_desk",
    description="First contact. Routes to a specialist or answers directly.",
    system=(
        "You are the front desk. Greet briefly, find out what the customer needs, "
        "and hand off to billing or technical when the request is clearly theirs."
    ),
    tools=["list_tickets", "create_tickets"],
    handoffs=["billing", "technical"],
    memory="notes",
    context=["policy"],
    scopes=["read", "write"],
    expose_scopes=["read"],
    max_steps=12,
    token_budget=120_000,     # caps the whole run, including any handoff
    context_window=60_000,    # older turns are summarised past this
    expose_at="/ask",
)


# --- Flows: orchestration as data -------------------------------------------

# A cheap model picks the specialist; no front desk in the loop.
app.flow(
    "desk",
    description="Route a message straight to the right specialist.",
    route={{"billing": "billing", "technical": "technical"}},
    default="front_desk",
    classify_with="fast",
    scopes=["read", "write"],
    token_budget=80_000,
)

# Two agents in sequence, sharing one budget: the writer gets the researcher's answer.
app.flow(
    "briefing",
    description="Research the queue, then write a two-paragraph summary.",
    pipeline=["technical", "billing"],
    scopes=["read", "write"],
    token_budget=100_000,
)


# --- A behaviour that fans out ---------------------------------------------
# The loop and the branch are Python; only the leaves are probabilistic, and
# the leaves run concurrently.

@app.behaviour(
    "triage",
    description="Classify every open ticket at once and mark the urgent ones.",
    tools=["list_tickets", "update_tickets"],
    context=["policy"],
    scopes=["read", "write"],
    max_steps=200,
    token_budget=150_000,
    model="fast",
)
def triage(ctx, input):
    tickets = [t for t in ctx.call("list_tickets", limit=50) if t["state"] == "open"]
    if not tickets:
        ctx.halt("no open tickets")

    policy = ctx.context("policy")
    verdicts = ctx.ask_many(                     # fifty prompts, one wait
        [f"Policy: {{policy}}\\n\\nClassify this ticket:\\n{{t['body']}}" for t in tickets],
        schema={{
            "type": "object",
            "properties": {{
                "kind": {{"enum": ["billing", "technical", "other"]}},
                "urgent": {{"type": "boolean"}},
            }},
            "required": ["kind", "urgent"],
        }},
    )

    updates = [
        ("update_tickets", {{
            "id": t["id"], "customer": t["customer"], "body": t["body"],
            "kind": v["kind"], "state": "urgent" if v["urgent"] else "triaged",
        }})
        for t, v in zip(tickets, verdicts)
    ]
    ctx.gather(*updates)                         # fifty writes, one wait
    return {{
        "triaged": len(tickets),
        "urgent": sum(1 for v in verdicts if v["urgent"]),
        "usage": ctx.usage,
    }}
'''


TEMPLATES = ("api", "fullstack", "agent", "behaviour", "orchestration")

