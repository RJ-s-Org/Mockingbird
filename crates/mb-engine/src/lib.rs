//! Mock response engine: prompt building, LLM generation with
//! validate → repair → fallback, schema validation and schema-driven faking.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use mb_llm::{ChatMessage, ChatRequest, Provider, ResponseFormat, Role, Usage};
use mb_mcm::{Bundle, Effect, Operation, OperationKind, OutputVariant, StatusSelector};
use mb_state::StoredResource;
use serde_json::{Map, Value};
use tokio::time::Instant;
use tracing::Instrument;
use tracing::field::Empty;

use crate::guards::Checked;

pub mod compact;
pub mod consistency;
pub mod extract;
pub mod faker;
pub mod guards;
pub mod message;
pub mod ndjson;
pub mod pii;
pub mod prompt;
pub mod scenario;
pub mod stream;
pub mod validate;

pub use extract::extract_json;
pub use faker::{Costs, Faker};
pub use guards::{EngineGuards, GenerationLimiter, TokenBudget};
pub use message::{GeneratedMessage, MessageContext};
pub use prompt::{build_message_messages, build_messages, build_stream_messages};
pub use scenario::{ScenarioSpec, apply_set};
pub use validate::SchemaValidator;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("invalid schema: {0}")]
    Schema(String),
    #[error("status {0} is not declared by the operation")]
    UndeclaredStatus(u16),
    #[error("operation declares no success output")]
    NoOutput,
}

/// A decoded, protocol-neutral invocation of an operation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OperationCall {
    pub operation_id: String,
    pub path_params: Map<String, Value>,
    pub query: Map<String, Value>,
    /// Only non-sensitive headers; the protocol adapter filters them.
    pub headers: Map<String, Value>,
    pub body: Option<Value>,
}

/// One root field of a GraphQL query answered by
/// [`Engine::generate_joint`]: its response key, operation, call and context
/// (a 2xx response; `ctx.response_schema` narrows it).
pub struct JointPart<'a> {
    pub key: &'a str,
    pub op: &'a Operation,
    pub call: &'a OperationCall,
    pub ctx: GenerationContext<'a>,
}

/// State the response must stay consistent with.
#[derive(Clone, Copy, Debug, Default)]
pub struct GenerationContext<'a> {
    pub related: &'a [StoredResource],
    /// Existing resource for query/update/delete.
    pub target: Option<&'a StoredResource>,
    pub forced_status: Option<u16>,
    /// The active scenario: its instructions go in the prompt and its `set`
    /// patches 2xx object bodies.
    pub scenario: Option<&'a ScenarioSpec>,
    /// A narrower schema for a 2xx response, used instead of the operation's
    /// (in the prompt, for validation and by the faker): the fields a
    /// GraphQL selection set asks for. Self-contained (no `$ref`).
    pub response_schema: Option<&'a Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Llm,
    Repaired,
    Fallback,
    /// Served from the session's stored resource, without the LLM.
    State,
    /// A repeat of an earlier answer in the same session.
    Cache,
}

impl Source {
    /// The less trustworthy of two sources: `Fallback` > `Repaired` > `Llm`.
    pub fn worst(self, other: Source) -> Source {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }

    fn rank(self) -> u8 {
        match self {
            Source::Llm | Source::State | Source::Cache => 0,
            Source::Repaired => 1,
            Source::Fallback => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Source::Llm => "llm",
            Source::Repaired => "repaired",
            Source::Fallback => "fallback",
            Source::State => "state",
            Source::Cache => "cache",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GeneratedResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Option<Value>,
    pub source: Source,
    /// Tokens spent on this response, summed over the primary and repair
    /// calls; zero when no LLM call was made.
    pub usage: Usage,
    /// Why the faker answered instead of the LLM, when it did because an LLM
    /// call failed or was not made (`None` for LLM answers and for
    /// deliberate fallbacks such as a forced status with no LLM).
    pub fallback_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub timeout: Duration,
    pub temperature: f32,
    pub max_tokens: u32,
    pub persona: Option<String>,
    pub faker_depth: usize,
    pub stream: StreamConfig,
}

/// Limits and pacing for [`Engine::generate_stream`].
#[derive(Clone, Debug)]
pub struct StreamConfig {
    /// Items generated when the caller asks for no particular count.
    pub default_items: usize,
    /// Upper bound on items per stream.
    pub max_items: usize,
    /// Pause before every item except the first.
    pub item_interval: Duration,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            default_items: 5,
            max_items: 50,
            item_interval: Duration::ZERO,
        }
    }
}

