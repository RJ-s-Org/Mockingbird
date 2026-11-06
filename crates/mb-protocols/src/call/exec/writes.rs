//! Updates, replaces and deletes (step 3 of the plan). Deletes are executed; updates
//! and replaces are not yet, and generate.

use mb_engine::GeneratedResponse;
use mb_engine::consistency::property_schema;
use mb_mcm::world::OpKind;
use mb_state::StoredResource;
use serde_json::{Map, Value};

use super::{Fallback, Held, Ran, Write, answered, bodiless, commit, fell_back};
use crate::call::{CallPlan, OperationCall, StateAccess, lifecycle, serve_held};

/// Whether the executor takes updates and replaces.
pub(super) const EXECUTES: bool = false;

/// Executes the call.
pub(super) async fn run(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: Held<'_>,
    response_schema: Option<&Value>,
    kind: OpKind,
) -> Ran {
    match kind {
        OpKind::Delete => delete(state, plan, call, held, response_schema, kind).await,
        _ => Ran::Generate(None),
    }
}

/// A delete of the held target: removed, or moved to its type's soft-deleted state,
/// answered with the operation's declared delete answer.
async fn delete(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: Held<'_>,
    response_schema: Option<&Value>,
    kind: OpKind,
) -> Ran {
    let rt = state.rt;
    let (Some(stored), Some(id)) = (held.target, held.target_id) else {
        fell_back(rt, plan, kind, Fallback::NotHeld.as_str());
        return Ran::Generate(None);
    };
    let t = stored.resource_type.as_str();
    let mut data = stored.data.clone();
    let soft = lifecycle::soft_delete(rt, t, &mut data);
    let Some(answer) = delete_answer(rt, plan, (stored, data.clone()), response_schema) else {
        fell_back(rt, plan, kind, Fallback::RenderFailed.as_str());
        return Ran::Generate(None);
    };
    let write = match soft {
        true => Write::Put {
            id: id.to_owned(),
            data,
        },
        false => Write::Remove { id: id.to_owned() },
    };
    let answer = commit(state, plan, call, (t, Some(stored)), write, answer).await;
    answered(rt, plan, kind);
    Ran::Answered(Box::new(answer))
}

/// What a delete answers: nothing when its success response declares no body; else its
/// own schema rendered from the record as the delete leaves it: the record in the
/// envelope the operation holds it in, or the record's members the schema declares (an
/// empty object for a schema declaring none).
fn delete_answer(
    rt: &crate::MockRuntime,
    plan: &CallPlan<'_>,
    (stored, data): (&StoredResource, Value),
    response_schema: Option<&Value>,
) -> Option<GeneratedResponse> {
    let op = plan.op;
    if let Some(none) = bodiless(op) {
        return Some(none);
    }
    if op.identity.as_ref().is_some_and(|i| i.record.is_some()) {
        let left = StoredResource {
            data,
            ..stored.clone()
        };
        return serve_held(rt, op, &left, None, response_schema);
    }
    let schema = response_schema.or_else(|| op.success_output()?.schema.as_ref())?;
    let declared: Map<String, Value> = match data {
        Value::Object(members) => members
            .into_iter()
            .filter(|(name, _)| property_schema(&rt.bundle, schema, name).is_some())
            .collect(),
        _ => Map::new(),
    };
    rt.engine
        .serve_stored(&rt.bundle, op, &Value::Object(declared), response_schema)
}
