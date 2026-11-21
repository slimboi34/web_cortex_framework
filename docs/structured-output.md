# Structured output

*New in 2.8.*

An agent's answer is text. That is fine for a person and awkward for a program: the next step
has to parse prose, and a model that is asked to "answer as JSON" does so most of the time.

Declare the shape instead, and the runtime holds the agent to it:

```python
from dataclasses import dataclass
from typing import Literal

@dataclass
class Triage:
    category: Literal["bug", "billing", "other"]
    urgency: int

app.agent(
    "triage",
    system="Classify the support ticket.",
    output=Triage,
)
```

```bash
curl -s localhost:8000/agents/triage -d '{"input": "I was charged twice this month"}'
```

```json
{
  "run_id": "…",
  "status": "completed",
  "output": "{\"category\":\"billing\",\"urgency\":3}",
  "data": {"category": "billing", "urgency": 3},
  "steps": [ … , {"kind": "output", "tool": "final_answer", "result": {"category": "billing", "urgency": 3}}]
}
```

`data` is the validated object. `output` is the same value as JSON text, so a caller that only
reads `output` keeps working.

## What `output=` accepts

- a **dataclass** or a **`TypedDict`**, converted with the same rules as a handler's signature
  ([type → schema](routing.md));
- a **JSON Schema dict** whose top-level `type` is `"object"`.

A dataclass or `TypedDict` is closed: since 2.9 its schema carries
`"additionalProperties": false`, nested classes included, so an answer with a key the class
does not declare is refused and corrected like any other mismatch. A `dict[str, X]` field stays
open. A recursive type (a `Node` whose `children` are `Node`s) is described once under `$defs`
and referred to with `{"$ref": "#/$defs/Node"}`, so every level is checked, not just the first.
A `set[...]` field carries `uniqueItems`, which is checked.

A schema dict is checked at declaration: a keyword of the wrong type (`required` that is not a
list of strings, a negative or fractional `minItems`, a `$ref` with no target) fails at boot
rather than quietly accepting every answer. `2.0` is accepted wherever an integer is.

The top level must be an object, because the answer is delivered as a tool call and a tool's
arguments are an object on both wire formats. Wrap a list or a scalar in a field:
`{"type": "object", "properties": {"items": {"type": "array", …}}, "required": ["items"]}`.
Anything else is a `ValueError` at declaration.

## How it works

The agent is given one more tool, `final_answer`, whose input schema is your schema, and one
more sentence of system prompt telling it to answer through that tool. Then:

1. **The model calls `final_answer`.** The runtime validates the arguments against the schema.
   If they match, the run completes with them as `data`. No further model call is made.
2. **They do not match.** The model gets the list of problems back as the tool's result
   (`$.urgency: expected integer, got string; $: unexpected property "notes"`) and tries again.
3. **The model answers in prose instead.** If the text contains JSON that matches the schema,
   that is accepted, which is what a small local model that ignores tools tends to produce.
   When the text holds several JSON objects (a draft, then the answer), the last one that
   parses is used.
   Otherwise the model is told what was wrong and the next call *forces* `final_answer`
   (`tool_choice`).
4. **After two corrections** a third answer that still does not match fails the run:
   `status: "failed"`, `code: "WC318"`, HTTP 502. Each refusal is a step with `kind: "output"`,
   `code: "WC318"` and the problems in `error`, so you can see what the model kept getting wrong.

The check happens in the Rust core, not at the provider. A provider *asks* the model to follow
a tool's schema; whether it enforces it depends on the provider, the model and the schema. The
runtime's check is what makes the shape a property of the run.

Corrections cost model calls, and they count against `max_steps` and the token budget like any
other step.

## What the validator checks

`type` (a name or a list), `nullable`, `enum`, `const`, `properties`, `required`,
`additionalProperties` (a boolean or a schema), `items`, `prefixItems`, `minItems`, `maxItems`,
`uniqueItems`, `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `minLength`,
`maxLength`, `anyOf`, `oneOf`, `allOf`, and `$ref` to a local `#/$defs/...` entry.

Numbers compare by value wherever they appear, so `1` matches an `enum` or `const` of `1.0`,
inside arrays and objects too, and integers are compared exactly, however large.

It is a deliberate subset. `pattern`, `format` and any other `$ref` are **ignored**, not
refused: an unknown keyword never fails an answer that is otherwise right. If a field needs a
pattern check, do it in the code that consumes `data`. `patternProperties` is not enforced
either, and because it is ignored, an object that has it is not held to
`additionalProperties: false` (a key the pattern was meant to allow is not refused).

## Composition

- **Agents as tools.** A supervisor that lists a typed agent in `tools=` receives the whole
  envelope as the tool's result, `data` included.
- **Flows.** A pipeline step that follows a typed agent receives `data`, the object, rather
  than the JSON text, so `{"tool": "lookup", "input": {"id": "$.urgency"}}` resolves against
  real fields.
- **Handoffs.** The schema belongs to the agent the caller asked. After a handoff the
  specialist is offered the same `final_answer` tool, so the caller's contract holds whoever
  ends up answering.
- **Sessions.** An accepted answer is stored as a complete tool call and result, so the next
  turn of the session replays cleanly.
- **OpenAPI and TypeScript.** A typed agent's route advertises its envelope with `data` typed,
  so the generated client knows the shape.

The tool name `final_answer` is taken on an agent with `output=`: listing a route with that
tool name in its `tools=` is a boot error, and so is handing off (directly or through a chain
of handoffs) to an agent that lists one. Agents without `output=` are unaffected.

## Behaviours already had this

Inside a behaviour, `ctx.ask(prompt, schema={...})` forces a single model call into a shape,
and since 2.9 it is held to it the same way: the answer is validated, a mismatch goes back to
the model with the problems (twice), and a third miss raises with `WC318`. `output=` is the
same idea for a whole agent run, with its tools, handoffs and budget.

## Testing it offline

Under `WEBCORTEX_FAKE_PROVIDER=1` the input `tool:final_answer {"category": "billing", "urgency": 3}`
makes the fake model give that answer, so both the accepting and the refusing paths can be
tested without a key:

```python
status, body = post("/agents/triage", {"input": 'tool:final_answer {"category": "nope", "urgency": 3}'})
assert body["status"] == "failed" and body["code"] == "WC318"
```

Pair it with an [eval](evals.md) to pin the shape down in CI.
