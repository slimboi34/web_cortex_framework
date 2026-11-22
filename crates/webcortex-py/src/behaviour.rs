//! The bridge that lets a Behaviour — ordinary Python with real loops and
//! real conditionals — reach back into the Rust runtime.
//!
//! # Why this exists
//!
//! A "skill" as commonly written is a prompt: the model reads instructions and
//! *may* follow them. "If X then Y" is a suggestion, and a model that ignores it
//! fails silently.
//!
//! A Behaviour inverts that. The control flow is Python — a `for` loop is a
//! loop, an `if` is a branch, and both execute whether or not a model would have
//! chosen to. Only the *leaves* are model calls. What you get is a procedure
//! with deterministic structure and probabilistic steps, rather than a
//! probabilistic procedure.
//!
//! # Threading
//!
//! Behaviours run on the Python worker pool, which is deliberately *not* a tokio
//! context. That lets `Handle::block_on` bridge each leaf back into the async
//! runtime without the reentrancy panic that calling it from inside a tokio
//! worker would cause. `gather` and `ask_many` block on a `join_all`, so the
//! leaves they fan out run genuinely concurrently on the runtime while the
//! worker thread waits once.
//!
//! Every such wait is bounded (see [`BehaviourContext::wait`]): it ends at
//! the run's deadline, or as soon as whoever started the run stops waiting
//! for it, and the behaviour gets a `BehaviourHalted` carrying `WC203`. An
//! unbounded wait on a pool thread, for a Python route that needs a pool
//! thread to run, is how a full pool used to wedge for good.

