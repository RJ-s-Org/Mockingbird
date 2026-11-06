//! The state-first executor (`docs/superpowers/specs/2026-11-06-state-first-engine-design.md`):
//! for a resource operation the effective world description classifies, the store
//! decides the answer and the stored record is rendered into the response schema.
//! Everything it cannot classify, and every call it falls back on, takes the generated
//! path unchanged.
//!
//! [`run`] is the seam [`super::prepare`] calls once the call's target, refusals and
//! tokens are settled. A classified call is logged at `info` with the field `exec` (its
//! kind) when the executor answers it, and with `exec_fallback` (a [`Fallback`]) when it
//! goes on to generation; executor answers are tagged `state`, as stored answers are.

use mb_engine::GeneratedResponse;
use mb_mcm::OperationKind;
use mb_mcm::world::OpKind;
use mb_state::StoredResource;
use serde_json::{Map, Value};
use tracing::Instrument;

use super::write_memory::Earlier;
use super::{
    CallPlan, OperationCall, StateAccess, derived, envelopes, plain, serve_held, state_span,
    target_id, write_memory,
};

/// Why a classified call goes on to generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fallback {
    /// The mock's `consistency` is off.
    ConsistencyOff,
    /// A scenario or a forced status (an injected chaos error forces one) applies.
    NotPlain,
    /// The request names a seed (`X-Mockingbird-Seed`), which asks for that seed's answer.
    Seeded,
    /// The operation's type has no identity.
    NoIdentity,
    /// The request does not name the operation's target.
    NoTarget,
    /// The session does not hold the target, and nothing says it is missing.
    NotHeld,
    /// Writes the held record does not show yet are remembered.
    Behind,
    /// The rendered record does not fit the response schema.
    RenderFailed,
}

impl Fallback {
    /// The reason as logged (`exec_fallback`).
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Fallback::ConsistencyOff => "consistency_off",
            Fallback::NotPlain => "not_plain",
            Fallback::Seeded => "seeded",
            Fallback::NoIdentity => "no_identity",
            Fallback::NoTarget => "no_target",
            Fallback::NotHeld => "not_held",
            Fallback::Behind => "behind",
            Fallback::RenderFailed => "render_failed",
        }
    }
}

mod action;
mod create;
mod list;
mod writes;

/// What [`run`] did with a call.
pub(super) enum Ran {
    /// The call is answered.
    Answered(Box<GeneratedResponse>),
    /// The executor's answer to a write, which [`super::complete`] stores, publishes and
    /// serves as it does a generated one, with no LLM call.
    #[expect(
        dead_code,
        reason = "the write executors (steps 2, 3 and 5 of the plan) answer through it"
    )]
    Complete(Box<GeneratedResponse>),
    /// The call generates, with what was recalled of the writes its held target does not
    /// show yet, when that was read.
    Generate(Option<Earlier>),
}

/// The call's target as [`super::prepare`] found it.
pub(super) struct Held<'h> {
    /// The held target, if any.
    pub(super) target: Option<&'h StoredResource>,
    /// Its id, `None` when the key holds another owner's record.
    pub(super) target_id: Option<&'h str>,
}

