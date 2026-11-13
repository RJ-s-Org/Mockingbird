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

use mb_mcm::world::{IdentityDesc, OpDesc, OpKind, Refusal, World, apply, tier0};
use mb_mcm::{Bundle, Operation, OperationKind};
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
    /// Whether it is a check, not something missing: behaviour the contract hints
    /// at, to describe only when the docs state it ([`GapCode::is_advisory`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub advisory: bool,
}

/// What kind of gap a [`Gap`] is.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GapCode {
    /// A type that operations name has no identity their calls can use
    /// ([`identified`]), so the model answers those calls.
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
    /// An action the description gives nothing to change: no `changes`,
    /// `transition` or `queue`. Advisory.
    NoEffect,
    /// A list query parameter the executor reads nothing for: a request that
    /// sends it is answered by the model.
    UndescribedParam,
    /// A type whose create can answer `409` with no `unique`. Advisory.
    NoUnique,
    /// A deleted type whose record has a deleted or archived member, with no
    /// `lifecycle.soft_delete`. Advisory.
    NoSoftDelete,
    /// A type with no `lifecycle` whose record has a state member: its values
    /// listed, set by an update's or action's body and not by the create.
    NoLifecycle,
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
            Self::UndescribedParam => "undescribed_param",
            Self::NoUnique => "no_unique",
            Self::NoSoftDelete => "no_soft_delete",
            Self::NoLifecycle => "no_lifecycle",
        }
    }

    /// Whether gaps of this code are checks rather than something missing: the
    /// contract hints at a behaviour (an action, a `409`, a deleted flag) that
    /// the docs may or may not state, so the gap stays when they do not.
    #[must_use]
    pub fn is_advisory(self) -> bool {
        matches!(self, Self::NoEffect | Self::NoUnique | Self::NoSoftDelete)
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
/// - [`GapCode::NoIdentity`]: a type named by an operation whose calls have no
///   identity to work with ([`identified`]), once per type: the executor
///   answers none of those calls, the model does (`exec_fallback: no_identity`
///   in the request log).
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
/// a claim about the API). The advisory ones ([`GapCode::is_advisory`]) are
/// checks: they stay when the docs do not state the behaviour.
/// - [`GapCode::NoEffect`] (advisory): an action with no `changes`,
///   `transition` or `queue`.
/// - [`GapCode::UndescribedParam`]: a list's query parameters the executor
///   reads nothing for, once per list (see `call::undescribed_params`).
/// - [`GapCode::NoUnique`] (advisory): a type a create writes that can answer
///   `409`, with no `unique`; once per type.
/// - [`GapCode::NoSoftDelete`] (advisory): a type a delete removes whose
///   record has a member such as `deleted`, `is_trashed` or `deleted_at`,
///   with no `lifecycle.soft_delete`; once per type.
/// - [`GapCode::NoLifecycle`]: a type with no `lifecycle` whose record has a
///   state member: a string member whose values the contract lists (an
///   `enum`, or a description listing them as code spans), that no create of
///   the type sets (its body lacks it, or marks it `readOnly`) and that the
///   body of an update, replace or action of the type sets, a body naming at
///   most half of the record's members; once per type, at the operation that
///   sets it (an action's before an update's).
#[must_use]
pub fn gaps(bundle: &Bundle, world: &World, refused: &[Refusal]) -> Vec<Gap> {
    let mut out = structural_gaps(bundle, world, refused);
    behaviour_gaps(bundle, world, &mut out);
    out
}

/// The gaps of [`gaps`] up to the refused entries: `no_identity`, `no_target`,
/// `no_record`, `skipped_route` and `refused`, without the behaviour the contract
/// signals (what a runtime marks its calls with).
#[must_use]
pub fn structural_gaps(bundle: &Bundle, world: &World, refused: &[Refusal]) -> Vec<Gap> {
    let mut out = Vec::new();
    let none = OpDesc::default();
    let desc = |op: &Operation| world.operations.get(&op.id).unwrap_or(&none);
    let kind = |op: &Operation| desc(op).kind.or_else(|| OpKind::of(op));
    let ty = |op: &Operation| desc(op).r#type.clone().or_else(|| op.resource_type.clone());

    // Judged on the bundle as the runtime holds it, with `world` written into it.
    let applied = apply(bundle, &tier0(bundle), world);
    for (t, ops) in &unidentified(&applied, world) {
        out.push(Gap {
            code: GapCode::NoIdentity,
            operation: None,
            r#type: Some(t.clone()),
            message: format!(
                "type `{t}` is named by {} but has no identity: no member is known to name a record, so their calls are answered by the model",
                list(ops)
            ),
            advisory: false,
        });
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
                advisory: false,
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
                    advisory: false,
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
            advisory: false,
        });
    }
    out
}

