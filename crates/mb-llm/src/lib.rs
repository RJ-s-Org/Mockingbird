//! LLM provider abstraction and an OpenAI-compatible client.
//!
//! Internal crate of Mockingbird, published for `mockingbird-cli`, with no
//! API stability guarantee. [`Provider`] is the interface the engine calls;
//! [`OpenAiCompatible`] talks to any OpenAI-compatible `/chat/completions`
//! endpoint, and [`FallbackProvider`] retries a failed request on a second
//! provider.
//!
//! ```
//! use mb_llm::{OpenAiCompatible, OpenAiConfig, OutputMode};
//!
//! let provider = OpenAiCompatible::new(OpenAiConfig {
//!     base_url: "http://127.0.0.1:11434/v1".into(),
//!     api_key: None,
//!     model: "llama3.2".into(),
//!     output_mode: OutputMode::JsonSchema,
//!     headers: Vec::new(),
//!     extra_body: serde_json::Map::new(),
//!     http: mb_llm::HttpOptions::default(),
//! })?;
//! # let _ = provider;
//! # Ok::<(), mb_llm::LlmError>(())
//! ```
#![warn(
    missing_docs,
    missing_debug_implementations,
    clippy::missing_errors_doc
)]

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
pub use openai::{MAX_FREE_TEXT_CHARS, OpenAiCompatible, OpenAiConfig, OutputMode};
#[cfg(feature = "testing")]
pub use scripted::ScriptedProvider;

/// The author of a [`ChatMessage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// Instructions for the model.
    System,
    /// The request the model answers.
    User,
    /// An earlier answer of the model.
    Assistant,
}

impl Role {
    /// The role's name on the wire (`system`, `user`, `assistant`).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// The role's name on the wire, as [`Role::as_str`].
impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One message of a chat completion request.
#[derive(Clone, Debug)]
pub struct ChatMessage {
    /// Who wrote it.
    pub role: Role,
    /// Its text.
    pub content: String,
}

/// The shape a completion must take.
#[derive(Clone, Debug)]
pub enum ResponseFormat {
    /// Free text.
    Text,
    /// Any JSON object (JSON mode).
    JsonObject,
    /// JSON matching `schema` (structured outputs).
    JsonSchema {
        /// Name the schema is sent under.
        name: String,
        /// The JSON Schema the answer must match.
        schema: Value,
    },
}

/// A chat completion request.
#[derive(Clone, Debug)]
pub struct ChatRequest {
    /// The conversation, oldest message first.
    pub messages: Vec<ChatMessage>,
    /// The shape of the answer.
    pub response_format: ResponseFormat,
    /// Sampling temperature.
    pub temperature: f32,
    /// Most completion tokens the answer may use.
    pub max_tokens: u32,
    /// How long one request may take.
    pub timeout: Duration,
}

/// Tokens spent on a completion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Usage {
    /// Tokens of the request.
    pub prompt_tokens: u32,
    /// Tokens of the answer, reasoning included.
    pub completion_tokens: u32,
    /// The part of `completion_tokens` a reasoning model spent thinking
    /// (`usage.completion_tokens_details.reasoning_tokens`), when the
    /// provider reports it; 0 otherwise.
    pub reasoning_tokens: u32,
}

impl Usage {
    /// The completion tokens of the answer itself: completion less
    /// reasoning.
    #[must_use]
    pub fn answer_tokens(self) -> u32 {
        self.completion_tokens.saturating_sub(self.reasoning_tokens)
    }
}

impl Add for Usage {
    type Output = Usage;

    /// Saturating, so a misreporting provider cannot overflow a total.
    fn add(self, rhs: Usage) -> Usage {
        Usage {
            prompt_tokens: self.prompt_tokens.saturating_add(rhs.prompt_tokens),
            completion_tokens: self.completion_tokens.saturating_add(rhs.completion_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_add(rhs.reasoning_tokens),
        }
    }
}

/// A completed chat request.
#[derive(Clone, Debug)]
pub struct ChatResponse {
    /// The answer's text.
    pub content: String,
    /// Tokens the provider reports for it.
    pub usage: Usage,
}

/// The response formats a provider supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Capabilities {
    /// [`ResponseFormat::JsonSchema`] (structured outputs).
    pub json_schema: bool,
    /// [`ResponseFormat::JsonObject`] (JSON mode), which forces an object
    /// at the top level.
    pub json_mode: bool,
}

/// Why an LLM call failed.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// The request took longer than its timeout.
    #[error("LLM request timed out")]
    Timeout,
    /// The provider answered with an error status (or reported one inside a
    /// `200` reply).
    #[error("LLM returned HTTP {status}: {body}")]
    Http {
        /// The HTTP status.
        status: u16,
        /// The response body, as the provider sent it.
        body: String,
    },
    /// The request could not be sent or its answer not read.
    #[error("LLM transport error: {0}")]
    Transport(String),
    /// The answer is not the JSON the client expects.
    #[error("LLM response decode error: {0}")]
    Decode(String),
    /// The provider stopped at `max_tokens` (`finish_reason: "length"`), so
    /// the output is cut short. Retrying with the same limit would truncate
    /// again; a caller may salvage `partial` instead.
    #[error("LLM output truncated: {message}")]
    Truncated {
        /// What was cut, and where.
        message: String,
        /// The text received before the cut (empty for a stream, whose
        /// text was already yielded).
        partial: String,
        /// The tokens the provider reports for the cut reply.
        usage: Usage,
    },
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
            LlmError::Timeout | LlmError::Decode(_) | LlmError::Truncated { .. } => false,
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
    /// The text deltas.
    pub text: TextStream,
    /// The usage the provider reports, readable once `text` has ended.
    pub usage: StreamUsage,
}

impl std::fmt::Debug for MeteredStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeteredStream")
            .field("usage", &self.usage)
            .finish_non_exhaustive()
    }
}

/// An LLM the engine can send chat requests to.
#[async_trait]
pub trait Provider: Send + Sync {
    /// The response formats it supports.
    fn capabilities(&self) -> Capabilities;

    /// Sends `req` and waits for the whole answer.
    ///
    /// # Errors
    ///
    /// An [`LlmError`] when the request times out, cannot be sent, or the
    /// provider answers with an error, a cut-short or an empty reply.
    /// [`LlmError::is_retryable`] says whether another attempt may succeed.
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError>;

    /// The longest a [`Provider::complete`] call with `request_timeout` may
    /// take: the request timeout for a single provider; the sum over every
    /// hop for a [`FallbackProvider`]. Callers bound the call by this.
    fn total_timeout(&self, request_timeout: Duration) -> Duration {
        request_timeout
    }

    /// Streams the completion of `req` as text deltas. The default calls
    /// [`Provider::complete`] once and yields its content as one delta.
    ///
    /// # Errors
    ///
    /// As [`Provider::complete`], for a request that fails before the first
    /// delta; a failure mid-stream arrives as an `Err` item instead.
    async fn stream(&self, req: ChatRequest) -> Result<TextStream, LlmError> {
        let response = self.complete(req).await?;
        Ok(Box::pin(futures::stream::once(std::future::ready(Ok(
            response.content,
        )))))
    }

    /// [`Provider::stream`], with the usage the provider reports for it.
    /// The default reports none, so callers estimate it.
    ///
    /// # Errors
    ///
    /// As [`Provider::stream`].
    async fn stream_metered(&self, req: ChatRequest) -> Result<MeteredStream, LlmError> {
        Ok(MeteredStream {
            text: self.stream(req).await?,
            usage: StreamUsage::new(),
        })
    }
}
