//! The seam between the runtime and any embedded interpreter.
//!
//! `webcortex-core` never links against Python. It talks to this trait, which the
//! `webcortex-py` crate implements over PyO3. That keeps the runtime testable
//! without an interpreter and leaves room for other host languages later.

use crate::http::{WebCortexRequest, WebCortexResponse};
use futures::future::BoxFuture;

pub trait PyBridge: Send + Sync + 'static {
    /// Dispatch to the Python handler registered at `handler`. Implementations
    /// are expected to be non-blocking from the caller's perspective: the work
    /// lands on an interpreter worker and completes via channel.
    fn call<'a>(
        &'a self,
        handler: u32,
        req: WebCortexRequest,
    ) -> BoxFuture<'a, Result<WebCortexResponse, String>>;

    /// Run a Behaviour.
    ///
    /// Distinct from [`Self::call`] because a behaviour receives a context
    /// object rather than a request: it needs to reach back into the runtime for
    /// tool and model calls, which a plain handler never does. `budget` is the
    /// request tree's; the runtime creates it when the behaviour is outermost.
    fn call_behaviour<'a>(
        &'a self,
        _app: std::sync::Arc<crate::App>,
        def: crate::manifest::BehaviourDef,
        _input: serde_json::Value,
        _principal: crate::auth::Principal,
        _depth: u32,
        _budget: std::sync::Arc<crate::agent::SharedBudget>,
    ) -> BoxFuture<'a, Result<serde_json::Value, String>> {
        Box::pin(async move {
            Err(format!(
                "behaviour {:?} requires an interpreter, but this runtime was built without one",
                def.name
            ))
        })
    }

    /// How many interpreter workers are live. Reported on the health endpoint.
    fn workers(&self) -> usize {
        0
    }
}

/// Used when an app declares no Python handlers at all — a pure
/// declaration-only service, which is a supported and very fast mode.
pub struct NoBridge;

impl PyBridge for NoBridge {
    fn call<'a>(
        &'a self,
        handler: u32,
        _req: WebCortexRequest,
    ) -> BoxFuture<'a, Result<WebCortexResponse, String>> {
        Box::pin(async move {
            Err(format!(
                "route requires Python handler {handler}, but this runtime was built without an interpreter"
            ))
        })
    }
}
