//! 2.8: structured output, request options, traces and cassettes, end to end
//! through `App`.

use serde_json::{Value, json};
use std::sync::Arc;
use webcortex_core::agent::provider::{
    CompletionRequest, FakeProvider, ModelProvider, ScriptedProvider, anthropic_body, openai_body,
};
use webcortex_core::agent::replay::{RecordingProvider, ReplayProvider};
use webcortex_core::agent::{Conversation, FINAL_TOOL, MAX_OUTPUT_REPAIRS, Message, StepKind};
use webcortex_core::manifest::AgentDef;
use webcortex_core::{AgentRuntime, App, Manifest, MemoryAudit, Principal, RunOptions, RunStatus, WebCortexRequest};

fn weather_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"city": {"type": "string"}, "temp_c": {"type": "number"}},
        "required": ["city", "temp_c"],
        "additionalProperties": false,
    })
}

fn manifest() -> Manifest {
    serde_json::from_value(json!({
        "name": "evidence",
        "version": "1.2.3",
        "routes": [
            {"id": 0, "method": "GET", "path": "/ping", "op": {"kind": "static", "body": {"pong": true}},
             "tool": {"expose": true, "name": "ping", "read_only": true}},
            {"id": 1, "method": "POST", "path": "/agents/worker", "op": {"kind": "agent", "agent": "worker"},
             "tool": {"expose": true, "name": "worker"}},
            {"id": 2, "method": "POST", "path": "/agents/boss", "op": {"kind": "agent", "agent": "boss"},
             "tool": {"expose": true, "name": "boss"}},
            {"id": 3, "method": "POST", "path": "/agents/typed", "op": {"kind": "agent", "agent": "typed"},
             "tool": {"expose": true, "name": "typed"}},
            {"id": 4, "method": "POST", "path": "/echo", "op": {"kind": "static", "body": {"ok": true}},
             "tool": {"expose": true, "name": "echo"}},
            {"id": 5, "method": "POST", "path": "/flows/report", "op": {"kind": "flow", "flow": "report"},
             "tool": {"expose": true, "name": "report"}},
            {"id": 6, "method": "POST", "path": "/danger", "op": {"kind": "static", "body": {"done": true}},
             "tool": {"expose": true, "name": "danger"}, "approval": "required"}
        ],
        "agents": [
            {"name": "worker", "model": "test", "tools": ["ping"], "max_steps": 4},
            {"name": "boss", "model": "test", "tools": ["worker", "ping", "danger"], "max_steps": 6},
            {"name": "typed", "model": "test", "tools": ["ping"], "handoffs": ["worker"], "max_steps": 8,
             "output_schema": weather_schema()}
        ],
        "flows": [
            {"name": "report", "kind": {"kind": "pipeline", "steps": [{"tool": "typed"}, {"tool": "echo"}]}}
        ]
    }))
    .expect("manifest")
}

async fn app_with(provider: Arc<dyn ModelProvider>) -> Arc<App> {
    let rt = AgentRuntime::new(provider, Arc::new(MemoryAudit::default()));
    App::build_without_python(manifest()).await.expect("app").with_agent_runtime(rt).into_arc()
}

fn who() -> Principal {
    Principal {
        id: "u1".into(),
        kind: webcortex_core::PrincipalKind::ApiKey,
        scopes: Vec::new(),
        claims: Default::default(),
    }
}

async fn run(app: &App, agent: &str, input: &str) -> webcortex_core::RunResult {
    app.invoke_agent(agent, input, &who(), RunOptions::default()).await.expect("runtime")
}

fn spans(app: &App, trace_id: &str) -> Vec<Value> {
    app.tracer().get(trace_id).expect("trace held")["spans"].as_array().cloned().unwrap()
}

// ---------------------------------------------------------------- structured output

