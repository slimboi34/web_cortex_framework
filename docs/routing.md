# Routing

Nine kinds of route. Seven never enter the Python interpreter at request time;
Python handlers and behaviours do. Flows are covered in
[Orchestration](orchestration.md) and behaviours in [Behaviours](behaviours.md);
this page covers the rest.

Choosing between them is the main design decision in a WebCortex app, so this
page covers each one and when to reach for it.

## Static

A constant response, serialised once at boot.

```python
app.static("GET", "/health/ready", {"ready": True})
app.static("GET", "/version", {"version": "1.2.3"}, status=200)
```

Use for: readiness probes, feature flags, service discovery, anything that does
not change between deploys.

## Query

Binds a route directly to SQL. The fastest kind of route with real data.

```python
app.query(
    "GET", "/tickets/by-status/{status}",
    "SELECT * FROM tickets WHERE status = ? ORDER BY priority DESC",
    params=["status"],
    returns="many",
    tool=True,
    tool_name="tickets_by_status",
    scopes=["read"],
)
```

`params` names the values bound to each `?`, **in order**. Each is resolved
from the path, then the query string, then the JSON body — so a caller (or a
model) can supply them wherever is natural.

`returns` controls the shape:

| Value | Response |
|---|---|
| `"many"` | JSON array of row objects |
| `"one"` | Single object, or **404** when nothing matches |
| `"affected"` | `{"affected": n, "last_insert_id": m}` |

!!! danger "Parameters are data, never syntax"
    Values are always bound, never interpolated. `?limit=1 OR 1=1` reaches
    SQLite as the *string* `"1 OR 1=1"` bound to a LIMIT — a type error, which
    returns **400** and is the proof that parameterisation held.

    The SQL text itself must come from your source, never from a request.

## Resource

Generates a full CRUD surface. The Django-admin-scale shortcut, except the
endpoints are executed by Rust rather than an ORM.

```python
app.resource(
    "tickets",
    fields={"id": int, "subject": str, "body": str, "priority": int},
    tools=True,
    read_scopes=["read"],
    write_scopes=["write"],
)
```

Five routes, five tools, zero interpreter involvement:

| Method | Path | Tool name | Guarded by |
|---|---|---|---|
| GET | `/tickets` | `list_tickets` | `read_scopes` |
| GET | `/tickets/{id}` | `get_tickets` | `read_scopes` |
| POST | `/tickets` | `create_tickets` | `write_scopes` |
| PUT | `/tickets/{id}` | `update_tickets` | `write_scopes` |
| DELETE | `/tickets/{id}` | `delete_tickets` | `write_scopes` |

### Options

| Argument | Default | Notes |
|---|---|---|
| `table` | `name` | Underlying table, if it differs |
| `primary_key` | `"id"` | Must appear in `fields` |
| `tools` | `False` | Expose all five as agent tools |
| `read_scopes` | — | Guards list + get |
| `write_scopes` | — | Guards create + update + delete |
| `scopes` | — | Shorthand setting both |
| `create_table` | `True` | Emit `CREATE TABLE IF NOT EXISTS` |

!!! warning "Split reads from writes"
    Reads and writes almost never warrant the same scope. Setting only `scopes`
    gives an agent that can read your data the ability to delete it. Leaving all
    three unset makes the resource fully public — `webcortex security` reports
    that, and `webcortex dev` warns on boot.

### Schema handling

`create_table=True` emits `CREATE TABLE IF NOT EXISTS` and nothing more.

```console
$ webcortex sql
CREATE TABLE IF NOT EXISTS tickets (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  subject TEXT,
  body TEXT,
  priority INTEGER
);
```

There is deliberately **no auto-migration**. Silently issuing `ALTER TABLE`
because a Python dict changed is how frameworks destroy production data.
Migrations must be versioned, reviewable, and explicit.

## Python handlers

For logic that is not expressible as data.

```python
@app.get("/tickets/{id}/summary", tool=True, scopes=["read"])
def summarize(id: int, style: str = "short") -> dict:
    """Summarise a ticket."""
    if style not in ("short", "long"):
        raise HTTPError(422, "style must be 'short' or 'long'")
    return {"id": id, "style": style, "summary": f"Ticket #{id}"}
```

Decorators: `@app.get`, `@app.post`, `@app.put`, `@app.patch`, `@app.delete`,
or `@app.route(method, path)`.

### Two handler styles

=== "Typed parameters (preferred)"

    ```python
    @app.get("/tickets/{id}")
    def get_ticket(id: int, verbose: bool = False) -> dict:
        return {"id": id, "verbose": verbose}
    ```

    Parameters are bound by name and coerced from the annotation. The signature
    **becomes the tool schema**, so an agent gets a precise contract for free.

=== "Raw request"

    ```python
    from webcortex import Request

    @app.post("/webhook")
    def webhook(req: Request) -> dict:
        signature = req.headers.get("x-signature", "")
        payload = req.json()
        return {"received": len(req.body)}
    ```

    Name the parameter `req` or `request`, or annotate it `Request`. Use when
    you need headers, the raw body, or the caller's identity.

