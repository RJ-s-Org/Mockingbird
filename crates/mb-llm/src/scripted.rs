use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::{Capabilities, ChatRequest, ChatResponse, LlmError, Provider, Usage};

/// Test provider that replays a fixed script and records requests.
pub struct ScriptedProvider {
    script: Mutex<VecDeque<Result<ChatResponse, LlmError>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl ScriptedProvider {
    /// Replays `script` reporting zero usage for every call.
    pub fn new(script: Vec<Result<String, LlmError>>) -> Self {
        Self::with_usage(
            script
                .into_iter()
                .map(|r| r.map(|content| (content, Usage::default())))
                .collect(),
        )
    }

    /// Replays `script`, reporting the given usage for each successful call.
    pub fn with_usage(script: Vec<Result<(String, Usage), LlmError>>) -> Self {
        let script = script
            .into_iter()
            .map(|r| r.map(|(content, usage)| ChatResponse { content, usage }))
            .collect();
        Self {
            script: Mutex::new(script),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests
            .lock()
            .map(|r| r.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: true,
            json_mode: true,
        }
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(req);
        self.script
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .unwrap_or_else(|| Err(LlmError::Transport("script exhausted".into())))
    }
}