/// Owned state a stream's items must stay consistent with.
#[derive(Clone, Debug, Default)]
pub struct StreamContext {
    pub related: Vec<StoredResource>,
    pub target: Option<StoredResource>,
    /// The active scenario: its instructions go in the prompt and its `set`
    /// patches object items.
    pub scenario: Option<ScenarioSpec>,
    /// Items the client has sent so far (client and bidi streams).
    pub request_stream: Vec<Value>,
    /// Overrides [`StreamConfig::item_interval`].
    pub item_interval: Option<Duration>,
}

/// One generated stream item.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamItem {
    pub value: Value,
    pub source: Source,
    /// Tokens attributed to this item: its share of the provider stream's
    /// estimate (see [`Engine::generate_stream`]) plus any repair call; zero
    /// for a faker item that read nothing from the provider.
    pub usage: Usage,
}

/// The fallback reason of a mock with no LLM provider.
pub const OFFLINE_REASON: &str = "no LLM is configured for this mock (offline)";

/// Faker attempts (cycling through top-level `oneOf`/`anyOf` branches and
/// fresh random draws) before an unvalidated fallback body is accepted.
const FALLBACK_ATTEMPTS: usize = 12;

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3),
            temperature: 0.7,
            max_tokens: 2048,
            persona: None,
            faker_depth: 6,
            stream: StreamConfig::default(),
        }
    }
}

/// Generates schema-valid responses, preferring the LLM and falling back to
/// the faker.
pub struct Engine {
    /// `None` means offline: every response comes from the faker.
    provider: Option<Arc<dyn Provider>>,
    config: EngineConfig,
    /// Keyed by (`<source digest>/<operation id>`, status).
    validators: DashMap<(String, u16), Arc<SchemaValidator>>,
    guards: EngineGuards,
}

