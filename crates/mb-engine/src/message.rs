//! Event message generation and validation for AsyncAPI channels: the unary
//! validate → repair → fallback path with a message prompt.

use std::sync::Arc;

use mb_llm::{ResponseFormat, Usage};
use mb_mcm::{Bundle, Channel};
use mb_state::StoredResource;
use serde_json::Value;

use crate::prompt::build_message_messages;
use crate::{Engine, EngineError, SchemaValidator, Source, format_name};

/// What a generated message must stay consistent with.
#[derive(Clone, Debug, Default)]
pub struct MessageContext {
    /// What fired the generation, e.g. `{"kind":"schedule"}` or
    /// `{"kind":"on_state_change","resource_type":"orders","id":"42","change":"upserted"}`.
    pub trigger: Value,
    /// Context supplied with a manual publish.
    pub context: Option<Value>,
    /// The consumed message being replied to.
    pub received: Option<Value>,
    pub related: Vec<StoredResource>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GeneratedMessage {
    pub payload: Value,
    pub source: Source,
    /// Tokens spent, summed over the primary and repair calls; zero when no
    /// LLM call was made.
    pub usage: Usage,
}

impl Engine {
    /// Generate one message for `channel`: the LLM (repaired once), else the
    /// faker.
    pub async fn generate_message(
        &self,
        bundle: &Bundle,
        channel: &Channel,
        ctx: &MessageContext,
    ) -> Result<GeneratedMessage, EngineError> {
        let validator = self.channel_validator(bundle, channel)?;
        let schema = &channel.message_schema;
        let Some(provider) = &self.provider else {
            return Ok(GeneratedMessage {
                payload: self.fake_value(
                    bundle,
                    schema,
                    Some(&validator),
                    &crate::FakerHints::default(),
                ),
                source: Source::Fallback,
                usage: Usage::default(),
            });
        };
        let messages = build_message_messages(bundle, channel, ctx, self.config.persona.as_deref());
        let format = ResponseFormat::JsonSchema {
            name: format_name(&channel.id),
            schema: crate::compact::prompt_schema(bundle, schema),
        };
        let mut usage = Usage::default();
        let (payload, source) = match self
            .generate_with_llm(provider.as_ref(), &validator, messages, &format, &mut usage)
            .await
        {
            Ok(generated) => generated,
            Err(reason) => {
                tracing::warn!(channel = %channel.id, %reason, "LLM message generation fell back to faker");
                (
                    self.fake_value(
                        bundle,
                        schema,
                        Some(&validator),
                        &crate::FakerHints::default(),
                    ),
                    Source::Fallback,
                )
            }
        };
        Ok(GeneratedMessage {
            payload,
            source,
            usage,
        })
    }

    /// Validate `value` against `channel`'s message schema. A schema that
    /// cannot compile is reported as a single error.
    pub fn validate_message(
        &self,
        bundle: &Bundle,
        channel: &Channel,
        value: &Value,
    ) -> Result<(), Vec<String>> {
        let validator = self
            .channel_validator(bundle, channel)
            .map_err(|e| vec![e.to_string()])?;
        validator.validate(value)
    }

    /// A faker value for `schema`, validated against it: other
    /// `oneOf`/`anyOf` branches and fresh draws are tried before an invalid
    /// value is accepted. An uncompilable schema is faked unvalidated.
    pub fn fake(&self, bundle: &Bundle, schema: &Value) -> Value {
        match SchemaValidator::new(&bundle.with_reachable_defs(schema)) {
            Ok(validator) => self.fake_value(
                bundle,
                schema,
                Some(&validator),
                &crate::FakerHints::default(),
            ),
            Err(e) => {
                tracing::warn!(error = %e, "schema cannot be validated; faking unvalidated");
                self.fake_value(bundle, schema, None, &crate::FakerHints::default())
            }
        }
    }

    /// The cached validator of `channel`'s message schema, keyed by
    /// (`<source digest>/channel:<id>`, 0).
    fn channel_validator(
        &self,
        bundle: &Bundle,
        channel: &Channel,
    ) -> Result<Arc<SchemaValidator>, EngineError> {
        let key = (
            format!("{}/channel:{}", bundle.source_digest, channel.id),
            0,
        );
        if let Some(v) = self.validators.get(&key) {
            return Ok(Arc::clone(&v));
        }
        let v = Arc::new(SchemaValidator::new(
            &bundle.with_reachable_defs(&channel.message_schema),
        )?);
        self.validators.insert(key, Arc::clone(&v));
        Ok(v)
    }
}
