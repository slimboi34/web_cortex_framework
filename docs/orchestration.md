# Orchestration

One agent is a tool loop. Several agents are a system, and a system needs
answers to questions a loop never asks: who is in control, what may it do, how
much may the whole thing cost, and what happens when a human has to decide.

WebCortex answers them with four primitives, all executed by the runtime.

| Primitive | Shape | Declared with |
|---|---|---|
| **Agents as tools** | supervisor calls workers | `app.agent("boss", tools=["worker"])` |
| **Handoffs** | the conversation moves to a specialist | `app.agent("desk", handoffs=["billing"])` |
| **Flows** | pipeline · parallel · route, as data | `app.flow(...)` |
| **Sessions** | a conversation that continues | `{"input": ..., "session_id": ...}` |

And two guarantees that hold across all of them: authority only ever
**shrinks** as work moves between agents, and one **shared budget** bounds the
whole tree.

---

## Every agent is a tool

An agent is mounted at `/agents/<name>` (or `expose_at`) and exposed under its
own name, so another agent can list it in `tools=[...]`. That is the entire
supervisor/worker pattern:

```python
app.agent("researcher", description="Finds facts in the catalogue.",
          tools=["list_books", "books_by_author"], scopes=["read"])

app.agent("writer", description="Turns notes into a blurb.",
          tools=[], scopes=[])

app.agent(
    "editor",
    description="Produces a finished blurb for any request.",
    system="Use the researcher for facts and the writer for prose. Never invent titles.",
    tools=["researcher", "writer"],
    scopes=["read"],
    token_budget=120_000,
    expose_at="/blurb",
)
```

The editor calls `researcher` the way it calls `list_books`: an in-process
function call through the same dispatcher, under a principal delegated from the
editor's own, one nesting level deeper. The worker's run result comes back as
the tool result, and its tokens count against the editor's budget.

A worker's endpoint takes `{"input": "…"}`, so a supervisor's tool call is the
same shape a human's request would be. If you do not want workers reachable
over HTTP at all, give them `expose_scopes=["internal"]` and grant that scope
to nobody — the supervisor still reaches them, because a delegated agent
inherits the *caller's* scopes, and the route is guarded by *who may start*
it, which for an in-process call is the supervisor itself. Or simply mark
them `tool=True` and leave the HTTP guard as tight as the rest of the app.

---

## Handoffs

A supervisor asks a worker a question and gets an answer. A handoff is
different: the **conversation itself** moves to another agent, which continues
it with its own system prompt, tools and context.

```python
app.agent("billing",   description="Invoices, refunds, payments.",
          tools=["list_invoices", "create_refunds"], scopes=["billing"])
app.agent("technical", description="Bugs, outages, how-to.",
          tools=["list_tickets", "update_tickets"], scopes=["support"])

app.agent(
    "front_desk",
    system="Find out what the customer needs. Hand off when it is clearly for a specialist.",
    handoffs=["billing", "technical"],
    scopes=["billing", "support"],
    token_budget=100_000,
    expose_at="/ask",
)
```

Each handoff target becomes a tool named `transfer_to_<name>`, described with
the target's `description`, and the front desk's system prompt gains one line
telling the model to use them. When the model calls one:

1. The active agent becomes the target. Its system prompt, tools and context
   providers apply from the next step.
2. The conversation carries over intact — the customer does not repeat
   themselves.
3. The budget carries over. `max_steps` and `token_budget` belong to the run,
   which belongs to the agent that *started* it.
4. Authority is re-derived from the original caller, **then filtered by what
   the previous agent held**. A chain of handoffs can only lose scopes.

The result records the path:

```json
{"agent": "billing", "path": ["front_desk", "billing"],
 "usage": {"handoffs": 1, "steps": 4, "...": "..."}}
```

and the audit trail has a `handoff` event with the reason the model gave.

!!! note "Handoff or tool?"
    Use a **tool** (list the agent in `tools=`) when the caller needs an
    answer and stays in charge. Use a **handoff** when the specialist should
    take over the conversation and answer the human directly. A front desk
    hands off; an editor consults.

---

## Flows: orchestration as data

When the arrangement is known in advance, it should not be a prompt and it
should not be a Python loop either. It should be data the runtime executes.

```python
# Steps in order; each receives the previous output.
app.flow("briefing", pipeline=["researcher", "writer"], token_budget=150_000)

# Every branch gets the same input, concurrently.
app.flow("review", parallel=["security_review", "style_review"], merge="collect")

# A cheap model picks one branch.
app.flow(
    "desk",
    route={"billing": "billing", "technical": "technical"},
    default="front_desk",
    classify_with="fast",
)
```

Every step is a **tool** — an agent, a behaviour, another flow, or any route
marked `tool=True` — so composition is uniform. A flow is itself a tool and a
route (`/flows/<name>`), so flows nest and agents can invoke them.

### What a step receives