/// Member names a record keeps when a delete only marks it.
pub(crate) const SOFT_DELETE_MEMBERS: &[&str] = &[
    "deleted",
    "is_deleted",
    "isdeleted",
    "deleted_at",
    "deletedat",
    "trashed",
    "is_trashed",
    "istrashed",
    "trashed_at",
    "trashedat",
];

/// The gaps of behaviour the contract signals that `world` does not state.
/// See [`gaps`].
fn behaviour_gaps(bundle: &Bundle, world: &World, out: &mut Vec<Gap>) {
    let none = OpDesc::default();
    let desc = |op: &Operation| world.operations.get(&op.id).unwrap_or(&none);
    let kind = |op: &Operation| desc(op).kind.or_else(|| OpKind::of(op));
    let ty = |op: &Operation| desc(op).r#type.clone().or_else(|| op.resource_type.clone());
    let mut seen: BTreeSet<(&'static str, String)> = BTreeSet::new();
    for op in &bundle.operations {
        let (Some(t), Some(k)) = (ty(op), kind(op)) else {
            continue;
        };
        let d = desc(op);
        let td = world.types.get(&t);
        if k == OpKind::Action
            && d.changes.is_empty()
            && d.set.is_empty()
            && d.transition.is_none()
            && d.queue.is_none()
        {
            out.push(Gap {
                code: GapCode::NoEffect,
                operation: Some(op.id.clone()),
                r#type: Some(t.clone()),
                message: format!(
                    "action `{}` on `{t}` is given nothing to change: if the docs say what it does to the record, write its `set` (members it sets, each an expression over the request, the record and the session) or a `transition` of `{t}`'s lifecycle; an action the docs say changes nothing (a send, a test) needs neither",
                    op.id
                ),
                advisory: true,
            });
        }
        if k == OpKind::List {
            let open = crate::call::undescribed_params(bundle, world, op);
            if !open.is_empty() {
                out.push(Gap {
                    code: GapCode::UndescribedParam,
                    operation: Some(op.id.clone()),
                    r#type: Some(t.clone()),
                    message: format!(
                        "list `{}` reads nothing for {}: a request that sends one with a value that asks something is answered by the model, not from the records. Describe each as the docs define it: a filter, the scope, or the paging's parameter; list it in `ignores` when it asks nothing of the items. A sort parameter has no description yet: leave it",
                        op.id,
                        quoted(&open)
                    ),
                    advisory: false,
                });
            }
        }
        if k == OpKind::Create
            && answers(op, 409)
            && td.is_none_or(|td| td.unique.is_empty())
            && seen.insert((GapCode::NoUnique.as_str(), t.clone()))
        {
            out.push(Gap {
                code: GapCode::NoUnique,
                operation: Some(op.id.clone()),
                r#type: Some(t.clone()),
                message: format!(
                    "creating `{t}` (`{}`) can answer 409 and `{t}` has no `unique`: if the docs say which members must be unique, write them with that conflict status; otherwise leave it",
                    op.id
                ),
                advisory: true,
            });
        }
        if td.is_none_or(|td| td.lifecycle.is_none())
            && seen.insert((GapCode::NoLifecycle.as_str(), t.clone()))
            && let Some(state) = state_member(bundle, &t, &ty, &kind)
        {
            let listed: Vec<String> = state.values.iter().map(|v| format!("`{v}`")).collect();
            let member = &state.member;
            let now = if state.enumerated {
                format!("a record's `{member}` is any of them, drawn at random")
            } else {
                format!("the schema does not enumerate them, so a record's `{member}` is free text")
            };
            out.push(Gap {
                code: GapCode::NoLifecycle,
                operation: Some(state.operation.clone()),
                r#type: Some(t.clone()),
                message: format!(
                    "`{t}` records have `{member}`, a state: the contract lists its values ({}), `{}` sets it and creating a `{t}` does not. With no `lifecycle`, {now}, and nothing says which state a created record starts in or which state each operation moves it to. Write `{t}`'s `lifecycle` with `member: {member}`, its `states` and the `initial` state of a created record (the docs say which), and a `transition` on each action that moves it",
                    listed.join(", "),
                    state.operation
                ),
                advisory: false,
            });
        }
        if k == OpKind::Delete
            && td
                .and_then(|td| td.lifecycle.as_ref())
                .is_none_or(|l| l.soft_delete.is_none())
            && !seen.contains(&(GapCode::NoSoftDelete.as_str(), t.clone()))
            && let Some(member) = record_members(bundle, &t, &ty, &kind)
                .into_iter()
                .find(|m| SOFT_DELETE_MEMBERS.contains(&m.to_ascii_lowercase().as_str()))
        {
            seen.insert((GapCode::NoSoftDelete.as_str(), t.clone()));
            out.push(Gap {
                code: GapCode::NoSoftDelete,
                operation: Some(op.id.clone()),
                r#type: Some(t.clone()),
                message: format!(
                    "`{}` deletes `{t}`, whose records have `{member}`: if the docs say a delete only marks the record, write `lifecycle` with `soft_delete` (and `listed_with` when a list parameter still shows such records); otherwise leave it",
                    op.id
                ),
                advisory: true,
            });
        }
    }
}

/// A state member of a type ([`state_member`]).
struct StateMember {
    member: String,
    /// Its values, as the contract lists them.
    values: Vec<String>,
    /// Whether they are the schema's `enum` (not only its description's).
    enumerated: bool,
    /// The operation that sets it: an action's when one does.
    operation: String,
}

/// The state member of type `t`, if it has one: of the members of a `t`
/// record ([`record_schemas`]), a top-level string whose values the contract
/// lists ([`listed_values`]) that
/// - no create of `t` sets: its body does not name it, or marks it `readOnly`;
/// - the body of an update, replace or action of `t` names, a body naming at
///   most half of the record's members (one that restates the record sets
///   every member, which sets none apart as a state).
///
/// A query parameter sets nothing. Names compare ignoring case, `_`, `-` and
/// one trailing `s`. Of several, the first in record order that only actions
/// set, else the first; it is reported at the first operation setting it.
fn state_member(
    b: &Bundle,
    t: &str,
    ty: &dyn Fn(&Operation) -> Option<String>,
    kind: &dyn Fn(&Operation) -> Option<OpKind>,
) -> Option<StateMember> {
    let record = record_schemas(b, t, ty, kind);
    if record.is_empty() {
        return None;
    }
    let in_record: BTreeSet<String> = record.iter().map(|(k, _)| normalize(k)).collect();
    let body = |op: &Operation| -> Vec<(String, bool)> {
        op.input
            .body
            .as_ref()
            .map(|s| {
                properties(b, s)
                    .into_iter()
                    .map(|(k, v)| (normalize(k), read_only(b, v)))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut created: BTreeSet<String> = BTreeSet::new();
    // Each setting operation's id, whether it is an action, and what it names.
    let mut setters: Vec<(&str, bool, BTreeSet<String>)> = Vec::new();
    for op in &b.operations {
        if ty(op).as_deref() != Some(t) {
            continue;
        }
        match kind(op) {
            Some(OpKind::Create) => {
                created.extend(body(op).into_iter().filter(|(_, ro)| !ro).map(|(k, _)| k));
            }
            Some(k @ (OpKind::Update | OpKind::Replace | OpKind::Action)) => {
                let named: BTreeSet<String> = body(op).into_iter().map(|(k, _)| k).collect();
                let overlap = named.intersection(&in_record).count();
                if overlap > 0 && overlap * 2 <= record.len() {
                    setters.push((op.id.as_str(), k == OpKind::Action, named));
                }
            }
            _ => {}
        }
    }
    let mut found: Vec<(StateMember, bool)> = Vec::new();
    for (m, s) in &record {
        let n = normalize(m);
        if m.contains('.') || created.contains(&n) {
            continue;
        }
        let by: Vec<&(&str, bool, BTreeSet<String>)> = setters
            .iter()
            .filter(|(_, _, named)| named.contains(&n))
            .collect();
        let Some(first) = by.iter().find(|(_, action, _)| *action).or(by.first()) else {
            continue;
        };
        let Some((values, enumerated)) = listed_values(b, s) else {
            continue;
        };
        let only_actions = by.iter().all(|(_, action, _)| *action);
        found.push((
            StateMember {
                member: (*m).to_owned(),
                values,
                enumerated,
                operation: first.0.to_owned(),
            },
            only_actions,
        ));
    }
    let at = found.iter().position(|(_, only)| *only).unwrap_or(0);
    (at < found.len()).then(|| found.swap_remove(at).0)
}

/// Whether a schema is `readOnly`.
fn read_only(b: &Bundle, schema: &Value) -> bool {
    schema.get("readOnly").and_then(Value::as_bool) == Some(true)
        || b.resolve(schema).get("readOnly").and_then(Value::as_bool) == Some(true)
}

/// The values a string member's schema lists, and whether they are its `enum`:
/// its `enum` of at least two strings, unless its description lists more of
/// them ([`code_span_list`]) with all of the enum's among them; else its
/// description's list of at least two.
fn listed_values(b: &Bundle, schema: &Value) -> Option<(Vec<String>, bool)> {
    let s = b.resolve(schema);
    let string = match s.get("type") {
        Some(Value::String(t)) => t == "string",
        Some(Value::Array(ts)) => ts.iter().any(|t| t == "string"),
        None => true,
        Some(_) => false,
    };
    if !string {
        return None;
    }
    let prose = s
        .get("description")
        .and_then(Value::as_str)
        .map(code_span_list)
        .unwrap_or_default();
    if let Some(e) = s.get("enum").and_then(Value::as_array) {
        let values: Vec<String> = e
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        if prose.len() > values.len() && values.iter().all(|v| prose.contains(v)) {
            return Some((prose, false));
        }
        return (values.len() >= 2).then_some((values, true));
    }
    (prose.len() >= 2).then_some((prose, false))
}

/// The longest run of code spans in `text` joined only by commas, `or` and
/// `and` (`` `a`, `b` or `c` ``), each a short token with no space. A run
/// right after `e.g.`, `for example` or `such as` lists examples, not the
/// values, and counts for none; a text with an unpaired backtick lists none.
fn code_span_list(text: &str) -> Vec<String> {
    let parts: Vec<&str> = text.split('`').collect();
    if parts.len().is_multiple_of(2) {
        return Vec::new();
    }
    let token = |v: &str| {
        !v.is_empty()
            && v.len() <= 64
            && v.chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
    };
    let joiner = |between: &str| {
        between
            .split(|c: char| c == ',' || c.is_whitespace())
            .all(|w| w.is_empty() || w.eq_ignore_ascii_case("or") || w.eq_ignore_ascii_case("and"))
    };
    let examples = |before: &str| {
        let before = before.trim_end().to_ascii_lowercase();
        let before = before.trim_end_matches([':', ',']).trim_end();
        ["e.g.", "e.g", "eg.", "for example", "such as"]
            .iter()
            .any(|p| before.ends_with(p))
    };
    let mut best: Vec<String> = Vec::new();
    let mut run: Vec<String> = Vec::new();
    let mut skipping = false;
    // Odd parts are inside backticks.
    let mut i = 1;
    while i + 1 < parts.len() {
        let (before, span, after) = (parts[i - 1], parts[i], parts[i + 1]);
        if run.is_empty() && !skipping {
            skipping = examples(before);
        }
        if token(span) && !skipping {
            run.push(span.to_owned());
        } else {
            run.clear();
        }
        if run.len() > best.len() {
            best = run.clone();
        }
        if !joiner(after) {
            run.clear();
            skipping = false;
        }
        i += 2;
    }
    best
}

/// The members of a `t` record with their schemas ([`record_members`]).
fn record_schemas<'a>(
    b: &'a Bundle,
    t: &str,
    ty: &dyn Fn(&Operation) -> Option<String>,
    kind: &dyn Fn(&Operation) -> Option<OpKind>,
) -> Vec<(&'a str, &'a Value)> {
    for want in [OpKind::Read, OpKind::Create] {
        for op in &b.operations {
            if ty(op).as_deref() == Some(t)
                && kind(op) == Some(want)
                && let Some(answer) = success_schema(op)
            {
                let props = properties(b, answer);
                let wrapped = props
                    .iter()
                    .filter(|(k, s)| {
                        (*k == "data" || normalize(k) == normalize(t)) && is_object(b, s)
                    })
                    .map(|(_, s)| *s)
                    .next();
                let props = wrapped.map_or(props, |s| properties(b, s));
                if !props.is_empty() {
                    return props;
                }
            }
        }
    }
    Vec::new()
}

/// The members of a `t` record: the success answer of a read of `t`, else of
/// a create (kinds as the description has them), or of the one object member
/// of that answer named `data` or after the type when it wraps the record.
fn record_members(
    b: &Bundle,
    t: &str,
    ty: &dyn Fn(&Operation) -> Option<String>,
    kind: &dyn Fn(&Operation) -> Option<OpKind>,
) -> Vec<String> {
    record_schemas(b, t, ty, kind)
        .into_iter()
        .map(|(k, _)| k.to_owned())
        .collect()
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
            advisory: false,
        });
    }
}