use pyo3::exceptions::{PyPermissionError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use webcortex_core::App;
use webcortex_core::agent::provider::{CompletionRequest, ModelProvider};
use webcortex_core::agent::{Conversation, Message, SharedBudget, ToolSpec, recover_json};
use webcortex_core::auth::Principal;
use webcortex_core::manifest::AgentDef;

// Raised inside a Behaviour when the runtime refuses to continue: a budget is
// exhausted, or a step needs human approval.
pyo3::create_exception!(
    _core,
    BehaviourHalted,
    pyo3::exceptions::PyException,
    "A behaviour was stopped by the runtime rather than by its own logic."
);

/// How far a behaviour's own deadline sits past its route's timeout.
const DEADLINE_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// True when a tool call failed on the runtime's nesting ceiling, a deliberate
/// stop rather than a handler fault. Matched on the status `call_tool_in_tree`
/// reports: a bare "508" also matches a handler's own message, and the phrase
/// "nesting ceiling" never appears in the error at all.
fn is_nesting_ceiling(error: &str) -> bool {
    error.contains("failed with 508:")
}

/// Handed to a Behaviour as `ctx`. Every method crosses back into Rust, so
/// scopes, budgets, approval gates, and the audit trail apply to a behaviour
/// exactly as they apply to an agent.
#[pyclass(name = "BehaviourContext")]
pub struct BehaviourContext {
    app: Arc<App>,
    handle: tokio::runtime::Handle,
    principal: Principal,
    provider: Arc<dyn ModelProvider>,
    run_id: String,
    behaviour: String,

    /// Tools this behaviour may call. Empty means "anything the principal can".
    allowed_tools: Vec<String>,
    /// Context providers this behaviour may resolve.
    contexts: Vec<String>,

    max_steps: u32,
    token_budget: Option<u64>,
    steps: AtomicU64,
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    cache_tokens: AtomicU64,

    /// Model defaults for `ask`, overridable per call.
    model: String,
    max_tokens: u32,
    temperature: Option<f32>,
    reasoning: Option<String>,

    /// How deep this run already sits in the invocation chain. Tools called from
    /// here run one level deeper, which is what bounds a recursive behaviour.
    depth: u32,
    /// The request tree's shared token ceiling.
    budget: Arc<SharedBudget>,
    /// The run's input, so context providers can bind against it.
    input: serde_json::Value,
    /// Extra request-body fields for `ask` calls (`@app.behaviour(options=...)`).
    options: webcortex_core::manifest::RequestOptions,
    /// The behaviour's span; its tool and model calls are children of it.
    span: Option<webcortex_core::trace::TraceCtx>,

    /// When the run must be over: its route's time limit, counted from when
    /// it was started. No call it makes may wait past this.
    deadline: tokio::time::Instant,
    /// How long the run was given, for the message when it runs out.
    limit: std::time::Duration,
    /// Resolves once whoever started the run has stopped waiting for it (the
    /// request timed out, the client left, the agent that called it gave up):
    /// its sender is held by the future the caller awaits.
    caller: tokio::sync::watch::Receiver<()>,

    trace: Mutex<Vec<TraceEntry>>,
}

#[derive(Clone)]
struct TraceEntry {
    kind: String,
    label: String,
    detail: serde_json::Value,
    duration_ms: u64,
}

#[allow(clippy::too_many_arguments)]
impl BehaviourContext {
    pub fn new(
        app: Arc<App>,
        handle: tokio::runtime::Handle,
        principal: Principal,
        provider: Arc<dyn ModelProvider>,
        run_id: String,
        behaviour: String,
        allowed_tools: Vec<String>,
        contexts: Vec<String>,
        max_steps: u32,
        token_budget: Option<u64>,
        model: String,
        max_tokens: u32,
        temperature: Option<f32>,
        reasoning: Option<String>,
        depth: u32,
        budget: Arc<SharedBudget>,
        input: serde_json::Value,
        options: webcortex_core::manifest::RequestOptions,
        span: Option<webcortex_core::trace::TraceCtx>,
        limit: std::time::Duration,
        caller: tokio::sync::watch::Receiver<()>,
    ) -> Self {
        Self {
            app,
            handle,
            principal,
            provider,
            run_id,
            behaviour,
            allowed_tools,
            contexts,
            max_steps,
            token_budget,
            steps: AtomicU64::new(0),
            input_tokens: AtomicU64::new(0),
            output_tokens: AtomicU64::new(0),
            cache_tokens: AtomicU64::new(0),
            model,
            max_tokens,
            temperature,
            reasoning,
            depth,
            budget,
            input,
            options,
            span,
            // A little past the route's own timeout, so that while a caller is
            // waiting the route's 504 always wins: with equal deadlines a busy
            // runtime could deliver the behaviour's `halted` result first and
            // answer 200. The caller-gone branch of `wait` still stops the
            // behaviour the moment the route gives up.
            deadline: tokio::time::Instant::now() + limit + DEADLINE_GRACE,
            limit,
            caller,
            trace: Mutex::new(Vec::new()),
        }
    }

    /// Wait on the runtime for `work`, from this worker thread, but only for
    /// as long as the run may last: until its deadline, or until whoever
    /// started it stops waiting. `Err` carries the `WC203` message to raise.
    ///
    /// Through 2.8.0 the wait had no bound. A behaviour holds a Python worker
    /// thread while it waits, and a Python route it calls needs one to run:
    /// once every thread held a behaviour, the routes they waited for never
    /// started, the requests timed out, and the threads stayed blocked for
    /// good. Now the wait ends, the behaviour is told, and its thread is free.
    fn wait<F: std::future::Future>(&self, work: F) -> Result<F::Output, String> {
        let deadline = self.deadline;
        let mut caller = self.caller.clone();
        let outcome = self.handle.block_on(async move {
            tokio::select! {
                out = work => Ok(out),
                _ = tokio::time::sleep_until(deadline) => Err(true),
                // Never sent to: this resolves when the sender is dropped.
                _ = caller.changed() => Err(false),
            }
        });
        outcome.map_err(|out_of_time| {
            if out_of_time {
                format!(
                    "behaviour {:?} ran out of time: it was given {} s (the route's timeout) and the call \
                     it was waiting on was abandoned (WC203)",
                    self.behaviour,
                    self.limit.as_secs()
                )
            } else {
                format!(
                    "behaviour {:?} was abandoned: the request that started it timed out or its caller \
                     went away, so the call it was waiting on was dropped (WC203)",
                    self.behaviour
                )
            }
        })
    }

    fn charge_step(&self) -> PyResult<()> {
        let used = self.steps.fetch_add(1, Ordering::SeqCst) + 1;
        if used > self.max_steps as u64 {
            return Err(BehaviourHalted::new_err(format!(
                "behaviour {:?} exceeded its step budget of {}",
                self.behaviour, self.max_steps
            )));
        }
        Ok(())
    }

    fn charge_tokens(&self, input: u64, output: u64, cache: u64) -> PyResult<()> {
        // Saturating, like the shared budget: the counts come from upstream.
        let add = |counter: &AtomicU64, n: u64| {
            webcortex_core::agent::saturating_fetch_add(counter, n).saturating_add(n)
        };
        let total = add(&self.input_tokens, input)
            .saturating_add(add(&self.output_tokens, output))
            .saturating_add(add(&self.cache_tokens, cache));
        // The tree's ceiling is checked as well as the behaviour's own, so a
        // behaviour launched by an agent cannot outspend the agent's budget.
        if let Err(e) = self.budget.charge(input.saturating_add(output).saturating_add(cache)) {
            return Err(BehaviourHalted::new_err(e));
        }
        if let Some(budget) = self.token_budget
            && total > budget {
                return Err(BehaviourHalted::new_err(format!(
                    "behaviour {:?} exhausted its token budget of {budget}",
                    self.behaviour
                )));
            }
        Ok(())
    }

    /// Refuse a model call once there is nothing left to spend. Charging
    /// after the call raises once, but a behaviour that catches that and asks
    /// again was making — and paying for — every further call regardless.
    fn ensure_budget(&self) -> PyResult<()> {
        if self.budget.exhausted() {
            return Err(BehaviourHalted::new_err(format!(
                "the request tree rooted at {:?} has spent its shared token budget of {} ({} spent)",
                self.budget.label,
                self.budget.max_tokens.unwrap_or_default(),
                self.budget.used()
            )));
        }
        if let Some(budget) = self.token_budget {
            let spent = self
                .input_tokens
                .load(Ordering::SeqCst)
                .saturating_add(self.output_tokens.load(Ordering::SeqCst))
                .saturating_add(self.cache_tokens.load(Ordering::SeqCst));
            if spent >= budget {
                return Err(BehaviourHalted::new_err(format!(
                    "behaviour {:?} exhausted its token budget of {budget}",
                    self.behaviour
                )));
            }
        }
        Ok(())
    }

    /// Charge a response to the ledger and to both budgets, without deciding
    /// yet whether the behaviour may continue.
    fn account(&self, response: &webcortex_core::agent::ProviderResponse) -> PyResult<()> {
        self.app.ledger().charge(
            "behaviour",
            &self.behaviour,
            &response.model,
            response.input_tokens,
            response.output_tokens,
            response.cache_read_tokens,
            response.cache_write_tokens,
        );
        self.charge_tokens(
            response.input_tokens,
            response.output_tokens,
            response.cache_read_tokens.saturating_add(response.cache_write_tokens),
        )
    }

    fn record(&self, kind: &str, label: &str, detail: serde_json::Value, duration_ms: u64) {
        if let Ok(mut trace) = self.trace.lock() {
            trace.push(TraceEntry {
                kind: kind.into(),
                label: label.into(),
                detail,
                duration_ms,
            });
        }
    }

    /// The checks every tool call goes through before it is dispatched.
    fn admit(&self, tool: &str) -> PyResult<()> {
        if !self.allowed_tools.is_empty() && !self.allowed_tools.iter().any(|t| t == tool) {
            return Err(PyPermissionError::new_err(format!(
                "behaviour {:?} may not call {tool:?}; declared tools: {}",
                self.behaviour,
                self.allowed_tools.join(", ")
            )));
        }
        // An approval gate must stop a behaviour just as it stops an agent,
        // otherwise wrapping a gated tool in a behaviour would launder it —
        // as would calling a flow that runs one as a step.
        if let Some(gated) = self.app.gated_tool_behind(tool) {
            return Err(BehaviourHalted::new_err(if gated == tool {
                format!("tool {tool:?} requires human approval and cannot run unattended inside a behaviour")
            } else {
                format!(
                    "tool {tool:?} runs the approval-gated tool {gated:?} and cannot run unattended \
                     inside a behaviour"
                )
            }));
        }
        Ok(())
    }

    fn settle_call(&self, py: Python<'_>, tool: &str, result: Result<serde_json::Value, String>, elapsed: u64) -> PyResult<Py<PyAny>> {
        match result {
            Ok(value) => {
                self.record("call", tool, value.clone(), elapsed);
                json_to_py(py, &value)
            }
            Err(e) => {
                self.record("call_failed", tool, serde_json::json!({"error": &e}), elapsed);
                // The runtime's nesting ceiling is a deliberate stop, not a
                // handler fault; surface it as a halt so the caller sees why.
                if is_nesting_ceiling(&e) {
                    return Err(BehaviourHalted::new_err(format!(
                        "behaviour {:?} was stopped: {e}",
                        self.behaviour
                    )));
                }
                Err(PyRuntimeError::new_err(e))
            }
        }
    }

    fn agent_def(&self, model: Option<&str>, system: Option<&str>, max_tokens: Option<u32>, temperature: Option<f32>) -> AgentDef {
        AgentDef {
            name: self.behaviour.clone(),
            description: String::new(),
            model: model.unwrap_or(&self.model).to_string(),
            system: system.unwrap_or_default().to_string(),
            tools: Vec::new(),
            max_steps: Some(1),
            token_budget: None,
            scopes: Vec::new(),
            temperature: temperature.or(self.temperature),
            reasoning: self.reasoning.clone(),
            max_tokens: max_tokens.unwrap_or(self.max_tokens),
            handoffs: Vec::new(),
            context: Vec::new(),
            cache: false,
            policy: Default::default(),
            options: self.options.clone(),
            output_schema: None,
        }
    }

    /// A span for one tool call made by this behaviour.
    fn tool_span(&self, tool: &str) -> webcortex_core::trace::Scoped<'_> {
        let mut span = self.app.tracer().scoped(self.span.as_ref(), "tool", &format!("tool {tool}"));
        span.set("gen_ai.operation.name", serde_json::json!("execute_tool"));
        span.set("gen_ai.tool.name", serde_json::json!(tool));
        span
    }

    /// A span for one model call made by this behaviour.
    fn model_span(&self, model: &str) -> webcortex_core::trace::Scoped<'_> {
        let mut span = self.app.tracer().scoped(self.span.as_ref(), "model", &format!("chat {model}"));
        span.set("gen_ai.operation.name", serde_json::json!("chat"));
        span.set("gen_ai.request.model", serde_json::json!(model));
        span
    }

    /// A structured answer as `ask` checks it: the `respond` call's
    /// arguments, or JSON recovered from prose when a local model answered in
    /// text, held to the schema. `Err` lists what is wrong with it, each
    /// problem naming the place it is at.
    ///
    /// Through 2.8.0 nothing was checked: whatever object the model sent back
    /// — a key missing, a string where the schema said integer, a value off
    /// the enum, even an array — was handed to the behaviour as the answer.
    fn structured(response: &webcortex_core::agent::ProviderResponse, schema: &serde_json::Value) -> Result<serde_json::Value, Vec<String>> {
        let value = response
            .tool_calls
            .iter()
            .find(|c| c.name == RESPOND)
            .map(|c| c.arguments.clone())
            .or_else(|| recover_json(&response.text));
        let Some(value) = value else {
            return Err(vec![format!(
                "$: the answer was not structured output (no {RESPOND} call and no JSON in the text: {})",
                if response.text.is_empty() { "<nothing>".to_string() } else { clip(&response.text, 200) }
            )]);
        };
        let problems = webcortex_core::schema::validate(schema, &value);
        if problems.is_empty() { Ok(value) } else { Err(problems) }
    }

    /// What the model is told when its structured answer did not match.
    fn correction(problems: &[String]) -> String {
        format!(
            "the answer does not match the required schema (WC318): {}. Call {RESPOND} again with \
             arguments that match its schema.",
            problems.join("; ")
        )
    }

    /// The error `ask` raises once the corrections are spent: a RuntimeError
    /// whose message starts `WC318:`, with `code` and `problems` attributes.
    fn mismatch(&self, py: Python<'_>, problems: &[String], elapsed: u64) -> PyErr {
        self.record("ask_failed", "structured", serde_json::json!({"code": "WC318", "problems": problems}), elapsed);
        self.app.recorder().record_failure(
            "behaviour",
            &format!("ask in behaviour {}", self.behaviour),
            "WC318",
            &self.run_id,
        );
        let err = PyRuntimeError::new_err(format!(
            "WC318: the model's answer still did not match the schema after {} correction(s): {}",
            webcortex_core::agent::MAX_OUTPUT_REPAIRS,
            problems.join("; ")
        ));
        coded(py, err, "WC318", Some(problems))
    }

    /// The text `ask` returns for an unstructured answer.
    fn text_answer(&self, py: Python<'_>, response: webcortex_core::agent::ProviderResponse, elapsed: u64) -> PyResult<Py<PyAny>> {
        self.record(
            "ask",
            "text",
            serde_json::json!({"chars": response.text.len(), "model": response.model}),
            elapsed,
        );
        Ok(response.text.into_pyobject(py)?.into_any().unbind())
    }

    /// Send one completion per conversation, at most `limit` at once, each
    /// under its own model span, bounded by the run's deadline.
    fn complete_all(
        &self,
        py: Python<'_>,
        def: &AgentDef,
        conversations: &[Conversation],
        tools: &[ToolSpec],
        limit: usize,
    ) -> Result<Vec<Result<webcortex_core::agent::ProviderResponse, String>>, String> {
        let provider = self.provider.clone();
        let structured = !tools.is_empty();
        let spans: Vec<_> = conversations.iter().map(|_| self.model_span(&def.model)).collect();
        let waited = py.detach(|| {
            self.wait(async {
                use futures::StreamExt;
                futures::stream::iter(conversations.iter())
                    .map(|conversation| {
                        let provider = provider.clone();
                        async move {
                            let req = CompletionRequest {
                                agent: def,
                                conversation,
                                tools,
                                force_tool: if structured { Some(RESPOND) } else { None },
                                system_suffix: "",
                            };
                            provider.complete(&req).await
                        }
                    })
                    .buffered(limit)
                    .collect::<Vec<_>>()
                    .await
            })
        });
        match waited {
            Ok(responses) => {
                for (span, response) in spans.into_iter().zip(&responses) {
                    end_model_span(span, response);
                }
                Ok(responses)
            }
            Err(stopped) => {
                for span in spans {
                    end_model_span(span, &Err(stopped.clone()));
                }
                Err(stopped)
            }
        }
    }

    /// Ask every prompt, holding structured answers to `schema`: an answer
    /// that does not match is sent back with what is wrong, at most
    /// [`MAX_OUTPUT_REPAIRS`](webcortex_core::agent::MAX_OUTPUT_REPAIRS)
    /// times, as an agent's `output=` answer is. Every response is charged
    /// before anything is raised; each correction is a step.
    #[allow(clippy::too_many_arguments)]
    fn ask_all(
        &self,
        py: Python<'_>,
        prompts: &[String],
        schema: Option<serde_json::Value>,
        system: Option<&str>,
        model: Option<&str>,
        max_tokens: Option<u32>,
        temperature: Option<f32>,
        limit: usize,
    ) -> PyResult<Vec<Py<PyAny>>> {
        let def = self.agent_def(model, system, max_tokens, temperature);
        // A schema is expressed as a single tool the model must use. That is
        // what turns "please answer as JSON" from a request into a guarantee
        // — and the runtime checks the answer, because a provider only asks.
        let tools: Vec<ToolSpec> = match &schema {
            Some(s) => vec![ToolSpec {
                name: RESPOND.into(),
                description: "Respond with structured output in the required shape.".into(),
                input_schema: s.clone(),
            }],
            None => Vec::new(),
        };
        let mut conversations: Vec<Conversation> = prompts
            .iter()
            .map(|p| Conversation { messages: vec![Message { role: "user".into(), content: serde_json::json!(p) }] })
            .collect();
        let mut answers: Vec<Option<Py<PyAny>>> = prompts.iter().map(|_| None).collect();
        let mut pending: Vec<usize> = (0..prompts.len()).collect();
        let mut repairs = 0;
        let started = std::time::Instant::now();
        loop {
            let round: Vec<Conversation> = pending.iter().map(|&i| conversations[i].clone()).collect();
            let responses = match self.complete_all(py, &def, &round, &tools, limit) {
                Ok(r) => r,
                Err(stopped) => {
                    let elapsed = started.elapsed().as_millis() as u64;
                    self.record("ask_failed", "model", serde_json::json!({"error": &stopped}), elapsed);
                    return Err(coded(py, BehaviourHalted::new_err(stopped), "WC203", None));
                }
            };
            let elapsed = started.elapsed().as_millis() as u64;

            // Every answer that came back was paid for, so every one is
            // charged before anything is raised. Settling in order and
            // stopping at the first failure (or the first answer over budget)
            // left the rest of the batch out of the ledger and the budget.
            let mut halted: Option<PyErr> = None;
            for response in responses.iter().flatten() {
                if let Err(e) = self.account(response)
                    && halted.is_none()
                {
                    halted = Some(e);
                }
            }
            if let Some(e) = halted {
                return Err(e);
            }

            let mut retry = Vec::new();
            let mut failed: Option<Vec<String>> = None;
            for (&i, response) in pending.iter().zip(responses) {
                let response = response.map_err(|e| {
                    self.record("ask_failed", "model", serde_json::json!({"error": &e}), elapsed);
                    PyRuntimeError::new_err(e)
                })?;
                let Some(schema) = &schema else {
                    answers[i] = Some(self.text_answer(py, response, elapsed)?);
                    continue;
                };
                // Checked without the GIL: the check is bounded, but a large
                // answer to a schema with many alternatives can take a
                // noticeable fraction of a second, and other Python threads
                // should run meanwhile.
                match py.detach(|| Self::structured(&response, schema)) {
                    Ok(value) => {
                        self.record("ask", "structured", value.clone(), elapsed);
                        answers[i] = Some(json_to_py(py, &value)?);
                    }
                    Err(problems) if repairs < webcortex_core::agent::MAX_OUTPUT_REPAIRS => {
                        self.record("ask_rejected", "structured", serde_json::json!({"code": "WC318", "problems": &problems}), elapsed);
                        conversations[i].messages.extend(webcortex_core::agent::correction_turns(&response, &Self::correction(&problems)));
                        retry.push(i);
                    }
                    Err(problems) => {
                        failed.get_or_insert(problems);
                    }
                }
            }
            if let Some(problems) = failed {
                return Err(self.mismatch(py, &problems, elapsed));
            }
            if retry.is_empty() {
                break;
            }
            // Each correction is another model call: one step, and only while
            // there is budget left to spend.
            self.ensure_budget()?;
            for _ in &retry {
                self.charge_step()?;
            }
            repairs += 1;
            pending = retry;
        }
        Ok(answers.into_iter().map(|a| a.expect("every prompt answered")).collect())
    }
}

