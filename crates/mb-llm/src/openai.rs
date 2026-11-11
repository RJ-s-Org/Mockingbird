//! The OpenAI-compatible client ([`OpenAiCompatible`]).

use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::repetition::RepetitionGuard;
use crate::{
    CUT_REPLIES_METRIC, Capabilities, ChatRequest, ChatResponse, HttpOptions, LlmError,
    MeteredStream, Provider, ResponseFormat, StreamUsage, TextStream, Usage,
};

/// An OpenAI-compatible endpoint. Its `Debug` output redacts `api_key`, the
/// header values (header names are kept) and any user and password in
/// `base_url` or the proxy URL.
#[derive(Clone)]
pub struct OpenAiConfig {
    /// The endpoint, up to (not including) `/chat/completions`.
    pub base_url: String,
    /// Sent as `Authorization: Bearer <key>` when set.
    pub api_key: Option<String>,
    /// The model to ask for.
    pub model: String,
    /// How the endpoint is asked for JSON (`response_format`).
    pub output_mode: OutputMode,
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
            .field("base_url", &crate::http::redact_userinfo(&self.base_url))
            .field("api_key", &self.api_key.as_ref().map(|_| REDACTED))
            .field("model", &self.model)
            .field("output_mode", &self.output_mode)
            .field("headers", &headers)
            .field("extra_body", &self.extra_body)
            .field("http", &self.http)
            .finish()
    }
}

/// How an [`OpenAiCompatible`] endpoint is asked for JSON: the
/// `response_format` it sends for a JSON answer.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    /// No `response_format`: the prompt alone asks for JSON. For models
    /// that answer worse (or not at all) in JSON mode.
    None,
    /// `{"type": "json_object"}` (JSON mode), which every
    /// OpenAI-compatible endpoint supports. It forces an object at the top
    /// level.
    JsonObject,
    /// `{"type": "json_schema", ...}` (structured outputs) with the
    /// answer's schema, sent with `strict: false`: strict mode rejects most
    /// API contracts' schemas (optional members, open objects). A schema the
    /// endpoint rejects is asked for in JSON mode instead.
    #[default]
    JsonSchema,
}

impl OutputMode {
    /// The deprecated `json_schema` flag's mode: `true` is
    /// [`OutputMode::JsonSchema`], `false` [`OutputMode::JsonObject`].
    #[must_use]
    pub fn from_json_schema(json_schema: bool) -> Self {
        if json_schema {
            OutputMode::JsonSchema
        } else {
            OutputMode::JsonObject
        }
    }

    /// The config name: `none`, `json_object` or `json_schema`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            OutputMode::None => "none",
            OutputMode::JsonObject => "json_object",
            OutputMode::JsonSchema => "json_schema",
        }
    }
}

impl std::fmt::Display for OutputMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for OutputMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "none" => Ok(OutputMode::None),
            "json_object" => Ok(OutputMode::JsonObject),
            "json_schema" => Ok(OutputMode::JsonSchema),
            other => Err(format!(
                "unknown output mode {other:?}; expected none, json_object or json_schema"
            )),
        }
    }
}

/// The limit a schema sent as `response_format` puts on a string with no
/// shape of its own. Some endpoints enforce the schema while generating, and a
/// model can then loop in an unbounded string until `max_tokens`. A string
/// this long is a runaway to the engine anyway, which cuts it, so the limit
/// takes nothing a client would otherwise get. Answers are still validated
/// against the contract's own schema.
pub const MAX_FREE_TEXT_CHARS: u64 = 2_000;

/// Rejected schemas remembered before the list starts over.
const MAX_REJECTED_SCHEMAS: usize = 4096;

/// A [`Provider`] for an OpenAI-compatible `/chat/completions` endpoint.
///
/// `Debug` shows its [`OpenAiConfig`], redacted the same way.
pub struct OpenAiCompatible {
    client: reqwest::Client,
    config: OpenAiConfig,
    /// The `reasoning` setting requests send (a [`Reasoning`] as `u8`; see
    /// [`Self::with_reasoning_disabled`]), stepped down each time the
    /// endpoint rejects it.
    reasoning: AtomicU8,
    /// Whether streams ask for usage (`stream_options.include_usage`);
    /// cleared when the endpoint rejects that.
    stream_usage: AtomicBool,
    /// The schemas the endpoint rejected, by name and content, asked for in
    /// JSON mode from then on (see [`MAX_REJECTED_SCHEMAS`]).
    rejected_schemas: Mutex<HashSet<(String, u64)>>,
    /// Whether streamed text is watched for loops (see
    /// [`Self::with_repetition_guard`]).
    repetition_guard: bool,
    /// How long a loop must run before it is stopped (see
    /// [`Self::with_repetition_span`]).
    repetition_span: usize,
    /// Whether `complete` may stream; cleared for good when the endpoint
    /// refuses `stream` itself.
    streaming: AtomicBool,
}