#[tokio::test]
async fn a_final_answer_that_matches_the_schema_completes_the_run_with_data() {
    let provider = Arc::new(ScriptedProvider::sequence(vec![
        ("tool", "ping", json!({})),
        ("tool", FINAL_TOOL, json!({"city": "Paris", "temp_c": 21.5})),
    ]));
    let app = app_with(provider.clone()).await;
    let out = run(&app, "typed", "weather?").await;
    assert_eq!(out.status, RunStatus::Completed);
    assert_eq!(out.data, Some(json!({"city": "Paris", "temp_c": 21.5})));
    assert_eq!(serde_json::from_str::<Value>(&out.output).unwrap(), json!({"city": "Paris", "temp_c": 21.5}));
    assert_eq!(out.code, None);
    let last = out.steps.last().unwrap();
    assert_eq!(last.kind, StepKind::Output);
    assert_eq!(last.result, Some(json!({"city": "Paris", "temp_c": 21.5})));
    // The model was offered the answer tool, and told to use it.
    let seen = provider.snapshots();
    assert!(seen[0].tool_names.contains(&FINAL_TOOL.to_string()));
    assert!(seen[0].system_suffix.contains("final_answer"));
    assert_eq!(provider.calls(), 2, "an accepted answer ends the run without another model call");
}

#[tokio::test]
async fn an_answer_that_does_not_match_is_sent_back_and_a_corrected_one_is_accepted() {
    let provider = Arc::new(ScriptedProvider::sequence(vec![
        ("tool", FINAL_TOOL, json!({"city": "Paris", "temp_c": "warm", "extra": 1})),
        ("tool", FINAL_TOOL, json!({"city": "Paris", "temp_c": 21})),
    ]));
    let app = app_with(provider.clone()).await;
    let out = run(&app, "typed", "weather?").await;
    assert_eq!(out.status, RunStatus::Completed);
    assert_eq!(out.data, Some(json!({"city": "Paris", "temp_c": 21})));
    let rejected = out.steps.iter().find(|s| s.code.as_deref() == Some("WC318")).expect("a rejected step");
    assert_eq!(rejected.kind, StepKind::Output);
    let why = rejected.error.as_deref().unwrap();
    assert!(why.contains("$.temp_c: expected number, got string"), "{why}");
    assert!(why.contains("unexpected property \"extra\""), "{why}");
    // The model saw the problems as the tool's result.
    let second = &provider.snapshots()[1];
    let told = second.messages.last().unwrap().to_string();
    assert!(told.contains("WC318") && told.contains("temp_c"), "{told}");
}

#[tokio::test]
async fn json_in_a_text_answer_is_accepted_when_it_conforms() {
    let provider = Arc::new(ScriptedProvider::text("Here you go: {\"city\": \"Oslo\", \"temp_c\": -3}"));
    let app = app_with(provider.clone()).await;
    let out = run(&app, "typed", "weather?").await;
    assert_eq!(out.status, RunStatus::Completed);
    assert_eq!(out.data, Some(json!({"city": "Oslo", "temp_c": -3})));
    assert_eq!(provider.calls(), 1);
}

#[tokio::test]
async fn a_prose_answer_is_followed_by_a_forced_final_answer_call() {
    let provider = Arc::new(ScriptedProvider::sequence(vec![
        ("text", "It is warm in Paris.", json!(null)),
        ("tool", FINAL_TOOL, json!({"city": "Paris", "temp_c": 25})),
    ]));
    let app = app_with(provider.clone()).await;
    let out = run(&app, "typed", "weather?").await;
    assert_eq!(out.status, RunStatus::Completed);
    assert_eq!(out.data.unwrap()["temp_c"], 25);
    let seen = provider.snapshots();
    assert_eq!(seen[0].force_tool, None);
    assert_eq!(seen[1].force_tool.as_deref(), Some(FINAL_TOOL));
    assert!(seen[1].messages.last().unwrap().to_string().contains("plain text"));
}

