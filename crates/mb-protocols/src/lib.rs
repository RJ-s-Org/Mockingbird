//! Protocol adapters that expose a compiled contract as a live mock.
//!
//! Each adapter sits behind a cargo feature (`http-rest`, ...).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use mb_engine::{Engine, ScenarioSpec};
use mb_ext::Extensions;
use mb_mcm::Bundle;
use mb_state::StateStore;

#[cfg(feature = "http-rest")]
pub mod http_rest;
mod record;

pub use record::{RequestRecord, RequestRecorder};

/// A saved scenario as the data plane applies it (spec §7.1).
#[derive(Clone, Debug, Default)]
pub struct ScenarioRuntime {
    /// Instructions and `set` patch handed to the engine.
    pub spec: ScenarioSpec,
    /// Status to answer with unless `X-Mockingbird-Status` overrides it.
    pub status: Option<u16>,
    /// Delay before responding unless `X-Mockingbird-Latency` overrides it.
    pub latency: Option<Duration>,
    /// Operation ids the scenario applies to; empty means every operation.
    pub operations: Vec<String>,
}

impl ScenarioRuntime {
    /// Whether the scenario applies to `operation_id`.
    pub fn applies_to(&self, operation_id: &str) -> bool {
        self.operations.is_empty() || self.operations.iter().any(|o| o == operation_id)
    }
}

/// Everything an adapter needs to serve one mock.
pub struct MockRuntime {
    /// Mock name; also used as `SessionKey::mock`.
    pub name: String,
    pub bundle: Arc<Bundle>,
    pub engine: Arc<Engine>,
    pub state: Arc<dyn StateStore>,
    /// Honour `X-Mockingbird-*` override headers. Defaults to `true`.
    pub header_overrides: bool,
    /// Scenarios selectable with `X-Mockingbird-Scenario`, by name.
    pub scenarios: HashMap<String, ScenarioRuntime>,
    /// Tenancy, admission and usage hooks.
    pub ext: Extensions,
    /// Receives one [`RequestRecord`] per served call.
    pub recorder: Option<Arc<dyn RequestRecorder>>,
    /// Set while the state store is failing; cleared by the next successful
    /// state call. Calls are served statelessly meanwhile.
    pub state_degraded: Arc<AtomicBool>,
}

impl MockRuntime {
    /// A runtime with header overrides enabled, no scenarios, the
    /// self-hosted extensions and no recorder.
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
            scenarios: HashMap::new(),
            ext: Extensions::default(),
            recorder: None,
            state_degraded: Arc::new(AtomicBool::new(false)),
        }
    }
}
