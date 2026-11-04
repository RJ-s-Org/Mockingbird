//! REST adapter: serves a bundle's HTTP operations through one axum router.

use std::sync::Arc;

use axum::Router;
use axum::http::HeaderName;
use tower_http::cors::{Any, CorsLayer};

use crate::MockRuntime;

mod form;
mod handler;
pub mod problem;
pub mod routing;
#[cfg(feature = "sse")]
mod sse;

pub use problem::problem;
pub use routing::{RouteError, RouteTable};

/// A router serving every HTTP operation of `rt.bundle`.
///
/// An operation whose route cannot be registered (it matches exactly the
/// requests of an earlier one, or is no valid pattern) is logged as a
/// warning and left out; the others are served. Use
/// [`router_and_skipped`] to get those operations instead.
pub fn router(rt: Arc<MockRuntime>) -> Router {
    let (router, skipped) = router_and_skipped(rt);
    for error in &skipped {
        tracing::warn!(operation = error.operation(), %error, "HTTP route skipped; the other operations are served");
    }
    router
}

/// Like [`router`], also returning the operations left out (see
/// [`RouteTable::skipped`]), without logging them.
///
/// Mocks are meant to be called from browsers, so a permissive CORS layer
/// answers preflights and exposes `X-Mockingbird-Source`.
pub fn router_and_skipped(rt: Arc<MockRuntime>) -> (Router, Vec<RouteError>) {
    let state = handler::RestState::new(rt);
    let skipped = state.skipped().to_vec();
    let router = Router::new()
        .fallback(handler::handle)
        .with_state(Arc::new(state))
        .layer(cors());
    (router, skipped)
}

fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([
            HeaderName::from_static("x-mockingbird-source"),
            HeaderName::from_static("x-mockingbird-fallback-reason"),
            handler::VALIDATION_HEADER,
        ])
}