/// The tool a structured `ask` is answered through.
const RESPOND: &str = "respond";

/// The first `max` characters of `text`, marked when cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// `err` with a `code` attribute (and `problems`, when given), so a
/// behaviour can tell a deadline (`WC203`) or a schema mismatch (`WC318`)
/// from any other failure without parsing the message.
fn coded(py: Python<'_>, err: PyErr, code: &str, problems: Option<&[String]>) -> PyErr {
    let value = err.value(py);
    let _ = value.setattr("code", code);
    if let Some(problems) = problems {
        let _ = value.setattr("problems", problems.to_vec());
    }
    err
}

/// Close a tool call's span with its outcome.
fn end_tool_span(span: webcortex_core::trace::Scoped<'_>, result: &Result<serde_json::Value, String>) {
    match result {
        Ok(_) => span.ok(),
        Err(e) => span.fail(webcortex_core::errors::code_in(e).unwrap_or("WC308")),
    }
}

/// Close a model call's span with its token counts, or its failure.
fn end_model_span(
    mut span: webcortex_core::trace::Scoped<'_>,
    response: &Result<webcortex_core::agent::ProviderResponse, String>,
) {
    match response {
        Ok(r) => {
            span.set("gen_ai.response.model", serde_json::json!(r.model));
            span.set("gen_ai.usage.input_tokens", serde_json::json!(r.input_tokens));
            span.set("gen_ai.usage.output_tokens", serde_json::json!(r.output_tokens));
            span.set("webcortex.usage.cache_read_tokens", serde_json::json!(r.cache_read_tokens));
            span.set("webcortex.usage.cache_write_tokens", serde_json::json!(r.cache_write_tokens));
            span.ok();
        }
        Err(e) => span.fail(webcortex_core::errors::classify_run_error(e)),
    }
}

