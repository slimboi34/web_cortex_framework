# Changelog

Notable changes per release. Versions follow [semantic versioning](https://semver.org);
while the major version is 0, minor bumps may contain breaking changes.

## [0.3.1] — 2026-08-06

Packaging and supported-interpreter corrections. No library behaviour changed.

### Removed

- **GIL-enabled CPython 3.14 is no longer supported**, and no `cp314` wheel is
  built or published. The extension cannot be imported on CPython 3.14.7:
  `ValueError: module functions cannot set METH_CLASS or METH_STATIC`. 3.12,
  3.13, 3.14.6 and the free-threaded 3.14 build are all unaffected, so this is
  neither a regression here nor a free-threading problem. Publishing a wheel
  that installs cleanly and then fails at import is worse than publishing none —
  the error arrives later, further from its cause. Use 3.13 or `3.14t`.

  0.3.0 still carries its `cp314` files; PyPI is immutable. The investigation,
  including what has been ruled out, is in AGENTS.md §11.

### Fixed

- The sdist now contains `LICENSE`. Declaring `license = { text = "Apache-2.0" }`
  makes maturin write `License-File: LICENSE` into the metadata, and PyPI
  enforces that claim — 0.3.0's sdist was rejected with
  `400 License-File LICENSE does not exist in distribution file` and that release
  shipped wheels only. Wheels were unaffected because maturin copies the file
  into `.dist-info` itself.
- The README no longer says the project is unpublished. It is the
  `long_description`, so that text was the PyPI landing page.

### Added

- `AGENTS.md`: a machine-facing reference for AI coding tools — the full API
  surface with exact signatures and defaults, the parameter-binding order, the
  scope and delegation model, and the failure modes that are cheap to hit and
  expensive to diagnose.
- CI reports the resolved interpreter and any `_core` import failure as workflow
  annotations. `3.14` is a moving target that `uv` re-resolves per run, so the
  same commit could pass and fail half an hour apart with no way to tell why.

## [0.3.0] — 2026-08-04

The first release intended for other people to install.

### Renamed

The project is now **WebCortex**. It was developed as *Pylon* and briefly carried
the name *Rango*; both are taken on PyPI.

- Import and CLI: `webcortex` (`from webcortex import WebCortex`, `webcortex dev`)
- Distribution on PyPI: **`web-cortex-framework`**. Unlike the earlier names,
  this one is not forced — both `web-cortex-framework` and `webcortex` are free.
  The longer distribution name is the project's chosen identity; the shorter
  import name is for ergonomics, the way `djangorestframework` imports as
  `rest_framework`.
- Crates: `webcortex-core`, `webcortex-py`
- Environment variables: `PYLON_*` → `WEBCORTEX_*`
- Control plane: `/_pylon/*` → `/_webcortex/*`
- Generated API keys: `pyl_…` → `wcx_…`
- OpenAPI extensions: `x-pylon-*` → `x-webcortex-*`

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
  `WEBCORTEX_DEBUG_ERRORS=1` opts back in.
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
