//! Actions (step 5 of the plan): an action whose description sets members of its
//! target (a `transition`, `changes`) sets them on the held target, stores it, and
//! answers it, with no LLM call; so does one whose verb names a state of its target's
//! type ([`super::verbs`]) when no model answers it; one whose description gives it a `queue` puts messages
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
    // An action the description sets nothing for moves its target's state as its verb
    // says (`closeIssue` → `CLOSED`, [`super::verbs`]) when no model answers it; with
    // one, the model's answer does.
    let described = transitions::sets_members(rt, op);
    let no_model = !plan.engine.has_provider() || plan.engine.provider_blocked().is_some();
    if !described && !no_model {
        fell_back(rt, plan, kind, "undescribed");
        return Ran::Generate(None);
    }
    let (Some(stored), Some(id)) = (held.target, held.target_id) else {
        let why = if described {
            Fallback::NotHeld.as_str()
        } else {
            "undescribed"
        };
        fell_back(rt, plan, kind, why);
        return Ran::Generate(None);
    };
    // Of the record the action changes: its own type, whatever the operation names.
    let verb_sets = if described {
        None
    } else {
        match super::verbs::inferred(rt, op, &stored.resource_type) {
            Some(set) => {
                tracing::info!(
                    mock = %rt.name,
                    operation = %op.id,
                    member = %set.0,
                    value = %set.1,
                    exec_inferred = "verb",
                    "an action's verb set its target's state"
                );
                Some(set)
            }
            None => {
                fell_back(rt, plan, kind, "undescribed");
                return Ran::Generate(None);
            }
        }
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
    match &verb_sets {
        Some((member, value)) => {
            if let Some(record) = changed.data.as_object_mut() {
                record.insert(member.clone(), value.clone());
            }
        }
        None => {
            transitions::apply(state, (plan, call), Some(&stored.data), &mut changed.data).await;
        }
    }
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