impl Engine {
    pub fn new(provider: Option<Arc<dyn Provider>>, config: EngineConfig) -> Self {
        Self {
            provider,
            config,
            validators: DashMap::new(),
            guards: EngineGuards::default(),
        }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Whether `value` is a valid stream item of `op`; `false` when `op`
    /// has no stream item schema.
    pub fn conform_item(&self, bundle: &Bundle, op: &Operation, value: &Value) -> bool {
        let Some(schema) = op.stream_item_schema() else {
            return false;
        };
        let Some(status) = op.success_output().map(|o| success_status(o.status)) else {
            return false;
        };
        match self.validator(bundle, op, status, schema) {
            Ok(validator) => validator.validate(value).is_ok(),
            Err(e) => {
                tracing::warn!(operation = %op.id, error = %e, "stream item schema cannot be validated");
                false
            }
        }
    }

    pub async fn generate(
        &self,
        bundle: &Bundle,
        op: &Operation,
        call: &OperationCall,
        ctx: GenerationContext<'_>,
    ) -> Result<GeneratedResponse, EngineError> {
        let (variant, status) = match ctx.forced_status {
            Some(code) => (
                op.output_for_status(code)
                    .ok_or(EngineError::UndeclaredStatus(code))?,
                code,
            ),
            None => {
                let variant = op.success_output().ok_or(EngineError::NoOutput)?;
                (variant, success_status(variant.status))
            }
        };

        let narrowed = ctx
            .response_schema
            .filter(|_| (200..300).contains(&status) && variant.schema.is_some());
        let Some(schema) = narrowed.or(variant.schema.as_ref()) else {
            return Ok(GeneratedResponse {
                status,
                content_type: variant.content_type.clone(),
                body: None,
                source: Source::Llm,
                usage: Usage::default(),
                fallback_reason: None,
            });
        };

        // A narrowed schema varies per query, so its validator is not cached.
        let validator = match narrowed {
            Some(schema) => Arc::new(SchemaValidator::new(&bundle.with_reachable_defs(schema))?),
            None => self.validator(bundle, op, status, schema)?,
        };
        let mut usage = Usage::default();
        let mut response = match &self.provider {
            None => {
                let mut fake =
                    self.fake_response(bundle, variant, status, schema, Some(&validator));
                fake.fallback_reason = Some(OFFLINE_REASON.to_string());
                fake
            }
            Some(provider) => {
                let messages = tracing::info_span!("mb.prompt").in_scope(|| {
                    build_messages(
                        bundle,
                        op,
                        status,
                        schema,
                        call,
                        &ctx,
                        self.config.persona.as_deref(),
                    )
                });
                let format = ResponseFormat::JsonSchema {
                    name: format_name(&op.id),
                    schema: compact::prompt_schema(bundle, schema),
                };
                match self
                    .generate_with_llm(provider.as_ref(), &validator, messages, &format, &mut usage)
                    .await
                {
                    Ok((body, source)) => GeneratedResponse {
                        status,
                        content_type: variant.content_type.clone(),
                        body: Some(body),
                        source,
                        usage: Usage::default(),
                        fallback_reason: None,
                    },
                    Err(reason) => {
                        tracing::warn!(operation = %op.id, %reason, "LLM generation fell back to faker");
                        let mut fake =
                            self.fake_response(bundle, variant, status, schema, Some(&validator));
                        fake.fallback_reason = Some(reason);
                        fake
                    }
                }
            }
        };
        response.usage = usage;
        Ok(self.finish(&validator, bundle, schema, op, call, &ctx, response))
    }

    /// Answers every part with one LLM call: one object keyed by the parts'
    /// keys (a nullable part may be left out and reads as null), validated (and repaired) as a whole, then split and finished per
    /// part. The call's usage is reported on the first part. Without a
    /// provider, for a part with a forced status or no schema, or when the
    /// joint answer stays invalid, every part is generated on its own.
    pub async fn generate_joint(
        &self,
        bundle: &Bundle,
        parts: &[JointPart<'_>],
    ) -> Vec<Result<GeneratedResponse, EngineError>> {
        match self.try_joint(bundle, parts).await {
            Some(responses) => responses.into_iter().map(Ok).collect(),
            None => {
                futures::future::join_all(
                    parts
                        .iter()
                        .map(|p| self.generate(bundle, p.op, p.call, p.ctx)),
                )
                .await
            }
        }
    }

    async fn try_joint(
        &self,
        bundle: &Bundle,
        parts: &[JointPart<'_>],
    ) -> Option<Vec<GeneratedResponse>> {
        let provider = self.provider.as_ref()?;
        let mut shapes = Vec::with_capacity(parts.len());
        let mut properties = Map::new();
        for part in parts {
            if part.ctx.forced_status.is_some() {
                return None;
            }
            let variant = part.op.success_output()?;
            let status = success_status(variant.status);
            let schema = part.ctx.response_schema.or(variant.schema.as_ref())?;
            let validator = match part.ctx.response_schema {
                Some(s) => Arc::new(SchemaValidator::new(&bundle.with_reachable_defs(s)).ok()?),
                None => self.validator(bundle, part.op, status, schema).ok()?,
            };
            properties.insert(part.key.to_string(), schema.clone());
            shapes.push((variant, status, validator));
        }
        // A part that may be null may also be left out: models drop the
        // fields they mean as null.
        let keys: Vec<Value> = parts
            .iter()
            .zip(&shapes)
            .filter(|(_, (_, _, validator))| validator.validate(&Value::Null).is_err())
            .map(|(p, _)| Value::from(p.key))
            .collect();
        let combined = serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": keys,
        });
        let validator = SchemaValidator::new(&bundle.with_reachable_defs(&combined)).ok()?;
        let prompt_schema = compact::prompt_schema(bundle, &combined);
        let messages = tracing::info_span!("mb.prompt").in_scope(|| {
            prompt::build_joint_messages(
                bundle,
                parts,
                &prompt_schema,
                self.config.persona.as_deref(),
            )
        });
        let format = ResponseFormat::JsonSchema {
            name: "query".into(),
            schema: prompt_schema,
        };
        let mut usage = Usage::default();
        let (value, source) = match self
            .generate_with_llm(provider.as_ref(), &validator, messages, &format, &mut usage)
            .await
        {
            Ok(answer) => answer,
            Err(reason) => {
                tracing::info!(%reason, "joint generation failed; answering each field on its own");
                return None;
            }
        };
        let mut responses = Vec::with_capacity(parts.len());
        for (i, (part, (variant, status, validator))) in parts.iter().zip(shapes).enumerate() {
            let response = GeneratedResponse {
                status,
                content_type: variant.content_type.clone(),
                body: Some(value.get(part.key).cloned().unwrap_or(Value::Null)),
                source,
                usage: if i == 0 { usage } else { Usage::default() },
                fallback_reason: None,
            };
            let schema = part
                .ctx
                .response_schema
                .or(variant.schema.as_ref())
                .unwrap_or(&Value::Null);
            responses.push(self.finish(
                &validator, bundle, schema, part.op, part.call, &part.ctx, response,
            ));
        }
        Some(responses)
    }

    /// A faker-generated response for `variant`, tagged `Fallback`. The body
    /// is validated against the variant's schema (retrying other branches and
    /// draws) and, for 2xx responses, made consistent with the request and
    /// state exactly like an LLM response would be.
    pub fn fallback(
        &self,
        bundle: &Bundle,
        op: &Operation,
        variant: &OutputVariant,
        status: u16,
        call: &OperationCall,
        ctx: &GenerationContext<'_>,
    ) -> GeneratedResponse {
        let Some(schema) = &variant.schema else {
            return GeneratedResponse {
                status,
                content_type: variant.content_type.clone(),
                body: None,
                source: Source::Fallback,
                usage: Usage::default(),
                fallback_reason: None,
            };
        };
        match self.validator(bundle, op, status, schema) {
            Ok(validator) => {
                let response =
                    self.fake_response(bundle, variant, status, schema, Some(&validator));
                self.finish(&validator, bundle, schema, op, call, ctx, response)
            }
            Err(e) => {
                tracing::warn!(operation = %op.id, error = %e, "fallback schema cannot be validated");
                self.fake_response(bundle, variant, status, schema, None)
            }
        }
    }

    /// `stored` served as `op`'s success answer, tagged `State`, when the
    /// success variant has a schema (the narrowed `response_schema` when
    /// given) and `stored` validates against it. `None` otherwise: a partial
    /// resource stored by a create is generated and overlaid instead.
    pub fn serve_stored(
        &self,
        bundle: &Bundle,
        op: &Operation,
        stored: &Value,
        response_schema: Option<&Value>,
    ) -> Option<GeneratedResponse> {
        let variant = op.success_output()?;
        let status = success_status(variant.status);
        let schema = variant.schema.as_ref()?;
        let validator = match response_schema {
            Some(narrowed) => Arc::new(
                SchemaValidator::new(&bundle.with_reachable_defs(narrowed))
                    .inspect_err(|e| {
                        tracing::warn!(operation = %op.id, error = %e, "narrowed schema cannot be validated");
                    })
                    .ok()?,
            ),
            None => self
                .validator(bundle, op, status, schema)
                .inspect_err(|e| {
                    tracing::warn!(operation = %op.id, error = %e, "response schema cannot be validated");
                })
                .ok()?,
        };
        validator.validate(stored).ok()?;
        Some(GeneratedResponse {
            status,
            content_type: variant.content_type.clone(),
            body: Some(stored.clone()),
            source: Source::State,
            usage: Usage::default(),
            fallback_reason: None,
        })
    }

    /// A `404` for a resource the session does not hold: the operation's
    /// declared `404` response, faker-filled, else a problem+json body with
    /// `detail`. Always tagged `State`.
    pub fn not_found(
        &self,
        bundle: &Bundle,
        op: &Operation,
        call: &OperationCall,
        detail: &str,
    ) -> GeneratedResponse {
        let declared = op.output_for_status(404).map(|variant| {
            let ctx = GenerationContext {
                forced_status: Some(404),
                ..GenerationContext::default()
            };
            self.fallback(bundle, op, variant, 404, call, &ctx)
        });
        let mut response = declared.unwrap_or_else(|| GeneratedResponse {
            status: 404,
            content_type: Some("application/problem+json".into()),
            body: Some(serde_json::json!({
                "type": "about:blank",
                "title": "Not Found",
                "status": 404,
                "detail": detail,
            })),
            source: Source::State,
            usage: Usage::default(),
            fallback_reason: None,
        });
        response.source = Source::State;
        response.fallback_reason = None;
        response
    }

    /// Makes a generated answer consistent with the request, state and
    /// scenario; a list read gets unique item ids.
    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        validator: &SchemaValidator,
        bundle: &Bundle,
        schema: &Value,
        op: &Operation,
        call: &OperationCall,
        ctx: &GenerationContext<'_>,
        response: GeneratedResponse,
    ) -> GeneratedResponse {
        let response = reflect(validator, op, call, ctx, response);
        let response = if matches!(op.kind, OperationKind::Query)
            && ctx.target.is_none()
            && (200..300).contains(&response.status)
        {
            self.unique_list_ids(bundle, schema, op, validator, response)
        } else {
            response
        };
        apply_scenario(validator, op, call, ctx, response)
    }