/// One `(tool, kwargs)` request to `gather`.
fn parse_gather_item(item: &Bound<'_, PyAny>) -> PyResult<(String, serde_json::Value)> {
    if let Ok(t) = item.cast::<pyo3::types::PyTuple>()
        && (t.len() == 1 || t.len() == 2) {
            let name: String = t.get_item(0)?.extract()?;
            let args = if t.len() == 2 { py_to_json(&t.get_item(1)?)? } else { serde_json::json!({}) };
            if !args.is_object() {
                return Err(PyValueError::new_err("gather: the second element must be a dict of arguments"));
            }
            return Ok((name, args));
        }
    if let Ok(s) = item.extract::<String>() {
        return Ok((s, serde_json::json!({})));
    }
    if let Ok(d) = item.cast::<PyDict>() {
        let mut map = serde_json::Map::new();
        let mut name = None;
        for (k, v) in d.iter() {
            let key: String = k.extract()?;
            if key == "tool" {
                name = Some(v.extract::<String>()?);
            } else {
                map.insert(key, py_to_json(&v)?);
            }
        }
        if let Some(name) = name {
            return Ok((name, serde_json::Value::Object(map)));
        }
    }
    Err(PyValueError::new_err(
        "gather items must be (tool, kwargs) tuples, tool names, or dicts with a 'tool' key",
    ))
}

