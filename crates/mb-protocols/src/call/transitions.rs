//! Transitions, `set` (and `changes`, read as `set`) and replaces read from the
//! effective world description.
//!
//! Kind and target need nothing here: [`mb_mcm::world::apply`] writes them into the
//! runtime's bundle, so every heuristic that reads `op.kind` or `op.identity` follows
//! them. What the bundle has no place for is read here, by operation id:
//!
//! - [`apply`]: the members an update, replace, action (or the create executor) sets on
//!   the record it stores, whatever the model answered: its `transition` (states of the
//!   type's lifecycle member), then its `set` expressions ([`super::exprs`]), which hold
//!   its `changes` too (`from_request` as the request member's expression, `value` as a
//!   literal). Each `set` value is written as its member's schema declares it
//!   ([`super::exprs::written_back`]); every value is checked against that schema and
//!   dropped, with an info log, when it does not fit.
//! - [`replaces`]: an update the description calls a `replace`, which stores what its
//!   request sends instead of merging it (see `write_memory::onto_record`).
//!
//! Tier 0 emits no `transition`, `set`, `changes` or `replace`, so a runtime with no
//! world and one serving tier 0 answer alike.

use mb_engine::GeneratedResponse;
use mb_engine::OperationCall;
use mb_mcm::world::OpKind;
use mb_mcm::{Effect, Operation, OperationKind};
use serde_json::Value;

use super::entities::{added_violation, entity_fits};
use super::write_memory::read_schema;
use super::{CallPlan, StateAccess, exprs, id_string};
use crate::MockRuntime;

/// Whether the description says `op` sets members of its target: a `transition`, or
/// `set` (`changes` among them).
pub(super) fn sets_members(rt: &MockRuntime, op: &Operation) -> bool {
    rt.described()
        .op(&op.id)
        .is_some_and(|d| d.transition.as_ref().is_some_and(|t| !t.is_empty()))
        || exprs::sets(rt, &op.id)
}

/// Sets on `record` (a record of `plan`'s operation's type) the members the
/// description says the operation sets: its `transition` members (a state is written
/// as the member's type declares it), then its `set` values ([`exprs::set_values`],
/// evaluated once per call against `before`, the target as stored before it; `changes`
/// among them), each `set` value written as its member declares it
/// ([`exprs::written_back`]) and each `changes` value as it is. A
/// value that does not fit its member's schema is dropped, and so is the whole set
/// when it adds a violation of the type's schema to the record (see
/// [`added_violation`]): it breaks a record that fit, or a record already not fitting
/// (stored in an earlier, incomplete shape) gets a violation it did not have. Answers
/// the members it set, in that order.
pub(super) async fn apply(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    before: Option<&Value>,
    record: &mut Value,
) -> Vec<String> {
    applied(state, (plan, call), (before, false), record).await
}

/// [`apply`] on the record a create stores: its `set` alone, since `changes` (which
/// `set` replaces) never applied to a create, nor does a `transition`.
pub(super) async fn apply_on_create(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    record: &mut Value,
) -> Vec<String> {
    applied(state, (plan, call), (None, true), record).await
}

