# FAQ

### How is this different from FastAPI?

FastAPI is an excellent ASGI framework; your handler still runs in Python for
every request. WebCortex moves the boundary to build time — declared routes
compile to a plan Rust executes, and the interpreter is never woken for them.

The other difference is the agent surface. FastAPI can produce an OpenAPI
document you then convert to MCP tools with a separate library and a separate
server. Here the route *is* the tool.

### Is it faster than Django or FastAPI?

**Unproven, and not claimed.** The benchmarks on this site compare WebCortex's
own route kinds against each other, on a stdlib Python load generator that is
probably the bottleneck. The defensible claim is the ratio: native routes
sustain ~2.5x the throughput of the Python bridge path.

A real comparison needs `wrk` or `oha` on a tuned host. Until that exists,
treat any cross-framework number as marketing.

### Do I have to use free-threaded Python?

No. WebCortex runs correctly on a GIL build and tells you which mode it is in.
Free-threading is the fast path for Python-backed routes — measured 4.82x versus
1.38x scaling at concurrency 8 — but native routes are unaffected either way.

### Can I use SQLAlchemy / Django ORM / asyncpg?

Yes, inside a Python handler. `app.query` is not an ORM and there will not be
one — see [Database](database.md#using-python-instead).

### Why is there no auto-migration?

Because silently issuing `ALTER TABLE` because a Python dict changed is how
frameworks destroy production data. `create_table=True` emits
`CREATE TABLE IF NOT EXISTS` and nothing more.

### Does it support Postgres?

Not in v0.3 — SQLite only for `Query` ops. Postgres needs a small dialect layer
and is the next milestone. You can use Postgres today from a Python handler.

### Can agents modify my data?

Only within the scopes their caller holds, and never through an
approval-gated tool without a human. See
[Agents](agents.md#2-authority-is-delegated-never-granted).

### What happens if a handler raises?

It is logged in full and the client gets a bare 500. Tracebacks are never
returned — they disclose file paths, dependency versions, and code structure.
`WEBCORTEX_DEBUG_ERRORS=1` opts back in for development.

`HTTPError` is different: it is deliberately raised by your code, so its message
reaches the caller.

### What if a Behaviour calls itself?

It is bounded by `max_invocation_depth` (default 8) and halts cleanly with a
508. This was a real vulnerability before the depth counter existed — see
[How it works](concepts.md#nesting-is-bounded).

### Is it secure?

It has been through an adversarial review of its own controls; six issues were
found and fixed, each with a regression test.
[SECURITY.md](https://github.com/slimboi34/web_cortex_framework/blob/main/SECURITY.md)
reports them **and what was not tested** — no fuzzing, no TLS, no CSRF, and a
rate limiter that needs trusted-proxy configuration at an edge.

It was also a self-review, with the blind spots that implies.

### Can I serve TLS directly?

No. WebCortex serves plaintext and expects a terminating proxy. Certificate
lifecycle belongs in infrastructure you already operate.

### Why "WebCortex"?

It was built as *Pylon*, briefly renamed *Rango* — both taken on PyPI. The
current name was checked against PyPI, crates.io, npm, and GitHub before the
rename.

### How do I contribute?

Open an issue or PR at
[github.com/slimboi34/web_cortex_framework](https://github.com/slimboi34/web_cortex_framework).
CI runs the Rust and Python suites across four interpreters and five platform
targets, plus clippy with `-D warnings` and `cargo audit`.
