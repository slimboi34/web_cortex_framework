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

    /// How deep this run already sits in the invocation chain. Tools called from
    /// here run one level deeper, which is what bounds a recursive behaviour.
    depth: u32,
    /// The request tree's shared token ceiling.
    budget: Arc<SharedBudget>,
    /// The run's input, so context providers can bind against it.
    input: serde_json::Value,

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
        depth: u32,
        budget: Arc<SharedBudget>,
        input: serde_json::Value,
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
            depth,
            budget,
            input,
            trace: Mutex::new(Vec::new()),
        }
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
        let add = |counter: &AtomicU64, n: u64| match counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| Some(c.saturating_add(n)))
        {
            Ok(c) | Err(c) => c.saturating_add(n),
        };
        let total = add(&self.input_tokens, input)
            .saturating_add(add(&self.output_tokens, output))
            .saturating_add(add(&self.cache_tokens, cache));
        // The tree's ceiling is checked as well as the behaviour's own, so a
        // behaviour launched by an agent cannot outspend the agent's budget.
        if let Err(e) = self.budget.charge(input.saturating_add(output).saturating_add(cache)) {
            return Err(BehaviourHalted::new_err(e));
        }
        if let Some(budget) = self.token_budget {
            if total > budget {
                return Err(BehaviourHalted::new_err(format!(
                    "behaviour {:?} exhausted its token budget of {budget}",
                    self.behaviour
                )));
            }
        }
        Ok(())
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
        // otherwise wrapping a gated tool in a behaviour would launder it.
        if let Some(route) = self.app.route_for_tool(tool) {
            if route.approval == webcortex_core::manifest::Approval::Required {
                return Err(BehaviourHalted::new_err(format!(
                    "tool {tool:?} requires human approval and cannot run unattended \
                     inside a behaviour"
                )));
            }
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
            max_tokens: max_tokens.unwrap_or(self.max_tokens),
            handoffs: Vec::new(),
            context: Vec::new(),
            cache: false,
            policy: Default::default(),
        }
    }

    /// Turn a provider response into what `ask` returns: structured output
    /// when a schema was given (recovered from prose if a local model answered
    /// in text), otherwise the text.
    fn settle_ask(&self, py: Python<'_>, response: webcortex_core::agent::ProviderResponse, structured: bool, elapsed: u64) -> PyResult<Py<PyAny>> {
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
        )?;

        if structured {
            let value = response
                .tool_calls
                .iter()
                .find(|c| c.name == "respond")
                .map(|c| c.arguments.clone())
                .or_else(|| recover_json(&response.text));
            return match value {
                Some(value) => {
                    self.record("ask", "structured", value.clone(), elapsed);
                    json_to_py(py, &value)
                }
                None => Err(PyRuntimeError::new_err(format!(
                    "model did not return structured output; it said: {}",
                    if response.text.is_empty() { "<nothing>" } else { &response.text }
                ))),
            };
        }

        self.record(
            "ask",
            "text",
            serde_json::json!({"chars": response.text.len(), "model": response.model}),
            elapsed,
        );
        Ok(response.text.into_pyobject(py)?.into_any().unbind())
    }
}

