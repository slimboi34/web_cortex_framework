//! `webcortex._core` — the Python-facing surface of the runtime.
//!
//! Python's job is to describe the application and to supply handler callables.
//! Rust owns the event loop, the socket, and the router. When a `Python` op
//! fires, work is handed to the Python-side dispatcher, which spreads it across
//! free-threaded interpreter workers and calls back into [`Completer`].

mod behaviour;

use behaviour::{BehaviourContext, BehaviourHalted, json_to_py, py_to_json};
use webcortex_core::{App, Manifest, WebCortexRequest, WebCortexResponse, PyBridge};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use std::sync::{Arc, Mutex};

/// Handed to Python with each dispatched request. Calling it exactly once
/// resolves the pending Rust future.
#[pyclass]
struct Completer {
    tx: Mutex<Option<tokio::sync::oneshot::Sender<Result<WebCortexResponse, String>>>>,
}

#[pymethods]
impl Completer {
    /// Deliver a successful response. `headers` is an optional list of pairs.
    #[pyo3(signature = (status, body, content_type = "application/json".to_string(), headers = None))]
    fn complete(
        &self,
        status: u16,
        body: &[u8],
        content_type: String,
        headers: Option<Vec<(String, String)>>,
    ) -> PyResult<()> {
        // Checked here as well as in `_bridge._encode`, because anything that
        // reaches the HTTP layer invalid becomes a bare-text 500 with no code
        // and no clue which handler produced it. Refusing here raises in the
        // Python caller, which then fails the request with the reason.
        if !(100..=599).contains(&status) {
            return Err(PyValueError::new_err(format!(
                "invalid HTTP status {status}: a response status is from 100 to 599"
            )));
        }
        let mut hs = vec![("content-type".to_string(), content_type)];
        if let Some(extra) = headers {
            hs.extend(extra);
        }
        for (name, value) in &hs {
            if let Some(problem) = header_problem(name, value) {
                return Err(PyValueError::new_err(format!("response header {name:?}: {problem}")));
            }
        }
        self.send(Ok(WebCortexResponse {
            status,
            headers: hs,
            body: bytes::Bytes::copy_from_slice(body),
        }))
    }

    /// Deliver a handler failure. The message reaches the client as a 500 and
    /// the log as an error; the Python side is responsible for deciding how
    /// much of the traceback is safe to include.
    fn fail(&self, message: String) -> PyResult<()> {
        self.send(Err(message))
    }

    /// True once nobody is waiting for this response: the request timed out
    /// or the client went away. An actuator queued that long is not started.
    fn abandoned(&self) -> bool {
        self.tx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_none_or(|tx| tx.is_closed())
    }
}

/// The emergency stop, handed to Python with each actuator call so the
/// dispatcher can check it as the handler starts rather than when it queued.
#[pyclass]
struct ActuatorGate {
    gate: webcortex_core::bridge::HaltGate,
}

#[pymethods]
impl ActuatorGate {
    /// The 423 message while the stop is engaged, else `None`.
    fn refusal(&self) -> Option<String> {
        self.gate.refusal()
    }
}

/// Why `name: value` cannot be sent as an HTTP header, if it cannot: an empty
/// or non-token name, or a value with a control character (CR/LF would split
/// the response).
fn header_problem(name: &str, value: &str) -> Option<&'static str> {
    let token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
    if name.is_empty() || !name.chars().all(token) {
        return Some("the name is not a valid header name");
    }
    if value.chars().any(|c| c.is_control() && c != '\t' && (c as u32) < 0x80) {
        return Some("the value contains a control character");
    }
    None
}

impl Completer {
    fn send(&self, value: Result<WebCortexResponse, String>) -> PyResult<()> {
        let mut guard = self
            .tx
            .lock()
            .map_err(|_| PyRuntimeError::new_err("completer lock poisoned"))?;
        match guard.take() {
            Some(tx) => {
                // The receiver is gone if the client disconnected first; that is
                // normal, not an error worth propagating into Python.
                let _ = tx.send(value);
                Ok(())
            }
            None => Err(PyRuntimeError::new_err(
                "completer already used; a handler resolved its request twice",
            )),
        }
    }
}

