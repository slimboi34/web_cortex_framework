# Database

WebCortex ships a deliberately small data layer. Understanding what it is *not*
matters as much as what it is.

## Configuration

```python
app = WebCortex("myapp", database="sqlite://./app.db")
```

Overridable at runtime with `WEBCORTEX_DATABASE_URL`, so the same image runs in
every environment.

!!! info "SQLite today; Postgres next"
    v0.3 supports SQLite. The `Query` op assumes `?` placeholders and SQLite's
    `RETURNING` semantics; Postgres needs a small dialect layer and is the next
    milestone. For a Postgres-backed app today, use a Python handler with
    `asyncpg` or SQLAlchemy — that path works, it just runs in the interpreter.

## Declaring tables

```python
app.resource("tickets",
             fields={"id": int, "subject": str, "priority": int},
             read_scopes=["read"], write_scopes=["write"])
```

Python types map to SQLite affinities:

| Python | SQLite |
|---|---|
| `int` | `INTEGER` (`PRIMARY KEY AUTOINCREMENT` for the key) |
| `float` | `REAL` |
| `str` | `TEXT` |
| `bool` | `INTEGER` |
| `bytes` | `BLOB` |

```console
$ webcortex sql        # inspect the generated DDL
```

## Migrations: deliberately absent

`create_table=True` emits `CREATE TABLE IF NOT EXISTS` and **nothing else**.
Changing `fields` will not alter an existing table.

!!! danger "This is on purpose"
    Silently issuing `ALTER TABLE` because a Python dict changed is how
    frameworks destroy production data. Schema evolution must be versioned,
    reviewable, and explicit.

    For now, manage migrations with a tool you already trust — Alembic,
    `sqlite-utils`, or plain SQL files under version control — and set
    `create_table=False`.

## Writing queries

```python
app.query(
    "GET", "/tickets/recent",
    "SELECT * FROM tickets WHERE created_at > ? ORDER BY created_at DESC LIMIT COALESCE(?, 50)",
    params=["since", "limit"],
    returns="many",
    scopes=["read"],
)
```

`params` names the value bound to each `?`, in order, resolved from the path,
then query string, then JSON body.

`COALESCE(?, 50)` is the idiom for an optional parameter with a default: an
absent value binds as `NULL` and the default applies.

### Return shapes

```python
returns="many"      # [] of row objects
returns="one"       # single object, 404 if no match
returns="affected"  # {"affected": n, "last_insert_id": m}
```

### Writes that return the row

SQLite supports `RETURNING`, which is how `app.resource` gives you the created
row rather than just an id:

```python
app.query("POST", "/tickets",
          "INSERT INTO tickets (subject, priority) VALUES (?, ?) RETURNING *",
          params=["subject", "priority"], returns="one", scopes=["write"])
```

## Injection

Values are **always bound, never interpolated**.

```console
$ curl "localhost:8000/tickets?limit=1%20OR%201=1"
{"error":{"status":400,"message":"the request contained a value this query cannot use"}}
```

That 400 *is* the proof: `1 OR 1=1` arrived as a string bound to a LIMIT, which
is a type error rather than an injected clause.

The SQL text itself must come from your source. Never build a query string from
request data — if you find yourself wanting to, use a Python handler and a real
query builder.

!!! note "Client faults are 400, not 500"
    A bad parameter type is the *caller's* mistake, so it gets a 4xx. Conflating
    it with a server fault inflates error budgets and pages on-call for client
    mistakes. Detail stays in the log; the client is told only which side is at
    fault.

## Using Python instead

When you need transactions, joins across services, or an ORM:

```python
import sqlite3

@app.get("/reports/summary", scopes=["read"])
def summary() -> dict:
    con = sqlite3.connect("app.db")
    try:
        rows = con.execute(
            "SELECT status, COUNT(*) FROM tickets GROUP BY status"
        ).fetchall()
        return {"by_status": dict(rows)}
    finally:
        con.close()
```

This runs on the Python worker pool — slower than a `Query` op, but it is real
Python and everything is available.

!!! tip "There is no WebCortex ORM, and there will not be one"
    SQLAlchemy is twenty years of accumulated correctness around identity maps,
    lazy loading, and transaction boundaries. Competing with it is a multi-year
    project orthogonal to everything interesting here.

    `app.query` is not an ORM — it is a way to bind a route to SQL so Rust can
    execute it. When you need object mapping, use SQLAlchemy inside a handler.

## Performance notes

Reads sustain ~22,000 req/s. Writes are different:

| Operation | req/s | p50 | p99 |
|---|---:|---:|---:|
| `SELECT` | 22,317 | 0.92 ms | 2.88 ms |
| `INSERT` | 2,461 | 2.92 ms | **119.21 ms** |

That write tail is **SQLite serialising writers**, not the framework. It is a
property of the database and a good reason to prioritise Postgres for
write-heavy workloads.

[Frontend :material-arrow-right:](frontend.md){ .md-button .md-button--primary }
