//! LLM provider selection: the `ProviderPolicy` trait is part of the
//! semver-stable surface `mockingbird-cloud` builds against.

use crate::{Rejection, Tenancy};

/// Chooses the LLM profile a call may use, given the mock's configured one.
/// `Ok(None)` means "no LLM profile" (the engine falls back accordingly).
///
/// Asked for every call once the [`TenantResolver`] has resolved its
/// tenancy, with that tenancy: data-plane calls, and the imports and
/// draft previews of the Admin API. A call whose answer is the configured
/// profile generates with the mock's own provider; another profile's
/// provider is built on first use and kept for the mock. A rejection is
/// answered as the call's problem, like a quota's. Being asked per call,
/// `choose` should not block or do I/O.
///
/// [`TenantResolver`]: crate::TenantResolver
pub trait ProviderPolicy: Send + Sync {
    /// The profile to use for a call on `mock` whose configured profile is
    /// `configured`, or a rejection.
    ///
    /// # Errors
    ///
    /// A [`Rejection`] when `tenancy` may not use an LLM profile for this
    /// call; the caller answers it as an RFC 9457 problem.
    fn choose(
        &self,
        tenancy: &Tenancy,
        mock: &str,
        configured: Option<&str>,
    ) -> Result<Option<String>, Rejection>;
}

/// Self-hosted default: use the mock's configured profile unchanged.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConfiguredProfile;

impl ProviderPolicy for ConfiguredProfile {
    fn choose(
        &self,
        _tenancy: &Tenancy,
        _mock: &str,
        configured: Option<&str>,
    ) -> Result<Option<String>, Rejection> {
        Ok(configured.map(str::to_string))
    }
}
