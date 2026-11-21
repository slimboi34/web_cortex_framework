# Evals and cassettes

*New in 2.8.*

An agent application has two halves. The model's choices are the probabilistic half.
Everything around them is code and declaration: which tools exist, what scopes a delegate
holds, how a handoff narrows authority, where a budget stops a tree, what a flow feeds the
next step. That half is yours, it can regress like any other code, and it can be tested the
same way.

An **eval** is one call to a tool, and a set of statements about the result:

```python
app.eval(
    "refund_goes_to_billing",
    target="front_desk",
    input="I was charged twice this month",
    scopes=["support"],
    expect={
        "path": ["front_desk", "billing"],          # which agents the run passed through
        "tools_called": ["lookup_invoice"],         # in this order, anywhere in the tree
        "tools_not_called": ["issue_refund"],       # the gated one stayed untouched
        "max_tokens": 6000,                         # the whole request tree
    },
)
```

```text
$ webcortex eval
  ok   refund_goes_to_billing           replay → front_desk  1830 tok  4 model call(s)  12ms
  ok   triage_is_typed                  replay → triage  402 tok  1 model call(s)  3ms
  2/2 passed, 2 replayed from cassettes, 2232 tokens
```

`webcortex eval` exits 1 when any eval fails. It runs in-process: the app is built exactly as
`webcortex run` builds it, and the target is called through the same dispatcher an HTTP
request uses, but no port is bound.

## Cassettes: record once, replay for free

The first time, run the evals against the real model and keep its answers:

```bash
webcortex eval --record        # writes evals/<name>.json for each eval
git add evals/
```

From then on `webcortex eval` **replays** those answers. No network, no API key, no tokens
spent, and the same result on every run, so the evals can run in CI on every commit.

What is replayed is only the model. Your tools run for real, through the real dispatcher: the
SQL executes, the Python handlers run, scopes are checked, approval gates suspend, budgets are
charged (with the token counts that were recorded) and flows pass values along. If a refactor
breaks a tool's schema, drops a scope, or reorders a pipeline, the replay fails.

### A cassette is tied to the agent it was recorded for

Each recorded response carries a **fingerprint** of the request's stable parts: the agent's
name, system prompt and model name, the concrete model that name resolved to, its context
providers (their names and declarations, not what they returned), its tools (names,
descriptions and input schemas), a forced tool, `max_tokens`, `temperature`, `reasoning`, and
its [request options](request-options.md), including those set for the whole provider with
`app.request_options(...)`. On replay the fingerprint of the call being made is compared with
the recorded one. If they differ, the recorded answer is an answer to a
different question, and the run fails with `WC319` instead of pretending:

```text
  FAIL refund_goes_to_billing           replay → front_desk  0 tok  0 model call(s)  1ms
         - WC319: the cassette is stale for "front_desk": its system prompt, model, tools or
           options changed since it was recorded (fingerprint 3fa1… then, 9c20… now); if the
           change is intended, record again with `webcortex eval --record`
```

So a prompt change cannot slip through on old recordings, and neither can re-pointing an
alias (`app.models(fast=...)`) or adding a context provider. Record again, and read the diff of
the cassette in review: it is a plain JSON file, and that diff is what your prompt change did
to the model's behaviour.

!!! note "Cassettes recorded with 2.8.0 are stale once"
    2.9 added the resolved model, the context providers and provider-level options to the
    fingerprint, so every cassette recorded with 2.8.0 reports `WC319` on its first replay.
    Run `webcortex eval --record` once after upgrading and commit the result.

The conversation itself is not part of the fingerprint. Tool results carry run ids and timings
that differ on every run, so matching on them would make every replay miss. Responses are
replayed per agent, in the order they were recorded. Two consequences:

- A run that makes **more** model calls than were recorded fails with `WC319` (nothing left to
  replay). One that makes **fewer** fails with a message naming the unused responses. Either
  way the run's shape changed.
- A tool that returns something different does not by itself fail a replay, because the model's
  recorded next move is replayed regardless. Use `expect` to pin down what matters about tool
  results (`data`, `output_contains`).

### Modes

| | |
| --- | --- |
| `webcortex eval` | **auto**: replay when the eval has a cassette, otherwise call the providers |
| `webcortex eval --replay` | replay only; an eval with no cassette fails (`WC320`). **Use this in CI**, so a missing recording cannot pass as a live run |
| `webcortex eval --record` | call the providers and write every cassette |
| `webcortex eval --live` | call the providers, ignore cassettes; a smoke test against the real model |

Also: `--only NAME` (repeatable), `--dir DIR` (default `evals`), `--list`, `--json`.

A cassette that cannot be read or written (unreadable, a directory, not valid JSON) is
`WC320` with its path, never a traceback. The other ways a run can fail before it starts, such
as an `--only` name no eval has, are coded too: the command exits 1, and with `--json` prints
`{"ok": false, "error": {...}}` on stdout.

Under `WEBCORTEX_FAKE_PROVIDER=1` "the providers" is the deterministic fake, so evals written
in its command language (`tool:<name> {…}`, `handoff:<agent>`) need no cassette at all.

## What `expect` can say

Every key is optional. An unknown key is a `ValueError` when the eval is declared, so a typo
cannot quietly turn a check into nothing.

