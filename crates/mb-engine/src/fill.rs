//! [`Engine::fill`]: values for the members of a stored record that nothing else
//! decides, the one LLM call of the state-first executor's creates and replaces.

use std::borrow::Cow;

use mb_llm::{ResponseFormat, Usage};
use mb_mcm::{Bundle, Operation};
use serde_json::{Map, Value, json};

use crate::guards::{Checks, Echoes};
use crate::{Engine, FakerHints, OperationCall, compact, consistency, format_name, prompt, seed};

/// The call a record is filled for: the operation that writes it and its request.
#[derive(Clone, Copy, Debug)]
pub struct FillContext<'a> {
    /// The operation writing the record.
    pub op: &'a Operation,
    /// The call it answers.
    pub call: &'a OperationCall,
    /// The members the request sent for the record, where the executor read them
    /// from somewhere other than the body's top level (inside an `input` or
    /// resource-named wrapper); `None` reads the body's top level.
    pub sent: Option<&'a Map<String, Value>>,
    /// What says who the caller is: what the session's routes that name it (a
    /// `/user`, a `/users/@me`) answered, for the objects of the record of the same
    /// schema (an issue's `user`).
    pub caller: &'a [CallerAnswer],
    /// The top-level enum members that are the record's state (a lifecycle member, one
    /// a world rule moves between values) whose start nothing gave: the model picks
    /// each, never the persona's single named value or the first listed one; its
    /// `default`, else its first value, only when the model writes none.
    pub states: &'a [String],
}

