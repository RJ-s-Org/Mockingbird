//! A provider that retries a failed request on a secondary provider
//! (spec §10: LLM failure → fallback profile → faker).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;

use crate::{Capabilities, ChatRequest, ChatResponse, LlmError, Provider, TextStream};

/// The request settings of one hop of a fallback chain (its profile's
/// timeout, temperature and token limit), applied to the request before it
/// is sent to that hop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HopSettings {
    pub timeout: Duration,
    pub temperature: f32,
    pub max_tokens: u32,
}

impl HopSettings {
    /// `req` with this hop's timeout, temperature and token limit.
    pub fn apply(&self, mut req: ChatRequest) -> ChatRequest {
        req.timeout = self.timeout;
        req.temperature = self.temperature;
        req.max_tokens = self.max_tokens;
        req
    }
}

/// Sends each request to `primary`; on any error, sends the same request to
/// `secondary` and returns its result. Each side may carry its own
/// [`HopSettings`]; without them it gets the request as given.
pub struct FallbackProvider {
    primary: Arc<dyn Provider>,
    primary_settings: Option<HopSettings>,
    secondary: Arc<dyn Provider>,
    secondary_settings: Option<HopSettings>,
}

impl FallbackProvider {
    pub fn new(primary: Arc<dyn Provider>, secondary: Arc<dyn Provider>) -> Self {
        Self {
            primary,
            primary_settings: None,
            secondary,
            secondary_settings: None,
        }
    }

    /// Requests to the primary use `settings`.
    pub fn with_primary_settings(mut self, settings: HopSettings) -> Self {
        self.primary_settings = Some(settings);
        self
    }

    /// Requests to the secondary use `settings`.
    pub fn with_secondary_settings(mut self, settings: HopSettings) -> Self {
        self.secondary_settings = Some(settings);
        self
    }
}

/// `req` with `settings` applied, if any.
fn for_hop(settings: Option<HopSettings>, req: ChatRequest) -> ChatRequest {
    match settings {
        Some(s) => s.apply(req),
        None => req,
    }
}

/// The total timeout of one side, given the caller's request timeout.
fn hop_timeout(
    provider: &dyn Provider,
    settings: Option<HopSettings>,
    request: Duration,
) -> Duration {
    provider.total_timeout(settings.map_or(request, |s| s.timeout))
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
        let primary_req = for_hop(self.primary_settings, req.clone());
        match self.primary.complete(primary_req).await {
            Ok(response) => Ok(response),
            Err(error) => {
                tracing::warn!(%error, "primary LLM provider failed; trying fallback provider");
                self.secondary
                    .complete(for_hop(self.secondary_settings, req))
                    .await
            }
        }
    }

    /// Opens the primary's stream. When opening it fails, its first item is
    /// an error, or it ends before yielding any content, returns the
    /// secondary's stream instead. Once the primary
    /// has yielded a delta it is never switched, so its later errors reach
    /// the caller.
    async fn stream(&self, req: ChatRequest) -> Result<TextStream, LlmError> {
        let primary_req = for_hop(self.primary_settings, req.clone());
        let error = match self.primary.stream(primary_req).await {
            Ok(mut stream) => match stream.next().await {
                Some(Ok(first)) => {
                    return Ok(Box::pin(
                        futures::stream::once(std::future::ready(Ok(first))).chain(stream),
                    ));
                }
                Some(Err(error)) => error,
                None => LlmError::Empty("the stream ended before any content".to_string()),
            },
            Err(error) => error,
        };
        tracing::warn!(%error, "primary LLM provider stream failed; trying fallback provider");
        self.secondary
            .stream(for_hop(self.secondary_settings, req))
            .await
    }

    /// The primary's and the secondary's total timeouts, added.
    fn total_timeout(&self, request_timeout: Duration) -> Duration {
        hop_timeout(
            self.primary.as_ref(),
            self.primary_settings,
            request_timeout,
        )
        .saturating_add(hop_timeout(
            self.secondary.as_ref(),
            self.secondary_settings,
            request_timeout,
        ))
    }
}
