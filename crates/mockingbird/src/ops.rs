//! Ops endpoints on the admin port: `/healthz`, `/readyz` and `/metrics`.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
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
}

/// - `/healthz`: liveness, always `200 {"status":"ok"}`.
/// - `/readyz`: `503 unavailable` when the control store is unreachable;
///   `200 degraded` (with `reasons`) when the state store is unreachable, a
///   mock failed to rebuild (`mock <name>: <error>`; it keeps serving its
///   previous version) or a mounted mock is serving statelessly; else
///   `200 ready`. Each store ping is bounded by [`PING_TIMEOUT`].
/// - `/metrics`: Prometheus text exposition.
pub fn router(st: OpsState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
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

async fn readyz(State(st): State<OpsState>) -> Response {
    // Both pings run at once, each bounded, so readyz answers in ~2s at most.
    let (control, state) = tokio::join!(ping_within(st.store.ping()), ping_within(st.state.ping()));
    if let Err(error) = control {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({
                "status": "unavailable",
                "reasons": [format!("control store unreachable: {error}")],
            })),
        )
            .into_response();
    }
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
    let body = if reasons.is_empty() {
        json!({"status": "ready"})
    } else {
        json!({"status": "degraded", "reasons": reasons})
    };
    axum::Json(body).into_response()
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
