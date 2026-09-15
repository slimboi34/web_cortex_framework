//! The agent runtime.
//!
//! What makes this different from writing a tool loop by hand:
//!
//! * **Tools are the app's own routes**, dispatched in-process. No HTTP hop, no
//!   second schema, no drift. Other agents, behaviours and flows are tools too,
//!   which is all a supervisor/worker arrangement needs.
//! * **Authority is delegated, never granted.** A run executes as
//!   `caller.delegate_to_agent(...)`, which can only ever hold a subset of the
//!   caller's scopes — and a handoff can only shrink it further.
//! * **Budgets are enforced by the runtime, and they compose.** A
//!   [`SharedBudget`] travels with the request tree, so an agent that calls an
//!   agent that calls a behaviour spends against one ceiling.
//! * **Context is bounded.** Tool results are capped before the model sees
//!   them, and a conversation that outgrows its window is compacted.
//! * **Dangerous tools stop and wait for a human**, and the run can be resumed
//!   once the human has decided — including the rest of the turn it was in.
//! * **Every step is audited and every token is charged to a ledger.**

use crate::app::App;
use crate::audit::{AuditEvent, AuditSink};
use crate::auth::Principal;
use crate::manifest::{AgentDef, Approval};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod provider;
pub mod registry;
pub mod session;

pub use provider::{
    CompletionRequest, ModelProvider, ProviderResponse, StopReason, ToolCall, recover_json,
};
pub use registry::ProviderRegistry;
pub use session::SessionStore;

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// The outcome of an agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub run_id: String,
    /// The agent that produced the final answer — after any handoffs.
    pub agent: String,
    /// Every agent the run passed through, in order.
    pub path: Vec<String>,
    pub status: RunStatus,
    /// Final assistant text, if the run produced any.
    pub output: String,
    pub steps: Vec<Step>,
    pub usage: Usage,
    /// Set when `status` is `AwaitingApproval`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_approval: Option<PendingApproval>,
    /// Echoed when the run belongs to a session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Completed,
    /// The last response was cut off at `max_tokens`: `output` may be
    /// incomplete, and any tool call in that response was not run.
    MaxTokens,
    /// Hit `max_steps` before the model stopped asking for tools.
    StepLimit,
    /// Hit the token budget — its own, or the request tree's.
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
    /// A human declined a gated call; the model was told so and continued.
    ApprovalDenied,
    /// The conversation was handed to another agent.
    Handoff,
    /// Older turns were summarised to stay inside the context window.
    Compaction,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    pub steps: u32,
    pub tool_calls: u32,
    #[serde(default)]
    pub handoffs: u32,
    #[serde(default)]
    pub compactions: u32,
    /// Tokens spent by the whole request tree this run belongs to, including
    /// nested agents, behaviours and flows. Equal to `total_tokens()` for a
    /// run that called nothing model-backed.
    #[serde(default)]
    pub tree_tokens: u64,
}

impl Usage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    pub approval_id: String,
    pub tool: String,
    pub arguments: Value,
    pub reason: String,
}

/// Conversation state, carried across steps, suspensions and sessions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Conversation {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

// ---------------------------------------------------------------------------
// Shared budget
// ---------------------------------------------------------------------------

/// A token ceiling shared by every model-backed thing in one request tree.
///
/// The security review of v0.3 found that per-frame limits do not bound a
/// request: each nested invocation had a fresh budget, so recursion was
/// unbounded even though every frame was capped. A budget that travels *with
/// the request* is the fix, generalised. Steps stay per-run; tokens are what
/// cost money, so tokens are what is shared.
///
/// Only the outermost agent, behaviour or flow creates one, from its own
/// `token_budget`. Every nested run charges that same `Arc`, so spend in any
/// branch reduces what is left for all the others. A nested agent's or
/// behaviour's own `token_budget` still caps the model calls it makes itself,
/// but sets no smaller ceiling for the runs it starts; a nested flow's
/// `token_budget` is not consulted at all.
#[derive(Debug)]
pub struct SharedBudget {
    pub label: String,
    pub max_tokens: Option<u64>,
    used: AtomicU64,
}

impl SharedBudget {
    pub fn new(label: impl Into<String>, max_tokens: Option<u64>) -> Arc<Self> {
        Arc::new(Self { label: label.into(), max_tokens, used: AtomicU64::new(0) })
    }

    /// Record spend. Returns `Err` once the ceiling is passed; the tokens are
    /// still counted, because they were still spent. The counter saturates:
    /// token counts are whatever an upstream's `usage` field says, and a
    /// counter that wrapped would reopen a budget that was already exhausted.
    pub fn charge(&self, tokens: u64) -> Result<(), String> {
        let previous = match self
            .used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |u| Some(u.saturating_add(tokens)))
        {
            Ok(p) | Err(p) => p,
        };
        let total = previous.saturating_add(tokens);
        match self.max_tokens {
            Some(max) if total > max => Err(format!(
                "the request tree rooted at {:?} exhausted its shared token budget of {max} \
                 ({total} spent)",
                self.label
            )),
            _ => Ok(()),
        }
    }

    pub fn exhausted(&self) -> bool {
        matches!(self.max_tokens, Some(max) if self.used.load(Ordering::SeqCst) >= max)
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::SeqCst)
    }
}

/// How a run is started: where it sits in the request tree and whether it
/// continues a session.
#[derive(Default, Clone)]
pub struct RunOptions {
    pub depth: u32,
    pub budget: Option<Arc<SharedBudget>>,
    /// Client-visible session id. The store key also includes the principal.
    pub session_id: Option<String>,
    /// Discard any existing session history before this turn.
    pub reset_session: bool,
}

// ---------------------------------------------------------------------------
// Suspended runs
// ---------------------------------------------------------------------------

/// Everything needed to continue a run after a human decides. Held in memory
/// by the app until `approval_ttl_secs` passes.
#[derive(Clone)]
pub struct SuspendedRun {
    pub approval_id: String,
    pub created: std::time::Instant,
    state: RunState,
    /// The tool calls of the turn that was interrupted.
    calls: Vec<ToolCall>,
    /// Index of the gated call within `calls`.
    gated: usize,
    /// Results already produced for calls before the gated one.
    results: Vec<Value>,
}

impl SuspendedRun {
    pub fn summary(&self) -> Value {
        json!({
            "approval_id": self.approval_id,
            "run_id": self.state.run_id,
            "agent": self.state.def.name,
            "caller": self.state.caller.id,
            "tool": self.calls[self.gated].name,
            "arguments": self.calls[self.gated].arguments,
            "age_secs": self.created.elapsed().as_secs(),
            "session_id": self.state.session_id,
        })
    }