#[tokio::test]
async fn an_answer_that_never_matches_fails_the_run_with_wc318_after_the_repair_limit() {
    let provider = Arc::new(ScriptedProvider::always_tool(FINAL_TOOL, json!({"city": 7})));
    let app = app_with(provider.clone()).await;
    let out = run(&app, "typed", "weather?").await;
    assert_eq!(out.status, RunStatus::Failed);
    assert_eq!(out.code.as_deref(), Some("WC318"));
    assert!(out.hint.as_deref().unwrap().contains("schema"));
    assert_eq!(out.data, None);
    assert_eq!(provider.calls() as u32, MAX_OUTPUT_REPAIRS + 1);
    assert_eq!(out.steps.iter().filter(|s| s.code.as_deref() == Some("WC318")).count() as u32, MAX_OUTPUT_REPAIRS + 1);
    // The HTTP face of the same failure.
    assert_eq!(webcortex_core::app::run_result_response(&out).status, 502);
}

#[tokio::test]
async fn an_accepted_answer_leaves_a_session_the_provider_can_continue() {
    let provider = Arc::new(ScriptedProvider::sequence(vec![
        ("tool", FINAL_TOOL, json!({"city": "Paris", "temp_c": 20})),
        ("tool", FINAL_TOOL, json!({"city": "Rome", "temp_c": 28})),
    ]));
    let app = app_with(provider.clone()).await;
    let opts = || RunOptions { session_id: Some("s".into()), ..Default::default() };
    let first = app.invoke_agent("typed", "paris?", &who(), opts()).await.unwrap();
    assert_eq!(first.status, RunStatus::Completed);
    let second = app.invoke_agent("typed", "rome?", &who(), opts()).await.unwrap();
    assert_eq!(second.data.unwrap()["city"], "Rome");
    // Every tool_use the second request replays is followed by its tool_result.
    let messages = &provider.snapshots()[1].messages;
    for (i, m) in messages.iter().enumerate() {
        let uses: Vec<&str> = m["content"].as_array().into_iter().flatten()
            .filter(|b| b["type"] == "tool_use").filter_map(|b| b["id"].as_str()).collect();
        for id in uses {
            let next = messages.get(i + 1).expect("a message after the tool_use").to_string();
            assert!(next.contains(id), "tool_use {id} has no result");
        }
    }
}

#[tokio::test]
async fn the_entry_agents_schema_still_applies_after_a_handoff() {
    let provider = Arc::new(ScriptedProvider::sequence(vec![
        ("tool", "transfer_to_worker", json!({"reason": "knows weather"})),
        ("tool", FINAL_TOOL, json!({"city": "Lima", "temp_c": 18})),
    ]));
    let app = app_with(provider.clone()).await;
    let out = run(&app, "typed", "weather?").await;
    assert_eq!(out.status, RunStatus::Completed);
    assert_eq!(out.path, vec!["typed", "worker"]);
    assert_eq!(out.data.unwrap()["city"], "Lima");
    let after = &provider.snapshots()[1];
    assert!(after.tool_names.contains(&FINAL_TOOL.to_string()));
    assert!(after.system_suffix.contains("final_answer"));
}

#[tokio::test]
async fn an_agent_without_a_schema_is_unchanged_and_may_use_the_tool_name() {
    let provider = Arc::new(ScriptedProvider::sequence(vec![("tool", FINAL_TOOL, json!({"x": 1})), ("text", "done", json!(null))]));
    let app = app_with(provider.clone()).await;
    let out = run(&app, "worker", "go").await;
    assert_eq!(out.status, RunStatus::Completed);
    assert_eq!(out.output, "done");
    assert_eq!(out.data, None);
    assert!(!provider.snapshots()[0].tool_names.contains(&FINAL_TOOL.to_string()));
    assert_eq!(out.steps.iter().filter(|s| s.code.as_deref() == Some("WC307")).count(), 1);
    assert!(serde_json::to_value(&out).unwrap().get("data").is_none());
}

