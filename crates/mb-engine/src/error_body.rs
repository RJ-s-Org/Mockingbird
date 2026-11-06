//! Error bodies the mock answers itself: a request that fails its schema, a read of
//! what the session does not hold, and a write refused from state (a value a unique set
//! keeps already held by another record, a stale concurrency token; see
//! [`crate::Engine::refuse_held`] and [`RefusalFacts`]). They are the contract's declared error response,
//! but what a faker draws for their members says nothing: a validation error read
//! `{"path": "nectar"}` and a missing record `{"type": "quartz"}`.
//!
//! - The call's own facts go into members of matching names: the reasons into the
//!   message-like member ([`set_message`]) and into arrays of strings named as
//!   messages, and the violated field into path-like members, a body member's JSON
//!   pointer into a member named `pointer` and a query or path parameter's name into
//!   one named `parameter` or `param` ([`with_reasons`]). A path-like member the
//!   contract shows to hold a URI (its `format`, or an example that is a URL or an
//!   absolute path, as a request URI reads) is not the violated field; a `pointer`
//!   holds a URI only by its `format`, since a JSON pointer starts with `/` too. An
//!   array of error objects gets one violation per item, its message that violation's
//!   reason, and grows to one item per violation (its last item repeated, or, when
//!   empty, items made of the members that name each violation); a single error
//!   object names the first violation's field and keeps every reason in its message.
//!   Each change is kept only while the body still validates.
//! - A code-like member (`code`, `type`, `ErrorCode`, `error_code`: a scalar whose
//!   name is `type` or ends in the word `code`, but not one holding a URI, as
//!   problem+json's `type` does, nor an integer HTTP status such as `status_code`)
//!   whose schema does not fix its value ([`writes_code`]) is the contract's to say, often in prose (an error-code table in
//!   the API description). With an LLM, the body is written once per operation,
//!   status and [`ErrorCause`] by the model, told the status and the cause, and kept
//!   ([`crate::Engine::error_answer`]); the call's facts are then laid over it (a
//!   refusal from state: its message, and the members a taken unique set names in its
//!   path-like members). Offline, or when the model's answer fails, the faker's body
//!   stands, with the same facts (a failed write is asked for again after a minute).
//!   A refusal decided from state (a taken unique value, a stale token, a missing
//!   record) has no violation to say, so its body is also written when its schema has
//!   a free text member nothing else fills ([`writes_text`]: JSON:API's `title` beside
//!   its `detail`), which the faker alone would fill with a word.
//! - A string member that holds the HTTP status as text, by its schema alone (its
//!   shown values are 3-digit codes, or its pattern takes the status and no word), says
//!   the response's status ([`with_status_text`]), at the top level or in the items of
//!   an array of error objects.
//! - Since a written body serves every later call, it is checked before it is kept
//!   (`body_flaw`): it validates against the declared error schema; each code-like
//!   string is a short token (at most `64` characters, no unit repeated back to back
//!   3 times, no whitespace run, line break, tab or edge space), unless it is a token
//!   the API description or the response's description names in backticks
//!   (`` `invalid_parameters` ``); and no string is longer than `2048` characters,
//!   nor, past `40` characters, more than `50` percent one unit repeated. Only a
//!   word-like unit counts: at least 3 characters, with a letter, and no repeat of a
//!   shorter unit itself; so a zero-filled id (`req_000…`), a run of one character, a
//!   hex code (`0xABABAB`) or a quoted `hahaha` is kept, and `invalid_parameters!`
//!   repeated is not. Nor does a repeat count that the call which triggers the write
//!   carries (a path, query or header value, or a body value): quoting the request is
//!   no loop. The guard sees the model's body alone, before the call's facts are laid
//!   over it. A body that fails is asked for once more (2 asks in all); when the
//!   second fails too, the write failed: the faker's body stands.

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
/// How long a failed write is kept: past it, the next answer of that error asks the
/// LLM again, so one transient provider error does not pin the faker's code.
pub(crate) const FAILED_RETRY: std::time::Duration = std::time::Duration::from_secs(60);

/// A written error body (`None` when the model's answer failed), and when.
#[derive(Clone, Debug)]
pub(crate) struct Kept {
    body: Option<Value>,
    at: std::time::Instant,
}

impl Kept {
    fn now(body: Option<Value>) -> Self {
        Self {
            body,
            at: std::time::Instant::now(),
        }
    }

    /// A failed write kept past [`FAILED_RETRY`]: to be written again.
    fn stale(&self) -> bool {
        self.body.is_none() && self.at.elapsed() >= FAILED_RETRY
    }
}

/// A written error body, set once ([`Kept`]); a cell holding a stale failure is
/// replaced by a new one.
pub(crate) type ErrorCell = std::sync::Arc<tokio::sync::OnceCell<Kept>>;

/// Why the mock answers an error itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCause {
    /// The request does not satisfy the contract's request schema.
    Invalid,
    /// What the call names is not there: never held, or deleted.
    Missing,
    /// A value the request gives a member the API keeps unique is already held by
    /// another record.
    Taken,
    /// The request's concurrency token or precondition does not match the record's
    /// current version.
    Stale,
}

impl ErrorCause {
    fn told(self) -> &'static str {
        match self {
            ErrorCause::Invalid => {
                "the request does not satisfy the request schema the contract declares (a \
member is missing, of the wrong type, or out of its bounds)"
            }
            ErrorCause::Missing => "the resource the request names does not exist",
            ErrorCause::Taken => {
                "a value the request gives a member the API keeps unique is already held by \
another record"
            }
            ErrorCause::Stale => {
                "the request's concurrency token or precondition does not match the record's \
current version"
            }
        }
    }
}

/// What a refusal decided from state says of the call, laid over its error body (see
/// [`crate::Engine::refuse_held`]): a message, and the request members it is about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefusalFacts {
    message: String,
    members: Vec<String>,
}

impl RefusalFacts {
    /// A unique set whose values `taken` (each member, as a dotted path, and the value
    /// the request gives it) another record holds: `name 'x' is already in use`, the
    /// members joined by `with`.
    #[must_use]
    pub fn taken(taken: &[(String, Value)]) -> Self {
        let named: Vec<String> = taken
            .iter()
            .map(|(member, value)| match value {
                Value::String(s) => format!("{member} '{s}'"),
                other => format!("{member} {other}"),
            })
            .collect();
        Self {
            message: format!("{} is already in use", named.join(" with ")),
            members: taken.iter().map(|(m, _)| m.clone()).collect(),
        }
    }

    /// A concurrency token or precondition that is not the record's current one.
    #[must_use]
    pub fn stale() -> Self {
        Self::message("the request's concurrency token does not match the record's current version")
    }

    /// Only `message`, about no member.
    #[must_use]
    pub fn message(message: &str) -> Self {
        Self {
            message: message.to_string(),
            members: Vec::new(),
        }
    }

    /// The message-like member's text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.message
    }

    /// The request members the refusal is about, as dotted paths.
    #[must_use]
    pub fn members(&self) -> &[String] {
        &self.members
    }

    /// One body [`Violation`] per member, each with the message as its reason.
    pub(crate) fn violations(&self) -> Vec<Violation> {
        self.members
            .iter()
            .map(|member| Violation {
                location: "body".into(),
                pointer: member
                    .split('.')
                    .map(|step| format!("/{}", step.replace('~', "~0").replace('/', "~1")))
                    .collect(),
                reason: self.message.clone(),
            })
            .collect()
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

/// Whether member `name`, declared by `prop`, is a code-like scalar whose value its
/// schema does not fix: not a URI (problem+json's `type`) nor an integer HTTP status
/// (`status_code`), which are no code the contract's prose says.
fn open_code(name: &str, prop: &Value, defs: &Map<String, Value>) -> bool {
    if !code_like(name) {
        return false;
    }
    let prop_parts = branches(prop, defs);
    let kinds = types(&prop_parts);
    let scalar = kinds
        .iter()
        .any(|t| matches!(*t, "string" | "integer" | "number"));
    let uri = uri_format(&prop_parts) || shown_values(&prop_parts).any(|v| v.contains("://"));
    let http_status = folded(name).ends_with("statuscode") && !kinds.contains(&"string");
    scalar && !fixed(&prop_parts) && !uri && !http_status
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
            members
                .into_iter()
                .flatten()
                .any(|(name, prop)| open_code(name, prop, defs) || walk(prop, defs, depth + 1))
                || b.get("items")
                    .is_some_and(|items| walk(items, defs, depth + 1))
        })
    }
    walk(schema, defs, 0)
}