#[cfg(not(feature = "http-rest"))]
fn skipped_routes(_: &Bundle, _: &dyn Fn(&Operation) -> Option<String>, _: &mut Vec<Gap>) {}

/// The operations of `applied` whose calls have no identity to work with under
/// `world` ([`identified`]), by type, in contract order: the ones the executor
/// falls back to the model on (`no_identity`). `applied` is the bundle as the
/// runtime holds it, with `world` written into it ([`mb_mcm::world::apply`]). An
/// operation with no kind or type is left out.
#[must_use]
pub fn unidentified(applied: &Bundle, world: &World) -> BTreeMap<String, Vec<String>> {
    let read_by_id = reads_by_id(applied);
    let none = OpDesc::default();
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for op in &applied.operations {
        let d = world.operations.get(&op.id).unwrap_or(&none);
        let kind = d.kind.or_else(|| OpKind::of(op));
        let ty = d.r#type.clone().or_else(|| op.resource_type.clone());
        if let (Some(t), Some(k)) = (ty, kind) {
            let described = world.types.get(&t).and_then(|d| d.identity.as_ref());
            if !identified(k, op.identity.is_some(), described, read_by_id.contains(&t)) {
                out.entry(t).or_default().push(op.id.clone());
            }
        }
    }
    out
}

