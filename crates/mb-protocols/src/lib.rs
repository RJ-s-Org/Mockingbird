//! Protocol adapters that expose a compiled contract as a live mock.
//!
//! Each adapter sits behind a cargo feature (`http-rest`, `sse`, `graphql`,
//! `grpc`, `soap`, `websocket`); all of them share the [`call`] pipeline.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mb_engine::{Engine, ScenarioSpec};
use mb_ext::Extensions;
use mb_mcm::Bundle;
use mb_state::StateStore;

// Until an adapter is compiled in, nothing calls the shared pipeline.
#[cfg_attr(
    not(any(
        feature = "http-rest",
        feature = "graphql",
        feature = "grpc",
        feature = "soap"
    )),
    allow(dead_code)
)]
mod call;
#[cfg(feature = "graphql")]
pub mod graphql;
#[cfg(feature = "grpc")]
pub mod grpc;
#[cfg(feature = "http-rest")]
pub mod http_rest;
mod record;
#[cfg(feature = "soap")]
pub mod soap;
#[cfg(feature = "websocket")]
pub mod ws;

pub use record::{RequestRecord, RequestRecorder};

/// Request header capping how many items a streamed response carries
/// (an integer in `1..=10000`; the engine clamps it to its own maximum).
pub const STREAM_ITEMS_HEADER: &str = "x-mockingbird-stream-items";

/// A request extension carrying the mount prefix the dispatcher stripped
/// before forwarding the request to a mock's router (e.g. `/calc`).
#[derive(Clone, Debug)]
pub struct MountPrefix(pub String);

/// Protocol knobs shared by every mock (the `protocols:` config section).
#[derive(Clone, Debug)]
pub struct ProtocolSettings {
    /// Path the GraphQL endpoint is served on, below the mount prefix.
    pub graphql_path: String,
    /// Whether GraphQL introspection queries are answered.
    pub graphql_introspection: bool,
    pub graphql_max_depth: usize,
    pub graphql_max_complexity: usize,
    /// Interval between SSE keepalive comments.
    pub sse_keepalive: Duration,
    /// Interval between WebSocket pings.
    pub ws_keepalive: Duration,
    /// Largest gRPC message decoded or encoded.
    pub grpc_max_message_bytes: usize,
}

impl Default for ProtocolSettings {
    fn default() -> Self {
        Self {
            graphql_path: "/graphql".into(),
            graphql_introspection: true,
            graphql_max_depth: 15,
            graphql_max_complexity: 1000,
            sse_keepalive: Duration::from_secs(15),
            ws_keepalive: Duration::from_secs(30),
            grpc_max_message_bytes: 4 * 1024 * 1024,
        }
    }
}

/// A saved scenario as the data plane applies it (spec §7.1).
#[derive(Clone, Debug, Default)]
pub struct ScenarioRuntime {
    /// Instructions and `set` patch handed to the engine.
    pub spec: ScenarioSpec,
    /// Status to answer with unless `X-Mockingbird-Status` overrides it;
    /// ignored for operations that do not declare it.
    pub status: Option<u16>,
    /// Delay before responding unless `X-Mockingbird-Latency` overrides it.
    pub latency: Option<Duration>,
    /// Operation ids the scenario applies to; empty means every operation.
    pub operations: Vec<String>,
    /// Items per streamed response unless `X-Mockingbird-Stream-Items`
    /// overrides it.
    pub stream_items: Option<usize>,
    /// Pause between streamed items, overriding the engine's default.
    pub stream_item_interval: Option<Duration>,
}

impl ScenarioRuntime {
    /// Whether the scenario applies to `operation_id`.
    pub fn applies_to(&self, operation_id: &str) -> bool {
        self.operations.is_empty() || self.operations.iter().any(|o| o == operation_id)
    }
}

/// A circuit breaker over a mock's state store calls.
///
/// While healthy every call uses the store. The first failure marks the
/// mock degraded; from then on at most one call per retry interval tries
/// the store (the others are served statelessly at once), until one
/// succeeds and clears the mark.
#[derive(Debug)]
pub struct StateHealth {
    degraded: AtomicBool,
    /// When a call last tried the store while degraded.
    last_attempt: Mutex<Option<Instant>>,
    retry_interval: Duration,
}

impl StateHealth {
    /// Default time between store attempts while degraded.
    pub const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_secs(5);

    pub fn new(retry_interval: Duration) -> Self {
        Self {
            degraded: AtomicBool::new(false),
            last_attempt: Mutex::new(None),
            retry_interval,
        }
    }

    /// Whether the state store is failing for this mock.
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }

    /// Whether a call may use the store now: always while healthy; while
    /// degraded, only the first caller of each retry interval.
    pub fn try_acquire(&self) -> bool {
        if !self.is_degraded() {
            return true;
        }
        let Ok(mut last) = self.last_attempt.lock() else {
            return true;
        };
        let now = Instant::now();
        match *last {
            Some(at) if now.duration_since(at) < self.retry_interval => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    }

    /// A store call succeeded: the mock is healthy again.
    pub fn record_success(&self) {
        self.degraded.store(false, Ordering::Relaxed);
    }

    /// A store call failed: the mock is degraded, and the next attempt
    /// waits a full retry interval from now.
    pub fn record_failure(&self) {
        if let Ok(mut last) = self.last_attempt.lock() {
            *last = Some(Instant::now());
        }
        self.degraded.store(true, Ordering::Relaxed);
    }
}

impl Default for StateHealth {
    fn default() -> Self {
        Self::new(Self::DEFAULT_RETRY_INTERVAL)
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
    /// The state store circuit breaker: degraded while the store is
    /// failing, when calls are served statelessly.
    pub state_health: Arc<StateHealth>,
    /// Protocol knobs (GraphQL limits, keepalives, gRPC message size).
    pub protocols: ProtocolSettings,
}

impl MockRuntime {
    /// A runtime with header overrides enabled, no scenarios, the
    /// self-hosted extensions, no recorder and default protocol settings.
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
            state_health: Arc::new(StateHealth::default()),
            protocols: ProtocolSettings::default(),
        }
    }
}
