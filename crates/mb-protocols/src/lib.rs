//! Protocol adapters that expose a compiled contract as a live mock.
//!
//! Each adapter sits behind a cargo feature (`http-rest`, ...).

use std::sync::Arc;

use mb_engine::Engine;
use mb_mcm::Bundle;
use mb_state::StateStore;

#[cfg(feature = "http-rest")]
pub mod http_rest;

/// Everything an adapter needs to serve one mock.
pub struct MockRuntime {
    /// Mock name; also used as `SessionKey::mock`.
    pub name: String,
    pub bundle: Arc<Bundle>,
    pub engine: Arc<Engine>,
    pub state: Arc<dyn StateStore>,
    /// Honour `X-Mockingbird-*` override headers. Defaults to `true`.
    pub header_overrides: bool,
}

impl MockRuntime {
    /// A runtime with header overrides enabled.
    pub fn new(
        name: impl Into<String>,
        bundle: Arc<Bundle>,
        engine: Arc<Engine>,
        state: Arc<dyn StateStore>,
    ) -> Self {
        Self {
            name: name.into(),
            bundle,
            engine,
            state,
            header_overrides: true,
        }
    }
}