    pub fn caller(&self) -> &Principal {
        &self.state.caller
    }
}

#[derive(Clone)]
struct RunState {
    run_id: String,
    /// The agent currently in control. Changes on handoff.
    def: AgentDef,
    /// The agent that started the run. Its limits govern the whole run.
    entry: AgentDef,
    /// Who started the run. A handoff re-delegates from here.
    caller: Principal,
    /// The authority tool calls run under: `caller` delegated to `def`, and
    /// only ever narrowed by a handoff.
    actor: Principal,
    conversation: Conversation,
    usage: Usage,
    steps: Vec<Step>,
    output: String,
    path: Vec<String>,
    /// This run's nesting level; its tool calls dispatch at `depth + 1`.
    depth: u32,
    /// The request tree's budget, shared with every nested run.
    budget: Arc<SharedBudget>,
    /// Resolved context blocks and handoff guidance, appended to the system
    /// prompt. Re-resolved for the new agent on a handoff.
    system_suffix: String,
    /// Input size of the last provider call: the real measure of context.
    last_input_tokens: u64,
    /// Consecutive failed compactions, and the step the next attempt waits
    /// for. A summariser that keeps failing would otherwise be called, and
    /// charged, on every remaining step.
    compaction_failures: u32,
    compaction_retry_at: u32,
    /// The client's session id, echoed in the result.
    session_id: Option<String>,
    /// Where the conversation is saved: the session id scoped by the entry
    /// agent and the caller, so another principal's id cannot reach it.
    session_key: Option<String>,
}

enum CallsOutcome {
    Done(Vec<Value>),
    Suspended { gated: usize, results: Vec<Value> },
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

pub struct AgentRuntime {
    provider: Arc<dyn ModelProvider>,
    audit: Arc<dyn AuditSink>,
}

impl AgentRuntime {
    pub fn new(provider: Arc<dyn ModelProvider>, audit: Arc<dyn AuditSink>) -> Self {
        Self { provider, audit }
    }

    pub fn provider(&self) -> &Arc<dyn ModelProvider> {
        &self.provider
    }

    /// Run an agent to completion, a limit, or an approval gate.
    pub async fn run(
        &self,
        app: &App,
        def: &AgentDef,
        caller: &Principal,
        input: &str,
        opts: RunOptions,
    ) -> RunResult {
        let run_id = uuid::Uuid::new_v4().to_string();
        // The single most important line in this file: the run executes with
        // delegated authority, which is always a subset of the caller's.
        let actor = caller.delegate_to_agent(&def.name, &def.scopes);

        let session_key = opts
            .session_id
            .as_ref()
            .map(|sid| SessionStore::key(&def.name, &caller.id, sid));
        let mut conversation = match (&session_key, opts.reset_session) {
            (Some(key), false) => app.sessions().load(key).unwrap_or_default(),
            (Some(key), true) => {
                app.sessions().forget(key);
                Conversation::default()
            }
            _ => Conversation::default(),
        };
        conversation.messages.push(Message { role: "user".into(), content: json!(input) });

        let budget = opts
            .budget
            .unwrap_or_else(|| SharedBudget::new(format!("agent:{}", def.name), def.token_budget));

        let system_suffix = match self.system_suffix(app, def, &actor, input).await {
            Ok(s) => s,
            Err(e) => {
                let ev = AuditEvent::agent_failed(&run_id, &def.name, &e);
                self.audit.record(ev);
                return RunResult {
                    run_id,
                    agent: def.name.clone(),
                    path: vec![def.name.clone()],
                    status: RunStatus::Failed,
                    output: String::new(),
                    steps: vec![Step {
                        index: 0,
                        kind: StepKind::Model,
                        tool: None,
                        arguments: None,
                        result: None,
                        error: Some(e),
                        duration_ms: 0,
                    }],
                    usage: Usage::default(),
                    pending_approval: None,
                    session_id: opts.session_id,
                };
            }
        };

        let state = RunState {
            run_id,
            def: def.clone(),
            entry: def.clone(),
            caller: caller.clone(),
            actor,
            conversation,
            usage: Usage::default(),
            steps: Vec::new(),
            output: String::new(),
            path: vec![def.name.clone()],
            depth: opts.depth,
            budget,
            system_suffix,
            last_input_tokens: 0,
            compaction_failures: 0,
            compaction_retry_at: 0,
            session_id: opts.session_id,
            session_key,
        };

        let tools = self.tool_specs(app, &state.def);
        self.audit.record(AuditEvent::agent_started(
            &state.run_id, &state.def.name, &state.actor, &tools,
        ));

        self.drive(app, state).await
    }

    /// Continue a suspended run after a human has approved or denied the
    /// gated call. The rest of the interrupted turn is executed too, so a model
    /// that asked for three tools and hit a gate on the second still gets all
    /// three results back.
    pub async fn resume_approval(
        &self,
        app: &App,
        suspended: SuspendedRun,
        approve: bool,
        note: &str,
    ) -> RunResult {
        let SuspendedRun { state: mut st, calls, gated, mut results, approval_id, .. } = suspended;
        let call = &calls[gated];

        if approve {
            self.audit.record(AuditEvent::approval_decided(
                &st.run_id, &st.actor, &call.name, &approval_id, true, note,
            ));
            results.push(self.dispatch_call(app, &mut st, call).await);
        } else {
            self.audit.record(AuditEvent::approval_decided(
                &st.run_id, &st.actor, &call.name, &approval_id, false, note,
            ));
            let msg = if note.is_empty() {
                format!("a human declined to allow {:?}; do not retry it", call.name)
            } else {
                format!("a human declined to allow {:?}: {note}. Do not retry it", call.name)
            };
            st.steps.push(Step {
                index: st.usage.steps,
                kind: StepKind::ApprovalDenied,
                tool: Some(call.name.clone()),
                arguments: Some(call.arguments.clone()),
                result: None,
                error: Some(msg.clone()),
                duration_ms: 0,
            });
            results.push(tool_result(&call.id, &json!({"error": msg}), true));
        }

        match self.execute_calls(app, &mut st, &calls, gated + 1, results).await {
            CallsOutcome::Done(results) => {
                st.conversation.messages.push(Message { role: "user".into(), content: Value::Array(results) });
                self.drive(app, st).await
            }
            CallsOutcome::Suspended { gated, results } => self.suspend(app, st, calls, gated, results),
        }
    }

