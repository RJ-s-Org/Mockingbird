//! Identity for the admin API: roles and permissions, principals, mapping of
//! identity-provider claims to roles, API keys, server-side admin sessions,
//! the OIDC relying party and bearer-JWT verification. This crate never
//! depends on axum; extractors and middleware live in `mb-admin`.
//!
//! Every admin request resolves to a [`Principal`] through an
//! [`Authenticate`] implementation ([`LocalAuth`] in `auth.mode: none`,
//! [`Authenticator`] in `oidc` mode), and its workspace [`Role`] decides
//! which [`Permission`]s it has:
//!
//! ```
//! use mb_auth::{Permission, Principal, Role};
//!
//! let role: Role = "editor".parse()?;
//! assert!(role.allows(Permission::Edit));
//! assert!(!role.allows(Permission::ManageApiKeys));
//!
//! // `auth.mode: none`: the local user is a sysadmin, Owner everywhere.
//! let local = Principal::local();
//! assert_eq!(local.role_in("default"), Some(Role::Owner));
//! # Ok::<(), mb_auth::ParseRoleError>(())
//! ```
//!
//! Secrets stay out of `Debug` output: [`GeneratedKey`] redacts its token,
//! [`OidcSettings`] its client secret, and the stateful types
//! ([`ApiKeys`], [`SessionManager`], [`OidcClient`]) show only their
//! settings.
//!
//! The `testing` feature adds `testing::MockOidcProvider`, a wiremock
//! OpenID provider for tests.

#![warn(
    missing_debug_implementations,
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

mod apikey;
mod apikey_cache;
mod authenticator;
mod error;
mod http;
mod jwt;
mod mapping;
mod oidc;
mod principal;
mod role;
mod session;
#[cfg(feature = "testing")]
pub mod testing;

pub use apikey::{
    API_KEY_CACHE_CAPACITY, API_KEY_PREFIX, ApiKeys, DEFAULT_API_KEY_CACHE_TTL, GeneratedKey,
    KeyScope, NewKeyRequest, generate_key, hash_secret, parse_token,
};
pub use authenticator::{Authenticate, Authenticator, BearerVerifier, LocalAuth};
pub use error::AuthError;
pub use http::{HttpError, http_client, http_client_with, send, try_http_client};
pub use jwt::{ALLOWED_ALGS, JwtVerifier};
pub use mapping::RoleMapping;
pub use oidc::{
    CompletedLogin, LoginRedirect, MAX_RETURN_TO, OidcClient, OidcSettings, validate_return_to,
};
pub use principal::{Principal, PrincipalKind};
pub use role::{ParseRoleError, Permission, Role};
pub use session::{CookieSecurity, SESSION_COOKIE, SessionManager};

/// Lowercase hex encoding of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Hex sha256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(bytes))
}
