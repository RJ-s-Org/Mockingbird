//! LLM provider abstraction and an OpenAI-compatible client.

use std::ops::Add;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;

pub mod fallback;
pub mod openai;
#[cfg(feature = "testing")]
pub mod scripted;

pub use fallback::{FallbackProvider, HopSettings};
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
}

/// Text deltas of a streamed completion, in arrival order. A transport or
/// decode error mid-stream arrives as one `Err` item, after which the stream
/// ends.
pub type TextStream = BoxStream<'static, Result<String, LlmError>>;

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
}
