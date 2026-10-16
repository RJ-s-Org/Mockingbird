//! Identity for the admin API: roles and permissions, principals, mapping of
//! identity-provider claims to roles, API keys and server-side admin
//! sessions. This crate never depends on axum; extractors and middleware
//! live in `mb-admin`.

mod apikey;
mod authenticator;
mod error;
mod mapping;
mod principal;
mod role;
mod session;

pub use apikey::{
    API_KEY_PREFIX, ApiKeys, GeneratedKey, KeyScope, NewKeyRequest, generate_key, hash_secret,
    parse_token,
};
pub use authenticator::{Authenticate, Authenticator, BearerVerifier, LocalAuth};
pub use error::AuthError;
pub use mapping::RoleMapping;
pub use principal::{Principal, PrincipalKind};
pub use role::{Permission, Role};
pub use session::{SESSION_COOKIE, SessionManager};

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
