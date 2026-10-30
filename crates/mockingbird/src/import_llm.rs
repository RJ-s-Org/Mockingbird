//! The Admin API's LLM access ([`mb_admin::LlmResolver`]) on top of the
//! runtime's profile chain.

use std::sync::Arc;

use async_trait::async_trait;
use mb_admin::LlmAccessError;
use mb_engine::Engine;
use mb_ext::{Rejection, Tenancy};
use mb_import::smart::ExtractLlm;
use mb_store::Mock;

use crate::runtime::{
    RuntimeDeps, build_engine, import_provider, import_provider_for, preview_engine_for,
};

/// Resolves import and preview LLM access the way the data plane does,
/// with `import_profile` (`import.llm_profile`) overriding the mock's
/// profile for imports. The provider policy chooses the profile for the
/// tenancy the Admin API resolved.
#[derive(Debug)]
pub struct RuntimeLlm {
    pub deps: RuntimeDeps,
    pub import_profile: Option<String>,
}

/// An LLM access failure: the provider policy's rejection, or the reason
/// the LLM cannot be set up.
fn access_error(e: &anyhow::Error) -> LlmAccessError {
    match e.downcast_ref::<Rejection>() {
        Some(r) => LlmAccessError::Rejected(r.clone()),
        None => LlmAccessError::Unavailable(format!("{e:#}")),
    }
}

#[async_trait]
impl mb_admin::LlmResolver for RuntimeLlm {
    async fn for_import(&self, mock: &Mock) -> Result<Option<ExtractLlm>, String> {
        import_provider(&self.deps, mock, self.import_profile.as_deref())
            .await
            .map_err(|e| format!("{e:#}"))
    }

    async fn preview_engine(&self, mock: &Mock) -> Result<Arc<Engine>, String> {
        build_engine(&self.deps, mock)
            .await
            .map(Arc::new)
            .map_err(|e| format!("{e:#}"))
    }

    async fn import_llm_for(
        &self,
        mock: &Mock,
        tenancy: &Tenancy,
    ) -> Result<Option<ExtractLlm>, LlmAccessError> {
        import_provider_for(&self.deps, mock, self.import_profile.as_deref(), tenancy)
            .await
            .map_err(|e| access_error(&e))
    }

    async fn preview_engine_for(
        &self,
        mock: &Mock,
        tenancy: &Tenancy,
    ) -> Result<Arc<Engine>, LlmAccessError> {
        preview_engine_for(&self.deps, mock, tenancy)
            .await
            .map(Arc::new)
            .map_err(|e| access_error(&e))
    }
}