/// What a route that says who the caller is answered ([`FillContext::caller`]).
#[derive(Clone, Debug, PartialEq)]
pub struct CallerAnswer {
    /// The schema of its answer.
    pub schema: Value,
    /// Its scalar members.
    pub members: Map<String, Value>,
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
    /// value). Of the rest, those holding free text ([`free_text`]) and the objects
    /// and lists the request sent a member of the name of (the model carries the sent
    /// items over) come from one structured-output LLM call whose schema is `schema`
    /// restricted to them, with `known` in the prompt so that the values agree with
    /// it, an object asked for its free-text members alone (the faker writing the rest
    /// of it, and nothing the model wrote unasked kept); none is made when none is
    /// left. A list of objects the request sent nothing for is empty (where the schema
    /// allows). Of what is left, a request value of the same name, letter case and
    /// separators aside, is echoed where it fits (`To` into `to`); a scalar that may be null is null
    /// and a boolean false, as on a fresh record; an enum is the one value the persona
    /// names, the model's when the persona names it otherwise, else its `default`, else
    /// its first value (`enum_choice`), but one of [`FillContext::states`] is the
    /// model's; and every other member (ids, numbers,
    /// formatted, patterned or timestamp strings, objects of nothing else) is the
    /// faker's, with no model call. Inside an object the faker draws, a member the
    /// call's context gives (`Context::nested`: the request at its path; inside an
    /// object of the schema of a [`FillContext::caller`] answer, its member of that
    /// name; right below the top, a path or query parameter of its name) is that value,
    /// where the object still validates; such a string is no free text to ask for.
    /// A member the answer leaves invalid is the faker's too (the answer's local
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
        let FillContext {
            op,
            call,
            sent,
            caller,
            states,
        } = ctx;
        let declared: Vec<(&str, Value)> = missing
            .iter()
            .filter_map(|m| Some((m.as_str(), consistency::property_schema(bundle, schema, m)?)))
            .collect();
        let mut filled = Filled::default();
        if declared.is_empty() {
            return filled;
        }
        // The request's members: what the executor read the record from (inside a
        // wrapper), else the body's top level.
        let body = call.body.as_ref().and_then(Value::as_object);
        let sent = sent.or(body);
        let was_sent = |name: &str| sent.is_some_and(|s| s.contains_key(name));
        let context = Context {
            call,
            sent,
            body,
            callers: caller_objects(bundle, &declared, caller),
        };
        // A string the context gives is no free text to ask for (and is set after).
        let resolved = |path: &str| context.nested(path).is_some_and(Value::is_string);
        // A nullable object or list that nothing set (not the request, the world
        // description or the executor) is null, not the model's to write, when it is
        // a reference to another record (nothing named a record it could point at) or
        // optional (a fresh payment's `next_action`, a checkout session's dozens of
        // optional settings): null is what the contract allows for it, and writing
        // them out was most of the model's output. A required one (an issue's `user`)
        // stays the model's when it holds free text; a faker object would tell a
        // client to act on it.
        // An optional object, or list of objects, that may not be null is left out
        // when the request did not send a member of that name: absent is what the
        // contract allows for it, and its schema was most of the prompt (a checkout
        // session's branding). One the request sent stays asked (a checkout session's
        // line items, sent as a plain array while the record holds a list object the
        // executor leaves to fill): the model sees the request and carries the sent
        // items over, and leaving it out would drop what the client sent. A world `set`
        // gives a left-out member a client reads.
        let required = consistency::required_members(bundle, schema);
        let hints = echoes(call, &[sent, body], declared.iter().map(|(name, _)| *name));
        let persona = self
            .config
            .persona
            .as_deref()
            .unwrap_or_default()
            .to_lowercase();
        let mut asked: Vec<(&str, Value)> = Vec::new();
        let mut pruned: Vec<(&str, Value)> = Vec::new();
        let mut enums: Vec<(&str, Value)> = Vec::new();
        let mut left_out = Vec::new();
        for (name, schema) in &declared {
            let name = *name;
            if nulled_by_fill(bundle, &required, name, schema) {
                filled.members.insert(name.to_string(), Value::Null);
                continue;
            }
            let optional = !required.iter().any(|r| r == name);
            if optional
                && !was_sent(name)
                && structured(bundle, schema)
                && holds_objects(bundle, schema)
            {
                left_out.push(name);
                continue;
            }
            if was_sent(name) && structured(bundle, schema) {
                asked.push((name, schema.clone()));
                continue;
            }
            // A list of objects the request sent nothing for holds none yet.
            if empty_list_fits(bundle, schema) {
                filled
                    .members
                    .insert(name.to_string(), Value::Array(Vec::new()));
                continue;
            }
            // Text the request sent under another spelling of the name (`Body` for
            // `body`) is the faker's echo.
            let echoed = hints
                .echoes
                .get(mb_mcm::leaf_member(name))
                .is_some_and(|value| echo_fits(bundle, schema, value));
            // Free text the request and the call's context (the path, the query, who
            // the caller is) do not give is the model's.
            if free_text_in(bundle, name, schema, &resolved) && !echoed {
                // An object is asked for its free text alone; the faker writes the rest.
                match free_text_schema(bundle, name, schema, &resolved) {
                    Some(only_text) => {
                        pruned.push((name, only_text.clone()));
                        asked.push((name, only_text));
                    }
                    None => asked.push((name, schema.clone())),
                }
                continue;
            }
            if echoed {
                continue;
            }
            // What nothing set of a fresh record: a nullable scalar is null, a
            // boolean false (not a coin's toss of `deleted` or `archived`), and an
            // enum what the persona says of it (`enum_choice`); a state the caller
            // found no start for is the model's.
            if fresh_null(bundle, schema) {
                filled.members.insert(name.to_string(), Value::Null);
            } else if boolean(bundle, schema) {
                filled.members.insert(name.to_string(), Value::Bool(false));
            } else {
                let choice = match states.iter().any(|s| s == name) {
                    true => enum_fallback(bundle, schema).map(|_| EnumChoice::Ask),
                    false => enum_choice(bundle, name, schema, &persona),
                };
                match choice {
                    Some(EnumChoice::Value(value)) => {
                        filled.members.insert(name.to_string(), value);
                    }
                    Some(EnumChoice::Ask) => {
                        asked.push((name, schema.clone()));
                        enums.push((name, schema.clone()));
                    }
                    None => {}
                }
            }
        }
        if !asked.is_empty() {
            let asked: Vec<&(&str, Value)> = asked.iter().collect();
            self.fill_with_llm(bundle, op, call, known, &asked, &mut filled)
                .await;
        }
        // An enum the model wrote no value for is its `default`, else its first value.
        for (name, schema) in &enums {
            if !filled.members.contains_key(*name)
                && let Some(value) = enum_fallback(bundle, schema)
            {
                filled.members.insert((*name).to_string(), value);
            }
        }
        let [path, query, body] = seed::call_parts(call);
        let mut draws = seed::Draws::new(
            seed::request_seed(call).or(self.faker_seed),
            &[&bundle.source_digest, &op.id, "fill"],
            &[&path, &query, &body],
        );
        for (name, member) in &declared {
            // What the model wrote of an object's free text (and nothing it was not
            // asked), the rest the faker's.
            if let Some((_, only_text)) = pruned.iter().find(|(p, _)| p == name)
                && let Some(Value::Object(text)) = filled.members.get(*name)
            {
                let mut text = text.clone();
                keep_asked(&mut text, only_text);
                let member = timestamp_string(bundle, name, member);
                // The rest is drawn as an object (a nullable one's faker draw may be
                // `null`); a draw that is still no object stands whole, as the model's
                // free text alone lacks the rest.
                let mut drawn = self.fake_member(bundle, name, &member, &hints, &mut draws);
                if !drawn.is_object() {
                    let shape = object_shape(bundle, &member);
                    drawn = self.fake_member(bundle, name, &shape, &hints, &mut draws);
                }
                mint_nested_ids(bundle, &member, &mut drawn, &mut draws.rng, 0);
                let value = match drawn {
                    Value::Object(whole) => {
                        lay_under(&mut text, whole, only_text);
                        Value::Object(text)
                    }
                    other => other,
                };
                let value = self.with_context(bundle, name, &member, value, &context);
                filled.members.insert((*name).to_string(), value);
                continue;
            }
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
            let member = timestamp_string(bundle, name, member);
            let mut value = self.fake_member(bundle, name, &member, &hints, &mut draws);
            mint_nested_ids(bundle, &member, &mut value, &mut draws.rng, 0);
            let value = self.with_context(bundle, name, &member, value, &context);
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

/// How deep [`free_text`] looks into a member's objects and lists.
const FREE_TEXT_DEPTH: usize = 8;

/// Whether member `name` of schema `schema` holds free text, the one kind of value
/// worth a model call to fill: a string with no `format`, `pattern`, `enum` or `const`
/// whose name does not say it is an identifier (`id`, `*_id`, `*Id`), a link (`url`,
/// `*_uri`, `href`) or a moment (`created`, `*_at`); or an object, list or union
/// holding one at any depth (a list's items go by the list's name, an object's members
/// by their own). Ids, enums, numbers, booleans and formatted, patterned or timestamp
/// strings are not: the faker writes them as well as a model would.
#[must_use]
pub fn free_text(bundle: &Bundle, name: &str, schema: &Value) -> bool {
    free_text_in(bundle, name, schema, &|_| false)
}

/// [`free_text`] of member `path` (dotted below the record's top), but a string at a
/// path `resolved` says the call's context gives (inside an object; a list's items are
/// never resolved).
fn free_text_in(
    bundle: &Bundle,
    path: &str,
    schema: &Value,
    resolved: &dyn Fn(&str) -> bool,
) -> bool {
    fn walk(
        bundle: &Bundle,
        name: &str,
        schema: &Value,
        depth: usize,
        resolved: &dyn Fn(&str) -> bool,
    ) -> bool {
        if depth > FREE_TEXT_DEPTH {
            return false;
        }
        let Some(s) = bundle.resolve(schema).as_object() else {
            return false;
        };
        if s.contains_key("const") || s.contains_key("enum") {
            return false;
        }
        let never = |_: &str| false;
        // Cheapest first, each part only while none said yes.
        (string_typed(s)
            && !s.contains_key("format")
            && !s.contains_key("pattern")
            && !structured_name(leaf_of(name))
            && !resolved(name))
            || s.get("items")
                .is_some_and(|items| walk(bundle, name, items, depth + 1, &never))
            || s.get("properties")
                .and_then(Value::as_object)
                .is_some_and(|ms| {
                    ms.iter().any(|(member, schema)| {
                        walk(
                            bundle,
                            &format!("{name}.{member}"),
                            schema,
                            depth + 1,
                            resolved,
                        )
                    })
                })
            || ["anyOf", "oneOf", "allOf"].iter().any(|k| {
                s.get(*k).and_then(Value::as_array).is_some_and(|bs| {
                    bs.iter()
                        .any(|b| walk(bundle, name, b, depth + 1, resolved))
                })
            })
    }
    walk(bundle, path, schema, 0, resolved)
}

/// The last segment of a dotted member path.
fn leaf_of(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
}

/// Whether schema `s` declares the `string` type (alone or beside others).
fn string_typed(s: &Map<String, Value>) -> bool {
    match s.get("type") {
        Some(Value::String(t)) => t == "string",
        Some(Value::Array(ts)) => ts.iter().any(|t| t == "string"),
        _ => false,
    }
}

/// Whether a string member's name says what it holds without free text: an
/// identifier, a link or a moment.
fn structured_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let camel = |suffixes: &[&str]| {
        suffixes
            .iter()
            .any(|suffix| name.len() > suffix.len() && name.ends_with(suffix))
    };
    let link_like = ["url", "uri", "href"].iter().any(|word| {
        lower == *word
            || lower.ends_with(&format!("_{word}"))
            || lower.ends_with(&format!("-{word}"))
    }) || camel(&["Url", "URL", "Uri", "URI", "Href"]);
    id_like(name) || link_like || crate::faker::timestamp_field(name)
}

/// Gives each id member below the top of `value`, a faker draw of `schema` (an id
/// [`one_id_name`] names, whose schema is a plain string: no format, pattern, enum or
/// const), an id of 20 letters and digits drawn from `rng` in place of the faker's
/// word, in objects and list items alike.
fn mint_nested_ids(
    bundle: &Bundle,
    schema: &Value,
    value: &mut Value,
    rng: &mut rand::rngs::StdRng,
    depth: usize,
) {
    use rand::RngExt;
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    if depth > FREE_TEXT_DEPTH {
        return;
    }
    match value {
        Value::Object(members) => {
            for (key, inner) in members.iter_mut() {
                let Some(member) = consistency::property_schema(bundle, schema, key) else {
                    continue;
                };
                if inner.is_string() && one_id_name(key) && plain_string(bundle, &member) {
                    let id: String = (0..20)
                        .filter_map(|_| ALPHABET.get(rng.random_range(0..ALPHABET.len())))
                        .map(|&b| char::from(b))
                        .collect();
                    *inner = Value::String(id);
                } else {
                    mint_nested_ids(bundle, &member, inner, rng, depth + 1);
                }
            }
        }
        Value::Array(items) => {
            if let Some(item) = items_schema(bundle, schema) {
                for inner in items {
                    mint_nested_ids(bundle, &item, inner, rng, depth + 1);
                }
            }
        }
        _ => {}
    }
}

/// The item schema of list `schema` (through `$ref`s and a nullable `anyOf`/`oneOf`).
fn items_schema(bundle: &Bundle, schema: &Value) -> Option<Value> {
    let s = bundle.resolve(schema);
    if let Some(items) = s.get("items") {
        return Some(items.clone());
    }
    ["anyOf", "oneOf"]
        .iter()
        .filter_map(|k| s.get(*k).and_then(Value::as_array))
        .flatten()
        .find_map(|branch| bundle.resolve(branch).get("items").cloned())
}

/// Whether `schema` (through `$ref`s and a nullable `anyOf`/`oneOf`) is a string of no
/// declared form: no `format`, `pattern`, `enum` or `const`.
fn plain_string(bundle: &Bundle, schema: &Value) -> bool {
    let s = bundle.resolve(schema);
    let branches: Vec<&Value> = ["anyOf", "oneOf"]
        .iter()
        .filter_map(|k| s.get(*k).and_then(Value::as_array))
        .flatten()
        .map(|b| bundle.resolve(b))
        .filter(|b| b.get("type").and_then(Value::as_str) != Some("null"))
        .collect();
    let s = match branches.as_slice() {
        [] => s,
        [only] => *only,
        _ => return false,
    };
    s.as_object().is_some_and(|o| {
        string_typed(o)
            && !["format", "pattern", "enum", "const"]
                .iter()
                .any(|k| o.contains_key(*k))
    })
}

/// Whether member `name` names one identifier: `id`, `uuid`, `guid`, `*_id`, `*-id`, or
/// a camel-case `*Id` / `*ID` (`nodeId`, `parentID`; not `PAID`).
fn one_id_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["id", "uuid", "guid"].contains(&lower.as_str())
        || ["_id", "-id", "_uuid", "_guid"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
        || ["Id", "ID", "Uuid", "UUID"].iter().any(|suffix| {
            name.strip_suffix(suffix)
                .and_then(|rest| rest.chars().next_back())
                .is_some_and(char::is_lowercase)
        })
}

/// Whether member `name` names an identifier (`id`, `*_id`, `*Id`, `uuid`).
fn id_like(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["id", "ids", "uuid", "guid"].contains(&lower.as_str())
        || ["_id", "-id", "_ids", "-ids", "_uuid", "_guid"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
        || ["Id", "ID", "Ids", "IDs", "Uuid", "UUID"]
            .iter()
            .any(|suffix| name.len() > suffix.len() && name.ends_with(suffix))
}

/// `member` (member `name`'s schema) when it is a plain string the name says holds a
/// moment, with the `date` (a `*date` name) or `date-time` format the faker then draws
/// it in, not a word; any other member as it is.
fn timestamp_string<'a>(bundle: &Bundle, name: &str, member: &'a Value) -> Cow<'a, Value> {
    let leaf = mb_mcm::leaf_member(name);
    let resolved = bundle.resolve(member);
    let plain = resolved.as_object().is_some_and(|s| {
        string_typed(s)
            && ![
                "format", "pattern", "enum", "const", "anyOf", "oneOf", "allOf",
            ]
            .iter()
            .any(|k| s.contains_key(*k))
            // Room for an RFC 3339 date-time (`2026-01-01T00:00:00Z`).
            && s.get("maxLength")
                .and_then(Value::as_u64)
                .is_none_or(|max| max >= 20)
    });
    if !plain || !crate::faker::timestamp_field(leaf) {
        return Cow::Borrowed(member);
    }
    let mut out = resolved.clone();
    if let Value::Object(s) = &mut out {
        let format = if leaf.to_ascii_lowercase().ends_with("date") {
            "date"
        } else {
            "date-time"
        };
        s.insert("format".into(), Value::String(format.into()));
    }
    Cow::Owned(out)
}

