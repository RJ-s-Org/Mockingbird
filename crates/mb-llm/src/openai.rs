use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{
    Capabilities, ChatRequest, ChatResponse, HttpOptions, LlmError, MeteredStream, Provider,
    ResponseFormat, StreamUsage, TextStream, Usage,
};

/// An OpenAI-compatible endpoint. Its `Debug` output redacts `api_key` and
/// the header values (header names are kept).
#[derive(Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub json_schema: bool,
    /// Extra request headers (name, value), already resolved: Cloudflare AI
    /// Gateway's `cf-aig-authorization`, Azure's `api-key`, attribution.
    pub headers: Vec<(String, String)>,
    /// JSON merged into every request body, under the client's own fields
    /// (OpenRouter `provider` routing, `reasoning` settings).
    pub extra_body: serde_json::Map<String, Value>,
    /// Extra CA roots and the proxy (`outbound:`).
    pub http: HttpOptions,
}

impl std::fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const REDACTED: &str = "<redacted>";
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(name, _)| (name.as_str(), REDACTED))
            .collect();
        f.debug_struct("OpenAiConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| REDACTED))
            .field("model", &self.model)
            .field("json_schema", &self.json_schema)
            .field("headers", &headers)
            .field("extra_body", &self.extra_body)
            .field("http", &self.http)
            .finish()
    }
}

pub struct OpenAiCompatible {
    client: reqwest::Client,
    config: OpenAiConfig,
    /// Whether requests ask for no reasoning (see [`Self::reasoning_off`]);
    /// cleared when the endpoint rejects that.
    reasoning_off: AtomicBool,
    /// Whether streams ask for usage (`stream_options.include_usage`);
    /// cleared when the endpoint rejects that.
    stream_usage: AtomicBool,
}

impl OpenAiCompatible {
    /// OpenRouter endpoints ask for no reasoning by default.
    pub fn new(config: OpenAiConfig) -> Result<Self, LlmError> {
        let client = crate::http_client(&config.http, None)?;
        let reasoning_off = is_openrouter(&config.base_url);
        Ok(Self {
            client,
            config,
            reasoning_off: AtomicBool::new(reasoning_off),
            stream_usage: AtomicBool::new(true),
        })
    }

    /// Whether to send `reasoning: {"enabled": false}` (OpenRouter's switch)
    /// unless `extra_body` sets `reasoning`. Mock answers are data, not
    /// problems to think through, and hidden reasoning tokens were most of
    /// the completion of a large answer (DeepSeek: 2–5k tokens, 10–30 s).
    /// An endpoint that rejects the switch (a model that always reasons) is
    /// asked once more without it, and then never again.
    pub fn reasoning_off(self, on: bool) -> Self {
        self.reasoning_off.store(on, Ordering::Relaxed);
        self
    }

    fn sends_reasoning_off(&self) -> bool {
        self.reasoning_off.load(Ordering::Relaxed)
            && !self.config.extra_body.contains_key("reasoning")
    }

    /// Whether `e` rejects the reasoning switch this client sent; if so the
    /// switch is turned off for good.
    fn rejected_reasoning_off(&self, e: &LlmError) -> bool {
        let rejected = self.sends_reasoning_off()
            && matches!(e, LlmError::Http { status: 400 | 404 | 422, body } if body.to_ascii_lowercase().contains("reasoning"));
        if rejected {
            tracing::info!(
                "the endpoint rejects reasoning: {{enabled: false}}; not sending it again"
            );
            self.reasoning_off.store(false, Ordering::Relaxed);
        }
        rejected
    }

    /// Whether streams send `stream_options: {"include_usage": true}`, so
    /// the endpoint ends them with a usage chunk; not when `extra_body` sets
    /// `stream_options`, nor after the endpoint rejected it.
    fn sends_stream_usage(&self) -> bool {
        self.stream_usage.load(Ordering::Relaxed)
            && !self.config.extra_body.contains_key("stream_options")
    }