impl std::fmt::Debug for OpenAiCompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatible")
            .field("config", &self.config)
            .field("reasoning", &self.reasoning_setting())
            .field("stream_usage", &self.stream_usage.load(Ordering::Relaxed))
            .field("repetition_guard", &self.repetition_guard)
            .field("repetition_span", &self.repetition_span)
            .field("streaming", &self.streaming.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatible {
    /// OpenRouter endpoints ask for no reasoning by default.
    ///
    /// # Errors
    ///
    /// [`LlmError::Transport`] when the HTTP client cannot be built from
    /// [`OpenAiConfig::http`] (see [`HttpOptions::apply`]).
    pub fn new(config: OpenAiConfig) -> Result<Self, LlmError> {
        let client = crate::http_client(&config.http, None)?;
        let reasoning = if is_openrouter(&config.base_url) {
            Reasoning::Off
        } else {
            Reasoning::Unset
        };
        Ok(Self {
            client,
            config,
            reasoning: AtomicU8::new(reasoning as u8),
            stream_usage: AtomicBool::new(true),
            rejected_schemas: Mutex::new(HashSet::new()),
            repetition_guard: true,
            repetition_span: crate::repetition::DEFAULT_SPAN,
            streaming: AtomicBool::new(true),
        })
    }

    /// Whether to stop a reply that has fallen into a loop (on by default):
    /// the tail of its text has become a few short pieces repeated (see
    /// [`crate::repetition`]), which goes on until `max_tokens` and is lost
    /// anyway. The stopped call is an [`LlmError::Repeated`], which
    /// [`Provider::complete`] retries once, like other retryable errors.
    ///
    /// To watch the text as it is written, [`Provider::complete`] then asks
    /// for a stream (`stream: true`, with `stream_options.include_usage`
    /// unless the endpoint rejects it) and builds the reply from its chunks;
    /// an endpoint that answers with a plain JSON reply instead is read as
    /// one. Off, `complete` sends a plain request and waits for the whole
    /// reply. [`Provider::stream`] is watched either way when on.
    ///
    /// `complete` does not stream, so the guard is off for its calls, when
    /// `extra_body` sets `stream` or asks for more than one choice (`n`),
    /// and once the endpoint has refused `stream` (a 4xx naming it): that
    /// request is sent again plainly, and so are later ones.
    #[must_use]
    pub fn with_repetition_guard(mut self, on: bool) -> Self {
        self.repetition_guard = on;
        self
    }

    /// How long the text must have been of low variety before the guard
    /// stops it, in bytes (default [`crate::repetition::DEFAULT_SPAN`], at
    /// least [`crate::repetition::WINDOW`]; see [`crate::repetition`]).
    #[must_use]
    pub fn with_repetition_span(mut self, bytes: usize) -> Self {
        self.repetition_span = bytes.max(crate::repetition::WINDOW);
        self
    }

    /// Whether `complete` streams (and so guards) its calls: the guard is
    /// on, `extra_body` sets neither `stream` nor `n` above 1, and the
    /// endpoint has not refused `stream`.
    fn streams_complete(&self) -> bool {
        let extra = &self.config.extra_body;
        self.repetition_guard
            && !extra.contains_key("stream")
            && extra
                .get("n")
                .and_then(Value::as_u64)
                .is_none_or(|n| n <= 1)
            && self.streaming.load(Ordering::Relaxed)
    }

    /// Whether `e` refuses `stream` itself (a 4xx naming `stream` but not
    /// `stream_options`); if so `complete` stops streaming for good.
    fn rejected_streaming(&self, e: &LlmError) -> bool {
        let rejected = matches!(e, LlmError::Http { status, body }
        if (400..500).contains(status) && *status != 408 && *status != 429 && {
            // `stream` as a word: not `upstream`, nor `stream_options`.
            let body = body.to_ascii_lowercase();
            !body.contains("stream_options")
                && body
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .any(|word| word == "stream")
        });
        if rejected && self.streaming.swap(false, Ordering::Relaxed) {
            tracing::warn!(
                model = %self.config.model,
                "the endpoint refuses streamed requests; sending plain ones, without the repetition guard"
            );
        }
        rejected
    }

    /// Whether to send `reasoning: {"enabled": false}` (OpenRouter's switch)
    /// unless `extra_body` sets `reasoning`. Mock answers are data, not
    /// problems to think through, and hidden reasoning tokens were most of
    /// the completion of a large answer (DeepSeek: 2–5k tokens, 10–30 s).
    ///
    /// A model that always reasons rejects the switch with a 400. The request
    /// is then sent once more with `reasoning: {"effort": "minimal"}`, which
    /// keeps the reasoning short, and when that is rejected too, once more
    /// without `reasoning`. The client remembers each step (it serves one
    /// model), so later calls do not pay for the 400 again, and a warning
    /// naming the model is logged once per step.
    #[must_use]
    pub fn with_reasoning_disabled(self, disabled: bool) -> Self {
        let setting = if disabled {
            Reasoning::Off
        } else {
            Reasoning::Unset
        };
        self.reasoning.store(setting as u8, Ordering::Relaxed);
        self
    }

    /// Whether to ask for no reasoning; `true` disables it.
    #[deprecated(since = "0.1.1", note = "use `with_reasoning_disabled`")]
    #[must_use]
    pub fn reasoning_off(self, on: bool) -> Self {
        self.with_reasoning_disabled(on)
    }

    fn reasoning_setting(&self) -> Reasoning {
        Reasoning::from_u8(self.reasoning.load(Ordering::Relaxed))
    }

    /// The `reasoning` member a request with `setting` sends; `None` when
    /// `extra_body` sets its own.
    fn sent_reasoning(&self, setting: Reasoning) -> Option<Value> {
        if self.config.extra_body.contains_key("reasoning") {
            return None;
        }
        setting.body()
    }

    /// Whether `e` rejects the `reasoning` setting `sent` with the request;
    /// if so the setting steps down from `sent` for good (off, then minimal
    /// effort, then none), with a warning naming the model. Concurrent
    /// calls that sent the same setting step it down once.
    fn rejected_reasoning(&self, e: &LlmError, sent: Reasoning) -> bool {
        let Some(sent_body) = self.sent_reasoning(sent) else {
            return false;
        };
        let rejected = matches!(e, LlmError::Http { status: 400 | 404 | 422, body }
            if rejects_reasoning(body, sent));
        if !rejected {
            return false;
        }
        let next = sent.step_down();
        if self
            .reasoning
            .compare_exchange(sent as u8, next as u8, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            tracing::warn!(
                model = %self.config.model,
                rejected = %sent_body,
                sending = %next.body().map_or_else(|| "no reasoning setting".to_string(), |b| b.to_string()),
                "the model rejects the reasoning setting; it reasons, and its reasoning tokens count against max_tokens"
            );
        }
        true
    }

    fn schema_rejected(&self, name: &str, schema: &Value) -> bool {
        self.rejected_schemas
            .lock()
            .is_ok_and(|rejected| rejected.contains(&schema_key(name, schema)))
    }

    /// Whether `req` goes out with its schema as `response_format`.
    fn sends_schema(&self, req: &ChatRequest) -> bool {
        self.config.output_mode == OutputMode::JsonSchema
            && matches!(&req.response_format, ResponseFormat::JsonSchema { name, schema }
                if !self.schema_rejected(name, schema))
    }

    /// Whether `e` says the endpoint refused the schema `req` was sent with;
    /// if so that schema is asked for in JSON mode from now on.
    fn rejected_schema(&self, e: &LlmError, req: &ChatRequest) -> bool {
        let ResponseFormat::JsonSchema { name, schema } = &req.response_format else {
            return false;
        };
        let rejected =
            matches!(e, LlmError::Http { status: 400 | 404 | 422, body } if rejects_schema(body));
        if rejected && let Ok(mut known) = self.rejected_schemas.lock() {
            if known.len() >= MAX_REJECTED_SCHEMAS {
                known.clear();
            }
            if known.insert(schema_key(name, schema)) {
                tracing::warn!(
                    model = %self.config.model,
                    schema = %name,
                    "the endpoint rejects this response schema; asking for it in JSON mode"
                );
            }
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

    fn body(&self, req: &ChatRequest, reasoning: Reasoning, json_only: bool) -> Value {
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
        let format = match (&req.response_format, self.config.output_mode) {
            (ResponseFormat::Text, _) | (_, OutputMode::None) => None,
            (ResponseFormat::JsonSchema { name, schema }, OutputMode::JsonSchema)
                if !json_only && !self.schema_rejected(name, schema) =>
            {
                let mut schema = schema.clone();
                limit_free_text(&mut schema);
                Some(json!({
                    "type": "json_schema",
                    "json_schema": {"name": name, "schema": schema, "strict": false}
                }))
            }
            (ResponseFormat::JsonObject | ResponseFormat::JsonSchema { .. }, _) => {
                Some(json!({"type": "json_object"}))
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
            if let Some(reasoning) = self.sent_reasoning(reasoning) {
                obj.insert("reasoning".to_string(), reasoning);
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
        // The reasoning setting steps down at most twice, and stream_options
        // and the schema are each dropped at most once: five tries at most.
        let mut sent = self.reasoning_setting();
        let mut with_schema = self.sends_schema(req);
        let mut json_only = false;
        let mut result = self.open_stream_once(req, sent, json_only).await;
        for _ in 0..4 {
            match result {
                Err(e) if with_schema && self.rejected_schema(&e, req) => {
                    with_schema = false;
                    json_only = true;
                    result = self.open_stream_once(req, sent, json_only).await;
                }
                Err(e) if self.rejected_reasoning(&e, sent) || self.rejected_stream_usage(&e) => {
                    sent = self.reasoning_setting();
                    result = self.open_stream_once(req, sent, json_only).await;
                }
                other => return other,
            }
        }
        result
    }

    async fn open_stream_once(
        &self,
        req: &ChatRequest,
        reasoning: Reasoning,
        json_only: bool,
    ) -> Result<reqwest::Response, LlmError> {
        let mut body = self.body(req, reasoning, json_only);
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
        // The reasoning setting steps down at most twice and the schema is
        // dropped at most once. An empty answer under a schema (some routes
        // lose it in that mode) is tried once in JSON mode, for this call only.
        // A streamed request (see `with_repetition_guard`) also drops
        // stream_options at most once, and streaming itself at most once.
        let mut sent = self.reasoning_setting();
        let mut with_schema = self.sends_schema(req);
        let mut json_only = false;
        let mut result = self.send_once(req, sent, json_only).await;
        for _ in 0..5 {
            match result {
                Err(e)
                    if with_schema
                        && (matches!(e, LlmError::Empty(_)) || self.rejected_schema(&e, req)) =>
                {
                    with_schema = false;
                    json_only = true;
                    result = self.send_once(req, sent, json_only).await;
                }
                Err(e)
                    if self.rejected_reasoning(&e, sent)
                        || (self.streams_complete()
                            && (self.rejected_stream_usage(&e) || self.rejected_streaming(&e))) =>
                {
                    sent = self.reasoning_setting();
                    result = self.send_once(req, sent, json_only).await;
                }
                other => return other,
            }
        }
        result
    }

    async fn send_once(
        &self,
        req: &ChatRequest,
        reasoning: Reasoning,
        json_only: bool,
    ) -> Result<ChatResponse, LlmError> {
        if self.streams_complete() {
            return self.send_once_streamed(req, reasoning, json_only).await;
        }
        let resp = self.post(&self.body(req, reasoning, json_only)).await?;
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
        parse_reply_text(&text, req.max_tokens)
    }

    /// [`Self::send_once`] as a stream, watched for loops: the chunks are
    /// gathered into the reply a plain request would have got, which is then
    /// read the same way. A looping answer or reasoning stops the stream
    /// (dropping the response closes the connection) with
    /// [`LlmError::Repeated`]. An endpoint that ignores `stream` and answers
    /// with a plain JSON reply is read as one.
    async fn send_once_streamed(
        &self,
        req: &ChatRequest,
        reasoning: Reasoning,
        json_only: bool,
    ) -> Result<ChatResponse, LlmError> {
        let resp = self.open_stream_once(req, reasoning, json_only).await?;
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|t| t.trim_start().to_ascii_lowercase());
        let mut bytes = resp.bytes_stream();
        // Without a content type, the first bytes say whether it is SSE.
        let mut first = None;
        let is_sse = match content_type {
            Some(t) => t.starts_with("text/event-stream"),
            None => {
                first = bytes
                    .next()
                    .await
                    .transpose()
                    .map_err(|e| LlmError::Transport(e.to_string()))?;
                first.as_deref().is_some_and(looks_like_sse)
            }
        };
        let mut bytes = futures::stream::iter(first.map(Ok)).chain(bytes);
        if !is_sse {
            let mut body = Vec::new();
            while let Some(chunk) = bytes.next().await {
                body.extend_from_slice(&chunk.map_err(|e| LlmError::Transport(e.to_string()))?);
            }
            return parse_reply_text(&String::from_utf8_lossy(&body), req.max_tokens);
        }
        let mut events = bytes.eventsource();
        let mut reply = GatheredReply::new(
            prompt_bytes(req),
            self.repetition_span,
            req.max_answer_bytes,
        );
        let mut done = false;
        let mut any = false;
        while let Some(event) = events.next().await {
            let event = event.map_err(|e| LlmError::Transport(e.to_string()))?;
            any = true;
            let data = event.data.trim();
            if data == "[DONE]" {
                done = true;
                break;
            }
            let v: Value = serde_json::from_str(data)
                .map_err(|e| LlmError::Decode(format!("{e}: {}", excerpt(data))))?;
            if let Some(error) = in_band_error(&v) {
                return Err(error);
            }
            reply.absorb(&v)?;
        }
        if !any {
            // An empty body, as a plain request reads it.
            return parse_reply_text("", req.max_tokens);
        }
        if !done && !reply.finished() {
            return Err(LlmError::Transport(format!(
                "{}the stream ended with neither [DONE] nor a finish reason, after {} bytes",
                reply.provider_prefix(),
                reply.bytes()
            )));
        }
        parse_reply(&reply.into_reply(), req.max_tokens)
    }

    /// [`Self::send`], once more when the first failure is retryable and
    /// there is time left before `deadline`.
    async fn send_with_retry(
        &self,
        req: &ChatRequest,
        deadline: tokio::time::Instant,
        stopped: &Mutex<Option<LlmError>>,
    ) -> Result<ChatResponse, LlmError> {
        match self.send(req).await {
            Err(e) if e.is_retryable() && tokio::time::Instant::now() < deadline => {
                tracing::debug!(error = %e, "retrying the LLM call once");
                let spent = e.spent();
                if spent != Usage::default()
                    && let Ok(mut held) = stopped.lock()
                {
                    // Read by `complete` if the retry runs out of time.
                    *held = Some(e);
                }
                match self.send(req).await {
                    Ok(mut response) => {
                        response.usage = spent + response.usage;
                        Ok(response)
                    }
                    Err(e) => Err(e.with_earlier_spent(spent)),
                }
            }
            other => other,
        }
    }
}

/// The `reasoning` member an [`OpenAiCompatible`] sends, in the order it
/// steps down when the endpoint rejects one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Reasoning {
    /// `{"enabled": false}` (OpenRouter's switch).
    Off = 0,
    /// `{"effort": "minimal"}`: for a model that always reasons.
    Minimal = 1,
    /// Nothing.
    Unset = 2,
}

impl Reasoning {
    fn from_u8(n: u8) -> Self {
        match n {
            0 => Reasoning::Off,
            1 => Reasoning::Minimal,
            _ => Reasoning::Unset,
        }
    }

    fn body(self) -> Option<Value> {
        match self {
            Reasoning::Off => Some(json!({"enabled": false})),
            Reasoning::Minimal => Some(json!({"effort": "minimal"})),
            Reasoning::Unset => None,
        }
    }

    fn step_down(self) -> Self {
        match self {
            Reasoning::Off => Reasoning::Minimal,
            Reasoning::Minimal | Reasoning::Unset => Reasoning::Unset,
        }
    }
}

/// What [`OpenAiCompatible::rejected_schemas`] remembers a schema by: selections
/// of one operation share a name and may be rejected separately.
fn schema_key(name: &str, schema: &Value) -> (String, u64) {
    let mut hasher = DefaultHasher::new();
    schema.to_string().hash(&mut hasher);
    (name.to_string(), hasher.finish())
}

/// Whether an error body says the endpoint refused the response schema, or
/// structured output altogether.
fn rejects_schema(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    !body.contains("context length")
        && !body.contains("context_length")
        && [
            "response_format",
            "response format",
            "response_schema",
            "json_schema",
            "json schema",
            "structured output",
            "requested parameters",
        ]
        .iter()
        .any(|marker| body.contains(marker))
}

/// `schema` with [`MAX_FREE_TEXT_CHARS`] on every string that has no length,
/// pattern, format, encoding or fixed values. `enum`, `const`, `default` and `examples`
/// hold data, not schemas, and are left alone.
fn limit_free_text(schema: &mut Value) {
    match schema {
        Value::Object(node) => {
            let is_string = match node.get("type") {
                Some(Value::String(t)) => t == "string",
                Some(Value::Array(types)) => types.iter().any(|t| t == "string"),
                _ => false,
            };
            let shaped = [
                "maxLength",
                "minLength",
                "pattern",
                "format",
                "enum",
                "const",
                "contentEncoding",
                "contentMediaType",
            ]
            .iter()
            .any(|k| node.contains_key(*k));
            if is_string && !shaped {
                node.insert("maxLength".to_string(), json!(MAX_FREE_TEXT_CHARS));
            }
            for (key, value) in node.iter_mut() {
                if !matches!(key.as_str(), "enum" | "const" | "default" | "examples") {
                    limit_free_text(value);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(limit_free_text),
        _ => {}
    }
}

/// Whether an error `body` rejects the request's `reasoning` parameter
/// itself (`sent`): it names `reasoning` (or `effort`, when that was sent)
/// and says the parameter or value is unsupported, invalid or mandatory. An
/// error that only mentions reasoning (a context-length error counting
/// reasoning tokens) is not one.
fn rejects_reasoning(body: &str, sent: Reasoning) -> bool {
    let body = body.to_ascii_lowercase();
    let names =
        body.contains("reasoning") || (sent == Reasoning::Minimal && body.contains("effort"));
    if !names || body.contains("context length") || body.contains("context_length") {
        return false;
    }
    [
        "not supported",
        "unsupported",
        "does not support",
        "invalid",
        "cannot be disabled",
        "mandatory",
        "is required",
        "unrecognized",
        "unknown parameter",
        "not allowed",
        "not permitted",
    ]
    .iter()
    .any(|phrase| body.contains(phrase))
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
    usage.get("prompt_tokens").and_then(Value::as_u64)?;
    Some(usage_of(usage))
}

/// The tokens a `usage` object reports (0 for each one it lacks). Reasoning
/// tokens are read from `completion_tokens_details.reasoning_tokens`
/// (OpenAI, OpenRouter), else a top-level `reasoning_tokens`.
fn usage_of(usage: &Value) -> Usage {
    let tok = |pointer: &str| {
        usage
            .pointer(pointer)
            .and_then(Value::as_u64)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
    };
    Usage {
        prompt_tokens: tok("/prompt_tokens").unwrap_or(0),
        completion_tokens: tok("/completion_tokens").unwrap_or(0),
        reasoning_tokens: tok("/completion_tokens_details/reasoning_tokens")
            .or_else(|| tok("/reasoning_tokens"))
            .unwrap_or(0),
    }
}

/// The message of a reply cut at `max_tokens`, saying how much of it went
/// to reasoning when the provider reports that.
fn truncated_message(provider: &str, max_tokens: u32, usage: Usage) -> String {
    let reasoning = if usage.reasoning_tokens > 0 {
        format!(
            ", {} of {} completion tokens spent reasoning",
            usage.reasoning_tokens, usage.completion_tokens
        )
    } else {
        String::new()
    };
    format!(
        "{provider}stopped at max_tokens ({max_tokens}){reasoning} (finish_reason=length); raise the profile's max_tokens"
    )
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
    let usage = v.get("usage").map(usage_of).unwrap_or_default();
    if finish == "length" {
        count_cut(CutReason::MaxTokens);
        let partial = content.unwrap_or_default();
        return Err(LlmError::Truncated {
            message: format!(
                "{} (got {})",
                truncated_message(&provider, max_tokens, usage),
                excerpt(partial)
            ),
            partial: partial.to_string(),
            usage,
        });
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
    Ok(ChatResponse { content, usage })
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

/// Whether the first bytes of a body with no content type are an event
/// stream: an SSE field (`data:`, `event:`, `id:`, `retry:`) or comment.
fn looks_like_sse(first: &[u8]) -> bool {
    let start = first
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map_or(&[][..], |i| &first[i..]);
    [&b"data:"[..], b"event:", b"id:", b"retry:", b":"]
        .iter()
        .any(|field| start.starts_with(field))
}

/// The last 120 bytes of `text`, cut at a character boundary.
fn tail_excerpt(text: &str) -> String {
    const LIMIT: usize = 120;
    if text.len() <= LIMIT {
        return text.to_string();
    }
    let mut start = text.len() - LIMIT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

/// A plain (not streamed) reply body, read by [`parse_reply`].
fn parse_reply_text(text: &str, max_tokens: u32) -> Result<ChatResponse, LlmError> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| LlmError::Decode(format!("{e}: {}", excerpt(text))))?;
    parse_reply(&v, max_tokens)
}

/// The bytes of `req`'s messages, for estimating its prompt tokens.
fn prompt_bytes(req: &ChatRequest) -> usize {
    req.messages.iter().map(|m| m.content.len()).sum()
}

/// `ceil(bytes / 4)`: the estimate of tokens a provider did not report.
fn estimated_tokens(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(4)).unwrap_or(u32::MAX)
}

/// Why a reply was cut short, the `reason` label of [`CUT_REPLIES_METRIC`].
#[derive(Clone, Copy)]
enum CutReason {
    MaxTokens,
    Repetition,
    AnswerCap,
}

fn count_cut(reason: CutReason) {
    let label = match reason {
        CutReason::MaxTokens => "max_tokens",
        CutReason::Repetition => "repetition",
        CutReason::AnswerCap => "answer_cap",
    };
    metrics::counter!(CUT_REPLIES_METRIC, "reason" => label).increment(1);
}

/// The [`LlmError::Repeated`] of a stream stopped once `text` (the answer,
/// or the reasoning when `in_reasoning`) had looped. `usage` is the one the
/// provider reported, if any; otherwise it is estimated from the prompt's
/// bytes and the completion bytes received (answer and reasoning), the
/// reasoning bytes counting as reasoning tokens.
fn repeated(
    provider: &str,
    text: &str,
    in_reasoning: bool,
    (prompt_bytes, completion_bytes, reasoning_bytes): (usize, usize, usize),
    reported: Option<Usage>,
) -> LlmError {
    count_cut(CutReason::Repetition);
    let usage = reported.unwrap_or(Usage {
        prompt_tokens: estimated_tokens(prompt_bytes),
        completion_tokens: estimated_tokens(completion_bytes),
        reasoning_tokens: estimated_tokens(reasoning_bytes),
    });
    let what = if in_reasoning { "reasoning" } else { "answer" };
    LlmError::Repeated {
        message: format!(
            "{provider}aborted after about {} tokens: the last {} KiB of the {what} are a few short pieces repeated ({})",
            usage.completion_tokens,
            crate::repetition::WINDOW / 1024,
            tail_excerpt(text),
        ),
        usage,
    }
}

/// A streamed completion gathered chunk by chunk into the reply a plain
/// request gets, with its answer and its reasoning each watched for loops.
struct GatheredReply {
    provider: Option<Value>,
    content: String,
    reasoning: String,
    finish: Option<Value>,
    usage: Option<Value>,
    content_guard: RepetitionGuard,
    reasoning_guard: RepetitionGuard,
    prompt_bytes: usize,
    /// [`ChatRequest::max_answer_bytes`].
    max_answer: Option<usize>,
}

impl GatheredReply {
    fn new(prompt_bytes: usize, span: usize, max_answer: Option<usize>) -> Self {
        Self {
            max_answer,
            provider: None,
            content: String::new(),
            reasoning: String::new(),
            finish: None,
            usage: None,
            content_guard: RepetitionGuard::with_span(span),
            reasoning_guard: RepetitionGuard::with_span(span),
            prompt_bytes,
        }
    }

    /// Whether a chunk gave a finish reason.
    fn finished(&self) -> bool {
        self.finish.is_some()
    }

    /// The answer and reasoning bytes received.
    fn bytes(&self) -> usize {
        self.content.len() + self.reasoning.len()
    }

    fn provider_prefix(&self) -> String {
        self.provider
            .as_ref()
            .and_then(Value::as_str)
            .map(|p| format!("provider {p}, "))
            .unwrap_or_default()
    }

    /// Adds one chunk; [`LlmError::Repeated`] once the answer or the
    /// reasoning loops.
    fn absorb(&mut self, v: &Value) -> Result<(), LlmError> {
        if let Some(p) = v.get("provider").filter(|p| p.is_string()) {
            self.provider = Some(p.clone());
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(u.clone());
        }
        if let Some(f) = v
            .pointer("/choices/0/finish_reason")
            .filter(|f| f.is_string())
        {
            self.finish = Some(f.clone());
        }
        let delta = |key: &str| {
            v.pointer(&format!("/choices/0/delta/{key}"))
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
        };
        if let Some(text) = delta("content") {
            self.content.push_str(text);
            if self.content_guard.push(text) {
                return Err(self.repeated(false));
            }
            if let Some(max) = self.max_answer.filter(|max| self.content.len() > *max) {
                // An answer looping when it passes its cap is a loop: stopped as
                // one and retried, only sooner than the guard's span.
                if self.content_guard.low_variety() {
                    return Err(self.repeated(false));
                }
                count_cut(CutReason::AnswerCap);
                let usage = self.usage.as_ref().map(usage_of).unwrap_or_else(|| Usage {
                    prompt_tokens: estimated_tokens(self.prompt_bytes),
                    completion_tokens: estimated_tokens(self.bytes()),
                    reasoning_tokens: estimated_tokens(self.reasoning.len()),
                });
                return Err(LlmError::Truncated {
                    message: format!(
                        "{}stopped after {} bytes of answer, past the {max} an answer to its schema is expected to need",
                        self.provider_prefix(),
                        self.content.len()
                    ),
                    partial: self.content.clone(),
                    usage,
                });
            }
        }
        // Some gateways send the reasoning under both names: the first one
        // present is it.
        if let Some(text) = delta("reasoning").or_else(|| delta("reasoning_content")) {
            self.reasoning.push_str(text);
            if self.reasoning_guard.push(text) {
                return Err(self.repeated(true));
            }
        }
        Ok(())
    }

    fn repeated(&self, in_reasoning: bool) -> LlmError {
        let provider = self.provider_prefix();
        let text = if in_reasoning {
            &self.reasoning
        } else {
            &self.content
        };
        let reported = self.usage.as_ref().map(usage_of);
        repeated(
            &provider,
            text,
            in_reasoning,
            (self.prompt_bytes, self.bytes(), self.reasoning.len()),
            reported,
        )
    }

    /// The reply a plain request would have got. Without a usage chunk its
    /// usage is estimated from the bytes, so a call that produced text never
    /// reports 0 tokens.
    fn into_reply(self) -> Value {
        let usage = self.usage.clone().unwrap_or_else(|| {
            json!({
                "prompt_tokens": estimated_tokens(self.prompt_bytes),
                "completion_tokens": estimated_tokens(self.bytes()),
                "completion_tokens_details": {
                    "reasoning_tokens": estimated_tokens(self.reasoning.len()),
                },
            })
        });
        let mut choice = json!({"message": {
            "content": (!self.content.is_empty()).then_some(self.content),
            "reasoning": self.reasoning,
        }});
        if let (Some(f), Some(obj)) = (self.finish, choice.as_object_mut()) {
            obj.insert("finish_reason".into(), f);
        }
        let mut reply = json!({"choices": [choice]});
        if let Some(obj) = reply.as_object_mut() {
            if let Some(p) = self.provider {
                obj.insert("provider".into(), p);
            }
            obj.insert("usage".into(), usage);
        }
        reply
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiCompatible {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: self.config.output_mode == OutputMode::JsonSchema,
            json_mode: self.config.output_mode != OutputMode::None,
        }
    }

    /// Records the model as `llm.model` on the current span (the engine's
    /// `mb.llm` span).
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        tracing::Span::current().record("llm.model", self.config.model.as_str());
        let deadline = tokio::time::Instant::now() + req.timeout;
        // A failed attempt that spent tokens, kept outside the timed call:
        // when the retry runs out of time, that error (and its tokens) is
        // the answer rather than a bare timeout.
        let stopped = Mutex::new(None);
        match tokio::time::timeout_at(deadline, self.send_with_retry(&req, deadline, &stopped))
            .await
        {
            Ok(r) => r,
            Err(_) => match stopped.lock().ok().and_then(|mut held| held.take()) {
                Some(e) => Err(e),
                None => Err(LlmError::Timeout),
            },
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
        let prompt_bytes = prompt_bytes(&req);
        let usage = StreamUsage::new();
        let reported = usage.clone();
        // Watches the text for loops, with the text received so far.
        let span = self.repetition_span;
        let watch = self
            .repetition_guard
            .then(|| (RepetitionGuard::with_span(span), String::new(), 0usize));
        // The state is the event source, an error to yield after the delta
        // just returned (a truncation marked on a chunk with content) and the
        // loop watch; `None` once the stream has ended.
        let deltas = futures::stream::unfold(Some((events, None, watch)), move |state| {
            let reported = reported.clone();
            async move {
                let (mut events, pending, mut watch) = state?;
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
                        count_cut(CutReason::MaxTokens);
                        let usage = reported.get().unwrap_or_default();
                        LlmError::Truncated {
                            message: truncated_message(&provider_prefix(&v), max_tokens, usage),
                            partial: String::new(),
                            usage,
                        }
                    });
                    let text = v
                        .pointer("/choices/0/delta/content")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty());
                    if let (Some(text), Some((guard, seen, received))) = (text, watch.as_mut()) {
                        seen.push_str(text);
                        *received += text.len();
                        if guard.push(text) {
                            let provider = provider_prefix(&v);
                            let bytes = (prompt_bytes, *received, 0);
                            let error = repeated(&provider, seen, false, bytes, reported.get());
                            return Some((Err(error), None));
                        }
                        // Only the tail is ever shown.
                        if seen.len() > 2 * crate::repetition::WINDOW {
                            let mut cut = seen.len() - crate::repetition::WINDOW;
                            while !seen.is_char_boundary(cut) {
                                cut += 1;
                            }
                            seen.drain(..cut);
                        }
                    }
                    match (text, truncated) {
                        (Some(text), truncated) => {
                            return Some((Ok(text.to_string()), Some((events, truncated, watch))));
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