/// The request's scalar path and query values, then those of `more` (what it sent,
/// its body), each under the name of the member of `names` it
/// names, letter case and `_`/`-` separators aside (`To` for `to`, `callerName` for
/// `caller_name`), for the faker to echo where it fits.
fn echoes<'a>(
    call: &OperationCall,
    more: &[Option<&Map<String, Value>>],
    names: impl Iterator<Item = &'a str>,
) -> FakerHints {
    let mut sources = vec![Some(&call.path_params), Some(&call.query)];
    sources.extend_from_slice(more);
    let mut echoes = Map::new();
    for name in names {
        let leaf = mb_mcm::leaf_member(name);
        if let Some(value) = by_name(&sources, leaf) {
            echoes.insert(leaf.to_string(), value.clone());
        }
    }
    FakerHints { echoes }
}

/// `name` with letter case and `_`/`-` separators aside (`caller_name`, `callerName`).
fn plain(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// The first scalar of `sources` under a member named `name` ([`plain`]ly).
fn by_name<'v>(sources: &[Option<&'v Map<String, Value>>], name: &str) -> Option<&'v Value> {
    let wanted = plain(name);
    sources.iter().flatten().find_map(|source| {
        source.iter().find_map(|(k, v)| {
            (plain(k) == wanted
                && matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_)))
            .then_some(v)
        })
    })
}

