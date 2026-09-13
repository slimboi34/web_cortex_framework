# Security

Every default here is the safe one. Relaxing a control is what costs a line of
code, not tightening it — a framework where security is opt-in ships insecure
applications.

## Authentication

Two mechanisms. Both establish a **principal** once, at the edge.

### API keys

```python
app.api_key("BILLING_KEY", id="billing-service", scopes=["billing:read"])
app.api_key("ADMIN_KEY", id="admin", scopes=["*"])
```

The argument is the **name of an environment variable**, never the key itself.
Keys stay out of your source, out of the manifest, and out of anything you log
or hand to an agent.

The runtime stores a SHA-256 of each key and compares in constant time.

```console
$ webcortex keygen
wcx_rU2Y-W1mTd2Jw4FdlYAaAJYFM5P_CE69sCwNhRG4aPk
```

Presented as `x-api-key` by default (`api_key_header` to change).

A missing or empty env var logs a loud warning at boot — a silently missing key
means an endpoint you believe is reachable is not.

### JWT

```python
app.jwt(secret_env="JWT_SECRET", algorithm="HS256",
        audience="api.example.com", issuer="https://auth.example.com",
        leeway_secs=30)
```

Presented as `Authorization: Bearer <token>`. Scopes are read from the standard
`scope` (space-delimited) or `scopes` (array) claims.

- `exp` is **required** and always validated
- `nbf` is validated
- HS256/384/512, RS256/384/512, ES, and EdDSA are supported
- HMAC secrets under 32 bytes are rejected at boot as brute-forceable

!!! note "Expiry leeway is explicit"
    The underlying library defaults to **60 seconds** of grace on `exp`, so a
    token expired a minute ago still authenticates. WebCortex pins it to 30 and
    exposes it, because inheriting that silently is a surprise.

### Anonymous callers

```python
app.anonymous_scopes("read")   # empty by default
```

Empty by default: unauthenticated means unprivileged. Use sparingly — this is
how a route you believed was protected becomes public.

### Error semantics

| Situation | Status |
|---|---|
| No credential, scoped route | **401** — the client can fix this |
| Valid credential, missing scope | **403** — authenticating again will not help |
| Malformed or expired credential | **401**, with a reason |

Collapsing these into one status makes auth bugs very hard to debug.

## Authorization

Scopes are matched **exactly**. There is no prefix matching:

```python
app.static("GET", "/vault", {...}, scopes=["admin"])
```

A token claiming `admin:readonly` does **not** satisfy `admin`. Glob-style
scopes invite mistakes that are invisible in review. The single wildcard `*`
grants everything.

Split reads from writes:

```python
app.resource("invoices", fields={...},
             read_scopes=["billing:read"],
             write_scopes=["billing:write"])
```

### Delegation

Agents and Behaviours run with **intersected** scopes — never more than the
caller. See [Agents](agents.md#2-authority-is-delegated-never-granted).

## Transport hardening

### CORS

```python
app.cors("https://app.example.com", "https://admin.example.com",
         credentials=True, max_age=600)
```

Disabled by default. Untrusted origins are never reflected — including `null`
and suffix-confusion attempts like `https://app.example.com.evil.test`. The
response always carries `Vary: Origin` so a cache cannot serve one origin's
response to another.

`credentials=True` with a `*` origin is **rejected at boot**. The combination is
forbidden by the CORS spec and browsers fail it in ways that are maddening to
debug.

### Rate limiting

```python
app.rate_limit(per_second=50, burst=100)
```

A sharded token-bucket limiter keyed by **principal**, falling back to client IP
for anonymous traffic — so one tenant cannot exhaust another's budget. Idle
buckets are evicted, so an attacker rotating keys cannot grow the map without
bound.

Rejections carry `Retry-After`, `X-RateLimit-Limit`, and `X-RateLimit-Remaining`.

!!! warning "Behind a load balancer"
    `X-Forwarded-For` is deliberately **not** trusted — it is client-controlled,
    and trusting it hands an attacker unlimited bucket rotation. The consequence
    is that behind a proxy every anonymous caller shares one bucket, which
    degrades legitimate users rather than attackers.

    Trusted-proxy configuration is not yet implemented. Until it is, rate limit
    at your edge as well.

### Security headers

On by default, on every response including errors:

```
x-content-type-options: nosniff
x-frame-options: DENY
referrer-policy: strict-origin-when-cross-origin
cross-origin-opener-policy: same-origin
strict-transport-security: max-age=31536000; includeSubDomains   (HTTPS only)
```

```python
app.security_headers(
    frame_options="SAMEORIGIN",
    content_security_policy="default-src 'self'",
)
```

HSTS is emitted only over TLS — over plaintext browsers ignore it, and it is a
footgun in local development.

### Request limits

| Limit | Default | Configurable |
|---|---|---|
| Body size | 32 MB | compile-time |
| Request timeout | 30s | `request_timeout` |
| Shutdown drain | 25s | `shutdown_timeout` |
| Invocation depth | 8 | not yet (`server.max_invocation_depth`) |

## What has been tested

WebCortex has been through an adversarial review of its own controls. Six issues
were found and fixed; each has a regression test in `tests/test_pentest.py`.

**Held under attack:** JWT `alg=none`, signature stripping, payload tampering
with a retained signature, algorithm confusion, expired and non-expiring tokens;
key prefix/suffix/case forgery; exact-match scope enforcement; MCP per-caller
filtering; approval gates over MCP and through Behaviours; SQL injection via
path, query, and body; stored XSS; SSTI; CRLF header injection; ten encodings of
path traversal; symlink escape; dotfile exposure; oversized bodies; 400-deep
nested JSON; malformed MCP payloads; CORS origin reflection.

Full detail — **including what was not tested** — in
[SECURITY.md](https://github.com/slimboi34/web_cortex_framework/blob/main/SECURITY.md).

## Checklist before you ship

```console
$ webcortex security
```

- [ ] `auth_configured` is `true`
- [ ] `public_routes` contains **only** what you intend to be public
- [ ] No `POST`/`PUT`/`DELETE` route appears in `public_routes`
- [ ] `rate_limited` is `true`
- [ ] `anonymous_scopes` is empty, or deliberately minimal
- [ ] Every agent's `scopes` is the minimum it needs
- [ ] Destructive tools carry `approval="required"`
- [ ] Secrets come from the environment; none are in source
- [ ] TLS terminates at a proxy in front of WebCortex
- [ ] `WEBCORTEX_DEBUG_ERRORS` is **not** set in production
- [ ] `cargo audit` is clean

`webcortex dev` prints a warning on every boot when authentication is configured
but routes remain public — an unintentionally public route is the failure that
actually happens.

## Known gaps

Stated plainly, because a security page that only lists successes is marketing.

- **No TLS.** WebCortex serves plaintext and expects a terminating proxy.
- **No CSRF protection or sessions.** The page layer suits internal tools and
  API-key clients, not public authenticated browser apps.
- **Rate limiting is per-process** and needs trusted-proxy configuration to be
  meaningful at an edge.
- **No fuzzing.** The manifest parser, JSON-RPC surface, and router are all
  reachable pre-auth and deserve a fuzz harness.
- **Approval resume is manual.** The runtime records the request; wiring
  approval into a UI is application work.

[Deployment :material-arrow-right:](deployment.md){ .md-button .md-button--primary }
