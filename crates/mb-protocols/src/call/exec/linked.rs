//! Server-set references of a record a create stored, given a value when a read serves
//! it.
//!
//! A record may hold a reference to a record of another type that no request can set:
//! the server links it (Plaid's Item `institution_id`, the institution the Link session
//! connected; the exchange sends only a public token, and no call updates an Item). A
//! create's fill leaves such a member `null` where its schema allows that, as on a fresh
//! record, and a client then asks for the record it names with `null` (the quickstart's
//! `/institutions/get_by_id`, which its SDK refuses, failing the app with a 500).
//!
//! When the executor reads, over HTTP, a held record that a create of this runtime
//! stored, each such member it holds as `null` names a record of that type instead, and
//! the record is stored so: one the session holds, else one of the mock's record pool,
//! else an id the faker draws as the member's schema declares. A member is such a
//! reference when:
//! - it is named as one (`<x>_id`, `<x>Id`) for a type of the contract other than the
//!   record's own, which a read of the contract serves, and that no operation of the
//!   contract creates (the server picks from records it has, as from a catalog);
//! - its schema admits an id (text or an integer) besides `null`;
//! - it is not the type's described `parent` member, and no operation of the record's
//!   type sets it: none has a request body declaring it, a path or query parameter of
//!   its name, or a described `set` or effect naming it. A `null` is then nothing a
//!   client or the description decided;
//! - the contract's examples show it holding a value ([`evidenced`]): Plaid's Item
//!   example names `ins_109508`. One whose examples show `null`, or hold no such
//!   member (a `closed_by_id` of an open record), stays `null`.
//!
//! The write is a compare and set: the record is stored only when it is still as it was
//! read, so a write another call made meanwhile is never undone.
//!
//! A record seeded, imported or mentioned by an answer keeps what it holds.

use mb_mcm::{Binding, Effect, OperationKind};
use mb_state::StoredResource;
use serde_json::Value;

use super::pool;
use crate::MockRuntime;
use crate::call::header_ids::{MintSeed, draw_id_as};
use crate::call::{CallPlan, OperationCall, StateAccess, entity_type, id_string, property_schema};

/// `stored`, a record a read is about to serve, with each server-set reference it holds
/// as `null` or not at all naming a record (see the module's doc), stored so; `None`
/// when it has none to name, or the store did not take it.
pub(super) async fn named(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    stored: &StoredResource,
) -> Option<StoredResource> {
    let rt = state.rt;
    let t = stored.resource_type.as_str();
    if !matches!(plan.op.binding, Binding::Http { .. })
        || !rt.collections().created(&plan.session, t, &stored.id)
    {
        return None;
    }
    let held = stored.data.as_object()?;
    let schema = record_schema(rt, t)?;
    let declared = mb_engine::consistency::declared_members(rt.bundle(), &schema)?;
    let mut names: Vec<&String> = declared
        .iter()
        .filter(|name| held.get(name.as_str()).is_some_and(Value::is_null))
        .collect();
    names.sort();
    let mut data = stored.data.clone();
    let mut changed = false;
    for name in names {
        let Some(member) = property_schema(rt.bundle(), &schema, name) else {
            continue;
        };
        let Some(u) = referred(rt, t, name, &member) else {
            continue;
        };
        if !evidenced(rt, t, name, &member) {
            continue;
        }
        let Some(value) = pick(state, (plan, call), (t, name), (&u, &member)).await else {
            continue;
        };
        if let Some(record) = data.as_object_mut() {
            record.insert(name.clone(), value);
            changed = true;
        }
    }
    if !changed {
        return None;
    }
    let named = StoredResource {
        data,
        ..stored.clone()
    };
    // Compare and set: stored only over the record as it was read. One another call
    // wrote since keeps what that call wrote, and this read serves the record it read.
    let current = state.get(&plan.session, t, &stored.id).await?;
    if current.data != stored.data {
        return None;
    }
    state
        .upsert(&plan.session, named.clone())
        .await
        .then_some(named)
}

/// Examples are looked into this deep for a member of the record.
const MAX_EXAMPLE_DEPTH: usize = 4;

/// Whether the contract's examples show member `name` of a record of type `t` (of schema
/// `member`) holding a value other than `null`: the member's own `example`/`examples`,
/// or those of the record's schema, of the answers of the reads of the type and of the
/// operations writing it (a response's examples, which the import keeps as its
/// schema's), at any member of that name up to [`MAX_EXAMPLE_DEPTH`] deep. A member
/// whose examples show only `null`, or that no example holds, is left `null`.
fn evidenced(rt: &MockRuntime, t: &str, name: &str, member: &Value) -> bool {
    let bundle = rt.bundle();
    if schema_examples(rt, member).iter().any(|e| !e.is_null()) {
        return true;
    }
    let answers = bundle
        .operations
        .iter()
        .filter(|op| super::list::item_type(rt, op) == Some(t))
        .filter_map(|op| op.success_output()?.schema.clone());
    let schemas: Vec<Value> = record_schema(rt, t)
        .into_iter()
        .chain(super::read_schemas(rt, t))
        .chain(answers)
        .collect();
    schemas
        .iter()
        .flat_map(|schema| schema_examples(rt, schema))
        .any(|example| shows(&example, name, 0))
}

