//! LLM provider abstraction and an OpenAI-compatible client.

use std::ops::Add;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;

pub mod fallback;
pub mod http;
pub mod openai;
#[cfg(feature = "testing")]
pub mod scripted;

pub use fallback::{FallbackProvider, HopSettings};
pub use http::{HttpOptions, http_client};
pub use openai::{OpenAiCompatible, OpenAiConfig};
#[cfg(feature = "testing")]
pub use scripted::ScriptedProvider;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Clone, Debug)]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema { name: String, schema: Value },
}

#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub response_format: ResponseFormat,
    pub temperature: f32,
    pub max_tokens: u32,
    pub timeout: Duration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

impl Add for Usage {
    type Output = Usage;

    /// Saturating, so a misreporting provider cannot overflow a total.
    fn add(self, rhs: Usage) -> Usage {
        Usage {
            prompt_tokens: self.prompt_tokens.saturating_add(rhs.prompt_tokens),
            completion_tokens: self.completion_tokens.saturating_add(rhs.completion_tokens),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChatResponse {
    pub content: String,
    pub usage: Usage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub json_schema: bool,
    pub json_mode: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("LLM request timed out")]
    Timeout,
    #[error("LLM returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("LLM transport error: {0}")]
    Transport(String),
    #[error("LLM response decode error: {0}")]
    Decode(String),
    /// The provider stopped at `max_tokens`, so the output is cut short.
    /// Retrying with the same limit would truncate again.
    #[error("LLM output truncated: {0}")]
    Truncated(String),
    /// The reply carried no usable content: an empty answer, or reasoning
    /// with no JSON in it.
    #[error("LLM returned no content: {0}")]
    Empty(String),
}

impl LlmError {
    /// Whether one more attempt may succeed: transport failures, throttling
    /// and server errors (including errors reported inside a 200 reply), and
    /// empty replies.
    pub fn is_retryable(&self) -> bool {
        match self {
            LlmError::Transport(_) | LlmError::Empty(_) => true,
            LlmError::Http { status, .. } => *status == 408 || *status == 429 || *status >= 500,
            LlmError::Timeout | LlmError::Decode(_) | LlmError::Truncated(_) => false,
        }
    }
}

/// Text deltas of a streamed completion, in arrival order. A transport or
/// decode error mid-stream arrives as one `Err` item, after which the stream
/// ends.
pub type TextStream = BoxStream<'static, Result<String, LlmError>>;

/// The usage a provider reports for a streamed completion, filled in by the
/// stream (typically from its final chunk) and readable by the caller once
/// the stream has ended. Clones share one value.
#[derive(Clone, Debug, Default)]
pub struct StreamUsage(Arc<Mutex<Option<Usage>>>);

impl StreamUsage {
    /// Nothing reported yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the reported usage (a later report replaces it).
    pub fn set(&self, usage: Usage) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(usage);
    }

    /// The reported usage; `None` while nothing has been reported (the
    /// stream has not ended, or the provider reports none).
    pub fn get(&self) -> Option<Usage> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A streamed completion ([`Provider::stream_metered`]): its text deltas
/// and the usage its provider reports.
pub struct MeteredStream {
    pub text: TextStream,
    pub usage: StreamUsage,
}

impl std::fmt::Debug for MeteredStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeteredStream")
            .field("usage", &self.usage)
            .finish_non_exhaustive()
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError>;

    /// The longest a [`Provider::complete`] call with `request_timeout` may
    /// take: the request timeout for a single provider; the sum over every
    /// hop for a [`FallbackProvider`]. Callers bound the call by this.
    fn total_timeout(&self, request_timeout: Duration) -> Duration {
        request_timeout
    }

    /// Streams the completion of `req` as text deltas. The default calls
    /// [`Provider::complete`] once and yields its content as one delta.
    async fn stream(&self, req: ChatRequest) -> Result<TextStream, LlmError> {
        let response = self.complete(req).await?;
        Ok(Box::pin(futures::stream::once(std::future::ready(Ok(
            response.content,
        )))))
    }

    /// [`Provider::stream`], with the usage the provider reports for it.
    /// The default reports none, so callers estimate it.
    async fn stream_metered(&self, req: ChatRequest) -> Result<MeteredStream, LlmError> {
        Ok(MeteredStream {
            text: self.stream(req).await?,
            usage: StreamUsage::new(),
        })
    }
}
