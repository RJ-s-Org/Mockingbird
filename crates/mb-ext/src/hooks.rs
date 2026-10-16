//! Admin extension routes: the `AdminHooks` trait is part of the semver-stable
//! surface `mockingbird-cloud` builds against.

/// Contributes extra routes to the admin API.
///
/// The routes receive only authenticated requests: the host wraps them in
/// the same authentication middleware as the Admin API, so a request
/// without valid credentials is answered `401` before it reaches them. A
/// handler reads the caller from the request extensions
/// (`axum::Extension<mb_auth::Principal>`) and does its own authorization
/// (role and workspace checks).
pub trait AdminHooks: Send + Sync {
    /// Extra routes merged into the admin router, behind authentication.
    fn routes(&self) -> Option<axum::Router> {
        None
    }
}

/// Self-hosted default: no extra admin routes.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl AdminHooks for NoHooks {}
