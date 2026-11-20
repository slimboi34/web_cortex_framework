# Request options

*New in 2.8.*

WebCortex speaks two wire formats and gives the settings every model call needs a keyword:
`model`, `max_tokens`, `temperature`, `reasoning`. Providers have many more, and they differ:
`seed`, `top_p`, Ollama's `keep_alive`, vLLM's guided decoding, OpenAI's `response_format`,
Anthropic's `thinking`. Adding a keyword for each would be the universal LLM abstraction the
[design](https://github.com/slimboi34/web_cortex_framework/blob/main/DESIGN.md) refuses to
build.

`options` is the pass-through instead. Each key becomes a top-level field of the request body
sent to the provider, exactly as you wrote it:

```python
app.agent(
    "extractor",
    model="ollama/qwen3.5:9b",
    options={"seed": 7, "top_p": 0.9, "keep_alive": "30m"},
)
```

WebCortex does not interpret an option, rename it or check it against the provider. What you
write is what is sent, so the provider's own documentation is the reference, and a field the
provider does not know is the provider's error to report (`WC304`, with its message).

## Three places to set them

| Where | Applies to |
| --- | --- |
| `app.agent(..., options={...})` | every model call that agent makes |
| `@app.behaviour(..., options={...})` | every `ctx.ask` / `ctx.ask_many` in that behaviour |
| `app.request_options(provider, **options)` or `app.provider(..., options={...})` | every call to that provider, from anything |

Provider options sit **beneath** an agent's or behaviour's own: the two are merged key by key
and the agent wins.

```python
app.request_options("ollama", keep_alive="30m")            # all local calls
app.agent("fast_local", model="ollama/qwen3.5:4b", options={"seed": 1})
# sends keep_alive="30m" and seed=1
```

The provider name is the prefix of the model name. For names without a prefix, it is
`"anthropic"` for `claude-*` and `"openai"` for `gpt-*`, `o1`, `o3` and `o4`, the same rule
that picks the provider ([models](models.md#providers-two-wire-formats-chosen-by-prefix)).
Provider options also reach flow classifiers and compaction calls, which have no options of
their own.

## Removing a field

A value of `None` removes a field the runtime would otherwise send. Newer OpenAI models take
`max_completion_tokens` and reject `max_tokens`:

```python
app.request_options("openai", max_tokens=None, max_completion_tokens=8000)
```

An option also overrides a keyword that maps to the same field: `options={"reasoning_effort": "high"}`
wins over `reasoning="low"`.

## Reserved fields

`model`, `messages`, `tools`, `tool_choice`, `system` and `stream` belong to the runtime. The
tool loop builds the first five; `stream` would turn the response into an event stream the
runtime does not parse. Setting or removing any of them is a `ValueError` at declaration, and
the core refuses a hand-written manifest that tries (`webcortex check` reports it).

## Examples

```python
# Anthropic: let the model decide how much to think
app.agent("analyst", model="default", max_tokens=20_000,
          options={"thinking": {"type": "adaptive"}})

# OpenAI-compatible: a reproducible sampling seed
app.agent("grader", model="openai/gpt-5", options={"seed": 42})

# vLLM: switch a Qwen chat template's thinking off for every call to that server
app.provider("vllm", base_url="http://gpu-box:8000/v1",
             options={"chat_template_kwargs": {"enable_thinking": False}})

# OpenRouter: route by price
app.provider("openrouter", base_url="https://openrouter.ai/api/v1",
             api_key_env="OPENROUTER_API_KEY", options={"provider": {"sort": "price"}})
```

These are illustrations of the mechanism. Which fields a given model accepts changes; check the
provider's documentation for the model you use.

## Seeing what is sent

`webcortex context` lists each agent's and provider's options. Under
`WEBCORTEX_FAKE_PROVIDER=1`, the input `options?` makes the fake model answer with the merged
options the call carried, which is how the test suite checks the merge:

```python
status, body = post("/agents/extractor", {"input": "options?"})
assert json.loads(body["output"]) == {"keep_alive": "30m", "seed": 7, "top_p": 0.9}
```

Options are part of what an [eval cassette](evals.md) is fingerprinted on, so changing one
makes the recorded responses stale, as it should.
