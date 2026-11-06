//! Transitions, changes and replaces read from the effective world description.
//!
//! Kind and target need nothing here: [`mb_mcm::world::apply`] writes them into the
//! runtime's bundle, so every heuristic that reads `op.kind` or `op.identity` follows
//! them. What the bundle has no place for is read here, by operation id:
//!
//! - [`apply`]: the members an update or action sets on its target, whatever the model
//!   answered: its `transition` (states of the type's lifecycle member), then its
//!   `changes` (a request member, or a fixed value). Each value is checked against its
//!   member's schema and dropped, with an info log, when it does not fit.
//! - [`replaces`]: an update the description calls a `replace`, which stores what its
//!   request sends instead of merging it (see `write_memory::onto_record`).
//!
//! Tier 0 emits no `transition`, `changes` or `replace`, so a runtime with no world and
//! one serving tier 0 answer alike.

use mb_engine::GeneratedResponse;
use mb_mcm::world::{ChangeDesc, OpKind};
use mb_mcm::{Effect, Operation, OperationKind};
use serde_json::Value;

use super::entities::entity_fits;
use super::id_string;
use super::write_memory::read_schema;
use crate::MockRuntime;

/// Whether the description says `op` sets members of its target: a `transition` or
/// `changes`.
pub(super) fn sets_members(rt: &MockRuntime, op: &Operation) -> bool {
    rt.described().op(&op.id).is_some_and(|d| {
        d.transition.as_ref().is_some_and(|t| !t.is_empty()) || !d.changes.is_empty()
    })
}

/// Sets on `record` (a record of `op`'s type) the members the description says `op`
/// sets: its `transition` members (a state is written as the member's type declares
/// it), then its `changes` (the `request` body at a `from_request` pointer, or a fixed
/// `value`). A value that does not fit its member's schema is dropped, and so is the
/// whole set when the record it leaves does not fit the type. Answers the members it
/// set, in that order.
pub(super) fn apply(
    rt: &MockRuntime,
    op: &Operation,
    request: Option<&Value>,
    record: &mut Value,
) -> Vec<String> {
    let Some(d) = rt.described().op(&op.id) else {
        return Vec::new();
    };
    let Some(t) = op.resource_type.as_deref() else {
        return Vec::new();
    };
    if !record.is_object() {
        return Vec::new();
    }
    let mut wanted: Vec<(&str, Option<Value>)> = Vec::new();
    for (member, value) in d.transition.iter().flatten() {
        let typed = match value {
            Value::String(state) => rt.described().state_value(t, state),
            other => Some(other.clone()),
        };
        wanted.push((member, typed));
    }
    for (member, change) in &d.changes {
        let value = match change {
            ChangeDesc::FromRequest(pointer) => {
                request.and_then(|r| r.pointer(pointer.as_str())).cloned()
            }
            ChangeDesc::Value(value) => Some(value.clone()),
        };
        wanted.push((member, value));
    }
    if wanted.is_empty() {
        return Vec::new();
    }
    let before = record.clone();
    let mut set = Vec::new();
    for (member, value) in wanted {
        match value.filter(|v| fits_member(rt, t, member, v)) {
            Some(value) => {
                if mb_mcm::set_member_at(record, member, value) {
                    set.retain(|m| m != member);
                    set.push(member.to_owned());
                }
            }
            None => tracing::info!(
                mock = %rt.name,
                operation = %op.id,
                member = %member,
                "a described transition or change has no value its member's schema takes: dropped"
            ),
        }
    }
    if !set.is_empty() && !entity_fits(rt, t, record) {
        tracing::info!(
            mock = %rt.name,
            operation = %op.id,
            "the record a described transition or change leaves breaks its schema: none is set"
        );
        *record = before;
        return Vec::new();
    }
    set
}

/// Sets on the record `generated` answers (an update's or action's, the record an
/// envelope holds already taken out of it) what [`apply`] sets on the stored one, when
/// that record is the target: a record of `op`'s type holding `target_id` as its
/// identity member. An answer of another shape (an acknowledgement) is left as it is.
pub(super) fn onto_answer(
    rt: &MockRuntime,
    op: &Operation,
    request: Option<&Value>,
    target_id: Option<&str>,
    generated: &mut GeneratedResponse,
) {
    if !matches!(
        op.kind,
        OperationKind::Command(Effect::Update | Effect::Other)
    ) || !sets_members(rt, op)
    {
        return;
    }
    let (Some(t), Some(target_id)) = (op.resource_type.as_deref(), target_id) else {
        return;
    };
    let Some(body) = generated.body.as_mut() else {
        return;
    };
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let holds_target = mb_mcm::member_at(body, field)
        .and_then(id_string)
        .is_some_and(|id| id == target_id);
    if !holds_target || !entity_fits(rt, t, body) {
        return;
    }
    apply(rt, op, request, body);
}

/// Whether the description says update `op` replaces its target whole (`kind:
/// replace`) rather than merging into it.
pub(super) fn replaces(rt: &MockRuntime, op: &Operation) -> bool {
    rt.described()
        .op(&op.id)
        .is_some_and(|d| d.kind == Some(OpKind::Replace))
}

/// Whether `value` fits the schema the record schema of type `t` gives `member`; a
/// member with no schema to go by takes anything.
fn fits_member(rt: &MockRuntime, t: &str, member: &str, value: &Value) -> bool {
    let bundle = rt.bundle();
    let Some(schema) =
        read_schema(rt, t).and_then(|r| mb_engine::consistency::property_schema(bundle, r, member))
    else {
        return true;
    };
    mb_engine::SchemaValidator::new(&bundle.with_reachable_defs(&schema))
        .is_ok_and(|v| v.is_valid(value))
}
