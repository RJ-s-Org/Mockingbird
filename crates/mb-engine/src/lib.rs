//! Mock response engine: prompt building, LLM generation with
//! validate → repair → fallback, schema validation and schema-driven faking.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use mb_llm::{ChatMessage, ChatRequest, Provider, ResponseFormat, Role};
use mb_mcm::{Bundle, Operation, OperationKind, OutputVariant, StatusSelector};
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
        let mut response = match &self.provider {
            None => self.fallback(bundle, variant, status),
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
                    name: op.id.clone(),
                    schema: bundle.with_defs(schema),
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
                        self.fallback(bundle, variant, status)
                    }
                }
            }
        };

        if matches!(op.kind, OperationKind::Query)
            && (200..300).contains(&status)
            && let Some(target) = ctx.target
            && let Some(body) = response.body.take()
        {
            response.body = Some(overlay_target(&validator, target, body));
        }
        Ok(response)
    }

    /// A faker-generated response for `variant`, tagged `Fallback`.
    pub fn fallback(
        &self,
        bundle: &Bundle,
        variant: &OutputVariant,
        status: u16,
    ) -> GeneratedResponse {
        let body = variant.schema.as_ref().map(|schema| {
            let schema = bundle.with_defs(schema);
            Faker::new(self.config.faker_depth).fake(&schema, &mut rand::rng())
        });
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
        let v = Arc::new(SchemaValidator::new(&bundle.with_defs(schema))?);
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
