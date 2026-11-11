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
    /// record holding `known` already. A member that may be null and holds a reference
    /// to another record (an id-or-object member) is null; one that may be null and
    /// holds any other object or list is the faker's. The rest come from one
    /// structured-output LLM call whose schema is `schema` restricted to them, with
    /// `known` in the prompt so that the values agree with it; none is made when none
    /// is left. A member the answer leaves invalid is the faker's (the answer's local
    /// patch), and so is every member when the call fails or there is no provider; the
    /// faker draws by the engine's seed, the operation and the request. A member
    /// `schema` does not declare gets no value, nor one the faker draws `null` for
    /// unless it may be null.
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
        // What a nullable member that nothing set (not the request, the world
        // description or the executor) holds is not the model's to write: a
        // reference to another record (an id-or-object member) is null, as nothing
        // named a record it could point at, and any other object or list is the
        // faker's (valid, and at once). Writing them out was most of the model's
        // output, and their schemas most of its prompt. Scalars (a client secret, a
        // description) are still the model's.
        let (local, asked): (Vec<_>, Vec<_>) = declared
            .iter()
            .partition(|(_, schema)| admits_null(bundle, schema) && structured(bundle, schema));
        for (name, schema) in &local {
            if reference(bundle, schema) {
                filled.members.insert((*name).to_string(), Value::Null);
            }
        }
        if !asked.is_empty() {
            self.fill_with_llm(bundle, op, call, known, &asked, &mut filled)
                .await;
        }
        let [path, query, body] = seed::call_parts(call);
        let mut draws = seed::Draws::new(
            seed::request_seed(call).or(self.faker_seed),
            &[&bundle.source_digest, &op.id, "fill"],
            &[&path, &query, &body],
        );
        for (name, member) in &declared {
            if !filled.members.contains_key(*name) {
                // A nullable object or list left to the faker is drawn without its
                // null: present, as an author or a status object a client reads.
                let local = local.iter().any(|(n, _)| n == name);
                let member = if local {
                    without_null(bundle.resolve(member))
                } else {
                    member.clone()
                };
                let value = self.fake_field(bundle, name, &member, &mut draws);
                if !value.is_null() {
                    filled.members.insert((*name).to_string(), value);
                }
            }
        }
        filled
    }

    /// The model's values for the `asked` members, into `filled`; nothing when the
    /// call fails or there is no provider (the faker fills them after).
    async fn fill_with_llm(
        &self,
        bundle: &Bundle,
        op: &Operation,
        call: &OperationCall,
        known: &Map<String, Value>,
        asked: &[&(&str, Value)],
        filled: &mut Filled,
    ) {
        let properties: Map<String, Value> = asked
            .iter()
            .map(|(name, schema)| ((*name).to_string(), schema.clone()))
            .collect();
        let names: Vec<&str> = asked.iter().map(|(name, _)| *name).collect();
        let restricted = json!({"type": "object", "properties": properties, "required": names});
        if let (Some(provider), Ok(validator)) =
            (&self.provider, self.schema_validator(bundle, &restricted))
        {
            let prompt_schema = compact::response_prompt_schema(
                bundle,
                &restricted,
                &compact::expanded_members(call),
            );
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
    }
}

/// Whether `schema` (a member's, through one `$ref`) admits `null`: a `null`
/// type, `nullable: true`, or a `null` branch of an `anyOf`/`oneOf`.
fn admits_null(bundle: &Bundle, schema: &Value) -> bool {
    fn direct(schema: &Value) -> bool {
        let null_type = match schema.get("type") {
            Some(Value::String(t)) => t == "null",
            Some(Value::Array(ts)) => ts.iter().any(|t| t == "null"),
            _ => false,
        };
        null_type
            || schema.get("nullable") == Some(&Value::Bool(true))
            || ["anyOf", "oneOf"].iter().any(|k| {
                schema
                    .get(*k)
                    .and_then(Value::as_array)
                    .is_some_and(|bs| bs.iter().any(direct))
            })
    }
    direct(schema) || direct(bundle.resolve(schema))
}

/// Whether `schema` (a member's) holds an object, an array, or a reference to
/// another record (a union with an object branch, such as an id-or-object member):
/// anything but a plain scalar, `null` branches aside.
fn structured(bundle: &Bundle, schema: &Value) -> bool {
    fn walk(bundle: &Bundle, schema: &Value, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        let schema = bundle.resolve(schema);
        let typed = |t: &str| t == "object" || t == "array";
        let by_type = match schema.get("type") {
            Some(Value::String(t)) => typed(t),
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).any(typed),
            _ => false,
        };
        by_type
            || schema.get("properties").is_some()
            || schema.get("items").is_some()
            || ["anyOf", "oneOf", "allOf"].iter().any(|k| {
                schema
                    .get(*k)
                    .and_then(Value::as_array)
                    .is_some_and(|bs| bs.iter().any(|b| walk(bundle, b, depth + 1)))
            })
    }
    walk(bundle, schema, 0)
}

/// Whether `schema` (a member's) names another record by id or holds it whole:
/// a union of a plain string and an object branch, `null` branches aside.
fn reference(bundle: &Bundle, schema: &Value) -> bool {
    fn walk(bundle: &Bundle, schema: &Value, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        let schema = bundle.resolve(schema);
        ["anyOf", "oneOf"].iter().any(|k| {
            let Some(branches) = schema.get(*k).and_then(Value::as_array) else {
                return false;
            };
            let string = |b: &Value| {
                let b = bundle.resolve(b);
                b.get("type").and_then(Value::as_str) == Some("string")
                    && b.get("enum").is_none()
                    && b.get("const").is_none()
            };
            (branches.iter().any(string) && branches.iter().any(|b| structured(bundle, b)))
                || branches.iter().any(|b| walk(bundle, b, depth + 1))
        })
    }
    walk(bundle, schema, 0)
}

/// `schema` with its `null` left out: the `null` type, `nullable`, and the `null`
/// branches of an `anyOf`/`oneOf` (a union left with one branch becomes it).
fn without_null(schema: &Value) -> Value {
    let mut schema = schema.clone();
    let Value::Object(obj) = &mut schema else {
        return schema;
    };
    obj.remove("nullable");
    if let Some(Value::Array(types)) = obj.get_mut("type") {
        types.retain(|t| t != "null");
        if let [only] = types.as_slice() {
            let only = only.clone();
            obj.insert("type".into(), only);
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(branches)) = obj.get_mut(key) {
            branches.retain(|b| b.get("type").and_then(Value::as_str) != Some("null"));
            if let [only] = branches.as_slice() {
                let only = only.clone();
                obj.remove(key);
                if obj.is_empty() {
                    return only;
                }
                return json!({"allOf": [Value::Object(obj.clone()), only]});
            }
        }
    }
    schema
}
