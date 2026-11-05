//! Error bodies the mock answers itself: a request that fails its schema, and a read
//! of what the session does not hold. They are the contract's declared error response,
//! but what a faker draws for their members says nothing: a validation error read
//! `{"path": "nectar"}` and a missing record `{"type": "quartz"}`.
//!
//! - The call's own facts go into members of matching names: the reasons into the
//!   message-like member ([`set_message`]) and into arrays of strings named as
//!   messages, and the violated field into path-like members, a body member's JSON
//!   pointer into a member named `pointer` and a query or path parameter's name into
//!   one named `parameter` or `param` ([`with_reasons`]). A path-like member the
//!   contract shows to hold a URI (its `format`, or an example that is a URL or an
//!   absolute path, as a request URI reads) is not the violated field. An array of error objects gets one
//!   violation per item. Each change is kept only while the body still validates.
//! - A code-like member (`code`, `type`, `ErrorCode`, `error_code`: a scalar whose
//!   name is `type` or ends in the word `code`) whose schema does not fix its value
//!   ([`writes_code`]) is the contract's to say, often in prose (an error-code table in
//!   the API description). With an LLM, the body is written once per operation,
//!   status and [`ErrorCause`] by the model, told the status and the cause, and kept
//!   ([`crate::Engine::error_answer`]); the call's facts are then laid over it. Offline,
//!   or when the model's answer fails, the faker's body stands.

use mb_mcm::{Bundle, Operation};
use serde_json::{Map, Value};

use crate::guards::{Checks, Echoes};
use crate::validate::SchemaValidator;
use crate::{Engine, GeneratedResponse, GenerationContext, OperationCall, Source, Usage};

/// Properties of a declared error shape that carry the reasons, most preferred first.
pub const MESSAGE_FIELDS: [&str; 7] = [
    "message",
    "detail",
    "error_description",
    "description",
    "msg",
    "title",
    "error",
];

/// Names (lower case, without `_` or `-`) of a member that holds the reasons as an
/// array of strings.
const MESSAGES_ARRAYS: [&str; 4] = ["messages", "errors", "details", "reasons"];

/// Names (lower case, without `_` or `-`) of a member that names the violated field,
/// wherever it is.
const PATH_FIELDS: [&str; 4] = ["path", "field", "fieldname", "property"];

/// Names (lower case, without `_` or `-`) of a member that names a violated query or
/// path parameter.
const PARAMETER_FIELDS: [&str; 2] = ["parameter", "param"];

/// The name (lower case, without `_` or `-`) of a member that takes the violated
/// field's JSON pointer.
const POINTER_FIELD: &str = "pointer";

/// How deep a message field is looked for in an error shape.
const MAX_MESSAGE_DEPTH: usize = 3;
/// How deep a schema or a body is walked.
const MAX_DEPTH: usize = 6;
/// Definitions followed from one schema, at most.
const MAX_REFS: usize = 64;
/// Written error bodies an engine keeps; a new one past them drops another.
pub(crate) const MAX_ERROR_BODIES: usize = 512;

/// What a written error body is kept under: (`<source digest>/<operation id>`,
/// status, cause).
pub(crate) type ErrorKey = (String, u16, ErrorCause);
/// A written error body, set once (`None` when the model's answer failed).
pub(crate) type ErrorCell = std::sync::Arc<tokio::sync::OnceCell<Option<Value>>>;

/// Why the mock answers an error itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCause {
    /// The request does not satisfy the contract's request schema.
    Invalid,
    /// What the call names is not there: never held, or deleted.
    Missing,
}

impl ErrorCause {
    fn told(self) -> &'static str {
        match self {
            ErrorCause::Invalid => {
                "the request does not satisfy the request schema the contract declares (a \
member is missing, of the wrong type, or out of its bounds)"
            }
            ErrorCause::Missing => "the resource the request names does not exist",
        }
    }
}

