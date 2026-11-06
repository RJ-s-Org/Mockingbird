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

use super::entities::{added_violation, entity_fits};
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
/// whole set when it adds a violation of the type's schema to the record (see
/// [`added_violation`]): it breaks a record that fit, or a record already not fitting
/// (stored in an earlier, incomplete shape) gets a violation it did not have. Answers
/// the members it set, in that order.
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
    // A fitting record refuses a change that breaks it; one that did not fit before
    // (stored in an earlier, incomplete shape) takes a change that adds no violation.
    if set.is_empty() {
        return set;
    }
    if let Some((path, keyword)) = added_violation(rt, t, &before, record) {
        tracing::info!(
            mock = %rt.name,
            operation = %op.id,
            path = %path,
            keyword = %keyword,
            "the record a described transition or change leaves has a violation it did not: none is set"
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use mb_engine::{Engine, EngineConfig};
    use mb_state::{MemoryStore, StateStore};
    use serde_json::json;

    use super::*;

    /// Items read as `{id, name, created}` (all required, at most four members), starred
    /// by an action.
    const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Items, version: "1"}
paths:
  /items/{id}:
    get:
      operationId: getItem
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
  /items/{id}/star:
    post:
      operationId: starItem
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
components:
  schemas:
    Item:
      type: object
      required: [id, name, created]
      maxProperties: 4
      properties:
        id: {type: string}
        name: {type: string}
        created: {type: string}
        starred: {type: boolean}
        rank: {type: integer}
"##;

    /// The runtime of [`CONTRACT`] whose world says `starItem` sets `changes`.
    fn runtime(changes: serde_json::Value) -> MockRuntime {
        let bundle = mb_import::compile(CONTRACT.as_bytes()).unwrap();
        let doc: mb_mcm::world::World = serde_json::from_value(json!({
            "operations": {"starItem": {"kind": "action", "target": {"path_param": "id"}, "changes": changes}}
        }))
        .unwrap();
        let base = mb_mcm::world::tier0(&bundle);
        let (world, refused) = mb_mcm::world::effective(&bundle, &base, &doc);
        assert!(refused.is_empty(), "{refused:?}");
        let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
        let engine = Arc::new(Engine::new(None, EngineConfig::default()));
        MockRuntime::new("items", Arc::new(bundle), engine, state).with_world(&base, &world, 1)
    }

    fn star(rt: &MockRuntime) -> Operation {
        rt.bundle().operation("starItem").unwrap().clone()
    }

    #[test]
    fn a_change_on_a_record_that_already_breaks_its_schema_is_set() {
        let rt = runtime(json!({"starred": {"value": true}}));
        let op = star(&rt);
        // Stored in an earlier shape: no `created`, which the read requires.
        let mut record = json!({"id": "i1", "name": "a"});
        assert_eq!(apply(&rt, &op, None, &mut record), vec!["starred"]);
        assert_eq!(record, json!({"id": "i1", "name": "a", "starred": true}));
    }

    #[test]
    fn a_change_adding_a_violation_to_a_record_that_already_breaks_its_schema_is_refused() {
        // Stored in an earlier shape (no `created`) with four members: a fifth passes
        // the read's `maxProperties`, a violation the record did not have.
        let rt = runtime(json!({"starred": {"value": true}}));
        let op = star(&rt);
        let held = json!({"id": "i1", "name": "a", "rank": 1, "note": "x"});
        let mut record = held.clone();
        assert!(apply(&rt, &op, None, &mut record).is_empty());
        assert_eq!(record, held);
    }

    #[test]
    fn a_wrong_typed_change_on_a_record_that_already_breaks_its_schema_is_refused() {
        let rt = runtime(json!({"starred": {"value": "yes"}}));
        let op = star(&rt);
        let held = json!({"id": "i1", "name": "a"});
        let mut record = held.clone();
        assert!(apply(&rt, &op, None, &mut record).is_empty());
        assert_eq!(record, held);
    }

    #[test]
    fn a_change_that_breaks_a_fitting_record_is_refused() {
        // Each value fits its member, but together they pass the read's `maxProperties`.
        let rt = runtime(json!({"starred": {"value": true}, "rank": {"value": 1}}));
        let op = star(&rt);
        let fitting = json!({"id": "i1", "name": "a", "created": "2026-01-01"});
        let mut record = fitting.clone();
        assert!(apply(&rt, &op, None, &mut record).is_empty());
        assert_eq!(record, fitting);
    }
}