| Target | Without a mapping, the step gets |
|---|---|
| an agent | `{"input": <previous output>}` |
| anything else | the previous output as its arguments (an object is passed as-is; a scalar is wrapped as `{"input": …}`) |

Agent and flow results are envelopes; the next step gets their `output`, not
the whole record.

For anything else, map explicitly:

```python
app.flow("refund", pipeline=[
    {"tool": "get_orders",   "input": {"id": "$input.order_id"}},
    {"tool": "assess",       "input": {"order": "$", "reason": "$input.reason"}},
    {"tool": "create_refunds", "input": {"order_id": "$.order_id", "amount": "$.amount"}},
])
```

`$` is the incoming value, `$.a.b` a path into it, `$input` the flow's original
input, `$input.x` a path into that. Anything else is a literal.

### Merging parallel branches

`merge="collect"` (the default) returns a list in declaration order.
`merge="merge"` folds object outputs into one object — later branches win on
collisions — and keeps non-object outputs under the branch's tool name.

### Routing

The router asks `classify_with` (default: the `fast` alias) to choose exactly
one label, forced through a structured tool call so the answer is a value, not
prose. `classify_prompt` may use `{labels}` and `{input}`; without `{input}`
the input is appended. A label with no route falls to `default`; with no
default, the flow fails and says which label was chosen.

### The result

```json
{
  "flow": "briefing", "run_id": "…", "status": "completed",
  "output": "Two paragraphs about the queue…",
  "steps": [{"tool": "researcher", "duration_ms": 2410, "ok": true},
            {"tool": "writer",     "duration_ms": 1820, "ok": true}],
  "usage": {"tree_tokens": 41930}
}
```

`status` is `budget_exhausted` when the shared budget ran out mid-way; the
output so far is still returned. A step that suspends on an approval gate
fails the flow with the approval id — a flow cannot wait for a human, so keep
gated tools out of flow steps or resume that run directly.

---

## Sessions

An agent endpoint accepts a `session_id`. With one, the runtime loads the
conversation it last saved under that id, appends the new input, runs, and
saves the result:

```console
$ curl -X POST localhost:8000/ask -H "x-api-key: $KEY" \
    -d '{"input": "Which tickets are urgent?", "session_id": "u1-2026-09-13"}'
$ curl -X POST localhost:8000/ask -H "x-api-key: $KEY" \
    -d '{"input": "Escalate the first one.", "session_id": "u1-2026-09-13"}'
```

The store key includes the **principal**, so two callers using the same id
never see each other's history. `"reset": true` discards it first.

Sessions live in memory — bounded (`session_capacity`, default 1000) and
expiring (`session_ttl_secs`, default 3600) — and do not survive a restart.
That is stated rather than hidden: durable conversation state is application
data, and it belongs in a table you own. Combine sessions with a
[context window](context.md#compaction) so a long conversation is compacted
rather than truncated.

---

## Approvals that resume

A tool marked `approval="required"` suspends the run that asks for it. In
v0.3 that was where the story ended; in v2 the run waits, and a human decides:

```console
$ curl localhost:8000/_webcortex/approvals -H "x-api-key: $ADMIN"
{"approvals": [{"approval_id": "b71e…", "agent": "assistant", "caller": "service",
                "tool": "create_tickets_purge", "arguments": {"confirm": true},
                "age_secs": 12}]}

$ curl -X POST localhost:8000/_webcortex/approvals/b71e… -H "x-api-key: $ADMIN" \
    -d '{"approve": true, "note": "confirmed with the customer"}'
```

The response is the continued run. Three things are true about it:

- **The rest of the turn runs.** If the model asked for three tools and the
  second was gated, the first ran before suspension and the third runs after
  approval. The model gets all three results, in order.
- **Denial is information.** `{"approve": false, "note": "…"}` sends the model
  a tool error saying a human declined, with the note, and the run continues
  — usually with the model explaining that it cannot do that.
- **A decision is consumed.** A second POST with the same id is a 404.

Suspended runs are held in memory for `approval_ttl_secs` (default 3600).
Deciding requires the `webcortex:admin` scope; the run itself resumes as the
delegated principal of whoever started it, not as the approver.

---

## Budgets compose

The security review of v0.3 found that per-frame limits do not bound a
request: a recursive behaviour got a fresh step budget at every level. The
fix then was a depth counter that travels with the request. v2 generalises it.

A **shared budget** is created when the outermost agent, behaviour or flow
starts, from its `token_budget`, and travels with every in-process call it
makes. Nested runs charge the same counter. When it is exhausted, whichever run
is executing stops with `budget_exhausted`, and the outer result reports
`usage.tree_tokens` — what the whole tree spent.

```python
app.agent("editor", tools=["researcher", "writer"], token_budget=100_000)
```

That line caps the editor *and* every researcher and writer run it starts.
Steps remain per-run, because steps are a shape limit; tokens are shared,
because tokens are what cost money.

[Context and memory :material-arrow-right:](context.md){ .md-button .md-button--primary }
