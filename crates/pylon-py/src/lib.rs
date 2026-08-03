//! `pylon._core` — the Python-facing surface of the runtime.
//!
//! Python's job is to describe the application and to supply handler callables.
//! Rust owns the event loop, the socket, and the router. When a `Python` op
//! fires, work is handed to the Python-side dispatcher, which spreads it across
//! free-threaded interpreter workers and calls back into [`Completer`].

use pylon_core::{App, Manifest, PylonRequest, PylonResponse, PyBridge};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use std::sync::{Arc, Mutex};

/// Handed to Python with each dispatched request. Calling it exactly once
/// resolves the pending Rust future.
#[pyclass]
struct Completer {
    tx: Mutex<Option<tokio::sync::oneshot::Sender<Result<PylonResponse, String>>>>,
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
        let mut hs = vec![("content-type".to_string(), content_type)];
        if let Some(extra) = headers {
            hs.extend(extra);
        }
        self.send(Ok(PylonResponse {
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
}

impl Completer {
    fn send(&self, value: Result<PylonResponse, String>) -> PyResult<()> {
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

impl PyBridge for PythonBridge {
    fn call<'a>(
        &'a self,
        handler: u32,
        req: PylonRequest,
    ) -> futures::future::BoxFuture<'a, Result<PylonResponse, String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();

        let submitted = Python::attach(|py| -> PyResult<()> {
            let completer = Py::new(
                py,
                Completer {
                    tx: Mutex::new(Some(tx)),
                },
            )?;
            let request = request_to_py(py, &req)?;
            self.dispatcher
                .bind(py)
                .call_method1("submit", (handler, request, completer))?;
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

    fn workers(&self) -> usize {
        self.workers
    }
}

fn request_to_py<'py>(py: Python<'py>, req: &PylonRequest) -> PyResult<Bound<'py, PyDict>> {
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

    let headers = PyDict::new(py);
    for (k, v) in &req.headers {
        headers.set_item(k, v)?;
    }
    d.set_item("headers", headers)?;

    d.set_item("body", PyBytes::new(py, &req.body))?;
    d.set_item("scopes", req.scopes.clone())?;
    Ok(d)
}

/// Build the app and serve forever. Blocks the calling thread with the GIL
/// released, so Python worker threads run unimpeded.
#[pyfunction]
#[pyo3(signature = (manifest_json, dispatcher, workers = 0))]
fn serve(py: Python<'_>, manifest_json: &str, dispatcher: Py<PyAny>, workers: usize) -> PyResult<()> {
    let manifest: Manifest = serde_json::from_str(manifest_json)
        .map_err(|e| PyValueError::new_err(format!("invalid manifest: {e}")))?;

    pylon_core::init_tracing("info");

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
            pylon_core::server::serve(Arc::new(app))
                .await
                .map_err(PyRuntimeError::new_err)
        })
    })
}

/// Validate a manifest and return the derived route/tool table without binding
/// a socket. Used by `pylon check` and by tests.
#[pyfunction]
fn inspect_manifest(manifest_json: &str) -> PyResult<String> {
    let manifest: Manifest = serde_json::from_str(manifest_json)
        .map_err(|e| PyValueError::new_err(format!("invalid manifest: {e}")))?;
    manifest
        .validate()
        .map_err(|e| PyValueError::new_err(format!("invalid application: {e}")))?;

    let report = serde_json::json!({
        "name": manifest.name,
        "routes": manifest.routes.len(),
        "native_routes": manifest.routes.iter()
            .filter(|r| !matches!(r.op, pylon_core::manifest::Op::Python { .. }))
            .count(),
        "tools": manifest.routes.iter().filter(|r| r.tool.expose)
            .map(|r| r.tool_name()).collect::<Vec<_>>(),
        "agents": manifest.agents.iter().map(|a| &a.name).collect::<Vec<_>>(),
        "openapi": pylon_core::openapi::generate(&manifest),
    });
    Ok(report.to_string())
}

/// Render the OpenAPI document for a manifest without starting a server.
#[pyfunction]
fn openapi_for(manifest_json: &str) -> PyResult<String> {
    let manifest: Manifest = serde_json::from_str(manifest_json)
        .map_err(|e| PyValueError::new_err(format!("invalid manifest: {e}")))?;
    Ok(pylon_core::openapi::generate(&manifest).to_string())
}

// `gil_used = false` marks this extension as free-threading compatible, which is
// what lets CPython 3.13+/3.14 skip re-enabling the GIL when it is imported.
#[pymodule(gil_used = false)]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Completer>()?;
    m.add_function(wrap_pyfunction!(serve, m)?)?;
    m.add_function(wrap_pyfunction!(inspect_manifest, m)?)?;
    m.add_function(wrap_pyfunction!(openapi_for, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