/// Whether a string member, declared by `parts`, holds the HTTP status as text: every
/// value the contract shows for it (`example`, `examples`, `default`, `enum`) is a
/// 3-digit code, or, showing none, its `pattern` takes `status` and no word. A member
/// whose values are words (`"fail"`, `"error"`) is the API's own. Its name says nothing.
fn holds_status_text(parts: &[&Value], status: u16) -> bool {
    if !types(parts).contains(&"string") {
        return false;
    }
    let code = |v: &str| v.len() == 3 && v.bytes().all(|b| b.is_ascii_digit());
    let mut shown = shown_values(parts).peekable();
    if shown.peek().is_some() {
        return shown.all(code);
    }
    let text = status.to_string();
    parts
        .iter()
        .filter_map(|b| b.get("pattern").and_then(Value::as_str))
        .any(|pattern| {
            regex::Regex::new(pattern).is_ok_and(|re| re.is_match(&text) && !re.is_match("error"))
        })
}

/// Whether `schema` declares a free text member, at any depth, that nothing but the
/// faker would fill in an error body for `status`: a string with no `const`, `enum`
/// or `format`, that is not its object's message-like member (the first of
/// [`MESSAGE_FIELDS`] it declares), nor one naming the violated field
/// ([`with_reasons`]' path-like, parameter and pointer members), nor one holding the
/// HTTP status as text ([`with_status_text`]). JSON:API's `title` beside its
/// `detail` is one: the faker's word there says nothing.
#[must_use]
pub fn writes_text(schema: &Value, defs: &Map<String, Value>, status: u16) -> bool {
    fn walk(schema: &Value, defs: &Map<String, Value>, status: u16, depth: usize) -> bool {
        if depth > MAX_DEPTH {
            return false;
        }
        let parts = branches(schema, defs);
        let members: Vec<(&String, &Value)> = parts
            .iter()
            .filter_map(|b| b.get("properties").and_then(Value::as_object))
            .flatten()
            .collect();
        let message = MESSAGE_FIELDS.iter().find_map(|wanted| {
            let wanted = folded(wanted);
            members
                .iter()
                .find(|(name, prop)| {
                    folded(name) == wanted && types(&branches(prop, defs)).contains(&"string")
                })
                .map(|(name, _)| name.as_str())
        });
        let free = |name: &str, prop: &Value| {
            let prop_parts = branches(prop, defs);
            types(&prop_parts).contains(&"string")
                && Some(name) != message
                && !names_violation(&folded(name))
                && !prop_parts.iter().any(|b| {
                    b.get("const").is_some() || b.get("enum").is_some() || b.get("format").is_some()
                })
                && !holds_status_text(&prop_parts, status)
        };
        members
            .iter()
            .any(|(name, prop)| free(name, prop) || walk(prop, defs, status, depth + 1))
            || parts
                .iter()
                .filter_map(|b| b.get("items"))
                .any(|items| walk(items, defs, status, depth + 1))
    }
    walk(schema, defs, status, 0)
}

/// `body`, an error body declared by `schema` (its definitions in `defs`), with each
/// string member that holds the HTTP status as text ([`holds_status_text`]) saying
/// `status`: at the top level, or in each item of an array member (JSON:API's
/// `errors[].status`). Each change is kept only while the body still validates
/// against `validator`.
#[must_use]
pub fn with_status_text(
    validator: &SchemaValidator,
    (schema, defs): (&Value, &Map<String, Value>),
    body: Value,
    status: u16,
) -> Value {
    let members = |schema: &'_ Value| -> Vec<(String, Value)> {
        branches(schema, defs)
            .into_iter()
            .filter_map(|b| b.get("properties").and_then(Value::as_object))
            .flatten()
            .map(|(name, prop)| (name.clone(), prop.clone()))
            .collect()
    };
    let escaped = |name: &str| name.replace('~', "~0").replace('/', "~1");
    // Each place as (the object's pointer, the member's name).
    let mut places: Vec<(String, String)> = Vec::new();
    for (name, prop) in members(schema) {
        if holds_status_text(&branches(&prop, defs), status) {
            places.push((String::new(), name));
            continue;
        }
        let Some(items) = items_schema(Some(&prop), defs) else {
            continue;
        };
        let held = body
            .get(&name)
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        for (item_name, item_prop) in members(items) {
            if holds_status_text(&branches(&item_prop, defs), status) {
                for i in 0..held {
                    places.push((format!("/{}/{i}", escaped(&name)), item_name.clone()));
                }
            }
        }
    }
    let text = Value::String(status.to_string());
    let mut out = body;
    for (at, member) in places {
        if out.pointer(&at).and_then(|o| o.get(&member)) == Some(&text) {
            continue;
        }
        let mut candidate = out.clone();
        let Some(Value::Object(object)) = candidate.pointer_mut(&at) else {
            continue;
        };
        object.insert(member, text.clone());
        if validator.is_valid(&candidate) {
            out = candidate;
        }
    }
    out
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
/// reason in that item's message-like member. Outside such an array, the first
/// violation names the field and the message-like member keeps every reason. A map
/// of reasons (`reasons_map`) is keyed by the violated fields, and an array of
/// objects naming no field says the reasons in their message-like member
/// (`message_items`). Each change is kept only while the body
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
    let body = grown(validator, body, defs, violations);
    let _ = walk_body(
        &body,
        Some(root),
        &Walk {
            defs,
            reasons,
            violations,
        },
        &mut String::new(),
        Scope::Outside,
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

/// `body`, valid against `validator`, with each array of error objects in it
/// ([`error_arrays`]) holding one item per violation where it held fewer: its last
/// item repeated, or, when it is empty, items made of the members that name each
/// violation ([`made_item`]). Each array grows as far as the body still validates.
fn grown(
    validator: &SchemaValidator,
    body: Value,
    defs: &Map<String, Value>,
    violations: &[Violation],
) -> Value {
    if violations.is_empty() {
        return body;
    }
    let mut arrays = Vec::new();
    error_arrays(
        &body,
        Some(validator.schema()),
        defs,
        &mut String::new(),
        0,
        &mut arrays,
    );
    let mut out = body;
    for (pointer, item_schema) in arrays {
        let Some(Value::Array(items)) = out.pointer(&pointer) else {
            continue;
        };
        let held = items.len();
        if held >= violations.len() {
            continue;
        }
        let extra: Vec<Value> = match items.last() {
            Some(last) => violations[held..].iter().map(|_| last.clone()).collect(),
            None => violations
                .iter()
                .map_while(|v| made_item(&item_schema, defs, v, 0))
                .collect(),
        };
        for keep in (1..=extra.len()).rev() {
            let mut candidate = out.clone();
            let Some(Value::Array(slot)) = candidate.pointer_mut(&pointer) else {
                break;
            };
            slot.extend(extra[..keep].iter().cloned());
            if validator.is_valid(&candidate) {
                out = candidate;
                break;
            }
        }
    }
    out
}

/// Whether a member of this (folded) name names a violation: a path-like, parameter
/// or pointer member.
fn names_violation(folded: &str) -> bool {
    folded == POINTER_FIELD || PATH_FIELDS.contains(&folded) || PARAMETER_FIELDS.contains(&folded)
}

/// Whether objects `schema` declares are error objects: a string member of theirs,
/// or of an object one member down (JSON:API's `source.pointer`), names a violation.
fn error_object(schema: &Value, defs: &Map<String, Value>, depth: usize) -> bool {
    branches(schema, defs).into_iter().any(|b| {
        b.get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .any(|(name, prop)| {
                let parts = branches(prop, defs);
                let kinds = types(&parts);
                (names_violation(&folded(name)) && kinds.contains(&"string"))
                    || (depth == 0 && kinds.contains(&"object") && error_object(prop, defs, 1))
            })
    })
}

/// The arrays of error objects in `value`, declared by `schema`: each one's JSON
/// pointer in the body and its items' schema. An array inside one is not looked into.
fn error_arrays(
    value: &Value,
    schema: Option<&Value>,
    defs: &Map<String, Value>,
    at: &mut String,
    depth: usize,
    out: &mut Vec<(String, Value)>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    match value {
        Value::Array(items) => {
            let item_schema = items_schema(schema, defs);
            if let Some(item_schema) = item_schema.filter(|s| error_object(s, defs, 0)) {
                out.push((at.clone(), item_schema.clone()));
                return;
            }
            for (i, v) in items.iter().enumerate() {
                let len = at.len();
                at.push('/');
                at.push_str(&i.to_string());
                error_arrays(v, item_schema, defs, at, depth + 1, out);
                at.truncate(len);
            }
        }
        Value::Object(members) => {
            for (name, member) in members {
                let len = at.len();
                at.push('/');
                at.push_str(&name.replace('~', "~0").replace('/', "~1"));
                error_arrays(
                    member,
                    member_schema(schema, name, defs),
                    defs,
                    at,
                    depth + 1,
                    out,
                );
                at.truncate(len);
            }
        }
        _ => {}
    }
}

/// An error object `schema` declares, made of the members that name `violation`
/// (as [`walk_body`] would fill them) and its message-like member holding the
/// reason; `None` when no member names it.
fn made_item(
    schema: &Value,
    defs: &Map<String, Value>,
    violation: &Violation,
    depth: usize,
) -> Option<Value> {
    let in_body = violation.location == "body";
    let mut item = Map::new();
    for b in branches(schema, defs) {
        let Some(props) = b.get("properties").and_then(Value::as_object) else {
            continue;
        };
        for (name, prop) in props {
            if item.contains_key(name) {
                continue;
            }
            let folded = folded(name);
            let parts = branches(prop, defs);
            let kinds = types(&parts);
            if kinds.contains(&"string") {
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
                let named = named.filter(|_| !holds_uri(Some(prop), defs, folded == POINTER_FIELD));
                if let Some(named) = named {
                    item.insert(name.clone(), Value::String(named));
                }
            } else if depth == 0
                && kinds.contains(&"object")
                && let Some(nested) = made_item(prop, defs, violation, depth + 1)
            {
                item.insert(name.clone(), nested);
            }
        }
    }
    if item.is_empty() {
        return None;
    }
    if depth == 0 {
        let props: Vec<&Map<String, Value>> = branches(schema, defs)
            .into_iter()
            .filter_map(|b| b.get("properties").and_then(Value::as_object))
            .collect();
        let message = MESSAGE_FIELDS.iter().find_map(|wanted| {
            let wanted = folded(wanted);
            props.iter().find_map(|p| {
                p.iter().find(|(name, prop)| {
                    folded(name) == wanted
                        && types(&branches(prop, defs)).contains(&"string")
                        && !item.contains_key(*name)
                })
            })
        });
        if let Some((name, _)) = message {
            item.insert(name.clone(), Value::String(violation.reason.clone()));
        }
    }
    Some(Value::Object(item))
}

/// Where in a body [`walk_body`] is, as to the violations it names.
#[derive(Clone, Copy)]
enum Scope {
    /// Outside any array: the first violation names the field.
    Outside,
    /// In item `i` of an array of error objects ([`error_arrays`]): violation `i` (the
    /// last one past them).
    Item(usize),
    /// In an item of another array (one nested in an error item, or one whose items
    /// are no error objects): no violation is named.
    Unrelated,
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

/// Whether a branch's `format` is one of URIs (`uri`, `uri-reference`, `iri`, `url`).
fn uri_format(parts: &[&Value]) -> bool {
    parts.iter().any(|b| {
        b.get("format")
            .and_then(Value::as_str)
            .is_some_and(|f| ["uri", "iri", "url"].iter().any(|p| f.starts_with(p)))
    })
}

/// The string values the contract shows for a member (`example`, `examples`,
/// `default`, `enum`).
fn shown_values<'a>(parts: &[&'a Value]) -> impl Iterator<Item = &'a str> {
    parts.iter().flat_map(|b| {
        let one = ["example", "default"].into_iter().filter_map(|k| b.get(k));
        let many = ["examples", "enum"]
            .into_iter()
            .filter_map(|k| b.get(k).and_then(Value::as_array))
            .flatten();
        one.chain(many).filter_map(Value::as_str)
    })
}

