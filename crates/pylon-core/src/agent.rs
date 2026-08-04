//! The agent runtime.
//!
//! What makes this different from writing a tool loop by hand:
//!
//! * **Tools are the app's own routes**, dispatched in-process. No HTTP hop, no
//!   second schema, no drift.
//! * **Authority is delegated, never granted.** A run executes as
//!   `caller.delegate_to_agent(...)`, which can only ever hold a subset of the
//!   caller's scopes. See [`crate::auth::Principal::delegate_to_agent`].
//! * **Budgets are enforced by the runtime.** Steps and tokens are checked
//!   before each provider call, so a looping model costs a bounded amount rather
//!   than whatever the provider is willing to sell.
//! * **Dangerous tools stop and wait for a human.** A route marked
//!   `approval="required"` suspends the run and emits an approval request
//!   instead of executing.
//! * **Every step is audited**, including the ones that were refused.

use crate::app::App;
use crate::audit::{AuditEvent, AuditSink};
use crate::auth::Principal;
use crate::manifest::{AgentDef, Approval};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

pub mod provider;

pub use provider::{ModelProvider, ProviderResponse, StopReason, ToolCall};

/// The outcome of an agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub run_id: String,
    pub agent: String,
    pub status: RunStatus,
    /// Final assistant text, if the run produced any.
    pub output: String,
    pub steps: Vec<Step>,
    pub usage: Usage,
    /// Set when `status` is `AwaitingApproval`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_approval: Option<PendingApproval>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Completed,
    /// Hit `max_steps` before the model stopped asking for tools.
    StepLimit,
    /// Hit the token budget.
    BudgetExhausted,
    /// Suspended on an approval gate; resumable.
    AwaitingApproval,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub index: u32,
    pub kind: StepKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Model,
    ToolCall,
    /// The runtime refused the call — missing scope, unknown tool, or a gate.
    ToolRefused,
    ApprovalRequested,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub steps: u32,
    pub tool_calls: u32,
}

impl Usage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    pub approval_id: String,
    pub tool: String,
    pub arguments: Value,
    pub reason: String,
}

/// Conversation state, carried across steps and across suspensions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Conversation {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Value,
}

pub struct AgentRuntime {
    provider: Arc<dyn ModelProvider>,
    audit: Arc<dyn AuditSink>,
}

impl AgentRuntime {
    pub fn new(provider: Arc<dyn ModelProvider>, audit: Arc<dyn AuditSink>) -> Self {
        Self { provider, audit }
    }

    /// Run an agent to completion, a limit, or an approval gate.
    pub async fn run(
        &self,
        app: &App,
        def: &AgentDef,
        caller: &Principal,
        input: &str,
    ) -> RunResult {
        let mut conversation = Conversation::default();
        conversation.messages.push(Message {
            role: "user".into(),
            content: json!(input),
        });
        self.resume(app, def, caller, conversation, Usage::default(), Vec::new())
            .await
    }

    /// Continue a run from existing state. Used both for the initial run and
    /// for resuming after a human approves a gated tool call.
    pub async fn resume(
        &self,
        app: &App,
        def: &AgentDef,
        caller: &Principal,
        mut conversation: Conversation,
        mut usage: Usage,
        mut steps: Vec<Step>,
    ) -> RunResult {
        let run_id = uuid::Uuid::new_v4().to_string();
        // The single most important line in this file: the run executes with
        // delegated authority, which is always a subset of the caller's.
        let actor = caller.delegate_to_agent(&def.name, &def.scopes);

        let max_steps = def.max_steps.unwrap_or(12);
        let tools = self.tool_specs(app, def);

        self.audit.record(AuditEvent::agent_started(
            &run_id, &def.name, &actor, &tools,
        ));

        let mut output = String::new();

        loop {
            if usage.steps >= max_steps {
                return self.finish(run_id, def, RunStatus::StepLimit, output, steps, usage, None);
            }
            if let Some(budget) = def.token_budget {
                if usage.total_tokens() >= budget {
                    return self.finish(
                        run_id, def, RunStatus::BudgetExhausted, output, steps, usage, None,
                    );
                }
            }

            let started = std::time::Instant::now();
            let response = match self
                .provider
                .complete(def, &conversation, &tools)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    steps.push(Step {
                        index: usage.steps,
                        kind: StepKind::Model,
                        tool: None,
                        arguments: None,
                        result: None,
                        error: Some(e.clone()),
                        duration_ms: started.elapsed().as_millis() as u64,
                    });
                    self.audit.record(AuditEvent::agent_failed(&run_id, &def.name, &e));
                    return self.finish(run_id, def, RunStatus::Failed, output, steps, usage, None);
                }
            };