/// The kind the description gives `plan`'s operation when the executor handles that
/// kind: `Ok` when the call executes, `Err` with the reason it does not. `None` for a
/// kind the executor does not handle.
pub(super) fn classify(
    rt: &crate::MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Option<Result<OpKind, (OpKind, Fallback)>> {
    let op = plan.op;
    let desc = rt.described().op(&op.id);
    let kind = desc.and_then(|d| d.kind).or_else(|| OpKind::of(op))?;
    if !executes(kind) {
        return None;
    }
    let fallback = if !rt.consistency {
        Some(Fallback::ConsistencyOff)
    } else if !plain(plan) {
        Some(Fallback::NotPlain)
    } else if mb_engine::seed::request_seed(call).is_some() {
        Some(Fallback::Seeded)
    } else if op.identity.is_none()
        || desc
            .and_then(|d| d.r#type.as_deref())
            .or(op.resource_type.as_deref())
            .is_none()
    {
        Some(Fallback::NoIdentity)
    } else if target_id(op, call).is_none() {
        Some(Fallback::NoTarget)
    } else {
        None
    };
    Some(match fallback {
        Some(reason) => Err((kind, reason)),
        None => Ok(kind),
    })
}

/// Whether the executor takes calls of `kind`.
fn executes(kind: OpKind) -> bool {
    match kind {
        OpKind::Read => true,
        OpKind::Create => create::EXECUTES,
        OpKind::Update | OpKind::Replace | OpKind::Delete => writes::EXECUTES,
        OpKind::List => list::EXECUTES,
        OpKind::Action => action::EXECUTES,
    }
}

/// Executes a call [`classify`] accepts, or takes today's path for one it does not.
pub(super) async fn run(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: Held<'_>,
    response_schema: Option<&Value>,
) -> Ran {
    match classify(state.rt, plan, call) {
        Some(Ok(kind)) => match kind {
            OpKind::Read => read(state, plan, call, held, response_schema, kind).await,
            OpKind::Create => create::run(state, plan, call, held, response_schema).await,
            OpKind::Update | OpKind::Replace | OpKind::Delete => {
                writes::run(state, plan, call, held, response_schema, kind).await
            }
            OpKind::List => list::run(state, plan, call, held, response_schema).await,
            OpKind::Action => action::run(state, plan, call, held, response_schema).await,
        },
        Some(Err((kind, reason))) => {
            fell_back(state.rt, plan, kind, reason.as_str());
            unclassified(state, plan, call, held, response_schema).await
        }
        None => unclassified(state, plan, call, held, response_schema).await,
    }
}

/// A read of one record: the held record rendered, else generation.
async fn read(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: Held<'_>,
    response_schema: Option<&Value>,
    kind: OpKind,
) -> Ran {
    let rt = state.rt;
    // A missing record the session knows of (deleted, or unknown under
    // `unknown_ids: 404`) was refused before this; any other generates.
    let Some(stored) = held.target else {
        fell_back(rt, plan, kind, Fallback::NotHeld.as_str());
        return Ran::Generate(None);
    };
    match held_read(state, plan, call, stored, held.target_id, response_schema).await {
        HeldRead::Served(served) => {
            answered(rt, plan, kind);
            Ran::Answered(served)
        }
        HeldRead::Behind(earlier) => {
            fell_back(rt, plan, kind, Fallback::Behind.as_str());
            Ran::Generate(Some(earlier))
        }
        HeldRead::Unrendered(earlier) => {
            fell_back(rt, plan, kind, Fallback::RenderFailed.as_str());
            Ran::Generate(earlier)
        }
    }
}

/// Today's path for a call the executor does not take: a plain read of a held record
/// is still served from state when it can be.
async fn unclassified(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: Held<'_>,
    response_schema: Option<&Value>,
) -> Ran {
    let Some(stored) = held.target else {
        return Ran::Generate(None);
    };
    if !(matches!(plan.op.kind, OperationKind::Query) && plain(plan)) {
        return Ran::Generate(None);
    }
    match held_read(state, plan, call, stored, held.target_id, response_schema).await {
        HeldRead::Served(served) => Ran::Answered(served),
        HeldRead::Behind(earlier) => Ran::Generate(Some(earlier)),
        HeldRead::Unrendered(earlier) => Ran::Generate(earlier),
    }
}

/// What [`held_read`] made of a held record.
enum HeldRead {
    /// Rendered and valid.
    Served(Box<GeneratedResponse>),
    /// Writes it does not show yet are remembered: the read generates, with the
    /// rendered record kept for a catch-up that fails.
    Behind(Earlier),
    /// It does not render into the answer, with what was recalled, when that was read.
    Unrendered(Option<Earlier>),
}

/// `stored` answered as the call's operation answers its record: with the members the
/// description derives from its children when that answer suits the schema, else as
/// stored, in the envelope the operation holds it in (with what the session's answers
/// said of that envelope, [`envelopes`]).
async fn held_read(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    stored: &StoredResource,
    target_id: Option<&str>,
    response_schema: Option<&Value>,
) -> HeldRead {
    let rt = state.rt;
    let op = plan.op;
    let derived = derived::filled(state, &plan.session, stored)
        .instrument(state_span())
        .await;
    let held = |kept: Option<&Map<String, Value>>| {
        derived
            .as_ref()
            .and_then(|filled| serve_held(rt, op, filled, kept, response_schema))
            .or_else(|| serve_held(rt, op, stored, kept, response_schema))
    };
    // What the schema alone tells of the envelope costs no read; what the session's
    // answers said of it is read only for an answer that stands.
    let (bare, enveloped) = (held(None), envelopes::applies(rt, op));
    if bare.is_none() && !enveloped {
        return HeldRead::Unrendered(None);
    }
    let mut earlier = write_memory::recall(state, plan, call, target_id, true)
        .instrument(state_span())
        .await;
    if earlier.forces(rt, plan, call) {
        earlier.stored_answer = bare.map(Box::new);
        return HeldRead::Behind(earlier);
    }
    let kept = match enveloped {
        true => {
            envelopes::recall(state, plan)
                .instrument(state_span())
                .await
        }
        false => None,
    };
    match kept.and_then(|kept| held(Some(&kept))).or(bare) {
        Some(mut served) => {
            write_memory::note_not_caught_up(&mut served, plan, Some(stored), &earlier);
            HeldRead::Served(Box::new(served))
        }
        None => HeldRead::Unrendered(Some(earlier)),
    }
}

/// Logs that the executor answered the call.
fn answered(rt: &crate::MockRuntime, plan: &CallPlan<'_>, kind: OpKind) {
    tracing::info!(
        mock = %rt.name,
        operation = %plan.op.id,
        exec = kind_name(kind),
        "answered by the executor"
    );
}

/// Logs why a classified call goes on to generation: a [`Fallback`]'s name, or a reason
/// of one kind's executor (`undescribed_param`).
fn fell_back(rt: &crate::MockRuntime, plan: &CallPlan<'_>, kind: OpKind, reason: &str) {
    tracing::info!(
        mock = %rt.name,
        operation = %plan.op.id,
        exec = kind_name(kind),
        exec_fallback = reason,
        "the executor fell back to generation"
    );
}

/// The kind as the description writes it.
fn kind_name(kind: OpKind) -> &'static str {
    match kind {
        OpKind::Create => "create",
        OpKind::Read => "read",
        OpKind::List => "list",
        OpKind::Update => "update",
        OpKind::Replace => "replace",
        OpKind::Delete => "delete",
        OpKind::Action => "action",
    }
}
