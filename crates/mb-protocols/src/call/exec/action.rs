//! Actions (step 5 of the plan): an action whose description sets members of its
//! target (a `transition`, `changes`) sets them on the held target, stores it, and
//! answers it, with no LLM call. Any other action generates.

use mb_mcm::world::OpKind;
use serde_json::Value;
use tracing::Instrument;

use super::{Fallback, Held, Ran, Write, answered, commit, fell_back, render};
use crate::call::{CallPlan, OperationCall, StateAccess, derived, state_span, transitions};

/// Whether the executor takes these calls.
pub(super) const EXECUTES: bool = true;

/// Executes the call: the members the description says the action sets are set on the
/// held target (a value that does not fit its member is dropped; nothing is set when
/// the record would not fit its type), which is stored and answered as the operation
/// answers a write of it ([`render`]: with the members the description derives, when
/// that answer suits the schema, else as stored).
pub(super) async fn run(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: Held<'_>,
    response_schema: Option<&Value>,
) -> Ran {
    let rt = state.rt;
    let op = plan.op;
    let kind = OpKind::Action;
    if !transitions::sets_members(rt, op) {
        fell_back(rt, plan, kind, "undescribed");
        return Ran::Generate(None);
    }
    let (Some(stored), Some(id)) = (held.target, held.target_id) else {
        fell_back(rt, plan, kind, Fallback::NotHeld.as_str());
        return Ran::Generate(None);
    };
    let mut changed = stored.clone();
    transitions::apply(rt, op, call.body.as_ref(), &mut changed.data);
    let derived = derived::filled(state, &plan.session, &changed)
        .instrument(state_span())
        .await;
    let answer = derived
        .and_then(|shown| render(rt, plan, call, (id, &shown.data), response_schema))
        .or_else(|| render(rt, plan, call, (id, &changed.data), response_schema));
    let Some(answer) = answer else {
        fell_back(rt, plan, kind, Fallback::RenderFailed.as_str());
        return Ran::Generate(None);
    };
    let write = Write::Put {
        id: id.to_owned(),
        data: changed.data,
    };
    let t = stored.resource_type.as_str();
    let answer = commit(state, plan, call, (t, Some(stored)), write, answer).await;
    answered(rt, plan, kind);
    Ran::Answered(Box::new(answer))
}
