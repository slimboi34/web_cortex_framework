# Diagnostics benchmark: 2.4.0 vs 2.5.0

Measured 2026-10-15 on an Apple-silicon Mac, CPython 3.13. 2.4.0 is the wheel from PyPI
(`web-cortex-framework==2.4.0`) in a throwaway venv; 2.5.0 is this tree built with
`maturin develop --release`. Both run the same `scenario_app.py`.

Reproduce:

```console
$ python benchmarks/diagnostics/run.py --python /path/to/venv-2.4.0/bin/python --out results-2.4.0.json
$ python benchmarks/diagnostics/run.py --python .venv/bin/python --out results-2.5.0.json
```

No real model API is called: agents answer from the offline fake provider
(`WEBCORTEX_FAKE_PROVIDER=1`), or fail on a missing key or a closed local port. The raw
outputs are in `results-2.4.0.json` and `results-2.5.0.json`.

## What is scored

Eighteen realistic failures — sixteen against a running server (HTTP and MCP), two from the CLI
(`webcortex check`). For each, the failure's output is scored on:

- **a. code** — it names a stable error code (matches `WC\d{3}`).
- **b. hint** — it carries an explicit hint: a non-empty `hint` field anywhere in the JSON, or a
  CLI line starting `hint:`. Advice written into the prose of a message does not count here; see
  the secondary column below.
- **c. JSON** — the failure output parses as JSON.
- **d. status** — the failure is visible in the live status view: the `x-request-id` the
  benchmark sent, or the run id, appears in `GET /_webcortex/status`. Not applicable (–) to the
  CLI scenarios, which have no server.

## Results

| Scenario | 2.4.0 status | a | b | c | d | 2.5.0 status | a | b | c | d |
|---|---|---|---|---|---|---|---|---|---|---|
| missing provider key | 502 | ✗ | ✗ | ✓ | ✗ | 502 | ✓ | ✓ | ✓ | ✓ |
| provider unreachable | 502 | ✗ | ✗ | ✓ | ✗ | 502 | ✓ | ✓ | ✓ | ✓ |
| bad route | 404 | ✗ | ✗ | ✓ | ✗ | 404 | ✓ | ✓ | ✓ | ✓ |
| wrong method | 405 | ✗ | ✗ | ✓ | ✗ | 405 | ✓ | ✓ | ✓ | ✓ |
| budget exceeded | 200 | ✗ | ✗ | ✓ | ✗ | 200 | ✓ | ✓ | ✓ | ✓ |
| actuator called while halted | 423 | ✗ | ✗ | ✓ | ✗ | 423 | ✓ | ✓ | ✓ | ✓ |
| malformed agent request | 400 | ✗ | ✗ | ✓ | ✗ | 400 | ✓ | ✓ | ✓ | ✓ |
| tool argument missing (HTTP) | 422 | ✗ | ✗ | ✓ | ✗ | 422 | ✓ | ✓ | ✓ | ✓ |
| tool argument missing (inside an agent run) | 200 | ✗ | ✗ | ✓ | ✗ | 200 | ✓ | ✓ | ✓ | ✓ |
| unauthenticated admin call | 401 | ✗ | ✗ | ✓ | ✗ | 401 | ✓ | ✓ | ✓ | ✓ |
| authenticated but missing scope | 403 | ✗ | ✗ | ✓ | ✗ | 403 | ✓ | ✓ | ✓ | ✓ |
| cross-origin browser POST | 200 ¹ | ✗ | ✗ | ✗ | ✗ | 403 | ✓ | ✓ | ✓ | ✓ |
| unknown device ² | 404 | ✗ | ✗ | ✓ | ✗ | 404 | ✓ | ✓ | ✓ | ✓ |
| camera has sent no frame | 404 | ✗ | ✗ | ✓ | ✗ | 404 | ✓ | ✓ | ✓ | ✓ |
| Python handler raised | 500 | ✗ | ✗ | ✓ | ✗ | 500 | ✓ | ✓ | ✓ | ✓ |
| gated tool called over MCP | 200 | ✗ | ✗ | ✓ | ✗ | 200 | ✓ | ✓ | ✓ | ✓ |
| bad manifest (agent names a missing tool) | exit 1 | ✗ | ✗ | ✗ | – | exit 1 | ✓ | ✓ | ✓ | – |
| app module fails to import | exit 1 | ✗ | ✗ | ✗ | – | exit 1 | ✓ | ✓ | ✓ | – |

