use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{Value, json};

use crate::{
    Capabilities, ChatRequest, ChatResponse, HttpOptions, LlmError, Provider, ResponseFormat,
    TextStream, Usage,
};

#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub json_schema: bool,
    /// Extra CA roots and the proxy (`outbound:`).
    pub http: HttpOptions,
}

pub struct OpenAiCompatible {
    client: reqwest::Client,
    config: OpenAiConfig,
}

impl OpenAiCompatible {
    pub fn new(config: OpenAiConfig) -> Result<Self, LlmError> {
        let client = crate::http_client(&config.http, None)?;
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

    /// Posts `body` and waits for the response head.
    async fn post(&self, body: &Value) -> Result<reqwest::Response, LlmError> {
        let mut builder = self.client.post(self.url()).json(body);
        if let Some(key) = &self.config.api_key {
            builder = builder.bearer_auth(key);
        }
        builder
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))
    }

    /// Opens a streaming completion: the response head has arrived with a
    /// success status. A non-2xx status is an [`LlmError::Http`] carrying the
    /// response body.
    async fn open_stream(&self, req: &ChatRequest) -> Result<reqwest::Response, LlmError> {
        let mut body = self.body(req);
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".to_string(), Value::Bool(true));
        }
        let resp = self.post(&body).await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp
                .text()
                .await
                .map_err(|e| LlmError::Transport(e.to_string()))?;
            return Err(LlmError::Http {
                status: status.as_u16(),
                body: text,
            });
        }
        Ok(resp)
    }

    async fn send(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let resp = self.post(&self.body(req)).await?;
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

    /// Records the model as `llm.model` on the current span (the engine's
    /// `mb.llm` span).
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        tracing::Span::current().record("llm.model", self.config.model.as_str());
        match tokio::time::timeout(req.timeout, self.send(&req)).await {
            Ok(r) => r,
            Err(_) => Err(LlmError::Timeout),
        }
    }

    /// `req.timeout` bounds only the arrival of the response head (and of an
    /// error body); the deltas then arrive as fast as the server sends them.
    async fn stream(&self, req: ChatRequest) -> Result<TextStream, LlmError> {
        tracing::Span::current().record("llm.model", self.config.model.as_str());
        let resp = match tokio::time::timeout(req.timeout, self.open_stream(&req)).await {
            Ok(r) => r?,
            Err(_) => return Err(LlmError::Timeout),
        };
        let events = resp.bytes_stream().eventsource();
        let deltas = futures::stream::unfold(Some(events), |state| async move {
            let mut events = state?;
            loop {
                let event = match events.next().await {
                    None => return None,
                    Some(Err(e)) => return Some((Err(LlmError::Transport(e.to_string())), None)),
                    Some(Ok(event)) => event,
                };
                let data = event.data.trim();
                if data == "[DONE]" {
                    return None;
                }
                let v: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(e) => return Some((Err(LlmError::Decode(e.to_string())), None)),
                };
                match v
                    .pointer("/choices/0/delta/content")
                    .and_then(Value::as_str)
                {
                    Some(text) if !text.is_empty() => {
                        return Some((Ok(text.to_string()), Some(events)));
                    }
                    _ => continue,
                }
            }
        });
        Ok(Box::pin(deltas))
    }
}
