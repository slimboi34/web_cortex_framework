# Frontend

Two paths, sharing one data layer, with no overlap between them.

| You want | Use | Runs in |
|---|---|---|
| Server-rendered HTML | `app.page(...)` | Rust |
| A SPA / React app | `webcortex typegen` | your bundler |

Pick per route. A dashboard can be server-rendered while a customer-facing app
consumes the typed client, from the same declarations.

---

## Server-rendered pages

Templates are rendered **in Rust** by a Jinja2-compatible engine. No Python in
the request path.

```python
app = WebCortex("myapp", database="sqlite://./app.db", templates="templates")

app.page("/", "index.html",
         sql="SELECT * FROM tickets ORDER BY id DESC LIMIT 20",
         bind="tickets", scopes=["read"])
```

```html title="templates/index.html"
{% extends "base.html" %}
{% block content %}
  <h1>Tickets</h1>
  <ul>
    {% for t in tickets %}
      <li>#{{ t.id }} — {{ t.subject }}</li>
    {% endfor %}
  </ul>
{% endblock %}
```

### The rule that keeps this clean

!!! quote "A template receives a data object and nothing else."
    It has no database handle, no way to call Python, no ability to issue a
    query. Django's template layer grew unmaintainable precisely because
    template tags could reach back into application code and trigger work. Here
    that is structurally impossible — so a template can only ever be
    presentation.

Data is resolved **before** rendering begins, from exactly one declared source.

### Three data sources

=== "SQL"

    ```python
    app.page("/", "index.html",
             sql="SELECT * FROM tickets LIMIT 20", bind="tickets")
    ```

    Fully dynamic, zero Python.

=== "Constant"

    ```python
    app.page("/about", "about.html", data={"title": "About us"})
    ```

    Available as `{{ data.title }}`.

=== "Python"

    ```python
    @app.page_handler("/stats", "stats.html", scopes=["admin"])
    def stats() -> dict:
        return {"active": 128, "revenue": compute_revenue()}
    ```

    For context that needs real computation. Available as `{{ data.active }}`.

### Always available

```jinja
{{ request.path }}   {{ request.params }}   {{ request.query }}
{{ user.id }}        {{ user.authenticated }}   {{ user.scopes }}
```

```jinja
{% if user.authenticated %}
  Signed in as {{ user.id }}
{% else %}
  <a href="/login">Sign in</a>
{% endif %}
```

### Escaping

`.html`, `.htm`, and `.xml` autoescape. `.txt` and `.json` do not.

```jinja
{{ user_input }}          {# <script> becomes &lt;script&gt; #}
{{ trusted | safe }}      {# opt out, deliberately #}
```

Stored template syntax is **never evaluated** — a ticket titled `{{ 7*7 }}`
renders as that literal text, not `49`. There is no SSTI path from stored data.

### Filters

Beyond the Jinja2 built-ins:

| Filter | Example |
|---|---|
| `json` | `<script>const d = {{ data | json }}</script>` |
| `currency` | `${{ price | currency }}` → `12.50` |
| `truncate_words` | `{{ body | truncate_words(20) }}` |

The set is deliberately small. Anything more expressive belongs in the data
source, not the template.

### Boot-time verification

Every declared template is parsed at startup. A syntax error or missing file is
a **boot failure**, not a 500 for whoever visits that page first.

Render errors are logged with full detail and return a bare 500 — template
errors carry source snippets, which do not belong in a response body.

### Static assets

```python
app.static_files("/assets", "static", index="index.html", cache_secs=3600)
```

ETags and `304` responses come free. Traversal in any encoding, symlinks
pointing outside the root, and dotfiles are all refused.

---

## The typed TypeScript client

```console
$ webcortex typegen
Wrote client/api.ts (12 typed methods, 284 lines)
```

Zero dependencies, plain `fetch`, no runtime package to version-skew against the
server that generated it.

```typescript
import { createClient, WebCortexError } from "./client/api";

const api = createClient({
  baseUrl: "https://api.example.com",
  apiKey: process.env.API_KEY,
  // or: token: async () => await refreshAccessToken(),
});

const tickets = await api.listTickets({ limit: 20 });
const summary = await api.getTicketsByIdSummary({ id: 1, style: "long" });
```

### What is generated

- One method per API route, camelCased from the tool name
- A `XxxParams` interface per route with parameters
- A `XxxResult` interface per route with a described response
- Docstrings become JSDoc comments
- Path, query, and body parameters routed automatically

```typescript
export interface GetTicketsByIdSummaryParams {
  id: number;
  style?: string;
}
```

Pages and static mounts are excluded — they are not part of the JSON API.

### Error handling

```typescript
try {
  await api.createTickets({ subject: "Broken", priority: 3 });
} catch (e) {
  if (e instanceof WebCortexError) {
    console.error(e.status, e.message);   // 403, "missing required scope(s): write"
  }
}
```

### Keeping it in sync

```json title="package.json"
{
  "scripts": {
    "gen": "webcortex typegen --out src/api.ts",
    "prebuild": "npm run gen"
  }
}
```

Regenerate on every build and the TypeScript compiler tells you what a route
change broke — at build time, not in production.

!!! tip "Commit it or generate it, not both"
    Either commit `client/api.ts` and regenerate deliberately, or gitignore it
    and generate in CI. A half-committed generated file drifts.

---

## Using both

Nothing stops you:

```python
app.page("/admin", "admin.html", sql="...", bind="rows", scopes=["admin"])  # server-rendered
app.resource("tickets", fields={...}, tools=True)                           # JSON for the SPA
app.static_files("/app", "frontend/dist")                                   # built SPA
```

Server-render what benefits from it, and serve JSON to the parts that need
interactivity.

[Security :material-arrow-right:](security.md){ .md-button .md-button--primary }
