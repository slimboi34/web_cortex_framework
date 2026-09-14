# Models and token economy

The cost of an agent system is decided by three things: which model answers
each call, how much context each call carries, and how many calls are
made. WebCortex gives each a declaration.

---

## Aliases: name the tier, not the model

```python
app.models(
    default="claude-opus-5",
    fast="claude-haiku-4-5-20251001",
    local="ollama/qwen3.5:9b",
)
```

Anywhere a model is named — `app.agent(model=...)`, `@app.behaviour(model=...)`,
`ctx.ask(model=...)`, a flow's `classify_with` — an alias resolves here. Both
`default` and `fast` have built-in values, so an app that never calls
`app.models` still works.

The habit this enables is the single largest token lever there is: **judgement
uses `default`, classification and extraction use `fast`**, and moving a
workload to a cheaper or local model is one edit.

```python
@app.behaviour("triage", tools=["list_tickets", "update_tickets"], model="fast")
def triage(ctx, input):
    tickets = ctx.call("list_tickets", limit=50)
    verdicts = ctx.ask_many(                               # fast model, 50 prompts, one wait
        [f"Classify: {t['body']}" for t in tickets],
        schema={"type": "object", "properties": {"urgent": {"type": "boolean"}}},
    )
    ...
```

---

## Providers: two wire formats, chosen by prefix

WebCortex is deliberately not a universal LLM abstraction. It speaks exactly
two wire formats, each implemented properly, and a model name picks one by
prefix:

| Name | Provider | Needs |
|---|---|---|
| `claude-*` or `anthropic/<model>` | Anthropic Messages | `ANTHROPIC_API_KEY` |
| `gpt-*` or `openai/<model>` | OpenAI Chat Completions | `OPENAI_API_KEY` |
| `ollama/<model>` | Ollama, over its OpenAI-compatible endpoint | nothing; `OLLAMA_HOST` to override `http://127.0.0.1:11434` |
| `<name>/<model>` | a provider you declared | as declared |

The OpenAI Chat Completions format is what Ollama, vLLM, LM Studio, Groq,
OpenRouter and OpenAI itself all speak, so one implementation covers local
models and most hosted ones:

```python
app.provider("groq", base_url="https://api.groq.com/openai/v1", api_key_env="GROQ_API_KEY")
app.provider("lab",  base_url="http://gpu-box:8000/v1")          # vLLM, no key
app.models(fast="groq/llama-3.3-70b-versatile", local="lab/qwen3.5:32b")
```

Internally every conversation is kept in one canonical shape (Anthropic's
content blocks); the OpenAI provider translates at its boundary in both
directions. Sessions, the audit trail and compaction never see a
provider-specific structure, and a session started on one model can continue
on another.

### Local models

```bash
ollama pull qwen3.5:9b
```

```python
app.models(fast="ollama/qwen3.5:9b")
```

That is the whole setup. Structured output (`ctx.ask(schema=...)`, flow
routing) is requested through a forced tool call, which hosted providers
honour; smaller local models often answer in text instead, so the runtime
recovers JSON from prose and code fences before reporting a failure. In
practice a 9B model is a dependable classifier, and running fifty
classification leaves on it costs nothing.

An app that declares agents but sets no hosted key still boots; a run that
needs a missing key fails with a message naming the variable, and everything
else keeps serving.

---

## Prompt caching

On Anthropic, `cache=True` (the default on every agent) marks the system
prompt and the tool definitions as cacheable. Both are identical on every step
of a run, which is exactly the shape caching rewards: from the second step on,
that part of the input is billed at the cache-read rate rather than the input
rate. Context providers ride along, because they are part of the system
prompt.

The usage record separates the kinds:

```json
{"input_tokens": 1204, "cache_read_tokens": 18410, "cache_write_tokens": 0,
 "output_tokens": 611, "steps": 6, "tool_calls": 4}
```

On OpenAI-compatible endpoints the runtime reports `cached_tokens` where the
server provides them; nothing is requested, because nothing needs to be.

---

## Budgets

Every model-backed thing has a `token_budget`, enforced by the runtime before
each call — and in v2 the budget of the outermost run is **shared by
everything it calls**:

```python
app.agent("editor", tools=["researcher", "writer"], token_budget=100_000)
app.flow("briefing", pipeline=["researcher", "writer"], token_budget=150_000)
```

A run that exhausts its own budget or the tree's stops with
`budget_exhausted`; the result reports both `usage.total_tokens` (this run)
and `usage.tree_tokens` (everything under it). See
[Budgets compose](orchestration.md#budgets-compose).

Behaviours can read their spend mid-run and adapt:

```python
if ctx.usage["tree_tokens"] > 80_000:
    model = "fast"
```

---

## Concurrency: fewer waits, not fewer tokens

`ctx.ask_many` and `ctx.gather` do not reduce tokens — every prompt is still
sent — but they collapse a loop of round trips into one wait, which is the
difference between a triage that takes a minute and one that takes three
seconds. On a free-threaded interpreter the leaves run genuinely in parallel;
on any build the model calls are concurrent, because they are I/O.

```python
verdicts = ctx.ask_many(prompts, schema=..., model="fast", concurrency=8)
rows     = ctx.gather(("get_orders", {"id": 1}), ("get_orders", {"id": 2}))
```

Each item is one step, so `max_steps` still bounds the total.

---

## The ledger

Every provider call is charged to an in-process ledger, keyed by what made it
and which model answered:

```console
$ curl localhost:8000/_webcortex/usage -H "x-api-key: $ADMIN"
```

```json
{
  "runs": 41,
  "totals": {"calls": 188, "input_tokens": 412300, "output_tokens": 58211,
             "cache_read_tokens": 1203400, "cache_write_tokens": 21000},
  "by_model": {"claude-opus-5": {...}, "ollama/qwen3.5:9b": {...}},
  "by_caller": {"agent:front_desk": {...}, "behaviour:triage": {...}, "flow:desk": {...}},
  "estimated_cost_usd": null,
  "priced_cost_usd": 3.18,
  "unpriced_models": ["ollama/qwen3.5:9b"]
}
```

Cost is computed only from prices you declare, because prices change and a
stale number is worse than none:

```python
app.pricing("claude-opus-5", input_per_mtok=5, output_per_mtok=25,
            cache_read_per_mtok=0.5, cache_write_per_mtok=6.25)
```

`estimated_cost_usd` is a number only when every model that spent anything
has a price; otherwise it is `null` and `priced_cost_usd` shows the part that
could be computed. A local model with no declared price is unpriced, not free
— declare `input_per_mtok=0` if that is what you mean.

The ledger is bounded, resets on restart, and answers "what is this app
spending, right now, on what" without a metrics pipeline. Ship the audit log
for history.

---

## A cost checklist

- [ ] Leaves that classify, extract or route use `model="fast"` — ideally a
      local model.
- [ ] Every agent, behaviour and flow has a `token_budget`; the outermost one
      is what you would be willing to pay per request.
- [ ] `tool_result_limit` is set below the default for agents that call
      `list_*` tools.
- [ ] Long-running or session-backed agents have a `context_window`.
- [ ] Context providers use `LIMIT` and a small `max_chars`.
- [ ] `webcortex check` shows the tools each agent actually needs — no more.
- [ ] Prices are declared, so `/usage` reports dollars.

[AI-native development :material-arrow-right:](ai-development.md){ .md-button .md-button--primary }