/// One way a request fails its schema: where (`path`, `query` or `body`), the JSON
/// pointer of the member it is about within that part (`/name`; `""` for the part
/// itself), and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The part of the request: `path`, `query` or `body`.
    pub location: String,
    /// The JSON pointer of the member within that part; `""` for the part itself.
    pub pointer: String,
    /// Why, as the validation reasons say it.
    pub reason: String,
}

impl Violation {
    /// The member's name as a path-like member gives it: its pointer's steps joined by
    /// `.` (`address.city`), `None` for the part itself.
    fn field(&self) -> Option<String> {
        let steps: Vec<String> = self
            .pointer
            .split('/')
            .skip(1)
            .map(|s| s.replace("~1", "/").replace("~0", "~"))
            .collect();
        (!steps.is_empty() && steps.iter().all(|s| !s.is_empty())).then(|| steps.join("."))
    }

    /// The member's JSON pointer, `None` for the part itself.
    fn json_pointer(&self) -> Option<&str> {
        (!self.pointer.is_empty() && self.pointer != "/").then_some(self.pointer.as_str())
    }
}

/// `name` lower case, without `_` or `-`.
fn folded(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether a member of this name holds an error's code (see the module's
/// documentation).
fn code_like(name: &str) -> bool {
    let folded = folded(name);
    folded == "type" || folded.ends_with("code")
}

/// `schema` and the `$ref`, `allOf`, `anyOf` and `oneOf` parts it reaches.
fn branches<'a>(schema: &'a Value, defs: &'a Map<String, Value>) -> Vec<&'a Value> {
    let mut out = Vec::new();
    let mut pending = vec![(schema, 0)];
    while let Some((node, depth)) = pending.pop() {
        if depth > MAX_DEPTH || out.len() >= MAX_REFS {
            break;
        }
        out.push(node);
        if let Some(target) = node
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|name| defs.get(&name.replace("~1", "/").replace("~0", "~")))
        {
            pending.push((target, depth + 1));
        }
        for key in ["allOf", "anyOf", "oneOf"] {
            for member in node
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                pending.push((member, depth + 1));
            }
        }
    }
    out
}

