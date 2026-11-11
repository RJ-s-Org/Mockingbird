//! Actions (step 5 of the plan): an action whose description sets members of its
//! target (a `transition`, `changes`) sets them on the held target, stores it, and
//! answers it, with no LLM call; one whose description gives it a `queue` puts messages
//! on its target or takes them off ([`super::queue`]). Any other action generates, as does one on a target
//! behind writes that write memory keeps for it.

use mb_mcm::world::OpKind;
use serde_json::Value;
use tracing::Instrument;

use super::writes::written;
use super::{Fallback, Held, Ran, Rendered, Write, answered, commit, fell_back};
use crate::call::{
    CallPlan, OperationCall, StateAccess, computed, state_span, transitions, write_memory,
};

/// Whether the executor takes these calls.
pub(super) const EXECUTES: bool = true;

/// Executes the call: the members the description says the action sets are set on the
/// held target (a value that does not fit its member is dropped; nothing is set when
/// the record would not fit its type), which is stored and answered as the operation
/// answers a write of it ([`written`], as a create's and an update's are: with the
/// members the description derives, when that answer suits the schema, else as stored;
/// a described long-running answer holds it as its result, and is done).
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
    // A message queue's publish or get ([`super::queue`]).
    if let Some(queue) = rt.described().op(&op.id).and_then(|d| d.queue.as_ref()) {
        return super::queue::run(state, plan, call, held, response_schema, queue).await;
    }
    if !transitions::sets_members(rt, op) {
        fell_back(rt, plan, kind, "undescribed");
        return Ran::Generate(None);
    }
    let (Some(stored), Some(id)) = (held.target, held.target_id) else {
        fell_back(rt, plan, kind, Fallback::NotHeld.as_str());
        return Ran::Generate(None);
    };
    // Writes the stored copy does not show yet (an acknowledged create, an action that
    // answered nothing, a create referring to the target) are remembered: the action
    // generates and is told them, as a read behind them is, since committing would
    // settle that memory with what it held never stored.
    let earlier = write_memory::recall(state, plan, call, Some(id), true)
        .instrument(state_span())
        .await;
    if !earlier.shown.is_empty() {
        fell_back(rt, plan, kind, Fallback::Behind.as_str());
        return Ran::Generate(Some(earlier));
    }
    let mut changed = stored.clone();
    transitions::apply(state, (plan, call), Some(&stored.data), &mut changed.data).await;
    let derived = computed::filled(state, (plan, call), &changed)
        .instrument(state_span())
        .await;
    let answer = derived
        .and_then(|shown| written(rt, plan, call, (id, &shown.data), response_schema))
        .or_else(|| written(rt, plan, call, (id, &changed.data), response_schema));
    let Some(answer) = answer else {
        fell_back(rt, plan, kind, Fallback::RenderFailed.as_str());
        return Ran::Generate(None);
    };
    let rendered = Rendered {
        id: id.to_owned(),
        record: changed.clone(),
        schema: response_schema,
        new: false,
    };
    let write = Write::Put {
        id: id.to_owned(),
        data: changed.data,
    };
    let t = stored.resource_type.as_str();
    let held = (t, Some(stored));
    let answer = commit(state, plan, call, held, (write, Some(rendered)), answer).await;
    answered(rt, plan, kind);
    Ran::Answered(Box::new(answer))
}
