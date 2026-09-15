# Behaviours

The distinctive idea in WebCortex, and the one worth understanding properly.

## The problem with prompts as procedures

A "skill" written as a prompt is a **suggestion**. The model reads it and may
ignore it. Worse, `"if X then Y"` fails *silently* when it does — you get a
plausible-looking answer where step three never happened, and nothing in your
logs says so.

That is fine for open-ended work. It is not fine for a procedure you need
executed the same way every time.

## Inverting the control flow

A Behaviour makes the loops and branches **real Python that always runs**. Only
the leaves are probabilistic.

```python
@app.behaviour("triage", tools=["list_tickets", "update_tickets"])
def triage(ctx, input):
    tickets = ctx.call("list_tickets", status="open")   # deterministic

    urgent = 0
    for ticket in tickets:                               # a real loop
        verdict = ctx.ask(                               # a model call
            f"Classify this ticket: {ticket['body']}",
            schema={
                "type": "object",
                "properties": {
                    "category": {"enum": ["bug", "billing", "other"]},
                    "urgency": {"enum": ["low", "high"]},
                },
                "required": ["category", "urgency"],
            },
        )
        if verdict["urgency"] == "high":                 # a real branch
            ctx.call("update_tickets", id=ticket["id"], priority=1)
            urgent += 1

    return {"reviewed": len(tickets), "escalated": urgent}
```

Compare the failure modes:

| | Prompt-as-procedure | Behaviour |
|---|---|---|
| Loop over every ticket | Model *should* | `for` — guaranteed |
| Branch on urgency | Model *should* | `if` — guaranteed |
| Classification | Model judgment | Model judgment, schema-validated |
| Skipped a step | Silent | Impossible |
| Reproducible | No | Control flow, yes |

The model does what models are good at — judgment on unstructured text. Python
does what code is good at — iterating, branching, and never forgetting.

## The context object

The first parameter is a `ctx` handle; the second is the input payload.

### `ctx.call(tool, **kwargs)`

Invoke an exposed route, in-process, under the run's delegated authority.

```python
tickets = ctx.call("list_tickets", status="open", limit=50)
```

Scope enforcement is the same code that guards the HTTP path. A Behaviour cannot
reach a route its caller could not.

### `ctx.ask(prompt, *, schema=None, system=None, model=None, max_tokens=None, temperature=None)`

A model call. With `schema`, the result is validated JSON — so the surrounding
Python can branch on it safely.

```python
verdict = ctx.ask(
    f"Is this refund request legitimate?\n\n{request}",
    schema={"type": "object",
            "properties": {"legitimate": {"type": "boolean"},
                           "reason": {"type": "string"}},
            "required": ["legitimate", "reason"]},
)
if verdict["legitimate"]:
    ...
```

Without `schema` you get text.

### `ctx.gather(*items, return_exceptions=False)`

Several tool calls at once. Each item is `(tool, kwargs)`, a bare tool name,
or `{"tool": name, **kwargs}`. The calls run concurrently on the Rust runtime,
so a loop of fifty `ctx.call`s becomes one wait, and every one is admitted,
scoped, gated and charged exactly as `call` is — one step per item.

```python
rows = ctx.gather(("get_orders", {"id": 1}), ("get_orders", {"id": 2}), "ping")
```

By default the first failure is raised after every call has finished. With
`return_exceptions=True`, failures come back in place as
`{"error": "...", "ok": False}`.

### `ctx.ask_many(prompts, *, schema=None, model=None, concurrency=8, ...)`

The classification loop collapsed into one wait: the prompts are sent
concurrently, at most `concurrency` in flight, and the answers come back in
order. Same tokens, a fraction of the wall-clock.

```python
verdicts = ctx.ask_many(
    [f"Classify: {t['body']}" for t in tickets],
    schema={"type": "object", "properties": {"urgent": {"type": "boolean"}}},
    model="fast",
)
```

### `ctx.context(name)`