fn types<'a>(branches: &[&'a Value]) -> Vec<&'a str> {
    branches
        .iter()
        .flat_map(|b| match b.get("type") {
            Some(Value::String(t)) => vec![t.as_str()],
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// Whether a branch fixes the value (`const`, or an `enum` of one value).
fn fixed(branches: &[&Value]) -> bool {
    branches.iter().any(|b| {
        b.get("const").is_some()
            || b.get("enum")
                .and_then(Value::as_array)
                .is_some_and(|e| e.len() == 1)
    })
}

/// Whether `schema` declares a code-like scalar member, at any depth, whose value its
/// schema does not fix: what only the contract's prose can say.
#[must_use]
pub fn writes_code(schema: &Value, defs: &Map<String, Value>) -> bool {
    fn walk(schema: &Value, defs: &Map<String, Value>, depth: usize) -> bool {
        if depth > MAX_DEPTH {
            return false;
        }
        let parts = branches(schema, defs);
        parts.iter().any(|b| {
            let members = b.get("properties").and_then(Value::as_object);
            members.into_iter().flatten().any(|(name, prop)| {
                let prop_parts = branches(prop, defs);
                let scalar = types(&prop_parts)
                    .iter()
                    .any(|t| matches!(*t, "string" | "integer" | "number"));
                (code_like(name) && scalar && !fixed(&prop_parts)) || walk(prop, defs, depth + 1)
            }) || b
                .get("items")
                .is_some_and(|items| walk(items, defs, depth + 1))
        })
    }
    walk(schema, defs, 0)
}

/// Writes `message` into the first message-like string property of `schema` (see
/// [`MESSAGE_FIELDS`]), at the top level or nested in objects (Stripe's
/// `error.message`), creating the objects on the way.
pub fn set_message(body: &mut Value, schema: &Value, defs: &Map<String, Value>, message: &str) {
    let Some(path) = message_path(schema, defs, 0) else {
        return;
    };
    let mut at = body;
    for (key, _) in &path[..path.len() - 1] {
        let Value::Object(obj) = at else { return };
        let slot = obj
            .entry(key.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if slot.is_null() {
            *slot = Value::Object(Map::new());
        }
        at = slot;
    }
    let (key, max_len) = &path[path.len() - 1];
    if let Value::Object(obj) = at {
        obj.insert(key.clone(), Value::String(capped(message, *max_len)));
    }
}

fn capped(text: &str, max_len: Option<usize>) -> String {
    match max_len {
        Some(n) => text.chars().take(n).collect(),
        None => text.to_string(),
    }
}

/// The property path to a message-like string field of `schema`, each step with the
/// field's `maxLength`, preferring shallower fields.
fn message_path(
    schema: &Value,
    defs: &Map<String, Value>,
    depth: usize,
) -> Option<Vec<(String, Option<usize>)>> {
    let parts = branches(schema, defs);
    let props: Vec<&Map<String, Value>> = parts
        .iter()
        .filter_map(|b| b.get("properties").and_then(Value::as_object))
        .collect();
    for wanted in MESSAGE_FIELDS {
        let wanted = folded(wanted);
        for p in &props {
            let Some((name, prop)) = p.iter().find(|(name, _)| folded(name) == wanted) else {
                continue;
            };
            let prop_parts = branches(prop, defs);
            let free_string = types(&prop_parts).contains(&"string")
                && prop_parts
                    .iter()
                    .all(|b| b.get("enum").is_none() && b.get("const").is_none());
            if free_string {
                let max_len = prop_parts
                    .iter()
                    .find_map(|b| b.get("maxLength").and_then(Value::as_u64))
                    .and_then(|n| usize::try_from(n).ok());
                return Some(vec![(name.clone(), max_len)]);
            }
        }
    }
    if depth + 1 >= MAX_MESSAGE_DEPTH {
        return None;
    }
    props
        .iter()
        .flat_map(|p| p.iter())
        .find_map(|(name, prop)| {
            let mut path = message_path(prop, defs, depth + 1)?;
            path.insert(0, (name.clone(), None));
            Some(path)
        })
}

/// One change [`with_reasons`] would make: the JSON pointer of a member of the body,
/// and its new value.
type Change = (String, Value);

/// `body`, valid against `validator`, with the call's facts in the members named for
/// them (see the module's documentation): `reasons` in arrays of strings named as
/// messages, and each of `violations` in the path-like and pointer members of one
/// item of an array of error objects (the last one in the items past them), with its
/// reason in that item's message-like member. Each change is kept only while the body
/// still validates; a body not valid to begin with is left as it is.
#[must_use]
pub fn with_reasons(
    validator: &SchemaValidator,
    body: Value,
    reasons: &[String],
    violations: &[Violation],
) -> Value {
    if !validator.is_valid(&body) {
        return body;
    }
    let mut changes = Vec::new();
    let root = validator.schema();
    let no_defs = Map::new();
    let defs = root
        .get("$defs")
        .and_then(Value::as_object)
        .unwrap_or(&no_defs);
    let _ = walk_body(
        &body,
        Some(root),
        &Walk {
            defs,
            reasons,
            violations,
        },
        &mut String::new(),
        0,
        0,
        &mut changes,
    );
    let mut out = body;
    for (pointer, value) in changes {
        let mut candidate = out.clone();
        let Some(slot) = candidate.pointer_mut(&pointer) else {
            continue;
        };
        *slot = value;
        if validator.is_valid(&candidate) {
            out = candidate;
        }
    }
    out
}

/// What [`walk_body`] lays over a body, and the definitions its schema reaches.
struct Walk<'a> {
    defs: &'a Map<String, Value>,
    reasons: &'a [String],
    violations: &'a [Violation],
}

/// The schema of member `name` of an object `schema` declares.
fn member_schema<'a>(
    schema: Option<&'a Value>,
    name: &str,
    defs: &'a Map<String, Value>,
) -> Option<&'a Value> {
    branches(schema?, defs).into_iter().find_map(|b| {
        b.get("properties")
            .and_then(Value::as_object)
            .and_then(|p| p.get(name))
    })
}