/// Whether request value `value` fits string member schema `schema` as the faker
/// echoes it: a string within its length bounds.
fn echo_fits(bundle: &Bundle, schema: &Value, value: &Value) -> bool {
    let (Some(s), Value::String(text)) = (bundle.resolve(schema).as_object(), value) else {
        return false;
    };
    let n = text.chars().count() as u64;
    let bound = |k: &str| s.get(k).and_then(Value::as_u64);
    string_typed(s)
        && !s.contains_key("enum")
        && !s.contains_key("const")
        && bound("minLength").is_none_or(|m| n >= m)
        && bound("maxLength").is_none_or(|m| n <= m)
}

/// Whether `schema` (a member's) is a list of objects an empty list satisfies (no
/// `minItems` above zero).
fn empty_list_fits(bundle: &Bundle, schema: &Value) -> bool {
    let Some(s) = bundle.resolve(schema).as_object() else {
        return false;
    };
    let list = match s.get("type") {
        Some(Value::String(t)) => t == "array",
        Some(Value::Array(ts)) => ts.iter().any(|t| t == "array"),
        _ => false,
    };
    list && s.get("minItems").and_then(Value::as_u64).unwrap_or(0) == 0
        && s.get("items")
            .is_some_and(|items| holds_objects(bundle, items))
}