    /// The main loop. Everything mutable lives in `st`.
    async fn drive(&self, app: &App, mut st: RunState) -> RunResult {
        let max_steps = st.entry.max_steps.unwrap_or(12);

        loop {
            if st.usage.steps >= max_steps {
                return self.finish(app, st, RunStatus::StepLimit, None);
            }
            if let Some(budget) = st.entry.token_budget {
                if st.usage.total_tokens() >= budget {
                    return self.finish(app, st, RunStatus::BudgetExhausted, None);
                }
            }
            if st.budget.exhausted() {
                return self.finish(app, st, RunStatus::BudgetExhausted, None);
            }

            if st.usage.steps >= st.compaction_retry_at {
                let compactions = st.usage.compactions;
                match self.maybe_compact(app, &mut st).await {
                    Ok(()) if st.usage.compactions > compactions => st.compaction_failures = 0,
                    Ok(()) => {}
                    Err(e) => {
                        // A failed compaction is not a reason to abandon the run;
                        // the next provider call may still fit. Recorded so it is
                        // visible, then retried after 2, 4, 8… steps rather than
                        // on every one.
                        st.compaction_failures = st.compaction_failures.saturating_add(1);
                        let wait = 1u32.checked_shl(st.compaction_failures).unwrap_or(u32::MAX);
                        st.compaction_retry_at = st.usage.steps.saturating_add(wait);
                        st.steps.push(Step {
                            index: st.usage.steps,
                            kind: StepKind::Compaction,
                            tool: None,
                            arguments: None,
                            result: None,
                            error: Some(e),
                            duration_ms: 0,
                        });
                    }
                }
            }

            let tools = self.tool_specs(app, &st.def);
            let started = std::time::Instant::now();
            let response = {
                let req = CompletionRequest {
                    agent: &st.def,
                    conversation: &st.conversation,
                    tools: &tools,
                    force_tool: None,
                    system_suffix: &st.system_suffix,
                };
                self.provider.complete(&req).await
            };
            let response = match response {
                Ok(r) => r,
                Err(e) => {
                    st.steps.push(Step {
                        index: st.usage.steps,
                        kind: StepKind::Model,
                        tool: None,
                        arguments: None,
                        result: None,
                        error: Some(e.clone()),
                        duration_ms: started.elapsed().as_millis() as u64,
                    });
                    self.audit.record(AuditEvent::agent_failed(&st.run_id, &st.def.name, &e));
                    return self.finish(app, st, RunStatus::Failed, None);
                }
            };

            self.charge(app, &mut st, "agent", &response);
            st.usage.steps += 1;
            st.last_input_tokens = response
                .input_tokens
                .saturating_add(response.cache_read_tokens)
                .saturating_add(response.cache_write_tokens);

            st.steps.push(Step {
                index: st.usage.steps - 1,
                kind: StepKind::Model,
                tool: None,
                arguments: None,
                result: Some(json!({"model": response.model, "input_tokens": st.last_input_tokens,
                                    "output_tokens": response.output_tokens})),
                error: None,
                duration_ms: started.elapsed().as_millis() as u64,
            });

            if !response.text.is_empty() {
                st.output = response.text.clone();
            }
            st.conversation.messages.push(Message {
                role: "assistant".into(),
                content: response.raw_content.clone(),
            });

            // A truncated response may hold a half-written tool call; running
            // it would act on arguments the model never finished.
            if response.stop_reason == StopReason::MaxTokens {
                return self.finish(app, st, RunStatus::MaxTokens, None);
            }
            if response.stop_reason != StopReason::ToolUse || response.tool_calls.is_empty() {
                return self.finish(app, st, RunStatus::Completed, None);
            }

            let calls = response.tool_calls;
            match self.execute_calls(app, &mut st, &calls, 0, Vec::new()).await {
                CallsOutcome::Done(results) => {
                    st.conversation.messages.push(Message { role: "user".into(), content: Value::Array(results) });
                }
                CallsOutcome::Suspended { gated, results } => {
                    return self.suspend(app, st, calls, gated, results);
                }
            }
        }
    }

    /// Execute the tool calls of one model turn, starting at `start`, in order.
    async fn execute_calls(
        &self,
        app: &App,
        st: &mut RunState,
        calls: &[ToolCall],
        start: usize,
        mut results: Vec<Value>,
    ) -> CallsOutcome {
        for (i, call) in calls.iter().enumerate().skip(start) {
            let started = std::time::Instant::now();

            // 0. A handoff: swap the agent in control and carry on.
            if let Some(target) = call.name.strip_prefix("transfer_to_") {
                if st.def.handoffs.iter().any(|h| h == target) {
                    match self.handoff(app, st, target, &call.arguments).await {
                        Ok(msg) => {
                            st.steps.push(Step {
                                index: st.usage.steps,
                                kind: StepKind::Handoff,
                                tool: Some(call.name.clone()),
                                arguments: Some(call.arguments.clone()),
                                result: Some(json!({"agent": target})),
                                error: None,
                                duration_ms: started.elapsed().as_millis() as u64,
                            });
                            results.push(tool_result(&call.id, &json!({"handed_off_to": target, "note": msg}), false));
                        }
                        Err(e) => {
                            st.steps.push(refused(st.usage.steps, call, &e, started));
                            results.push(tool_result(&call.id, &json!({"error": e}), true));
                        }
                    }
                    continue;
                }
            }

            // 1. Is the tool one this agent was given?
            if !st.def.tools.iter().any(|t| t == &call.name) {
                let msg = format!(
                    "tool {:?} is not available to this agent; available: {}",
                    call.name,
                    st.def.tools.join(", ")
                );
                st.steps.push(refused(st.usage.steps, call, &msg, started));
                self.audit
                    .record(AuditEvent::tool_refused(&st.run_id, &st.actor, &call.name, &msg));
                results.push(tool_result(&call.id, &json!({"error": msg}), true));
                continue;
            }

            // 2. Does it need a human?
            let route_approval = app
                .route_for_tool(&call.name)
                .map(|r| r.approval)
                .unwrap_or(Approval::Never);
            if route_approval == Approval::Required {
                return CallsOutcome::Suspended { gated: i, results };
            }

            // 3. Dispatch in-process, under delegated authority and the shared
            //    budget. Scope enforcement happens inside `dispatch`, so an
            //    agent cannot reach a route its caller could not.
            results.push(self.dispatch_call(app, st, call).await);
        }
        CallsOutcome::Done(results)
    }