#[tokio::test]
async fn a_flow_hands_a_structured_answer_to_the_next_step_as_an_object() {
    let provider = Arc::new(ScriptedProvider::sequence(vec![("tool", FINAL_TOOL, json!({"city": "Kyiv", "temp_c": 9}))]));
    let app = app_with(provider).await;
    let step = webcortex_core::manifest::FlowStep { tool: "echo".into(), input: None };
    let envelope = app.call_tool_as("typed", &json!({"input": "x"}), &who()).await.unwrap();
    assert_eq!(envelope["data"], json!({"city": "Kyiv", "temp_c": 9}));
    // What the pipeline does with that envelope: the next step's arguments are the object.
    let args = webcortex_core::flow::build_args(&app, &step, &envelope["data"], &json!({}));
    assert_eq!(args, json!({"city": "Kyiv", "temp_c": 9}));
}

#[test]
fn unusable_output_schemas_and_reserved_options_are_boot_errors() {
    let with = |agent: Value, extra: Value| -> Result<(), String> {
        let mut m = json!({
            "name": "t",
            "routes": [{"id": 0, "method": "GET", "path": "/f", "op": {"kind": "static", "body": {}},
                        "tool": {"expose": true, "name": "final_answer"}}],
            "agents": [agent],
        });
        for (k, v) in extra.as_object().unwrap() {
            m[k] = v.clone();
        }
        serde_json::from_value::<Manifest>(m).map_err(|e| e.to_string())?.validate()
    };
    let none = json!({});
    assert!(with(json!({"name": "a", "model": "m", "output_schema": {"type": "object"}}), none.clone()).is_ok());
    let err = with(json!({"name": "a", "model": "m", "output_schema": {"type": "array"}}), none.clone()).unwrap_err();
    assert!(err.contains("output schema") && err.contains("object"), "{err}");
    let err = with(json!({"name": "a", "model": "m", "tools": ["final_answer"], "output_schema": {"type": "object"}}), none.clone()).unwrap_err();
    assert!(err.contains("final_answer") && err.contains("taken"), "{err}");
    // Without a schema the name is free.
    assert!(with(json!({"name": "a", "model": "m", "tools": ["final_answer"]}), none.clone()).is_ok());

    let err = with(json!({"name": "a", "model": "m", "options": {"messages": []}}), none.clone()).unwrap_err();
    assert!(err.contains("agent \"a\"") && err.contains("\"messages\""), "{err}");
    let err = with(json!({"name": "a", "model": "m"}), json!({"models": {"options": {"ollama": {"stream": true}}}})).unwrap_err();
    assert!(err.contains("provider \"ollama\"") && err.contains("stream"), "{err}");
    assert!(with(json!({"name": "a", "model": "m", "options": {"seed": 7, "max_tokens": null}}), none.clone()).is_ok());

    let err = with(json!({"name": "a", "model": "m"}), json!({"tracing": {"keep": 0}})).unwrap_err();
    assert!(err.contains("tracing.keep"), "{err}");
    let err = with(json!({"name": "a", "model": "m"}), json!({"tracing": {"otlp_endpoint": "collector:4317"}})).unwrap_err();
    assert!(err.contains("otlp_endpoint"), "{err}");
}

// ---------------------------------------------------------------- request options

fn def_with(options: Value) -> AgentDef {
    serde_json::from_value(json!({"name": "a", "model": "m", "options": options, "reasoning": "none"})).unwrap()
}

#[test]
fn options_become_top_level_body_fields_on_both_wire_formats() {
    let def = def_with(json!({"seed": 7, "top_p": 0.9, "max_tokens": null, "max_completion_tokens": 512,
                              "thinking": {"type": "adaptive"}}));
    let conversation = Conversation { messages: vec![Message { role: "user".into(), content: json!("hi") }] };
    let req = CompletionRequest { agent: &def, conversation: &conversation, tools: &[], force_tool: None, system_suffix: "" };
    for body in [openai_body(&req), anthropic_body(&req)] {
        assert_eq!(body["seed"], 7);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["max_completion_tokens"], 512);
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert!(body.get("max_tokens").is_none(), "a null option removes the field");
        assert_eq!(body["model"], "m");
        assert!(body["messages"].is_array());
    }
    // An option overrides what a keyword set.
    let def = def_with(json!({"reasoning_effort": "high"}));
    let req = CompletionRequest { agent: &def, conversation: &conversation, tools: &[], force_tool: None, system_suffix: "" };
    assert_eq!(openai_body(&req)["reasoning_effort"], "high");
}

