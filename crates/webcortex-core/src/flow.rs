//! Flows: orchestration as data, executed in Rust.
//!
//! Three shapes cover most multi-agent arrangements:
//!
//! * **Pipeline** — steps in order, each fed the previous output.
//! * **Parallel** — every branch fed the same input, run concurrently.
//! * **Route** — a cheap model classifies the input; one branch runs.
//!
//! Every step is a tool call through the same dispatcher everything else uses,
//! under a delegated principal, one nesting level deeper, against the flow's
//! shared budget. A flow is itself a tool, so flows nest.

use crate::agent::{CompletionRequest, Conversation, Message, SharedBudget, ToolSpec, recover_json};
use crate::app::App;
use crate::audit::AuditEvent;
use crate::auth::Principal;
use crate::manifest::{AgentDef, FlowDef, FlowKind, FlowStep, MergeStrategy, Op};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Clone, serde::Serialize)]
struct StepRecord {
    tool: String,
    duration_ms: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

pub async fn run(
    app: &App,
    def: &FlowDef,
    input: Value,
    caller: &Principal,
    depth: u32,
    budget: Option<Arc<SharedBudget>>,
) -> Result<Value, String> {
    let actor = caller.delegate_to_agent(&def.name, &def.scopes);
    let budget = budget.unwrap_or_else(|| SharedBudget::new(format!("flow:{}", def.name), def.token_budget));
    let run_id = uuid::Uuid::new_v4().to_string();
    let started = std::time::Instant::now();

    app.audit.record(AuditEvent::flow_started(&run_id, &def.name, &actor));

    let mut records: Vec<StepRecord> = Vec::new();
    let mut status = "completed";

    let output = match &def.kind {
        FlowKind::Pipeline { steps } => {
            let mut value = input.clone();
            for step in steps {
                let (rec, result) = call_step(app, step, &value, &input, &actor, depth, &budget).await;
                records.push(rec);
                value = result?;
                if budget.exhausted() {
                    status = "budget_exhausted";
                    break;
                }
            }
            value
        }
        FlowKind::Parallel { branches, merge } => {
            let futures = branches
                .iter()
                .map(|b| call_step(app, b, &input, &input, &actor, depth, &budget));
            let results = futures::future::join_all(futures).await;
            let mut outputs = Vec::with_capacity(results.len());
            let mut first_err = None;
            for (rec, result) in results {
                records.push(rec);
                match result {
                    Ok(v) => outputs.push(v),
                    Err(e) if first_err.is_none() => first_err = Some(e),
                    Err(_) => {}
                }
            }
            if let Some(e) = first_err {
                app.audit.record(AuditEvent::flow_finished(&run_id, &def.name, "failed", budget.used()));
                return Err(e);
            }
            if budget.exhausted() {
                status = "budget_exhausted";
            }
            match merge {
                MergeStrategy::Collect => Value::Array(outputs),
                MergeStrategy::Merge => {
                    let mut merged = serde_json::Map::new();
                    for (b, v) in branches.iter().zip(outputs) {
                        match v {
                            Value::Object(m) => merged.extend(m),
                            other => {
                                merged.insert(b.tool.clone(), other);
                            }
                        }
                    }
                    Value::Object(merged)
                }
            }
        }
        FlowKind::Route { routes, default, classify_with, classify_prompt } => {
            let labels: Vec<String> = routes.keys().cloned().collect();
            let t = std::time::Instant::now();
            let label = classify(app, def, &labels, classify_with.as_deref(), classify_prompt.as_deref(), &input, &budget).await;
            let label = match label {
                Ok(l) => {
                    records.push(StepRecord { tool: "classify".into(), duration_ms: t.elapsed().as_millis() as u64, ok: true, error: None, label: Some(l.clone()) });
                    l
                }
                Err(e) => {
                    records.push(StepRecord { tool: "classify".into(), duration_ms: t.elapsed().as_millis() as u64, ok: false, error: Some(e.clone()), label: None });
                    app.audit.record(AuditEvent::flow_finished(&run_id, &def.name, "failed", budget.used()));
                    return Err(e);
                }
            };
            let step = match routes.get(&label).or(default.as_ref()) {
                Some(s) => s,
                None => {
                    let e = format!("flow {:?}: classifier chose {label:?}, which has no route and there is no default", def.name);
                    app.audit.record(AuditEvent::flow_finished(&run_id, &def.name, "failed", budget.used()));
                    return Err(e);
                }
            };
            let (rec, result) = call_step(app, step, &input, &input, &actor, depth, &budget).await;
            records.push(rec);
            match result {
                Ok(v) => v,
                Err(e) => {
                    app.audit.record(AuditEvent::flow_finished(&run_id, &def.name, "failed", budget.used()));
                    return Err(e);
                }
            }
        }
    };

    app.audit.record(AuditEvent::flow_finished(&run_id, &def.name, status, budget.used()));
    app.ledger().count_run();
    Ok(json!({
        "flow": def.name,
        "run_id": run_id,
        "status": status,
        "output": output,
        "steps": records,
        "usage": {"tree_tokens": budget.used()},
        "duration_ms": started.elapsed().as_millis() as u64,
    }))
}

async fn call_step(
    app: &App,
    step: &FlowStep,
    incoming: &Value,
    original: &Value,
    actor: &Principal,
    depth: u32,
    budget: &Arc<SharedBudget>,
) -> (StepRecord, Result<Value, String>) {
    let started = std::time::Instant::now();
    let args = build_args(app, step, incoming, original);
    let result = app
        .call_tool_in_tree(&step.tool, &args, actor, depth + 1, Some(budget.clone()))
        .await
        .and_then(|v| unwrap_output(&step.tool, v));
    let rec = StepRecord {
        tool: step.tool.clone(),
        duration_ms: started.elapsed().as_millis() as u64,
        ok: result.is_ok(),
        error: result.as_ref().err().cloned(),
        label: None,
    };
    (rec, result)
}

/// What the next step receives. Agent and flow results are envelopes; the
/// useful part is `output`.
fn unwrap_output(tool: &str, v: Value) -> Result<Value, String> {
    let is_envelope = v.get("output").is_some() && (v.get("run_id").is_some() || v.get("flow").is_some());
    if !is_envelope {
        return Ok(v);
    }
    if v.get("status").and_then(|s| s.as_str()) == Some("awaiting_approval") {
        let id = v["pending_approval"]["approval_id"].as_str().unwrap_or("?");
        return Err(format!(
            "step {tool:?} suspended on an approval gate (approval {id}); a flow cannot wait for \
             a human, so resume that run directly or keep gated tools out of flow steps"
        ));
    }
    if v.get("status").and_then(|s| s.as_str()) == Some("failed") {
        let why = v["steps"]
            .as_array()
            .and_then(|s| s.iter().rev().find_map(|st| st.get("error").and_then(|e| e.as_str()).map(str::to_string)))
            .unwrap_or_default();
        return Err(format!("step {tool:?} failed: {why}"));
    }
    Ok(v["output"].clone())
}

fn target_kind(app: &App, tool: &str) -> &'static str {
    match app.route_for_tool(tool).map(|r| &r.op) {
        Some(Op::Agent { .. }) => "agent",
        Some(Op::Behaviour { .. }) => "behaviour",
        Some(Op::Flow { .. }) => "flow",
        _ => "route",
    }
}

