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
use mb_llm::{
    ChatMessage, ChatRequest, Provider, ResponseFormat, Role, StreamUsage, TextStream, Usage,
};
use mb_mcm::{Bundle, Operation};
use serde_json::Value;
use tokio::time::Instant;
use tracing::Instrument;

use crate::guards::{self, Checked, Checks, GenerationPermit};
use crate::ndjson::LineSplitter;
use crate::prompt::{self, build_stream_messages};
use crate::validate::SchemaValidator;
use crate::{
    Engine, EngineError, OperationCall, Source, StreamContext, StreamItem, apply_set, format_name,
    llm_span, record_usage, success_status,
};

impl Engine {
    /// Streams `N = max_items.unwrap_or(default_items)` items, clamped to
    /// `1..=max_items`. With a provider, one text stream is opened on first
    /// poll and each NDJSON line becomes an item (`Llm`), repaired once
    /// (`Repaired`) or replaced by faker output (`Fallback`); once the
    /// provider stream ends, fails or times out, the remaining items come
    /// from the faker. Offline, every item comes from the faker.
    ///
    /// # Errors
    ///
    /// [`EngineError::NoOutput`] when `op` declares no stream item schema,
    /// and [`EngineError::Schema`] when that schema does not compile.
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
        // One generator for the stream: its items differ from each other,
        // and a seeded stream repeats item for item.
        let draws = self.call_draws(&bundle, &op.id, status, &call);

        let (upstream, repair_system) = match &self.provider {
            None => (Upstream::Closed, None),
            Some(_) => {
                let messages = tracing::info_span!("mb.prompt").in_scope(|| {
                    build_stream_messages(
                        &bundle,
                        op,
                        &schema,
                        items,
                        &call,
                        &ctx,
                        self.config.persona.as_deref(),
                    )
                });
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
            prompt_bytes: 0,
            completion_bytes: 0,
            reported: StreamUsage::new(),
            attributed: Usage::default(),
            repair_usage: Usage::default(),
            draws,
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
        meter: StreamMeter,
    },
    /// Offline, finished, failed or timed out: only the faker remains.
    Closed,
}

/// The longest the stream waits, after its last item, for the provider
/// stream to end, so the usage it reports at its end can be read.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// Charges an open provider stream to the token budget when it closes
/// (ends, fails, times out, is released or dropped): the usage the provider
/// reported ([`Provider::stream_metered`]), else an estimate of
/// `ceil(bytes / 4)` of the prompt and of the text received.
struct StreamMeter {
    engine: Arc<Engine>,
    prompt_bytes: usize,
    completion_bytes: usize,
    reported: StreamUsage,
    /// The provider call's `mb.llm` span: open until the stream closes,
    /// when the estimate is recorded on it.
    span: tracing::Span,
}

impl Drop for StreamMeter {
    fn drop(&mut self) {
        let usage = self
            .reported
            .get()
            .unwrap_or_else(|| estimate_usage(self.prompt_bytes, self.completion_bytes));
        record_usage(&self.span, usage);
        self.engine.spend(usage);
    }
}

/// The estimated tokens of a provider stream: `ceil(bytes / 4)` of the
/// prompt, and of the prompt plus the text received for the total (the sum
/// is rounded, not each half).
fn estimate_usage(prompt_bytes: usize, completion_bytes: usize) -> Usage {
    let estimate = |bytes: usize| u32::try_from(bytes.div_ceil(4)).unwrap_or(u32::MAX);
    let prompt_tokens = estimate(prompt_bytes);
    let completion_tokens =
        estimate(prompt_bytes.saturating_add(completion_bytes)).saturating_sub(prompt_tokens);
    Usage {
        prompt_tokens,
        completion_tokens,
        reasoning_tokens: 0,
    }
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
    /// Bytes of the provider stream's prompt and of the text received so
    /// far (mirrors its [`StreamMeter`], which outlives no item).
    prompt_bytes: usize,
    completion_bytes: usize,
    /// The usage the provider stream reports, once it has ended.
    reported: StreamUsage,
    /// The stream usage already attributed to emitted items.
    attributed: Usage,
    /// Repair-call tokens not yet attributed to an item.
    repair_usage: Usage,
    /// The faker draws of the stream's fallback items.
    draws: crate::seed::Draws,
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
        if self.emitted == self.items {
            self.drain().await;
        }
        let usage = self.take_usage();
        Some(StreamItem {
            value: self.apply_scenario(value),
            source,
            usage,
        })
    }