#[test]
fn reserved_fields_survive_even_if_options_name_them() {
    let mut def = def_with(json!({}));
    def.options.insert("model".into(), json!("other"));
    def.options.insert("messages".into(), json!(null));
    def.options.insert("stream".into(), json!(true));
    let conversation = Conversation { messages: vec![Message { role: "user".into(), content: json!("hi") }] };
    let req = CompletionRequest { agent: &def, conversation: &conversation, tools: &[], force_tool: None, system_suffix: "" };
    let body = openai_body(&req);
    assert_eq!(body["model"], "m");
    assert!(body["messages"].is_array());
    assert!(body.get("stream").is_none());
}

#[test]
fn a_providers_options_sit_beneath_the_agents() {
    let cfg: webcortex_core::manifest::ModelsConfig = serde_json::from_value(json!({
        "options": {
            "ollama": {"keep_alive": "30m", "seed": 1},
            "anthropic": {"service_tier": "auto"},
            "openai": {"store": false},
        }
    }))
    .unwrap();
    let registry = webcortex_core::ProviderRegistry::from_env(&cfg);
    let own = def_with(json!({"seed": 9})).options;
    assert_eq!(Value::Object(registry.options_for("ollama/qwen3.5:9b", &own)), json!({"keep_alive": "30m", "seed": 9}));
    assert_eq!(Value::Object(registry.options_for("claude-opus-5", &own)), json!({"service_tier": "auto", "seed": 9}));
    assert_eq!(Value::Object(registry.options_for("gpt-5", &Default::default())), json!({"store": false}));
    assert_eq!(Value::Object(registry.options_for("groq/llama", &own)), json!({"seed": 9}));
}

// ---------------------------------------------------------------- traces

#[tokio::test]
async fn a_supervisor_and_its_worker_are_one_trace() {
    // boss → worker (an agent as a tool) → ping, then both answer.
    let provider = Arc::new(
        ScriptedProvider::sequence(vec![
            ("tool", "worker", json!({"input": "look"})),
            ("tool", "ping", json!({})),
            ("text", "worker done", json!(null)),
            ("text", "all done", json!(null)),
        ])
        .with_tokens(100, 20),
    );
    let app = app_with(provider).await;
    let out = run(&app, "boss", "go").await;
    assert_eq!(out.status, RunStatus::Completed);
    let trace_id = out.trace_id.clone().expect("a trace id on the result");
    assert_eq!(trace_id.len(), 32);

    let spans = spans(&app, &trace_id);
    let named = |n: &str| spans.iter().find(|s| s["name"] == n).unwrap_or_else(|| panic!("no span {n}: {spans:#?}"));
    let boss = named("agent boss");
    let worker_tool = named("tool worker");
    let worker = named("agent worker");
    let ping = named("tool ping");
    assert_eq!(boss["parent_span_id"], Value::Null);
    assert_eq!(worker_tool["parent_span_id"], boss["span_id"]);
    assert_eq!(worker["parent_span_id"], worker_tool["span_id"]);
    assert_eq!(ping["parent_span_id"], worker["span_id"]);
    assert_eq!(boss["attributes"]["webcortex.run.status"], "completed");
    assert_eq!(boss["attributes"]["webcortex.run.id"], out.run_id.as_str());
    assert_eq!(worker["attributes"]["webcortex.run.depth"], 1);
    assert_eq!(ping["attributes"]["gen_ai.tool.name"], "ping");
    let models: Vec<&Value> = spans.iter().filter(|s| s["kind"] == "model").collect();
    assert_eq!(models.len(), 4);
    assert!(models.iter().all(|m| m["status"] == "ok" && m["attributes"]["gen_ai.usage.input_tokens"] == 100));
    assert!(spans.iter().all(|s| s["duration_ms"].is_number()), "every span ended");

    // The summary counts the whole tree, which one run's usage does not.
    let listed = &app.tracer().list()["traces"][0];
    assert_eq!(listed["trace_id"], trace_id.as_str());
    assert_eq!(listed["name"], "agent boss");
    assert_eq!(listed["model_calls"], 4);
    assert_eq!(listed["tool_calls"], 2);
    assert_eq!(listed["input_tokens"], 400);
    assert_eq!(listed["output_tokens"], 80);
    assert_eq!(listed["input_tokens"].as_u64().unwrap() + listed["output_tokens"].as_u64().unwrap(), out.usage.tree_tokens);
    // And the status view points at it.
    assert_eq!(app.status()["agents"]["recent"].as_array().unwrap().last().unwrap()["trace_id"], trace_id.as_str());
}

