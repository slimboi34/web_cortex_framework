# Traces

*New in 2.8.*

A run result lists the steps one agent took. It does not show what the worker it called did,
how long the flow around it took, or where the tokens of the whole request tree went. A trace
does. Every agent, flow and behaviour run records a tree of **spans**: the run itself, each
model call with its token counts, each tool call, each handoff, and any run a tool call
started, nested under the call that started it.

```text
$ webcortex trace latest
  trace 7d1c…e02a  ok  2.41s  4 model call(s)  2 tool call(s)  1830 tokens
  agent front_desk  2.41s  completed
    └ chat default  0.61s  412 in / 38 out
    └ tool billing  1.52s
      └ agent billing  1.52s  completed
        └ chat default  0.58s  390 in / 41 out
        └ tool lookup_invoice  3ms
        └ chat default  0.90s  455 in / 96 out
    └ chat default  0.27s  362 in / 36 out
```

Tracing is on by default. There is nothing to add to an app to get it. (The tree above shows
what the command prints; its timings and token counts are an illustration, not a
measurement.)

## Finding a trace

- A run result carries `trace_id`. So does a flow's envelope.
- At the edge, the HTTP response carries the header `x-webcortex-trace`. That is the only way
  to learn a behaviour's trace, since a behaviour returns whatever its function returned.
- `GET /_webcortex/status` lists `trace_id` on each recent run.
- `"latest"` names the newest trace wherever an id is accepted.

## Reading traces

| | |
| --- | --- |
| `GET /_webcortex/traces` | recent traces, newest first, one summary each |
| `GET /_webcortex/traces/{id}` | one trace with every span; `latest` works as an id |
| `webcortex trace [ID]` | the same, as a table or a tree; `--json` for the document |
| MCP tool `webcortex_trace` | the same for a coding agent: no arguments lists, `trace_id` returns one |
| `app.traces()` / `app.trace(id)` | the same from Python |

All of it is control-plane data: once authentication is configured it needs the
`webcortex:admin` scope, like `/status`. A trace that is not held is a 404 with code `WC219`.

A summary has `trace_id`, `name` and `kind` (of the root span), `status`, `started_unix_ms`,
`duration_ms` (null while any span is still open), `spans`, `open_spans`, `dropped_spans`,
`model_calls`, `tool_calls`, `input_tokens`, `output_tokens`, `cache_tokens` and `errors` (the
codes of failed spans). The token counts are for the **whole tree**, which is what
`usage.tree_tokens` on the outermost run reports as one number.

A span has:

| field | |
| --- | --- |
| `span_id`, `parent_span_id` | 16 hex digits; the root's parent is null |
| `kind` | `agent`, `model`, `tool`, `flow`, `behaviour`, `handoff` or `output` |
| `name` | `agent billing`, `chat default`, `tool lookup_invoice`, `flow intake`, … |
| `start_ms` | offset from the start of the trace |
| `duration_ms` | null while the span is open |
| `status` | `ok`, `error`, `cancelled` (the request timed out or the client left) or `open` |
| `code` | the [error code](error-codes.md) of a span that failed |
| `attributes` | see below |

## What a span holds, and what it never holds

Attributes use the OpenTelemetry GenAI names where one exists:

| attribute | on |
| --- | --- |
| `gen_ai.operation.name` | `invoke_agent`, `chat`, `execute_tool` |
| `gen_ai.agent.name`, `gen_ai.tool.name` | agent and tool spans |
| `gen_ai.request.model`, `gen_ai.response.model` | model spans: the name the app used, and what it resolved to |
| `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens` | model spans |
| `webcortex.usage.cache_read_tokens`, `webcortex.usage.cache_write_tokens` | model spans |
| `webcortex.run.id`, `.status`, `.steps`, `.tokens`, `.depth` | agent spans |
| `webcortex.flow.name` | flow spans |

A span never holds a prompt, a tool's arguments, a tool's result, a model's answer, a header
or a credential. Names, timings, counts and codes only, the same rule the
[status view](diagnostics.md#the-live-status-view) follows. The audit trail
(`/_webcortex/audit`) is where arguments are recorded; a trace is safe to send to a third
party's collector in a way the audit trail is not.

## Limits

Traces are kept in memory: the newest 200, each up to 500 spans, and not across a restart.
Past 500, further spans in that trace are counted in `dropped_spans` rather than stored. Both
numbers are settings:

```python
app.tracing(keep=1000, max_spans=2000)
app.tracing(enabled=False)        # no spans, no trace_id on results
```

Only routes that run a model open spans. A static, query, proxy, page or plain Python route
never touches the tracer. For a traced run the cost is a short mutex-guarded write at the
start and at the end of each span. It has not been benchmarked.

## Exporting to a collector (OTLP)

Finished spans can be sent to any OpenTelemetry collector, or a vendor that accepts OTLP over
HTTP, as JSON:

```python
app.tracing(
    otlp_endpoint="http://localhost:4318",     # /v1/traces is appended
    otlp_headers_env="OTLP_HEADERS",           # the NAME of a variable holding "k=v,k2=v2"
    service_name="support-desk",               # defaults to the app's name
)
```

Or with no code at all, through the standard environment variables, which are read when the
app declares no endpoint:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318     # /v1/traces is appended
export OTEL_EXPORTER_OTLP_HEADERS="authorization=Bearer …"
webcortex run
```

`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, when set, is used exactly as given. As everywhere else
in WebCortex the credential is named, never embedded: `otlp_headers_env` is the name of a
variable, and the manifest stays safe to log.

Spans are posted in batches (up to 256, at most about a second after the first one in the
batch ended), with `service.name`, `service.version` (the app's version) and the scope
`webcortex`. Model calls are `CLIENT` spans and everything else `INTERNAL`; a failed span has
status `ERROR` with its code as the message and as the attribute `webcortex.error.code`.

The exporter is deliberately lossy. A batch the collector refuses, or cannot be sent, is
dropped and counted, never retried, and the queue between the runtime and the exporter is
bounded (4096 spans): a collector that is down must not grow memory inside the app it
observes. `GET /_webcortex/traces` reports `export: {configured, spans_exported, spans_failed,
spans_dropped, last_error}`, and `webcortex trace` prints that line. On shutdown the exporter
sends one last batch.

Only the protocol's JSON encoding over HTTP is implemented, not protobuf or gRPC. Collectors
accept it on port 4318.

## Joining a caller's trace

A request that carries a W3C `traceparent` header joins that trace instead of starting one:
the run's root span takes the header's trace id and names the caller's span as its parent, so
in your tracing backend the agent run appears under the request that caused it.

```bash
curl -s localhost:8000/agents/desk \
  -H 'traceparent: 00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01' \
  -d '{"input": "hi"}'
# "trace_id": "0af7651916cd43dd8448eb211c80319c"
```

A malformed header is ignored and a fresh trace is started. WebCortex does not yet send
`traceparent` onward to proxied upstreams or to model providers.

## Approvals

A run that suspends on an [approval gate](agents.md#approval-gates) ends its span with
`webcortex.run.status: awaiting_approval`. When a human decides, the continuation is a new
span named `agent <name> (resumed)` in the same trace, a child of the original, so the wait is
visible as the gap between them.

## For a coding agent

With the MCP server connected (`webcortex mcp-config`), an agent debugging the app can ask
`webcortex_trace` for the last run and read where the time and tokens went, then
`webcortex_explain` for any code on a failed span. Together with `webcortex_status` that is the
loop: what is failing, what that run did, what the code means.