    /// Whether `e` rejects the `stream_options` this client sent; if so it
    /// is not sent again (and those streams' usage is estimated).
    fn rejected_stream_usage(&self, e: &LlmError) -> bool {
        let rejected = self.sends_stream_usage()
            && matches!(e, LlmError::Http { status: 400 | 404 | 422, body } if body.to_ascii_lowercase().contains("stream_options"));
        if rejected {
            tracing::info!(
                "the endpoint rejects stream_options: {{include_usage: true}}; not sending it again"
            );
            self.stream_usage.store(false, Ordering::Relaxed);
        }
        rejected
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
        if let Some(obj) = body.as_object_mut() {
            if let Some(f) = format {
                obj.insert("response_format".to_string(), f);
            }
            for (key, value) in &self.config.extra_body {
                if !obj.contains_key(key) {
                    obj.insert(key.clone(), value.clone());
                }
            }
            if self.sends_reasoning_off() {
                obj.insert("reasoning".to_string(), json!({"enabled": false}));
            }
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
        // Each switch is dropped at most once, so this is three tries at most.
        let mut result = self.open_stream_once(req).await;
        for _ in 0..2 {
            match result {
                Err(e) if self.rejected_reasoning_off(&e) || self.rejected_stream_usage(&e) => {
                    result = self.open_stream_once(req).await;
                }
                other => return other,
            }
        }
        result
    }

    async fn open_stream_once(&self, req: &ChatRequest) -> Result<reqwest::Response, LlmError> {
        let mut body = self.body(req);
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".to_string(), Value::Bool(true));
            if self.sends_stream_usage() {
                obj.insert("stream_options".to_string(), json!({"include_usage": true}));
            }
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
        match self.send_once(req).await {
            Err(e) if self.rejected_reasoning_off(&e) => self.send_once(req).await,
            other => other,
        }
    }

    async fn send_once(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
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

/// Whether `base_url` is OpenRouter's API.
fn is_openrouter(base_url: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|h| h == "openrouter.ai" || h.ends_with(".openrouter.ai"))
}

/// The content of a chat completion, or why there is none. Handles what
/// OpenAI-compatible gateways actually send back: errors reported inside a
/// 200 (`choices[0].error`, `finish_reason: "error"`, a top-level `error`),
/// output cut at `max_tokens`, and reasoning models that put the whole
/// answer in `reasoning` with `content` null.
/// `"provider <name>, "` when the reply (or chunk) names its provider.
fn provider_prefix(v: &Value) -> String {
    v.get("provider")
        .and_then(Value::as_str)
        .map(|p| format!("provider {p}, "))
        .unwrap_or_default()
}

/// The `usage` a stream chunk carries (the final one, when the request set
/// `stream_options.include_usage`).
fn chunk_usage(v: &Value) -> Option<Usage> {
    let usage = v.get("usage").filter(|u| u.is_object())?;
    let tok = |name: &str| {
        usage
            .get(name)
            .and_then(Value::as_u64)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
    };
    Some(Usage {
        prompt_tokens: tok("prompt_tokens")?,
        completion_tokens: tok("completion_tokens").unwrap_or(0),
    })
}

/// The `finish_reason` of the first choice, `"none"` without one.
fn finish_reason(v: &Value) -> &str {
    v.pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("none")
}

/// The error a reply or stream chunk reports in band: an `error` object (on
/// the first choice or the top level) or `finish_reason: "error"`. Its
/// `code` becomes the status when it is an HTTP error code, else 502.
fn in_band_error(v: &Value) -> Option<LlmError> {
    let error = v
        .pointer("/choices/0/error")
        .or_else(|| v.get("error"))
        .filter(|e| !e.is_null());
    if error.is_none() && finish_reason(v) != "error" {
        return None;
    }
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
    Some(LlmError::Http {
        status,
        body: format!("{}{message}", provider_prefix(v)),
    })
}

fn parse_reply(v: &Value, max_tokens: u32) -> Result<ChatResponse, LlmError> {
    let provider = provider_prefix(v);
    let choice = v.pointer("/choices/0");
    let finish = finish_reason(v);
    if let Some(error) = in_band_error(v) {
        return Err(error);
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

/// The JSON object or array that `text` ends with (closing whitespace or a
/// code fence aside): a model that answers in its reasoning instead of in
/// `content` ends with the answer. Reasoning that goes on after its last JSON
/// value only quotes fragments while planning, and yields nothing.
fn json_in(text: &str) -> Option<&str> {
    let (found, end) = last_json(text)?;
    let rest = text[end..].trim().trim_start_matches("```").trim();
    rest.is_empty().then_some(found)
}

/// The last complete top-level JSON object or array in `text`, and the byte
/// offset where it ends. Values nested in one already found are skipped.
///
/// Each candidate is only validated (as a [`Probe`]), never built: a value
/// that starts with `{` or `[` and parses is an object or an array.
fn last_json(text: &str) -> Option<(&str, usize)> {
    let mut found = None;
    let mut next = 0;
    for (start, c) in text.char_indices() {
        if start < next || (c != '{' && c != '[') {
            continue;
        }
        let rest = &text[start..];
        let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<Probe>();
        if let Some(Ok(Probe)) = stream.next() {
            let end = stream.byte_offset();
            next = start + end;
            found = Some((&rest[..end], next));
        }
    }
    found
}

/// Any JSON value, read and dropped: what parsing into a [`Value`] accepts
/// (nesting limit included), without building one.
/// ([`serde::de::IgnoredAny`] skips values without serde_json's nesting
/// limit, so it would accept more.)
struct Probe;

impl<'de> serde::Deserialize<'de> for Probe {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(ProbeVisitor)
    }
}

struct ProbeVisitor;

impl<'de> serde::de::Visitor<'de> for ProbeVisitor {
    type Value = Probe;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<Probe, E> {
        Ok(Probe)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Probe, E> {
        Ok(Probe)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Probe, E> {
        Ok(Probe)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Probe, E> {
        Ok(Probe)
    }

    fn visit_str<E>(self, _: &str) -> Result<Probe, E> {
        Ok(Probe)
    }

    fn visit_unit<E>(self) -> Result<Probe, E> {
        Ok(Probe)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Probe, A::Error> {
        while seq.next_element::<Probe>()?.is_some() {}
        Ok(Probe)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Probe, A::Error> {
        while map.next_entry::<Probe, Probe>()?.is_some() {}
        Ok(Probe)
    }
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
        self.stream_metered(req).await.map(|m| m.text)
    }

    /// [`Provider::stream`], asking for usage (`stream_options`, dropped for
    /// good when the endpoint rejects it): the usage of the final chunk is
    /// reported once the stream has ended.
    async fn stream_metered(&self, req: ChatRequest) -> Result<MeteredStream, LlmError> {
        tracing::Span::current().record("llm.model", self.config.model.as_str());
        let resp = match tokio::time::timeout(req.timeout, self.open_stream(&req)).await {
            Ok(r) => r?,
            Err(_) => return Err(LlmError::Timeout),
        };
        let events = resp.bytes_stream().eventsource();
        let max_tokens = req.max_tokens;
        let usage = StreamUsage::new();
        let reported = usage.clone();
        // The state is the event source plus an error to yield after the
        // delta just returned (a truncation marked on a chunk with content);
        // `None` once the stream has ended.
        let deltas = futures::stream::unfold(Some((events, None)), move |state| {
            let reported = reported.clone();
            async move {
                let (mut events, pending) = state?;
                if let Some(error) = pending {
                    return Some((Err(error), None));
                }
                loop {
                    let event = match events.next().await {
                        None => return None,
                        Some(Err(e)) => {
                            return Some((Err(LlmError::Transport(e.to_string())), None));
                        }
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
                    if let Some(error) = in_band_error(&v) {
                        return Some((Err(error), None));
                    }
                    if let Some(u) = chunk_usage(&v) {
                        reported.set(u);
                    }
                    let truncated = (finish_reason(&v) == "length").then(|| {
                    LlmError::Truncated(format!(
                        "{}stopped at max_tokens ({max_tokens}); raise the profile's max_tokens",
                        provider_prefix(&v)
                    ))
                });
                    let text = v
                        .pointer("/choices/0/delta/content")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty());
                    match (text, truncated) {
                        (Some(text), truncated) => {
                            return Some((Ok(text.to_string()), Some((events, truncated))));
                        }
                        (None, Some(error)) => return Some((Err(error), None)),
                        // Keep-alive, role-only, stop and usage chunks carry nothing.
                        (None, None) => continue,
                    }
                }
            }
        });
        Ok(MeteredStream {
            text: Box::pin(deltas),
            usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{json_in, last_json};

    /// Timing harness for [`last_json`]: `cargo test -p mockingbird-llm --lib
    /// -- --ignored --nocapture last_json_timing`.
    #[test]
    #[ignore = "timing harness"]
    fn last_json_timing() {
        let answer = r#"{"id": 7, "tags": ["a", "b"], "owner": {"name": "x"}}"#;
        for n in [1_000usize, 10_000] {
            // Planning prose quoting small fragments, then the answer.
            let mut prose = String::new();
            while prose.len() < n {
                prose.push_str("Maybe {\"id\": 1} or [1, 2] fits; ");
            }
            prose.push_str(answer);
            // Unclosed drafts: every '{' and '[' is a failed parse to the end.
            let drafts = format!("{}{answer}", "{\"a\": [".repeat(n / 7));
            let runs = 20u32;
            for (name, text) in [("prose", &prose), ("drafts", &drafts)] {
                let started = std::time::Instant::now();
                for _ in 0..runs {
                    std::hint::black_box(last_json(std::hint::black_box(text)));
                }
                println!(
                    "{name} {} bytes: {:?} per call",
                    text.len(),
                    started.elapsed() / runs
                );
            }
        }
    }

    #[test]
    fn last_json_finds_the_trailing_value_and_skips_nested_ones() {
        let text = r#"try {"a": 1} then [2, {"b": 3}] and finally {"c": [4]}"#;
        assert_eq!(last_json(text).map(|(j, _)| j), Some(r#"{"c": [4]}"#));
        assert_eq!(json_in(text), Some(r#"{"c": [4]}"#));
        assert_eq!(json_in(r#"{"a": 1} then prose"#), None);
        assert_eq!(
            json_in("{\"a\": [1, {\"b\": 2}]}\n```"),
            Some("{\"a\": [1, {\"b\": 2}]}")
        );
        // An unclosed draft before the answer; a scalar never counts.
        assert_eq!(
            json_in(r#"{"a": [ 42 {"ok": true}"#),
            Some(r#"{"ok": true}"#)
        );
        assert_eq!(last_json("only 42 and \"x\""), None);
        // Strings, escapes, numbers and literals inside a candidate.
        assert_eq!(
            json_in(r#"so: {"s": "a\"}b", "n": -1.5e3, "t": true, "z": null, "e": {}}"#),
            Some(r#"{"s": "a\"}b", "n": -1.5e3, "t": true, "z": null, "e": {}}"#)
        );
    }

    /// Nesting past serde_json's limit is no JSON value, as parsing into a
    /// `Value` says: only an inner part parses, and text follows it.
    #[test]
    fn nesting_past_the_limit_is_not_a_value() {
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert!(serde_json::from_str::<serde_json::Value>(&deep).is_err());
        assert_eq!(json_in(&deep), None);
        let ok = format!("{}{}", "[".repeat(100), "]".repeat(100));
        assert_eq!(json_in(&ok), Some(ok.as_str()));
    }
}
