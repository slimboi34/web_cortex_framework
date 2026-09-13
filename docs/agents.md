# Agents

An agent is a model with tools, a budget, and an identity — declared next to the
routes it can call.

```python
app.agent(
    "assistant",
    description="Answers questions about tickets.",
    system="You help support staff triage tickets. Prefer read-only tools.",
    tools=["list_tickets", "get_tickets", "get_tickets_by_id_summary"],
    context=["policy"],
    memory="notes",
    scopes=["read"],
    expose_scopes=["read"],
    max_steps=8,
    token_budget=50_000,
    expose_at="/ask",
)
```

`model` defaults to the `default` alias — see [Models](models.md) — so an agent
need not name a model at all.

## What makes this different from writing a tool loop

Four things, and they are all enforced by the runtime rather than by your
diligence.

### 1. Tools are the app's own routes

Dispatched in-process. Ten tool calls cost ten function calls, not ten loopback
HTTP round trips. No second schema, no drift. Other agents, behaviours and flows
are routes too, so they are tools too — see [Orchestration](orchestration.md).

A typo in `tools`, `handoffs` or `context` is a **boot error**, with a
suggestion:

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
defence, and it holds whether the run started over HTTP, over MCP, from a
Behaviour, from a supervisor agent, or through a handoff — which intersects
once more with what the previous agent held.

!!! tip "`scopes` and `expose_scopes` are different questions"
    - `scopes` — what a run **may do**
    - `expose_scopes` — who may **start** a run

    They default to the same set, because an endpoint that spends tokens and
    exercises tools should not be less guarded than the tools themselves.
    Passing `expose_scopes=[]` makes the endpoint public, which
    `webcortex security` reports.

### 3. Budgets are enforced by the runtime — and they compose

`max_steps` and `token_budget` are checked before each provider call. A looping
model costs a bounded amount rather than whatever the provider will sell you.
The budget of the outermost run is shared by every agent, behaviour and flow it
calls; see [Budgets compose](orchestration.md#budgets-compose).

```json
{
  "status": "budget_exhausted",
  "usage": {"steps": 12, "tool_calls": 9,
            "input_tokens": 31402, "cache_read_tokens": 88100, "output_tokens": 18730,
            "handoffs": 0, "compactions": 1, "tree_tokens": 161240}
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
   "detail": {"reason": "missing required scope(s): write"}},
  {"kind": "handoff", "tool": "transfer_to_billing",
   "detail": {"from": "front_desk", "to": "billing", "reason": "invoice question"}},
  {"kind": "approval_decided", "tool": "create_tickets_purge",
   "detail": {"approval_id": "…", "approved": false, "note": "not today"}}
]}
```

`granted_scopes` on the start event proves what the run *could* have done —
usually the fact you want during an incident. Refusals are recorded because
they are the interesting events.

## The result

```json
{
  "run_id": "9f3c…",
  "agent": "assistant",
  "path": ["assistant"],
  "status": "completed",
  "output": "Three tickets are urgent: #41, #47 and #52.",
  "session_id": "u1-2026-09-13",
  "steps": [
    {"index": 0, "kind": "model", "result": {"model": "claude-opus-5", "input_tokens": 2104, "output_tokens": 88}, "duration_ms": 1410},
    {"index": 1, "kind": "tool_call", "tool": "list_tickets", "arguments": {"limit": 50}, "result": [...], "duration_ms": 3},
    {"index": 1, "kind": "model", "result": {...}, "duration_ms": 980}
  ],
  "usage": {"steps": 2, "tool_calls": 1, "input_tokens": 4300, "output_tokens": 210, "tree_tokens": 4510, "...": "..."}
}
```

`path` lists every agent the run passed through; `agent` is the one that
answered. Step kinds are `model`, `tool_call`, `tool_refused`,
`approval_requested`, `approval_denied`, `handoff` and `compaction`.

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

A human decides at `POST /_webcortex/approvals/b71e…` with
`{"approve": true|false, "note": "…"}`, and the run continues — including the
rest of the turn it was interrupted in. See
[Approvals that resume](orchestration.md#approvals-that-resume).

The gate holds on every path:

- Directly over **MCP** — refused, because honouring it only inside the agent
  loop would leave an obvious way around
- Wrapped in a **Behaviour**, alone or inside `ctx.gather` — the Behaviour
  halts
- As a **flow step** — the flow fails and reports the approval id

An approval gate on a route that is not exposed as a tool is a **boot error**:
a gate that can never fire is dead configuration giving false confidence.

## Invoking an agent

=== "Over HTTP"

    ```console
    $ curl -X POST localhost:8000/ask -H "x-api-key: $KEY" \
        -H 'content-type: application/json' \
        -d '{"input":"Which tickets are urgent?", "session_id": "u1"}'
    ```

    `session_id` is optional; with it, the conversation continues next time.
    `"reset": true` starts it over.

=== "From a Behaviour"

    ```python
    @app.behaviour("escalate", tools=["assistant"])
    def escalate(ctx, input):
        return ctx.call("assistant", input="Summarise open tickets")
    ```

=== "From another agent"

    ```python
    app.agent("supervisor", tools=["assistant", "list_tickets"], ...)
    ```

=== "From a flow"

    ```python
    app.flow("daily", pipeline=["assistant", "writer"])
    ```

## Configuration

| Argument | Default | Purpose |
|---|---|---|
| `model` | `"default"` | A model name or an alias from `app.models` |
| `system` | `""` | System prompt |
| `tools` | `()` | Exposed route tool names — routes, agents, behaviours, flows |
| `handoffs` | `()` | Agents this one may transfer the conversation to |
| `context` | `()` | Context providers resolved at run start |
| `memory` | `None` | A store from `app.memory`; adds its tools and a hint |
| `scopes` | `()` | What a run may do (intersected with caller) |
| `expose_scopes` | `= scopes` | Who may start a run |
| `max_steps` | `12` | Provider round trips |
| `token_budget` | `None` | Total tokens for the run and everything it calls |
| `cache` | `True` | Prompt caching of system prompt and tools |
| `tool_result_limit` | `16384` | Bytes of any tool result the model sees |
| `context_window` | `None` | Measured input tokens beyond which older turns are compacted |
| `compact_with` | `"fast"` | Model used for compaction |
| `keep_recent` | `6` | Messages kept verbatim by a compaction |
| `temperature` | `1.0` | |
| `max_tokens` | `4096` | Per response |
| `expose_at` | `/agents/<name>` | The POST route |
| `tool` | `True` | Expose under the agent's own name |

## Providers

Anthropic (Messages API) and any OpenAI-compatible endpoint (Ollama, vLLM,
LM Studio, OpenAI, Groq, OpenRouter), selected by model name prefix. Set
`ANTHROPIC_API_KEY` and/or `OPENAI_API_KEY`; `ollama/<model>` needs no key.
[Models](models.md) has the details.

An app with declared agents but no key still boots; a run that needs a missing
key fails with a message naming the variable, and everything else keeps
serving.

!!! note "Deliberately not a universal LLM abstraction"
    Two wire formats, each done properly, behind a trait narrow enough that
    the runtime never knows which one it is talking to. Conversations are kept
    in one canonical shape, so a session can move between providers.

## Limits worth knowing

- **Runs are ephemeral.** Sessions and suspended approvals live in memory,
  bounded and expiring, and do not survive a restart. Durable, resumable runs
  mean building a small workflow engine with exactly-once tool execution;
  that is deliberately not in v2.
- **Responses are returned whole.** Token-level SSE streaming is the next
  invasive change and is not claimed.

[Orchestration :material-arrow-right:](orchestration.md){ .md-button .md-button--primary }