/// The examples `schema` gives of itself (`example`, `examples`), in any of its parts.
fn schema_examples(rt: &MockRuntime, schema: &Value) -> Vec<Value> {
    let (parts, _) = crate::call::schema_parts(rt.bundle(), Some(schema));
    parts
        .iter()
        .flat_map(|part| {
            let listed = part
                .get("examples")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            part.get("example").cloned().into_iter().chain(listed)
        })
        .collect()
}

/// Whether `example` holds a member `name` that is not `null`, at `depth` or below.
fn shows(example: &Value, name: &str, depth: usize) -> bool {
    if depth > MAX_EXAMPLE_DEPTH {
        return false;
    }
    match example {
        Value::Object(members) => members
            .iter()
            .any(|(key, value)| (key == name && !value.is_null()) || shows(value, name, depth + 1)),
        Value::Array(items) => items.iter().any(|item| shows(item, name, depth + 1)),
        _ => false,
    }
}

/// The schema of a record of type `t` as its read serves it.
fn record_schema(rt: &MockRuntime, t: &str) -> Option<Value> {
    let (op, shape) = crate::call::type_check(rt, t)?;
    shape
        .cloned()
        .or_else(|| op.success_output()?.schema.clone())
}

/// The type member `name` (of schema `member`) of a record of type `t` is a server-set
/// reference to, when it is one (see the module's doc).
fn referred(rt: &MockRuntime, t: &str, name: &str, member: &Value) -> Option<String> {
    let index = rt.index();
    let prefix = crate::call::reference_prefix(name)?;
    let u = entity_type(index, None, prefix).filter(|u| u != t)?;
    let read = index.read_types.contains(&u) || index.detail_reads.contains_key(&u);
    if !read || index.creatable.contains(&u) {
        return None;
    }
    non_null_id(member)?;
    let described = rt.described();
    let parent = described
        .type_desc(t)
        .and_then(|d| d.parent.as_ref())
        .and_then(|p| p.member.as_deref());
    if parent == Some(name) {
        return None;
    }
    let world = described.world();
    let described_sets = world.is_some_and(|w| {
        w.operations.values().any(|d| {
            (d.r#type.as_deref() == Some(t) && d.set.contains_key(name))
                || d.effects
                    .iter()
                    .any(|e| e.r#type == t && e.set.contains_key(name))
        })
    });
    let settable = described_sets
        || rt.bundle().operations.iter().any(|op| {
            super::list::item_type(rt, op) == Some(t)
                && (params_name(rt, op, name)
                    || matches!(
                        op.kind,
                        OperationKind::Command(Effect::Create | Effect::Update | Effect::Other)
                    ) && op
                        .input
                        .body
                        .as_ref()
                        .is_some_and(|body| declares(rt, body, name)))
        });
    (!settable).then_some(u)
}

/// Whether `op` has a path or query parameter named `name`.
fn params_name(rt: &MockRuntime, op: &mb_mcm::Operation, name: &str) -> bool {
    [op.input.path.as_ref(), op.input.query.as_ref()]
        .into_iter()
        .flatten()
        .any(|params| property_schema(rt.bundle(), params, name).is_some())
}

/// Whether request `body`'s schema declares `name`, at its top or in the one object it
/// wraps the record in.
fn declares(rt: &MockRuntime, body: &Value, name: &str) -> bool {
    let bundle = rt.bundle();
    if property_schema(bundle, body, name).is_some() {
        return true;
    }
    mb_engine::consistency::declared_members(bundle, body)
        .into_iter()
        .flatten()
        .filter_map(|wrapper| property_schema(bundle, body, &wrapper))
        .any(|inner| property_schema(bundle, &inner, name).is_some())
}

/// `member` with `null` left out of its `type`, when it admits text or an integer.
fn non_null_id(member: &Value) -> Option<Value> {
    let mut schema = member.clone();
    let types: Vec<String> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(Value::as_str)
            .filter(|t| *t != "null")
            .map(str::to_owned)
            .collect(),
        _ => return None,
    };
    if !types.iter().any(|t| t == "string" || t == "integer") {
        return None;
    }
    if let Some(object) = schema.as_object_mut() {
        object.insert(
            "type".to_owned(),
            match types.as_slice() {
                [one] => Value::String(one.clone()),
                _ => Value::from(types),
            },
        );
    }
    Some(schema)
}

/// An id of a record of type `u` for member `name` of a record of type `t`: one the
/// session holds, else one of the pool's, else the faker's draw by `member`'s schema.
async fn pick(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    (t, name): (&str, &str),
    (u, member): (&str, &Value),
) -> Option<Value> {
    let rt = state.rt;
    let declared = non_null_id(member)?;
    let fits = |value: &Value| {
        plan.engine
            .schema_violations(rt.bundle(), &declared, value)
            .is_some_and(|violations| violations.is_empty())
    };
    let id_member = crate::call::exprs::identity_member(rt, u);
    let held = state.list(&plan.session, u, 1).await;
    let held = held.first().and_then(|r| {
        r.data
            .get(&id_member)
            .cloned()
            .or_else(|| Some(Value::String(r.id.clone())))
    });
    if let Some(value) = held.filter(|v| fits(v)) {
        return Some(value);
    }
    if let Some(value) =
        pool::an_id(rt, u, &id_member).filter(|v| id_string(v).is_some() && fits(v))
    {
        return Some(value);
    }
    let seed = MintSeed::new(rt, &plan.session, call);
    draw_id_as(rt, (seed, t, name), &declared).filter(|v| fits(v))
}