    /// A list body whose items share an identity gets a fresh value for
    /// each later duplicate, kept only if the whole body still validates.
    fn unique_list_ids(
        &self,
        bundle: &Bundle,
        schema: &Value,
        op: &Operation,
        validator: &SchemaValidator,
        mut response: GeneratedResponse,
    ) -> GeneratedResponse {
        let Some(body) = &response.body else {
            return response;
        };
        let field = consistency::list_identity(bundle, op);
        let Some(items) = consistency::list_items(body) else {
            return response;
        };
        let mut seen = std::collections::HashSet::new();
        let duplicate = items
            .iter()
            .filter_map(|item| item.get(&field))
            .any(|id| !seen.insert(id.to_string()));
        if !duplicate {
            return response;
        }
        let Some(id_schema) = consistency::item_schema(bundle, schema, body)
            .and_then(|item| consistency::property_schema(bundle, &item, &field))
        else {
            return response;
        };
        let mut fixed = body.clone();
        let Some(items) = consistency::list_items_mut(&mut fixed) else {
            return response;
        };
        let mut used: std::collections::HashSet<String> = items
            .iter()
            .filter_map(|item| item.get(&field))
            .map(Value::to_string)
            .collect();
        let mut kept = std::collections::HashSet::new();
        for item in items.iter_mut() {
            let Some(id) = item.get(&field) else {
                continue;
            };
            if kept.insert(id.to_string()) {
                continue;
            }
            let fresh = (0..IDENTITY_DRAWS)
                .map(|_| self.fake_value(bundle, &id_schema, None))
                .find(|v| !v.is_null() && !used.contains(&v.to_string()));
            if let (Some(fresh), Value::Object(obj)) = (fresh, &mut *item) {
                used.insert(fresh.to_string());
                kept.insert(fresh.to_string());
                obj.insert(field.clone(), fresh);
            }
        }
        if validator.validate(&fixed).is_ok() {
            response.body = Some(fixed);
        }
        response
    }

