//! What a world description leaves open.
//!
//! [`gaps`] lists, for a bundle and its effective (validated) world
//! description, what the description does not say that a world needs: types
//! written to with no identity, operations on one record with no target,
//! create and update answers whose record could not be found, HTTP routes
//! the contract has but that are not served, and entries refused at
//! validation. A gap never makes a mock refuse a request; it tells an
//! assistant or a person what to describe.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use mb_mcm::world::{OpDesc, OpKind, Refusal, World};
use mb_mcm::{Bundle, Operation};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One thing a world description leaves open.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    /// What kind of gap it is.
    pub code: GapCode,
    /// The operation it is about, when it is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// The resource type it is about, when it is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// What is missing, for a person or an assistant to fix.
    pub message: String,
}

/// What kind of gap a [`Gap`] is.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GapCode {
    /// A type that operations write to has no `identity`.
    NoIdentity,
    /// A read, update, replace, delete or action on a type has no `target`.
    NoTarget,
    /// A create, update or replace answers an object in which no member fits
    /// the type, and the description has no `answer.record`.
    NoRecord,
    /// An HTTP route of the contract that is not served (it matches the same
    /// requests as an earlier one, or is not a valid route pattern).
    SkippedRoute,
    /// An entry of the description refused at validation.
    Refused,
    /// An action (by the description or the contract's route) that the
    /// description gives nothing to change: no `changes`, `transition` or
    /// `queue`.
    NoEffect,
    /// A list under path parameters with no `scope`.
    UnscopedList,
    /// A list query parameter the description does not mention.
    UndescribedParam,
    /// A type whose create can answer `409` with no `unique`.
    NoUnique,
    /// A type whose writes take `If-Match` or can answer `412` with no
    /// `concurrency`.
    NoConcurrency,
    /// A deleted type whose record has a deleted or archived member, with no
    /// `lifecycle.soft_delete`.
    NoSoftDelete,
}

impl GapCode {
    /// The serialized name, e.g. `no_identity`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoIdentity => "no_identity",
            Self::NoTarget => "no_target",
            Self::NoRecord => "no_record",
            Self::SkippedRoute => "skipped_route",
            Self::Refused => "refused",
            Self::NoEffect => "no_effect",
            Self::UnscopedList => "unscoped_list",
            Self::UndescribedParam => "undescribed_param",
            Self::NoUnique => "no_unique",
            Self::NoConcurrency => "no_concurrency",
            Self::NoSoftDelete => "no_soft_delete",
        }
    }
}