    /// Run one admitted tool call, record its step and audit event, and return
    /// the `tool_result` the model sees, bounded. Shared by a fresh turn and an
    /// approved resume, so the two cannot drift. A failed tool is information
    /// the model can act on, not a reason to abort the run.
    async fn dispatch_call(&self, app: &App, st: &mut RunState, call: &ToolCall) -> Value {
        let started = std::time::Instant::now();
        st.usage.tool_calls += 1;
        let outcome = app
            .call_tool_in_tree(&call.name, &call.arguments, &st.actor, st.depth + 1, Some(st.budget.clone()))
            .await;
        let duration_ms = started.elapsed().as_millis() as u64;
        self.audit.record(AuditEvent::tool_called(
            &st.run_id, &st.actor, &call.name, &call.arguments, outcome.is_ok(), duration_ms,
        ));
        let (result, error, shown, is_error) = match outcome {
            Ok(value) => {
                let shown = bounded(&value, st.entry.policy.max_tool_result_bytes);
                (Some(value), None, shown, false)
            }
            Err(e) => (None, Some(e.clone()), json!({"error": e}), true),
        };
        st.steps.push(Step {
            index: st.usage.steps,
            kind: StepKind::ToolCall,
            tool: Some(call.name.clone()),
            arguments: Some(call.arguments.clone()),
            result,
            error,
            duration_ms,
        });
        tool_result(&call.id, &shown, is_error)
    }

    async fn handoff(
        &self,
        app: &App,
        st: &mut RunState,
        target: &str,
        arguments: &Value,
    ) -> Result<String, String> {
        let next = app
            .agent(target)
            .cloned()
            .ok_or_else(|| format!("handoff target {target:?} is not a declared agent"))?;

        // Authority can only shrink along a chain of handoffs: the new actor
        // is delegated from the original caller *and* filtered by what the
        // current actor already held.
        let mut actor = st.caller.delegate_to_agent(&next.name, &next.scopes);
        actor.scopes.retain(|s| st.actor.has_scope(s));

        let reason = arguments
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        self.audit.record(AuditEvent::handoff(&st.run_id, &st.actor, &st.def.name, target, &reason));

        let last_input = st
            .conversation
            .messages
            .first()
            .and_then(|m| m.content.as_str())
            .unwrap_or("")
            .to_string();
        st.system_suffix = self.system_suffix(app, &next, &actor, &last_input).await?;
        st.actor = actor;
        st.def = next;
        st.path.push(target.to_string());
        st.usage.handoffs += 1;
        Ok(format!("You are now {target}. Continue the conversation from here."))
    }

    /// Summarise older turns when the last call's measured input exceeded the
    /// window. The cut lands on an assistant message so `tool_use` and
    /// `tool_result` pairs are never separated, and the summary is inserted as
    /// a user turn so role alternation holds.
    async fn maybe_compact(&self, app: &App, st: &mut RunState) -> Result<(), String> {
        let policy = &st.entry.policy;
        let Some(max) = policy.max_context_tokens else { return Ok(()) };
        if st.last_input_tokens <= max {
            return Ok(());
        }
        let n = st.conversation.messages.len();
        let keep = policy.keep_recent.max(1);
        if n <= keep + 1 {
            return Ok(());
        }
        let Some(cut) = (1..n)
            .rev()
            .find(|&i| st.conversation.messages[i].role == "assistant" && n - i >= keep)
        else {
            return Ok(());
        };
        if cut < 2 {
            return Ok(());
        }

        let mut transcript = String::new();
        for m in &st.conversation.messages[..cut] {
            transcript.push_str(&m.role);
            transcript.push_str(": ");
            transcript.push_str(&render_message(&m.content));
            transcript.push('\n');
        }
        // The compaction call must itself fit. Bound the transcript to roughly
        // the window, oldest text first to go.
        let limit = (max as usize).saturating_mul(3).max(4000);
        if transcript.len() > limit {
            let start = transcript.len() - limit;
            let boundary = transcript
                .char_indices()
                .map(|(i, _)| i)
                .find(|&i| i >= start)
                .unwrap_or(start);
            transcript = format!("[earlier turns omitted]\n{}", &transcript[boundary..]);
        }

        let model = policy
            .compact_with
            .clone()
            .unwrap_or_else(|| "fast".to_string());
        let summariser = AgentDef {
            name: format!("{}:compact", st.entry.name),
            description: String::new(),
            model,
            system: "You compress conversations between a user, an assistant and its tools. \
                     Produce a dense summary that preserves: the user's original request \
                     verbatim if short; every fact, identifier, number and decision; the \
                     results of tool calls that later steps may need; and any open \
                     questions. Omit pleasantries. Do not add anything."
                .into(),
            tools: Vec::new(),
            max_steps: Some(1),
            token_budget: None,
            scopes: Vec::new(),
            temperature: None,
            max_tokens: 4096,
            handoffs: Vec::new(),
            context: Vec::new(),
            cache: false,
            policy: Default::default(),
        };
        let conversation = Conversation {
            messages: vec![Message {
                role: "user".into(),
                content: json!(format!("Summarise this conversation so far:\n\n{transcript}")),
            }],
        };
        let started = std::time::Instant::now();
        let response = {
            let req = CompletionRequest {
                agent: &summariser,
                conversation: &conversation,
                tools: &[],
                force_tool: None,
                system_suffix: "",
            };
            self.provider
                .complete(&req)
                .await
                .map_err(|e| format!("compaction with model {:?} failed: {e}", summariser.model))?
        };
        self.charge(app, st, "compaction", &response);
        if response.text.is_empty() {
            return Err(format!("compaction model {:?} returned no text", summariser.model));
        }
        let summary = response.text;

        let before = st.conversation.messages.len();
        let tail = st.conversation.messages.split_off(cut);
        st.conversation.messages = vec![Message {
            role: "user".into(),
            content: json!(format!("[Summary of the conversation so far]\n{summary}")),
        }];
        st.conversation.messages.extend(tail);
        st.usage.compactions += 1;
        st.last_input_tokens = 0;
        st.steps.push(Step {
            index: st.usage.steps,
            kind: StepKind::Compaction,
            tool: None,
            arguments: None,
            result: Some(json!({
                "messages_before": before,
                "messages_after": st.conversation.messages.len(),
                "summary_chars": summary.len(),
            })),
            error: None,
            duration_ms: started.elapsed().as_millis() as u64,
        });
        self.audit.record(AuditEvent {
            kind: "compaction".into(),
            run_id: st.run_id.clone(),
            actor: Some(st.actor.id.clone()),
            tool: None,
            detail: json!({"messages_before": before, "messages_after": st.conversation.messages.len()}),
        });
        Ok(())
    }