| key | passes when |
| --- | --- |
| `status` | the run's status equals it. Without `status`, `http_status` or `code`, a run must be `completed` and a plain route must answer below 400 |
| `http_status` | the route answered with this HTTP status |
| `code` | the result's [error code](error-codes.md) equals it, e.g. `"WC402"` for a budget test |
| `output_contains` | the output contains the string, or every string of a list |
| `output_not_contains` | it contains none of them |
| `output_matches` | the regular expression is found in the output |
| `data` | the [structured answer](structured-output.md) (or a behaviour's or route's JSON result) contains this subset: nested objects are matched key by key, everything else by equality, where `true` does not equal `1` (but `1` equals `1.0`) |
| `tools_called` | these tools ran, in this order, anywhere in the request tree (other calls may come between) |
| `tools_not_called` | none of these ran |
| `path` | the agents the run passed through are exactly these |
| `max_steps` | the run took at most this many steps |
| `max_tokens` | the whole request tree spent at most this many tokens |
| `max_model_calls` | it made at most this many model calls |

`tools_called` is read from the run's [trace](tracing.md), so it sees tools called by a nested
agent, a flow step or a behaviour, not only the outermost agent's. A call the runtime refused
does not count as called: a tool the agent was not given (`WC307`), a malformed call
(`WC317`), a missing scope (`WC602`) or an actuator refused by the emergency stop (`WC502`).

`tools_called`, `tools_not_called` and `max_model_calls` need the trace, so they **fail** when
there is none to read: when tracing is off (`app.tracing(enabled=False)`), or when the trace
was truncated at `max_spans` and dropped spans (the failure says how many). Before 2.9 they
fell back to the outermost agent's own steps, and `tools_not_called` could pass on a run that
had called the tool from a nested agent.

Each value is type-checked when the eval is declared: `max_steps`, `max_tokens`,
`max_model_calls` and `http_status` must be integers (not `True`), `status` and `code`
strings, and `tools_called`, `tools_not_called` and `path` a string or a list of strings.
Anything else is a `ValueError`, like an unknown key.

For anything else, pass a function. It receives the result body; raising, returning `False`
or returning a non-empty string (the reason) fails the eval, and anything else (`None`,
`True`, an empty string) passes it. An exception raised by `check` fails that eval only: the
others still run, and `--json` still prints the report:

```python
def cites_an_invoice(result):
    if "INV-" not in result["output"]:
        return "the answer cites no invoice number"

app.eval("cites", target="billing", input="what did I pay in May?", check=cites_an_invoice)
```

There is no model-graded check, on purpose. A grader is another probabilistic step with its
own cost and its own failures; if you want one, call it from `check`.

## Token budgets as tests

`max_tokens` and `max_model_calls` turn the token economy into something CI enforces. A
replayed run reports the token counts that were recorded, so:

- adding a context provider, a longer system prompt or another tool makes the cassette stale;
- re-recording shows the new spend, and `max_tokens` fails if it grew past what you allowed.

The eval report's `tokens` is every model call in the request tree, whatever made it.

## What an eval can target

Any tool: an agent (`input` may be a plain string, shorthand for `{"input": "…"}`), a flow, a
behaviour, a query, a Python route. The caller is a principal named `eval` holding exactly the
`scopes` the eval lists, so authorization is part of what is tested:

```python
app.eval("readers_cannot_delete", target="delete_notes", input={"id": 1},
         scopes=["read"], expect={"http_status": 403, "code": "WC602"})
```

## Isolation and determinism

Each eval gets a freshly built app, so sessions, approvals, the ledger and the trace store
start empty, and so does an in-memory database. A file database is shared between evals and
between runs; point evals at a scratch one:

```bash
WEBCORTEX_DATABASE_URL="sqlite://:memory:" webcortex eval --replay
```

Two things can make a replay differ from its recording, and both are worth knowing:

- **Concurrent model calls by one name.** `ctx.ask_many` in a behaviour sends several prompts
  at once under the behaviour's name. They are recorded and replayed in call order, which is
  stable, but two *different* behaviours or agents racing each other are matched by name, not
  by time.
- **An eval that waits for a human.** A run that suspends on an approval gate ends as
  `awaiting_approval`; assert that (`expect={"status": "awaiting_approval"}`). Resuming is not
  part of an eval.

## From Python

```python
report = app.run_evals(mode="replay")
assert report["ok"], [r["failures"] for r in report["results"] if not r["ok"]]
```

`run_evals(mode=, directory=, only=)` returns `{"ok", "total", "passed", "failed", "tokens",
"mode", "directory", "results"}`; each result has `name`, `target`, `mode` (the one actually
used), `ok`, `failures`, `status`, `code`, `tokens`, `model_calls`, `duration_ms`, `trace_id`,
`cassette` and `result` (the response body). That makes a pytest wrapper three lines:

```python
from api import app

def test_evals():
    report = app.run_evals(mode="replay")
    assert report["ok"], {r["name"]: r["failures"] for r in report["results"] if not r["ok"]}
```

## The cassette file

```json
{
  "version": 1,
  "eval": "refund_goes_to_billing",
  "target": "front_desk",
  "recorded_with": "webcortex 2.8.0",
  "calls": [
    {"agent": "front_desk", "model": "default", "fingerprint": "3fa1c0de5b7a9e21",
     "response": {"text": "", "stop_reason": "tool_use", "tool_calls": [ … ],
                  "input_tokens": 412, "output_tokens": 38, "model": "claude-opus-5", "raw_content": [ … ]}}
  ]
}
```

It holds what the model said, so treat it like any other fixture: it will contain whatever the
model wrote about your eval's input. It never holds the prompt, the tool results or a
credential. Evals are not part of the manifest and nothing about them reaches a running
server.