#[pymethods]
impl BehaviourContext {
    /// Call one of the application's own tools.
    ///
    /// Goes through the same dispatcher an HTTP request uses, under this
    /// behaviour's delegated principal — so a behaviour cannot reach a route its
    /// caller could not.
    #[pyo3(signature = (tool, **kwargs))]
    fn call(
        &self,
        py: Python<'_>,
        tool: &str,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        self.admit(tool)?;
        self.charge_step()?;

        let args = match kwargs {
            Some(d) => py_to_json(d.as_any())?,
            None => serde_json::json!({}),
        };

        let started = std::time::Instant::now();
        let app = self.app.clone();
        let principal = self.principal.clone();
        let tool_name = tool.to_string();
        let depth = self.depth;
        let budget = self.budget.clone();
        let span = self.tool_span(tool);
        let trace = span.ctx().cloned();

        // Release the GIL: the tool call may take real time, and other worker
        // threads must keep running while it does.
        let waited = py.detach(|| {
            self.wait(async move {
                app.call_tool_traced(&tool_name, &args, &principal, depth, Some(budget), trace)
                    .await
            })
        });
        let elapsed = started.elapsed().as_millis() as u64;
        let result = match waited {
            Ok(result) => result,
            Err(stopped) => {
                end_tool_span(span, &Err(stopped.clone()));
                self.record("call_failed", tool, serde_json::json!({"error": &stopped}), elapsed);
                return Err(coded(py, BehaviourHalted::new_err(stopped), "WC203", None));
            }
        };
        end_tool_span(span, &result);

        self.settle_call(py, tool, result, elapsed)
    }

