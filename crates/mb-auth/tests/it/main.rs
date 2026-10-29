//! The mockingbird-auth integration tests, in one binary: one module per
//! former `tests/<name>.rs` file. The OIDC tests need the `testing` feature
//! (wiremock and RSA keys), so only that module is gated on it.

mod core;
#[cfg(feature = "testing")]
mod oidc;