    /// The tokens spent since the previous item: the growth of the provider
    /// stream's usage (the prompt counts toward the first item that reads
    /// it) plus any repair call. The stream's usage is an estimate until the
    /// provider reports it at the end of its stream; the item read then
    /// carries the difference (or nothing, when the estimate was higher).
    /// Summed over every item, this is what the stream charges to the token
    /// budget, less text received after the last item, when the estimate
    /// was not too high.
    fn take_usage(&mut self) -> Usage {
        let total = self
            .reported
            .get()
            .unwrap_or_else(|| estimate_usage(self.prompt_bytes, self.completion_bytes));
        let stream = Usage {
            prompt_tokens: total
                .prompt_tokens
                .saturating_sub(self.attributed.prompt_tokens),
            completion_tokens: total
                .completion_tokens
                .saturating_sub(self.attributed.completion_tokens),
            reasoning_tokens: total
                .reasoning_tokens
                .saturating_sub(self.attributed.reasoning_tokens),
        };
        self.attributed = total;
        stream + std::mem::take(&mut self.repair_usage)
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
                    let prompt_bytes = request.messages.iter().map(|m| m.content.len()).sum();
                    // One span for the whole provider stream, never one per item.
                    let span = llm_span();
                    let open = provider.stream_metered(request).instrument(span.clone());
                    match tokio::time::timeout_at(deadline, open).await {
                        Ok(Ok(metered)) => {
                            self.prompt_bytes = prompt_bytes;
                            self.reported = metered.usage.clone();
                            self.upstream = Upstream::Open {
                                stream: metered.text,
                                splitter: LineSplitter::new(),
                                meter: StreamMeter {
                                    engine: Arc::clone(&self.engine),
                                    prompt_bytes,
                                    completion_bytes: 0,
                                    reported: metered.usage,
                                    span,
                                },
                            };
                        }
                        Ok(Err(e)) => self.give_up(&e.to_string()),
                        Err(_) => self.give_up("LLM stream timed out"),
                    }
                }
                Upstream::Open {
                    mut stream,
                    mut splitter,
                    mut meter,
                } => match tokio::time::timeout_at(deadline, stream.next()).await {
                    Ok(Some(Ok(delta))) => {
                        meter.completion_bytes += delta.len();
                        self.completion_bytes += delta.len();
                        self.lines.extend(splitter.push(&delta));
                        self.upstream = Upstream::Open {
                            stream,
                            splitter,
                            meter,
                        };
                    }
                    Ok(None) => self.lines.extend(splitter.finish()),
                    Ok(Some(Err(e))) => {
                        // The line cut at max_tokens is still a candidate,
                        // for salvage (see `accept`).
                        if matches!(e, mb_llm::LlmError::Truncated { .. }) {
                            self.lines.extend(splitter.finish());
                        }
                        self.give_up(&e.to_string());
                    }
                    Err(_) => self.give_up("LLM stream timed out"),
                },
            }
        }
    }

    /// After the last item: reads the open provider stream to its end, for
    /// at most [`DRAIN_TIMEOUT`] (and the request timeout), so the usage it
    /// reports at the end is known. Text other than whitespace means the
    /// model writes more than was asked for, so the stream is left to be
    /// closed (and estimated) instead.
    async fn drain(&mut self) {
        let Upstream::Open { stream, meter, .. } = &mut self.upstream else {
            return;
        };
        let deadline = Instant::now() + DRAIN_TIMEOUT.min(self.engine.config.timeout);
        loop {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(Ok(delta))) => {
                    meter.completion_bytes += delta.len();
                    self.completion_bytes += delta.len();
                    if !delta.trim().is_empty() {
                        return;
                    }
                }
                Ok(Some(Err(_)) | None) | Err(_) => return,
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

    /// A candidate line as an item: valid → `Llm`, fixed locally (or cut
    /// short and salvaged: the stream stopped at `max_tokens`) → `Patched`;
    /// otherwise one repair call → `Repaired`, else a faker item.
    async fn accept(&mut self, provider: &dyn Provider, line: &str) -> (Value, Source) {
        let checks = Checks::default();
        let checked = match self.engine.check_output(&self.validator, line, &checks) {
            Checked::Invalid(errors) if errors.len() == 1 && errors[0] == guards::NOT_JSON => {
                match self.engine.check_truncated(&self.validator, line, &checks) {
                    salvaged @ Checked::Patched(_) => salvaged,
                    _ => Checked::Invalid(errors),
                }
            }
            other => other,
        };
        let errors = match checked {
            Checked::Valid(value) => return (value, Source::Llm),
            Checked::Patched(value) => return (value, Source::Patched),
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
        if self.engine.budget_exhausted() {
            return Err("token budget exhausted".into());
        }
        self.repair_call(provider, line, errors)
            .instrument(tracing::info_span!("mb.repair"))
            .await
    }

    /// The repair call of [`StreamState::repair`], in its `mb.repair` span.
    async fn repair_call(
        &mut self,
        provider: &dyn Provider,
        line: &str,
        errors: &[String],
    ) -> Result<Value, String> {
        let engine = &self.engine;
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
            schema: crate::compact::prompt_schema(&self.bundle, &self.schema),
        };
        let request = engine.chat_request(messages, format);
        let limit = provider.total_timeout(engine.config.timeout);
        let span = llm_span();
        let call = provider.complete(request).instrument(span.clone());
        let text = match tokio::time::timeout(limit, call).await {
            Ok(Ok(response)) => {
                record_usage(&span, response.usage);
                engine.spend(response.usage);
                self.repair_usage = self.repair_usage + response.usage;
                response.content
            }
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => return Err("LLM repair timed out".into()),
        };
        match engine.check_output(&self.validator, &text, &Checks::default()) {
            Checked::Valid(value) | Checked::Patched(value) => Ok(value),
            Checked::Rejected(reason) => Err(reason),
            Checked::Invalid(errors) => Err(errors.join("; ")),
        }
    }

    fn fake(&mut self) -> Value {
        self.engine.fake_value(
            &self.bundle,
            &self.schema,
            Some(&self.validator),
            &crate::FakerHints::default(),
            &mut self.draws,
        )
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
        if self.validator.is_valid(&patched) {
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