    /// Call several tools at once and wait for all of them.
    ///
    /// Each item is `(tool, kwargs)`. The calls run concurrently on the Rust
    /// runtime — a loop of fifty `ctx.call`s becomes one round trip — and every
    /// one is admitted, scoped, gated and charged exactly as `call` is. One
    /// step is charged per item, so `max_steps` still bounds the total.
    ///
    /// By default the first failure is raised after every call has finished.
    /// With `return_exceptions=True`, failures come back in place as
    /// `{"error": "...", "ok": False}`.
    #[pyo3(signature = (*items, return_exceptions = false))]
    fn gather(
        &self,
        py: Python<'_>,
        items: &Bound<'_, pyo3::types::PyTuple>,
        return_exceptions: bool,
    ) -> PyResult<Py<PyAny>> {
        let mut calls = Vec::with_capacity(items.len());
        for item in items.iter() {
            let (name, args) = parse_gather_item(&item)?;
            self.admit(&name)?;
            calls.push((name, args));
        }
        for _ in &calls {
            self.charge_step()?;
        }

        let started = std::time::Instant::now();
        let app = self.app.clone();
        let principal = self.principal.clone();
        let depth = self.depth;
        let budget = self.budget.clone();
        let names: Vec<String> = calls.iter().map(|(n, _)| n.clone()).collect();
        let spans: Vec<_> = names.iter().map(|n| self.tool_span(n)).collect();
        let traces: Vec<_> = spans.iter().map(|s| s.ctx().cloned()).collect();

        let waited = py.detach(|| {
            self.wait(async move {
                let futures = calls.iter().zip(traces).map(|((name, args), trace)| {
                    app.call_tool_traced(name, args, &principal, depth, Some(budget.clone()), trace)
                });
                futures::future::join_all(futures).await
            })
        });
        let elapsed = started.elapsed().as_millis() as u64;
        let results: Vec<Result<serde_json::Value, String>> = match waited {
            Ok(results) => results,
            Err(stopped) => {
                for (span, name) in spans.into_iter().zip(&names) {
                    end_tool_span(span, &Err(stopped.clone()));
                    self.record("call_failed", name, serde_json::json!({"error": &stopped}), elapsed);
                }
                return Err(coded(py, BehaviourHalted::new_err(stopped), "WC203", None));
            }
        };
        for (span, result) in spans.into_iter().zip(&results) {
            end_tool_span(span, result);
        }

        let list = PyList::empty(py);
        let mut first_err: Option<String> = None;
        for (name, result) in names.iter().zip(results) {
            match result {
                Ok(value) => {
                    self.record("call", name, value.clone(), elapsed);
                    list.append(json_to_py(py, &value)?)?;
                }
                Err(e) => {
                    self.record("call_failed", name, serde_json::json!({"error": &e}), elapsed);
                    if return_exceptions {
                        list.append(json_to_py(py, &serde_json::json!({"error": e, "ok": false}))?)?;
                    } else if first_err.is_none() {
                        first_err = Some(format!("{name}: {e}"));
                    }
                }
            }
        }
        if let Some(e) = first_err {
            if is_nesting_ceiling(&e) {
                return Err(BehaviourHalted::new_err(format!("behaviour {:?} was stopped: {e}", self.behaviour)));
            }
            return Err(PyRuntimeError::new_err(e));
        }
        Ok(list.into_any().unbind())
    }

    /// Ask the model a question.
    ///
    /// With `schema`, the model is *forced* to answer in that shape and the
    /// parsed object is returned — so a behaviour's branches can switch on real
    /// values rather than on parsed prose. The answer is checked against the
    /// schema; one that does not match is sent back to the model with what is
    /// wrong, at most twice, and then a `RuntimeError` (`code == "WC318"`,
    /// `problems` listing each mismatch) is raised. `model` may be an alias
    /// such as `"fast"`: use the cheap tier for classification leaves.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (prompt, *, schema = None, system = None, model = None, max_tokens = None, temperature = None))]
    fn ask(
        &self,
        py: Python<'_>,
        prompt: &str,
        schema: Option<&Bound<'_, PyAny>>,
        system: Option<&str>,
        model: Option<&str>,
        max_tokens: Option<u32>,
        temperature: Option<f32>,
    ) -> PyResult<Py<PyAny>> {
        self.ensure_budget()?;
        self.charge_step()?;
        let schema_json = match schema {
            Some(s) => Some(py_to_json(s)?),
            None => None,
        };
        let mut answers =
            self.ask_all(py, &[prompt.to_string()], schema_json, system, model, max_tokens, temperature, 1)?;
        Ok(answers.remove(0))
    }