    /// `body` (a create's answer) with its identity field (`op`'s, else
    /// `id`) replaced by a different faker value that keeps it valid
    /// against `op`'s success schema; `None` when none is found.
    pub fn fresh_identity(&self, bundle: &Bundle, op: &Operation, body: &Value) -> Option<Value> {
        let variant = op.success_output()?;
        let schema = variant.schema.as_ref()?;
        let validator = self
            .validator(bundle, op, success_status(variant.status), schema)
            .ok()?;
        let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
        let current = body.get(field)?;
        let id_schema = consistency::property_schema(bundle, schema, field)?;
        (0..IDENTITY_DRAWS).find_map(|_| {
            let fresh = self.fake_value(bundle, &id_schema, None);
            if fresh.is_null() || &fresh == current {
                return None;
            }
            let mut candidate = body.clone();
            candidate.as_object_mut()?.insert(field.to_string(), fresh);
            validator.validate(&candidate).is_ok().then_some(candidate)
        })
    }

    fn fake_response(
        &self,
        bundle: &Bundle,
        variant: &OutputVariant,
        status: u16,
        schema: &Value,
        validator: Option<&SchemaValidator>,
    ) -> GeneratedResponse {
        GeneratedResponse {
            status,
            content_type: variant.content_type.clone(),
            body: Some(self.fake_value(bundle, schema, validator)),
            source: Source::Fallback,
            usage: Usage::default(),
            fallback_reason: None,
        }
    }

