//! The Admin API's LLM access ([`mb_admin::LlmResolver`]) on top of the
//! runtime's profile chain.

use std::sync::Arc;

use async_trait::async_trait;
use mb_engine::Engine;
use mb_import::smart::ExtractLlm;
use mb_store::Mock;

use crate::runtime::{RuntimeDeps, build_engine, import_provider};

/// Resolves import and preview LLM access the way the data plane does,
/// with `import_profile` (`import.llm_profile`) overriding the mock's
/// profile for imports.
pub struct RuntimeLlm {
    pub deps: RuntimeDeps,
    pub import_profile: Option<String>,
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
}