/// One `(tool, kwargs)` request to `gather`.
fn parse_gather_item(item: &Bound<'_, PyAny>) -> PyResult<(String, serde_json::Value)> {
    if let Ok(t) = item.cast::<pyo3::types::PyTuple>() {
        if t.len() == 1 || t.len() == 2 {
            let name: String = t.get_item(0)?.extract()?;
            let args = if t.len() == 2 { py_to_json(&t.get_item(1)?)? } else { serde_json::json!({}) };
            if !args.is_object() {
                return Err(PyValueError::new_err("gather: the second element must be a dict of arguments"));
            }
            return Ok((name, args));
        }
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

        // Release the GIL: the tool call may take real time, and other worker
        // threads must keep running while it does.
        let result = py.detach(|| {
            self.handle.block_on(async move {
                app.call_tool_in_tree(&tool_name, &args, &principal, depth, Some(budget))
                    .await
            })
        });

        self.settle_call(py, tool, result, started.elapsed().as_millis() as u64)
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

        let results: Vec<Result<serde_json::Value, String>> = py.detach(|| {
            self.handle.block_on(async move {
                let futures = calls.iter().map(|(name, args)| {
                    app.call_tool_in_tree(name, args, &principal, depth, Some(budget.clone()))
                });
                futures::future::join_all(futures).await
            })
        });
        let elapsed = started.elapsed().as_millis() as u64;

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
    /// values rather than on parsed prose. `model` may be an alias such as
    /// `"fast"`: use the cheap tier for classification leaves.
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
        self.charge_step()?;

        let schema_json = match schema {
            Some(s) => Some(py_to_json(s)?),
            None => None,
        };
        let def = self.agent_def(model, system, max_tokens, temperature);
        let mut conversation = Conversation::default();
        conversation.messages.push(Message {
            role: "user".into(),
            content: serde_json::json!(prompt),
        });
        // A schema is expressed as a single tool the model must use. That is
        // what turns "please answer as JSON" from a request into a guarantee.
        let tools: Vec<ToolSpec> = match &schema_json {
            Some(s) => vec![ToolSpec {
                name: "respond".into(),
                description: "Respond with structured output in the required shape.".into(),
                input_schema: s.clone(),
            }],
            None => Vec::new(),
        };
        let structured = schema_json.is_some();

        let started = std::time::Instant::now();
        let provider = self.provider.clone();
        let response = py.detach(|| {
            self.handle.block_on(async {
                let req = CompletionRequest {
                    agent: &def,
                    conversation: &conversation,
                    tools: &tools,
                    force_tool: if structured { Some("respond") } else { None },
                    system_suffix: "",
                };
                provider.complete(&req).await
            })
        });
        let elapsed = started.elapsed().as_millis() as u64;

        let response = response.map_err(|e| {
            self.record("ask_failed", "model", serde_json::json!({"error": &e}), elapsed);
            PyRuntimeError::new_err(e)
        })?;
        self.settle_ask(py, response, structured, elapsed)
    }

    /// Ask the model many questions at once.
    ///
    /// The prompts are sent concurrently, at most `concurrency` in flight, and
    /// the answers come back in order. This is the loop
    /// `for ticket in tickets: ctx.ask(...)` collapsed into one wait — the same
    /// tokens, a fraction of the wall-clock. Each prompt is one step and is
    /// charged like a single `ask`.
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
        for _ in &prompts {
            self.charge_step()?;
        }
        let schema_json = match schema {
            Some(s) => Some(py_to_json(s)?),
            None => None,
        };
        let structured = schema_json.is_some();
        let def = self.agent_def(model, system, max_tokens, temperature);
        let tools: Vec<ToolSpec> = match &schema_json {
            Some(s) => vec![ToolSpec {
                name: "respond".into(),
                description: "Respond with structured output in the required shape.".into(),
                input_schema: s.clone(),
            }],
            None => Vec::new(),
        };

        let started = std::time::Instant::now();
        let provider = self.provider.clone();
        let limit = concurrency.clamp(1, 64);
        let responses = py.detach(|| {
            self.handle.block_on(async {
                use futures::StreamExt;
                let conversations: Vec<Conversation> = prompts
                    .iter()
                    .map(|p| Conversation {
                        messages: vec![Message { role: "user".into(), content: serde_json::json!(p) }],
                    })
                    .collect();
                futures::stream::iter(conversations.iter())
                    .map(|conversation| {
                        let provider = provider.clone();
                        let def = &def;
                        let tools = &tools;
                        async move {
                            let req = CompletionRequest {
                                agent: def,
                                conversation,
                                tools,
                                force_tool: if structured { Some("respond") } else { None },
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
        let elapsed = started.elapsed().as_millis() as u64;

        let list = PyList::empty(py);
        for response in responses {
            let response = response.map_err(|e| {
                self.record("ask_failed", "model", serde_json::json!({"error": &e}), elapsed);
                PyRuntimeError::new_err(e)
            })?;
            list.append(self.settle_ask(py, response, structured, elapsed)?)?;
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
        let result = py.detach(|| {
            self.handle.block_on(async move {
                webcortex_core::context::resolve_one(&app, &def, &principal, &input).await
            })
        });
        let elapsed = started.elapsed().as_millis() as u64;
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

pub fn py_to_json(value: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    use pyo3::types::{PyBool, PyFloat, PyInt, PyString};

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
        return Ok(serde_json::Value::from(i.extract::<i64>()?));
    }
    if let Ok(f) = value.cast::<PyFloat>() {
        return Ok(serde_json::Value::from(f.extract::<f64>()?));
    }
    if let Ok(d) = value.cast::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (k, v) in d.iter() {
            map.insert(k.str()?.to_str()?.to_string(), py_to_json(&v)?);
        }
        return Ok(serde_json::Value::Object(map));
    }
    if let Ok(list) = value.cast::<PyList>() {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            out.push(py_to_json(&item)?);
        }
        return Ok(serde_json::Value::Array(out));
    }
    if let Ok(tuple) = value.cast::<pyo3::types::PyTuple>() {
        let mut out = Vec::with_capacity(tuple.len());
        for item in tuple.iter() {
            out.push(py_to_json(&item)?);
        }
        return Ok(serde_json::Value::Array(out));
    }

    // Dataclasses, pydantic models, and anything else with a standard hook.
    for attr in ["model_dump", "_asdict"] {
        if let Ok(method) = value.getattr(attr) {
            if method.is_callable() {
                return py_to_json(&method.call0()?);
            }
        }
    }
    if let Ok(d) = value.getattr("__dict__") {
        if d.cast::<PyDict>().is_ok() {
            return py_to_json(&d);
        }
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
