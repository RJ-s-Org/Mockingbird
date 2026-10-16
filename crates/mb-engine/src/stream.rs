//! Streamed generation: NDJSON items from one provider stream, each
//! validated as it completes, repaired once or replaced by faker output.
//!
//! The returned stream owns its state and is polled by the consumer; nothing
//! is spawned, so dropping the stream drops the provider stream with it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use mb_llm::{ChatMessage, ChatRequest, Provider, ResponseFormat, Role, TextStream};
use mb_mcm::{Bundle, Operation};
use serde_json::Value;
use tokio::time::Instant;

use crate::guards::{Checked, GenerationPermit};
use crate::ndjson::LineSplitter;
use crate::prompt::{self, build_stream_messages};
use crate::validate::SchemaValidator;
use crate::{
    Engine, EngineError, OperationCall, Source, StreamContext, StreamItem, apply_set, format_name,
    success_status,
};

impl Engine {
    /// Streams `N = max_items.unwrap_or(default_items)` items, clamped to
    /// `1..=max_items`. With a provider, one text stream is opened on first
    /// poll and each NDJSON line becomes an item (`Llm`), repaired once
    /// (`Repaired`) or replaced by faker output (`Fallback`); once the
    /// provider stream ends, fails or times out, the remaining items come
    /// from the faker. Offline, every item comes from the faker.
    pub fn generate_stream(
        self: &Arc<Self>,
        bundle: Arc<Bundle>,
        op: &Operation,
        call: OperationCall,
        ctx: StreamContext,
        max_items: Option<usize>,
    ) -> Result<BoxStream<'static, StreamItem>, EngineError> {
        let schema = op
            .stream_item_schema()
            .ok_or(EngineError::NoOutput)?
            .clone();
        let status = op
            .success_output()
            .map(|o| success_status(o.status))
            .ok_or(EngineError::NoOutput)?;
        let validator = self.validator(&bundle, op, status, &schema)?;
        let limits = &self.config.stream;
        let items = max_items
            .unwrap_or(limits.default_items)
            .clamp(1, limits.max_items.max(1));
        let interval = ctx.item_interval.unwrap_or(limits.item_interval);

        let (upstream, repair_system) = match &self.provider {
            None => (Upstream::Closed, None),
            Some(_) => {
                let messages = build_stream_messages(
                    &bundle,
                    op,
                    &schema,
                    items,
                    &call,
                    &ctx,
                    self.config.persona.as_deref(),
                );
                let system = messages.first().cloned();
                let request = self.chat_request(messages, ResponseFormat::Text);
                (Upstream::Pending(request), system)
            }
        };

        let state = StreamState {
            engine: Arc::clone(self),
            provider: self.provider.clone(),
            bundle,
            op_id: op.id.clone(),
            schema,
            validator,
            set: ctx.scenario.and_then(|s| s.set),
            items,
            interval,
            emitted: 0,
            upstream,
            repair_system,
            lines: VecDeque::new(),
            permit: None,
        };
        Ok(futures::stream::unfold(state, |mut state| async move {
            let item = state.next_item().await?;
            Some((item, state))
        })
        .boxed())
    }

    fn chat_request(&self, messages: Vec<ChatMessage>, format: ResponseFormat) -> ChatRequest {
        ChatRequest {
            messages,
            response_format: format,
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            timeout: self.config.timeout,
        }
    }
}

enum Upstream {
    /// Not opened yet: the request to open it with.
    Pending(ChatRequest),
    Open {
        stream: TextStream,
        splitter: LineSplitter,
    },
    /// Offline, finished, failed or timed out: only the faker remains.
    Closed,
}

struct StreamState {
    engine: Arc<Engine>,
    /// `None` when offline.
    provider: Option<Arc<dyn Provider>>,
    bundle: Arc<Bundle>,
    op_id: String,
    schema: Value,
    validator: Arc<SchemaValidator>,
    set: Option<Value>,
    items: usize,
    interval: Duration,
    emitted: usize,
    upstream: Upstream,
    /// The stream prompt's system message, reused for repairs.
    repair_system: Option<ChatMessage>,
    /// Complete lines received but not yet consumed.
    lines: VecDeque<String>,
    /// The generation permit, held from opening the provider stream until
    /// it has no more candidates (or this stream is done or dropped).
    permit: Option<GenerationPermit>,
}

impl StreamState {
    async fn next_item(&mut self) -> Option<StreamItem> {
        if self.emitted >= self.items {
            self.release();
            return None;
        }
        if self.emitted > 0 && !self.interval.is_zero() {
            tokio::time::sleep(self.interval).await;
        }
        let (value, source) = match (self.next_candidate().await, self.provider.clone()) {
            (Some(line), Some(provider)) => self.accept(provider.as_ref(), &line).await,
            _ => (self.fake(), Source::Fallback),
        };
        self.emitted += 1;
        Some(StreamItem {
            value: self.apply_scenario(value),
            source,
        })
    }