    /// A faker value for `schema`, validated by `validator` when given:
    /// other `oneOf`/`anyOf` branches and fresh draws are tried before an
    /// invalid value is accepted.
    pub(crate) fn fake_value(
        &self,
        bundle: &Bundle,
        schema: &Value,
        validator: Option<&SchemaValidator>,
    ) -> Value {
        let schema = bundle.with_reachable_defs(schema);
        let faker = Faker::new(self.config.faker_depth);
        // One cost table for every attempt: computing it walks the schema.
        let costs = Costs::compute(&schema);
        let mut rng = rand::rng();
        let mut first: Option<Value> = None;
        for attempt in 0..FALLBACK_ATTEMPTS {
            let candidate = faker.fake_with_costs(&schema, Some(attempt), &costs, &mut rng);
            match validator {
                Some(v) if v.validate(&candidate).is_err() => {
                    first.get_or_insert(candidate);
                }
                _ => return candidate,
            }
        }
        tracing::warn!("faker output does not validate against the schema");
        first.unwrap_or(Value::Null)
    }

    pub(crate) fn validator(
        &self,
        bundle: &Bundle,
        op: &Operation,
        status: u16,
        schema: &Value,
    ) -> Result<Arc<SchemaValidator>, EngineError> {
        let key = (format!("{}/{}", bundle.source_digest, op.id), status);
        if let Some(v) = self.validators.get(&key) {
            return Ok(Arc::clone(&v));
        }
        let v = Arc::new(SchemaValidator::new(&bundle.with_reachable_defs(schema))?);
        self.validators.insert(key, Arc::clone(&v));
        Ok(v)
    }

    /// Ask the LLM, repairing once, under the guards: the budget and a
    /// generation permit (held for both calls) are checked first, waiting
    /// at most the call timeout. `Err` carries the reason to fall back.
    /// Every completed call's tokens are added to `usage` and charged to the
    /// budget, including calls whose output is ultimately discarded.
    ///
    /// Each provider call runs in an `mb.llm` span, the first output's check
    /// in `mb.validate`, and the repair (only when a repair call is made)
    /// in `mb.repair`.
    async fn generate_with_llm(
        &self,
        provider: &dyn Provider,
        validator: &SchemaValidator,
        mut messages: Vec<ChatMessage>,
        format: &ResponseFormat,
        usage: &mut Usage,
    ) -> Result<(Value, Source), String> {
        // A fallback chain gets every hop's timeout, not just the first's.
        let limit = provider.total_timeout(self.config.timeout);
        let deadline = Instant::now() + limit;
        let _permit = self.admit(deadline).await?;
        let first = self
            .complete(provider, &messages, format, usage, deadline)
            .await?;
        let checked =
            tracing::info_span!("mb.validate").in_scope(|| self.check_output(validator, &first));
        let errors = match checked {
            Checked::Valid(value) => return Ok((value, Source::Llm)),
            Checked::Patched(value) => return Ok((value, Source::Repaired)),
            Checked::Rejected(reason) => return Err(reason),
            Checked::Invalid(errors) => errors,
        };
        if self.budget_exhausted() {
            return Err("token budget exhausted".into());
        }
        tracing::info!(errors = %errors.join("; "), "LLM output invalid; asking for a repair");
        async {
            messages.push(ChatMessage {
                role: Role::Assistant,
                content: first,
            });
            let list: String = errors.iter().map(|e| format!("- {e}\n")).collect();
            messages.push(ChatMessage {
                role: Role::User,
                content: format!(
                    "The JSON above is invalid against the schema. Errors:\n{list}Return only corrected JSON."
                ),
            });
            let second = self
                .complete(provider, &messages, format, usage, Instant::now() + limit)
                .await?;
            let checked = tracing::info_span!("mb.validate")
                .in_scope(|| self.check_output(validator, &second));
            match checked {
                Checked::Valid(value) | Checked::Patched(value) => Ok((value, Source::Repaired)),
                Checked::Rejected(reason) => Err(reason),
                Checked::Invalid(errors) => Err(format!(
                    "repaired output still invalid: {}",
                    errors.join("; ")
                )),
            }
        }
        .instrument(tracing::info_span!("mb.repair"))
        .await
    }

