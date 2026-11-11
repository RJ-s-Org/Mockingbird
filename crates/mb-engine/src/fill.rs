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

/// Whether [`Engine::fill`] answers `null` for the member `name` (`member`, its schema)
/// with no model call: a nullable object or list that is a reference to another record,
/// or is optional (not in `required`).
fn nulled_by_fill(bundle: &Bundle, required: &[String], name: &str, member: &Value) -> bool {
    admits_null(bundle, member)
        && structured(bundle, member)
        && (reference(bundle, member) || !required.iter().any(|r| r == name))
}

/// The members of `missing` [`Engine::fill`] answers `null` for with no model call (see
/// its doc): what nothing else may fill in its place.
#[must_use]
pub fn fill_nulls(bundle: &Bundle, schema: &Value, missing: &[String]) -> Vec<String> {
    let required = consistency::required_members(bundle, schema);
    missing
        .iter()
        .filter(|m| {
            consistency::property_schema(bundle, schema, m)
                .is_some_and(|member| nulled_by_fill(bundle, &required, m, &member))
        })
        .cloned()
        .collect()
}

impl Engine {
    /// Values for the `missing` members of a record whose `schema` declares them, the
    /// record holding `known` already. A member that may be null and holds a reference
    /// to another record (an id-or-object member) is null, and so is an optional one
    /// (not in `schema`'s `required`) that holds any other object or list; an optional
    /// object or list of objects that may not be null gets no value unless the request
    /// body sent a member of its name (absent too when the model writes it no valid
    /// value). The rest come from one
    /// structured-output LLM call whose schema is `schema` restricted to them, with
    /// `known` in the prompt so that the values agree with it; none is made when none
    /// is left. A member the answer leaves invalid is the faker's (the answer's local
    /// patch), and so is every member when the call fails or there is no provider,
    /// but an object or list that may be null is null; the faker draws by the engine's
    /// seed, the operation and the request. A member `schema` does not declare gets no
    /// value, nor one the faker draws `null` for.
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
        // A nullable object or list that nothing set (not the request, the world
        // description or the executor) is null, not the model's to write, when it is
        // a reference to another record (nothing named a record it could point at) or
        // optional (a fresh payment's `next_action`, a checkout session's dozens of
        // optional settings): null is what the contract allows for it, and writing
        // them out was most of the model's output. A required one (an issue's `user`)
        // stays the model's, as does every scalar; a faker object would tell a client
        // to act on it.
        // An optional object, or list of objects, that may not be null is left out
        // when the request did not send a member of that name: absent is what the
        // contract allows for it, and its schema was most of the prompt (a checkout
        // session's branding). One the request sent stays asked (a checkout session's
        // line items, sent as a plain array while the record holds a list object the
        // executor leaves to fill): the model sees the request and carries the sent
        // items over, and leaving it out would drop what the client sent. A list of
        // scalars (a payment's method types) costs little and clients read it; it
        // stays asked. A world `set` gives a left-out member a client reads.
        let required = consistency::required_members(bundle, schema);
        let mut asked = Vec::new();
        let mut left_out = Vec::new();
        for member in &declared {
            let (name, schema) = member;
            if nulled_by_fill(bundle, &required, name, schema) {
                filled.members.insert((*name).to_string(), Value::Null);
                continue;
            }
            let optional = !required.iter().any(|r| r == name);
            if optional
                && !sent(call, name)
                && structured(bundle, schema)
                && holds_objects(bundle, schema)
            {
                left_out.push(*name);
                continue;
            }
            asked.push(member);
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
            if left_out.contains(name) || filled.members.contains_key(*name) {
                continue;
            }
            // The model wrote no valid value: an object or list that may be null is
            // null, not a faker object a client would act on (a payment's next step).
            if admits_null(bundle, member) && structured(bundle, member) {
                filled.members.insert((*name).to_string(), Value::Null);
                continue;
            }
            // An optional object or list of objects the request sent and the model did
            // not carry over is absent, like one left out: faker items would stand for
            // what the client sent.
            if !required.iter().any(|r| r == name)
                && structured(bundle, member)
                && holds_objects(bundle, member)
            {
                continue;
            }
            let value = self.fake_field(bundle, name, member, &mut draws);
            if !value.is_null() {
                filled.members.insert((*name).to_string(), value);
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

/// Whether `call`'s request body sends a top-level member `name`.
fn sent(call: &OperationCall, name: &str) -> bool {
    call.body
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(|body| body.contains_key(name))
}

/// Whether `schema` (a member's) holds an object or a list of objects (through
/// `$ref`s and unions), not only scalars or lists of them.
fn holds_objects(bundle: &Bundle, schema: &Value) -> bool {
    fn walk(bundle: &Bundle, schema: &Value, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        let schema = bundle.resolve(schema);
        let has_type = |t: &str| match schema.get("type") {
            Some(Value::String(s)) => s == t,
            Some(Value::Array(ts)) => ts.iter().any(|x| x == t),
            _ => false,
        };
        if has_type("object") || schema.get("properties").is_some() {
            return true;
        }
        if let Some(items) = schema.get("items")
            && walk(bundle, items, depth + 1)
        {
            return true;
        }
        ["anyOf", "oneOf", "allOf"].iter().any(|k| {
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
