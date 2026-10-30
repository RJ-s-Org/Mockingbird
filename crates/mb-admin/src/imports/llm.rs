//! How the admin API reaches an LLM for imports and draft previews. The
//! binary implements [`LlmResolver`] on top of its profile chain, so this
//! crate never resolves profiles or secrets itself.

use std::sync::Arc;

use async_trait::async_trait;
use mb_engine::{Engine, EngineConfig};
use mb_ext::{Rejection, Tenancy};
use mb_import::smart::ExtractLlm;
use mb_store::Mock;

/// Why [`LlmResolver::import_llm_for`] or
/// [`LlmResolver::preview_engine_for`] gave no LLM access.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum LlmAccessError {
    /// The provider policy refused the call's tenancy; answered as its
    /// problem (status, title, detail and `Retry-After`).
    Rejected(Rejection),
    /// The LLM could not be set up: a missing profile, a profile chain that
    /// does not build, a secret that does not resolve. Never a secret.
    Unavailable(String),
}

impl std::fmt::Display for LlmAccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(r) => r.fmt(f),
            Self::Unavailable(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for LlmAccessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected(r) => Some(r),
            Self::Unavailable(_) => None,
        }
    }
}

/// Resolves the LLM access of a mock.
#[async_trait]
pub trait LlmResolver: Send + Sync {
    /// Provider for extraction calls on `mock`; Ok(None) = no LLM (offline or no profile).
    async fn for_import(&self, mock: &Mock) -> Result<Option<ExtractLlm>, String>;
    /// The engine previews of `mock` are generated with.
    async fn preview_engine(&self, mock: &Mock) -> Result<Arc<Engine>, String>;

    /// [`LlmResolver::for_import`] for an import billed to `tenancy`: the
    /// provider policy chooses the profile for it. The default ignores
    /// `tenancy` and calls [`LlmResolver::for_import`].
    ///
    /// # Errors
    ///
    /// [`LlmAccessError::Rejected`] when the provider policy refuses
    /// `tenancy`; [`LlmAccessError::Unavailable`] when the LLM cannot be set
    /// up.
    async fn import_llm_for(
        &self,
        mock: &Mock,
        tenancy: &Tenancy,
    ) -> Result<Option<ExtractLlm>, LlmAccessError> {
        let _ = tenancy;
        self.for_import(mock)
            .await
            .map_err(LlmAccessError::Unavailable)
    }

    /// [`LlmResolver::preview_engine`] for a preview billed to `tenancy`:
    /// the provider policy chooses the profile for it. The default ignores
    /// `tenancy` and calls [`LlmResolver::preview_engine`].
    ///
    /// # Errors
    ///
    /// As [`LlmResolver::import_llm_for`].
    async fn preview_engine_for(
        &self,
        mock: &Mock,
        tenancy: &Tenancy,
    ) -> Result<Arc<Engine>, LlmAccessError> {
        let _ = tenancy;
        self.preview_engine(mock)
            .await
            .map_err(LlmAccessError::Unavailable)
    }
}

/// No LLM at all: imports of documentation are refused and previews use an
/// engine without a provider.
#[derive(Clone, Copy, Debug, Default)]
pub struct OfflineLlm;

#[async_trait]
impl LlmResolver for OfflineLlm {
    async fn for_import(&self, _mock: &Mock) -> Result<Option<ExtractLlm>, String> {
        Ok(None)
    }

    async fn preview_engine(&self, _mock: &Mock) -> Result<Arc<Engine>, String> {
        Ok(Arc::new(Engine::new(None, EngineConfig::default())))
    }
}