/// The schema of the items of an array `schema` declares.
fn items_schema<'a>(schema: Option<&'a Value>, defs: &'a Map<String, Value>) -> Option<&'a Value> {
    branches(schema?, defs)
        .into_iter()
        .find_map(|b| b.get("items").filter(|i| i.is_object()))
}

/// Whether a string `schema` declares holds a URI or URL, not a field's name: its
/// `format` is one of URIs (`uri`, `uri-reference`, `iri`, `url`), or a value the
/// contract shows for it (`example`, `examples`, `default`, `enum`) is a URL or an
/// absolute path (`/v1/widgets`, as a request URI reads).
fn holds_uri(schema: Option<&Value>, defs: &Map<String, Value>) -> bool {
    let Some(schema) = schema else { return false };
    let parts = branches(schema, defs);
    let format = parts.iter().any(|b| {
        b.get("format")
            .and_then(Value::as_str)
            .is_some_and(|f| ["uri", "iri", "url"].iter().any(|p| f.starts_with(p)))
    });
    let shown = parts.iter().any(|b| {
        let one = ["example", "default"]
            .iter()
            .filter_map(|k| b.get(*k))
            .collect::<Vec<_>>();
        let many = ["examples", "enum"]
            .iter()
            .filter_map(|k| b.get(*k).and_then(Value::as_array))
            .flatten();
        one.into_iter()
            .chain(many)
            .filter_map(Value::as_str)
            .any(|v| v.starts_with('/') || v.contains("://"))
    });
    format || shown
}

/// Collects [`with_reasons`]' changes to `value`, declared by `schema`, at `at` in
/// the body and in item `item` of the nearest array of error objects. Whether
/// `value`, an object, or one nested in it outside an array, names the violated
/// field.
///
/// A member named as a pointer takes a body violation's JSON pointer (a pointer is
/// into the request document); one named as a parameter (`parameter`, `param`) takes
/// a query's or a path's name; the other path-like names take any. A member whose
/// schema shows it holds a URI ([`holds_uri`]: Spring Boot's `path`, the request's) is
/// left as it is.
fn walk_body(
    value: &Value,
    schema: Option<&Value>,
    walk: &Walk<'_>,
    at: &mut String,
    item: usize,
    depth: usize,
    out: &mut Vec<Change>,
) -> bool {
    let Walk {
        defs,
        reasons,
        violations,
    } = *walk;
    if depth > MAX_DEPTH {
        return false;
    }
    match value {
        Value::Array(items) => {
            let item_schema = items_schema(schema, defs);
            for (i, v) in items.iter().enumerate() {
                let len = at.len();
                at.push('/');
                at.push_str(&i.to_string());
                let _ = walk_body(v, item_schema, walk, at, i, depth + 1, out);
                at.truncate(len);
            }
            false
        }
        Value::Object(members) => {
            let violation = violations.get(item.min(violations.len().saturating_sub(1)));
            let mut names_field = false;
            for (name, member) in members {
                let folded = folded(name);
                let pointer = format!("{at}/{}", name.replace('~', "~0").replace('/', "~1"));
                if let (Some(violation), Value::String(_)) = (violation, member) {
                    let in_body = violation.location == "body";
                    let named = if folded == POINTER_FIELD {
                        in_body
                            .then(|| violation.json_pointer().map(str::to_string))
                            .flatten()
                    } else if PARAMETER_FIELDS.contains(&folded.as_str()) {
                        (!in_body).then(|| violation.field()).flatten()
                    } else if PATH_FIELDS.contains(&folded.as_str()) {
                        violation.field()
                    } else {
                        None
                    };
                    let named =
                        named.filter(|_| !holds_uri(member_schema(schema, name, defs), defs));
                    if let Some(named) = named {
                        names_field = true;
                        out.push((pointer.clone(), Value::String(named)));
                    }
                }
                if MESSAGES_ARRAYS.contains(&folded.as_str())
                    && !reasons.is_empty()
                    && let Value::Array(items) = member
                    && items.iter().all(Value::is_string)
                {
                    let texts = reasons.iter().cloned().map(Value::String).collect();
                    out.push((pointer, Value::Array(texts)));
                }
            }
            for (name, member) in members {
                if member.is_object() || member.is_array() {
                    let len = at.len();
                    at.push('/');
                    at.push_str(&name.replace('~', "~0").replace('/', "~1"));
                    // A field named in an object under it (JSON:API's `source.pointer`)
                    // is named by this one too.
                    names_field |= walk_body(
                        member,
                        member_schema(schema, name, defs),
                        walk,
                        at,
                        item,
                        depth + 1,
                        out,
                    ) && member.is_object();
                    at.truncate(len);
                }
            }
            // A field named in it: its message is that violation's.
            if names_field && let Some(violation) = violation {
                let message = MESSAGE_FIELDS.iter().find_map(|wanted| {
                    let wanted = folded(wanted);
                    members
                        .iter()
                        .find(|(name, member)| folded(name) == wanted && member.is_string())
                });
                if let Some((name, _)) = message {
                    let pointer = format!("{at}/{}", name.replace('~', "~0").replace('/', "~1"));
                    out.push((pointer, Value::String(violation.reason.clone())));
                }
            }
            names_field
        }
        _ => false,
    }
}