/// Whether a call of an operation of `kind` has an identity to work with: `own`,
/// the operation's own (`op.identity`, where its answer holds the record), else
/// `described`, its type's in the effective description, else (but for a list)
/// `read_by_id`, whether a read by id of its type has one (a query, not an action,
/// whose identity has a path parameter and whose success answer has a schema). A list
/// needs its type's identity `member`.
///
/// The one rule for `no_identity`: the executor answers only calls of operations it
/// holds, and falls back to the model on the others (`exec_fallback: no_identity`);
/// [`structural_gaps`] reports the type of each of those operations.
#[must_use]
pub fn identified(
    kind: OpKind,
    own: bool,
    described: Option<&IdentityDesc>,
    read_by_id: bool,
) -> bool {
    own || match kind {
        OpKind::List => described.is_some_and(|i| i.member.is_some()),
        _ => described.is_some() || read_by_id,
    }
}

/// The types `bundle` reads by id: a query (not an action) of the type whose identity
/// has a path parameter and whose success answer has a schema.
fn reads_by_id(bundle: &Bundle) -> BTreeSet<String> {
    bundle
        .operations
        .iter()
        .filter(|op| {
            matches!(op.kind, OperationKind::Query)
                && !op.is_action()
                && op.identity.as_ref().is_some_and(|i| i.path_param.is_some())
                && success_schema(op).is_some()
        })
        .filter_map(|op| op.resource_type.clone())
        .collect()
}

fn list(ops: &[String]) -> String {
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