#[tokio::test]
async fn failures_refusals_and_handoffs_are_spans_with_codes() {
    let provider = Arc::new(ScriptedProvider::sequence(vec![
        ("tool", "nope", json!({})),
        ("tool", "transfer_to_worker", json!({"reason": "r"})),
        ("error", "anthropic 529: overloaded", json!(null)),
    ]));
    let app = app_with(provider).await;
    let out = run(&app, "typed", "go").await;
    assert_eq!(out.status, RunStatus::Failed);
    let spans = spans(&app, out.trace_id.as_deref().unwrap());
    let root = spans.iter().find(|s| s["name"] == "agent typed").unwrap();
    assert_eq!(root["status"], "error");
    assert_eq!(root["code"], "WC304");
    assert_eq!(spans.iter().find(|s| s["name"] == "tool nope").unwrap()["code"], "WC307");
    assert_eq!(spans.iter().find(|s| s["name"] == "handoff worker").unwrap()["kind"], "handoff");
    let failed_model = spans.iter().rfind(|s| s["kind"] == "model").unwrap();
    assert_eq!(failed_model["code"], "WC304");
    let errors = app.tracer().get(out.trace_id.as_deref().unwrap()).unwrap()["errors"].clone();
    assert!(errors.as_array().unwrap().contains(&json!("WC304")));
}

#[tokio::test]
async fn a_flow_and_its_steps_are_one_trace_and_the_response_names_it() {
    let provider = Arc::new(ScriptedProvider::sequence(vec![("tool", FINAL_TOOL, json!({"city": "Bern", "temp_c": 4}))]));
    let app = app_with(provider).await;
    let mut req = WebCortexRequest::synthetic("POST", "/flows/report");
    req.body = bytes::Bytes::from_static(b"{\"input\": \"x\"}");
    req.principal = who();
    let res = app.dispatch(req).await;
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    let body = res.json_value();
    let trace_id = body["trace_id"].as_str().expect("trace id in the flow envelope");
    let header = res.headers.iter().find(|(k, _)| k == "x-webcortex-trace").map(|(_, v)| v.as_str());
    assert_eq!(header, Some(trace_id));
    let spans = spans(&app, trace_id);
    let flow = spans.iter().find(|s| s["name"] == "flow report").unwrap();
    let step = spans.iter().find(|s| s["name"] == "tool typed").unwrap();
    let agent = spans.iter().find(|s| s["name"] == "agent typed").unwrap();
    assert_eq!(flow["kind"], "flow");
    assert_eq!(step["parent_span_id"], flow["span_id"]);
    assert_eq!(agent["parent_span_id"], step["span_id"]);
    assert!(spans.iter().any(|s| s["name"] == "tool echo"));
    assert_eq!(flow["status"], "ok");
}