/// Whether a string `schema` declares holds a URI or URL, not a field's name: its
/// `format` is one of URIs ([`uri_format`]), or, unless it is a `pointer` (a JSON
/// pointer starts with `/` too), a value the contract shows for it is a URL or an
/// absolute path (`/v1/widgets`, as a request URI reads).
fn holds_uri(schema: Option<&Value>, defs: &Map<String, Value>, pointer: bool) -> bool {
    let Some(schema) = schema else { return false };
    let parts = branches(schema, defs);
    uri_format(&parts)
        || (!pointer && shown_values(&parts).any(|v| v.starts_with('/') || v.contains("://")))
}

/// Collects [`with_reasons`]' changes to `value`, declared by `schema`, at `at` in
/// the body, in `scope` ([`Scope`]: outside an array, the first violation names the
/// field and the message keeps every reason; in an item of an array of error objects,
/// that item's violation does; in another array's items, none does).
/// Whether `value`, an object, or one nested in it outside an array, names the
/// violated field.
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
    scope: Scope,
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
            // As `error_arrays` finds them: not one inside an error item.
            let errors = !matches!(scope, Scope::Item(_))
                && item_schema.is_some_and(|s| error_object(s, defs, 0));
            if !errors && matches!(scope, Scope::Outside) {
                message_items(items, item_schema, walk, at, out);
            }
            for (i, v) in items.iter().enumerate() {
                let len = at.len();
                at.push('/');
                at.push_str(&i.to_string());
                let scope = if errors {
                    Scope::Item(i)
                } else {
                    Scope::Unrelated
                };
                let _ = walk_body(v, item_schema, walk, at, scope, depth + 1, out);
                at.truncate(len);
            }
            false
        }
        Value::Object(members) => {
            // The body itself, or a member named as the reasons (`{"errors": {…}}`):
            // another map (a `metadata` one) says something else.
            let reasons_member = depth == 0
                || at
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| MESSAGES_ARRAYS.contains(&folded(name).as_str()));
            if matches!(scope, Scope::Outside)
                && reasons_member
                && let Some(shape) = reasons_map(schema, defs)
            {
                return match keyed_reasons(members, shape, walk) {
                    Some(map) => {
                        out.push((at.clone(), map));
                        !violations.is_empty()
                    }
                    None => false,
                };
            }
            let last = violations.len().saturating_sub(1);
            let violation = match scope {
                Scope::Outside => violations.first(),
                Scope::Item(i) => violations.get(i.min(last)),
                Scope::Unrelated => None,
            };
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
                    let uri = || {
                        holds_uri(
                            member_schema(schema, name, defs),
                            defs,
                            folded == POINTER_FIELD,
                        )
                    };
                    let named = named.filter(|_| !uri());
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
                        scope,
                        depth + 1,
                        out,
                    ) && member.is_object();
                    at.truncate(len);
                }
            }
            // A field named in an item of an array of errors: its message is that
            // violation's. Outside one, the message keeps every reason.
            if names_field
                && matches!(scope, Scope::Item(_))
                && let Some(violation) = violation
            {
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

/// How a map of reasons ([`reasons_map`]) holds each member's reasons.
#[derive(Clone, Copy)]
enum ReasonsShape {
    /// One string.
    Text,
    /// An array of strings.
    Texts,
}

/// Whether objects `schema` declares are a map of reasons (Rails'
/// `{"name": ["can't be blank"]}`): they declare no member, and every other member is
/// a string or an array of strings.
fn reasons_map(schema: Option<&Value>, defs: &Map<String, Value>) -> Option<ReasonsShape> {
    let parts = branches(schema?, defs);
    let declares = parts.iter().any(|b| {
        b.get("properties")
            .and_then(Value::as_object)
            .is_some_and(|p| !p.is_empty())
    });
    if declares {
        return None;
    }
    parts.iter().find_map(|b| {
        let rest = b.get("additionalProperties").filter(|r| r.is_object())?;
        let rest_parts = branches(rest, defs);
        let kinds = types(&rest_parts);
        if kinds.contains(&"string") {
            return Some(ReasonsShape::Text);
        }
        let items = items_schema(Some(rest), defs)?;
        (kinds.contains(&"array") && types(&branches(items, defs)).contains(&"string"))
            .then_some(ReasonsShape::Texts)
    })
}

/// A map of reasons ([`reasons_map`]) saying the walk's facts: each violated field
/// keyed by its name (`address.city`) holding its reason, or, when no violation names
/// a field, the map's own keys holding every reason. `None` when there is nothing to
/// say.
fn keyed_reasons(
    members: &Map<String, Value>,
    shape: ReasonsShape,
    walk: &Walk<'_>,
) -> Option<Value> {
    let mut keyed: Vec<(String, Vec<String>)> = Vec::new();
    for violation in walk.violations {
        let Some(field) = violation.field() else {
            continue;
        };
        match keyed.iter_mut().find(|(name, _)| *name == field) {
            Some((_, reasons)) => reasons.push(violation.reason.clone()),
            None => keyed.push((field, vec![violation.reason.clone()])),
        }
    }
    if keyed.is_empty() {
        if walk.reasons.is_empty() || members.is_empty() {
            return None;
        }
        keyed = members
            .keys()
            .map(|name| (name.clone(), walk.reasons.to_vec()))
            .collect();
    }
    let map = keyed
        .into_iter()
        .map(|(name, reasons)| {
            let value = match shape {
                ReasonsShape::Text => Value::String(reasons.join("; ")),
                ReasonsShape::Texts => {
                    Value::Array(reasons.into_iter().map(Value::String).collect())
                }
            };
            (name, value)
        })
        .collect();
    Some(Value::Object(map))
}

/// Collects the changes that say the walk's facts in an array, at `at`, of objects
/// with a message-like string member and no member naming a violation (JSON:API's
/// `{"errors": [{"status", "title"}]}`): item `i`'s message is violation `i`'s reason
/// (the last one's past them), or, with no violation, every reason.
fn message_items(
    items: &[Value],
    item_schema: Option<&Value>,
    walk: &Walk<'_>,
    at: &str,
    out: &mut Vec<Change>,
) {
    let Some(item_schema) = item_schema else {
        return;
    };
    let declared: Vec<&Map<String, Value>> = branches(item_schema, walk.defs)
        .into_iter()
        .filter_map(|b| b.get("properties").and_then(Value::as_object))
        .collect();
    let message = MESSAGE_FIELDS.iter().find_map(|wanted| {
        let wanted = folded(wanted);
        declared.iter().find_map(|p| {
            p.iter()
                .find(|(name, prop)| {
                    folded(name) == wanted && types(&branches(prop, walk.defs)).contains(&"string")
                })
                .map(|(name, _)| name.clone())
        })
    });
    let Some(message) = message else {
        return;
    };
    let every = walk.reasons.join("; ");
    let last = walk.violations.len().saturating_sub(1);
    for (i, item) in items.iter().enumerate() {
        let text = match walk.violations.get(i.min(last)) {
            Some(violation) => violation.reason.clone(),
            None if !every.is_empty() => every.clone(),
            None => continue,
        };
        if item.get(&message).is_some_and(Value::is_string) {
            let pointer = format!("{at}/{i}/{}", message.replace('~', "~0").replace('/', "~1"));
            out.push((pointer, Value::String(text)));
        }
    }
}

/// How many times the model is asked for an error body whose answer is degenerate
/// ([`body_flaw`]) before the faker's stands.
const WRITE_ASKS: usize = 2;
/// The longest code-like value (in characters) a written error body keeps.
pub(crate) const MAX_CODE_LEN: usize = 64;
/// The longest string member (in characters) a written error body keeps.
pub(crate) const MAX_ERROR_STRING_LEN: usize = 2048;
/// A string at least this long (in characters) is checked for one repeated unit
/// covering most of it ([`MAX_REPEAT_SHARE`]).
pub(crate) const REPEAT_FLOOR: usize = 40;
/// The share of a string (at least [`REPEAT_FLOOR`] long) one unit repeated back to
/// back may cover, in percent.
pub(crate) const MAX_REPEAT_SHARE: usize = 50;
/// How many times a unit is repeated back to back to count as a repeat.
const MIN_REPEATS: usize = 3;
/// The shortest unit (in characters) whose repeats count ([`counted_unit`]).
const MIN_REPEAT_UNIT: usize = 3;
/// How deep the request's values are walked for its echoes ([`request_echoes`]).
const MAX_ECHO_DEPTH: usize = 8;
/// The longest unit (in characters) looked for repeated.
const MAX_REPEAT_UNIT: usize = 256;

/// Why a written error body is not kept ([`body_flaw`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flaw {
    /// It does not validate against the declared error schema.
    Invalid,
    /// A code-like value is longer than [`MAX_CODE_LEN`].
    LongCode,
    /// A code-like value repeats a word-like unit ([`counted_unit`]) back to back.
    RepeatedCode,
    /// A code-like value has a whitespace run, a line break or a tab, or starts or
    /// ends with whitespace.
    SpacedCode,
    /// A string member is longer than [`MAX_ERROR_STRING_LEN`].
    LongString,
    /// One word-like unit ([`counted_unit`]) repeated back to back covers most of a
    /// string member.
    RepeatedString,
}