fn error_key(bundle: &Bundle, op: &Operation, status: u16, cause: ErrorCause) -> ErrorKey {
    (format!("{}/{}", bundle.source_digest, op.id), status, cause)
}

/// What the model is told when it writes an error body the mock answers itself.
fn error_note(status: u16, cause: ErrorCause) -> String {
    format!(
        "\n\nThe API refuses this call with status {status}: {}. Answer the error body the API \
gives such a refusal, as the response schema declares it, with the error code, type and other \
values the contract (its API description and this status's description) gives this refusal.",
        cause.told()
    )
}

impl Engine {
    /// The body of `op`'s declared response for `status` that the mock answers itself
    /// for `cause` (see the module's documentation): the model's, written once per
    /// operation, status and cause when the response has a code-like member whose value
    /// the schema leaves open and an LLM answers, else the faker's; then holding what the
    /// contract names for its members (see [`crate::contract_values`]). `None` when `op`
    /// declares no response for `status`. The call's own facts are the caller's to lay
    /// over it ([`set_message`], [`with_reasons`]).
    pub async fn error_answer(
        &self,
        bundle: &Bundle,
        op: &Operation,
        call: &OperationCall,
        status: u16,
        cause: ErrorCause,
    ) -> Option<GeneratedResponse> {
        let variant = op.output_for_status(status)?;
        let ctx = GenerationContext {
            forced_status: Some(status),
            ..GenerationContext::default()
        };
        let mut usage = Usage::default();
        let written = self
            .written_error(bundle, op, (status, cause), call, &mut usage)
            .await;
        let only_empty = written.is_some();
        let mut response =
            written.unwrap_or_else(|| self.fallback(bundle, op, variant, status, call, &ctx));
        self.fill_contract_values(bundle, op, variant, &mut response, (&[], only_empty));
        response.usage = usage;
        Some(response)
    }

    /// Whether the error body for `status` and `cause` of `op` is already written (or
    /// its writing failed and the faker's stands): answering it asks no LLM. A caller
    /// admits the call's quota and rate limit before an answer that is not.
    #[must_use]
    pub fn error_kept(
        &self,
        bundle: &Bundle,
        op: &Operation,
        status: u16,
        cause: ErrorCause,
    ) -> bool {
        self.error_kept_by(&error_key(bundle, op, status, cause))
    }

    pub(crate) fn error_kept_by(&self, key: &ErrorKey) -> bool {
        self.error_bodies
            .get(key)
            .is_some_and(|cell| cell.initialized())
    }