    /// Resolve the agent's context providers into the system suffix.
    async fn system_suffix(
        &self,
        app: &App,
        def: &AgentDef,
        actor: &Principal,
        input: &str,
    ) -> Result<String, String> {
        let mut suffix = String::new();
        if !def.context.is_empty() {
            let rendered = crate::context::resolve(app, &def.context, actor, &json!({"input": input})).await?;
            suffix.push_str(&rendered);
        }
        if !def.handoffs.is_empty() {
            if !suffix.is_empty() {
                suffix.push_str("\n\n");
            }
            suffix.push_str(
                "When a request is better handled by another agent, call the matching \
                 transfer_to_* tool instead of answering it yourself.",
            );
        }
        Ok(suffix)
    }

    fn charge(&self, app: &App, st: &mut RunState, kind: &str, response: &ProviderResponse) {
        let u = &mut st.usage;
        u.input_tokens = u.input_tokens.saturating_add(response.input_tokens);
        u.output_tokens = u.output_tokens.saturating_add(response.output_tokens);
        u.cache_read_tokens = u.cache_read_tokens.saturating_add(response.cache_read_tokens);
        u.cache_write_tokens = u.cache_write_tokens.saturating_add(response.cache_write_tokens);
        // Over-budget is detected at the top of the loop; charging never fails
        // a step that already happened.
        let _ = st.budget.charge(response.total_tokens());
        app.ledger().charge(
            kind,
            &st.def.name,
            &response.model,
            response.input_tokens,
            response.output_tokens,
            response.cache_read_tokens,
            response.cache_write_tokens,
        );
    }

    fn tool_specs(&self, app: &App, def: &AgentDef) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = def
            .tools
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
            .collect();
        for target in &def.handoffs {
            let desc = app
                .agent(target)
                .map(|a| a.description.clone())
                .unwrap_or_default();
            specs.push(ToolSpec {
                name: format!("transfer_to_{target}"),
                description: if desc.is_empty() {
                    format!("Hand the conversation to the {target} agent.")
                } else {
                    format!("Hand the conversation to the {target} agent: {desc}")
                },
                input_schema: json!({
                    "type": "object",
                    "properties": {"reason": {"type": "string", "description": "Why this agent is better placed to continue."}},
                    "required": ["reason"],
                }),
            });
        }
        specs
    }

    fn suspend(
        &self,
        app: &App,
        mut st: RunState,
        calls: Vec<ToolCall>,
        gated: usize,
        results: Vec<Value>,
    ) -> RunResult {
        let call = &calls[gated];
        let approval_id = uuid::Uuid::new_v4().to_string();
        let reason = format!(
            "tool {:?} is gated and requires human approval before it runs",
            call.name
        );
        st.steps.push(Step {
            index: st.usage.steps,
            kind: StepKind::ApprovalRequested,
            tool: Some(call.name.clone()),
            arguments: Some(call.arguments.clone()),
            result: Some(json!({"approval_id": approval_id})),
            error: None,
            duration_ms: 0,
        });
        self.audit.record(AuditEvent::approval_requested(
            &st.run_id, &st.actor, &call.name, &call.arguments, &approval_id,
        ));
        let pending = PendingApproval {
            approval_id: approval_id.clone(),
            tool: call.name.clone(),
            arguments: call.arguments.clone(),
            reason,
        };
        app.suspend(SuspendedRun {
            approval_id,
            created: std::time::Instant::now(),
            state: st.clone(),
            calls,
            gated,
            results,
        });
        self.finish(app, st, RunStatus::AwaitingApproval, Some(pending))
    }