    async fn complete(
        &self,
        provider: &dyn Provider,
        messages: &[ChatMessage],
        format: &ResponseFormat,
        usage: &mut Usage,
        deadline: Instant,
    ) -> Result<String, String> {
        let request = ChatRequest {
            messages: messages.to_vec(),
            response_format: format.clone(),
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            timeout: self.config.timeout,
        };
        let span = llm_span();
        let call = provider.complete(request).instrument(span.clone());
        match tokio::time::timeout_at(deadline, call).await {
            Ok(Ok(response)) => {
                record_usage(&span, response.usage);
                *usage = *usage + response.usage;
                self.spend(response.usage);
                Ok(response.content)
            }
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("LLM request timed out".into()),
        }
    }
}

/// The span of one provider call. The provider records `llm.model` on it
/// (the current span while it runs); the caller records the tokens with
/// [`record_usage`].
pub(crate) fn llm_span() -> tracing::Span {
    tracing::info_span!(
        "mb.llm",
        llm.model = Empty,
        llm.tokens.prompt = Empty,
        llm.tokens.completion = Empty,
    )
}

/// Records a provider call's tokens on its [`llm_span`].
pub(crate) fn record_usage(span: &tracing::Span, usage: Usage) {
    span.record("llm.tokens.prompt", usage.prompt_tokens);
    span.record("llm.tokens.completion", usage.completion_tokens);
}

/// The status a success variant is served with.
/// Faker draws tried for an unused identity value.
const IDENTITY_DRAWS: usize = 8;

fn success_status(status: StatusSelector) -> u16 {
    match status {
        StatusSelector::Code(c) => c,
        StatusSelector::Default => 200,
    }
}

/// A `response_format` name matching `^[a-zA-Z0-9_-]{1,64}$`.
pub(crate) fn format_name(id: &str) -> String {
    let name: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if name.is_empty() {
        "response".into()
    } else {
        name
    }
}

/// Post-process a generated response: reflect request and state, then apply
/// the scenario's `set` patch.

/// For a 2xx object body, apply the scenario's `set` merge patch, re-apply
/// the request body (create/update) and the identity so they still win, and
/// validate. Stored state is not re-applied: `set` beats the target. The body
/// of the scenario's own forced status (an error variant) is patched too, but
/// never reflects the request. An invalid result is discarded and the
/// pre-patch body kept.
fn apply_scenario(
    validator: &SchemaValidator,
    op: &Operation,
    call: &OperationCall,
    ctx: &GenerationContext<'_>,
    response: GeneratedResponse,
) -> GeneratedResponse {
    let Some(scenario) = ctx.scenario else {
        return response;
    };
    let Some(set) = &scenario.set else {
        return response;
    };
    let success = (200..300).contains(&response.status);
    if !success && scenario.status != Some(response.status) {
        return response;
    }
    let Some(body @ Value::Object(_)) = &response.body else {
        return response;
    };
    let patched = GeneratedResponse {
        body: Some(apply_set(body, set)),
        ..response.clone()
    };
    let patched = if success {
        reassert_request(validator, op, call, patched)
    } else {
        patched
    };
    match patched.body.as_ref().map(|b| validator.validate(b)) {
        Some(Ok(())) => patched,
        Some(Err(errors)) => {
            tracing::warn!(
                operation = %op.id,
                scenario = %scenario.name,
                errors = %errors.join("; "),
                "scenario set produces an invalid body; ignoring it"
            );
            response
        }
        None => response,
    }
}