| | 2.4.0 | 2.5.0 |
|---|---:|---:|
| scenarios that failed as intended | 17/18 | 18/18 |
| a. stable code | 0/18 | **18/18** |
| b. explicit hint | 0/18 | **18/18** |
| c. machine-readable JSON | 15/18 | **18/18** |
| d. visible in the live status view | 0/16 | **16/16** |
| *secondary: message prose suggests a fix (keyword heuristic)* | *8/18* | *11/18* |

Excluding the cross-origin row (¹), 2.4.0 scores 0/17, 0/17, 15/17 and 0/15.

¹ 2.4.0 accepted the cross-origin POST: the refusal arrived in 2.4.1, which never reached PyPI
and ships in 2.5.0. Scored as no failure output, which is what a 2.4.0 user saw.

² Device routes are declared per device, so a request for an undeclared device is a route that
does not exist and is reported as `WC201` (no such route), not `WC501`.

What the 2.4.0 column means in practice: every server-side failure was already JSON, and about
half of the messages carried advice in their prose (`declare it with app.provider(...)`,
`try: GET`), but nothing a program could branch on except the HTTP status, nothing tying the
failure to a later look at the running app, and the two CLI failures were Python tracebacks.

The secondary column counts outputs whose text contains one of `set `, `export `, `declare`,
`use `, `add `, `release`, `list the origin`, `try:`, `requires`, `send `, `raise `. It is a
crude proxy, reported so that 2.4.0's in-prose advice gets credit; it is not a quality measure.

## Throughput: does the recorder cost anything?

Every request now updates per-route counters and a latency histogram (relaxed atomics), and every
failed request is pushed into a bounded ring under a mutex. ApacheBench, `-k -n 20000 -c 32`,
both servers up at once and measured alternately in the same session, after a 5,000-request
warm-up each, `WEBCORTEX_LOG=error`. Zero failed requests in every run. Requests per second:

Public Rust static route (`GET /fast`, 200):

| round | 2.4.0 | 2.5.0 |
|---|---:|---:|
| 1 | 174,303 | 168,468 |
| 2 | 173,306 | 173,304 |
| 3 | 174,741 | 175,254 |
| 4 | 176,075 | 171,630 |
| 5 | 175,600 | 176,121 |
| **median** | **174,741** | **173,304** |

A difference of −0.8%, inside the round-to-round spread. An earlier session of the same
comparison gave medians of 207,624 (2.4.0) and 204,847 (2.5.0); absolute numbers move with
machine load, and only the interleaved comparison means anything.

Every request a 404 (`GET /no/such/route`), the path that takes the error-ring lock:

| round | 2.4.0 | 2.5.0 |
|---|---:|---:|
| 1 | 139,596 | 137,182 |
| 2 | 171,923 | 166,466 |
| 3 | 173,622 | 166,297 |
| 4 | 173,316 | 166,639 |
| 5 | 171,846 | 165,949 |
| **median** | **171,846** | **166,297** |

About 3% slower when *every* request fails — consistently, in four of five rounds. The error
body is also larger (the hint adds ~150 bytes) and carries one more header. A first version of
the ring that stored a `serde_json::Value` per failure measured ~13–16% slower on this path; the
ring now stores plain fields, interns catalogued codes and frees evicted records outside the
lock, and the JSON is built only when `/status` is read.

No speedup is claimed for 2.5.0.