impl fmt::Display for GapCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The gaps of `world`, the effective description of `bundle`, with
/// `refused` the entries its validation refused. In order: types with no
/// identity (by name); then, in contract order, operations with no target or
/// no record; skipped HTTP routes; refused entries, in document order.
///
/// - [`GapCode::NoIdentity`]: a type an operation creates, updates,
///   replaces, deletes or acts on, with no `identity`.
/// - [`GapCode::NoTarget`]: a read, update, replace, delete or action with
///   a type and no `target`.
/// - [`GapCode::NoRecord`]: a create, update or replace with a type whose
///   identity member is known, whose success answer is an object that
///   neither has that member nor a property that holds it or is named after
///   the type, and with no `answer.record`, `answer.records` or
///   `answer.long_running`. A type with no identity is reported once as
///   [`GapCode::NoIdentity`] instead.
/// - [`GapCode::SkippedRoute`]: one per route `RouteTable::new(bundle)`
///   skips, with its message (only with the `http-rest` feature).
/// - [`GapCode::Refused`]: one per refusal, naming the operation or type
///   its path is under.
///
/// Then the behaviour the contract signals and the description does not
/// state, in contract order (each a pointer to what to read in the docs, never
/// a claim about the API):
/// - [`GapCode::NoEffect`]: an operation that is an action by its kind or
///   by the contract's route, other than a create, read, list or delete,
///   with no `changes`, `transition` or `queue`.
/// - [`GapCode::UnscopedList`]: a list whose route has path parameters and
///   that has no `scope`.
/// - [`GapCode::UndescribedParam`]: a list's query parameters that appear
///   nowhere in its description and are not members of its items, once per
///   list.
/// - [`GapCode::NoUnique`]: a type a create writes that can answer `409`,
///   with no `unique`; once per type.
/// - [`GapCode::NoConcurrency`]: a type an update, replace, delete or action
///   writes that takes an `If-Match` header or can answer `412`, with no
///   `concurrency`; once per type.
/// - [`GapCode::NoSoftDelete`]: a type a delete removes whose record has a
///   member such as `deleted`, `is_archived` or `deleted_at`, with no
///   `lifecycle.soft_delete`; once per type.
#[must_use]
pub fn gaps(bundle: &Bundle, world: &World, refused: &[Refusal]) -> Vec<Gap> {
    let mut out = Vec::new();
    let none = OpDesc::default();
    let desc = |op: &Operation| world.operations.get(&op.id).unwrap_or(&none);
    let kind = |op: &Operation| desc(op).kind.or_else(|| OpKind::of(op));
    let ty = |op: &Operation| desc(op).r#type.clone().or_else(|| op.resource_type.clone());

    let mut writes: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for op in &bundle.operations {
        if let (Some(t), Some(k)) = (ty(op), kind(op))
            && is_write(k)
        {
            writes.entry(t).or_default().push(&op.id);
        }
    }
    for (t, ops) in &writes {
        if world
            .types
            .get(t)
            .and_then(|d| d.identity.as_ref())
            .is_none()
        {
            out.push(Gap {
                code: GapCode::NoIdentity,
                operation: None,
                r#type: Some(t.clone()),
                message: format!(
                    "type `{t}` is written by {} but has no identity: no member is known to name a record",
                    list(ops)
                ),
            });
        }
    }

    for op in &bundle.operations {
        let (Some(t), Some(k)) = (ty(op), kind(op)) else {
            continue;
        };
        let d = desc(op);
        let targets = matches!(
            k,
            OpKind::Read | OpKind::Update | OpKind::Replace | OpKind::Delete | OpKind::Action
        );
        let targeted = d
            .target
            .as_ref()
            .is_some_and(|t| t.path_param.is_some() || t.member.is_some());
        if targets && !targeted {
            out.push(Gap {
                code: GapCode::NoTarget,
                operation: Some(op.id.clone()),
                r#type: Some(t.clone()),
                message: format!(
                    "operation `{}` ({k:?}) acts on one `{t}` record but no target says where the request names it",
                    op.id
                ),
            });
        }
        let answers_record = matches!(k, OpKind::Create | OpKind::Update | OpKind::Replace);
        let located = d
            .answer
            .as_ref()
            .is_some_and(|a| a.record.is_some() || a.records.is_some() || a.long_running.is_some());
        if answers_record && !located {
            let member = world
                .types
                .get(&t)
                .and_then(|d| d.identity.as_ref())
                .and_then(|i| i.member.as_deref());
            if let (Some(member), Some(answer)) = (member, success_schema(op))
                && is_object(bundle, answer)
                && !fits(bundle, answer, &t, member)
            {
                out.push(Gap {
                    code: GapCode::NoRecord,
                    operation: Some(op.id.clone()),
                    r#type: Some(t.clone()),
                    message: format!(
                        "operation `{}` answers an object with no `{member}` and no member holding a `{t}` record; set answer.record to where it is",
                        op.id
                    ),
                });
            }
        }
    }

    skipped_routes(bundle, &ty, &mut out);

    for r in refused {
        let tokens: Vec<String> = r
            .path
            .split('/')
            .skip(1)
            .map(|t| t.replace("~1", "/").replace("~0", "~"))
            .collect();
        let (operation, r#type) = match tokens.as_slice() {
            [root, name, ..] if root == "operations" => (Some(name.clone()), None),
            [root, name, ..] if root == "types" => (None, Some(name.clone())),
            _ => (None, None),
        };
        out.push(Gap {
            code: GapCode::Refused,
            operation,
            r#type,
            message: r.to_string(),
        });
    }
    behaviour_gaps(bundle, world, &desc, &kind, &ty, &mut out);
    out
}

