//! Mock response engine: prompt building, LLM generation with
//! validate → repair → fallback, schema validation and schema-driven faking.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use mb_llm::{ChatMessage, ChatRequest, Provider, ResponseFormat, Role};
use mb_mcm::{Bundle, Effect, Operation, OperationKind, OutputVariant, StatusSelector};
use mb_state::StoredResource;
use serde_json::{Map, Value};

pub mod extract;
pub mod faker;
pub mod prompt;
pub mod validate;

pub use extract::extract_json;
pub use faker::Faker;
pub use prompt::build_messages;
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

/// State the response must stay consistent with.
#[derive(Clone, Copy, Debug, Default)]
pub struct GenerationContext<'a> {
    pub related: &'a [StoredResource],
    /// Existing resource for query/update/delete.
    pub target: Option<&'a StoredResource>,
    pub forced_status: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Llm,
    Repaired,
    Fallback,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Llm => "llm",
            Source::Repaired => "repaired",
            Source::Fallback => "fallback",
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
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub timeout: Duration,
    pub temperature: f32,
    pub max_tokens: u32,
    pub persona: Option<String>,
    pub faker_depth: usize,
}

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
}

impl Engine {
    pub fn new(provider: Option<Arc<dyn Provider>>, config: EngineConfig) -> Self {
        Self {
            provider,
            config,
            validators: DashMap::new(),
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
                let status = match variant.status {
                    StatusSelector::Code(c) => c,
                    StatusSelector::Default => 200,
                };
                (variant, status)
            }
        };

        let Some(schema) = &variant.schema else {
            return Ok(GeneratedResponse {
                status,
                content_type: variant.content_type.clone(),
                body: None,
                source: Source::Llm,
            });
        };

        let validator = self.validator(bundle, op, status, schema)?;
        let response = match &self.provider {
            None => self.fake_response(bundle, variant, status, schema, Some(&validator)),
            Some(provider) => {
                let messages = build_messages(
                    bundle,
                    op,
                    status,
                    schema,
                    call,
                    &ctx,
                    self.config.persona.as_deref(),
                );
                let format = ResponseFormat::JsonSchema {
                    name: format_name(&op.id),
                    schema: bundle.with_reachable_defs(schema),
                };
                match self
                    .generate_with_llm(provider.as_ref(), &validator, messages, &format)
                    .await
                {
                    Ok((body, source)) => GeneratedResponse {
                        status,
                        content_type: variant.content_type.clone(),
                        body: Some(body),
                        source,
                    },
                    Err(reason) => {
                        tracing::warn!(operation = %op.id, %reason, "LLM generation fell back to faker");
                        self.fake_response(bundle, variant, status, schema, Some(&validator))
                    }
                }
            }
        };
        Ok(reflect(&validator, op, call, &ctx, response))
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
            };
        };
        match self.validator(bundle, op, status, schema) {
            Ok(validator) => {
                let response =
                    self.fake_response(bundle, variant, status, schema, Some(&validator));
                reflect(&validator, op, call, ctx, response)
            }
            Err(e) => {
                tracing::warn!(operation = %op.id, error = %e, "fallback schema cannot be validated");
                self.fake_response(bundle, variant, status, schema, None)
            }
        }
    }

    fn fake_response(
        &self,
        bundle: &Bundle,
        variant: &OutputVariant,
        status: u16,
        schema: &Value,
        validator: Option<&SchemaValidator>,
    ) -> GeneratedResponse {
        let schema = bundle.with_reachable_defs(schema);
        let faker = Faker::new(self.config.faker_depth);
        let mut rng = rand::rng();
        let mut first: Option<Value> = None;
        let mut body = None;
        for attempt in 0..FALLBACK_ATTEMPTS {
            let candidate = faker.fake_with_branch(&schema, Some(attempt), &mut rng);
            match validator {
                Some(v) if v.validate(&candidate).is_err() => {
                    first.get_or_insert(candidate);
                }
                _ => {
                    body = Some(candidate);
                    break;
                }
            }
        }
        if body.is_none() {
            tracing::warn!(status, "faker output does not validate against the schema");
            body = first;
        }
        GeneratedResponse {
            status,
            content_type: variant.content_type.clone(),
            body,
            source: Source::Fallback,
        }
    }

    fn validator(
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

    /// Ask the LLM, repairing once. `Err` carries the reason to fall back.
    async fn generate_with_llm(
        &self,
        provider: &dyn Provider,
        validator: &SchemaValidator,
        mut messages: Vec<ChatMessage>,
        format: &ResponseFormat,
    ) -> Result<(Value, Source), String> {
        let first = self.complete(provider, &messages, format).await?;
        let errors = match check(validator, &first) {
            Ok(value) => return Ok((value, Source::Llm)),
            Err(errors) => errors,
        };

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
        let second = self.complete(provider, &messages, format).await?;
        match check(validator, &second) {
            Ok(value) => Ok((value, Source::Repaired)),
            Err(errors) => Err(format!(
                "repaired output still invalid: {}",
                errors.join("; ")
            )),
        }
    }

    async fn complete(
        &self,
        provider: &dyn Provider,
        messages: &[ChatMessage],
        format: &ResponseFormat,
    ) -> Result<String, String> {
        let request = ChatRequest {
            messages: messages.to_vec(),
            response_format: format.clone(),
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            timeout: self.config.timeout,
        };
        match tokio::time::timeout(self.config.timeout, provider.complete(request)).await {
            Ok(Ok(response)) => Ok(response.content),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("LLM request timed out".into()),
        }
    }
}

/// Extract and validate LLM output.
fn check(validator: &SchemaValidator, text: &str) -> Result<Value, Vec<String>> {
    let value = extract_json(text).ok_or_else(|| vec!["output is not valid JSON".to_string()])?;
    validator.validate(&value)?;
    Ok(value)
}

/// A `response_format` name matching `^[a-zA-Z0-9_-]{1,64}$`.
fn format_name(id: &str) -> String {
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

/// Set `body[identity.field]` to the path id, as a string or (when the
/// schema wants one) a number.
fn force_identity(
    validator: &SchemaValidator,
    op: &Operation,
    call: &OperationCall,
    body: Value,
) -> Value {
    let Some(identity) = &op.identity else {
        return body;
    };
    let Some(id) = identity
        .path_param
        .as_ref()
        .and_then(|p| call.path_params.get(p))
    else {
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