    /// Ask the model many questions at once.
    ///
    /// The prompts are sent concurrently, at most `concurrency` in flight, and
    /// the answers come back in order. This is the loop
    /// `for ticket in tickets: ctx.ask(...)` collapsed into one wait — the same
    /// tokens, a fraction of the wall-clock. Each prompt is one step and is
    /// charged like a single `ask`, and each structured answer is checked
    /// against `schema` the same way.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (prompts, *, schema = None, system = None, model = None, max_tokens = None, temperature = None, concurrency = 8))]
    fn ask_many(
        &self,
        py: Python<'_>,
        prompts: Vec<String>,
        schema: Option<&Bound<'_, PyAny>>,
        system: Option<&str>,
        model: Option<&str>,
        max_tokens: Option<u32>,
        temperature: Option<f32>,
        concurrency: usize,
    ) -> PyResult<Py<PyAny>> {
        self.ensure_budget()?;
        for _ in &prompts {
            self.charge_step()?;
        }
        let schema_json = match schema {
            Some(s) => Some(py_to_json(s)?),
            None => None,
        };
        let limit = concurrency.clamp(1, 64);
        let answers = self.ask_all(py, &prompts, schema_json, system, model, max_tokens, temperature, limit)?;
        let list = PyList::empty(py);
        for answer in answers {
            list.append(answer)?;
        }
        Ok(list.into_any().unbind())
    }

    /// Resolve a declared context provider and return its value.
    ///
    /// Only providers named in the behaviour's `context=` list are reachable,
    /// so what a procedure can see is declared next to what it can call.
    fn context(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        if !self.contexts.iter().any(|c| c == name) {
            return Err(PyPermissionError::new_err(format!(
                "behaviour {:?} did not declare context {name:?}; declared: {}",
                self.behaviour,
                if self.contexts.is_empty() { "none".to_string() } else { self.contexts.join(", ") }
            )));
        }
        let def = self
            .app
            .manifest
            .context(name)
            .cloned()
            .ok_or_else(|| PyRuntimeError::new_err(format!("context {name:?} is not declared")))?;
        let started = std::time::Instant::now();
        let app = self.app.clone();
        let principal = self.principal.clone();
        let input = self.input.clone();
        let waited = py.detach(|| {
            self.wait(async move { webcortex_core::context::resolve_one(&app, &def, &principal, &input).await })
        });
        let elapsed = started.elapsed().as_millis() as u64;
        let result = match waited {
            Ok(result) => result,
            Err(stopped) => {
                self.record("context_failed", name, serde_json::json!({"error": &stopped}), elapsed);
                return Err(coded(py, BehaviourHalted::new_err(stopped), "WC203", None));
            }
        };
        match result {
            Ok(value) => {
                self.record("context", name, serde_json::json!({"chars": value.to_string().len()}), elapsed);
                json_to_py(py, &value)
            }
            Err(e) => {
                self.record("context_failed", name, serde_json::json!({"error": &e}), elapsed);
                Err(PyRuntimeError::new_err(e))
            }
        }
    }

    /// Run another behaviour or agent tool and return its result.
    /// Composition is just a tool call, so budgets and scopes still apply.
    #[pyo3(signature = (name, **kwargs))]
    fn run(
        &self,
        py: Python<'_>,
        name: &str,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        self.call(py, name, kwargs)
    }

    /// Emit a line into the behaviour's trace and the server log.
    fn log(&self, message: &str) {
        tracing::info!(
            target: "webcortex::behaviour",
            run_id = %self.run_id,
            behaviour = %self.behaviour,
            "{message}"
        );
        self.record("log", message, serde_json::Value::Null, 0);
    }

    /// Stop the behaviour deliberately, with a reason recorded in the trace.
    fn halt(&self, reason: &str) -> PyResult<()> {
        Err(BehaviourHalted::new_err(reason.to_string()))
    }

    /// Tools this behaviour is allowed to call.
    #[getter]
    fn tools(&self) -> Vec<String> {
        if self.allowed_tools.is_empty() {
            self.app
                .exposed_tools()
                .into_iter()
                .map(|r| r.tool_name())
                .collect()
        } else {
            self.allowed_tools.clone()
        }
    }

    /// Context providers this behaviour may resolve.
    #[getter]
    fn contexts(&self) -> Vec<String> {
        self.contexts.clone()
    }