/// Bridges [`PyBridge`] onto the Python-side dispatcher object.
struct PythonBridge {
    dispatcher: Py<PyAny>,
    workers: usize,
}

impl PythonBridge {
    fn submit<'a>(
        &'a self,
        handler: u32,
        req: WebCortexRequest,
        gate: Option<webcortex_core::bridge::HaltGate>,
    ) -> futures::future::BoxFuture<'a, Result<WebCortexResponse, String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();

        let submitted = Python::attach(|py| -> PyResult<()> {
            let completer = Py::new(
                py,
                Completer {
                    tx: Mutex::new(Some(tx)),
                },
            )?;
            let request = request_to_py(py, &req)?;
            let gate = gate.map(|gate| Py::new(py, ActuatorGate { gate })).transpose()?;
            self.dispatcher
                .bind(py)
                .call_method1("submit", (handler, request, completer, gate))?;
            Ok(())
        });

        Box::pin(async move {
            if let Err(e) = submitted {
                return Err(format!("failed to submit request to Python: {e}"));
            }
            match rx.await {
                Ok(result) => result,
                Err(_) => Err("Python handler dropped the request without responding".to_string()),
            }
        })
    }
}

impl PyBridge for PythonBridge {
    fn call<'a>(
        &'a self,
        handler: u32,
        req: WebCortexRequest,
    ) -> futures::future::BoxFuture<'a, Result<WebCortexResponse, String>> {
        self.submit(handler, req, None)
    }

    fn call_actuator<'a>(
        &'a self,
        handler: u32,
        req: WebCortexRequest,
        gate: webcortex_core::bridge::HaltGate,
    ) -> futures::future::BoxFuture<'a, Result<WebCortexResponse, String>> {
        self.submit(handler, req, Some(gate))
    }

    #[allow(clippy::too_many_arguments)]
    fn call_behaviour<'a>(
        &'a self,
        app: Arc<App>,
        def: webcortex_core::manifest::BehaviourDef,
        input: serde_json::Value,
        principal: webcortex_core::auth::Principal,
        depth: u32,
        budget: Arc<webcortex_core::agent::SharedBudget>,
        trace: Option<webcortex_core::trace::TraceCtx>,
    ) -> futures::future::BoxFuture<'a, Result<serde_json::Value, String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        // Captured here, on a tokio thread. The behaviour later runs on a Python
        // worker thread and uses this handle to re-enter the runtime.
        let handle = tokio::runtime::Handle::current();

        let submitted = Python::attach(|py| -> PyResult<()> {
            let ctx = BehaviourContext::new(
                app.clone(),
                handle,
                principal,
                app.provider().clone(),
                uuid::Uuid::new_v4().to_string(),
                def.name.clone(),
                def.tools.clone(),
                def.context.clone(),
                def.max_steps,
                def.token_budget,
                def.model.clone(),
                def.max_tokens,
                def.temperature,
                def.reasoning.clone(),
                depth,
                budget,
                input.clone(),
                def.options.clone(),
                trace,
            );
            let completer = Py::new(
                py,
                ValueCompleter { tx: Mutex::new(Some(tx)) },
            )?;
            let py_input = json_to_py(py, &input)?;
            self.dispatcher.bind(py).call_method1(
                "submit_behaviour",
                (def.handler, Py::new(py, ctx)?, py_input, completer),
            )?;
            Ok(())
        });

        Box::pin(async move {
            if let Err(e) = submitted {
                return Err(format!("failed to submit behaviour to Python: {e}"));
            }
            match rx.await {
                Ok(result) => result,
                Err(_) => Err("behaviour dropped without producing a result".to_string()),
            }
        })
    }

    fn workers(&self) -> usize {
        self.workers
    }
}

