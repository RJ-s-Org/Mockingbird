//! A provider that retries a failed request on a secondary provider
//! (spec §10: LLM failure → fallback profile → faker).

use std::sync::Arc;

use async_trait::async_trait;

use crate::{Capabilities, ChatRequest, ChatResponse, LlmError, Provider};

/// Sends each request to `primary`; on any error, sends the same request to
/// `secondary` and returns its result.
pub struct FallbackProvider {
    primary: Arc<dyn Provider>,
    secondary: Arc<dyn Provider>,
}

impl FallbackProvider {
    pub fn new(primary: Arc<dyn Provider>, secondary: Arc<dyn Provider>) -> Self {
        Self { primary, secondary }
    }
}

#[async_trait]
impl Provider for FallbackProvider {
    /// Only what both providers support, since either may serve a request.
    fn capabilities(&self) -> Capabilities {
        let (p, s) = (self.primary.capabilities(), self.secondary.capabilities());
        Capabilities {
            json_schema: p.json_schema && s.json_schema,
            json_mode: p.json_mode && s.json_mode,
        }
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        match self.primary.complete(req.clone()).await {
            Ok(response) => Ok(response),
            Err(error) => {
                tracing::warn!(%error, "primary LLM provider failed; trying fallback provider");
                self.secondary.complete(req).await
            }
        }
    }
}
