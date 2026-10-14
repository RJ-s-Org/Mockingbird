//! REST adapter: serves a bundle's HTTP operations through one axum router.

use std::sync::Arc;

use axum::Router;

use crate::MockRuntime;

mod handler;
pub mod problem;
pub mod routing;

pub use problem::problem;
pub use routing::{RouteError, RouteTable};

/// A router serving every HTTP operation of `rt.bundle`.
///
/// If the bundle's routes conflict, every request gets a 500 problem
/// response; use [`try_router`] to surface the error instead.
pub fn router(rt: Arc<MockRuntime>) -> Router {
    match try_router(rt) {
        Ok(router) => router,
        Err(e) => {
            tracing::error!(error = %e, "cannot build REST routes");
            let detail = e.to_string();
            Router::new().fallback(move || {
                let detail = detail.clone();
                async move { problem(500, "Invalid route table", detail) }
            })
        }
    }
}

/// Like [`router`], but fails when two operations claim the same route.
pub fn try_router(rt: Arc<MockRuntime>) -> Result<Router, RouteError> {
    let state = handler::RestState::new(rt)?;
    Ok(Router::new()
        .fallback(handler::handle)
        .with_state(Arc::new(state)))
}