#[tokio::test]
async fn an_incoming_traceparent_is_joined_and_only_edge_responses_carry_the_header() {
    let app = app_with(Arc::new(FakeProvider)).await;
    let mut req = WebCortexRequest::synthetic("POST", "/agents/boss");
    req.body = bytes::Bytes::from_static(b"{\"input\": \"tool:worker {\\\"input\\\": \\\"hi\\\"}\"}");
    req.principal = who();
    req.headers.insert("traceparent".into(), "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into());
    let res = app.dispatch(req).await;
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    let body = res.json_value();
    assert_eq!(body["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(res.headers.iter().filter(|(k, _)| k == "x-webcortex-trace").count(), 1);
    let spans = spans(&app, "4bf92f3577b34da6a3ce929d0e0e4736");
    let root = spans.iter().find(|s| s["name"] == "agent boss").unwrap();
    assert_eq!(root["parent_span_id"], "00f067aa0ba902b7");
    assert!(spans.iter().any(|s| s["name"] == "agent worker"));
    assert_eq!(app.tracer().list()["kept"], 1, "the nested run did not start a trace of its own");
}

#[tokio::test]
async fn a_run_resumed_after_an_approval_continues_its_trace() {
    let provider = Arc::new(ScriptedProvider::tool_then_text("danger", json!({}), "done"));
    let app = app_with(provider).await;
    let first = run(&app, "boss", "go").await;
    assert_eq!(first.status, RunStatus::AwaitingApproval);
    let trace_id = first.trace_id.clone().unwrap();
    let approval = first.pending_approval.unwrap().approval_id;
    let admin = Principal { scopes: vec!["webcortex:admin".into()], ..who() };
    let resumed = app.resolve_approval(&approval, true, "ok", &admin).await.unwrap();
    assert_eq!(resumed.status, RunStatus::Completed);
    assert_eq!(resumed.trace_id.as_deref(), Some(trace_id.as_str()));
    let spans = spans(&app, &trace_id);
    let original = spans.iter().find(|s| s["name"] == "agent boss").unwrap();
    let continued = spans.iter().find(|s| s["name"] == "agent boss (resumed)").unwrap();
    assert_eq!(continued["parent_span_id"], original["span_id"]);
    assert_eq!(original["attributes"]["webcortex.run.status"], "awaiting_approval");
    assert_eq!(spans.iter().find(|s| s["name"] == "tool danger").unwrap()["parent_span_id"], continued["span_id"]);
}

#[tokio::test]
async fn tracing_can_be_turned_off() {
    let mut m = serde_json::to_value(manifest()).unwrap();
    m["tracing"] = json!({"enabled": false});
    let rt = AgentRuntime::new(Arc::new(FakeProvider), Arc::new(MemoryAudit::default()));
    let app = App::build_without_python(serde_json::from_value(m).unwrap()).await.unwrap().with_agent_runtime(rt).into_arc();
    let out = run(&app, "boss", "tool:ping {}").await;
    assert_eq!(out.status, RunStatus::Completed);
    assert_eq!(out.trace_id, None);
    assert_eq!(app.tracer().list()["kept"], 0);
    assert!(serde_json::to_value(&out).unwrap().get("trace_id").is_none());
}

#[tokio::test]
async fn the_mcp_trace_tool_lists_traces_and_returns_one() {
    let app = app_with(Arc::new(FakeProvider)).await;
    let out = run(&app, "boss", "tool:ping {}").await;
    let call = |args: Value| {
        let app = app.clone();
        async move {
            let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                              "params": {"name": "webcortex_trace", "arguments": args}});
            let res = webcortex_core::mcp::handle(&app, body.to_string().as_bytes(), &who()).await;
            res.json_value()["result"].clone()
        }
    };
    let listed = call(json!({})).await;
    assert_eq!(listed["structuredContent"]["traces"][0]["trace_id"], out.trace_id.as_deref().unwrap());
    let one = call(json!({"trace_id": "latest"})).await;
    assert!(one["structuredContent"]["spans"].as_array().unwrap().len() >= 3);
    let missing = call(json!({"trace_id": "feedfacefeedfacefeedfacefeedface"})).await;
    assert_eq!(missing["isError"], true);
    assert_eq!(missing["structuredContent"]["error"]["code"], "WC219");
    let tools = webcortex_core::mcp::handle(
        &app,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string().as_bytes(),
        &who(),
    )
    .await
    .json_value();
    assert!(tools["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "webcortex_trace"));
}

// ---------------------------------------------------------------- cassettes

#[tokio::test]
async fn a_recorded_tree_replays_to_the_same_result_without_the_provider() {
    let script = || {
        Arc::new(
            ScriptedProvider::sequence(vec![
                ("tool", "worker", json!({"input": "look"})),
                ("tool", "ping", json!({})),
                ("text", "worker done", json!(null)),
                ("text", "all done", json!(null)),
            ])
            .with_tokens(50, 5),
        )
    };
    let recorder = Arc::new(RecordingProvider::new(script()));
    let app = App::build_without_python(manifest()).await.unwrap().with_provider(recorder.clone()).into_arc();
    let live = run(&app, "boss", "go").await;
    assert_eq!(live.status, RunStatus::Completed);
    let cassette: Value = serde_json::from_str(&recorder.cassette().to_string()).unwrap();
    assert_eq!(cassette["calls"].as_array().unwrap().len(), 4);
    assert_eq!(cassette["calls"][1]["agent"], "worker");

    let replayer = Arc::new(ReplayProvider::from_cassette(&cassette).unwrap());
    let app = App::build_without_python(manifest()).await.unwrap().with_provider(replayer.clone()).into_arc();
    let replayed = run(&app, "boss", "go").await;
    assert_eq!(replayed.status, RunStatus::Completed);
    assert_eq!(replayed.output, live.output);
    assert_eq!(replayed.usage.tree_tokens, live.usage.tree_tokens);
    let kinds = |r: &webcortex_core::RunResult| r.steps.iter().map(|s| (s.kind, s.tool.clone())).collect::<Vec<_>>();
    assert_eq!(kinds(&replayed), kinds(&live));
    assert_eq!(replayer.stats(), json!({"recorded": 4, "used": 4, "unused": 0}));
}

#[tokio::test]
async fn replaying_against_a_changed_agent_fails_the_run_with_wc319() {
    let recorder = Arc::new(RecordingProvider::new(Arc::new(FakeProvider)));
    let app = App::build_without_python(manifest()).await.unwrap().with_provider(recorder.clone()).into_arc();
    assert_eq!(run(&app, "worker", "hello").await.status, RunStatus::Completed);
    let cassette = recorder.cassette();

    let mut changed = serde_json::to_value(manifest()).unwrap();
    changed["agents"][0]["system"] = json!("Answer in French.");
    let replayer = Arc::new(ReplayProvider::from_cassette(&cassette).unwrap());
    let app = App::build_without_python(serde_json::from_value(changed).unwrap())
        .await
        .unwrap()
        .with_provider(replayer)
        .into_arc();
    let out = run(&app, "worker", "hello").await;
    assert_eq!(out.status, RunStatus::Failed);
    assert_eq!(out.code.as_deref(), Some("WC319"));
    assert!(out.steps.last().unwrap().error.as_deref().unwrap().contains("stale"));
    assert!(out.hint.as_deref().unwrap().contains("--record"));
}

#[tokio::test]
async fn tool_response_keeps_the_body_of_a_failed_run() {
    let app = app_with(Arc::new(ScriptedProvider::error("anthropic 529: overloaded"))).await;
    let res = app.tool_response("worker", &json!({"input": "x"}), &who(), 0, None, None).await.unwrap();
    assert_eq!(res.status, 502);
    assert_eq!(res.json_value()["status"], "failed");
    assert_eq!(res.json_value()["code"], "WC304");
    assert!(app.tool_response("nope", &json!({}), &who(), 0, None, None).await.is_err());
    // The string form every other caller gets is unchanged.
    let err = app.call_tool_as("worker", &json!({"input": "x"}), &who()).await.unwrap_err();
    assert!(err.starts_with("tool worker failed with 502: "), "{err}");
}