pub fn build_args(app: &App, step: &FlowStep, incoming: &Value, original: &Value) -> Value {
    if let Some(template) = &step.input {
        return resolve_template(template, incoming, original);
    }
    match target_kind(app, &step.tool) {
        "agent" => match incoming {
            Value::Object(m) if m.contains_key("input") => incoming.clone(),
            Value::String(s) => json!({"input": s}),
            other => json!({"input": serde_json::to_string(other).unwrap_or_default()}),
        },
        _ => match incoming {
            Value::Object(_) => incoming.clone(),
            other => json!({"input": other}),
        },
    }
}

/// `$` is the incoming value, `$input` the flow's original input, and either
/// may be followed by a dotted path. Everything else is literal.
pub fn resolve_template(t: &Value, incoming: &Value, original: &Value) -> Value {
    match t {
        Value::String(s) => {
            if s == "$" {
                incoming.clone()
            } else if s == "$input" {
                original.clone()
            } else if let Some(path) = s.strip_prefix("$input.") {
                walk(original, path)
            } else if let Some(path) = s.strip_prefix("$.") {
                walk(incoming, path)
            } else {
                t.clone()
            }
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| resolve_template(i, incoming, original)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), resolve_template(v, incoming, original)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn walk(v: &Value, path: &str) -> Value {
    let mut cur = v;
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(m) => match m.get(seg) {
                Some(x) => x,
                None => return Value::Null,
            },
            Value::Array(a) => match seg.parse::<usize>().ok().and_then(|i| a.get(i)) {
                Some(x) => x,
                None => return Value::Null,
            },
            _ => return Value::Null,
        };
    }
    cur.clone()
}

