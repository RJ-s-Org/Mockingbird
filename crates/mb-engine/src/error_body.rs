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
//!   ([`crate::Engine::error_answer`]); the call's facts are then laid over it. Offline,
//!   or when the model's answer fails, the faker's body stands (a failed write is
//!   asked for again after a minute).

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
                let kinds = types(&prop_parts);
                let scalar = kinds
                    .iter()
                    .any(|t| matches!(*t, "string" | "integer" | "number"));
                // A URI (problem+json's `type`) or an integer HTTP status
                // (`status_code`) is no code the contract's prose says.
                let uri =
                    uri_format(&prop_parts) || shown_values(&prop_parts).any(|v| v.contains("://"));
                let http_status =
                    folded(name).ends_with("statuscode") && !kinds.contains(&"string");
                (code_like(name) && scalar && !fixed(&prop_parts) && !uri && !http_status)
                    || walk(prop, defs, depth + 1)
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
/// reason in that item's message-like member. Outside such an array, the first
/// violation names the field and the message-like member keeps every reason. Each change is kept only while the body
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

    /// Whether this engine can write the error body for `status` of `op` itself (see
    /// [`Self::error_answer`]): an LLM answers, the response is JSON with a code-like
    /// member whose value its schema leaves open ([`writes_code`]), and the schema can
    /// be asked for as it is (not an array root a JSON-mode provider is asked to wrap).
    /// When it cannot, answering that error asks no LLM: a caller admits nothing for it.
    #[must_use]
    pub fn error_writable(&self, bundle: &Bundle, op: &Operation, status: u16) -> bool {
        self.writable(bundle, op, status).is_some()
    }

    /// What [`Self::written_error`] writes with, when [`Self::error_writable`].
    fn writable<'a>(
        &'a self,
        bundle: &Bundle,
        op: &'a Operation,
        status: u16,
    ) -> Option<Writable<'a>> {
        let provider = self.provider.as_deref()?;
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
        } = self.writable(bundle, op, status)?;
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
                        provider,
                        &validator,
                        messages,
                        &format,
                        &checks,
                        usage,
                    )
                    .await;
                match answer {
                    Ok(answer) => Kept::now(Some(answer.value)),
                    Err(reason) => {
                        tracing::warn!(operation = %op.id, status, %reason, "the error body was not written: the faker's is served");
                        Kept::now(None)
                    }
                }
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
}
