//! An operation's `effects`: writes to other records after its own write of its target.
//!
//! [`apply`] runs from [`super::exec::commit`], once the executor has stored (or
//! removed) the target, before the call's answer is kept and published. That is the
//! rule for when effects run: **only when the executor wrote the target in this call**.
//! A refused call, one the executor leaves to generation (whatever write memory then
//! keeps of the generated answer), and one that stores nothing run none.
//!
//! Each effect, in order:
//! - `update` and `delete` find their record with `held`'s lookup by key
//!   ([`records::held_by_key`]) of the id their `id` gives; one not held, or
//!   soft-deleted, is skipped;
//! - `when`, if any, must be `true`;
//! - its `set` values are evaluated with `record` the target as the write left it (as
//!   it was held, for a removal; the item as held before, for a write within its
//!   parent) and `it` the record found (`null` for a create); each is written as its
//!   member declares it ([`exprs::written_back`]), and a `null` sets nothing;
//! - a `create` stores a new record of its type (one a read of the type would not
//!   render is not stored, and is noted under `expr_failed`), under the id `set` gives its identity
//!   member, else one minted as a create mints it ([`records::mint_one`]), in the
//!   collection its parent's path parameter makes (the parent named by the record's
//!   `parent.member`, else the target when the target is of the parent type), noted as
//!   created in the session; an id already held skips it;
//! - an `update` stores the found record with its `set` members;
//! - a `delete` moves the found record to its type's soft-deleted state, or removes it
//!   and marks it deleted as a delete by its read's route would.
//!
//! An expression that fails logs `expr_failed` with its pointer and skips its effect.
//! Effects never cascade: they are stored directly, never through another operation's
//! `refuse`, `set` or `effects`.

use mb_mcm::world::{EffectEntry, EffectOp};

use super::exprs::{self, Bound, SessionHost};
use super::*;

/// Applies `plan`'s effects, after its write of its target of type `t`, `target` (the
/// record as the write leaves it) held under `id`. Whether any effect wrote.
pub(super) async fn apply(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: &str,
    (id, target): (&str, &Value),
) -> bool {
    let rt = state.rt;
    let Some(ops) = rt
        .described()
        .expressions(&plan.op.id)
        .filter(|o| !o.effects.is_empty())
    else {
        return false;
    };
    let mut host = exprs::host(state, rt, (plan, call), ops).await;
    let mut wrote = false;
    let mut bound = Bound::new(rt, call, Some(target));
    for effect in &ops.effects {
        // Each effect reads the session as the ones before it left it.
        host.refresh(state).await;
        let applied = one(state, (plan, call), (t, id), effect, &mut host, &mut bound).await;
        wrote |= applied;
        tracing::info!(
            mock = %rt.name,
            operation = %plan.op.id,
            effect = %effect.pointer,
            applied,
            "a described effect"
        );
    }
    host.store_counters(state).await;
    wrote
}

/// Applies one effect; whether it wrote.
async fn one(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    (t, target_id): (&str, &str),
    effect: &EffectEntry,
    host: &mut SessionHost<'_>,
    bound: &mut Bound,
) -> bool {
    let rt = state.rt;
    let session = &plan.session;
    let ty = effect.r#type.as_str();
    let at = effect.pointer.as_str();
    bound.it = None;
    let found = match &effect.id {
        Some(program) => {
            let pointer = format!("{at}/id");
            exprs::resolve_held(state, host, &[program], bound).await;
            let Some(value) = exprs::eval(plan, rt, (program, &pointer), bound, host) else {
                return false;
            };
            let Some(id) = id_string(&value) else {
                exprs::failed(plan, rt, &pointer, &format!("`id` gave {value}, not an id"));
                return false;
            };
            // A soft-deleted record is not held: it is neither updated nor deleted again.
            let gone = lifecycle::exclusion(rt, ty);
            match records::held_by_key(state, session, ty, &id).await {
                Some(found) if !gone.as_ref().is_some_and(|g| g.excludes(&found.data)) => {
                    Some(found)
                }
                _ => return false,
            }
        }
        None => None,
    };
    bound.it = found.as_ref().map(|f| f.data.clone());
    let programs: Vec<&mb_mcm::world::expr::Program> = effect
        .when
        .iter()
        .chain(effect.set.iter().map(|(_, _, p)| p))
        .collect();
    exprs::resolve_held(state, host, &programs, bound).await;
    if let Some(when) = &effect.when {
        let pointer = format!("{at}/when");
        match exprs::eval(plan, rt, (when, &pointer), bound, host) {
            Some(Value::Bool(true)) => {}
            Some(Value::Bool(false) | Value::Null) | None => return false,
            Some(other) => {
                exprs::failed(
                    plan,
                    rt,
                    &pointer,
                    &format!("`when` gave {other}, not a bool"),
                );
                return false;
            }
        }
    }
    let mut values: Vec<(&str, Value)> = Vec::new();
    for (member, pointer, program) in &effect.set {
        match exprs::eval(plan, rt, (program, pointer), bound, host) {
            None => return false,
            Some(Value::Null) => {}
            Some(value) => values.push((member, exprs::written_back(rt, ty, member, value))),
        }
    }
    match (effect.op, found) {
        (EffectOp::Create, _) => {
            let at = (plan, call, at);
            create(state, at, (t, target_id), ty, &values).await
        }
        (EffectOp::Update, Some(found)) => {
            let mut data = found.data.clone();
            for (member, value) in values {
                mb_mcm::set_member_at(&mut data, member, value);
            }
            let op = route(rt, ty, false).unwrap_or(plan.op);
            let place = (None, None, true);
            store_one(state, session, (op, ty), (found.id.clone(), data), place).await;
            write_memory::settled(state, session, ty, &found.id).await;
            true
        }
        (EffectOp::Delete, Some(found)) => delete(state, (plan, call), ty, &found).await,
        (EffectOp::Update | EffectOp::Delete, None) => false,
    }
}