async fn classify(
    app: &App,
    def: &FlowDef,
    labels: &[String],
    model: Option<&str>,
    prompt: Option<&str>,
    input: &Value,
    budget: &Arc<SharedBudget>,
) -> Result<String, String> {
    let rendered_input = match input {
        Value::String(s) => s.clone(),
        Value::Object(m) if m.len() == 1 && m.contains_key("input") => crate::context::render(&m["input"]),
        other => crate::context::render(other),
    };
    let labels_text = labels.join(", ");
    let prompt = match prompt {
        Some(p) => {
            let p = p.replace("{labels}", &labels_text);
            if p.contains("{input}") {
                p.replace("{input}", &rendered_input)
            } else {
                format!("{p}\n\nInput:\n{rendered_input}")
            }
        }
        None => format!(
            "Classify the input into exactly one of these labels: {labels_text}.\n\nInput:\n{rendered_input}"
        ),
    };

    let classifier = AgentDef {
        name: format!("{}:route", def.name),
        description: String::new(),
        model: model.unwrap_or("fast").to_string(),
        system: "You are a router. Read the input and choose exactly one label by calling the choose tool.".into(),
        tools: Vec::new(),
        max_steps: Some(1),
        token_budget: None,
        scopes: Vec::new(),
        temperature: None,
        max_tokens: 256,
        handoffs: Vec::new(),
        context: Vec::new(),
        cache: false,
        policy: Default::default(),
    };
    let tools = vec![ToolSpec {
        name: "choose".into(),
        description: "Choose the single best label for the input.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"label": {"type": "string", "enum": labels}},
            "required": ["label"],
        }),
    }];
    let conversation = Conversation { messages: vec![Message { role: "user".into(), content: json!(prompt) }] };
    let response = {
        let req = CompletionRequest {
            agent: &classifier,
            conversation: &conversation,
            tools: &tools,
            force_tool: Some("choose"),
            system_suffix: "",
        };
        app.provider().complete(&req).await?
    };
    let _ = budget.charge(response.total_tokens());
    app.ledger().charge(
        "flow", &def.name, &response.model,
        response.input_tokens, response.output_tokens,
        response.cache_read_tokens, response.cache_write_tokens,
    );

    let chosen = response
        .tool_calls
        .iter()
        .find(|c| c.name == "choose")
        .and_then(|c| c.arguments.get("label").and_then(|l| l.as_str()).map(str::to_string))
        .or_else(|| recover_json(&response.text).and_then(|v| v.get("label").and_then(|l| l.as_str()).map(str::to_string)))
        .or_else(|| {
            let t = response.text.trim().trim_matches('"').to_string();
            labels.iter().find(|l| l.eq_ignore_ascii_case(&t)).cloned()
        })
        .ok_or_else(|| format!("flow {:?}: the classifier did not choose a label (it said: {:?})", def.name, response.text))?;

    // Normalise case when the label matches; otherwise hand back what the
    // model said so the flow can fall through to its default route.
    Ok(labels
        .iter()
        .find(|l| l.eq_ignore_ascii_case(&chosen))
        .cloned()
        .unwrap_or(chosen))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_resolve_paths_against_incoming_and_original() {
        let incoming = json!({"a": {"b": [10, 20]}, "s": "x"});
        let original = json!({"q": "hello"});
        let t = json!({"whole": "$", "deep": "$.a.b.1", "orig": "$input.q", "lit": "$notapath", "n": 3});
        let out = resolve_template(&t, &incoming, &original);
        assert_eq!(out["whole"], incoming);
        assert_eq!(out["deep"], 20);
        assert_eq!(out["orig"], "hello");
        assert_eq!(out["lit"], "$notapath");
        assert_eq!(out["n"], 3);
        assert_eq!(resolve_template(&json!("$.missing.x"), &incoming, &original), Value::Null);
    }

    #[test]
    fn envelopes_are_unwrapped_and_suspended_runs_are_errors() {
        assert_eq!(unwrap_output("t", json!({"output": "x", "run_id": "1", "status": "completed"})).unwrap(), json!("x"));
        assert_eq!(unwrap_output("t", json!({"rows": 1})).unwrap(), json!({"rows": 1}));
        let err = unwrap_output("t", json!({"output": "", "run_id": "1", "status": "awaiting_approval",
                                            "pending_approval": {"approval_id": "abc"}})).unwrap_err();
        assert!(err.contains("abc"));
    }
}
