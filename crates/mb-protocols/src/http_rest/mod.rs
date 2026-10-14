//! REST adapter: serves a bundle's HTTP operations through one axum router.

use std::sync::Arc;

use axum::Router;
use axum::http::HeaderName;
use tower_http::cors::{Any, CorsLayer};

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
///
/// Mocks are meant to be called from browsers, so a permissive CORS layer
/// answers preflights and exposes `X-Mockingbird-Source`.
pub fn try_router(rt: Arc<MockRuntime>) -> Result<Router, RouteError> {
    let state = handler::RestState::new(rt)?;
    Ok(Router::new()
        .fallback(handler::handle)
        .with_state(Arc::new(state))
        .layer(cors()))
}

fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([HeaderName::from_static("x-mockingbird-source")])
}
