//! Ops endpoints on the admin port: `/healthz`, `/readyz` and `/metrics`.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use mb_admin::AdminState;
use mb_auth::Authenticate;
use mb_state::{Bus, StateStore};
use mb_store::ControlStore;
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::json;
use tokio::sync::watch;

use crate::file_config::Role;
use crate::registry::MockRegistry;

/// What the ops endpoints inspect.
#[derive(Clone)]
pub struct OpsState {
    pub store: Arc<dyn ControlStore>,
    pub state: Arc<dyn StateStore>,
    pub registry: Arc<MockRegistry>,
    pub prometheus: PrometheusHandle,
    /// Who may see `/readyz` `reasons`: `None` lets everyone (`auth.mode:
    /// none`); otherwise only callers it authenticates do, and anyone
    /// else gets just `{"status": ...}`.
    pub readyz_auth: Option<Arc<dyn Authenticate>>,
    /// The cross-node bus, whose health `/readyz` reports; `None` without
    /// one.
    pub bus: Option<Arc<dyn Bus>>,
    /// This node's role, which `/readyz` reports.
    pub role: Role,
    /// Whether this node is the leader (see
    /// [`Leadership::watch`](mb_state::leader::Leadership::watch)), which
    /// `/readyz` reports.
    pub leader: watch::Receiver<bool>,
    /// The outcome of the last `--watch` config reload.
    pub reload: crate::watch::ReloadStatus,
    /// The data plane this node serves, which `/readyz` reports so a client
    /// of the admin port (the MCP server's `call_mock`) can find it; `None`
    /// on a control node.
    pub data_plane: Option<DataPlane>,
    /// Which project this server was started for, which `/readyz` reports
    /// so that a local client (the MCP server) acts only on its own
    /// project's server.
    pub origin: Origin,
    /// Where this node's intercepting proxy listens (`proxy.enabled` on a
    /// node serving the data plane), which `/readyz` reports so that `proxy
    /// env` and `run` hand out this server's own proxy, never another one on
    /// the same port; `None` when it runs no proxy.
    pub proxy: Option<std::net::SocketAddr>,
    /// The LLM providers' health, which `/readyz` reports under `llm` (each
    /// profile's state and how many calls fell back because of it), with a
    /// reason for each provider marked unusable or backing off.
    pub llm_health: Arc<mb_llm::HealthRegistry>,
}

/// Where a server was started from, as `/readyz` reports it: its config
/// file (absolute; `None` without one) and its working folder.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Origin {
    pub config: Option<std::path::PathBuf>,
    pub dir: Option<std::path::PathBuf>,
}

impl Origin {
    /// The origin of this process, started with `config`.
    #[must_use]
    pub fn current(config: Option<&std::path::Path>) -> Self {
        Self {
            config: config.map(|c| std::fs::canonicalize(c).unwrap_or_else(|_| c.to_path_buf())),
            dir: std::env::current_dir().ok(),
        }
    }
}

/// Where a node's data plane listens, as `/readyz` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DataPlane {
    pub port: u16,
    pub tls: bool,
}

impl std::fmt::Debug for OpsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpsState")
            .field("role", &self.role)
            .field("leader", &*self.leader.borrow())
            .field("readyz_auth", &self.readyz_auth.is_some())
            .field("bus", &self.bus.is_some())
            .field("reload", &self.reload)
            .field("data_plane", &self.data_plane)
            .field("origin", &self.origin)
            .field("proxy", &self.proxy)
            .finish_non_exhaustive()
    }
}

/// - `/healthz`: liveness, always `200 {"status":"ok"}`.
/// - `/readyz`: `503 unavailable` when the control store is unreachable;
///   `200 degraded` (with `reasons`) when the state store is unreachable,
///   the last config reload failed (`config_reload: <error>`, until the
///   next good one), a mock failed to rebuild (`mock <label>: stale,
///   serving the previous version: <error>` while its previous version is
///   mounted, else `mock <label>: failed: <error>`; labelled by
///   [`mock_label`](crate::registry::mock_label)), a mock's channels are not all working (`mock
///   <label>: <reason>`, e.g. an unreachable broker), a mounted mock is
///   serving statelessly or a mock's daily token budget is exhausted
///   (`mock <label>: daily token budget exhausted`), or the cross-node bus
///   is disconnected (`bus: <reason>`); else `200 ready`. The body always
///   carries the node's `role`, whether it is the `leader`, the `config` file and working `dir` it was started with ([`Origin`]), its `proxy` address (`null` when it runs no proxy) and, on a node with a data plane, its `data_plane` (`port`, `tls`); like `reasons`, these only authenticated callers see when auth is on. Each store ping is bounded by
///   [`PING_TIMEOUT`]. With [`OpsState::readyz_auth`] set, `reasons` are
///   left out for callers without valid credentials.
/// - `/metrics`: Prometheus text exposition.
/// - `/proxy/hosts`: `{"hosts": [...]}`, the hosts the mounted mocks claim
///   (sorted), which a standalone `mockingbird proxy` intercepts.
pub fn router(st: OpsState) -> Router {
    router_with_metrics_gate(st, None)
}

