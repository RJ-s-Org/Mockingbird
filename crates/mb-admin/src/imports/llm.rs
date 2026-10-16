//! How the admin API reaches an LLM for imports and draft previews. The
//! binary implements [`LlmResolver`] on top of its profile chain, so this
//! crate never resolves profiles or secrets itself.

use std::sync::Arc;

use async_trait::async_trait;
use mb_engine::{Engine, EngineConfig};
use mb_import::smart::ExtractLlm;
use mb_store::Mock;

/// Resolves the LLM access of a mock.
#[async_trait]
pub trait LlmResolver: Send + Sync {
    /// Provider for extraction calls on `mock`; Ok(None) = no LLM (offline or no profile).
    async fn for_import(&self, mock: &Mock) -> Result<Option<ExtractLlm>, String>;
    /// The engine previews of `mock` are generated with.
    async fn preview_engine(&self, mock: &Mock) -> Result<Arc<Engine>, String>;
}

/// No LLM at all: imports of documentation are refused and previews use an
/// engine without a provider.
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