/// Resolves a behaviour run with a JSON value rather than an HTTP response.
#[pyclass]
struct ValueCompleter {
    tx: Mutex<Option<tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>>>,
}

#[pymethods]
impl ValueCompleter {
    fn complete(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let json = py_to_json(value)?;
        self.send(Ok(json))
    }

    fn fail(&self, message: String) -> PyResult<()> {
        self.send(Err(message))
    }
}

impl ValueCompleter {
    fn send(&self, value: Result<serde_json::Value, String>) -> PyResult<()> {
        let mut guard = self
            .tx
            .lock()
            .map_err(|_| PyRuntimeError::new_err("completer lock poisoned"))?;
        match guard.take() {
            Some(tx) => {
                let _ = tx.send(value);
                Ok(())
            }
            None => Err(PyRuntimeError::new_err("behaviour completed twice")),
        }
    }
}

fn request_to_py<'py>(py: Python<'py>, req: &WebCortexRequest) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("method", &req.method)?;
    d.set_item("path", &req.path)?;

    let params = PyDict::new(py);
    for (k, v) in &req.path_params {
        params.set_item(k, v)?;
    }
    d.set_item("path_params", params)?;

    let query = PyDict::new(py);
    for (k, v) in &req.query {
        query.set_item(k, v)?;
    }
    d.set_item("query", query)?;
    // Every pair, repeats included, for `Request.query_list` and list-typed params.
    let pairs: Vec<(&str, &str)> = req.query_pairs();
    d.set_item("query_all", pairs)?;

    let headers = PyDict::new(py);
    for (k, v) in &req.headers {
        headers.set_item(k, v)?;
    }
    d.set_item("headers", headers)?;

    d.set_item("body", PyBytes::new(py, &req.body))?;

    // The authenticated caller, so a handler can make its own decisions without
    // re-deriving identity from headers.
    let user = PyDict::new(py);
    user.set_item("id", &req.principal.id)?;
    user.set_item("authenticated", !req.principal.is_anonymous())?;
    user.set_item("scopes", req.principal.scopes.clone())?;
    d.set_item("user", user)?;
    d.set_item("scopes", req.principal.scopes.clone())?;
    Ok(d)
}

/// Build the app and serve forever. Blocks the calling thread with the GIL
/// released, so Python worker threads run unimpeded.
#[pyfunction]
#[pyo3(signature = (manifest_json, dispatcher, workers = 0))]
fn serve(py: Python<'_>, manifest_json: &str, dispatcher: Py<PyAny>, workers: usize) -> PyResult<()> {
    let manifest: Manifest = serde_json::from_str(manifest_json)
        .map_err(|e| PyValueError::new_err(format!("invalid manifest: {e}")))?;

    webcortex_core::init_tracing("info");

    let bridge = Arc::new(PythonBridge { dispatcher, workers });

    py.detach(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;

        rt.block_on(async move {
            let app = App::build(manifest, bridge)
                .await
                .map_err(PyRuntimeError::new_err)?;
            // `into_arc` rather than `Arc::new`: behaviours need the App's own
            // Arc in order to call tools.
            webcortex_core::server::serve(app.into_arc())
                .await
                .map_err(PyRuntimeError::new_err)
        })
    })
}

