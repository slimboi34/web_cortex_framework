# Context and memory

An agent's first step is only as good as what it knows when it takes it, and
its twentieth step is only affordable if the first nineteen did not fill the
window. Context is where both accuracy and cost are decided, so WebCortex makes
it something you **declare and bound** rather than something that accretes.

Three mechanisms:

| | What it does | Declared with |
|---|---|---|
| **Context providers** | facts injected at run start | `app.context(...)` |
| **Memory** | a durable, per-caller scratchpad | `app.memory(...)` |
| **Context policy** | caps on what a run carries | `app.agent(..., context_window=, tool_result_limit=)` |

---

## Context providers

A provider is a named source resolved when a run starts and handed to the
model as a delimited block in the system prompt. Declared once, reused by
anything that names it, capped in size because it is re-sent on every step.

```python
# A constant.
app.context("policy",
            data={"refund_window_days": 30, "auto_refund_limit_usd": 500},
            description="Support policy the agents must follow.")

# A query, executed in Rust. May bind @principal and any key of the run's input.
app.context("open_queue",
            sql="SELECT id, customer, kind FROM tickets WHERE state='open' LIMIT 20",
            max_chars=3000)

app.context("my_orders",
            sql="SELECT id, total FROM orders WHERE customer = ? ORDER BY id DESC LIMIT 10",
            params=["@principal"])

# A Python function, for anything else.
@app.context("account")
def account(req) -> dict:
    """The caller's account summary."""
    return crm.summary(req.user["id"])
```

Agents name what they need:

```python
app.agent("billing", context=["policy", "my_orders"], ...)
```

and the model sees, after the agent's own system prompt:

```xml
<context name="policy" description="Support policy the agents must follow.">
{"refund_window_days": 30, "auto_refund_limit_usd": 500}
</context>

<context name="my_orders">
[{"id": 1041, "total": 12900}, {"id": 1038, "total": 4800}]
</context>
```

Behaviours declare providers the same way and read them on demand:

```python
@app.behaviour("triage", tools=[...], context=["policy"])
def triage(ctx, input):
    policy = ctx.context("policy")          # the raw value, not a string
    ...
```

A behaviour may only read the providers it declared — what a procedure can
*see* is declared next to what it can *call* — and a provider name that does
not exist is a boot error.

!!! tip "`@principal` means the human"
    An agent runs as a delegated principal (`agent:billing#service`), and a
    worker started by that agent is delegated again. `@principal` resolves to
    the **root** of that chain — the API key or JWT subject behind however
    many agents deep the call is — so a query scoped by it returns that
    caller's data no matter which agent asks.

### Sizing

`max_chars` (default 4000) is a hard cap; a longer rendering is cut with a
marker saying so. Context is re-sent on **every step** of a run, so a 4,000
character block on a twelve-step run is roughly 12,000 tokens of input.
[Prompt caching](models.md#prompt-caching) makes most of that cheap on
Anthropic; it does not make it free. Prefer a query with a `LIMIT` to a dump
of the table.

---

## Memory

Most assistants need somewhere to keep what they learned; few frameworks ship
it. `app.memory` gives an application a durable, per-caller key-value store as
four tools, executed in Rust, with no Python and no extra service:

```python
notes = app.memory("notes", read_scopes=["read"], write_scopes=["write"])
# -> ["notes_remember", "notes_recall", "notes_search", "notes_forget"]

app.agent("assistant", memory="notes", ...)
```

`memory="notes"` adds the four tools to the agent and appends one sentence to
its system prompt saying how to use them. The tools are also ordinary routes
(`POST /notes/remember`, `GET /notes/recall/{key}`, `GET /notes/search`,
`DELETE /notes/forget/{key}`) and MCP tools, so a UI or another agent can read
the same memory.

| Tool | Does |
|---|---|
| `notes_remember(key, value)` | upsert |
| `notes_recall(key)` | one value, or 404 |
| `notes_search(query, limit=10)` | substring match on key or value, newest first |
| `notes_forget(key)` | delete |

Every row is keyed by the **root principal**. An agent writing on someone's
behalf writes to that someone's memory, and no caller can read another's —
enforced by the query, not by the model's good behaviour.

Search is substring match on purpose. Semantic retrieval over embeddings
belongs in a Python handler over the vector store you already run; this is the
scratchpad that most assistants need and that should not need infrastructure.

---

## Context policy

Tokens are the cost of an agent, and most of them are **re-sent** context:
every step replays the whole conversation. Two knobs on `app.agent` decide the
bulk of the bill.

### Tool result limit

```python
app.agent("analyst", tools=["list_orders"], tool_result_limit=8_000)
```

A tool result larger than `tool_result_limit` bytes (default 16 KB) is cut
before the model sees it, with a marker:

```
{"rows":[…]}…[truncated: showing 8000 of 61240 bytes; narrow the query to see the rest]
```

The full value is still in the step record and the audit log; only the model's
copy is bounded. A `list_*` call returning five hundred rows is the classic
way a context fills in one step, and the marker tells the model what to do
about it.

### Compaction

```python
app.agent("assistant", context_window=60_000, compact_with="fast", keep_recent=6)
```

The runtime knows the real size of every provider call — the provider reports
it — so `context_window` is checked against **measured** input tokens, not an
estimate. When the last call exceeded it, older turns are summarised by
`compact_with` (default: the `fast` alias) into one message, and the most
recent `keep_recent` messages are kept verbatim.

The cut always lands on an assistant turn, so a tool call is never separated
from its result, and the summary is inserted as a user turn, so role
alternation holds. The summary prompt asks for facts, identifiers, decisions
and the results later steps may need, and forbids adding anything. Each
compaction is a `compaction` step in the result and an event in the audit log.

```json
{"kind": "compaction", "result": {"messages_before": 23, "messages_after": 8, "summary_chars": 1810}}
```

Compaction is what makes [sessions](orchestration.md#sessions) usable for a
long conversation: instead of failing when the window fills, the conversation
gets shorter and continues.

[Models and token economy :material-arrow-right:](models.md){ .md-button .md-button--primary }