/// Member names a record keeps when a delete only marks it.
const SOFT_DELETE_MEMBERS: &[&str] = &[
    "deleted",
    "is_deleted",
    "isdeleted",
    "deleted_at",
    "deletedat",
    "archived",
    "is_archived",
    "isarchived",
    "archived_at",
    "archivedat",
    "trashed",
    "is_trashed",
];

/// The gaps of behaviour the contract signals that `world` does not state.
/// See [`gaps`].
fn behaviour_gaps<'a>(
    bundle: &'a Bundle,
    world: &World,
    desc: &dyn Fn(&Operation) -> &'a OpDesc,
    kind: &dyn Fn(&Operation) -> Option<OpKind>,
    ty: &dyn Fn(&Operation) -> Option<String>,
    out: &mut Vec<Gap>,
) where
    World: 'a,
{
    let mut seen: BTreeSet<(&'static str, String)> = BTreeSet::new();
    let mut once = |code: GapCode, t: &str| seen.insert((code.as_str(), t.to_owned()));
    for op in &bundle.operations {
        let (Some(t), Some(k)) = (ty(op), kind(op)) else {
            continue;
        };
        let d = desc(op);
        let td = world.types.get(&t);
        let by_route = op.is_action();
        if (k == OpKind::Action || by_route)
            && !matches!(
                k,
                OpKind::Create | OpKind::Read | OpKind::List | OpKind::Delete
            )
            && d.changes.is_empty()
            && d.transition.is_none()
            && d.queue.is_none()
        {
            out.push(Gap {
                code: GapCode::NoEffect,
                operation: Some(op.id.clone()),
                r#type: Some(t.clone()),
                message: format!(
                    "operation `{}` is an action on `{t}` and the description gives it nothing to change: if the docs say what it does to the record, write `changes` (members it sets, from the request or a value) or a `transition` of `{t}`'s lifecycle, with kind `action`",
                    op.id
                ),
            });
        }
        if k == OpKind::List {
            let path_params = names(bundle, op.input.path.as_ref());
            if !path_params.is_empty() && d.scope.is_none() {
                out.push(Gap {
                    code: GapCode::UnscopedList,
                    operation: Some(op.id.clone()),
                    r#type: Some(t.clone()),
                    message: format!(
                        "list `{}` sits under {} but has no `scope`: say which member of `{t}` holds the parent it lists, or every `{t}` of the session is listed",
                        op.id,
                        quoted(&path_params)
                    ),
                });
            }
            let mentioned = strings(&serde_json::to_value(d).unwrap_or(Value::Null));
            let members = item_members(bundle, op);
            let open: Vec<String> = names(bundle, op.input.query.as_ref())
                .into_iter()
                .filter(|p| !mentioned.contains(p.as_str()) && !members.contains(p.as_str()))
                .collect();
            if !open.is_empty() {
                out.push(Gap {
                    code: GapCode::UndescribedParam,
                    operation: Some(op.id.clone()),
                    r#type: Some(t.clone()),
                    message: format!(
                        "list `{}` takes {} that the description does not mention: a request that sends one is answered by the model, not from the records. Describe each as a filter, the scope, order, paging or fields as the docs define it, or list it in `ignores` when it asks nothing of the items",
                        op.id,
                        quoted(&open)
                    ),
                });
            }
        }
        if k == OpKind::Create
            && answers(op, 409)
            && td.is_none_or(|td| td.unique.is_empty())
            && once(GapCode::NoUnique, &t)
        {
            out.push(Gap {
                code: GapCode::NoUnique,
                operation: Some(op.id.clone()),
                r#type: Some(t.clone()),
                message: format!(
                    "creating `{t}` (`{}`) can answer 409 but `{t}` has no `unique`: write the member sets the docs say must be unique, with that conflict status",
                    op.id
                ),
            });
        }
        if matches!(
            k,
            OpKind::Update | OpKind::Replace | OpKind::Delete | OpKind::Action
        ) && (answers(op, 412)
            || names(bundle, op.input.headers.as_ref())
                .iter()
                .any(|h| h.eq_ignore_ascii_case("if-match")))
            && td.is_none_or(|td| td.concurrency.is_none())
            && once(GapCode::NoConcurrency, &t)
        {
            out.push(Gap {
                code: GapCode::NoConcurrency,
                operation: Some(op.id.clone()),
                r#type: Some(t.clone()),
                message: format!(
                    "`{}` takes If-Match or can answer 412 but `{t}` has no `concurrency`: say which member or header holds the version or ETag the docs compare",
                    op.id
                ),
            });
        }
        if k == OpKind::Delete
            && td
                .and_then(|td| td.lifecycle.as_ref())
                .is_none_or(|l| l.soft_delete.is_none())
            && let Some(member) = record_members(bundle, &t, ty)
                .into_iter()
                .find(|m| SOFT_DELETE_MEMBERS.contains(&m.to_ascii_lowercase().as_str()))
            && once(GapCode::NoSoftDelete, &t)
        {
            out.push(Gap {
                code: GapCode::NoSoftDelete,
                operation: Some(op.id.clone()),
                r#type: Some(t.clone()),
                message: format!(
                    "`{}` deletes `{t}`, whose records have `{member}`: if the docs say a delete only marks the record, write `lifecycle` with `soft_delete` (and `listed_with` when a list parameter still shows such records); otherwise leave it",
                    op.id
                ),
            });
        }
    }
}

/// The property names of an input schema (path, query or headers).
fn names(b: &Bundle, schema: Option<&Value>) -> Vec<String> {
    schema
        .map(|s| {
            properties(b, s)
                .into_iter()
                .map(|(k, _)| k.to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Every string anywhere in `v`.
fn strings(v: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![v];
    while let Some(v) = stack.pop() {
        match v {
            Value::String(s) => {
                out.insert(s.trim_start_matches('/').to_owned());
            }
            Value::Array(a) => stack.extend(a),
            Value::Object(o) => stack.extend(o.values()),
            _ => {}
        }
    }
    out
}

/// The members of a list's items: its success answer's array items, or the
/// items of an array property of it.
fn item_members(b: &Bundle, op: &Operation) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(answer) = success_schema(op) {
        let resolved = b.resolve(answer);
        let mut arrays = vec![resolved];
        arrays.extend(properties(b, answer).into_iter().map(|(_, s)| b.resolve(s)));
        for a in arrays {
            if a.get("items").is_some_and(Value::is_object) {
                out.extend(
                    properties(b, item(b, a))
                        .into_iter()
                        .map(|(k, _)| k.to_owned()),
                );
            }
        }
    }
    out
}

/// The members of a `t` record: the success answer of a read of `t`, else of
/// a create.
fn record_members(b: &Bundle, t: &str, ty: &dyn Fn(&Operation) -> Option<String>) -> Vec<String> {
    for want in [OpKind::Read, OpKind::Create] {
        for op in &b.operations {
            if ty(op).as_deref() == Some(t)
                && OpKind::of(op) == Some(want)
                && let Some(answer) = success_schema(op)
            {
                let props = properties(b, answer);
                if !props.is_empty() {
                    return props.into_iter().map(|(k, _)| k.to_owned()).collect();
                }
            }
        }
    }
    Vec::new()
}

/// Whether `op` declares the status `code`.
fn answers(op: &Operation, code: u16) -> bool {
    op.outputs
        .iter()
        .any(|o| matches!(o.status, mb_mcm::StatusSelector::Code(c) if c == code))
}

fn quoted(names: &[String]) -> String {
    let q: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    match q.len() {
        1 => format!("the parameter {}", q[0]),
        _ => format!("the parameters {}", q.join(", ")),
    }
}

#[cfg(feature = "http-rest")]
fn skipped_routes(bundle: &Bundle, ty: &dyn Fn(&Operation) -> Option<String>, out: &mut Vec<Gap>) {
    for e in crate::http_rest::RouteTable::new(bundle).skipped() {
        out.push(Gap {
            code: GapCode::SkippedRoute,
            operation: Some(e.operation().to_owned()),
            r#type: bundle.operation(e.operation()).and_then(ty),
            message: e.to_string(),
        });
    }
}

#[cfg(not(feature = "http-rest"))]
fn skipped_routes(_: &Bundle, _: &dyn Fn(&Operation) -> Option<String>, _: &mut Vec<Gap>) {}

fn is_write(k: OpKind) -> bool {
    matches!(
        k,
        OpKind::Create | OpKind::Update | OpKind::Replace | OpKind::Delete | OpKind::Action
    )
}

fn list(ops: &[&str]) -> String {
    let mut names: Vec<String> = ops.iter().take(3).map(|o| format!("`{o}`")).collect();
    if ops.len() > 3 {
        names.push(format!("{} more", ops.len() - 3));
    }
    names.join(", ")
}

fn success_schema(op: &Operation) -> Option<&Value> {
    op.success_output().and_then(|o| o.schema.as_ref())
}

/// How deep `$ref`s, compositions and nested lookups are followed.
const MAX_HOPS: usize = 16;

/// The properties of `schema`, through `$ref`s and `allOf`, `anyOf` and
/// `oneOf`, each schema visited once.
fn properties<'a>(b: &'a Bundle, schema: &'a Value) -> Vec<(&'a str, &'a Value)> {
    fn walk<'a>(
        b: &'a Bundle,
        schema: &'a Value,
        seen: &mut BTreeSet<*const Value>,
        depth: usize,
        out: &mut Vec<(&'a str, &'a Value)>,
    ) {
        let schema = b.resolve(schema);
        if depth > MAX_HOPS || !seen.insert(std::ptr::from_ref(schema)) {
            return;
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            out.extend(props.iter().map(|(k, v)| (k.as_str(), v)));
        }
        for key in ["allOf", "anyOf", "oneOf"] {
            for branch in schema
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                walk(b, branch, seen, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(b, schema, &mut BTreeSet::new(), 0, &mut out);
    out
}

/// Whether `schema` describes an object with properties.
fn is_object(b: &Bundle, schema: &Value) -> bool {
    !properties(b, schema).is_empty()
}

/// Whether `schema` has the member `member`: a property with that whole name,
/// or else the nested path its dots walk.
fn has_member(b: &Bundle, schema: &Value, member: &str) -> bool {
    // A property named with the whole member (e.g. `odata.id`) is that member,
    // as validation and `member_at` read it; the dotted walk comes second.
    if properties(b, schema).into_iter().any(|(k, _)| k == member) {
        return true;
    }
    let mut at = schema;
    for name in member.split('.') {
        match properties(b, at).into_iter().find(|(k, _)| *k == name) {
            Some((_, s)) => at = s,
            None => return false,
        }
    }
    true
}

/// The schema of one item of an array `schema`, or `schema` itself.
fn item<'a>(b: &'a Bundle, schema: &'a Value) -> &'a Value {
    let resolved = b.resolve(schema);
    resolved
        .get("items")
        .filter(|i| i.is_object())
        .unwrap_or(resolved)
}

/// Whether an answer object holds a record of type `t`: it has the identity
/// member itself, or one of its properties (or their items) does, or a
/// property is named after the type.
fn fits(b: &Bundle, answer: &Value, t: &str, member: &str) -> bool {
    if has_member(b, answer, member) {
        return true;
    }
    let t = normalize(t);
    properties(b, answer)
        .into_iter()
        .any(|(name, schema)| normalize(name) == t || has_member(b, item(b, schema), member))
}

/// Lowercased, without `_` and `-`, and without one trailing `s`.
fn normalize(name: &str) -> String {
    let mut n: String = name
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect();
    if n.ends_with('s') && !n.ends_with("ss") {
        n.pop();
    }
    n
}
