//! LLM provider selection: the `ProviderPolicy` trait is part of the
//! semver-stable surface `mockingbird-cloud` builds against.

use crate::{Rejection, Tenancy};

/// Chooses the LLM profile a call may use, given the mock's configured one.
/// `Ok(None)` means "no LLM profile" (the engine falls back accordingly).
pub trait ProviderPolicy: Send + Sync {
    fn choose(
        &self,
        t: &Tenancy,
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
        _t: &Tenancy,
        _mock: &str,
        configured: Option<&str>,
    ) -> Result<Option<String>, Rejection> {
        Ok(configured.map(str::to_string))
    }
}
