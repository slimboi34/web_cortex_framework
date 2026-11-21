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

With no `input_schema=`, the route advertises one derived from `params` (since
2.9; before, MCP clients, OpenAPI and agents saw an empty object): one property
per name, `@principal` left out, required when it is a `{name}` in the path,
typed from a resource field of the same name when the SQL names that resource's
table, otherwise a string (or any JSON value in a `POST`, `PUT` or `PATCH`
body). The description defaults to `summary`, else the first line of the SQL.
An explicit `input_schema=` still wins.

`returns` controls the shape:

| Value | Response |
|---|---|
| `"many"` | JSON array of row objects |
| `"one"` | Single object, or **404** when nothing matches |
| `"affected"` | `{"affected": n, "last_insert_id": m}`; `m` is `null` unless the statement inserted a row |

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

### What the generated routes accept

Each route advertises an input schema, and since 2.9 a body that breaks it is
refused with **422** (`WC206`, naming the field) before any SQL runs: a missing
required field, a key the resource does not declare, or a value of the wrong
type. `POST` needs every field except a generated integer key. `PUT` replaces
the whole row, so it needs every writable field too; there is no partial
update (`PATCH`) yet, so send the row back with your change:

```python
row = ctx.call("get_tickets", id=41)
ctx.call("update_tickets", **{**row, "priority": 1})
```

`GET /tickets` is newest first (by primary key, descending). `limit` defaults
to 50 and is clamped to 1–1000; `offset` defaults to 0 and is never negative.
A `bool` field reads back as `true`/`false`, not SQLite's `1`/`0`.

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

    Parameters are bound by name and converted to their annotations. The
    signature **becomes the tool schema**, so an agent gets a precise contract
    for free.

    A conversion is not a validation. A value that cannot be converted reaches
    the handler **unchanged**: `/tickets/abc` calls `get_ticket` with
    `id == "abc"`, and `verbose=maybe` arrives as the string `"maybe"`. The
    same holds for a dataclass body parameter: a body with a key the class does
    not declare, or without a required field, arrives as the raw `dict`, and a
    field that cannot be converted keeps the value that was sent. Check what you
    rely on, raise `HTTPError(422, ...)` when it is wrong, and never put a
    parameter into SQL, a shell command or a file path on the strength of its
    annotation. A model's tool call is bound the same way, so the same applies
    when an agent is the caller.

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
| `req.query` | `dict` | Query string; a repeated key keeps its last value |
| `req.query_list(name)` | `list[str]` | Every value of a query key, in order (`?tag=a&tag=b`) |
| `req.headers` | `dict` | Lower-cased keys |
| `req.body` | `bytes` | Raw |
| `req.json()` | `Any` | Parsed and cached. A body that is not JSON, or (since 2.9) is nested too deeply to parse, raises a `ValueError` that answers 400 `WC205` if you let it propagate |
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

`.html`, `.htm`, `.xhtml`, `.xml` and `.svg` templates autoescape (also as
`page.html.j2`), and since 2.9 the template's extension also sets the response's
content type, so a `.txt` or `.json` page is no longer served as HTML. Template
inheritance,
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

Since 2.9 static files are streamed from disk rather than read into memory
whole, with the same ETags and traversal checks.

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

Redirects are relayed, never followed. Since 2.9 a `3xx` from the upstream goes
back to the caller with its `Location` header (rewritten when it points inside
the upstream's `base_url`), and the proxy sends no `Referer`. Following it would
have sent your upstream credentials to whatever host the redirect named.

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
| `approval` | `"never"` | `"required"` gates agent, behaviour and MCP calls (not a direct HTTP call) |
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

### How a request path is matched

A request path is normalised before it is matched (since 2.9): percent-encoded
unreserved characters are decoded and hex is upper-cased, so `/p%69ng` is
`/ping`, and a route with a non-ASCII literal segment (`/café/{n}`) answers
whether the client sends raw UTF-8, `%C3%A9` or `%c3%a9`. `%2F`, `%5C` and
`%25` stay encoded, so an encoded separator never splits a segment, and a
parameter is still decoded exactly once.

[Database :material-arrow-right:](database.md){ .md-button .md-button--primary }
