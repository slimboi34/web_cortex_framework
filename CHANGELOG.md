# Changelog

Notable changes per release. Versions follow [semantic versioning](https://semver.org);
while the major version is 0, minor bumps may contain breaking changes.

## [0.3.0] — 2026-08-04

The first release intended for other people to install.

### Renamed

The project is now **Rango**. It was developed under the name Pylon, which is
taken on PyPI.

- Import and CLI: `rango` (`from rango import Rango`, `rango dev`)
- Distribution on PyPI: **`rango-framework`** — `rango` is held by an abandoned
  `v0.0.2a` package. The split mirrors `djangorestframework` → `import
  rest_framework`.
- Crates: `rango-core`, `rango-py`
- Environment variables: `PYLON_*` → `RANGO_*`
- Control plane: `/_pylon/*` → `/_rango/*`
- Generated API keys: `pyl_…` → `rng_…`
- OpenAPI extensions: `x-pylon-*` → `x-rango-*`

**This is a breaking change for anyone running v0.1–0.2.** No migration path is
provided; nothing was published, so nothing depends on the old names.

### Security

Six issues found by an adversarial review and fixed. Full detail, including what
was *not* tested, in [`SECURITY.md`](SECURITY.md).

- **Critical** — `jsonwebtoken` panicked inside `decode` because no crypto
  provider feature was enabled. Reachable pre-auth by any client sending an
  `Authorization` header; severed the connection with no status line and broke
  every JWT-configured app.
- **High** — no panic boundary on the request path. Now wrapped in
  `catch_unwind`; a panic becomes a 500 for that request, logged, never returned.
- **High** — proxy path traversal usable as an SSRF primitive: a path parameter
  could climb out of the upstream prefix.
- **High** — unbounded behaviour recursion exhausted the worker pool. Each nested
  invocation got a fresh step budget, so `max_steps` never bounded the total.
  Bounded now by `server.max_invocation_depth` (default 8).
- **Medium** — Python tracebacks were returned to clients. Now log-only;
  `RANGO_DEBUG_ERRORS=1` opts back in.
- **Low** — client input faults surfaced as 500s. `QueryError` now separates
  caller fault (400) from internal fault (500).

Also: JWT expiry leeway was silently inheriting the library's 60-second grace.
Now explicit (`leeway_secs`, default 30) with `exp` required.

### Changed

- `requires-python` is now `>=3.12`. The previous `>=3.11` was never verified;
  3.12 and 3.14 (free-threaded and GIL) are tested.
- Incremental compilation disabled for the dev profile — artifacts had reached
  several GB across rebuild cycles.

### Added

- 54-test adversarial suite (`tests/test_pentest.py`) covering auth bypass,
  injection, traversal, SSRF, privilege escalation, exhaustion, and disclosure.
- `LICENSE` (Apache-2.0), matching the declared license.
- CI: test matrix and multi-platform wheel builds.

### Verified

251 tests (66 Rust, 185 Python). Clippy clean. Soak: 1,786,805 requests over
120 seconds, 0 errors, 0 panics, memory at steady state.

## [0.2.0] — unreleased

Production hardening and the frontend story: authentication (API keys + JWT) with
scope enforcement, CORS, rate limiting, security headers, request IDs, graceful
shutdown, server-rendered pages via minijinja, static file serving, the agent
runtime with approval gates and runtime-enforced budgets, an audit trail, and
TypeScript client generation.

## [0.1.0] — unreleased

Initial proof of the core idea: a Python declaration layer compiled to a manifest
and executed by a Rust runtime, where routes that are queries, proxies, or static
responses never enter the interpreter — and every route marked `tool=True` is
simultaneously a REST endpoint, an OpenAPI operation, and a live MCP tool.