/// [`router`], with `/metrics` behind [`mb_admin::console_gate`] when
/// `metrics_gate` is set (`auth.public_metrics: false`): it then needs the
/// same credentials as the Admin API.
pub fn router_with_metrics_gate(st: OpsState, metrics_gate: Option<AdminState>) -> Router {
    let metrics_route = match metrics_gate {
        Some(admin) => get(metrics).route_layer(axum::middleware::from_fn_with_state(
            admin,
            mb_admin::console_gate,
        )),
        None => get(metrics),
    };
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", metrics_route)
        .route("/proxy/hosts", get(proxy_hosts))
        .with_state(st)
}

/// The hosts the mounted mocks claim, for a standalone `mockingbird proxy`.
async fn proxy_hosts(State(st): State<OpsState>) -> Response {
    axum::Json(json!({"hosts": st.registry.claimed_hosts()})).into_response()
}

async fn healthz() -> Response {
    axum::Json(json!({"status": "ok"})).into_response()
}

/// Longest a `/readyz` store ping may take before it counts as failed.
pub const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Runs `ping` with [`PING_TIMEOUT`]; `Err` carries the reason.
async fn ping_within<E: std::fmt::Display>(
    ping: impl std::future::Future<Output = Result<(), E>>,
) -> Result<(), String> {
    match tokio::time::timeout(PING_TIMEOUT, ping).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(format!("no answer within {}s", PING_TIMEOUT.as_secs())),
    }
}

async fn readyz(State(st): State<OpsState>, headers: HeaderMap) -> Response {
    let llm = st.llm_health.snapshot();
    // Both pings run at once, each bounded, so readyz answers in ~2s at most.
    let (control, state) = tokio::join!(ping_within(st.store.ping()), ping_within(st.state.ping()));
    let (status, reasons) = match control {
        Err(error) => (
            "unavailable",
            vec![format!("control store unreachable: {error}")],
        ),
        Ok(()) => {
            let mut reasons = Vec::new();
            if let Err(error) = state {
                reasons.push(format!("state store unreachable: {error}"));
            }
            if let Some(Err(reason)) = st.bus.as_ref().map(|bus| bus.health()) {
                reasons.push(format!("bus: {reason}"));
            }
            if let Some(error) = st.reload.last_error() {
                reasons.push(format!("config_reload: {error}"));
            }
            reasons.extend(st.registry.mock_issues());
            for (mock, reason) in st.registry.event_issues() {
                reasons.push(format!("mock {mock}: {reason}"));
            }
            reasons.extend(st.registry.budget_issues());
            reasons.extend(llm.iter().filter_map(llm_issue));
            for mock in st.registry.degraded_mocks() {
                reasons.push(format!(
                    "mock {mock}: state store failing, serving statelessly"
                ));
            }
            let status = if reasons.is_empty() {
                "ready"
            } else {
                "degraded"
            };
            (status, reasons)
        }
    };
    let code = if status == "unavailable" {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    let authorized = match &st.readyz_auth {
        None => true,
        Some(auth) => matches!(auth.authenticate(&headers).await, Ok(Some(_))),
    };
    let show_reasons = !reasons.is_empty() && authorized;
    let leader = *st.leader.borrow();
    let mut body = json!({"status": status, "role": st.role.as_str(), "leader": leader});
    if show_reasons {
        body["reasons"] = json!(reasons);
    }
    if let Some(dp) = st.data_plane.filter(|_| authorized) {
        body["data_plane"] = json!(dp);
    }
    if authorized && !llm.is_empty() {
        body["llm"] = json!(llm);
    }
    if authorized {
        body["config"] = json!(st.origin.config);
        body["dir"] = json!(st.origin.dir);
        body["proxy"] = json!(st.proxy.map(|a| a.to_string()));
    }
    (code, axum::Json(body)).into_response()
}

/// The `/readyz` reason of a provider that is not called now: `llm profile
/// <workspace/name>: unusable (HTTP 402: ...), N calls fell back`, or
/// `backing off (HTTP 429) until <unix time>`; `None` for one that is ok.
fn llm_issue(h: &mb_llm::HealthSnapshot) -> Option<String> {
    let status = h.status.map(|s| format!("HTTP {s}")).unwrap_or_default();
    let state = match h.state {
        "unusable" => format!(
            "unusable for this run ({status}: {})",
            h.reason.as_deref().unwrap_or("-")
        ),
        "backing_off" => format!(
            "backing off ({status}) until unix time {}",
            h.until_unix.unwrap_or_default()
        ),
        _ => return None,
    };
    Some(format!(
        "llm profile {}: {state}; {} calls fell back to the faker without calling it",
        h.profile, h.fallbacks
    ))
}

async fn metrics(State(st): State<OpsState>) -> Response {
    st.prometheus.run_upkeep();
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        st.prometheus.render(),
    )
        .into_response()
}