/// `schema` (an object member's, through `$ref`s, a one-shape `allOf` and a union
/// with `null`) with only the members holding free text ([`free_text`]), each pruned
/// the same way, and only those of its `required`; `null` stays allowed where it
/// was. `None` when `schema` is no plain object (a list, a union of several shapes):
/// it is asked whole.
fn free_text_schema(
    bundle: &Bundle,
    path: &str,
    schema: &Value,
    resolved: &dyn Fn(&str) -> bool,
) -> Option<Value> {
    fn prune(
        bundle: &Bundle,
        path: &str,
        schema: &Value,
        depth: usize,
        resolved: &dyn Fn(&str) -> bool,
    ) -> Option<Value> {
        if depth > FREE_TEXT_DEPTH {
            return None;
        }
        let s = bundle.resolve(schema).as_object()?;
        let nullable = s.get("nullable") == Some(&Value::Bool(true))
            || matches!(s.get("type"), Some(Value::Array(ts)) if ts.iter().any(|t| t == "null"));
        let null_branch =
            |b: &Value| bundle.resolve(b).get("type").and_then(Value::as_str) == Some("null");
        for key in ["anyOf", "oneOf", "allOf"] {
            if let Some(branches) = s.get(key).and_then(Value::as_array) {
                let shapes: Vec<&Value> = branches.iter().filter(|b| !null_branch(b)).collect();
                let [only] = shapes.as_slice() else {
                    return None;
                };
                if s.contains_key("properties") {
                    return None;
                }
                let inner = prune(bundle, path, only, depth + 1, resolved)?;
                return Some(match nullable || shapes.len() < branches.len() {
                    true => json!({"anyOf": [inner, {"type": "null"}]}),
                    false => inner,
                });
            }
        }
        let members = s.get("properties")?.as_object()?;
        let kept: Map<String, Value> = members
            .iter()
            .filter(|(name, member)| {
                free_text_in(bundle, &format!("{path}.{name}"), member, resolved)
            })
            .map(|(name, member)| {
                let below = format!("{path}.{name}");
                let member = prune(bundle, &below, member, depth + 1, resolved)
                    .unwrap_or_else(|| member.clone());
                (name.clone(), member)
            })
            .collect();
        let required: Vec<Value> = s
            .get("required")
            .and_then(Value::as_array)
            .map(|r| {
                r.iter()
                    .filter(|n| n.as_str().is_some_and(|n| kept.contains_key(n)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let object = json!({"type": "object", "properties": kept, "required": required});
        Some(match nullable {
            true => json!({"anyOf": [object, {"type": "null"}]}),
            false => object,
        })
    }
    prune(bundle, path, schema, 0, resolved)
}

/// The members of `schema` when it is an object [`free_text_schema`] pruned (alone or
/// in a union with `null`); `None` for a member it kept whole.
fn pruned_members(schema: &Value) -> Option<&Map<String, Value>> {
    let s = schema.as_object()?;
    if let Some(branches) = s.get("anyOf").and_then(Value::as_array) {
        let [object, null] = branches.as_slice() else {
            return None;
        };
        if null.get("type").and_then(Value::as_str) != Some("null") {
            return None;
        }
        return pruned_members(object);
    }
    if s.contains_key("oneOf") || s.contains_key("allOf") {
        return None;
    }
    s.get("properties")?.as_object()
}

/// Drops from `text` (the model's answer for a pruned object) every member `only_text`
/// (its pruned schema) did not ask for, at every depth it pruned: what the model was
/// not asked is the faker's.
fn keep_asked(text: &mut Map<String, Value>, only_text: &Value) {
    let Some(members) = pruned_members(only_text) else {
        return;
    };
    text.retain(|name, _| members.contains_key(name));
    for (name, value) in text.iter_mut() {
        if let (Value::Object(inner), Some(schema)) = (value, members.get(name)) {
            keep_asked(inner, schema);
        }
    }
}

/// Lays `whole` (the faker's draw of a pruned object) under `over` (the model's free
/// text of it, its schema `only_text`): each member `over` lacks is set, and a member
/// pruned in turn takes what it lacks the same way. A member kept whole (a union of
/// several shapes) is the model's as written, never mixed with another draw.
fn lay_under(over: &mut Map<String, Value>, whole: Map<String, Value>, only_text: &Value) {
    let members = pruned_members(only_text);
    for (name, value) in whole {
        match (over.get_mut(&name), value) {
            (None, value) => {
                over.insert(name, value);
            }
            (Some(Value::Object(held)), Value::Object(below)) => {
                if let Some(schema) = members.and_then(|m| m.get(&name))
                    && pruned_members(schema).is_some()
                {
                    lay_under(held, below, schema);
                }
            }
            _ => {}
        }
    }
}

/// Whether `schema` (a member's) is a scalar that admits `null`: what a fresh record
/// holds where nothing set it.
fn fresh_null(bundle: &Bundle, schema: &Value) -> bool {
    admits_null(bundle, schema) && !structured(bundle, schema)
}

/// Whether `schema` (a member's, through one `$ref`) is a boolean.
fn boolean(bundle: &Bundle, schema: &Value) -> bool {
    bundle.resolve(schema).get("type").and_then(Value::as_str) == Some("boolean")
}

/// How [`Engine::fill`] settles an enum member nothing set.
enum EnumChoice {
    /// This value.
    Value(Value),
    /// The model's to pick: the persona speaks of it but names no one value.
    Ask,
}

/// The shortest enum value looked for in the persona: shorter ones (`a`, `on`) are
/// words of any text.
const MIN_NAMED_VALUE: usize = 3;

/// How enum member `name` (`schema`, through one `$ref`) is settled, `persona` (lower
/// case) being the mock's persona: the one value it names (`direction "outbound-api"`),
/// the model's pick when it names several or the member itself but none, else the
/// schema's `default`, else its first value. `None` for a member with no `enum`.
fn enum_choice(bundle: &Bundle, name: &str, schema: &Value, persona: &str) -> Option<EnumChoice> {
    let s = bundle.resolve(schema);
    let values: Vec<&Value> = s
        .get("enum")?
        .as_array()?
        .iter()
        .filter(|v| !v.is_null())
        .collect();
    values.first()?;
    let named: Vec<&&Value> = values
        .iter()
        .filter(|v| {
            v.as_str().is_some_and(|text| {
                text.chars().count() >= MIN_NAMED_VALUE && spoken_of(persona, text)
            })
        })
        .collect();
    if let [only] = named.as_slice() {
        return Some(EnumChoice::Value((**only).clone()));
    }
    if named.len() > 1 || spoken_of(persona, leaf_of(name)) {
        return Some(EnumChoice::Ask);
    }
    enum_fallback(bundle, schema).map(EnumChoice::Value)
}

/// An enum member's (`schema`'s, through one `$ref`) value when nothing says which:
/// its `default` where that is one of its values, else its first value (`null`
/// aside).
fn enum_fallback(bundle: &Bundle, schema: &Value) -> Option<Value> {
    let s = bundle.resolve(schema);
    let values: Vec<&Value> = s
        .get("enum")?
        .as_array()?
        .iter()
        .filter(|v| !v.is_null())
        .collect();
    s.get("default")
        .filter(|d| values.contains(d))
        .or_else(|| values.first().copied())
        .cloned()
}

/// Whether `persona` (lower case) speaks of `word` (a member's name or an enum value):
/// as written, or with its `_`s and camel-case humps as spaces (`past_due` as "past
/// due", `billingCycle` as "billing cycle").
fn spoken_of(persona: &str, word: &str) -> bool {
    let mut spaced = String::with_capacity(word.len() + 4);
    for (i, c) in word.chars().enumerate() {
        if c == '_' {
            spaced.push(' ');
        } else if c.is_uppercase() && i > 0 {
            spaced.push(' ');
            spaced.extend(c.to_lowercase());
        } else {
            spaced.extend(c.to_lowercase());
        }
    }
    names(persona, &word.to_lowercase()) || names(persona, &spaced)
}

/// Whether `text` holds `word` as a whole token: not inside a longer word (letters,
/// digits, `_` and `-` around it end no token).
fn names(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let joins = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(joins) && !after.is_some_and(joins)
    })
}

/// What the call says beyond the request's top level: the request (what it sent, its
/// body), its path and query parameters, and the objects of the record that stand for
/// the caller.
struct Context<'a> {
    call: &'a OperationCall,
    sent: Option<&'a Map<String, Value>>,
    body: Option<&'a Map<String, Value>>,
    /// Each object of the record (by its dotted path) whose schema is that of a
    /// caller's answer ([`CallerAnswer`]), with that answer's members.
    callers: Vec<(String, &'a Map<String, Value>)>,
}

/// The scalar `root` holds at dotted `path`.
fn scalar_at<'v>(root: Option<&'v Map<String, Value>>, path: &str) -> Option<&'v Value> {
    let mut segments = path.split('.');
    let mut value = root?.get(segments.next()?)?;
    for segment in segments {
        value = value.as_object()?.get(segment)?;
    }
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_)).then_some(value)
}

