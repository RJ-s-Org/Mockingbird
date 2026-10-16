//! Admin extension routes: the `AdminHooks` trait is part of the semver-stable
//! surface `mockingbird-cloud` builds against.

/// Contributes extra routes to the admin API.
pub trait AdminHooks: Send + Sync {
    fn routes(&self) -> Option<axum::Router> {
        None
    }
}

/// Self-hosted default: no extra admin routes.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl AdminHooks for NoHooks {}