            usage.steps += 1;
            usage.input_tokens += response.input_tokens;
            usage.output_tokens += response.output_tokens;

            steps.push(Step {
                index: usage.steps - 1,
                kind: StepKind::Model,
                tool: None,
                arguments: None,
                result: None,
                error: None,
                duration_ms: started.elapsed().as_millis() as u64,
            });

            if !response.text.is_empty() {
                output = response.text.clone();
            }
            conversation.messages.push(Message {
                role: "assistant".into(),
                content: response.raw_content.clone(),
            });

            if response.stop_reason != StopReason::ToolUse || response.tool_calls.is_empty() {
                return self.finish(run_id, def, RunStatus::Completed, output, steps, usage, None);
            }

            // Execute every requested tool call, in order.
            let mut results = Vec::new();
            for call in &response.tool_calls {
                let started = std::time::Instant::now();

                // 1. Is the tool one this agent was given?
                if !def.tools.iter().any(|t| t == &call.name) {
                    let msg = format!(
                        "tool {:?} is not available to this agent; available: {}",
                        call.name,
                        def.tools.join(", ")
                    );
                    steps.push(refused(usage.steps, call, &msg, started));
                    self.audit
                        .record(AuditEvent::tool_refused(&run_id, &actor, &call.name, &msg));
                    results.push(tool_result(&call.id, &json!({"error": msg}), true));
                    continue;
                }

                // 2. Does it need a human?
                let route_approval = app
                    .route_for_tool(&call.name)
                    .map(|r| r.approval)
                    .unwrap_or(Approval::Never);
                if route_approval == Approval::Required {
                    let approval_id = uuid::Uuid::new_v4().to_string();
                    let reason = format!(
                        "tool {:?} is gated and requires human approval before it runs",
                        call.name
                    );
                    steps.push(Step {
                        index: usage.steps,
                        kind: StepKind::ApprovalRequested,
                        tool: Some(call.name.clone()),
                        arguments: Some(call.arguments.clone()),
                        result: None,
                        error: None,
                        duration_ms: 0,
                    });
                    self.audit.record(AuditEvent::approval_requested(
                        &run_id, &actor, &call.name, &call.arguments, &approval_id,
                    ));
                    return self.finish(
                        run_id, def, RunStatus::AwaitingApproval, output, steps, usage,
                        Some(PendingApproval {
                            approval_id,
                            tool: call.name.clone(),
                            arguments: call.arguments.clone(),
                            reason,
                        }),
                    );
                }

                // 3. Dispatch in-process, under delegated authority. Scope
                //    enforcement happens inside `call_tool`, so an agent cannot
                //    reach a route its caller could not.
                usage.tool_calls += 1;
                match app
                    .call_tool_as(&call.name, &call.arguments, &actor)
                    .await
                {
                    Ok(value) => {
                        steps.push(Step {
                            index: usage.steps,
                            kind: StepKind::ToolCall,
                            tool: Some(call.name.clone()),
                            arguments: Some(call.arguments.clone()),
                            result: Some(value.clone()),
                            error: None,
                            duration_ms: started.elapsed().as_millis() as u64,
                        });
                        self.audit.record(AuditEvent::tool_called(
                            &run_id, &actor, &call.name, &call.arguments, true,
                            started.elapsed().as_millis() as u64,
                        ));
                        results.push(tool_result(&call.id, &value, false));
                    }
                    Err(e) => {
                        steps.push(Step {
                            index: usage.steps,
                            kind: StepKind::ToolCall,
                            tool: Some(call.name.clone()),
                            arguments: Some(call.arguments.clone()),
                            result: None,
                            error: Some(e.clone()),
                            duration_ms: started.elapsed().as_millis() as u64,
                        });
                        self.audit.record(AuditEvent::tool_called(
                            &run_id, &actor, &call.name, &call.arguments, false,
                            started.elapsed().as_millis() as u64,
                        ));
                        // A failed tool is information the model can act on, not
                        // a reason to abort the run.
                        results.push(tool_result(&call.id, &json!({"error": e}), true));
                    }
                }
            }