/// Validate a manifest and return the derived route/tool table without binding
/// a socket. Used by `webcortex check` and by tests.
#[pyfunction]
fn inspect_manifest(manifest_json: &str) -> PyResult<String> {
    let manifest: Manifest = serde_json::from_str(manifest_json)
        .map_err(|e| PyValueError::new_err(format!("invalid manifest: {e}")))?;
    manifest
        .validate()
        .map_err(|e| PyValueError::new_err(format!("invalid application: {e}")))?;
    // Templates are loaded and parsed exactly as a boot would, so a page whose
    // template is missing or does not parse fails the check, not only `run`.
    webcortex_core::app::load_templates(&manifest)
        .map_err(|e| PyValueError::new_err(format!("invalid application: {e}")))?;

    let report = serde_json::json!({
        "name": manifest.name,
        "routes": manifest.routes.len(),
        "native_routes": manifest.routes.iter()
            .filter(|r| !matches!(r.op, webcortex_core::manifest::Op::Python { .. }))
            .count(),
        "tools": manifest.routes.iter().filter(|r| r.tool.expose)
            .map(|r| r.tool_name()).collect::<Vec<_>>(),
        "agents": manifest.agents.iter().map(|a| &a.name).collect::<Vec<_>>(),
        "behaviours": manifest.behaviours.iter().map(|b| &b.name).collect::<Vec<_>>(),
        "flows": manifest.flows.iter().map(|f| &f.name).collect::<Vec<_>>(),
        "contexts": manifest.contexts.iter().map(|c| &c.name).collect::<Vec<_>>(),

    });
    Ok(report.to_string())
}

/// Render the OpenAPI document for a manifest without starting a server.
#[pyfunction]
fn openapi_for(manifest_json: &str) -> PyResult<String> {
    let manifest: Manifest = serde_json::from_str(manifest_json)
        .map_err(|e| PyValueError::new_err(format!("invalid manifest: {e}")))?;
    Ok(webcortex_core::openapi::generate(&manifest).to_string())
}

/// Generate a dependency-free typed TypeScript client.
#[pyfunction]
fn typescript_client(manifest_json: &str) -> PyResult<String> {
    let manifest: Manifest = serde_json::from_str(manifest_json)
        .map_err(|e| PyValueError::new_err(format!("invalid manifest: {e}")))?;
    manifest
        .validate()
        .map_err(|e| PyValueError::new_err(format!("invalid application: {e}")))?;
    Ok(webcortex_core::typegen::generate(&manifest))
}

/// Mint an API key suitable for handing to a client.
#[pyfunction]
fn generate_api_key() -> String {
    webcortex_core::auth::generate_api_key()
}

/// One model completion, outside any server, using the app's model
/// configuration (aliases, providers) and the process environment.
///
/// This is what `webcortex evolve` uses: the same resolution rules as the
/// runtime, so `model="fast"` means the same thing on the command line as it
/// does in a behaviour. Returns the text, or the JSON string of structured
/// output when `schema_json` is given.
#[pyfunction]
#[pyo3(signature = (manifest_json, model, system, prompt, schema_json = None, max_tokens = 8192))]
fn ask_model(
    py: Python<'_>,
    manifest_json: &str,
    model: &str,
    system: &str,
    prompt: &str,
    schema_json: Option<&str>,
    max_tokens: u32,
) -> PyResult<String> {
    let manifest: Manifest = serde_json::from_str(manifest_json)
        .map_err(|e| PyValueError::new_err(format!("invalid manifest: {e}")))?;
    let schema: Option<serde_json::Value> = match schema_json {
        Some(s) => Some(serde_json::from_str(s).map_err(|e| PyValueError::new_err(format!("invalid schema: {e}")))?),
        None => None,
    };
    let model = model.to_string();
    let system = system.to_string();
    let prompt = prompt.to_string();

    py.detach(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;
        rt.block_on(async move {
            use webcortex_core::agent::provider::{CompletionRequest, ModelProvider};
            use webcortex_core::agent::{Conversation, Message, ToolSpec};

            let registry = webcortex_core::agent::ProviderRegistry::from_env(&manifest.models);
            let def = webcortex_core::manifest::AgentDef {
                name: "webcortex-cli".into(),
                description: String::new(),
                model,
                system,
                tools: Vec::new(),
                max_steps: Some(1),
                token_budget: None,
                scopes: Vec::new(),
                temperature: None,
                reasoning: None,
                max_tokens,
                handoffs: Vec::new(),
                context: Vec::new(),
                cache: false,
                policy: Default::default(),
                options: Default::default(),
                output_schema: None,
            };
            let conversation = Conversation {
                messages: vec![Message { role: "user".into(), content: serde_json::json!(prompt) }],
            };
            let tools: Vec<ToolSpec> = match &schema {
                Some(s) => vec![ToolSpec {
                    name: "respond".into(),
                    description: "Respond in the required shape.".into(),
                    input_schema: s.clone(),
                }],
                None => Vec::new(),
            };
            let req = CompletionRequest {
                agent: &def,
                conversation: &conversation,
                tools: &tools,
                force_tool: schema.as_ref().map(|_| "respond"),
                system_suffix: "",
            };
            let res = registry.complete(&req).await.map_err(PyRuntimeError::new_err)?;
            if schema.is_some() {
                let value = res
                    .tool_calls
                    .iter()
                    .find(|c| c.name == "respond")
                    .map(|c| c.arguments.clone())
                    .or_else(|| webcortex_core::agent::recover_json(&res.text))
                    .ok_or_else(|| PyRuntimeError::new_err(format!(
                        "model did not return structured output; it said: {}", res.text
                    )))?;
                return Ok(value.to_string());
            }
            Ok(res.text)
        })
    })
}

