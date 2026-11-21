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

Presented as `x-api-key` by default. To read keys from another header, pass
`WebCortex(..., api_key_header="x-service-key")` (since 2.9); the name must be a
valid header name, and `authorization`, `cookie` and other reserved names are
refused.

A missing or empty env var logs a loud warning at boot — a silently missing key
means an endpoint you believe is reachable is not. A value that still starts
with `replace-me` (a starter's placeholder) is treated the same way: the key is
not registered, so a copied `.env.example` can never become a working admin
key.

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
- `aud` and `iss` are **required** whenever `audience` or `issuer` is set: a
  token that leaves the claim out is refused (401), exactly like one that
  carries the wrong value. Before 2.9 a token without the claim was accepted
- HS256, HS384, HS512, RS256, RS384, RS512, ES256, ES384 and EdDSA are
  supported; any other `algorithm` is a `ValueError` at declaration. For the
  asymmetric ones `secret_env` holds the PEM public key
- HMAC secrets under 32 bytes are rejected at boot as brute-forceable
- A token whose `sub` equals the `id` of a configured API key, or starts with
  `watcher:`, is refused (401). Sessions, memory and `@principal` are keyed by
  that id, so such a token would otherwise act as the service or the watcher

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

Every route whose scopes are all in `anonymous_scopes` is public, and since 2.9
`webcortex security` lists it under `public_routes` alongside the unscoped ones.
That includes agents, behaviours and flows: with `anonymous_scopes("read")`, an
agent whose `expose_scopes` is `["read"]` can be started, and can spend tokens,
by anyone. Give such an agent a `token_budget`, or expose it under a scope
anonymous callers do not hold.

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

### Admin keys and MCP clients

The control plane (`/_webcortex/...`) needs `webcortex:admin` once any
authentication is configured, and that includes the MCP endpoint. A key you
give an MCP client (Claude Code, Cursor, a coding agent) is therefore a full
admin credential: with it the client can also decide approvals, halt and
release actuators, and read the audit log and traces. Give it only to tools you
would trust with that, and prefer a key of its own so it can be revoked alone.

### WebSockets

A socket is authenticated once, at the upgrade, and held to its route's scopes.
Since 2.9 two more limits apply for its whole life:

- **Each agent turn is rate limited** like a separate request. A turn over the
  limit is not run; the socket answers with `WC401` and `retry_after`.
- **A JWT's authority ends at its expiry.** After `exp` (plus `leeway_secs`) an
  agent socket refuses the next turn with `WC603` and closes with 1008, and a
  stream or ingest socket is closed.

`?access_token=` on an upgrade accepts an API key or, when JWT is configured, a
JWT. See [The device hub](device-hub.md#authentication-for-sockets).

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

### Cross-origin requests (CSRF and WebSocket hijacking)

CORS stops a hostile page *reading* a response, not *sending* the request: a
form or a `text/plain` `fetch` POST goes out without a preflight, and a
WebSocket is not subject to CORS at all. So every state-changing request
(anything but `GET`, `HEAD` and `OPTIONS`) and every WebSocket upgrade that a
browser marks as cross-origin is refused with `403`, unless its origin is one
`app.cors(...)` allows. Since 2.4.1.

The browser's own word decides: `Sec-Fetch-Site: same-origin` (or `none`) is
allowed, any other value is not; without that header, the `Origin` must match
`Host`. A request with neither header did not come from a web page — API
clients, devices, `curl`, `webcortex.client` — and is unaffected. To accept a
cross-origin browser app, list its origin in `app.cors(...)`.

This matters most for an app on `localhost` or a LAN with anonymous scopes,
which any page the operator visits could otherwise drive. It does not defend
against DNS rebinding, where the attacker's page *is* same-origin: an app with
actuators or sensitive tools should require a key even on `localhost`.

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
footgun in local development. WebCortex itself speaks plain HTTP, so "over TLS"
means the TLS proxy in front of it said so with `X-Forwarded-Proto: https` or
`Forwarded: proto=https`. Set `hsts_max_age=0` to never send it.

### Request limits

| Limit | Default | Configurable |
|---|---|---|
| Body size | 32 MB | compile-time; since 2.9 a body is read only after the route matched and the caller passed its scope check, so a 401, 403, 404 or 405 is answered without reading it |
| Sending a response | a connection whose client stops reading the response body is closed after a short period with no progress; reads and pings from that client do not count | not yet |
| Request timeout | 30s | `request_timeout` (`agent_timeout` on agent, behaviour and flow routes); the body must arrive within it too, or 408 `WC216` |
| Header read / idle connection | the request timeout, at most 30s | `request_timeout` |
| Shutdown drain | 25s | `shutdown_timeout`; WebSockets are closed with 1001 (going away) |
| Open connections | 10,000, WebSockets included | `max_connections`; past it a 503 `WC218` and close |
| Proxied response body | 64 MiB | `app.upstream(..., max_response_bytes=...)`; past it a 502 `WC217` |
| Invocation depth | 8 | not yet (`server.max_invocation_depth`) |
| Concurrent behaviour runs | the behaviour pool's size, separate from Python route handlers | past it a `503`, not a queue |
| Pending approvals and sessions | 1,000 each, with a share per caller | not yet; a caller over its share evicts its own oldest entry, never another caller's |

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
Its 2.9.0 section covers the stress test behind this release: an approval gate
bypass through a flow step, a slow-read denial of service and a worker-pool
deadlock, all fixed, and the behaviour changes that came with them.

## Checklist before you ship

```console
$ webcortex security
```

- [ ] `auth_configured` is `true`
- [ ] `public_routes` contains **only** what you intend to be public
- [ ] No `POST`/`PUT`/`DELETE` route appears in `public_routes`
- [ ] `rate_limited` is `true`
- [ ] `anonymous_scopes` is empty, or deliberately minimal
- [ ] Every agent, behaviour and flow in `public_routes` has a `token_budget`
- [ ] Every agent's `scopes` is the minimum it needs
- [ ] Destructive tools carry `approval="required"`, and the people who can
      call them directly hold the scope on purpose
- [ ] With JWT, `audience` and `issuer` are set
- [ ] Keys given to MCP clients are separate admin keys you can revoke
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
- **No sessions or CSRF tokens.** Cross-origin state-changing requests are
  refused (above), but the page layer suits internal tools and API-key clients,
  not public authenticated browser apps.
- **Rate limiting is per-process** and needs trusted-proxy configuration to be
  meaningful at an edge.
- **No coverage-guided fuzzing.** The manifest parser, JSON-RPC surface, and
  router are all reachable pre-auth and deserve a fuzz harness.
- **Untested:** TLS (WebCortex never speaks it), the timing of key comparison
  under statistical measurement, and behaviour against real model providers;
  every agent test runs against the deterministic fake.
- **Typed parameters are converted, not validated.** A value that cannot be
  converted reaches the handler unchanged; see
  [Routing](routing.md#two-handler-styles).
- **Device keys share a scope by default.** See
  [One key per device](device-hub.md#one-key-per-device).
- **Approval resume is manual.** The runtime records the request; wiring
  approval into a UI is application work.

[Deployment :material-arrow-right:](deployment.md){ .md-button .md-button--primary }