            conversation.messages.push(Message {
                role: "user".into(),
                content: Value::Array(results),
            });
        }
    }

    fn tool_specs(&self, app: &App, def: &AgentDef) -> Vec<ToolSpec> {
        def.tools
            .iter()
            .filter_map(|name| {
                app.route_for_tool(name).map(|r| ToolSpec {
                    name: name.clone(),
                    description: if r.description.is_empty() {
                        r.summary.clone()
                    } else {
                        r.description.clone()
                    },
                    input_schema: r
                        .input_schema
                        .clone()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        run_id: String,
        def: &AgentDef,
        status: RunStatus,
        output: String,
        steps: Vec<Step>,
        usage: Usage,
        pending: Option<PendingApproval>,
    ) -> RunResult {
        self.audit
            .record(AuditEvent::agent_finished(&run_id, &def.name, status, &usage));
        RunResult {
            run_id,
            agent: def.name.clone(),
            status,
            output,
            steps,
            usage,
            pending_approval: pending,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

fn refused(index: u32, call: &ToolCall, msg: &str, started: std::time::Instant) -> Step {
    Step {
        index,
        kind: StepKind::ToolRefused,
        tool: Some(call.name.clone()),
        arguments: Some(call.arguments.clone()),
        result: None,
        error: Some(msg.to_string()),
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

fn tool_result(id: &str, content: &Value, is_error: bool) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": id,
        "content": serde_json::to_string(content).unwrap_or_else(|_| content.to_string()),
        "is_error": is_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::MemoryAudit;
    use crate::manifest::Manifest;
    use provider::ScriptedProvider;

    fn manifest() -> Manifest {
        serde_json::from_value(json!({
            "name": "t",
            "routes": [
                {"id": 0, "method": "GET", "path": "/safe",
                 "op": {"kind": "static", "body": {"ok": true}},
                 "tool": {"expose": true, "name": "safe", "read_only": true}},
                {"id": 1, "method": "POST", "path": "/danger",
                 "op": {"kind": "static", "body": {"done": true}},
                 "tool": {"expose": true, "name": "danger"},
                 "approval": "required"},
                {"id": 2, "method": "GET", "path": "/admin",
                 "op": {"kind": "static", "body": {"secret": 1}},
                 "tool": {"expose": true, "name": "admin_only"},
                 "scopes": ["admin"]}
            ],
            "agents": [{
                "name": "a", "model": "test",
                "tools": ["safe", "danger", "admin_only"], "max_steps": 5
            }]
        }))
        .expect("test manifest")
    }

    async fn app() -> App {
        App::build_without_python(manifest()).await.expect("app builds")
    }

    fn runtime(provider: ScriptedProvider) -> (AgentRuntime, Arc<MemoryAudit>) {
        let audit = Arc::new(MemoryAudit::default());
        (AgentRuntime::new(Arc::new(provider), audit.clone()), audit)
    }

    fn caller(scopes: &[&str]) -> Principal {
        Principal {
            id: "u1".into(),
            kind: crate::auth::PrincipalKind::ApiKey,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            claims: Default::default(),
        }
    }

    #[tokio::test]
    async fn a_plain_answer_completes_in_one_step() {
        let app = app().await;
        let (rt, _) = runtime(ScriptedProvider::text("hello"));
        let def = app.manifest.agents[0].clone();
        let out = rt.run(&app, &def, &caller(&[]), "hi").await;
        assert_eq!(out.status, RunStatus::Completed);
        assert_eq!(out.output, "hello");
        assert_eq!(out.usage.tool_calls, 0);
    }

    #[tokio::test]
    async fn a_tool_call_is_dispatched_in_process_then_the_run_completes() {
        let app = app().await;
        let (rt, audit) = runtime(ScriptedProvider::tool_then_text("safe", json!({}), "done"));
        let def = app.manifest.agents[0].clone();
        let out = rt.run(&app, &def, &caller(&[]), "go").await;

        assert_eq!(out.status, RunStatus::Completed);
        assert_eq!(out.usage.tool_calls, 1);
        let call = out.steps.iter().find(|s| s.kind == StepKind::ToolCall).unwrap();
        assert_eq!(call.result, Some(json!({"ok": true})));
        assert!(audit.contains("tool_called"));
    }

    #[tokio::test]
    async fn a_gated_tool_suspends_the_run_instead_of_executing() {
        let app = app().await;
        let (rt, audit) = runtime(ScriptedProvider::tool_then_text("danger", json!({}), "never"));
        let def = app.manifest.agents[0].clone();
        let out = rt.run(&app, &def, &caller(&["*"]), "go").await;

        assert_eq!(out.status, RunStatus::AwaitingApproval);
        let pending = out.pending_approval.expect("should be awaiting approval");
        assert_eq!(pending.tool, "danger");
        assert!(audit.contains("approval_requested"));
        // Crucially, the tool did NOT run.
        assert!(!out.steps.iter().any(|s| s.kind == StepKind::ToolCall));
    }

    #[tokio::test]
    async fn an_agent_cannot_reach_a_scope_its_caller_lacks() {
        let app = app().await;
        let (rt, _) = runtime(ScriptedProvider::tool_then_text("admin_only", json!({}), "x"));
        let def = app.manifest.agents[0].clone();
        // Caller holds no scopes, so the delegated actor holds none either.
        let out = rt.run(&app, &def, &caller(&[]), "go").await;

        let call = out.steps.iter().find(|s| s.kind == StepKind::ToolCall).unwrap();
        assert!(call.error.is_some(), "admin route should have been refused");
        assert!(call.error.as_ref().unwrap().contains("403"));
    }

    #[tokio::test]
    async fn an_agent_reaches_a_scoped_route_when_the_caller_holds_the_scope() {
        let app = app().await;
        let (rt, _) = runtime(ScriptedProvider::tool_then_text("admin_only", json!({}), "x"));
        let def = app.manifest.agents[0].clone();
        let out = rt.run(&app, &def, &caller(&["admin"]), "go").await;
        let call = out.steps.iter().find(|s| s.kind == StepKind::ToolCall).unwrap();
        assert_eq!(call.result, Some(json!({"secret": 1})));
    }

    #[tokio::test]
    async fn a_tool_outside_the_agents_list_is_refused_not_dispatched() {
        let app = app().await;
        let (rt, audit) = runtime(ScriptedProvider::tool_then_text("nonexistent", json!({}), "x"));
        let mut def = app.manifest.agents[0].clone();
        def.tools = vec!["safe".into()];
        let out = rt.run(&app, &def, &caller(&["*"]), "go").await;
        assert!(out.steps.iter().any(|s| s.kind == StepKind::ToolRefused));
        assert!(audit.contains("tool_refused"));
    }

    #[tokio::test]
    async fn a_looping_model_is_stopped_by_the_step_limit() {
        let app = app().await;
        // Always asks for a tool, never concludes.
        let (rt, _) = runtime(ScriptedProvider::always_tool("safe", json!({})));
        let mut def = app.manifest.agents[0].clone();
        def.max_steps = Some(3);
        let out = rt.run(&app, &def, &caller(&[]), "go").await;
        assert_eq!(out.status, RunStatus::StepLimit);
        assert_eq!(out.usage.steps, 3, "must stop exactly at the limit");
    }

    #[tokio::test]
    async fn the_token_budget_is_enforced_by_the_runtime() {
        let app = app().await;
        let (rt, _) = runtime(ScriptedProvider::always_tool("safe", json!({})).with_tokens(100, 100));
        let mut def = app.manifest.agents[0].clone();
        def.max_steps = Some(50);
        def.token_budget = Some(500);
        let out = rt.run(&app, &def, &caller(&[]), "go").await;
        assert_eq!(out.status, RunStatus::BudgetExhausted);
        assert!(out.usage.total_tokens() >= 500 && out.usage.total_tokens() < 1000);
    }

    #[tokio::test]
    async fn a_provider_error_fails_the_run_without_panicking() {
        let app = app().await;
        let (rt, audit) = runtime(ScriptedProvider::error("upstream exploded"));
        let def = app.manifest.agents[0].clone();
        let out = rt.run(&app, &def, &caller(&[]), "go").await;
        assert_eq!(out.status, RunStatus::Failed);
        assert!(audit.contains("agent_failed"));
    }

    #[tokio::test]
    async fn every_run_is_audited_from_start_to_finish() {
        let app = app().await;
        let (rt, audit) = runtime(ScriptedProvider::text("hi"));
        let def = app.manifest.agents[0].clone();
        rt.run(&app, &def, &caller(&[]), "go").await;
        assert!(audit.contains("agent_started"));
        assert!(audit.contains("agent_finished"));
    }
}