/// Run one eval: build the app, call one of its tools in-process as the
/// principal `eval` holding `scopes`, and return what happened as JSON.
///
/// `mode` chooses where model responses come from: `"live"` asks the app's
/// providers (or the fake one under `WEBCORTEX_FAKE_PROVIDER`), `"record"`
/// does the same and returns the responses as a cassette, and `"replay"`
/// answers from `cassette_json` with no network. Tools always run for real.
///
/// Nothing is bound to a port. The result is
/// `{"status", "body", "error", "duration_ms", "usage", "trace", "cassette"?,
/// "replay"?}`: `status` and `body` are the HTTP status and JSON body the tool's
/// route produced, and `error` is set instead when the call could not be made.
/// An unreadable cassette or an unknown mode raises `ValueError` (`WC320`).
#[pyfunction]
#[pyo3(signature = (manifest_json, dispatcher, workers, tool, args_json, scopes, mode = "live", cassette_json = None))]
#[allow(clippy::too_many_arguments)]
fn eval_call(
    py: Python<'_>,
    manifest_json: &str,
    dispatcher: Py<PyAny>,
    workers: usize,
    tool: &str,
    args_json: &str,
    scopes: Vec<String>,
    mode: &str,
    cassette_json: Option<&str>,
) -> PyResult<String> {
    use webcortex_core::agent::replay::{RecordingProvider, ReplayProvider};

    let manifest: Manifest = serde_json::from_str(manifest_json)
        .map_err(|e| PyValueError::new_err(format!("invalid manifest: {e}")))?;
    let args: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|e| PyValueError::new_err(format!("WC320: the eval's input is not JSON: {e}")))?;
    let replayer = match (mode, cassette_json) {
        ("replay", Some(text)) => {
            let cassette: serde_json::Value = serde_json::from_str(text)
                .map_err(|e| PyValueError::new_err(format!("WC320: the cassette is not valid JSON: {e}")))?;
            Some(Arc::new(
                ReplayProvider::from_cassette(&cassette).map_err(|e| PyValueError::new_err(format!("WC320: {e}")))?,
            ))
        }
        ("replay", None) => return Err(PyValueError::new_err("WC320: replay needs a cassette")),
        ("live" | "record", _) => None,
        (other, _) => {
            return Err(PyValueError::new_err(format!(
                "WC320: unknown eval mode {other:?}; use live, record or replay"
            )));
        }
    };
    let record = mode == "record";
    let tool = tool.to_string();

    // Quiet by default: an eval run prints a report, not a server log.
    webcortex_core::init_tracing("error");
    let bridge = Arc::new(PythonBridge { dispatcher, workers });

    py.detach(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;
        rt.block_on(async move {
            let app = App::build(manifest, bridge).await.map_err(PyRuntimeError::new_err)?;
            let recorder = record.then(|| Arc::new(RecordingProvider::new(app.provider().clone())));
            let app = match (&recorder, &replayer) {
                (Some(r), _) => app.with_provider(r.clone()),
                (None, Some(r)) => app.with_provider(r.clone()),
                (None, None) => app,
            };
            let limit = std::time::Duration::from_secs(
                app.manifest.server.agent_timeout_secs.max(app.manifest.server.request_timeout_secs).max(1),
            );
            let app = app.into_arc();
            let principal = webcortex_core::Principal {
                id: "eval".into(),
                kind: webcortex_core::PrincipalKind::ApiKey,
                scopes,
                claims: Default::default(),
            };
            let started = std::time::Instant::now();
            let call = app.tool_response(&tool, &args, &principal, 0, None, None);
            let outcome = match tokio::time::timeout(limit, call).await {
                Ok(outcome) => outcome,
                Err(_) => Err(format!("the eval did not finish within {} s", limit.as_secs())),
            };
            let mut out = serde_json::json!({
                "duration_ms": started.elapsed().as_millis() as u64,
                "usage": app.ledger().snapshot(|m| app.registry().price_for(m)),
                "trace": app.tracer().latest().and_then(|id| app.tracer().get(&id)),
            });
            match outcome {
                Ok(res) => {
                    out["status"] = serde_json::json!(res.status);
                    out["body"] = res.json_value();
                    out["error"] = serde_json::Value::Null;
                }
                Err(e) => {
                    out["status"] = serde_json::Value::Null;
                    out["body"] = serde_json::Value::Null;
                    out["error"] = serde_json::json!(e);
                }
            }
            if let Some(r) = &recorder {
                out["cassette"] = r.cassette();
            }
            if let Some(r) = &replayer {
                out["replay"] = r.stats();
            }
            Ok(out.to_string())
        })
    })
}

