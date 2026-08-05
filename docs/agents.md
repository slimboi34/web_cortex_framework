# Agents

An agent is a model with tools, a budget, and an identity — declared next to the
routes it can call.

```python
app.agent(
    "assistant",
    model="claude-opus-5",
    description="Answers questions about tickets.",
    system="You help support staff triage tickets. Prefer read-only tools.",
    tools=["list_tickets", "get_tickets", "get_tickets_by_id_summary"],
    scopes=["read"],
    expose_scopes=["read"],
    max_steps=8,
    token_budget=50_000,
    expose_at="/ask",
)
```

## What makes this different from writing a tool loop

Four things, and they are all enforced by the runtime rather than by your
diligence.

### 1. Tools are the app's own routes

Dispatched in-process. Ten tool calls cost ten function calls, not ten loopback
HTTP round trips. No second schema, no drift.

A typo in `tools` is a **boot error**, with a suggestion:

```console
webcortex: agent "assistant" references tool "list_ticket", which is not an
exposed route. Did you mean "list_tickets"?
```

### 2. Authority is delegated, never granted

This is the most important line in the runtime:

```python
actor = caller.delegate_to_agent(name, declared_scopes)
```

Delegated scopes are the **intersection** of what the agent declares and what
the caller holds — never the union.

| Caller holds | Agent declares | Agent gets |
|---|---|---|
| `["read", "write"]` | `["read"]` | `["read"]` |
| `["read"]` | `["read", "write"]` | `["read"]` — not write |
| `[]` (anonymous) | `["admin"]` | `[]` — nothing |
| `["read", "write"]` | `[]` (unset) | `["read", "write"]` — inherits |

An agent is a delegate, never an escalation. This is the confused-deputy
defence, and it holds whether the run started over HTTP, over MCP, or from
another Behaviour.

!!! tip "`scopes` and `expose_scopes` are different questions"
    - `scopes` — what a run **may do**
    - `expose_scopes` — who may **start** a run

    They default to the same set, because an endpoint that spends tokens and
    exercises tools should not be less guarded than the tools themselves.
    Passing `expose_scopes=[]` makes the endpoint public, which
    `webcortex security` reports.

### 3. Budgets are enforced by the runtime

`max_steps` and `token_budget` are checked before each provider call. A looping
model costs a bounded amount rather than whatever the provider will sell you.

```json
{
  "status": "budget_exhausted",
  "usage": {"steps": 12, "tool_calls": 9,
            "input_tokens": 31402, "output_tokens": 18730}
}
```

Possible outcomes: `completed`, `step_limit`, `budget_exhausted`,
`awaiting_approval`, `failed`.

### 4. Everything is audited — including refusals

```console
$ curl localhost:8000/_webcortex/audit -H "x-api-key: $KEY"
```

```json
{"events": [
  {"kind": "agent_started", "run_id": "…", "actor": "agent:assistant#service",
   "detail": {"granted_scopes": ["read"], "tools": ["list_tickets", "..."]}},
  {"kind": "tool_called", "tool": "list_tickets",
   "detail": {"arguments": {"limit": 20}, "ok": true, "duration_ms": 3}},
  {"kind": "tool_refused", "tool": "delete_tickets",
   "detail": {"reason": "missing required scope(s): write"}}
]}
```

`granted_scopes` on the start event proves what the run *could* have done —
usually the fact you want during an incident. Refusals are recorded because
they are the interesting events.

## Approval gates

The difference between an agent you can point at production and a demo.

```python
@app.post("/tickets/purge", tool=True, scopes=["admin"], approval="required")
def purge(confirm: bool = False) -> dict:
    """Delete every closed ticket. Requires human approval."""
    ...
```

When an agent requests a gated tool, the runtime **suspends the run** and
returns `202` with an approval request. The tool does not execute.

```json
{
  "status": "awaiting_approval",
  "run_id": "9f3c…",
  "pending_approval": {
    "approval_id": "b71e…",
    "tool": "create_tickets_purge",
    "arguments": {"confirm": true},
    "reason": "tool is gated and requires human approval before it runs"
  }
}
```

The gate holds on every path:

- Directly over **MCP** — refused, because honouring it only inside the agent
  loop would leave an obvious way around
- Wrapped in a **Behaviour** — the Behaviour halts

An approval gate on a route that is not exposed as a tool is a **boot error**:
a gate that can never fire is dead configuration giving false confidence.

## Invoking an agent

=== "Over HTTP"

    ```console
    $ curl -X POST localhost:8000/ask -H "x-api-key: $KEY" \
        -H 'content-type: application/json' \
        -d '{"input":"Which tickets are urgent?"}'
    ```

=== "From a Behaviour"

    ```python
    @app.behaviour("escalate")
    def escalate(ctx, input):
        return ctx.run("assistant", input="Summarise open tickets")
    ```

## Configuration

| Argument | Default | Purpose |
|---|---|---|
| `model` | — | Provider model id |
| `system` | `""` | System prompt |
| `tools` | `()` | Exposed route tool names |
| `scopes` | `()` | What a run may do (intersected with caller) |
| `expose_scopes` | `= scopes` | Who may start a run |
| `max_steps` | `12` | Provider round trips |
| `token_budget` | `None` | Total tokens |
| `temperature` | `1.0` | |
| `max_tokens` | `4096` | Per response |
| `expose_at` | `None` | Mount a POST route |

## Providers

Anthropic is implemented. Set `ANTHROPIC_API_KEY`; optionally
`ANTHROPIC_BASE_URL` for a gateway.

An app with declared agents but no key still boots — agent routes return a clear
**503** rather than the process refusing to start, so a missing key in staging
does not take the whole service down.

!!! note "Deliberately not a universal LLM abstraction"
    Every provider's streaming and tool-call format differs and changes. A
    lowest-common-denominator interface rots quickly and hides the differences
    that matter. One provider implemented properly, behind a trait narrow enough
    that adding a second is contained work.

## Limits worth knowing

- **Runs are ephemeral.** A run does not survive a deploy. Durable, resumable
  runs mean building a small workflow engine with exactly-once tool execution;
  that is deliberately not in v0.3.
- **No streaming yet.** Responses are returned whole. SSE is the next milestone.
- **Approval resume is manual.** The runtime suspends and records the request;
  wiring "approve and continue" to a UI is application work today.

[Behaviours :material-arrow-right:](behaviours.md){ .md-button .md-button--primary }