Resolve a declared [context provider](context.md#context-providers) and return
its raw value. Only providers named in the behaviour's `context=[...]` are
reachable.

### `ctx.run(name, **kwargs)`

Run another Behaviour or agent, composing procedures. Nesting is bounded by
`max_invocation_depth` (default 8).

### `ctx.log(message)`

Adds to the run trace, returned with the result and written to the audit log.

### `ctx.halt(reason)`

Stop deliberately. Returns `{"halted": true, "reason": ...}` rather than raising
an error — a Behaviour that decides not to proceed is a *successful* outcome.

```python
if not tickets:
    ctx.halt("no open tickets to triage")
```

### Read-only members

| Member | Type | Meaning |
|---|---|---|
| `ctx.tools` | `list[str]` | Tools available to this run |
| `ctx.contexts` | `list[str]` | Context providers available to this run |
| `ctx.user` | `dict` | `{id, root_id, authenticated, scopes}` — the delegated principal and the human behind it |
| `ctx.usage` | `dict` | Steps and tokens for this run, plus `tree_tokens` for everything the request tree has spent |
| `ctx.run_id` | `str` | Correlates trace and audit entries |
| `ctx.depth` | `int` | Nesting depth; `0` when started over HTTP |

## Budgets

Every `ctx.ask` charges a step and its tokens against the run's budget. Exceeding
either halts the run — it does not raise.

```python
@app.behaviour("triage", tools=[...], max_steps=50, token_budget=200_000)
```

The step charge happens *before* the provider call, so a runaway loop stops at
the boundary rather than after paying for it. Tokens are also charged to the
request tree's shared budget, so a behaviour launched by an agent cannot
outspend the agent — see [Budgets compose](orchestration.md#budgets-compose).

Pick the model per leaf. `model="fast"` on the behaviour, or on an individual
`ctx.ask`, is the cheapest single change you can make to an agentic app:

```python
@app.behaviour("triage", tools=[...], model="fast")
```


## Exposing a Behaviour

By default a Behaviour is itself an agent tool (`tool=True`), so agents can
invoke your procedures. Mount it over HTTP with `expose_at`:

```python
@app.behaviour(
    "triage",
    tools=["list_tickets", "update_tickets"],
    scopes=["read", "write"],
    expose_at="/behaviours/triage",
    expose_scopes=["admin"],
    max_steps=50,
)
def triage(ctx, input): ...
```

Note the split again: `scopes` is what the procedure may do, `expose_scopes` is
who may start it.

## Worked example: refund processing

A procedure with a hard policy rule that must never be delegated to a model:

```python
@app.behaviour(
    "process_refund",
    tools=["get_orders", "create_refunds", "notify_customer"],
    scopes=["orders:read", "refunds:write"],
    max_steps=20,
)
def process_refund(ctx, input):
    order = ctx.call("get_orders", id=input["order_id"])

    # A hard rule. Not a suggestion in a prompt.
    if order["total"] > 500_00:
        ctx.log("above auto-approval threshold; escalating")
        return {"status": "escalated", "reason": "over $500"}

    assessment = ctx.ask(
        f"Assess this refund request:\n\nOrder: {order}\nReason: {input['reason']}",
        schema={"type": "object",
                "properties": {"legitimate": {"type": "boolean"},
                               "confidence": {"type": "number"},
                               "reason": {"type": "string"}},
                "required": ["legitimate", "confidence", "reason"]},
    )

    # Another hard rule: low confidence never auto-approves.
    if not assessment["legitimate"] or assessment["confidence"] < 0.8:
        return {"status": "manual_review", "assessment": assessment}

    refund = ctx.call("create_refunds", order_id=order["id"], amount=order["total"])
    ctx.call("notify_customer", order_id=order["id"], refund_id=refund["id"])
    return {"status": "refunded", "refund_id": refund["id"]}
```

The $500 threshold and the confidence floor are **code**. No amount of prompt
injection in `input["reason"]` moves them, because the model never sees them as
instructions it could follow or ignore — they are branches evaluated after it
returns.

## The trace

Every run returns a step-by-step trace:

```json
{
  "run_id": "3f2a…",
  "result": {"reviewed": 12, "escalated": 3},
  "trace": [
    {"kind": "call", "label": "list_tickets", "duration_ms": 4},
    {"kind": "ask", "label": "classify", "duration_ms": 812},
    {"kind": "call", "label": "update_tickets", "duration_ms": 3},
    {"kind": "log", "label": "escalated ticket 41"}
  ],
  "usage": {"steps": 13, "input_tokens": 8420, "output_tokens": 1203}
}
```

Because control flow is code, a trace that skipped a step means a *bug*, not a
model that felt differently that day.

## When to use which

| Use | When |
|---|---|
| **Route** | Deterministic. No model involved. |
| **Behaviour** | Known procedure with model judgment at specific points. |
| **Flow** | Known *arrangement* of agents and behaviours — a pipeline, a fan-out, a router — with no logic between the steps. |
| **Agent** | Open-ended. The sequence of steps is not known in advance. |


Reach for a Behaviour whenever you catch yourself writing "first do X, then for
each Y, if Z…" into a system prompt. That sentence is a program; write it as one.

[Security :material-arrow-right:](security.md){ .md-button .md-button--primary }