/// Make a 2xx body consistent with the request and state (spec §1.2,
/// criterion 4): queries reflect the stored target, creates reflect the
/// request body, updates reflect the target then the request body, and the
/// identity field always equals the path id. Every merge is validated; an
/// invalid one keeps the best valid value.
fn reflect(
    validator: &SchemaValidator,
    op: &Operation,
    call: &OperationCall,
    ctx: &GenerationContext<'_>,
    mut response: GeneratedResponse,
) -> GeneratedResponse {
    if !(200..300).contains(&response.status) {
        return response;
    }
    let Some(body) = response.body.take() else {
        return response;
    };
    let body = match op.kind {
        OperationKind::Query => match ctx.target {
            Some(target) => overlay_target(validator, target, body),
            None => body,
        },
        OperationKind::Command(Effect::Create) => overlay(validator, body, call.body.as_ref()),
        OperationKind::Command(Effect::Update) => {
            let body = overlay(validator, body, ctx.target.map(|t| &t.data));
            overlay(validator, body, call.body.as_ref())
        }
        OperationKind::Command(_) => body,
        // Streams have no stored target to reflect.
        OperationKind::StreamServer | OperationKind::StreamClient | OperationKind::StreamBidi => {
            body
        }
    };
    response.body = Some(force_identity(validator, op, call, body));
    response
}

/// After a scenario `set`: re-apply only the request-body overlay (creates
/// and updates) and the path identity, never the stored target.
fn reassert_request(
    validator: &SchemaValidator,
    op: &Operation,
    call: &OperationCall,
    mut response: GeneratedResponse,
) -> GeneratedResponse {
    let Some(body) = response.body.take() else {
        return response;
    };
    let body = match op.kind {
        OperationKind::Command(Effect::Create | Effect::Update) => {
            overlay(validator, body, call.body.as_ref())
        }
        _ => body,
    };
    response.body = Some(force_identity(validator, op, call, body));
    response
}

/// Overlay `fields`' top-level members onto an object `base`: all at once if
/// that validates, else each member that keeps the object valid.
fn overlay(validator: &SchemaValidator, base: Value, fields: Option<&Value>) -> Value {
    let (Value::Object(base_obj), Some(Value::Object(fields))) = (&base, fields) else {
        return base;
    };
    let mut merged = base_obj.clone();
    for (k, v) in fields {
        merged.insert(k.clone(), v.clone());
    }
    let merged = Value::Object(merged);
    if validator.validate(&merged).is_ok() {
        return merged;
    }
    let mut best = base;
    for (k, v) in fields {
        let mut candidate = best.clone();
        if let Value::Object(obj) = &mut candidate {
            obj.insert(k.clone(), v.clone());
        }
        if validator.validate(&candidate).is_ok() {
            best = candidate;
        }
    }
    best
}

/// Set `body[identity.field]` to the path id (else the request body's
/// `identity.input_field`), as a string or (when the schema wants one) a
/// number.
fn force_identity(
    validator: &SchemaValidator,
    op: &Operation,
    call: &OperationCall,
    body: Value,
) -> Value {
    let Some(identity) = &op.identity else {
        return body;
    };
    let from_path = identity
        .path_param
        .as_ref()
        .and_then(|p| call.path_params.get(p));
    let from_body = || {
        let field = identity.input_field.as_ref()?;
        call.body.as_ref()?.get(field)
    };
    let Some(id) = from_path.or_else(from_body) else {
        return body;
    };
    if !body.is_object() || body.get(&identity.field) == Some(id) {
        return body;
    }
    let mut candidates = vec![id.clone()];
    match id {
        Value::String(s) => {
            if let Ok(n) = s.parse::<i64>() {
                candidates.push(Value::from(n));
            } else if let Some(n) = s.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
                candidates.push(Value::Number(n));
            }
        }
        Value::Number(n) => candidates.push(Value::String(n.to_string())),
        _ => {}
    }
    for candidate in candidates {
        let mut forced = body.clone();
        if let Value::Object(obj) = &mut forced {
            obj.insert(identity.field.clone(), candidate);
        }
        if validator.validate(&forced).is_ok() {
            return forced;
        }
    }
    body
}

/// Overlay the stored resource's top-level fields onto a generated object.
/// If the merge is invalid, prefer the stored data when it validates, else
/// keep the generated body.
fn overlay_target(validator: &SchemaValidator, target: &StoredResource, generated: Value) -> Value {
    let (Value::Object(mut merged), Value::Object(stored)) = (generated.clone(), &target.data)
    else {
        return generated;
    };
    for (k, v) in stored {
        merged.insert(k.clone(), v.clone());
    }
    let merged = Value::Object(merged);
    if validator.validate(&merged).is_ok() {
        merged
    } else if validator.validate(&target.data).is_ok() {
        target.data.clone()
    } else {
        generated
    }
}