    fn finish(
        &self,
        app: &App,
        mut st: RunState,
        status: RunStatus,
        pending: Option<PendingApproval>,
    ) -> RunResult {
        st.usage.tree_tokens = st.budget.used();
        self.audit
            .record(AuditEvent::agent_finished(&st.run_id, &st.def.name, status, &st.usage));
        app.ledger().count_run();
        if status != RunStatus::AwaitingApproval {
            if let Some(key) = &st.session_key {
                app.sessions().save(key.clone(), st.conversation.clone());
            }
        }
        RunResult {
            run_id: st.run_id,
            agent: st.def.name,
            path: st.path,
            status,
            output: st.output,
            steps: st.steps,
            usage: st.usage,
            pending_approval: pending,
            session_id: st.session_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

pub fn tool_result(id: &str, content: &Value, is_error: bool) -> Value {
    let text = match content {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    };
    json!({
        "type": "tool_result",
        "tool_use_id": id,
        "content": text,
        "is_error": is_error,
    })
}

/// Cap what the model sees of a tool result. The full value is still in the
/// step record and the audit log; only the model's copy is cut.
pub fn bounded(value: &Value, max_bytes: usize) -> Value {
    let text = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    if max_bytes == 0 || text.len() <= max_bytes {
        return value.clone();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    Value::String(format!(
        "{}…[truncated: showing {} of {} bytes; narrow the query to see the rest]",
        &text[..end],
        end,
        text.len()
    ))
}

fn render_message(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b.get("type").and_then(|t| t.as_str()) {
                Some("text") => b.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                Some("tool_use") => format!(
                    "[called {} with {}]",
                    b.get("name").and_then(|n| n.as_str()).unwrap_or("?"),
                    b.get("input").map(|i| i.to_string()).unwrap_or_default()
                ),
                Some("tool_result") => format!(
                    "[result: {}]",
                    b.get("content").and_then(|c| c.as_str()).unwrap_or("")
                ),
                _ => String::new(),
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
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
                 "scopes": ["admin"]},
                {"id": 3, "method": "GET", "path": "/big",
                 "op": {"kind": "static", "body": {"rows": (0..2000).map(|i| json!({"i": i, "text": "row row row"})).collect::<Vec<_>>()}},
                 "tool": {"expose": true, "name": "big", "read_only": true}},
                {"id": 4, "method": "POST", "path": "/agents/worker",
                 "op": {"kind": "agent", "agent": "worker"},
                 "tool": {"expose": true, "name": "worker"},
                 "input_schema": {"type": "object", "properties": {"input": {"type": "string"}}}}
            ],
            "agents": [
                {"name": "a", "model": "test",
                 "tools": ["safe", "danger", "admin_only", "big", "worker"], "max_steps": 5,
                 "handoffs": ["b"]},
                {"name": "b", "model": "test", "description": "the specialist",
                 "tools": ["safe"], "scopes": ["read"], "max_steps": 5},
                {"name": "worker", "model": "test", "tools": ["safe"], "max_steps": 3}
            ]
        }))
        .expect("test manifest")
    }

    async fn app_with(provider: ScriptedProvider) -> (Arc<App>, Arc<MemoryAudit>, Arc<ScriptedProvider>) {
        let app = App::build_without_python(manifest()).await.expect("app builds");
        let audit = Arc::new(MemoryAudit::default());
        let provider = Arc::new(provider);
        let rt = AgentRuntime::new(provider.clone(), audit.clone());
        (app.with_agent_runtime(rt).into_arc(), audit, provider)
    }

    fn caller(scopes: &[&str]) -> Principal {
        Principal {
            id: "u1".into(),
            kind: crate::auth::PrincipalKind::ApiKey,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            claims: Default::default(),
        }
    }

    async fn run(app: &App, agent: &str, caller: &Principal, input: &str) -> RunResult {
        app.invoke_agent(agent, input, caller, RunOptions::default()).await.expect("runtime present")
    }

    #[tokio::test]
    async fn a_plain_answer_completes_in_one_step() {
        let (app, _, _) = app_with(ScriptedProvider::text("hello")).await;
        let out = run(&app, "a", &caller(&[]), "hi").await;
        assert_eq!(out.status, RunStatus::Completed);
        assert_eq!(out.output, "hello");
        assert_eq!(out.usage.tool_calls, 0);
        assert_eq!(out.path, vec!["a"]);
    }

    #[tokio::test]
    async fn a_tool_call_is_dispatched_in_process_then_the_run_completes() {
        let (app, audit, _) = app_with(ScriptedProvider::tool_then_text("safe", json!({}), "done")).await;
        let out = run(&app, "a", &caller(&[]), "go").await;
        assert_eq!(out.status, RunStatus::Completed);
        assert_eq!(out.usage.tool_calls, 1);
        let call = out.steps.iter().find(|s| s.kind == StepKind::ToolCall).unwrap();
        assert_eq!(call.result, Some(json!({"ok": true})));
        assert!(audit.contains("tool_called"));
    }

    #[tokio::test]
    async fn a_gated_tool_suspends_the_run_and_the_rest_of_the_turn_survives_resume() {
        // Three calls in one turn; the second is gated.
        let (app, audit, _) = app_with(ScriptedProvider::tools_then_text(
            vec![("safe", json!({})), ("danger", json!({})), ("safe", json!({"n": 2}))],
            "all done",
        ))
        .await;
        let out = run(&app, "a", &caller(&["*"]), "go").await;
        assert_eq!(out.status, RunStatus::AwaitingApproval);
        let pending = out.pending_approval.clone().expect("awaiting approval");
        assert_eq!(pending.tool, "danger");
        assert!(audit.contains("approval_requested"));
        // The gated tool did NOT run; the one before it did.
        assert_eq!(out.steps.iter().filter(|s| s.kind == StepKind::ToolCall).count(), 1);
        assert_eq!(app.pending_approvals().len(), 1);

        let resumed = app.resolve_approval(&pending.approval_id, true, "ok", &caller(&["*"])).await.expect("resumes");
        assert_eq!(resumed.status, RunStatus::Completed);
        assert_eq!(resumed.output, "all done");
        // All three tool calls now have results: safe, danger, safe.
        let calls: Vec<_> = resumed.steps.iter().filter(|s| s.kind == StepKind::ToolCall).map(|s| s.tool.clone().unwrap()).collect();
        assert_eq!(calls, vec!["safe", "danger", "safe"]);
        // And the conversation the model saw carried every tool_result.
        assert!(audit.contains("approval_decided"));
        assert!(app.pending_approvals().is_empty(), "consumed on resume");
    }

    #[tokio::test]
    async fn a_denied_approval_tells_the_model_and_continues() {
        let (app, _, provider) = app_with(ScriptedProvider::tool_then_text("danger", json!({}), "understood")).await;
        let out = run(&app, "a", &caller(&["*"]), "go").await;
        let pending = out.pending_approval.expect("awaiting approval");
        let resumed = app.resolve_approval(&pending.approval_id, false, "not today", &caller(&["*"])).await.expect("resumes");
        assert_eq!(resumed.status, RunStatus::Completed);
        assert!(resumed.steps.iter().any(|s| s.kind == StepKind::ApprovalDenied));
        assert!(!resumed.steps.iter().any(|s| s.kind == StepKind::ToolCall), "denied tool must not run");
        // The model was told, in the tool_result it received.
        let last = provider.snapshots().last().unwrap().messages.last().unwrap().clone();
        assert!(last["content"][0]["content"].as_str().unwrap().contains("not today"));
    }

    #[tokio::test]
    async fn an_unknown_approval_id_is_an_error() {
        let (app, _, _) = app_with(ScriptedProvider::text("x")).await;
        assert!(app.resolve_approval("nope", true, "", &caller(&["*"])).await.is_err());
    }

    #[tokio::test]
    async fn an_agent_cannot_reach_a_scope_its_caller_lacks() {
        let (app, _, _) = app_with(ScriptedProvider::tool_then_text("admin_only", json!({}), "x")).await;
        let out = run(&app, "a", &caller(&[]), "go").await;
        let call = out.steps.iter().find(|s| s.kind == StepKind::ToolCall).unwrap();
        assert!(call.error.as_ref().unwrap().contains("403"));
    }

    #[tokio::test]
    async fn an_agent_reaches_a_scoped_route_when_the_caller_holds_the_scope() {
        let (app, _, _) = app_with(ScriptedProvider::tool_then_text("admin_only", json!({}), "x")).await;
        let out = run(&app, "a", &caller(&["admin"]), "go").await;
        let call = out.steps.iter().find(|s| s.kind == StepKind::ToolCall).unwrap();
        assert_eq!(call.result, Some(json!({"secret": 1})));
    }

    #[tokio::test]
    async fn a_tool_outside_the_agents_list_is_refused_not_dispatched() {
        let (app, audit, _) = app_with(ScriptedProvider::tool_then_text("nonexistent", json!({}), "x")).await;
        let out = run(&app, "a", &caller(&["*"]), "go").await;
        assert!(out.steps.iter().any(|s| s.kind == StepKind::ToolRefused));
        assert!(audit.contains("tool_refused"));
    }

    #[tokio::test]
    async fn a_looping_model_is_stopped_by_the_step_limit() {
        let (app, _, _) = app_with(ScriptedProvider::always_tool("safe", json!({}))).await;
        let mut def = app.agent("a").unwrap().clone();
        def.max_steps = Some(3);
        let rt = app.agent_runtime().unwrap();
        let out = rt.run(&app, &def, &caller(&[]), "go", RunOptions::default()).await;
        assert_eq!(out.status, RunStatus::StepLimit);
        assert_eq!(out.usage.steps, 3, "must stop exactly at the limit");
    }

    #[tokio::test]
    async fn the_token_budget_is_enforced_by_the_runtime() {
        let (app, _, _) = app_with(ScriptedProvider::always_tool("safe", json!({})).with_tokens(100, 100)).await;
        let mut def = app.agent("a").unwrap().clone();
        def.max_steps = Some(50);
        def.token_budget = Some(500);
        let out = app.agent_runtime().unwrap().run(&app, &def, &caller(&[]), "go", RunOptions::default()).await;
        assert_eq!(out.status, RunStatus::BudgetExhausted);
        assert!(out.usage.total_tokens() >= 500 && out.usage.total_tokens() < 1000);
    }

    #[tokio::test]
    async fn a_shared_budget_bounds_a_supervisor_and_its_workers_together() {
        // The supervisor calls the worker agent as a tool, forever. Each nested
        // run has its own generous limits; only the shared budget stops it.
        let (app, _, _) = app_with(ScriptedProvider::always_tool("worker", json!({"input": "go"})).with_tokens(100, 100)).await;
        let mut def = app.agent("a").unwrap().clone();
        def.max_steps = Some(50);
        def.token_budget = Some(1_000);
        let out = app.agent_runtime().unwrap().run(&app, &def, &caller(&[]), "go", RunOptions::default()).await;
        assert_eq!(out.status, RunStatus::BudgetExhausted);
        assert!(out.usage.tree_tokens >= 1_000, "tree spend {} must reach the ceiling", out.usage.tree_tokens);
        assert!(out.usage.tree_tokens > out.usage.total_tokens(), "workers' tokens count against the tree");
        assert!(out.usage.tree_tokens < 3_000, "and the ceiling is enforced promptly: {}", out.usage.tree_tokens);
    }

    #[tokio::test]
    async fn nested_agent_calls_carry_depth_so_a_cycle_is_bounded() {
        let mut m = manifest();
        // Make the worker call itself. Two steps per level keeps the tree
        // small (2^depth runs); the ceiling is what must stop the recursion.
        m.agents[2].tools = vec!["worker".into()];
        m.agents[2].max_steps = Some(2);
        m.server.max_invocation_depth = 3;
        let app = App::build_without_python(m).await.unwrap();
        let audit = Arc::new(MemoryAudit::default());
        let rt = AgentRuntime::new(Arc::new(ScriptedProvider::always_tool("worker", json!({"input": "x"}))), audit);
        let app = app.with_agent_runtime(rt).into_arc();
        let out = run(&app, "worker", &caller(&[]), "go").await;
        // The 508 surfaces inside the nested run results, three levels down.
        let rendered = serde_json::to_string(&out.steps).unwrap();
        assert!(rendered.contains("508"), "the nesting ceiling must trip: {rendered}");
        assert!(rendered.contains("ceiling of 3"), "{rendered}");

    }

    #[tokio::test]
    async fn a_handoff_swaps_the_agent_and_shrinks_authority() {
        let (app, audit, provider) = app_with(ScriptedProvider::sequence(vec![
            ("tool", "transfer_to_b", json!({"reason": "specialist"})),
            ("tool", "admin_only", json!({})),
            ("text", "from b", json!(null)),
        ]))
        .await;
        // Caller holds admin, but agent b only declares read, so after the
        // handoff admin_only must be out of reach — and out of b's tool list.
        let out = run(&app, "a", &caller(&["admin", "read"]), "go").await;
        assert_eq!(out.status, RunStatus::Completed);
        assert_eq!(out.path, vec!["a", "b"]);
        assert_eq!(out.agent, "b");
        assert_eq!(out.usage.handoffs, 1);
        assert!(out.steps.iter().any(|s| s.kind == StepKind::Handoff));
        assert!(audit.contains("handoff"));
        // b was refused admin_only: not in its tool list.
        assert!(out.steps.iter().any(|s| s.kind == StepKind::ToolRefused && s.tool.as_deref() == Some("admin_only")));
        // The second provider call was made as b: b's tools, b's description.
        let snaps = provider.snapshots();
        assert_eq!(snaps[1].tool_names, vec!["safe"]);
        assert!(snaps[0].tool_names.contains(&"transfer_to_b".to_string()));
        assert!(snaps[0].system_suffix.contains("transfer_to_"));
    }

    #[tokio::test]
    async fn a_handoff_to_an_undeclared_agent_is_refused() {
        let (app, _, _) = app_with(ScriptedProvider::tool_then_text("transfer_to_worker", json!({"reason": "x"}), "ok")).await;
        let out = run(&app, "a", &caller(&["*"]), "go").await;
        assert!(out.steps.iter().any(|s| s.kind == StepKind::ToolRefused));
        assert_eq!(out.path, vec!["a"]);
    }

    #[tokio::test]
    async fn large_tool_results_are_bounded_before_the_model_sees_them() {
        let (app, _, provider) = app_with(ScriptedProvider::tool_then_text("big", json!({}), "ok")).await;
        let mut def = app.agent("a").unwrap().clone();
        def.policy.max_tool_result_bytes = 500;
        let out = app.agent_runtime().unwrap().run(&app, &def, &caller(&[]), "go", RunOptions::default()).await;
        assert_eq!(out.status, RunStatus::Completed);
        // The step record keeps the full value…
        let step = out.steps.iter().find(|s| s.kind == StepKind::ToolCall).unwrap();
        assert!(step.result.as_ref().unwrap()["rows"].as_array().unwrap().len() == 2000);
        // …but the model's copy was cut.
        let seen = provider.snapshots()[1].messages.last().unwrap().clone();
        let content = seen["content"][0]["content"].as_str().unwrap();
        assert!(content.contains("truncated"), "{content}");
        assert!(content.len() < 700);
    }

    #[tokio::test]
    async fn the_conversation_is_compacted_when_it_outgrows_the_window() {
        // Every call reports 1000 input tokens; the window is 500, so after the
        // first call the runtime must compact before the next.
        let (app, audit, provider) = app_with(
            ScriptedProvider::sequence(vec![
                ("tool", "safe", json!({})),
                ("tool", "safe", json!({})),
                ("text", "SUMMARY", json!(null)),      // the compaction answer
                ("tool", "safe", json!({})),
                ("text", "SUMMARY2", json!(null)),     // a second compaction
                ("text", "final", json!(null)),
            ])
            .with_tokens(1000, 10),
        )
        .await;
        let mut def = app.agent("a").unwrap().clone();
        def.policy.max_context_tokens = Some(500);
        def.policy.keep_recent = 2;
        def.policy.compact_with = Some("test".into());

        let out = app.agent_runtime().unwrap().run(&app, &def, &caller(&[]), "go", RunOptions::default()).await;
        assert_eq!(out.status, RunStatus::Completed, "{:?}", out.steps);
        assert_eq!(out.usage.compactions, 2, "{:?}", out.steps);
        assert_eq!(out.output, "final");

        assert!(audit.contains("compaction"));
        // Some provider call saw a conversation starting with the summary.
        let snaps = provider.snapshots();
        let summarised = snaps.iter().any(|s| {
            s.messages.first().and_then(|m| m["content"].as_str()).is_some_and(|t| t.contains("[Summary"))
        });
        assert!(summarised, "no call saw a compacted conversation");
        // And every compacted conversation still alternates and starts with user.
        for s in &snaps {
            assert_eq!(s.messages[0]["role"], "user");
        }
    }

    #[tokio::test]
    async fn a_failed_compaction_is_recorded_and_the_run_continues() {
        let (app, _, _) = app_with(
            ScriptedProvider::sequence(vec![
                ("tool", "safe", json!({})),
                ("tool", "safe", json!({})),
                ("error", "summariser unavailable", json!(null)), // the compaction call
                ("text", "final", json!(null)),
            ])
            .with_tokens(1000, 10),
        )
        .await;
        let mut def = app.agent("a").unwrap().clone();
        def.policy.max_context_tokens = Some(500);
        def.policy.keep_recent = 2;
        def.policy.compact_with = Some("test".into());

        let out = app.agent_runtime().unwrap().run(&app, &def, &caller(&[]), "go", RunOptions::default()).await;
        assert_eq!(out.status, RunStatus::Completed, "{:?}", out.steps);
        assert_eq!(out.output, "final");
        assert_eq!(out.usage.compactions, 0);
        let step = out.steps.iter().find(|s| s.kind == StepKind::Compaction).expect("a compaction step");
        let error = step.error.as_deref().unwrap_or_default();
        assert!(error.contains("\"test\"") && error.contains("summariser unavailable"), "{error}");
    }

    #[tokio::test]
    async fn a_compaction_that_keeps_failing_backs_off_instead_of_retrying_every_step() {
        // Every call gets a tool turn, so the summariser never returns text and
        // every compaction attempt fails.
        let (app, _, provider) =
            app_with(ScriptedProvider::always_tool("safe", json!({})).with_tokens(1000, 10)).await;
        let mut def = app.agent("a").unwrap().clone();
        def.max_steps = Some(12);
        def.policy.max_context_tokens = Some(500);
        def.policy.keep_recent = 2;
        def.policy.compact_with = Some("test".into());

        let out = app.agent_runtime().unwrap().run(&app, &def, &caller(&[]), "go", RunOptions::default()).await;
        assert_eq!(out.status, RunStatus::StepLimit, "{:?}", out.steps);
        let failed: Vec<_> =
            out.steps.iter().filter(|s| s.kind == StepKind::Compaction).map(|s| s.index).collect();
        // First eligible at step 2, then after waits of 2 and 4 steps; the next
        // would be step 16, past the limit.
        assert_eq!(failed, vec![2, 4, 8]);
        let attempts = provider.snapshots().iter().filter(|s| s.system.starts_with("You compress")).count();
        assert_eq!(attempts, 3);
    }

    #[tokio::test]
    async fn a_response_cut_off_at_max_tokens_is_reported_as_such() {
        let (app, _, _) = app_with(ScriptedProvider::sequence(vec![("truncated", "half an ans", json!(null))])).await;
        let out = run(&app, "a", &caller(&[]), "go").await;
        assert_eq!(out.status, RunStatus::MaxTokens);
        assert_eq!(out.output, "half an ans");
    }

    #[tokio::test]
    async fn model_loop_routes_are_recognised_for_their_own_timeout() {
        let (app, _, _) = app_with(ScriptedProvider::text("x")).await;
        assert!(app.runs_a_model_loop("POST", "/agents/worker"));
        assert!(!app.runs_a_model_loop("GET", "/safe"));
        assert!(!app.runs_a_model_loop("GET", "/no-such-route"));
    }

    #[tokio::test]
    async fn sessions_carry_the_conversation_between_runs() {
        let (app, _, provider) = app_with(ScriptedProvider::sequence(vec![
            ("text", "one", json!(null)),
            ("text", "two", json!(null)),
            ("text", "fresh", json!(null)),
        ]))
        .await;
        let opts = RunOptions { session_id: Some("s1".into()), ..Default::default() };
        let a = app.invoke_agent("a", "first", &caller(&[]), opts.clone()).await.unwrap();
        assert_eq!(a.session_id.as_deref(), Some("s1"));
        let b = app.invoke_agent("a", "second", &caller(&[]), opts.clone()).await.unwrap();
        assert_eq!(b.session_id.as_deref(), Some("s1"), "every turn echoes the session id");
        let snaps = provider.snapshots();
        assert_eq!(snaps[0].message_count, 1);
        assert_eq!(snaps[1].message_count, 3, "user, assistant, user");
        // Another principal with the same session id starts fresh.
        let other = Principal { id: "u2".into(), ..caller(&[]) };
        let _ = app.invoke_agent("a", "hello", &other, opts).await.unwrap();
        assert_eq!(provider.snapshots()[2].message_count, 1);
    }

    #[tokio::test]
    async fn a_provider_error_fails_the_run_without_panicking() {
        let (app, audit, _) = app_with(ScriptedProvider::error("upstream exploded")).await;
        let out = run(&app, "a", &caller(&[]), "go").await;
        assert_eq!(out.status, RunStatus::Failed);
        assert!(audit.contains("agent_failed"));
    }

    #[tokio::test]
    async fn every_run_is_audited_and_charged() {
        let (app, audit, _) = app_with(ScriptedProvider::text("hi")).await;
        run(&app, "a", &caller(&[]), "go").await;
        assert!(audit.contains("agent_started"));
        assert!(audit.contains("agent_finished"));
        let usage = app.ledger().snapshot(|_| None);
        assert_eq!(usage["runs"], 1);
        assert_eq!(usage["by_caller"]["agent:a"]["calls"], 1);
    }

    #[test]
    fn bounded_cuts_on_a_char_boundary() {
        let v = json!({"s": "ééééééééééééééééééééé"});
        let out = bounded(&v, 10);
        assert!(out.as_str().unwrap().contains("truncated"));
        assert_eq!(bounded(&v, 10_000), v);
    }

    #[test]
    fn a_shared_budget_saturates_rather_than_wrapping() {
        // A wrapped counter would read as nearly unspent and reopen the budget.
        let budget = SharedBudget::new("t", Some(1_000));
        assert!(budget.charge(u64::MAX).is_err());
        assert!(budget.charge(u64::MAX).is_err());
        assert_eq!(budget.used(), u64::MAX);
        assert!(budget.exhausted());
    }
}