async fn applied(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    (before, creating): (Option<&Value>, bool),
    record: &mut Value,
) -> Vec<String> {
    let rt = state.rt;
    let op = plan.op;
    let Some(t) = op.resource_type.as_deref() else {
        return Vec::new();
    };
    if !record.is_object() {
        return Vec::new();
    }
    let mut wanted: Vec<(String, Option<Value>)> = Vec::new();
    if let Some(d) = rt.described().op(&op.id).filter(|_| !creating) {
        for (member, value) in d.transition.iter().flatten() {
            let typed = match value {
                Value::String(state) => rt.described().state_value(t, state),
                other => Some(other.clone()),
            };
            wanted.push((member.clone(), typed));
        }
    }
    for (member, value, from_changes) in exprs::set_values(state, (plan, call), before).await {
        if creating && from_changes {
            continue;
        }
        // A `changes` value is set as it is, as `changes` always set it.
        let value = match from_changes {
            true => value,
            false => value.map(|v| exprs::written_back(rt, t, &member, v)),
        };
        wanted.push((member, value));
    }
    if wanted.is_empty() {
        return Vec::new();
    }
    let before = record.clone();
    let mut set = Vec::new();
    for (member, value) in wanted {
        match value.filter(|v| fits_member(rt, t, &member, v)) {
            Some(value) => {
                if mb_mcm::set_member_at(record, &member, value) {
                    set.retain(|m| *m != member);
                    set.push(member);
                }
            }
            None => tracing::info!(
                mock = %rt.name,
                operation = %op.id,
                member = %member,
                "a described transition or set has no value its member's schema takes: dropped"
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
            "the record a described transition or set leaves has a violation it did not: none is set"
        );
        *record = before;
        return Vec::new();
    }
    set
}

/// Sets on the record `generated` answers (an update's or action's, the record an
/// envelope holds already taken out of it) what [`apply`] sets on the stored one, when
/// that record is the target: a record of the operation's type holding `target_id` as
/// its identity member. An answer of another shape (an acknowledgement) is left as it
/// is.
pub(super) async fn onto_answer(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    (before, target_id): (Option<&Value>, Option<&str>),
    generated: &mut GeneratedResponse,
) {
    let rt = state.rt;
    let op = plan.op;
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
    apply(state, (plan, call), before, body).await;
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

    /// What [`apply`] sets on `record` for a `starItem` call sending `body`.
    async fn applied(rt: &MockRuntime, body: Option<Value>, record: &mut Value) -> Vec<String> {
        let op = rt.bundle().operation("starItem").unwrap().clone();
        let plan = crate::call::plan(
            rt,
            &op,
            &axum::http::HeaderMap::new(),
            crate::call::Presented::default(),
        )
        .await
        .unwrap();
        let call = OperationCall {
            operation_id: op.id.clone(),
            body,
            ..OperationCall::default()
        };
        let mut state = StateAccess::new(rt);
        let before = record.clone();
        apply(&mut state, (&plan, &call), Some(&before), record).await
    }

    #[tokio::test]
    async fn a_change_on_a_record_that_already_breaks_its_schema_is_set() {
        let rt = runtime(json!({"starred": {"value": true}}));
        // Stored in an earlier shape: no `created`, which the read requires.
        let mut record = json!({"id": "i1", "name": "a"});
        assert_eq!(applied(&rt, None, &mut record).await, vec!["starred"]);
        assert_eq!(record, json!({"id": "i1", "name": "a", "starred": true}));
    }

    #[tokio::test]
    async fn a_change_adding_a_violation_to_a_record_that_already_breaks_its_schema_is_refused() {
        // Stored in an earlier shape (no `created`) with four members: a fifth passes
        // the read's `maxProperties`, a violation the record did not have.
        let rt = runtime(json!({"starred": {"value": true}}));
        let held = json!({"id": "i1", "name": "a", "rank": 1, "note": "x"});
        let mut record = held.clone();
        assert!(applied(&rt, None, &mut record).await.is_empty());
        assert_eq!(record, held);
    }

    #[tokio::test]
    async fn a_wrong_typed_change_on_a_record_that_already_breaks_its_schema_is_refused() {
        let rt = runtime(json!({"starred": {"value": "yes"}}));
        let held = json!({"id": "i1", "name": "a"});
        let mut record = held.clone();
        assert!(applied(&rt, None, &mut record).await.is_empty());
        assert_eq!(record, held);
    }

    #[tokio::test]
    async fn a_change_that_breaks_a_fitting_record_is_refused() {
        // Each value fits its member, but together they pass the read's `maxProperties`.
        let rt = runtime(json!({"starred": {"value": true}, "rank": {"value": 1}}));
        let fitting = json!({"id": "i1", "name": "a", "created": "2026-01-01"});
        let mut record = fitting.clone();
        assert!(applied(&rt, None, &mut record).await.is_empty());
        assert_eq!(record, fitting);
    }
}
