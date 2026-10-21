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
    /// Extra request headers (name, value), already resolved: Cloudflare AI
    /// Gateway's `cf-aig-authorization`, Azure's `api-key`, attribution.
    pub headers: Vec<(String, String)>,
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
        for (name, value) in &self.config.headers {
            builder = builder.header(name, value);
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
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::Decode(format!("{e}: {}", excerpt(&text))))?;
        parse_reply(&v, req.max_tokens)
    }

    /// [`Self::send`], once more when the first failure is retryable and
    /// there is time left before `deadline`.
    async fn send_with_retry(
        &self,
        req: &ChatRequest,
        deadline: tokio::time::Instant,
    ) -> Result<ChatResponse, LlmError> {
        match self.send(req).await {
            Err(e) if e.is_retryable() && tokio::time::Instant::now() < deadline => {
                tracing::debug!(error = %e, "retrying the LLM call once");
                self.send(req).await
            }
            other => other,
        }
    }
}

/// The content of a chat completion, or why there is none. Handles what
/// OpenAI-compatible gateways actually send back: errors reported inside a
/// 200 (`choices[0].error`, `finish_reason: "error"`, a top-level `error`),
/// output cut at `max_tokens`, and reasoning models that put the whole
/// answer in `reasoning` with `content` null.
fn parse_reply(v: &Value, max_tokens: u32) -> Result<ChatResponse, LlmError> {
    let provider = v
        .get("provider")
        .and_then(Value::as_str)
        .map(|p| format!("provider {p}, "))
        .unwrap_or_default();
    let choice = v.pointer("/choices/0");
    let finish = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("none");
    let error = choice
        .and_then(|c| c.get("error"))
        .or_else(|| v.get("error"))
        .filter(|e| !e.is_null());
    if error.is_some() || finish == "error" {
        let error = error.cloned().unwrap_or(Value::Null);
        let status = error
            .get("code")
            .and_then(Value::as_u64)
            .and_then(|c| u16::try_from(c).ok())
            .filter(|c| (400..600).contains(c))
            .unwrap_or(502);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error reported inside a successful reply");
        return Err(LlmError::Http {
            status,
            body: format!("{provider}{message}"),
        });
    }
    let message = choice.and_then(|c| c.get("message"));
    let content = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty());
    if finish == "length" {
        return Err(LlmError::Truncated(format!(
            "{provider}stopped at max_tokens ({max_tokens}); raise the profile's max_tokens (got {})",
            excerpt(content.unwrap_or_default())
        )));
    }
    let content = match content {
        Some(c) => c.to_string(),
        None => {
            let reasoning = message
                .and_then(|m| m.get("reasoning"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            match json_in(reasoning) {
                Some(json) => json.to_string(),
                None if choice.is_none() => {
                    return Err(LlmError::Decode(format!(
                        "{provider}no choices in the reply: {}",
                        excerpt(&v.to_string())
                    )));
                }
                None => {
                    return Err(LlmError::Empty(format!(
                        "{provider}finish_reason {finish}, empty content{}",
                        if reasoning.is_empty() {
                            String::new()
                        } else {
                            format!(" and reasoning without JSON: {}", excerpt(reasoning))
                        }
                    )));
                }
            }
        }
    };
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

/// The last complete JSON object or array in `text`, which reasoning output
/// ends with when a model answers there instead of in `content`.
fn json_in(text: &str) -> Option<&str> {
    let mut found = None;
    for (start, c) in text.char_indices() {
        if c != '{' && c != '[' {
            continue;
        }
        let rest = &text[start..];
        let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        if let Some(Ok(value)) = stream.next()
            && (value.is_object() || value.is_array())
        {
            found = Some(&rest[..stream.byte_offset()]);
        }
    }
    found
}

/// The first 300 bytes of `text`, cut at a character boundary.
fn excerpt(text: &str) -> String {
    const LIMIT: usize = 300;
    if text.len() <= LIMIT {
        return text.to_string();
    }
    let mut end = LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
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
        let deadline = tokio::time::Instant::now() + req.timeout;
        match tokio::time::timeout_at(deadline, self.send_with_retry(&req, deadline)).await {
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