impl Flaw {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Flaw::Invalid => "schema",
            Flaw::LongCode => "code length",
            Flaw::RepeatedCode => "code repetition",
            Flaw::SpacedCode => "code whitespace",
            Flaw::LongString => "string length",
            Flaw::RepeatedString => "string repetition",
        }
    }
}

/// The backticked spans of `texts` (`` `invalid_parameters` ``): the tokens the
/// contract's prose names, a code-like value among which always passes
/// ([`body_flaw`]).
fn backticked<'a>(texts: impl Iterator<Item = &'a str>) -> Vec<String> {
    texts
        .flat_map(|text| {
            text.split('`')
                .skip(1)
                .step_by(2)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Whether repeats of `unit` count as degenerate: it is at least
/// [`MIN_REPEAT_UNIT`] characters, holds a letter, and is no repeat of a shorter unit
/// itself (`haha`). A run of one character (`000…`, `aaa…`, `...`), of a unit with no
/// letter (`0000-`), or of a two-character unit (`AB` in `0xABABAB`, `ha` in
/// `hahaha`) never counts: zero-filled ids, placeholders, hex codes and quoted input
/// read that way. A degenerate loop repeats a word or more (`invalid_parameters!`).
fn counted_unit(unit: &[char]) -> bool {
    let n = unit.len();
    let primitive = (1..n)
        .filter(|d| n.is_multiple_of(*d))
        .all(|d| unit.chunks(d).any(|c| c != &unit[..d]));
    n >= MIN_REPEAT_UNIT && unit.iter().any(|c| c.is_alphabetic()) && primitive
}

/// The most characters of `chars` one unit covers repeated back to back at least
/// [`MIN_REPEATS`] times (the last time without a separator that ends the unit),
/// over the units (of at most [`MAX_REPEAT_UNIT`] characters) whose repeats count
/// ([`counted_unit`]) and whose run of whole repeats no string of the request holds
/// (`echoes`: the model quoting the call is no loop).
fn repeated_span(chars: &[char], echoes: &[String]) -> usize {
    let n = chars.len();
    let mut best = 0;
    for period in 1..=MAX_REPEAT_UNIT.min((n + 1) / MIN_REPEATS) {
        let mut start = 0;
        let mut run = 0;
        // Each maximal run is judged once, where it ends.
        for i in 0..=n - period {
            if i < n - period && chars[i] == chars[i + period] {
                if run == 0 {
                    start = i;
                }
                run += 1;
                continue;
            }
            let span = run + period;
            if run > 0 && span > best {
                let found = &chars[start..start + period];
                // A unit ending in a separator (`not_found_`) repeats as often when
                // its last separator is left off (`not_found_not_found_not_found`).
                let separated = found.last().is_some_and(|c| !c.is_alphanumeric());
                let repeats =
                    span >= MIN_REPEATS * period || (separated && span + 1 >= MIN_REPEATS * period);
                if repeats && counted_unit(found) && !echoed(found, span, echoes) {
                    best = span;
                }
            }
            run = 0;
        }
    }
    best
}

/// Whether a string the request carries (`echoes`) holds the run of `unit`'s whole
/// repeats within `span` characters.
fn echoed(unit: &[char], span: usize, echoes: &[String]) -> bool {
    if echoes.is_empty() {
        return false;
    }
    let run: String = unit.iter().collect::<String>().repeat(span / unit.len());
    echoes.iter().any(|e| e.contains(&run))
}

/// The strings the request carries, an error body repeating which is an echo, not a
/// loop ([`repeated_span`]): its path and query parameters, its headers (the protocol
/// adapter keeps only the non-sensitive ones) and its body's values, numbers written
/// out.
fn request_echoes(call: &OperationCall) -> Vec<String> {
    fn walk(value: &Value, depth: usize, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Number(n) => out.push(n.to_string()),
            Value::Array(items) if depth < MAX_ECHO_DEPTH => {
                items.iter().for_each(|v| walk(v, depth + 1, out));
            }
            Value::Object(members) if depth < MAX_ECHO_DEPTH => {
                members.values().for_each(|v| walk(v, depth + 1, out));
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for map in [&call.path_params, &call.query, &call.headers] {
        map.values().for_each(|v| walk(v, 0, &mut out));
    }
    if let Some(body) = &call.body {
        walk(body, 0, &mut out);
    }
    out
}

/// Why a code-like `value` is no code, if it is not: too long, a word-like unit
/// ([`counted_unit`]) repeated back to back that the request does not hold
/// (`echoes`), or spaced oddly. A token the contract's prose names (`tokens`) always
/// passes.
fn code_flaw(value: &str, tokens: &[String], echoes: &[String]) -> Option<Flaw> {
    if tokens.iter().any(|t| t == value) {
        return None;
    }
    let chars: Vec<char> = value.chars().collect();
    if chars.len() > MAX_CODE_LEN {
        return Some(Flaw::LongCode);
    }
    let spaced = chars.first().is_some_and(|c| c.is_whitespace())
        || chars.last().is_some_and(|c| c.is_whitespace())
        || chars.iter().any(|c| c.is_whitespace() && *c != ' ')
        || chars
            .windows(2)
            .any(|w| w[0].is_whitespace() && w[1].is_whitespace());
    if spaced {
        return Some(Flaw::SpacedCode);
    }
    (repeated_span(&chars, echoes) > 0).then_some(Flaw::RepeatedCode)
}

/// Why a string member of a written error body is degenerate, if it is: longer than
/// [`MAX_ERROR_STRING_LEN`], or (past [`REPEAT_FLOOR`]) more than
/// [`MAX_REPEAT_SHARE`] percent one word-like unit repeated that the request does not
/// hold (`echoes`, see [`repeated_span`]).
fn string_flaw(value: &str, echoes: &[String]) -> Option<Flaw> {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() > MAX_ERROR_STRING_LEN {
        return Some(Flaw::LongString);
    }
    if chars.len() < REPEAT_FLOOR {
        return None;
    }
    (repeated_span(&chars, echoes) * 100 > chars.len() * MAX_REPEAT_SHARE)
        .then_some(Flaw::RepeatedString)
}

/// Why a written error `body`, declared by `schema`, is not to be kept, if it is not
/// (see the module's documentation): it fails `validator`, a code-like member
/// ([`open_code`]) is no short token ([`code_flaw`]; one of `tokens`, the backticked
/// spans of the contract's prose, always passes), or a string member is degenerate
/// ([`string_flaw`]). A repeat the request holds (`echoes`, [`request_echoes`]) is no
/// flaw.
pub(crate) fn body_flaw(
    body: &Value,
    schema: &Value,
    defs: &Map<String, Value>,
    validator: &SchemaValidator,
    (tokens, echoes): (&[String], &[String]),
) -> Option<Flaw> {
    fn walk(
        value: &Value,
        schema: Option<&Value>,
        defs: &Map<String, Value>,
        (tokens, echoes): (&[String], &[String]),
        depth: usize,
    ) -> Option<Flaw> {
        let known = (tokens, echoes);
        match value {
            Value::String(text) => string_flaw(text, echoes),
            Value::Array(items) => {
                let item_schema = if depth > MAX_DEPTH {
                    None
                } else {
                    items_schema(schema, defs)
                };
                items
                    .iter()
                    .find_map(|item| walk(item, item_schema, defs, known, depth + 1))
            }
            Value::Object(members) => members.iter().find_map(|(name, member)| {
                let prop = if depth > MAX_DEPTH {
                    None
                } else {
                    member_schema(schema, name, defs)
                };
                let code = match member {
                    Value::String(text) if prop.is_some_and(|p| open_code(name, p, defs)) => {
                        code_flaw(text, tokens, echoes)
                    }
                    _ => None,
                };
                code.or_else(|| walk(member, prop, defs, known, depth + 1))
            }),
            _ => None,
        }
    }
    if !validator.is_valid(body) {
        return Some(Flaw::Invalid);
    }
    walk(body, Some(schema), defs, (tokens, echoes), 0)
}

/// What the engine writes an error body with (see [`Engine::error_writable`]).
struct Writable<'a> {
    provider: &'a dyn mb_llm::Provider,
    variant: &'a mb_mcm::OutputVariant,
    schema: &'a Value,
    validator: std::sync::Arc<SchemaValidator>,
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
        self.error_answer_repeating(bundle, op, call, (status, cause), &[])
            .await
    }

    /// [`Self::error_answer`], its body refusing the repeated value of the request
    /// members `repeated` (what the contract names for such a repeat, see
    /// [`crate::contract_values`]).
    pub(crate) async fn error_answer_repeating(
        &self,
        bundle: &Bundle,
        op: &Operation,
        call: &OperationCall,
        (status, cause): (u16, ErrorCause),
        repeated: &[String],
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
        self.fill_contract_values(bundle, op, variant, &mut response, (repeated, only_empty));
        response.usage = usage;
        Some(response)
    }

    /// Whether the error body for `status` and `cause` of `op` is already written (or
    /// its writing failed less than a minute ago and the faker's stands):
    /// answering it asks no LLM. A caller
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
            .is_some_and(|cell| cell.get().is_some_and(|kept| !kept.stale()))
    }

    /// The cell `key`'s body is written into: the kept one, unless it holds a stale
    /// failure ([`Kept::stale`]), which a new one replaces; else a new one, which drops
    /// another body when [`MAX_ERROR_BODIES`] are kept.
    fn error_cell(&self, key: &ErrorKey) -> ErrorCell {
        if let Some(mut cell) = self.error_bodies.get_mut(key) {
            if cell.get().is_some_and(Kept::stale) {
                *cell = ErrorCell::default();
            }
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
        let _ = self.error_cell(&key).set(Kept::now(body));
    }

    #[cfg(test)]
    pub(crate) fn keep_error_body_at(
        &self,
        key: ErrorKey,
        body: Option<Value>,
        at: std::time::Instant,
    ) {
        let _ = self.error_cell(&key).set(Kept { body, at });
    }

    /// Whether this engine can write the error body for `status` of `op` itself, for a
    /// request that fails its schema ([`ErrorCause::Invalid`]; see
    /// [`Self::error_answer`]): an LLM answers, the response is JSON with a code-like
    /// member whose value its schema leaves open ([`writes_code`]), and the schema can
    /// be asked for as it is (not an array root a JSON-mode provider is asked to wrap).
    /// When it cannot, answering that error asks no LLM: a caller admits nothing for it.
    #[must_use]
    pub fn error_writable(&self, bundle: &Bundle, op: &Operation, status: u16) -> bool {
        self.writable(bundle, op, status, ErrorCause::Invalid)
            .is_some()
    }

    /// What [`Self::written_error`] writes with for `cause`: as [`Self::error_writable`]
    /// says, and, for a refusal decided from state ([`ErrorCause::Taken`],
    /// [`ErrorCause::Stale`], [`ErrorCause::Missing`]), also when the schema has a free
    /// text member nothing but the faker would fill ([`writes_text`]).
    fn writable<'a>(
        &'a self,
        bundle: &Bundle,
        op: &'a Operation,
        status: u16,
        cause: ErrorCause,
    ) -> Option<Writable<'a>> {
        let provider = self.provider.as_deref()?;
        let variant = op.output_for_status(status)?;
        if crate::media::is_non_json(op, variant) {
            return None;
        }
        let schema = variant.schema.as_ref()?;
        // A refusal decided from state has no request violation to say: a free text
        // member the faker alone would fill is the model's to write too.
        let from_state = matches!(
            cause,
            ErrorCause::Taken | ErrorCause::Stale | ErrorCause::Missing
        );
        if !writes_code(schema, &bundle.defs)
            && !(from_state && writes_text(schema, &bundle.defs, status))
        {
            return None;
        }
        let validator = self.validator(bundle, op, status, schema).ok()?;
        if crate::guards::items_envelope(provider.capabilities(), validator.schema()) {
            return None;
        }
        Some(Writable {
            provider,
            variant,
            schema,
            validator,
        })
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
        let Writable {
            provider,
            variant,
            schema,
            validator,
        } = self.writable(bundle, op, status, cause)?;
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
                // Kept per operation, status and cause, whatever the call:
                // its repairs are seeded by those alone.
                let error_call = OperationCall {
                    operation_id: call.operation_id.clone(),
                    ..OperationCall::default()
                };
                let checks = Checks {
                    fill: None,
                    echoes: Echoes::of(std::iter::empty()),
                    items_envelope: false,
                    effects_envelope: false,
                    refusals: &[],
                    seed: self.repair_seed(
                        bundle,
                        &format!("{}:{status}:{cause:?}", op.id),
                        [&error_call],
                    ),
                };
                let tokens = backticked(
                    [bundle.description.as_deref(), variant.description.as_deref()]
                        .into_iter()
                        .flatten(),
                );
                // The model is shown the operation alone, never this call's values,
                // so a body repeating one is a coincidence or the contract's example;
                // still, a repeat the call that triggers the write carries is an
                // echo, not a loop. The facts laid over the body later
                // ([`set_message`], [`with_reasons`]) are the call's own and are
                // never checked: the guard sees the model's body alone.
                let echoes = request_echoes(call);
                // A degenerate answer is asked for once more, and never kept: past a
                // second, the faker's body stands, as for a failed write.
                for attempt in 1..=WRITE_ASKS {
                    let answer = self
                        .generate_with_llm(
                            provider,
                            &validator,
                            messages.clone(),
                            &format,
                            &checks,
                            usage,
                        )
                        .await;
                    match answer {
                        Ok(answer) => {
                            match body_flaw(&answer.value, schema, &bundle.defs, &validator, (&tokens, &echoes)) {
                                None => return Kept::now(Some(answer.value)),
                                Some(flaw) => tracing::info!(
                                    operation = %op.id,
                                    status,
                                    ?cause,
                                    attempt,
                                    check = flaw.as_str(),
                                    "a written error body was not kept"
                                ),
                            }
                        }
                        Err(reason) => {
                            tracing::warn!(operation = %op.id, status, %reason, "the error body was not written: the faker's is served");
                            return Kept::now(None);
                        }
                    }
                }
                tracing::warn!(operation = %op.id, status, ?cause, "no written error body passed its checks: the faker's is served");
                Kept::now(None)
            })
            .await
            .body
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
            headers: Vec::new(),
            world_gaps: Vec::new(),
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
    fn free_text_is_a_string_nothing_else_fills() {
        let defs = Map::new();
        let item = |extra: Value| {
            let mut props = json!({
                "status": {"type": "string", "examples": ["422"]},
                "detail": {"type": "string"},
                "source": {"type": "object", "properties": {"pointer": {"type": "string"}}}
            });
            if let (Some(p), Value::Object(more)) = (props.as_object_mut(), extra) {
                p.extend(more);
            }
            json!({"type": "object", "properties": {"errors": {"type": "array", "items": {"type": "object", "properties": props}}}})
        };
        assert!(!writes_text(&item(json!({})), &defs, 422));
        assert!(writes_text(
            &item(json!({"title": {"type": "string"}})),
            &defs,
            422
        ));
        for filled in [
            json!({"kind": {"type": "string", "enum": ["a", "b"]}}),
            json!({"at": {"type": "string", "format": "date-time"}}),
            json!({"field": {"type": "string"}}),
            json!({"count": {"type": "integer"}}),
        ] {
            assert!(!writes_text(&item(filled.clone()), &defs, 422), "{filled}");
        }
        // A status shown as words is the API's own text: the faker says nothing there.
        let worded = json!({"type": "object", "properties": {
            "status": {"type": "string", "examples": ["fail", "error"]},
            "message": {"type": "string"}
        }});
        assert!(writes_text(&worded, &defs, 422));
    }

    #[test]
    fn status_text_is_told_by_its_values_or_pattern_not_its_name() {
        let parts = |schema: &Value| vec![schema.clone()];
        let held = |schema: Value| {
            let p = parts(&schema);
            holds_status_text(&p.iter().collect::<Vec<_>>(), 422)
        };
        assert!(held(json!({"type": "string", "examples": ["404", "422"]})));
        assert!(held(json!({"type": "string", "enum": ["400", "422"]})));
        assert!(held(json!({"type": "string", "pattern": "^[0-9]{3}$"})));
        assert!(!held(
            json!({"type": "string", "examples": ["fail", "error"]})
        ));
        assert!(!held(json!({"type": "string", "pattern": "^.*$"})));
        assert!(!held(json!({"type": "string"})));
        assert!(!held(json!({"type": "integer", "examples": [422]})));
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

    /// Only an array of error objects ([`error_arrays`]) has one violation per item: a
    /// path-like member in the items of another array (one nested in an error item,
    /// or one whose items name no violation themselves) is left as it is.
    #[test]
    fn only_an_array_of_errors_is_filled_per_item() {
        let schema = json!({
            "type": "object",
            "properties": {
                "errors": {"type": "array", "items": {"type": "object", "properties": {
                    "field": {"type": "string"},
                    "message": {"type": "string"},
                    "details": {"type": "array", "items": {"type": "object", "properties": {
                        "field": {"type": "string"}
                    }}}
                }}},
                "links": {"type": "array", "items": {"type": "object", "properties": {
                    "meta": {"type": "object", "properties": {
                        "info": {"type": "object", "properties": {"path": {"type": "string"}}}
                    }}
                }}}
            }
        });
        let validator = SchemaValidator::new(&schema).unwrap();
        let body = json!({
            "errors": [
                {"field": "a", "message": "b", "details": [{"field": "d0"}, {"field": "d1"}]},
                {"field": "a", "message": "b"}
            ],
            "links": [{"meta": {"info": {"path": "l0"}}}, {"meta": {"info": {"path": "l1"}}}]
        });
        let reasons = vec!["r1".to_string(), "r2".to_string()];
        let violations = [violation("/name", "r1"), violation("/size", "r2")];
        let out = with_reasons(&validator, body, &reasons, &violations);
        assert_eq!(out["errors"][0]["field"], "name");
        assert_eq!(out["errors"][1]["field"], "size");
        assert_eq!(out["errors"][0]["details"][0]["field"], "d0", "{out}");
        assert_eq!(out["errors"][0]["details"][1]["field"], "d1", "{out}");
        assert_eq!(out["links"][0]["meta"]["info"]["path"], "l0", "{out}");
        assert_eq!(out["links"][1]["meta"]["info"]["path"], "l1", "{out}");
    }

    /// A flat error shape (`{message, field}`) names one field, but its message keeps
    /// every reason: only an item of an array of errors is one violation's.
    #[test]
    fn a_flat_error_keeps_every_reason_in_its_message() {
        let schema = json!({"type": "object", "properties": {
            "message": {"type": "string"},
            "field": {"type": "string"}
        }});
        let validator = SchemaValidator::new(&schema).unwrap();
        let body = json!({"message": "r1; r2", "field": "x"});
        let reasons = vec!["r1".to_string(), "r2".to_string()];
        let violations = [violation("/name", "r1"), violation("/size", "r2")];
        let out = with_reasons(&validator, body, &reasons, &violations);
        assert_eq!(out["message"], "r1; r2");
        assert_eq!(out["field"], "name");

        // Nested under the root, not in an array: still the whole message.
        let schema = json!({"type": "object", "properties": {"error": schema}});
        let validator = SchemaValidator::new(&schema).unwrap();
        let body = json!({"error": {"message": "r1; r2", "field": "x"}});
        let out = with_reasons(&validator, body, &reasons, &violations);
        assert_eq!(out["error"]["message"], "r1; r2");
        assert_eq!(out["error"]["field"], "name");
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

    /// A JSON pointer always starts with `/`: a pointer member whose example is one
    /// (JSON:API's `source.pointer`) still takes the violated member's pointer, and
    /// its item's message the reason. Only a URI `format` keeps a pointer as it is.
    #[test]
    fn a_pointer_with_a_pointer_example_is_filled() {
        let schema = json!({"type": "object", "properties": {"errors": {"type": "array", "items": {
            "type": "object",
            "properties": {
                "detail": {"type": "string"},
                "source": {"type": "object", "properties": {
                    "pointer": {"type": "string", "example": "/data/attributes/title"}
                }}
            }
        }}}});
        let validator = SchemaValidator::new(&schema).unwrap();
        let body =
            json!({"errors": [{"detail": "d", "source": {"pointer": "/data/attributes/title"}}]});
        let out = with_reasons(
            &validator,
            body,
            &[],
            &[violation("/data/attributes/name", "r")],
        );
        assert_eq!(
            out["errors"][0]["source"]["pointer"],
            "/data/attributes/name"
        );
        assert_eq!(out["errors"][0]["detail"], "r");

        let schema = json!({"type": "object", "properties": {
            "pointer": {"type": "string", "format": "uri-reference"}
        }});
        let validator = SchemaValidator::new(&schema).unwrap();
        let out = with_reasons(
            &validator,
            json!({"pointer": "/x"}),
            &[],
            &[violation("/name", "r")],
        );
        assert_eq!(out["pointer"], "/x");
    }

    /// A `path` or `field` whose example is an absolute path reads as a request URI,
    /// even one of a single step (`/name`, which could be a JSON pointer too): it is
    /// left as it is, which is always valid.
    #[test]
    fn a_path_with_a_one_step_absolute_example_is_left() {
        for name in ["path", "field"] {
            let schema = json!({"type": "object", "properties": {
                name: {"type": "string", "example": "/name"}
            }});
            let validator = SchemaValidator::new(&schema).unwrap();
            let out = with_reasons(
                &validator,
                json!({name: "x"}),
                &[],
                &[violation("/size", "r")],
            );
            assert_eq!(out[name], "x", "{name}");
        }
    }

    /// An array of error objects holds one item per violation: the last item is
    /// repeated for the ones past it, and an empty array gets items made of the
    /// members that name a violation. A growth the schema refuses is not kept.
    #[test]
    fn an_array_of_errors_grows_to_one_item_per_violation() {
        let item = json!({"type": "object", "properties": {
            "code": {"type": "string"},
            "field": {"type": "string"},
            "message": {"type": "string"},
            "source": {"type": "object", "properties": {"pointer": {"type": "string"}}}
        }});
        let schema = json!({"type": "object", "properties": {
            "errors": {"type": "array", "items": item}
        }});
        let validator = SchemaValidator::new(&schema).unwrap();
        let violations = [
            violation("/name", "r1"),
            violation("/size", "r2"),
            violation("/address/city", "r3"),
        ];
        let body = json!({"errors": [{"code": "c", "field": "a", "message": "b", "source": {"pointer": "p"}}]});
        let out = with_reasons(&validator, body, &[], &violations);
        let errors = out["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 3);
        for (i, (field, reason, pointer)) in [
            ("name", "r1", "/name"),
            ("size", "r2", "/size"),
            ("address.city", "r3", "/address/city"),
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(errors[i]["field"], *field);
            assert_eq!(errors[i]["message"], *reason);
            assert_eq!(errors[i]["source"]["pointer"], *pointer);
            assert_eq!(errors[i]["code"], "c");
        }

        // Empty: items made of the naming members, the code left out.
        let out = with_reasons(&validator, json!({"errors": []}), &[], &violations);
        let errors = out["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 3);
        assert_eq!(
            errors[1],
            json!({"field": "size", "message": "r2", "source": {"pointer": "/size"}})
        );

        // A query violation has no pointer: none is made for it.
        let out = with_reasons(
            &validator,
            json!({"errors": []}),
            &[],
            &[located("query", "/limit", "r")],
        );
        assert_eq!(out["errors"], json!([{"field": "limit", "message": "r"}]));

        // maxItems: grown as far as it allows.
        let capped = json!({"type": "object", "properties": {
            "errors": {"type": "array", "maxItems": 2, "items": item}
        }});
        let validator = SchemaValidator::new(&capped).unwrap();
        let body = json!({"errors": [{"field": "a", "message": "b"}]});
        let out = with_reasons(&validator, body, &[], &violations);
        assert_eq!(out["errors"].as_array().unwrap().len(), 2);
        assert_eq!(out["errors"][1]["field"], "size");

        // An item that requires a member no violation names is not made up.
        let strict = json!({"type": "object", "properties": {"errors": {"type": "array", "items": {
            "type": "object",
            "required": ["code"],
            "properties": {"code": {"type": "string"}, "field": {"type": "string"}}
        }}}});
        let validator = SchemaValidator::new(&strict).unwrap();
        let out = with_reasons(&validator, json!({"errors": []}), &[], &violations);
        assert_eq!(out["errors"], json!([]));

        // An array of strings is not an array of error objects.
        let strings = json!({"type": "object", "properties": {
            "errors": {"type": "array", "items": {"type": "string"}}
        }});
        let validator = SchemaValidator::new(&strings).unwrap();
        let out = with_reasons(
            &validator,
            json!({"errors": []}),
            &["r1".into()],
            &violations,
        );
        assert_eq!(out["errors"], json!(["r1"]));
    }

    #[test]
    fn a_map_of_reasons_is_keyed_by_each_violated_field() {
        let texts = json!({"type": "object", "additionalProperties": {
            "type": "array", "items": {"type": "string"}
        }});
        let validator = SchemaValidator::new(&texts).unwrap();
        let violations = [violation("/name", "r1"), violation("/address/city", "r2")];
        let out = with_reasons(&validator, json!({"x": ["y"]}), &[], &violations);
        assert_eq!(out, json!({"name": ["r1"], "address.city": ["r2"]}));
        // No field: the map's own keys hold every reason.
        let out = with_reasons(&validator, json!({"x": ["y"]}), &["r".into()], &[]);
        assert_eq!(out, json!({"x": ["r"]}));

        let text = json!({"type": "object", "additionalProperties": {"type": "string"}});
        let validator = SchemaValidator::new(&text).unwrap();
        let out = with_reasons(&validator, json!({"x": "y"}), &[], &violations[..1]);
        assert_eq!(out, json!({"name": "r1"}));

        // Under a member named as the reasons, too; another map is left.
        let nested = json!({"type": "object", "properties": {
            "errors": texts,
            "meta": {"type": "object", "additionalProperties": {"type": "string"}}
        }});
        let validator = SchemaValidator::new(&nested).unwrap();
        let body = json!({"errors": {"x": ["y"]}, "meta": {"k": "v"}});
        let out = with_reasons(&validator, body, &[], &violations[..1]);
        assert_eq!(out, json!({"errors": {"name": ["r1"]}, "meta": {"k": "v"}}));
    }

    #[test]
    fn error_objects_naming_no_field_say_the_reason() {
        let schema = json!({"type": "object", "properties": {"errors": {"type": "array", "items": {
            "type": "object",
            "properties": {"status": {"type": "string"}, "title": {"type": "string"}}
        }}}});
        let validator = SchemaValidator::new(&schema).unwrap();
        let body =
            json!({"errors": [{"status": "a", "title": "b"}, {"status": "c", "title": "d"}]});
        let out = with_reasons(&validator, body.clone(), &[], &[violation("/name", "r1")]);
        assert_eq!(
            out,
            json!({"errors": [{"status": "a", "title": "r1"}, {"status": "c", "title": "r1"}]})
        );
        let out = with_reasons(&validator, body, &["r1".into(), "r2".into()], &[]);
        assert_eq!(out["errors"][0]["title"], "r1; r2");
    }

    /// A member that holds a URI (problem+json's `type`) or an integer HTTP status
    /// (`status_code`) is no code for the model to write.
    #[test]
    fn a_uri_or_an_http_status_is_no_code() {
        let defs = Map::new();
        let problem = json!({"type": "object", "properties": {
            "type": {"type": "string", "format": "uri"},
            "title": {"type": "string"},
            "status": {"type": "integer"},
            "detail": {"type": "string"}
        }});
        assert!(!writes_code(&problem, &defs));
        let shown = json!({"type": "object", "properties": {
            "type": {"type": "string", "example": "https://example.com/probs/out-of-stock"}
        }});
        assert!(!writes_code(&shown, &defs));
        for name in ["status_code", "statusCode", "http_status_code"] {
            let status = json!({"type": "object", "properties": {
                name: {"type": "integer"},
                "message": {"type": "string"}
            }});
            assert!(!writes_code(&status, &defs), "{name}");
        }
        let text_status =
            json!({"type": "object", "properties": {"status_code": {"type": "string"}}});
        assert!(writes_code(&text_status, &defs));
        let country = json!({"type": "object", "properties": {"country_code": {"type": "string"}}});
        assert!(writes_code(&country, &defs));
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

    /// A failed write is kept for [`FAILED_RETRY`] only: a transient provider error
    /// does not pin the faker's code for the engine's lifetime. A written body stays.
    #[test]
    fn a_failed_write_is_retried_after_a_while() {
        let engine = Engine::new(None, crate::EngineConfig::default());
        let long_ago = std::time::Instant::now()
            .checked_sub(FAILED_RETRY + std::time::Duration::from_secs(1))
            .unwrap();
        let failed: ErrorKey = ("d/failed".into(), 400, ErrorCause::Invalid);
        let fresh: ErrorKey = ("d/fresh".into(), 400, ErrorCause::Invalid);
        let written: ErrorKey = ("d/written".into(), 400, ErrorCause::Invalid);
        engine.keep_error_body_at(failed.clone(), None, long_ago);
        engine.keep_error_body(fresh.clone(), None);
        engine.keep_error_body_at(written.clone(), Some(json!({"code": "x"})), long_ago);
        assert!(!engine.error_kept_by(&failed));
        assert!(engine.error_kept_by(&fresh));
        assert!(engine.error_kept_by(&written));
        // The stale failure's cell is a new one, to be written again.
        assert!(!engine.error_cell(&failed).initialized());
        assert!(engine.error_cell(&written).initialized());
    }

    fn flaw_of(body: &Value, tokens: &[&str]) -> Option<Flaw> {
        flaw_echoing(body, tokens, &[])
    }

    fn flaw_echoing(body: &Value, tokens: &[&str], echoes: &[&str]) -> Option<Flaw> {
        let schema = json!({
            "type": "object",
            "required": ["code"],
            "properties": {
                "code": {"type": "string"},
                "type": {"type": "string", "format": "uri"},
                "message": {"type": "string"},
                "errors": {"type": "array", "items": {"type": "object", "properties": {
                    "error_code": {"type": "string"},
                    "detail": {"type": "string"}
                }}}
            }
        });
        let validator = SchemaValidator::new(&schema).unwrap();
        let tokens: Vec<String> = tokens.iter().map(|t| (*t).to_owned()).collect();
        let echoes: Vec<String> = echoes.iter().map(|t| (*t).to_owned()).collect();
        body_flaw(body, &schema, &Map::new(), &validator, (&tokens, &echoes))
    }

    #[test]
    fn a_body_failing_its_schema_is_not_kept() {
        assert_eq!(flaw_of(&json!({"message": "x"}), &[]), Some(Flaw::Invalid));
        assert_eq!(flaw_of(&json!({"code": 7}), &[]), Some(Flaw::Invalid));
    }

    #[test]
    fn a_short_token_code_is_kept() {
        for code in [
            "invalid_parameters",
            "E000123",
            "ERR-404",
            "resource_missing",
            "Bad Request",
            "banana_split",
        ] {
            assert_eq!(flaw_of(&json!({"code": code}), &[]), None, "{code}");
        }
    }

    #[test]
    fn a_long_code_is_not_kept_unless_the_contract_names_it() {
        let long = "invalid_parameters.required_playback_policies.missing_entries_for_signed";
        assert!(long.len() > MAX_CODE_LEN);
        assert_eq!(flaw_of(&json!({"code": long}), &[]), Some(Flaw::LongCode));
        assert_eq!(flaw_of(&json!({"code": long}), &[long]), None);
        let nested = json!({"code": "x", "errors": [{"error_code": "y".repeat(65)}]});
        assert_eq!(flaw_of(&nested, &[]), Some(Flaw::LongCode));
    }

    #[test]
    fn a_repeating_code_is_not_kept() {
        let looped = "invalid_parameters!".repeat(200);
        assert_eq!(flaw_of(&json!({"code": looped}), &[]), Some(Flaw::LongCode));
        for code in [
            "ERR_ERR_ERR",
            "abcabcabc",
            "not_found_not_found_not_found",
            "invalid_parametersinvalid_parametersinvalid_parameters",
        ] {
            assert_eq!(
                flaw_of(&json!({"code": code}), &[]),
                Some(Flaw::RepeatedCode),
                "{code}"
            );
        }
        // Named by the contract, it is its code.
        assert_eq!(flaw_of(&json!({"code": "abcabcabc"}), &["abcabcabc"]), None);
    }

    #[test]
    fn a_code_repeating_a_short_or_letterless_unit_is_kept() {
        for code in [
            "0xABABAB", "tototo_x", "abababab", "E000000", "1-1-1-1", "ERR__x",
        ] {
            assert_eq!(flaw_of(&json!({"code": code}), &[]), None, "{code}");
        }
    }

    #[test]
    fn a_code_the_request_carries_is_kept() {
        let code = "zipzipzip";
        assert_eq!(
            flaw_of(&json!({"code": code}), &[]),
            Some(Flaw::RepeatedCode)
        );
        assert_eq!(flaw_echoing(&json!({"code": code}), &[], &[code]), None);
    }

    #[test]
    fn an_oddly_spaced_code_is_not_kept() {
        for code in [
            "invalid  parameters",
            " invalid",
            "invalid ",
            "invalid\nparameters",
            "a\tb",
        ] {
            assert_eq!(
                flaw_of(&json!({"code": code}), &[]),
                Some(Flaw::SpacedCode),
                "{code:?}"
            );
        }
    }

    #[test]
    fn a_uri_type_is_no_code() {
        let body =
            json!({"code": "x", "type": "https://example.com/probs/out-of-credit/out-of-credit"});
        assert_eq!(flaw_of(&body, &[]), None);
    }

    #[test]
    fn a_string_one_unit_covers_is_not_kept() {
        let looped = format!("The request is invalid: {}", "again ".repeat(20));
        assert_eq!(
            flaw_of(&json!({"code": "x", "message": looped}), &[]),
            Some(Flaw::RepeatedString)
        );
        let long_loop = "invalid_parameters!".repeat(200);
        assert_eq!(
            flaw_of(&json!({"code": "x", "message": long_loop}), &[]),
            Some(Flaw::LongString)
        );
        let looped = "invalid_parameters!".repeat(100);
        assert_eq!(
            flaw_of(&json!({"code": "x", "errors": [{"detail": looped}]}), &[]),
            Some(Flaw::RepeatedString)
        );
        let word = format!("Unknown value '{}' for kind", "zorblax".repeat(8));
        assert_eq!(
            flaw_echoing(
                &json!({"code": "x", "message": word}),
                &[],
                &["zorbla", "other"]
            ),
            Some(Flaw::RepeatedString)
        );
        // Short strings are not judged.
        assert_eq!(flaw_of(&json!({"code": "x", "message": "aaaa"}), &[]), None);
    }

    #[test]
    fn zero_filled_ids_and_runs_of_one_character_are_kept() {
        let zeros = format!("req_{}", "0".repeat(44));
        let nil = "00000000-0000-0000-0000-000000000000-00000000";
        let dots = format!("Invalid{}", ".".repeat(60));
        let quoted = format!("The name '{}' is too long", "a".repeat(34));
        let laugh = format!("The comment '{}' is not allowed here", "ha".repeat(20));
        for text in [zeros.as_str(), nil, &dots, &quoted, &laugh] {
            assert_eq!(
                flaw_of(
                    &json!({"code": "x", "message": text, "errors": [{"detail": text}]}),
                    &[]
                ),
                None,
                "{text}"
            );
        }
        assert_eq!(
            flaw_of(&json!({"code": "x", "request_id": zeros}), &[]),
            None
        );
    }

    #[test]
    fn a_string_quoting_the_request_is_kept() {
        let value = "zorblax".repeat(8);
        let message = format!("Unknown value '{value}' for kind");
        let body = json!({"code": "x", "message": message});
        assert_eq!(flaw_of(&body, &[]), Some(Flaw::RepeatedString));
        assert_eq!(flaw_echoing(&body, &[], &[&value]), None);
    }

    #[test]
    fn the_request_echoes_are_its_strings_and_numbers() {
        let call = OperationCall {
            operation_id: "op".into(),
            path_params: Map::from_iter([("id".to_owned(), json!("p1"))]),
            query: Map::from_iter([("tag".to_owned(), json!(["q1", "q2"]))]),
            headers: Map::from_iter([("x-trace".to_owned(), json!("h1"))]),
            body: Some(json!({"name": "b1", "nested": {"n": 7, "on": true}})),
        };
        let mut echoes = request_echoes(&call);
        echoes.sort();
        assert_eq!(echoes, ["7", "b1", "h1", "p1", "q1", "q2"]);
    }

    #[test]
    fn a_very_long_string_is_not_kept() {
        let words: String = (0..600).map(|i| format!("w{i} ")).collect();
        assert!(words.chars().count() > MAX_ERROR_STRING_LEN);
        assert_eq!(
            flaw_of(&json!({"code": "x", "message": words}), &[]),
            Some(Flaw::LongString)
        );
    }

    #[test]
    fn a_long_message_that_does_not_repeat_is_kept() {
        let message = "The request body is invalid: `title` must be a string of at most 120 \
characters, `duration` must be a positive integer number of seconds, and `visibility` must be \
one of public, unlisted or private. See the API reference for the full list of accepted values \
for each field, and retry the request once every listed member is corrected.";
        assert!(message.len() > 300);
        assert_eq!(
            flaw_of(
                &json!({"code": "invalid_parameters", "message": message}),
                &[]
            ),
            None
        );
    }

    #[test]
    fn backticked_spans_are_the_contracts_tokens() {
        let tokens = backticked(
            [
                "Codes: `invalid_parameters`, `not_found` and ` `.",
                "`a.b` only",
            ]
            .into_iter(),
        );
        assert_eq!(tokens, ["invalid_parameters", "not_found", "a.b"]);
    }
}
