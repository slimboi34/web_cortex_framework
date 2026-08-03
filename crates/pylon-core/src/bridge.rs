//! The seam between the runtime and any embedded interpreter.
//!
//! `pylon-core` never links against Python. It talks to this trait, which the
//! `pylon-py` crate implements over PyO3. That keeps the runtime testable
//! without an interpreter and leaves room for other host languages later.

use crate::http::{PylonRequest, PylonResponse};
use futures::future::BoxFuture;

pub trait PyBridge: Send + Sync + 'static {
    /// Dispatch to the Python handler registered at `handler`. Implementations
    /// are expected to be non-blocking from the caller's perspective: the work
    /// lands on an interpreter worker and completes via channel.
    fn call<'a>(
        &'a self,
        handler: u32,
        req: PylonRequest,
    ) -> BoxFuture<'a, Result<PylonResponse, String>>;

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
        _req: PylonRequest,
    ) -> BoxFuture<'a, Result<PylonResponse, String>> {
        Box::pin(async move {
            Err(format!(
                "route requires Python handler {handler}, but this runtime was built without an interpreter"
            ))
        })
    }
}