/// The objects among `declared` (members of a record, at any depth below them through
/// objects, not lists) whose schema has the shape of one of `callers`' answers
/// ([`object_shape`]), each with that answer's members.
fn caller_objects<'a>(
    bundle: &Bundle,
    declared: &[(&str, Value)],
    callers: &'a [CallerAnswer],
) -> Vec<(String, &'a Map<String, Value>)> {
    fn walk<'a>(
        bundle: &Bundle,
        path: &str,
        schema: &Value,
        callers: &[(Value, &'a CallerAnswer)],
        depth: usize,
        out: &mut Vec<(String, &'a Map<String, Value>)>,
    ) {
        if depth > FREE_TEXT_DEPTH {
            return;
        }
        let shape = object_shape(bundle, schema);
        let Some(members) = shape.get("properties").and_then(Value::as_object) else {
            return;
        };
        if let Some((_, caller)) = callers.iter().find(|(s, _)| *s == shape) {
            out.push((path.to_string(), &caller.members));
        }
        for (name, member) in members {
            walk(
                bundle,
                &format!("{path}.{name}"),
                member,
                callers,
                depth + 1,
                out,
            );
        }
    }
    let mut out = Vec::new();
    if callers.is_empty() {
        return out;
    }
    let shapes: Vec<(Value, &CallerAnswer)> = callers
        .iter()
        .map(|c| (object_shape(bundle, &c.schema), c))
        .collect();
    for (name, schema) in declared {
        walk(bundle, name, schema, &shapes, 0, &mut out);
    }
    out
}

impl<'a> Context<'a> {
    /// The scalar the call gives for member `path` below the record's top (dotted, at
    /// least one level down): the request's at that very path; else, inside an object
    /// that stands for the caller, the caller's member of the same name ([`plain`]ly);
    /// else, for a member right below a top-level one whose name is no identifier's, a
    /// path or query parameter of its name (`repository.owner` of `{owner}`). `None`
    /// at the top, which the echoes cover.
    fn nested(&self, path: &str) -> Option<&'a Value> {
        let (parent, leaf) = path.rsplit_once('.')?;
        scalar_at(self.sent, path)
            .or_else(|| scalar_at(self.body, path))
            .or_else(|| {
                let (_, caller) = self.callers.iter().find(|(p, _)| p == parent)?;
                by_name(&[Some(caller)], leaf)
            })
            .or_else(|| {
                (!parent.contains('.') && !id_like(leaf)).then(|| {
                    by_name(
                        &[Some(&self.call.path_params), Some(&self.call.query)],
                        leaf,
                    )
                })?
            })
    }
}

impl Engine {
    /// `value`, member `name`'s (of schema `member`), with each scalar inside its
    /// objects that the call's context gives ([`Context::nested`]) set to it, where it
    /// is of the same JSON type and the member still validates with it: all of them
    /// when it does, else each that does on its own.
    fn with_context(
        &self,
        bundle: &Bundle,
        name: &str,
        member: &Value,
        value: Value,
        context: &Context<'_>,
    ) -> Value {
        fn given<'a>(
            held: &Value,
            path: &str,
            context: &Context<'a>,
            out: &mut Vec<(String, &'a Value)>,
        ) {
            match held {
                Value::Object(map) => {
                    for (key, inner) in map {
                        given(inner, &format!("{path}.{key}"), context, out);
                    }
                }
                Value::String(_) | Value::Number(_) | Value::Bool(_) => {
                    if let Some(given) = context.nested(path)
                        && std::mem::discriminant(given) == std::mem::discriminant(held)
                        && given != held
                    {
                        out.push((path.to_string(), given));
                    }
                }
                _ => {}
            }
        }
        fn set(value: &mut Value, path: &str, to: &Value) {
            let mut at = value;
            for segment in path.split('.').skip(1) {
                let Some(next) = at.get_mut(segment) else {
                    return;
                };
                at = next;
            }
            *at = to.clone();
        }
        if !value.is_object() {
            return value;
        }
        let mut changes = Vec::new();
        given(&value, name, context, &mut changes);
        if changes.is_empty() {
            return value;
        }
        let Ok(validator) = self.schema_validator(bundle, member) else {
            return value;
        };
        let mut all = value.clone();
        for (path, to) in &changes {
            set(&mut all, path, to);
        }
        if validator.is_valid(&all) {
            return all;
        }
        let mut kept = value;
        for (path, to) in &changes {
            let mut tried = kept.clone();
            set(&mut tried, path, to);
            if validator.is_valid(&tried) {
                kept = tried;
            }
        }
        kept
    }
}

/// The one object shape of `schema` (a member's): through `$ref`s and a union or
/// `allOf` with a single shape beside `null`, so the faker draws the object, never the
/// `null` the member also allows; `schema` itself when it has no single shape.
fn object_shape(bundle: &Bundle, schema: &Value) -> Value {
    let mut at = bundle.resolve(schema);
    for _ in 0..FREE_TEXT_DEPTH {
        let Some(s) = at.as_object() else { break };
        if s.contains_key("properties") {
            break;
        }
        let shapes: Vec<&Value> = ["anyOf", "oneOf", "allOf"]
            .iter()
            .filter_map(|k| s.get(*k).and_then(Value::as_array))
            .flatten()
            .filter(|b| bundle.resolve(b).get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        let [only] = shapes.as_slice() else { break };
        at = bundle.resolve(only);
    }
    at.clone()
}
