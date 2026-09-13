# Security review — v0.3

An adversarial review of WebCortex conducted against its own controls: authentication,
authorization, injection, traversal, SSRF, resource exhaustion, and information
disclosure, plus stress and soak testing.

**Six issues were found and fixed.** Every one now has a regression test in
[`tests/test_pentest.py`](tests/test_pentest.py), because a security control that
silently stops working looks exactly like one that is working.

This document reports what was tested and what was found. It is not a claim that
the framework is secure — see [Limits](#limits-of-this-review).

---

## Findings

### 1. Remote DoS and total auth failure via JWT — **Critical**, fixed

`jsonwebtoken` 11 requires exactly one crypto-provider feature to be selected.
Neither was enabled, so the library **panicked inside `decode`**.

The panic was reachable *before* any validation, by any unauthenticated client
sending `Authorization: Bearer <anything>`. The connection was severed with no
status line. Any application configuring JWT auth was completely broken, and the
failure mode — a dropped connection rather than an error — is close to the worst
case for diagnosis.

Missed by the original test suite because those tests only exercised JWT
*configuration validation*, never a token decode.

**Fix:** `features = ["rust_crypto", "use_pem"]`, both load-bearing —
`rust_crypto` supplies the provider, `use_pem` supplies the RSA/EC PEM parsers
that `default-features = false` had removed.

### 2. No panic boundary on the request path — **High**, fixed

Finding 1 was severe partly because nothing contained it. A panic anywhere in
request handling propagated out and killed the connection.

**Fix:** the request path is wrapped in `catch_unwind`. A panic now becomes a 500
for that one request, with the detail logged and never sent to the client.
Defence in depth: this class of dependency bug will now be visible instead of
mysterious.

### 3. Proxy path traversal → SSRF primitive — **High**, fixed

A path parameter substituted into a proxy `rewrite` template could contain
traversal segments and climb out of the intended upstream prefix. Confirmed:
`GET /proxy/..` caused the upstream to receive `/` rather than `/echo/..`.

Where the upstream is an internal service, this turns a deliberately narrow
proxy into a general request-forgery primitive against it.

**Fix:** path parameters are rejected if they contain `..`, `/`, `\`, or a NUL,
checked both raw and after double percent-decoding; the assembled path is then
re-checked for traversal segments before the request is issued.

### 4. Unbounded behaviour recursion exhausts the worker pool — **High**, fixed

A behaviour that calls itself recursed without limit. Each nested invocation
received a **fresh step budget**, so the per-behaviour `max_steps` cap never
bounded the total — and every level held a Python worker thread while waiting on
the level below.

Confirmed empirically. With a recursion bomb running, **0 of 60** subsequent
behaviour requests completed; without it, **60 of 60** completed in 6.0s. One
request could permanently deny every Python-backed route in the application.

The 30-second request timeout returned a 504 to the *caller* but did not stop
the work, so the leak persisted after the client gave up.

**Fix:** `WebCortexRequest` now carries an invocation `depth`, incremented on every
in-process tool call and enforced against `server.max_invocation_depth`
(default 8) before a behaviour or agent op runs. Exceeding it returns 508 Loop
Detected, surfaced to the behaviour as a clean halt. After the fix the bomb is
stopped in 0.01s and the pool recovers fully.

### 5. Python tracebacks returned to clients — **Medium**, fixed

An unhandled exception in a handler or behaviour returned the full traceback in
the response body, disclosing absolute file paths, dependency versions, and code
structure.

**Fix:** tracebacks are always written to the server log and never to the
response. Set `WEBCORTEX_DEBUG_ERRORS=1` to opt back in during development.

### 6. Client input faults reported as server faults — **Low**, fixed

A bad parameter type reached SQLite and surfaced as a 500. Beyond inflating
error budgets and paging on-call for client mistakes, conflating the two makes
genuine server faults harder to spot.

**Fix:** `QueryError` distinguishes caller fault (400) from internal fault (500).
Detail stays in the log; the client is told only which side is at fault.

### Also corrected: implicit 60-second JWT expiry grace

`jsonwebtoken` defaults to 60 seconds of leeway on `exp`, so a token expired a
minute ago still authenticated. That is the library's documented default rather
than a bug, but a framework inheriting it silently is a surprise.

Now explicit and configurable (`leeway_secs`, default 30), with `exp` stated as
required so a future library default cannot start accepting non-expiring tokens.

---

## What held under attack

No issue found in any of these. Each has regression tests.

**Authentication.** Key prefix/suffix/case forgery; empty keys; non-Bearer
`Authorization` schemes. JWT `alg=none`, signature stripping, payload tampering
with a retained signature, forged signatures, algorithm confusion (HS256→HS512),
expired tokens, and tokens with no `exp` — all rejected.

**Authorization.** Scope matching is exact, not prefix-based: a token claiming
`admin:readonly` does not satisfy `admin`. The MCP tool list is filtered per
caller — a reader sees 3 tools where an admin sees 6 — and calls cannot exceed
the caller's scopes. Approval gates hold over MCP and cannot be laundered by
wrapping the gated tool in a behaviour. Agent and behaviour scopes are
intersected with the caller's, never unioned.

**Injection.** SQL injection through path parameters, query strings, and JSON
bodies: parameters reach SQLite as bound data, never syntax. `1 OR 1=1` in a
`LIMIT` produces a *type error*, which is the proof parameterisation held. Stored
XSS is escaped on render. Stored template syntax is not evaluated (no SSTI). CRLF
in a handler-supplied header value does not forge response headers. Spoofed
identity headers (`x-user`, `x-scopes`, …) confer nothing.

**Traversal.** Ten encodings of static path traversal — including double
encoding, overlong UTF-8, `..;/`, and NUL truncation — all refused. Symlinks
pointing outside the static root are not followed. Dotfiles are never served.

**Resource limits.** 33 MB bodies rejected; 400-deep nested JSON, 5,000 query
parameters, and 60 KB URLs all handled without crashing; malformed JSON and six
malformed MCP payload shapes handled cleanly.

**Disclosure.** Errors carry no SQL, no SQLite strings, no Rust source paths, no
backtraces. Protected routes answer 401 rather than 404, so route existence is
not probed via status codes. No `Server` or `X-Powered-By` version advertising.

**CORS.** Untrusted origins are not reflected, including `null` and
suffix-confusion attempts (`https://allowed.test.evil.test`). Preflight from an
untrusted origin is refused. `*` with credentials is rejected at boot.

---

## Stress and soak

M-series arm64, CPython 3.14.4 free-threaded, 24–32 concurrent clients.

| Route kind | req/s | p50 | p99 |
|---|---:|---:|---:|
| Static (Rust) | 29,287 | 0.69 ms | 2.66 ms |
| Query (Rust + SQLite) | 22,317 | 0.92 ms | 2.88 ms |
| Page (Rust + minijinja) | 21,869 | 0.95 ms | 2.98 ms |
| Static files (Rust) | 21,633 | 0.91 ms | 4.31 ms |
| Python handler | 8,675 | 2.51 ms | 7.02 ms |
| Insert (Rust + SQLite) | 2,461 | 2.92 ms | 119.21 ms |

**Soak: 1,786,805 requests over 120 seconds, 0 errors, 0 panics.**

Memory reached steady state rather than leaking. Growth per 5s interval
decelerated across the run and the second half was **negative** (+15.6 MB then
−3.1 MB), settling at ~286 MB and holding at idle. The larger figure seen on a
short run (+197 MB) is cold-start warm-up to working set, not accumulation.

Two things worth knowing rather than fixing:

- **SQLite writes have a long tail** (p99 119 ms). SQLite serialises writers;
  this is the database's property, not the framework's. It is a reason to
  prioritise the Postgres dialect.
- **The load generator is stdlib Python** and is likely the ceiling on the
  ~21–29k rows. These are floors, not maxima. A proper benchmark against
  Django/FastAPI still needs `wrk`/`oha` on a tuned host, and is still owed
  before any performance claim.

---

## Limits of this review

Stated plainly, because a security document that only lists successes is
marketing.

- **Self-review.** Same author, same blind spots. Findings 1 and 4 were missed by
  the original suite for exactly that reason.
- **Not tested:** TLS termination (WebCortex serves plaintext and expects a
  terminating proxy), HTTP/2-specific attacks, request smuggling, slowloris and
  header-read timeouts, timing side channels measured statistically rather than
  by construction, and the live Anthropic provider path.
- **No fuzzing.** The manifest parser, the JSON-RPC surface, and the router are
  all reachable pre-auth and deserve a fuzz harness.
- **Dependency audit is now wired into CI** (`cargo audit` on every push). It
  found one advisory on its first run, documented below.
- **Rate limiting is per-process and keys anonymous traffic by socket address.**
  Behind a load balancer every anonymous caller shares one bucket, which
  degrades legitimate users rather than attackers. `X-Forwarded-For` is
  deliberately not trusted — it is client-controlled — but that means the
  limiter needs an explicit trusted-proxy configuration before it is useful at
  the edge.
- **No CSRF or session support.** The page layer suits internal tools, not
  public authenticated browser apps.
- **A behaviour holds a worker thread for its whole run.** Correct for
  procedures measured in seconds, wrong for ones measured in hours.

## Known advisories

**RUSTSEC-2023-0071 — Marvin Attack in `rsa` 0.9.x** (medium, 5.9). Reached
transitively via `jsonwebtoken`. No upstream fix exists.

**Assessed as not exploitable in WebCortex**, and the exception is recorded with its
reasoning in [`.cargo/audit.toml`](.cargo/audit.toml) rather than silenced on a
command line. The advisory concerns RSA *private key* operations — PKCS#1 v1.5
decryption and signing. WebCortex only ever constructs `DecodingKey` and only ever
*verifies* JWT signatures, which is a public-key operation. It never issues or
signs tokens; there is no `EncodingKey` in the codebase.

The exception must be revisited if WebCortex gains token-issuing capability, and can
be dropped entirely if `jsonwebtoken` is switched to the `aws_lc_rs` provider,
which does not depend on the `rsa` crate. That switch was not taken here because
it introduces a C toolchain dependency into the cross-platform wheel build.

## What v2 added to the surface

The orchestration release (2.0.0) widened what an agent can reach and what a
request can cost. Each addition is listed with the control that bounds it and
the test that proves the control holds. None of this has had a second
adversarial pass yet; that is owed, and the same limits as the review above
apply.

| Addition | Risk | Control | Proven by |
|---|---|---|---|
| Every agent is a route and a tool | An agent declared without scopes is a public endpoint that spends tokens | `expose_scopes` guards the route (defaults to `scopes`); `webcortex security` lists it; a run executes as a delegate of the caller regardless | `test_orchestration.py::test_every_agent_is_a_tool_named_after_itself`, `test_security.py` |
| Supervisors calling agents as tools | Nested runs each had their own budget; recursion through agents bypassed the depth ceiling in 0.3 | Depth now propagates through agent tool calls; a `SharedBudget` travels with the request tree | `agent.rs::nested_agent_calls_carry_depth_so_a_cycle_is_bounded`, `::a_shared_budget_bounds_a_supervisor_and_its_workers_together`, `test_a_supervisor_and_its_worker_share_one_budget` |
| Handoffs | A specialist could hold scopes the caller lacked | Re-delegated from the original caller, then intersected with the previous agent's scopes; targets validated at boot | `agent.rs::a_handoff_swaps_the_agent_and_shrinks_authority`, `::a_handoff_to_an_undeclared_agent_is_refused` |
| Sessions | One caller reading another's conversation | Store key includes the principal id; bounded and expiring | `session.rs::round_trips_and_is_keyed_by_principal`, `test_sessions_carry_the_conversation_and_are_per_principal` |
| Approval resume | Bypassing the gate, resuming twice, resuming as the approver | Deciding needs `webcortex:admin`; the run resumes as the original delegate; a decision is consumed; suspended runs expire | `test_a_gated_tool_suspends_and_the_run_resumes_after_approval`, `test_approvals_need_the_admin_scope`, `test_a_gate_cannot_be_laundered_through_gather` |
| `gather` / `ask_many` | Fanning out to bypass admission or budgets | Every item is admitted, scoped, gated and charged as a single call; one step each | `test_gather_runs_tool_calls_concurrently_and_reports_failures`, `test_a_gate_cannot_be_laundered_through_gather` |
| Memory | Reading another user's memory through an agent | Rows keyed by `@principal`, the root of the delegation chain, in the SQL itself | `test_memory_is_per_root_principal_and_executed_in_rust`, `test_an_agent_writes_memory_as_the_human_behind_it`, `auth.rs::the_root_principal_survives_a_chain_of_delegation` |
| Context providers | A behaviour reading context it was not given; a SQL provider leaking across callers | Behaviours may read only declared providers; SQL providers bind `@principal` | `test_a_behaviour_reads_only_the_context_it_declared`, `test_context_providers_land_in_the_system_prompt` |
| Flows | Unbounded fan-out, self-containing flows | Steps validated at boot; a flow cannot contain itself; one shared budget; depth propagates | `manifest.rs` validation, `test_a_flow_cannot_contain_itself`, `test_a_pipeline_feeds_each_agent_the_previous_output` |
| OpenAI-compatible provider | A second wire format is a second parser | Translation is at the boundary; the canonical shape is unchanged; parsing is unit-tested on malformed input | `provider.rs` tests |
| Control-plane endpoints (`/usage`, `/models`, `/approvals`, `/flows`, `/contexts`) | Disclosure | All require `webcortex:admin` once any auth is configured, like the rest of the plane | `test_approvals_need_the_admin_scope` |
| `WEBCORTEX_FAKE_PROVIDER` | Shipping a test double | Selected only by that variable; logs a warning at boot; documented as tests-only | — |

**Known limits specific to v2:** compaction sends earlier turns to a model to
summarise, so anything a tool returned is re-sent at least once more; the
summariser prompt is the one probabilistic step in an otherwise deterministic
loop. Tool results are bounded by size, not by content — a tool that returns a
secret returns it to the model. Sessions and suspended runs are in memory, so
a restart forgets them, which is stated but is still a loss. The `evolve`
command sends the context pack — route paths, tool descriptions, security
posture, but never credentials — to whichever model is named.

## Reporting


This is a personal project without a disclosure process. Open an issue.
