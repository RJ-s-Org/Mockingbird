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
use mb_state::StateStore;
use mb_store::ControlStore;
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::json;

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
}

/// - `/healthz`: liveness, always `200 {"status":"ok"}`.
/// - `/readyz`: `503 unavailable` when the control store is unreachable;
///   `200 degraded` (with `reasons`) when the state store is unreachable, a
///   mock failed to rebuild (`mock <label>: <error>`, labelled by
///   [`mock_label`](crate::registry::mock_label); it keeps serving its
///   previous version) or a mounted mock is serving statelessly; else
///   `200 ready`. Each store ping is bounded by [`PING_TIMEOUT`]. With
///   [`OpsState::readyz_auth`] set, `reasons` are left out for callers
///   without valid credentials.
/// - `/metrics`: Prometheus text exposition.
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
        .with_state(st)
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
            for (mock, error) in st.registry.failed_mocks() {
                reasons.push(format!("mock {mock}: {error}"));
            }
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
    let show_reasons = !reasons.is_empty()
        && match &st.readyz_auth {
            None => true,
            Some(auth) => matches!(auth.authenticate(&headers).await, Ok(Some(_))),
        };
    let body = if show_reasons {
        json!({"status": status, "reasons": reasons})
    } else {
        json!({"status": status})
    };
    (code, axum::Json(body)).into_response()
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