    /// The next candidate line from the provider, or `None` once it has
    /// nothing more within the deadline: the provider's total timeout from
    /// opening for the first item, the request timeout for each later one
    /// (measured after any item interval).
    async fn next_candidate(&mut self) -> Option<String> {
        let provider = self.provider.clone()?;
        let timeout = self.engine.config.timeout;
        let deadline = Instant::now()
            + match self.upstream {
                Upstream::Pending(_) => provider.total_timeout(timeout),
                _ => timeout,
            };
        loop {
            while let Some(line) = self.lines.pop_front() {
                if let Some(candidate) = candidate(&line) {
                    return Some(candidate.to_string());
                }
            }
            match std::mem::replace(&mut self.upstream, Upstream::Closed) {
                Upstream::Closed => {
                    self.permit = None;
                    return None;
                }
                Upstream::Pending(request) => {
                    match self.engine.admit(deadline).await {
                        Ok(permit) => self.permit = permit,
                        Err(reason) => {
                            self.give_up(&reason);
                            continue;
                        }
                    }
                    match tokio::time::timeout_at(deadline, provider.stream(request)).await {
                        Ok(Ok(stream)) => {
                            self.upstream = Upstream::Open {
                                stream,
                                splitter: LineSplitter::new(),
                            };
                        }
                        Ok(Err(e)) => self.give_up(&e.to_string()),
                        Err(_) => self.give_up("LLM stream timed out"),
                    }
                }
                Upstream::Open {
                    mut stream,
                    mut splitter,
                } => match tokio::time::timeout_at(deadline, stream.next()).await {
                    Ok(Some(Ok(delta))) => {
                        self.lines.extend(splitter.push(&delta));
                        self.upstream = Upstream::Open { stream, splitter };
                    }
                    Ok(None) => self.lines.extend(splitter.finish()),
                    Ok(Some(Err(e))) => self.give_up(&e.to_string()),
                    Err(_) => self.give_up("LLM stream timed out"),
                },
            }
        }
    }

    /// Close the provider stream and release the generation permit.
    fn release(&mut self) {
        self.upstream = Upstream::Closed;
        self.lines.clear();
        self.permit = None;
    }

    fn give_up(&mut self, reason: &str) {
        tracing::warn!(
            operation = %self.op_id,
            %reason,
            remaining = self.items - self.emitted,
            "LLM stream failed; filling with faker items"
        );
    }

    /// A candidate line as an item: valid → `Llm`; otherwise one repair
    /// call → `Repaired`, else a faker item.
    async fn accept(&mut self, provider: &dyn Provider, line: &str) -> (Value, Source) {
        let errors = match self.engine.check_output(&self.validator, line) {
            Checked::Valid(value) => return (value, Source::Llm),
            Checked::Rejected(reason) => {
                tracing::warn!(operation = %self.op_id, %reason, "stream item fell back to faker");
                return (self.fake(), Source::Fallback);
            }
            Checked::Invalid(errors) => errors,
        };
        match self.repair(provider, line, &errors).await {
            Ok(value) => (value, Source::Repaired),
            Err(reason) => {
                tracing::warn!(operation = %self.op_id, %reason, "stream item fell back to faker");
                (self.fake(), Source::Fallback)
            }
        }
    }

    /// `&mut self`, not `&self`: the state holds a `!Sync` provider stream,
    /// and the returned stream must stay `Send`.
    async fn repair(
        &mut self,
        provider: &dyn Provider,
        line: &str,
        errors: &[String],
    ) -> Result<Value, String> {
        let engine = &self.engine;
        if engine.budget_exhausted() {
            return Err("token budget exhausted".into());
        }
        let item = match serde_json::from_str::<Value>(line) {
            Ok(value) => prompt::data(&value),
            Err(_) => prompt::escape(line),
        };
        let mut messages: Vec<ChatMessage> = self.repair_system.iter().cloned().collect();
        messages.push(ChatMessage {
            role: Role::User,
            content: format!(
                "Fix this stream item. Errors: {}\n<item>{item}</item>\nReturn only the corrected JSON.",
                errors.join("; ")
            ),
        });
        let format = ResponseFormat::JsonSchema {
            name: format_name(&self.op_id),
            schema: self.bundle.with_reachable_defs(&self.schema),
        };
        let request = engine.chat_request(messages, format);
        let limit = provider.total_timeout(engine.config.timeout);
        let text = match tokio::time::timeout(limit, provider.complete(request)).await {
            Ok(Ok(response)) => {
                engine.spend(response.usage);
                response.content
            }
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => return Err("LLM repair timed out".into()),
        };
        match engine.check_output(&self.validator, &text) {
            Checked::Valid(value) => Ok(value),
            Checked::Rejected(reason) => Err(reason),
            Checked::Invalid(errors) => Err(errors.join("; ")),
        }
    }

    fn fake(&self) -> Value {
        self.engine
            .fake_value(&self.bundle, &self.schema, Some(&self.validator))
    }

    /// Apply the scenario's `set` merge patch to an object item, keeping the
    /// patch only when the result still validates.
    fn apply_scenario(&self, value: Value) -> Value {
        let Some(set) = &self.set else {
            return value;
        };
        if !value.is_object() {
            return value;
        }
        let patched = apply_set(&value, set);
        if self.validator.validate(&patched).is_ok() {
            patched
        } else {
            value
        }
    }
}

/// The JSON text of a received line: `None` for blank lines and Markdown
/// fences; a trailing `,` (from an array-style listing) is dropped.
fn candidate(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with("```") {
        return None;
    }
    Some(line.strip_suffix(',').unwrap_or(line).trim_end())
}