### The Request object

| Member | Type | Notes |
|---|---|---|
| `req.method` | `str` | |
| `req.path` | `str` | |
| `req.params` | `dict` | Path parameters |
| `req.query` | `dict` | Query string |
| `req.headers` | `dict` | Lower-cased keys |
| `req.body` | `bytes` | Raw |
| `req.json()` | `Any` | Parsed and cached |
| `req.user` | `dict` | `{id, authenticated, scopes}` |
| `req.scopes` | `list` | Convenience |
| `req.get(name, default)` | `Any` | Path → query → body |

### Returning

```python
return {"ok": True}                          # 200 JSON
return None                                  # 204 No Content
return Response({"created": 1}, status=201)  # explicit status
return Response(body, headers={"x-trace": "abc"})
raise HTTPError(404, "not found")            # clean error response
```

Dataclasses, Pydantic models, and anything with `model_dump`/`dict`/`_asdict`
serialise automatically.

!!! note "Exceptions are safe by default"
    An unhandled exception is logged in full and returns a bare
    `{"error": {"status": 500, "message": "internal error"}}`. Tracebacks are
    never sent to clients — they disclose file paths, dependency versions, and
    code structure. Set `WEBCORTEX_DEBUG_ERRORS=1` to see them in responses
    during development.

### Async handlers

```python
@app.get("/fetch")
async def fetch() -> dict:
    await asyncio.sleep(0.01)
    return {"ok": True}
```

Async handlers run on worker threads each owning an event loop; sync handlers go
to a separate thread pool so a blocking call cannot stall a shared loop.

## Pages

Server-rendered HTML, rendered in Rust with a Jinja2-compatible engine.

=== "From SQL"

    ```python
    app.page("/", "index.html",
             sql="SELECT * FROM tickets ORDER BY id DESC LIMIT 20",
             bind="tickets", scopes=["read"])
    ```

=== "From a constant"

    ```python
    app.page("/about", "about.html", data={"title": "About"})
    ```

=== "From Python"

    ```python
    @app.page_handler("/dashboard", "dashboard.html")
    def dashboard() -> dict:
        return {"stats": compute_stats()}
    ```

Every template also receives:

```jinja
{{ request.path }}  {{ request.params }}  {{ request.query }}
{{ user.id }}  {{ user.authenticated }}  {{ user.scopes }}
```

`.html`, `.htm`, and `.xml` templates autoescape. Template inheritance,
`{% extends %}`, `{% block %}`, and filters all work. See
[Frontend](frontend.md).

## Static files

```python
app.static_files("/assets", "static", index="index.html", cache_secs=3600)
```

Serves a directory with ETags and conditional `304` responses. The path gets a
wildcard segment automatically if you omit one.

Refused by the runtime: path traversal in **any** encoding (double-encoded,
overlong UTF-8, `..;/`, NUL truncation), symlinks pointing outside the root, and
dotfiles — so a stray `.env` or `.git` under a static root cannot leak.

## Proxy

The gateway primitive.

```python
app.upstream("billing", base_url="https://api.billing.internal",
             bearer_env="BILLING_TOKEN", timeout_ms=8000)

app.proxy("GET", "/billing/invoices/{id}",
          upstream="billing", rewrite="/v2/invoices/{id}",
          tool=True, scopes=["billing:read"])
```

Credentials are **named, not embedded** — `bearer_env` is resolved from the
environment at boot, so the manifest stays safe to log and diff.

The proxy forwards only `content-type` and `accept`. It never forwards the
caller's `Authorization` header: the upstream sees *your* credentials, not
whatever the client happened to send.

!!! danger "Path parameters are attacker-controlled"
    A parameter containing `..` or `/` is rejected with **400** before the
    request is issued, checked raw and after double percent-decoding. Without
    this, `GET /billing/..` reached `/` on the upstream — turning a narrow proxy
    into a general request-forgery primitive against an internal service.

## Agent

```python
app.agent("assistant", model="claude-opus-5",
          tools=["list_tickets"], scopes=["read"],
          expose_scopes=["read"], expose_at="/ask")
```

See [Agents](agents.md).

## Cross-cutting options

Every route-declaring function accepts:

| Option | Default | Purpose |
|---|---|---|
| `tool` | `False` | Expose as an agent tool |
| `tool_name` | derived | Override the generated name |
| `scopes` | `()` | Scopes required to reach it at all |
| `approval` | `"never"` | `"required"` gates agent invocation |
| `read_only` | by method | MCP hint |
| `idempotent` | by method | MCP hint |
| `summary` | docstring | |

### Derived tool names

Omit `tool_name` and it is generated from method and path:

| Route | Tool name |
|---|---|
| `GET /users` | `get_users` |
| `POST /users` | `create_users` |
| `GET /users/{id}` | `get_users_by_id` |
| `DELETE /a/b/{c}` | `delete_a_b_by_c` |

Two routes claiming the same name is a boot error, not an ambiguous tool call.

[Database :material-arrow-right:](database.md){ .md-button .md-button--primary }