    /// The principal this behaviour is acting as.
    #[getter]
    fn user<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("id", &self.principal.id)?;
        d.set_item("root_id", self.principal.root_id())?;
        d.set_item("authenticated", !self.principal.is_anonymous())?;
        d.set_item("scopes", self.principal.scopes.clone())?;
        Ok(d)
    }

    #[getter]
    fn run_id(&self) -> &str {
        &self.run_id
    }

    /// How deep this run sits in the invocation chain. Zero for a run started
    /// directly over HTTP.
    #[getter]
    fn depth(&self) -> u32 {
        self.depth
    }

    /// Budget consumed so far. Readable mid-run so a behaviour can adapt —
    /// switch to a cheaper model, stop early, or skip optional work.
    #[getter]
    fn usage<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("steps", self.steps.load(Ordering::SeqCst))?;
        d.set_item("max_steps", self.max_steps)?;
        d.set_item("input_tokens", self.input_tokens.load(Ordering::SeqCst))?;
        d.set_item("output_tokens", self.output_tokens.load(Ordering::SeqCst))?;
        d.set_item("cache_tokens", self.cache_tokens.load(Ordering::SeqCst))?;
        d.set_item("token_budget", self.token_budget)?;
        d.set_item("tree_tokens", self.budget.used())?;
        d.set_item("tree_budget", self.budget.max_tokens)?;
        d.set_item("depth", self.depth)?;
        Ok(d)
    }

    /// The ordered trace of every leaf this run executed.
    #[getter]
    fn trace<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let entries = match self.trace.lock() {
            Ok(t) => t.clone(),
            Err(p) => p.into_inner().clone(),
        };
        let list = PyList::empty(py);
        for e in entries {
            let d = PyDict::new(py);
            d.set_item("kind", &e.kind)?;
            d.set_item("label", &e.label)?;
            d.set_item("detail", json_to_py(py, &e.detail)?)?;
            d.set_item("duration_ms", e.duration_ms)?;
            list.append(d)?;
        }
        Ok(list)
    }

    fn __repr__(&self) -> String {
        format!(
            "<BehaviourContext {} run={} steps={}/{}>",
            self.behaviour,
            self.run_id,
            self.steps.load(Ordering::SeqCst),
            self.max_steps
        )
    }
}

// ---------------------------------------------------------------------------
// JSON <-> Python conversion
// ---------------------------------------------------------------------------

/// How deep a Python value may nest on its way to JSON. A self-referential
/// dict (`d["self"] = d`) or an object graph with a cycle would otherwise
/// recurse until the native stack overflows, which aborts the whole server
/// rather than failing one call.
const MAX_JSON_DEPTH: usize = 256;

pub fn py_to_json(value: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    to_json(value, 0)
}

fn to_json(value: &Bound<'_, PyAny>, depth: usize) -> PyResult<serde_json::Value> {
    use pyo3::types::{PyBool, PyFloat, PyInt, PyString};

    if depth > MAX_JSON_DEPTH {
        return Err(PyValueError::new_err(format!(
            "value nests more than {MAX_JSON_DEPTH} levels deep (a reference cycle?); cannot convert it to JSON"
        )));
    }

    if value.is_none() {
        return Ok(serde_json::Value::Null);
    }
    // bool before int: Python bools are ints, and checking int first would turn
    // `True` into `1`.
    if let Ok(b) = value.cast::<PyBool>() {
        return Ok(serde_json::Value::Bool(b.is_true()));
    }
    if let Ok(s) = value.cast::<PyString>() {
        return Ok(serde_json::Value::String(s.to_str()?.to_string()));
    }
    if let Ok(i) = value.cast::<PyInt>() {
        // JSON has one number type; every integer that fits 64 bits either
        // way round is kept exact. `2**63` used to raise OverflowError.
        if let Ok(n) = i.extract::<i64>() {
            return Ok(serde_json::Value::from(n));
        }
        if let Ok(n) = i.extract::<u64>() {
            return Ok(serde_json::Value::from(n));
        }
        return Err(PyValueError::new_err(format!(
            "integer {} does not fit in 64 bits, which is as large as a JSON number is kept exactly; pass it as a string",
            i.str()?
        )));
    }
    if let Ok(f) = value.cast::<PyFloat>() {
        return Ok(serde_json::Value::from(f.extract::<f64>()?));
    }
    if let Ok(d) = value.cast::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (k, v) in d.iter() {
            map.insert(k.str()?.to_str()?.to_string(), to_json(&v, depth + 1)?);
        }
        return Ok(serde_json::Value::Object(map));
    }
    if let Ok(list) = value.cast::<PyList>() {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            out.push(to_json(&item, depth + 1)?);
        }
        return Ok(serde_json::Value::Array(out));
    }
    if let Ok(tuple) = value.cast::<pyo3::types::PyTuple>() {
        let mut out = Vec::with_capacity(tuple.len());
        for item in tuple.iter() {
            out.push(to_json(&item, depth + 1)?);
        }
        return Ok(serde_json::Value::Array(out));
    }

    // Dataclasses, pydantic models, and anything else with a standard hook.
    for attr in ["model_dump", "_asdict"] {
        if let Ok(method) = value.getattr(attr)
            && method.is_callable() {
                return to_json(&method.call0()?, depth + 1);
            }
    }
    if let Ok(d) = value.getattr("__dict__")
        && d.cast::<PyDict>().is_ok() {
            return to_json(&d, depth + 1);
        }

    Err(PyValueError::new_err(format!(
        "cannot convert {} to JSON",
        value.get_type().name()?
    )))
}

pub fn json_to_py(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
    Ok(match value {
        serde_json::Value::Null => py.None(),
        serde_json::Value::Bool(b) => b.into_pyobject(py)?.to_owned().into_any().unbind(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_pyobject(py)?.into_any().unbind()
            } else if let Some(u) = n.as_u64() {
                // Above i64::MAX: an int, not a float that has lost the low bits.
                u.into_pyobject(py)?.into_any().unbind()
            } else {
                n.as_f64().unwrap_or(0.0).into_pyobject(py)?.into_any().unbind()
            }
        }
        serde_json::Value::String(s) => s.into_pyobject(py)?.into_any().unbind(),
        serde_json::Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(json_to_py(py, item)?)?;
            }
            list.into_any().unbind()
        }
        serde_json::Value::Object(map) => {
            let d = PyDict::new(py);
            for (k, v) in map {
                d.set_item(k, json_to_py(py, v)?)?;
            }
            d.into_any().unbind()
        }
    })
}