    /// The cell `key`'s body is written into: the kept one, else a new one, which
    /// drops another body when [`MAX_ERROR_BODIES`] are kept.
    fn error_cell(&self, key: &ErrorKey) -> ErrorCell {
        if let Some(cell) = self.error_bodies.get(key) {
            return std::sync::Arc::clone(&cell);
        }
        if self.error_bodies.len() >= MAX_ERROR_BODIES {
            let other = self
                .error_bodies
                .iter()
                .map(|e| e.key().clone())
                .find(|k| k != key);
            if let Some(other) = other {
                self.error_bodies.remove(&other);
            }
        }
        std::sync::Arc::clone(&self.error_bodies.entry(key.clone()).or_default())
    }

    #[cfg(test)]
    pub(crate) fn keep_error_body(&self, key: ErrorKey, body: Option<Value>) {
        let _ = self.error_cell(&key).set(body);
    }

    /// The validator of `op`'s response for `status`, when it has a JSON schema.
    #[must_use]
    pub fn error_validator(
        &self,
        bundle: &Bundle,
        op: &Operation,
        status: u16,
    ) -> Option<std::sync::Arc<SchemaValidator>> {
        let variant = op.output_for_status(status)?;
        if crate::media::is_non_json(op, variant) {
            return None;
        }
        let schema = variant.schema.as_ref()?;
        self.validator(bundle, op, status, schema).ok()
    }

