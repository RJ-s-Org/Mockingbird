use serde_json::{Value, json};

use crate::{Capabilities, ChatRequest, ChatResponse, LlmError, Provider, ResponseFormat, Usage};

#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub json_schema: bool,
}

pub struct OpenAiCompatible {
    client: reqwest::Client,
    config: OpenAiConfig,
}

impl OpenAiCompatible {
    pub fn new(config: OpenAiConfig) -> Result<Self, LlmError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        Ok(Self { client, config })
    }

    fn url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        )
    }

    fn body(&self, req: &ChatRequest) -> Value {
        let messages: Vec<Value> = req
            .messages
            .iter()
            .map(|m| json!({"role": m.role.as_str(), "content": m.content}))
            .collect();
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "temperature": req.temperature,
            "max_tokens": req.max_tokens,
        });
        let format = match &req.response_format {
            ResponseFormat::Text => None,
            ResponseFormat::JsonObject => Some(json!({"type": "json_object"})),
            ResponseFormat::JsonSchema { name, schema } => {
                if self.config.json_schema {
                    Some(json!({
                        "type": "json_schema",
                        "json_schema": {"name": name, "schema": schema, "strict": false}
                    }))
                } else {
                    Some(json!({"type": "json_object"}))
                }
            }
        };
        if let (Some(f), Some(obj)) = (format, body.as_object_mut()) {
            obj.insert("response_format".to_string(), f);
        }
        body
    }

    async fn send(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let mut builder = self.client.post(self.url()).json(&self.body(req));
        if let Some(key) = &self.config.api_key {
            builder = builder.bearer_auth(key);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::Http {
                status: status.as_u16(),
                body: text,
            });
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| LlmError::Decode(e.to_string()))?;
        let content = v
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| LlmError::Decode("missing choices[0].message.content".into()))?
            .to_string();
        let tok = |p: &str| {
            v.pointer(p)
                .and_then(Value::as_u64)
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
                .unwrap_or(0)
        };
        Ok(ChatResponse {
            content,
            usage: Usage {
                prompt_tokens: tok("/usage/prompt_tokens"),
                completion_tokens: tok("/usage/completion_tokens"),
            },
        })
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiCompatible {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: self.config.json_schema,
            json_mode: true,
        }
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        match tokio::time::timeout(req.timeout, self.send(&req)).await {
            Ok(r) => r,
            Err(_) => Err(LlmError::Timeout),
        }
    }
}