/// The error catalogue as JSON: `[{code, title, category, status, hint}]`.
/// The Rust table is the single source; `webcortex explain` reads it here.
#[pyfunction]
fn error_catalogue() -> String {
    webcortex_core::errors::catalogue_json().to_string()
}

/// How every model the app names would resolve right now, from the manifest
/// and the environment, without calling anything: the runtime's own routing,
/// so `webcortex doctor` reports exactly the error a run would fail with.
#[pyfunction]
fn diagnose_models(manifest_json: &str, models: Vec<String>) -> PyResult<String> {
    let manifest: Manifest = serde_json::from_str(manifest_json)
        .map_err(|e| PyValueError::new_err(format!("invalid manifest: {e}")))?;
    let registry = webcortex_core::ProviderRegistry::from_env(&manifest.models);
    let out: Vec<serde_json::Value> = models.iter().map(|m| registry.diagnose(m)).collect();
    Ok(serde_json::json!({"fake": registry.describe()["fake"], "models": out}).to_string())
}

// `gil_used = false` marks this extension as free-threading compatible, which is
// what lets CPython 3.13+/3.14 skip re-enabling the GIL when it is imported.
#[pymodule(gil_used = false)]
fn _core(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Completer>()?;
    m.add_class::<ValueCompleter>()?;
    m.add_class::<BehaviourContext>()?;
    m.add("BehaviourHalted", py.get_type::<BehaviourHalted>())?;
    m.add_function(wrap_pyfunction!(serve, m)?)?;
    m.add_function(wrap_pyfunction!(inspect_manifest, m)?)?;
    m.add_function(wrap_pyfunction!(openapi_for, m)?)?;
    m.add_function(wrap_pyfunction!(typescript_client, m)?)?;
    m.add_function(wrap_pyfunction!(generate_api_key, m)?)?;
    m.add_function(wrap_pyfunction!(ask_model, m)?)?;
    m.add_function(wrap_pyfunction!(eval_call, m)?)?;
    m.add_function(wrap_pyfunction!(error_catalogue, m)?)?;
    m.add_function(wrap_pyfunction!(diagnose_models, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
