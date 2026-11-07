//! [`Engine::fill`]: values for the members of a stored record that nothing else
//! decides, the one LLM call of the state-first executor's creates and replaces.

use mb_llm::{ResponseFormat, Usage};
use mb_mcm::{Bundle, Operation};
use serde_json::{Map, Value, json};

use crate::guards::{Checks, Echoes};
use crate::{Engine, OperationCall, compact, consistency, format_name, prompt, seed};

/// The call a record is filled for: the operation that writes it and its request.
#[derive(Clone, Copy, Debug)]
pub struct FillContext<'a> {
    /// The operation writing the record.
    pub op: &'a Operation,
    /// The call it answers.
    pub call: &'a OperationCall,
}

/// What [`Engine::fill`] wrote.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filled {
    /// A value for each member asked for that `schema` declares.
    pub members: Map<String, Value>,
    /// What the LLM call cost; nothing offline.
    pub usage: Usage,
}

impl Engine {
    /// Values for the `missing` members of a record whose `schema` declares them, the
    /// record holding `known` already: one structured-output LLM call whose schema is
    /// `schema` restricted to those members, with `known` in the prompt so that the
    /// values agree with it. A member the answer leaves invalid is the faker's (the
    /// answer's local patch), and so is every member when the call fails or there is
    /// no provider; the faker draws by the engine's seed, the operation and the request.
    /// A member `schema` does not declare gets no value, nor one the faker draws `null`
    /// for.
    pub async fn fill(
        &self,
        bundle: &Bundle,
        schema: &Value,
        known: &Map<String, Value>,
        missing: &[String],
        ctx: FillContext<'_>,
    ) -> Filled {
        let FillContext { op, call } = ctx;
        let declared: Vec<(&str, Value)> = missing
            .iter()
            .filter_map(|m| Some((m.as_str(), consistency::property_schema(bundle, schema, m)?)))
            .collect();
        let mut filled = Filled::default();
        if declared.is_empty() {
            return filled;
        }
        let properties: Map<String, Value> = declared
            .iter()
            .map(|(name, schema)| ((*name).to_string(), schema.clone()))
            .collect();
        let names: Vec<&str> = declared.iter().map(|(name, _)| *name).collect();
        let restricted = json!({"type": "object", "properties": properties, "required": names});
        if let (Some(provider), Ok(validator)) =
            (&self.provider, self.schema_validator(bundle, &restricted))
        {
            let prompt_schema = compact::response_prompt_schema(bundle, &restricted);
            let messages = prompt::fill_messages(
                bundle,
                op,
                &prompt_schema,
                call,
                known,
                self.config.persona.as_deref(),
            );
            let format = ResponseFormat::JsonSchema {
                name: format_name(&op.id),
                schema: prompt_schema,
            };
            let checks = Checks {
                echoes: Echoes::of([call]),
                seed: self.repair_seed(bundle, &op.id, [call]),
                ..Checks::default()
            };
            match self
                .generate_with_llm(
                    provider.as_ref(),
                    &validator,
                    messages,
                    &format,
                    &checks,
                    &mut filled.usage,
                )
                .await
            {
                Ok(answer) => {
                    if let Value::Object(mut answered) = answer.value {
                        for name in &names {
                            if let Some(value) = answered.remove(*name) {
                                filled.members.insert((*name).to_string(), value);
                            }
                        }
                    }
                }
                Err(reason) => {
                    tracing::warn!(operation = %op.id, %reason, "fill fell back to the faker");
                }
            }
        }
        let [path, query, body] = seed::call_parts(call);
        let mut draws = seed::Draws::new(
            seed::request_seed(call).or(self.faker_seed),
            &[&bundle.source_digest, &op.id, "fill"],
            &[&path, &query, &body],
        );
        for (name, member) in &declared {
            if !filled.members.contains_key(*name) {
                let value = self.fake_field(bundle, name, member, &mut draws);
                if !value.is_null() {
                    filled.members.insert((*name).to_string(), value);
                }
            }
        }
        filled
    }
}