    /// The error body the model wrote for `status` and `cause` of `op`, kept, or asked
    /// for now (see [`Self::error_answer`]). `None` offline, when the response has no
    /// code-like member to write, or when the model's answer failed.
    async fn written_error(
        &self,
        bundle: &Bundle,
        op: &Operation,
        (status, cause): (u16, ErrorCause),
        call: &OperationCall,
        usage: &mut Usage,
    ) -> Option<GeneratedResponse> {
        let provider = self.provider.as_ref()?;
        let variant = op.output_for_status(status)?;
        if crate::media::is_non_json(op, variant) {
            return None;
        }
        let schema = variant.schema.as_ref()?;
        if !writes_code(schema, &bundle.defs) {
            return None;
        }
        let validator = self.validator(bundle, op, status, schema).ok()?;
        if crate::guards::items_envelope(provider.capabilities(), validator.schema()) {
            return None;
        }
        let key = error_key(bundle, op, status, cause);
        let cell = self.error_cell(&key);
        let body = cell
            .get_or_init(|| async {
                let mut messages = crate::prompt::build_messages(
                    bundle,
                    op,
                    status,
                    schema,
                    &OperationCall {
                        operation_id: call.operation_id.clone(),
                        ..OperationCall::default()
                    },
                    &GenerationContext {
                        forced_status: Some(status),
                        ..GenerationContext::default()
                    },
                    self.config.persona.as_deref(),
                );
                if let Some(user) = messages.last_mut() {
                    user.content.push_str(&error_note(status, cause));
                }
                let format = mb_llm::ResponseFormat::JsonSchema {
                    name: crate::format_name(&op.id),
                    schema: crate::compact::response_prompt_schema(bundle, schema),
                };
                let checks = Checks {
                    fill: None,
                    echoes: Echoes::of(std::iter::empty()),
                    items_envelope: false,
                    effects_envelope: false,
                    refusals: &[],
                };
                let answer = self
                    .generate_with_llm(
                        provider.as_ref(),
                        &validator,
                        messages,
                        &format,
                        &checks,
                        usage,
                    )
                    .await;
                match answer {
                    Ok(answer) => Some(answer.value),
                    Err(reason) => {
                        tracing::warn!(operation = %op.id, status, %reason, "the error body was not written: the faker's is served");
                        None
                    }
                }
            })
            .await
            .clone()?;
        validator.is_valid(&body).then(|| GeneratedResponse {
            status,
            content_type: variant.content_type.clone(),
            body: Some(body),
            source: Source::Llm,
            usage: Usage::default(),
            fallback_reason: None,
            effects: Vec::new(),
            effect_outcomes: Vec::new(),
            dropped_effects: 0,
            new_record: false,
            patched: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn violation(pointer: &str, reason: &str) -> Violation {
        located("body", pointer, reason)
    }

    fn located(location: &str, pointer: &str, reason: &str) -> Violation {
        Violation {
            location: location.into(),
            pointer: pointer.into(),
            reason: reason.into(),
        }
    }

    #[test]
    fn a_code_like_member_is_named_type_or_ends_in_code() {
        for name in [
            "type",
            "code",
            "ErrorCode",
            "error_code",
            "errorCode",
            "Type",
        ] {
            assert!(code_like(name), "{name}");
        }
        for name in ["message", "codes_url", "typed", "status"] {
            assert!(!code_like(name), "{name}");
        }
    }

    #[test]
    fn a_code_the_schema_fixes_is_not_written() {
        let defs = Map::new();
        let open = json!({"type": "object", "properties": {"error": {"type": "object", "properties": {"code": {"type": "string"}}}}});
        assert!(writes_code(&open, &defs));
        let fixed =
            json!({"type": "object", "properties": {"code": {"type": "string", "enum": ["bad"]}}});
        assert!(!writes_code(&fixed, &defs));
        let none = json!({"type": "object", "properties": {"message": {"type": "string"}}});
        assert!(!writes_code(&none, &defs));
        let in_items = json!({"type": "array", "items": {"type": "object", "properties": {"ErrorCode": {"type": "integer"}}}});
        assert!(writes_code(&in_items, &defs));
    }

    #[test]
    fn each_error_item_names_its_violation() {
        let schema = json!({
            "type": "object",
            "properties": {
                "messages": {"type": "array", "items": {"type": "string"}},
                "errors": {"type": "array", "items": {"type": "object", "properties": {
                    "field": {"type": "string"},
                    "message": {"type": "string"},
                    "source": {"type": "object", "properties": {"pointer": {"type": "string"}}}
                }}}
            }
        });
        let validator = SchemaValidator::new(&schema).unwrap();
        let body = json!({
            "messages": ["x"],
            "errors": [
                {"field": "a", "message": "b", "source": {"pointer": "c"}},
                {"field": "a", "message": "b", "source": {"pointer": "c"}},
                {"field": "a", "message": "b", "source": {"pointer": "c"}}
            ]
        });
        let reasons = vec!["r1".to_string(), "r2".to_string()];
        let violations = [violation("/name", "r1"), violation("/address/city", "r2")];
        let out = with_reasons(&validator, body, &reasons, &violations);
        assert_eq!(out["messages"], json!(["r1", "r2"]));
        assert_eq!(out["errors"][0]["field"], "name");
        assert_eq!(out["errors"][0]["message"], "r1");
        assert_eq!(out["errors"][0]["source"]["pointer"], "/name");
        assert_eq!(out["errors"][1]["field"], "address.city");
        assert_eq!(out["errors"][1]["source"]["pointer"], "/address/city");
        // Past the violations, the last one.
        assert_eq!(out["errors"][2]["field"], "address.city");
    }

    #[test]
    fn a_change_the_schema_refuses_is_not_kept() {
        let schema = json!({"type": "object", "properties": {
            "path": {"type": "string", "maxLength": 2},
            "messages": {"type": "array", "maxItems": 1, "items": {"type": "string"}}
        }});
        let validator = SchemaValidator::new(&schema).unwrap();
        let body = json!({"path": "ab", "messages": ["x"]});
        let reasons = vec!["r1".to_string(), "r2".to_string()];
        let out = with_reasons(&validator, body, &reasons, &[violation("/name", "r1")]);
        assert_eq!(out, json!({"path": "ab", "messages": ["x"]}));
    }

    #[test]
    fn the_part_itself_names_no_field() {
        let schema = json!({"type": "object", "properties": {"path": {"type": "string"}}});
        let validator = SchemaValidator::new(&schema).unwrap();
        let out = with_reasons(&validator, json!({"path": "x"}), &[], &[violation("", "r")]);
        assert_eq!(out["path"], "x");
    }

    /// A `path` the contract shows as a request URI (Spring Boot's
    /// `{timestamp, status, error, message, path}`) is not the violated field.
    #[test]
    fn a_uri_path_member_is_not_the_violated_field() {
        let schema = json!({"type": "object", "properties": {
            "timestamp": {"type": "string"},
            "status": {"type": "integer"},
            "error": {"type": "string"},
            "message": {"type": "string"},
            "path": {"type": "string", "description": "The request URI", "example": "/v1/widgets"}
        }});
        let validator = SchemaValidator::new(&schema).unwrap();
        let body = json!({"timestamp": "t", "status": 400, "error": "e", "message": "m", "path": "/v1/widgets"});
        let out = with_reasons(&validator, body, &[], &[violation("/name", "r")]);
        assert_eq!(out["path"], "/v1/widgets");

        for member in [
            json!({"type": "string", "format": "uri-reference"}),
            json!({"type": "string", "format": "uri"}),
            json!({"type": "string", "default": "/v1/widgets"}),
            json!({"type": "string", "examples": ["https://api.example.com/v1/widgets"]}),
        ] {
            let schema = json!({"type": "object", "properties": {"path": member}});
            let validator = SchemaValidator::new(&schema).unwrap();
            let out = with_reasons(
                &validator,
                json!({"path": "x"}),
                &[],
                &[violation("/name", "r")],
            );
            assert_eq!(out["path"], "x", "{schema}");
        }

        // A field name as the example: the field.
        let schema = json!({"type": "object", "properties": {
            "path": {"type": "string", "example": "subject"}
        }});
        let validator = SchemaValidator::new(&schema).unwrap();
        let out = with_reasons(
            &validator,
            json!({"path": "x"}),
            &[],
            &[violation("/name", "r")],
        );
        assert_eq!(out["path"], "name");
    }

    /// The schema is followed through `$defs` to the member's own.
    #[test]
    fn a_uri_path_member_is_seen_through_refs() {
        let schema = json!({
            "type": "object",
            "properties": {"errors": {"type": "array", "items": {"$ref": "#/$defs/Err"}}},
            "$defs": {"Err": {"type": "object", "properties": {
                "path": {"type": "string", "format": "uri-reference"},
                "field": {"type": "string"}
            }}}
        });
        let validator = SchemaValidator::new(&schema).unwrap();
        let body = json!({"errors": [{"path": "/v1/x", "field": "f"}]});
        let out = with_reasons(&validator, body, &[], &[violation("/name", "r")]);
        assert_eq!(out["errors"][0]["path"], "/v1/x");
        assert_eq!(out["errors"][0]["field"], "name");
    }

    /// A pointer is into the request document: only a body violation has one. A
    /// parameter is a query's or a path's.
    #[test]
    fn a_pointer_is_the_bodys_and_a_parameter_the_querys() {
        let schema = json!({"type": "object", "properties": {"source": {"type": "object", "properties": {
            "pointer": {"type": "string"},
            "parameter": {"type": "string"}
        }}}});
        let validator = SchemaValidator::new(&schema).unwrap();
        let body = json!({"source": {"pointer": "p", "parameter": "q"}});
        let out = with_reasons(
            &validator,
            body.clone(),
            &[],
            &[located("query", "/limit", "r")],
        );
        assert_eq!(out["source"]["pointer"], "p");
        assert_eq!(out["source"]["parameter"], "limit");
        let out = with_reasons(&validator, body, &[], &[violation("/name", "r")]);
        assert_eq!(out["source"]["pointer"], "/name");
        assert_eq!(out["source"]["parameter"], "q");
    }

    /// A full cache gives up one body for a new one, not all of them.
    #[test]
    fn a_full_cache_drops_one_body() {
        let engine = Engine::new(None, crate::EngineConfig::default());
        for i in 0..MAX_ERROR_BODIES {
            engine.keep_error_body((format!("d/op{i}"), 400, ErrorCause::Invalid), None);
        }
        engine.keep_error_body(("d/new".into(), 400, ErrorCause::Invalid), None);
        assert_eq!(engine.error_bodies.len(), MAX_ERROR_BODIES);
        assert!(engine.error_kept_by(&("d/new".into(), 400, ErrorCause::Invalid)));
    }
}