/// A create effect: a new record of type `ty` holding `values`.
async fn create(
    state: &mut StateAccess<'_>,
    (plan, call, pointer): (&CallPlan<'_>, &OperationCall, &str),
    (t, target_id): (&str, &str),
    ty: &str,
    values: &[(&str, Value)],
) -> bool {
    let rt = state.rt;
    let session = &plan.session;
    let mut data = Value::Object(Map::new());
    for (member, value) in values {
        mb_mcm::set_member_at(&mut data, member, value.clone());
    }
    let field = exprs::identity_member(rt, ty);
    let id = match mb_mcm::member_at(&data, &field).and_then(id_string) {
        Some(id) => id,
        None => {
            let carried = request_values(plan.op, call);
            let minted = records::mint_one(
                state,
                (session, call),
                (ty, &field),
                &carried,
                &mut Vec::new(),
            )
            .await;
            let Some(id) = minted else {
                return false;
            };
            let Some(key) = id_string(&id) else {
                return false;
            };
            if !mb_mcm::set_member_at(&mut data, &field, id) {
                return false;
            }
            key
        }
    };
    if records::held_by_key(state, session, ty, &id)
        .await
        .is_some()
    {
        return false;
    }
    // A record a read of its type would not render is never stored, as a create's.
    let record = StoredResource {
        resource_type: ty.to_string(),
        id: id.clone(),
        data: data.clone(),
    };
    let filled = computed::filled_new(state, (plan, call), &record).await;
    if let Some(why) = super::exec::unreadable(rt, ty, &data, filled.as_ref().map(|f| &f.data)) {
        let why = format!("the record it creates would not render: {why}");
        exprs::failed(plan, rt, pointer, &why);
        return false;
    }
    // Where its type's create would put it: under its parent's path.
    let creating = route(rt, ty, true);
    let at_call = synthetic_call(rt, call, ty, (&id, &data), (t, target_id));
    let collection = creating.and_then(|op| collection_of(op, &at_call));
    let op = creating.or_else(|| route(rt, ty, false)).unwrap_or(plan.op);
    state_lists::mark_created(state, session, (op, ty), collection.as_ref(), &id).await;
    let place = (collection, None, false);
    store_one(state, session, (op, ty), (id, data), place).await;
    true
}

/// A delete effect of `found`, a held record of type `ty`.
async fn delete(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    ty: &str,
    found: &StoredResource,
) -> bool {
    let rt = state.rt;
    let session = &plan.session;
    let mut data = found.data.clone();
    let read = route(rt, ty, false);
    if lifecycle::delete_softly(rt, ty, &mut data, &Map::new()) {
        let op = read.unwrap_or(plan.op);
        let place = (None, None, true);
        store_one(state, session, (op, ty), (found.id.clone(), data), place).await;
        return true;
    }
    state.delete(session, ty, &found.id).await;
    rt.collections().forget(session, found);
    if state.tracks_deleted
        && let Some(read) = read
    {
        let at_call = synthetic_call(rt, call, ty, (&found.id, &found.data), ("", ""));
        deleted::mark(state, session, (read, &at_call), ty, &found.id, Some(found)).await;
    }
    tokens::forget(state, session, ty, &found.id).await;
    write_memory::settled(state, session, ty, &found.id).await;
    true
}

/// The operation of type `ty` an effect's write is stored as: its create (`creating`),
/// else its read by id.
fn route<'r>(rt: &'r MockRuntime, ty: &str, creating: bool) -> Option<&'r Operation> {
    let bundle = rt.bundle();
    if creating {
        return bundle.operations.iter().find(|op| {
            op.kind == OperationKind::Command(Effect::Create)
                && op.resource_type.as_deref() == Some(ty)
        });
    }
    rt.index()
        .detail_reads
        .get(ty)
        .and_then(|i| bundle.operations.get(*i))
}

/// A call naming the record of type `ty` held under `id` (`data`) by its own routes:
/// the path parameters of `call`, with its parent's path parameter set to its parent's
/// id (named by its `parent.member`, else the target, `target_id` of type `t`, when that
/// is its parent's type) and its read's target parameter to `id`.
fn synthetic_call(
    rt: &MockRuntime,
    call: &OperationCall,
    ty: &str,
    (id, data): (&str, &Value),
    (t, target_id): (&str, &str),
) -> OperationCall {
    let mut at = call.clone();
    if let Some(parent) = rt.described().type_desc(ty).and_then(|d| d.parent.as_ref())
        && let Some(param) = &parent.path_param
    {
        let named = parent
            .member
            .as_deref()
            .and_then(|m| mb_mcm::member_at(data, m))
            .and_then(id_string)
            .map(|v| {
                parent
                    .pattern
                    .as_deref()
                    .and_then(|p| records::bare_id(p, &v))
                    .unwrap_or(v)
            })
            .or_else(|| {
                (parent.r#type == t && !target_id.is_empty()).then(|| target_id.to_string())
            });
        if let Some(named) = named {
            at.path_params.insert(param.clone(), Value::String(named));
        }
    }
    if let Some(param) = route(rt, ty, false).and_then(target_param) {
        at.path_params.insert(param, Value::String(id.to_string()));
    }
    at
}
