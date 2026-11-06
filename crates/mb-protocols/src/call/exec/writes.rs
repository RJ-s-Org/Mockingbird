//! Updates, replaces and deletes (step 3 of the plan). Deletes are executed; updates
//! and replaces are not yet, and generate.

use mb_engine::GeneratedResponse;
use mb_engine::consistency::declared_members;
use mb_mcm::world::OpKind;
use serde_json::{Value, json};

use super::{Fallback, Held, Ran, Write, answered, commit, fell_back, render};
use crate::call::{CallPlan, OperationCall, StateAccess, lifecycle};

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
    let Some(answer) = delete_answer(rt, plan, call, (id, &data), response_schema) else {
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

/// What a delete answers: an empty object when its success response's schema declares
/// no members, else its answer rendered from the record as the delete leaves it.
fn delete_answer(
    rt: &crate::MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    (id, data): (&str, &Value),
    response_schema: Option<&Value>,
) -> Option<GeneratedResponse> {
    let op = plan.op;
    let free_form = op
        .success_output()
        .and_then(|v| response_schema.or(v.schema.as_ref()))
        .is_some_and(|schema| declared_members(&rt.bundle, schema).is_none());
    if free_form && op.identity.as_ref().is_none_or(|i| i.record.is_none()) {
        return rt
            .engine
            .serve_stored(&rt.bundle, op, &json!({}), response_schema);
    }
    render(rt, plan, call, (id, data), response_schema)
}
