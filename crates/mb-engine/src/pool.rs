//! [`Engine::pool_records`]: records of one type written ahead of any call, off the
//! request path, for a mock's record pool.

use std::time::Duration;

use mb_llm::{ResponseFormat, Usage};
use mb_mcm::{Bundle, Operation};
use serde_json::{Value, json};

use crate::guards::Checks;
use crate::{Engine, compact, format_name, prompt};

/// What [`Engine::pool_records`] wrote.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pooled {
    /// The records the answer held, each an object; checked against `schema` as one
    /// answer, not yet against the type's other read shapes.
    pub records: Vec<Value>,
    /// What the LLM calls cost.
    pub usage: Usage,
    /// No call was made: no generation slot was spare for it (requests come first).
    /// Try again later.
    pub busy: bool,
}

/// How long one background pool call may take: it waits for no request.
pub const POOL_CALL_TIMEOUT: Duration = Duration::from_secs(120);

impl Engine {
    /// `count` records of `type_name` as `op` answers one (`schema`, the record's
    /// schema), in one structured-output LLM call with the persona, avoiding the
    /// identifiers in `taken`. Nothing without a provider or when the call fails: the
    /// pool is for model-written records, and the faker already answers instantly. The
    /// call takes a generation slot only when another stays free for requests, and
    /// waits for none: [`Pooled::busy`] when there was none to spare.
    pub async fn pool_records(
        &self,
        bundle: &Bundle,
        op: &Operation,
        schema: &Value,
        type_name: &str,
        count: usize,
        taken: &[String],
    ) -> Pooled {
        let mut pooled = Pooled::default();
        let Some(provider) = &self.provider else {
            return pooled;
        };
        let wrapped = json!({
            "type": "object",
            "properties": {"records": {
                "type": "array", "items": schema, "minItems": 1, "maxItems": count
            }},
            "required": ["records"],
        });
        let Ok(validator) = self.schema_validator(bundle, &wrapped) else {
            return pooled;
        };
        let prompt_schema =
            compact::response_prompt_schema(bundle, &wrapped, &std::collections::HashSet::new());
        let messages = prompt::pool_messages(
            bundle,
            op,
            &prompt_schema,
            type_name,
            count,
            taken,
            self.config.persona.as_deref(),
        );
        let format = ResponseFormat::JsonSchema {
            name: format_name(&format!("{}_pool", op.id)),
            schema: prompt_schema,
        };
        let checks = Checks::default();
        match self
            .generate_with_llm_in(
                provider.as_ref(),
                &validator,
                messages,
                &format,
                &checks,
                &mut pooled.usage,
                // A fallback chain gets every hop's timeout.
                provider.total_timeout(POOL_CALL_TIMEOUT),
                true,
            )
            .await
        {
            Ok(answer) => {
                if let Value::Object(mut answered) = answer.value
                    && let Some(Value::Array(records)) = answered.remove("records")
                {
                    pooled.records = records.into_iter().filter(Value::is_object).collect();
                }
            }
            Err(reason) if reason == crate::guards::BUSY => pooled.busy = true,
            Err(reason) => {
                tracing::warn!(operation = %op.id, type_name, %reason, "record pool call failed");
            }
        }
        pooled
    }
}
