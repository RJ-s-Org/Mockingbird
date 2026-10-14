//! LLM provider abstraction and an OpenAI-compatible client.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

pub mod openai;
#[cfg(feature = "testing")]
pub mod scripted;

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

#[async_trait]
pub trait Provider: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError>;
}
