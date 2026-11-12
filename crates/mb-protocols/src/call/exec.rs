//! The state-first executor (`docs/superpowers/specs/2026-11-06-state-first-engine-design.md`):
//! for a resource operation the effective world description classifies, the store
//! decides the answer and the stored record is rendered into the response schema.
//! Everything it cannot classify, and every call it falls back on, takes the generated
//! path unchanged.
//!
//! [`run`] is the seam [`super::prepare`] calls once the call's target, refusals and
//! tokens are settled. A classified call is logged at `info` with the field `exec` (its
//! kind) when the executor answers it, and with `exec_fallback` (a [`Fallback`]) when it
//! goes on to generation, with `exec_fallback_on` naming the query parameter or body
//! member that caused it where one did (`undescribed_param`, `undeclared_member`,
//! `read_only_member`), a read's or a create's `render_failed` (and a create's
//! `record_invalid`) with `render_error` saying why, on the request record too; executor
//! answers are tagged `state`, as stored answers are.
//!
//! An executor answer is final: a write is stored through [`commit`], which keeps only
//! the side effects of a write (store, publish, webhooks, idempotency replay, cache),
//! never through [`super::complete`]'s reconciliation of a generated answer.

use mb_engine::GeneratedResponse;
use mb_engine::consistency::property_schema;
use mb_mcm::world::OpKind;
use mb_mcm::{Binding, Effect, OperationKind};
use mb_state::StoredResource;
use serde_json::{Map, Value};
use tracing::Instrument;

use super::write_memory::Earlier;
use super::writes::store_one;
use super::{
    CallPlan, OperationCall, StateAccess, collection_of, computed, deleted, envelopes, id_string,
    minted, named_target, plain, records, replays, serve_held, side_writes, state_lists,
    state_span, tokens, world, write_memory,
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
    /// A create's record would not render into one of its type's reads, or its answer
    /// does not render: nothing is stored.
    RecordInvalid,
    /// What the call adds would take its target past a cap (a queue's messages).
    OverCap,
    /// The read asks for embedded members its held record does not hold yet
    /// ([`super::query_roles`]): they are generated, and kept with the record.
    EmbedMissing,
    /// The read asks to expand a reference to a record the session does not hold
    /// ([`super::expansions`]): it is generated, and stored under the id.
    ReferenceUnheld,
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
            Fallback::RecordInvalid => "record_invalid",
            Fallback::OverCap => "over_cap",
            Fallback::EmbedMissing => "embed_missing",
            Fallback::ReferenceUnheld => "reference_unheld",
        }
    }
}

mod action;
mod create;
pub(super) use create::issued_aliases;
mod embedded;
mod fields;
mod held_gaps;
mod linked;
mod list;
mod patches;
pub(crate) mod pool;
mod queue;
mod redraw;
mod writes;

pub(super) use create::{initial_token, unreadable};
pub(super) use list::agree_generated as agree_generated_list;
pub(super) use list::fallback_lists_marked;
pub(super) use queue::forget as forget_queue;

/// Whether `call` sends `op`'s described fields parameter: its answer may then be a
/// part of the record, not the record.
pub(super) fn narrows_fields(
    rt: &crate::MockRuntime,
    op: &mb_mcm::Operation,
    call: &OperationCall,
) -> bool {
    fields::param(rt, op).is_some_and(|param| call.query.contains_key(param))
}

/// What [`run`] did with a call.
pub(super) enum Ran {
    /// The call is answered.
    Answered(Box<GeneratedResponse>),
    /// The call generates, with what was recalled of the writes its held target does not
    /// show yet, when that was read.
    Generate(Option<Earlier>),
    /// The call generates after a fill it already spent these tokens on, which the
    /// generated answer's usage counts too.
    Filled(mb_llm::Usage),
    /// [`Ran::Generate`] after a fill it already spent these tokens on (a held record
    /// completed for a read that still does not render), which the generated answer's
    /// usage counts too.
    Spent(Option<Earlier>, mb_llm::Usage),
}

/// The call's target as [`super::prepare`] found it.
pub(super) struct Held<'h> {
    /// The held target, if any.
    pub(super) target: Option<&'h StoredResource>,
    /// Its id, `None` when the key holds another owner's record.
    pub(super) target_id: Option<&'h str>,
    /// A read in another language than the target was stored in, with no variant kept
    /// for it (see [`super::variants`]): generated, knowing the target.
    pub(super) generates: bool,
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
    let kind = match desc.and_then(|d| d.kind) {
        Some(kind) => kind,
        None => match OpKind::of(op)? {
            OpKind::Create if acts_on_own_record(rt, op) => OpKind::Action,
            kind => kind,
        },
    };
    if !executes(kind) {
        return None;
    }
    let fallback = if !rt.consistency {
        Some(Fallback::ConsistencyOff)
    } else if !plain(plan) {
        Some(Fallback::NotPlain)
    } else if mb_engine::seed::request_seed(call).is_some() {
        Some(Fallback::Seeded)
    } else if !desc
        .and_then(|d| d.r#type.as_deref())
        .or(op.resource_type.as_deref())
        .is_some_and(|t| identified(rt, op, kind, t))
    {
        Some(Fallback::NoIdentity)
    } else if !matches!(kind, OpKind::Create | OpKind::List) && named_target(rt, op, call).is_none()
    {
        Some(Fallback::NoTarget)
    } else {
        None
    };
    Some(match fallback {
        Some(reason) => Err((kind, reason)),
        None => Ok(kind),
    })
}

/// Whether `op`, an HTTP `POST` the contract gives no kind but a create's, acts on a
/// record of its own answer's type rather than adding one: its path goes on, past
/// static segments only, from the route that reads that type by id
/// (`POST /workspaces/{id}/actions/lock` beside `GET /workspaces/{id}`), and no read
/// lists at its own path (a collection under the record, which a `POST` adds to).
/// One answering `201`, or naming a created record's id in a header, adds a record
/// (`POST /files/{id}/copy`).
fn acts_on_own_record(rt: &crate::MockRuntime, op: &mb_mcm::Operation) -> bool {
    let Binding::Http {
        method: mb_mcm::HttpMethod::Post,
        path,
    } = &op.binding
    else {
        return false;
    };
    let status = op.success_output().map_or(200, |o| match o.status {
        mb_mcm::StatusSelector::Code(code) => code,
        _ => 200,
    });
    if status == 201 || super::records::identity_header(rt, op, status).is_some() {
        return false;
    }
    let bundle = rt.bundle();
    let Some(read) = op
        .resource_type
        .as_deref()
        .and_then(|t| rt.index().detail_reads.get(t))
        .and_then(|i| bundle.operations.get(*i))
    else {
        return false;
    };
    let Binding::Http {
        path: read_path, ..
    } = &read.binding
    else {
        return false;
    };
    let segments = |p: &str| -> Vec<String> {
        p.split('/')
            .filter(|s| !s.is_empty())
            .map(|s| match s.starts_with('{') {
                true => "{}".to_owned(),
                false => s.to_owned(),
            })
            .collect()
    };
    let (own, keyed) = (segments(path), segments(read_path));
    let past = match own.strip_prefix(keyed.as_slice()) {
        Some(rest) if keyed.last().is_some_and(|s| s == "{}") => rest,
        _ => return false,
    };
    let listed_here = bundle.operations.iter().any(|other| {
        matches!(other.kind, OperationKind::Query)
            && matches!(&other.binding, Binding::Http { path: p, .. } if segments(p) == own)
    });
    !past.is_empty() && past.iter().all(|s| s != "{}") && !listed_here
}

/// Whether a call of `op`, of `kind` on type `t`, has an identity to work with
/// ([`crate::world_gaps::identified`], the rule the gap report's `no_identity` follows):
/// `op`'s own (where its answer holds the record), else the description's, else that
/// of the type's read by id.
fn identified(rt: &crate::MockRuntime, op: &mb_mcm::Operation, kind: OpKind, t: &str) -> bool {
    let read_by_id = rt
        .index()
        .detail_reads
        .get(t)
        .and_then(|i| rt.bundle().operations.get(*i))
        .is_some_and(|read| read.identity.is_some());
    crate::world_gaps::identified(
        kind,
        op.identity.is_some(),
        rt.described()
            .type_desc(t)
            .and_then(|d| d.identity.as_ref()),
        read_by_id,
    )
}

/// The schemas records of type `t` are read by: its read by id's record and each of
/// its lists' items ([`create::read_schemas`]).
pub(super) fn read_schemas(rt: &crate::MockRuntime, t: &str) -> Vec<Value> {
    create::read_schemas(rt, t)
}

/// The records type `t`'s creates answer ([`create::created_schemas`]).
pub(super) fn created_schemas(rt: &crate::MockRuntime, t: &str) -> Vec<Value> {
    create::created_schemas(rt, t)
}

/// Whether the executor answers some list of records of type `t`: a list whose items
/// have an identity ([`list::identified`]).
pub(super) fn lists(rt: &crate::MockRuntime, t: &str) -> bool {
    list::EXECUTES
        && rt.bundle().operations.iter().any(|op| {
            let kind = rt
                .described()
                .op(&op.id)
                .and_then(|d| d.kind)
                .or_else(|| OpKind::of(op));
            kind == Some(OpKind::List)
                && list::identified(rt, op, OpKind::List)
                && list::item_type(rt, op) == Some(t)
        })
}

/// Whether the executor takes calls of `kind`.
fn executes(kind: OpKind) -> bool {
    match kind {
        OpKind::Read => true,
        OpKind::Create => create::EXECUTES,
        OpKind::Update | OpKind::Replace => writes::EXECUTES,
        OpKind::Delete => true,
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
    let classified = classify(state.rt, plan, call);
    // A described refusal answers first, before anything is stored or generated: on
    // every call but one a scenario or a forced status answers, whether the executor
    // takes it or it generates (seeded, consistency off, a kind it does not take).
    if plain(plan)
        && let Some(refusal) = super::exprs::refusal(state, plan, call, held.target)
            .instrument(state_span())
            .await
    {
        replays::keep(state, plan, call, &refusal)
            .instrument(state_span())
            .await;
        if let Some(Ok(kind) | Err((kind, _))) = classified {
            answered(state.rt, plan, kind);
        }
        return Ran::Answered(Box::new(refusal));
    }
    if held.generates {
        return Ran::Generate(None);
    }
    match classified {
        Some(Ok(kind)) => match kind {
            OpKind::Read if matches!(plan.op.kind, OperationKind::Query) => {
                read(state, plan, call, held, response_schema, kind).await
            }
            OpKind::Read => unclassified(state, plan, call, held, response_schema).await,
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
    if lacks_embeds(rt, plan, call, &held) {
        fell_back(rt, plan, kind, Fallback::EmbedMissing.as_str());
        return Ran::Generate(None);
    }
    if lacks_references(state, plan, call, &held).await {
        fell_back(rt, plan, kind, Fallback::ReferenceUnheld.as_str());
        return Ran::Generate(None);
    }
    // One held in its parent's array member is that item.
    let within = embedded::current(state, plan, call, &held).await;
    // A missing record the session knows of (deleted, or unknown under
    // `unknown_ids: 404`) was refused before this; any other generates.
    let pooled: Option<StoredResource>;
    let mut pooled_id: Option<String> = None;
    let stored = match held.target.or(within.as_ref().map(|e| &e.child)) {
        Some(stored) => stored,
        None => {
            // From the mock's record pool, when it holds the type.
            pooled = pool::read(state, plan, call).await;
            let Some(stored) = pooled.as_ref() else {
                fell_back(rt, plan, kind, Fallback::NotHeld.as_str());
                return Ran::Generate(None);
            };
            pooled_id = Some(stored.id.clone());
            stored
        }
    };
    // A server-set reference it holds as `null` names a record, and is stored so; not
    // for an item held in its parent's array, which is stored with the parent.
    let own = held.target.is_some() || within.is_none();
    let linked = match own {
        true => {
            linked::named(state, (plan, call), stored)
                .instrument(state_span())
                .await
        }
        false => None,
    };
    let stored = linked.as_ref().unwrap_or(stored);
    let target_id = match &pooled_id {
        Some(id) => Some(id.as_str()),
        None => held.target_id,
    };
    let read = held_read(state, plan, call, stored, target_id, response_schema, true);
    match read.await {
        HeldRead::Served(served) => {
            answered(rt, plan, kind);
            Ran::Answered(served)
        }
        HeldRead::Behind(earlier) => {
            fell_back(rt, plan, kind, Fallback::Behind.as_str());
            Ran::Generate(Some(earlier))
        }
        HeldRead::Unrendered(earlier, why) => {
            // A record lacking what the answer requires is completed from state, its
            // stored values kept, rather than generated whole ([`held_gaps`]). Not one
            // with remembered writes it does not show yet: generation catches up on them.
            let remembered = match target_id {
                Some(id) => {
                    write_memory::remembers(state, &plan.session, &stored.resource_type, id)
                        .instrument(state_span())
                        .await
                }
                None => false,
            };
            let done = match remembered {
                true => None,
                false => held_gaps::completed(state, (plan, call), stored, response_schema).await,
            };
            let Some(done) = done else {
                fell_back_unrendered(rt, plan, kind, Fallback::RenderFailed, &why);
                return Ran::Generate(earlier);
            };
            let again = held_read(
                state,
                plan,
                call,
                &done.record,
                target_id,
                response_schema,
                true,
            );
            let (earlier, usage) = match again.await {
                HeldRead::Served(mut served) => {
                    if !held_gaps::keep(state, plan, stored, &done).await {
                        tracing::debug!(
                            operation = %plan.op.id,
                            "a completed record is answered, not stored"
                        );
                    }
                    served.usage = served.usage + done.usage;
                    answered(rt, plan, kind);
                    return Ran::Answered(served);
                }
                HeldRead::Behind(recalled) => {
                    fell_back(rt, plan, kind, Fallback::Behind.as_str());
                    (Some(recalled), done.usage)
                }
                HeldRead::Unrendered(recalled, why) => {
                    fell_back_unrendered(rt, plan, kind, Fallback::RenderFailed, &why);
                    (recalled.or(earlier), done.usage)
                }
            };
            match usage == mb_llm::Usage::default() {
                true => Ran::Generate(earlier),
                false => Ran::Spent(earlier, usage),
            }
        }
    }
}

/// Whether the read asks for members to be embedded that `held`, the session's record
/// it targets, does not hold yet ([`super::query_roles`]): they are generated over it,
/// and kept with it. A record from the pool or held in its parent answers without them.
fn lacks_embeds(
    rt: &crate::MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: &Held<'_>,
) -> bool {
    held.target.is_some_and(|target| {
        !super::query_roles::embeds(rt, plan.op, call)
            .missing(&target.data)
            .is_empty()
    })
}

/// Whether the read asks to expand a reference that `held`, the session's record it
/// targets, names by an id the session holds no record of ([`super::expansions`]): the
/// read generates that record, and it is stored under the id.
async fn lacks_references(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: &Held<'_>,
) -> bool {
    let Some(target) = held.target else {
        return false;
    };
    !super::expansions::unbuilt(state, &plan.session, (plan.op, call), &target.data)
        .await
        .is_empty()
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
    if !(matches!(plan.op.kind, OperationKind::Query) && plain(plan))
        || lacks_embeds(state.rt, plan, call, &held)
        || lacks_references(state, plan, call, &held).await
    {
        return Ran::Generate(None);
    }
    let read = held_read(
        state,
        plan,
        call,
        stored,
        held.target_id,
        response_schema,
        false,
    );
    match read.await {
        HeldRead::Served(served) => Ran::Answered(served),
        HeldRead::Behind(earlier) => Ran::Generate(Some(earlier)),
        HeldRead::Unrendered(earlier, _) => Ran::Generate(earlier),
    }
}

/// What [`held_read`] made of a held record.
enum HeldRead {
    /// Rendered and valid.
    Served(Box<GeneratedResponse>),
    /// Writes it does not show yet are remembered: the read generates, with the
    /// rendered record kept for a catch-up that fails.
    Behind(Earlier),
    /// It does not render into the answer, with what was recalled, when that was read,
    /// and why ([`unrendered`]).
    Unrendered(Option<Earlier>, String),
}

/// `stored` answered as the call's operation answers its record: with the members the
/// description derives from its children when that answer suits the schema, else as
/// stored, in the envelope the operation holds it in (with what the session's answers
/// said of that envelope, [`envelopes`]). For the executor (`drawing`), the envelope's
/// members that nothing kept and that their schema does not tell are `true` for a
/// boolean, else the faker's ([`mb_engine::Engine::envelope_draws`]); today's path
/// generates the read instead.
async fn held_read(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    stored: &StoredResource,
    target_id: Option<&str>,
    response_schema: Option<&Value>,
    drawing: bool,
) -> HeldRead {
    let rt = state.rt;
    let op = plan.op;
    // Drawn only once an answer without them does not stand.
    let drawn = std::sync::OnceLock::new();
    let drawn = || {
        drawn.get_or_init(|| match drawing {
            true => rt.engine.envelope_draws(&rt.bundle, op, response_schema),
            false => Map::new(),
        })
    };
    // What the session's answers said of the envelope, over the draws.
    let over_drawn = |kept: Option<&Map<String, Value>>| {
        let mut values = drawn().clone();
        values.extend(kept.cloned().unwrap_or_default());
        (!values.is_empty()).then_some(values)
    };
    let derived = computed::filled(state, (plan, call), stored)
        .instrument(state_span())
        .await;
    let held = |kept: Option<&Map<String, Value>>| {
        derived
            .as_ref()
            .and_then(|filled| serve_held(rt, op, filled, kept, response_schema))
            .or_else(|| serve_held(rt, op, stored, kept, response_schema))
    };
    // `kept` alone, else over the draws.
    let held = |kept: Option<&Map<String, Value>>| {
        held(kept).or_else(|| {
            let values = over_drawn(kept).filter(|v| Some(v) != kept)?;
            held(Some(&values))
        })
    };
    // Why it does not render, for the executor's log line.
    let why = |kept: Option<&Map<String, Value>>| match drawing {
        true => unrendered(rt, op, stored, over_drawn(kept).as_ref(), response_schema),
        false => String::new(),
    };
    // What the schema alone tells of the envelope costs no read; what the session's
    // answers said of it is read only for an answer that stands.
    let (bare, enveloped) = (held(None), envelopes::applies(rt, op));
    if bare.is_none() && !enveloped {
        return HeldRead::Unrendered(None, why(None));
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
    let served = kept
        .as_ref()
        .and_then(|kept| held(Some(kept)))
        .or(bare)
        .map(|mut served| {
            // A nullable member the selection asks for that the record lacks is `null`.
            // With a model, only in what the description computes: the model writes the rest.
            if op.protocol == mb_mcm::Protocol::Graphql
                && !plan.renames
                && let (Some(schema), Some(body)) = (response_schema, served.body.as_mut())
            {
                let within = plan
                    .engine
                    .has_provider()
                    .then(|| held_gaps::derived_members(rt, &stored.resource_type));
                held_gaps::null_selected(rt, schema, body, within.as_deref());
            }
            served
        })
        .filter(|served| selected_held(rt, op, response_schema, served));
    match served {
        Some(mut served) => {
            write_memory::note_not_caught_up(&mut served, plan, Some(stored), &earlier);
            // A described fields parameter narrows the record the answer holds.
            fields::narrow_read(rt, op, call, served.body.as_mut());
            // Embedded members the read does not ask for are left out.
            super::query_roles::embeds(rt, op, call).leave_out_unasked(served.body.as_mut());
            // References it asks to be expanded are the records the session holds.
            if let Some(body) = served.body.as_mut() {
                super::expansions::expand(state, &plan.session, (op, call), body)
                    .instrument(state_span())
                    .await;
            }
            HeldRead::Served(Box::new(served))
        }
        None => HeldRead::Unrendered(Some(earlier), why(kept.as_ref())),
    }
}

/// Whether `served`, a held GraphQL record rendered for a selection, holds every member
/// that selection asks for (`response_schema`, narrowed to it), at every depth. The
/// narrowed schema leaves a nullable member optional, so a record stored by a smaller
/// selection (a user's repositories without their `languages`) would otherwise answer
/// it `null`; such a read generates instead, over the record. What the read's own record
/// and the objects it holds lack was set to `null` before this
/// ([`held_gaps::null_selected`], offline): only another record it embeds can still lack one.
/// Other protocols' schemas are not narrowed to what the client reads, so they are not
/// checked.
fn selected_held(
    rt: &crate::MockRuntime,
    op: &mb_mcm::Operation,
    response_schema: Option<&Value>,
    served: &GeneratedResponse,
) -> bool {
    if op.protocol != mb_mcm::Protocol::Graphql {
        return true;
    }
    let (Some(schema), Some(body)) = (response_schema, served.body.as_ref()) else {
        return true;
    };
    holds_selected(rt, schema, schema, body, 0)
}

/// Bounds [`holds_selected`]'s walk (recursive types).
const MAX_SELECTED_DEPTH: usize = 16;

fn holds_selected(
    rt: &crate::MockRuntime,
    root: &Value,
    schema: &Value,
    value: &Value,
    depth: usize,
) -> bool {
    if depth > MAX_SELECTED_DEPTH {
        return true;
    }
    let schema = match schema.get("$ref").and_then(Value::as_str) {
        Some(r) => {
            let name = r.rsplit('/').next().unwrap_or_default();
            match root
                .get("$defs")
                .and_then(|d| d.get(name))
                .or_else(|| rt.bundle.defs.get(name))
            {
                Some(found) => found,
                None => return true,
            }
        }
        None => schema,
    };
    match value {
        Value::Object(obj) => {
            let branches: Vec<&Value> = std::iter::once(schema)
                .chain(
                    ["anyOf", "oneOf", "allOf"]
                        .iter()
                        .filter_map(|k| schema.get(*k).and_then(Value::as_array))
                        .flatten(),
                )
                .collect();
            branches.iter().all(|branch| {
                let branch = match branch.get("$ref") {
                    Some(_) if !std::ptr::eq(*branch, schema) => {
                        return holds_selected(rt, root, branch, value, depth + 1);
                    }
                    _ => *branch,
                };
                let Some(props) = branch.get("properties").and_then(Value::as_object) else {
                    return true;
                };
                // A branch of another type (`__typename` fixed elsewhere) says nothing.
                let other_type = props
                    .get("__typename")
                    .and_then(|t| t.get("const"))
                    .is_some_and(|t| obj.get("__typename").is_some_and(|v| v != t));
                other_type
                    || props.iter().all(|(name, member)| match obj.get(name) {
                        Some(v) => holds_selected(rt, root, member, v, depth + 1),
                        None => false,
                    })
            })
        }
        Value::Array(items) => {
            let item = std::iter::once(schema)
                .chain(
                    ["anyOf", "oneOf"]
                        .iter()
                        .filter_map(|k| schema.get(*k).and_then(Value::as_array))
                        .flatten(),
                )
                .find_map(|s| s.get("items"));
            match item {
                Some(item) => items
                    .iter()
                    .all(|v| holds_selected(rt, root, item, v, depth + 1)),
                None => true,
            }
        }
        _ => true,
    }
}

/// Why `stored` does not render into `op`'s answer, as logged (`render_error`): the
/// first violation of the rendered answer (`<pointer> <keyword>`), or that its envelope
/// requires a member nothing gives a value.
fn unrendered(
    rt: &crate::MockRuntime,
    op: &mb_mcm::Operation,
    stored: &StoredResource,
    kept: Option<&Map<String, Value>>,
    response_schema: Option<&Value>,
) -> String {
    let engine = &rt.engine;
    let Some(answer) = engine.held_answer(&rt.bundle, op, &stored.data, kept, response_schema)
    else {
        return "envelope member unfilled".to_owned();
    };
    match engine.stored_violations(&rt.bundle, op, &answer, response_schema) {
        Some(found) => found.into_iter().next().map_or_else(
            || "no violation".to_owned(),
            |(pointer, keyword)| match pointer.as_str() {
                "" => format!("<root> {keyword}"),
                at => format!("{at} {keyword}"),
            },
        ),
        None => "no response schema".to_owned(),
    }
}

/// Counts the repair the call's answer took, when it took one
/// ([`redraw::ANSWER_REPAIRS_METRIC`]): once per call, when the executor answers it or
/// leaves it to generation.
fn count_repair(rt: &crate::MockRuntime, plan: &CallPlan<'_>) {
    if let Some(outcome) = plan.exprs.repair_to_count() {
        metrics::counter!(
            redraw::ANSWER_REPAIRS_METRIC,
            "mock" => rt.name.clone(),
            "operation" => plan.op.id.clone(),
            "outcome" => outcome
        )
        .increment(1);
    }
}

/// Logs that the executor answered the call, and counts the repair its answer took
/// ([`count_repair`]).
fn answered(rt: &crate::MockRuntime, plan: &CallPlan<'_>, kind: OpKind) {
    count_repair(rt, plan);
    tracing::info!(
        mock = %rt.name,
        operation = %plan.op.id,
        exec = kind_name(kind),
        "answered by the executor"
    );
}

/// Logs why the executor did not answer a classified call (today's path then answers it,
/// from state where it can): a [`Fallback`]'s name, or a reason
/// of one kind's executor (`undescribed_param`).
fn fell_back(rt: &crate::MockRuntime, plan: &CallPlan<'_>, kind: OpKind, reason: &str) {
    fell_back_on(rt, plan, kind, &Stop::from(reason));
}

/// [`fell_back`] for `reason` because the record did not render, saying `why`
/// (`render_error`), on the log line and the request record.
fn fell_back_unrendered(
    rt: &crate::MockRuntime,
    plan: &CallPlan<'_>,
    kind: OpKind,
    reason: Fallback,
    why: &str,
) {
    plan.exprs.unrendered(reason.as_str(), why);
    count_repair(rt, plan);
    tracing::info!(
        mock = %rt.name,
        operation = %plan.op.id,
        exec = kind_name(kind),
        exec_fallback = reason.as_str(),
        render_error = %super::log_capped(why),
        "not answered by the executor"
    );
}

/// Why one kind's executor did not answer a call: its reason (`exec_fallback`), and the
/// one query parameter or body member that caused it, when one did (`exec_fallback_on`).
pub(super) struct Stop<'r> {
    reason: &'r str,
    on: Option<String>,
}

impl<'r> From<&'r str> for Stop<'r> {
    fn from(reason: &'r str) -> Self {
        Self { reason, on: None }
    }
}

impl<'r> Stop<'r> {
    /// `reason`, caused by query parameter or body member `on`.
    pub(super) fn on(reason: &'r str, on: &str) -> Self {
        Self {
            reason,
            on: Some(on.to_owned()),
        }
    }
}

/// [`fell_back`] for `stop`, naming what caused it (`exec_fallback_on`) when it says.
fn fell_back_on(rt: &crate::MockRuntime, plan: &CallPlan<'_>, kind: OpKind, stop: &Stop<'_>) {
    plan.exprs.fell_back(stop.reason, stop.on.as_deref());
    count_repair(rt, plan);
    match &stop.on {
        Some(on) => tracing::info!(
            mock = %rt.name,
            operation = %plan.op.id,
            exec = kind_name(kind),
            exec_fallback = stop.reason,
            exec_fallback_on = %super::log_capped(on),
            "not answered by the executor"
        ),
        None => tracing::info!(
            mock = %rt.name,
            operation = %plan.op.id,
            exec = kind_name(kind),
            exec_fallback = stop.reason,
            "not answered by the executor"
        ),
    }
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

/// The record a write's answer renders, as [`writes::written`] rendered it: rendered
/// again by [`commit`] once the operation's effects wrote, since its computed members
/// may read what they wrote.
pub(super) struct Rendered<'a> {
    /// The id the answer names.
    pub(super) id: String,
    /// The record as written, before its computed members are filled.
    pub(super) record: StoredResource,
    /// The schema the answer is rendered to.
    pub(super) schema: Option<&'a Value>,
    /// Whether it is a create's record ([`computed::filled_new`]).
    pub(super) new: bool,
}

/// What a write does to its record.
pub(super) enum Write {
    /// Stores `data` under `id`, over what is held there.
    Put {
        /// The record's id.
        id: String,
        /// The record.
        data: Value,
    },
    /// Removes the record held under `id`.
    Remove {
        /// The record's id.
        id: String,
    },
    /// Stores `data`, the parent record held under `id` with the written record
    /// `child` changed or removed in its array member ([`embedded`]): in the parent's
    /// collection, as it was. A removed child is marked deleted as a removed record is.
    Within {
        /// The parent's id.
        id: String,
        /// The parent record.
        data: Value,
        /// The written record, as it was held.
        child: StoredResource,
        /// Whether the write removed it.
        removed: bool,
    },
}

/// Applies a write the executor decided (of a record of type `t`, or of its parent's
/// record for a [`Write::Within`]), with the side effects every write has, and
/// returns `answer` as it is: the session listed, the write noted as the session's
/// latest and told to its list routes' memory, the record stored in its collection (a create noted as a
/// member of its listed collection; a write of a held record that changed its identity
/// moved to its new key unless another record holds it, in the old one's place in its
/// collection and its type's creation order, the old key removed, marked deleted and its
/// write memory settled, and the old values of
/// its described alternate members retired, [`deleted::retire`]), a removal marked deleted under its route's parent
/// and its tokens ended, the requests write memory kept for it forgotten (the executor's
/// record is authoritative), the entity tag's state, the answer kept for a repeat of its
/// idempotency key, the session's cached answers cleared, the webhooks sent, and the
/// session kept alive. The store publishes the change. The operation's effects run once
/// the write landed ([`side_writes::apply`]); when one wrote, the record `rendered`
/// names is rendered again into the answer's body, its computed members reading the
/// session as the effects left it.
pub(super) async fn commit(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    (t, held): (&str, Option<&StoredResource>),
    (write, rendered): (Write, Option<Rendered<'_>>),
    mut answer: GeneratedResponse,
) -> GeneratedResponse {
    async {
        let rt = state.rt;
        let op = plan.op;
        let session = &plan.session;
        let collection = collection_of(op, call);
        // What the operation's effects see of its target, when it has any: the record
        // as the write leaves it (as it was held, for a removal; the child as written,
        // or as held when removed, for a write within its parent), and its type.
        let effected = rt
            .described()
            .expressions(&op.id)
            .is_some_and(|o| !o.effects.is_empty())
            .then(|| match &write {
                Write::Put { data, .. } => (t.to_string(), data.clone()),
                Write::Remove { .. } => {
                    (t.to_string(), held.map_or(Value::Null, |h| h.data.clone()))
                }
                // The child as written, when the answer renders it.
                Write::Within { child, .. } => match &rendered {
                    Some(r) => (child.resource_type.clone(), r.record.data.clone()),
                    None => (child.resource_type.clone(), child.data.clone()),
                },
            });
        // The record the write was of: the child for a write within its parent.
        let mut written = None;
        let id = match write {
            Write::Within {
                id,
                data,
                child,
                removed,
            } => {
                let place = (None, None, true);
                store_one(state, session, (op, t), (id.clone(), data), place).await;
                if removed && state.tracks_deleted {
                    let (ct, cid) = (child.resource_type.as_str(), child.id.as_str());
                    deleted::mark(state, session, (op, call), ct, cid, Some(&child)).await;
                }
                written = Some(child.id);
                id
            }
            Write::Put { id, data } => {
                let creates = op.kind == OperationKind::Command(Effect::Create);
                if creates {
                    state_lists::mark_created(state, session, (op, t), collection.as_ref(), &id)
                        .await;
                }
                let held = held.filter(|_| !creates);
                if let Some(held) = held {
                    retire_names(state, (plan, call), t, held, &data).await;
                }
                // A write that rewrote the identity moves the record to its new key, in
                // the old one's place in its collection and its type's creation order:
                // the old key is gone, as a delete leaves it. A new key another record
                // holds is left to it, and the record stays where it was.
                let mut moved = held.and_then(|h| re_keyed(rt, op, t, (&id, h), &data));
                if let Some(new) = &moved
                    && state.get(session, t, new).await.is_some()
                {
                    moved = None;
                }
                let was_held = held.is_some() && moved.is_none();
                let id = match (moved, held) {
                    (Some(new), Some(gone)) => {
                        // Moved before the delete, which forgets the old key's place.
                        let renamed = StoredResource {
                            resource_type: t.to_string(),
                            id: new.clone(),
                            data: data.clone(),
                        };
                        rt.collections().rename(session, gone, &renamed);
                        state.delete(session, t, &id).await;
                        state_lists::note_renamed(state, session, t, (&id, &new)).await;
                        if state.tracks_deleted {
                            deleted::mark(state, session, (op, call), t, &id, None).await;
                        }
                        tokens::forget(state, session, t, &id).await;
                        write_memory::settled(state, session, t, &id).await;
                        new
                    }
                    _ => id,
                };
                let place = (collection, Some(&call.query), was_held);
                store_one(state, session, (op, t), (id.clone(), data), place).await;
                id
            }
            Write::Remove { id } => {
                state.delete(session, t, &id).await;
                if let Some(gone) = held {
                    rt.collections().forget(session, gone);
                }
                // The executor removed it, so its own answer, whatever members the faker
                // gave it, is no evidence it stayed: a GraphQL delete marks it as REST's does.
                let graphql = matches!(op.binding, Binding::Graphql { .. });
                if state.tracks_deleted || (graphql && deleted::marks_by_id(rt, plan)) {
                    deleted::mark(state, session, (op, call), t, &id, held).await;
                }
                tokens::forget(state, session, t, &id).await;
                // A queue's messages go with it.
                queue::forget(state, session, t, &id).await;
                id
            }
        };
        // The operation's effects, once its own write landed.
        if let Some((target_type, target)) = &effected
            && state.available()
        {
            let target_id = written.as_deref().unwrap_or(&id);
            let wrote =
                side_writes::apply(state, (plan, call), target_type, (target_id, target)).await;
            if wrote && let Some(rendered) = &rendered {
                render_again(state, (plan, call), rendered, &mut answer).await;
            }
        }
        // What earlier answers said of a deleted record goes, a soft-deleted one too.
        world::after_delete(
            state,
            plan,
            Some(written.as_deref().unwrap_or(&id)),
            &answer,
        )
        .await;
        // The session's latest write, and what its list routes remember of it.
        write_memory::after_route_write(state, plan, call, &answer, (false, false)).await;
        // A session only reads had written to is listed once a write lands.
        let (_, unlisted) = world::recall_answers(state, plan).await;
        world::list_session(state, plan, unlisted, &answer).await;
        write_memory::settled(state, session, t, &id).await;
        tokens::after_answer(state, plan, t, &id).await;
        minted::after_answer(state, plan, call, &answer).await;
        world::after_answer(state, plan, call, &answer).await;
        replays::keep(state, plan, call, &answer).await;
        if let Some(cache) = rt.state.response_cache() {
            cache.clear_session(session);
        }
        #[cfg(any(
            feature = "http-rest",
            feature = "graphql",
            feature = "grpc",
            feature = "soap"
        ))]
        {
            // A write within a parent is one of the child, named by its own id.
            let target = written.as_deref().unwrap_or(&id);
            crate::webhooks::emit::emit(rt, &plan.engine, op, call, &answer, Some(target), session);
        }
        answer
    }
    .instrument(state_span())
    .await
}

/// `answer`'s body rendered again from `rendered`'s record with its computed members
/// filled as the session now holds it (a long-running answer's result member set in
/// the body it has); left as it is when that does not render.
async fn render_again(
    state: &mut StateAccess<'_>,
    at: (&CallPlan<'_>, &OperationCall),
    rendered: &Rendered<'_>,
    answer: &mut GeneratedResponse,
) {
    let (plan, call) = at;
    let rt = state.rt;
    if !computed::describes(rt, &rendered.record.resource_type) {
        return;
    }
    let filled = match rendered.new {
        true => computed::filled_new(state, at, &rendered.record).await,
        false => computed::filled(state, at, &rendered.record).await,
    };
    let record = filled.as_ref().unwrap_or(&rendered.record);
    let long_running = rt
        .described()
        .op(&plan.op.id)
        .and_then(|d| d.answer.as_ref())
        .and_then(|a| a.long_running.as_ref());
    let body = match long_running {
        Some(lr) => answer.body.clone().and_then(|mut body| {
            mb_mcm::set_member_at(&mut body, &lr.result, record.data.clone()).then_some(())?;
            rt.engine
                .serve_stored(rt.bundle(), plan.op, &body, rendered.schema)?
                .body
        }),
        None => writes::written(
            rt,
            plan,
            call,
            (&rendered.id, &record.data),
            rendered.schema,
        )
        .and_then(|again| again.body),
    };
    if let Some(body) = body {
        answer.body = Some(body);
    }
}

/// The new id of `held`, held under `id`, once a write leaves it as `data`: the value of
/// its identity member, when the write changed it from `id`.
fn re_keyed(
    rt: &crate::MockRuntime,
    op: &mb_mcm::Operation,
    t: &str,
    (id, held): (&str, &StoredResource),
    data: &Value,
) -> Option<String> {
    let member = records::identity_member(rt, op, t);
    let was = mb_mcm::member_at(&held.data, member).and_then(id_string)?;
    let now = mb_mcm::member_at(data, member).and_then(id_string)?;
    (was == id && now != was).then_some(now)
}

/// Retires the names a write took from `held` ([`deleted::retire`]): the values of its
/// type's described alternate members that `data`, what the write leaves, no longer
/// holds.
async fn retire_names(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: &str,
    held: &StoredResource,
    data: &Value,
) {
    if !state.tracks_deleted {
        return;
    }
    let rt = state.rt;
    let current = deleted::alternate_values(rt, t, data);
    let retired: Vec<String> = deleted::alternate_values(rt, t, &held.data)
        .into_iter()
        .filter(|name| !current.contains(name))
        .collect();
    if retired.is_empty() {
        return;
    }
    let at = (plan.op, call);
    deleted::retire(state, &plan.session, at, t, (&retired, &current)).await;
}

/// The success answer of `op` with no body, when its success response declares none
/// (a `204`).
pub(super) fn bodiless(op: &mb_mcm::Operation) -> Option<GeneratedResponse> {
    let variant = op.success_output()?;
    if variant.schema.is_some() {
        return None;
    }
    let status = match variant.status {
        mb_mcm::StatusSelector::Code(code) => code,
        _ => 200,
    };
    Some(GeneratedResponse {
        status,
        content_type: None,
        body: None,
        source: mb_engine::Source::State,
        usage: mb_llm::Usage::default(),
        fallback_reason: None,
        effects: Vec::new(),
        effect_outcomes: Vec::new(),
        dropped_effects: 0,
        new_record: false,
        patched: None,
        headers: Vec::new(),
        world_gaps: Vec::new(),
        exec_notes: serde_json::Map::new(),
    })
}

/// `record` (of the call's target type) as the call's operation answers a write of it
/// (spec §4): no body when its success response declares none; the record in the
/// envelope the operation holds it in; the record itself when the schema declares every
/// member it holds; else an acknowledgement (`{ok, id, rev}`, `{deleted: true}`,
/// `{success, data: {id}}`). An acknowledgement is the faker's answer for the call,
/// seeded by the mock's or the request's seed, else by the default one, so a call
/// answers alike in every run. Its boolean members (at the top and one member down) are
/// `true`, as the write succeeded, but a member the record holds keeps the record's
/// value, `false` included: the record's members it declares at its top, and the
/// operation's identity member, take the record's values; an answer holding none of
/// them at its top holds the record's members in the one envelope member whose schema
/// declares the record's identity, and one not declaring the identity at its top holds
/// the record's id there. Failing validation, it tries the faker's answer with the
/// identity alone (at its top, or in that envelope member), then as drawn. `None` only
/// when none of these validates. Tagged `state`.
///
/// An acknowledgement holding the record's members that fails validation is first
/// repaired ([`redraw`]): only the members that break the schema are re-drawn, the
/// identity and every other stored value kept. It falls back to the answers above only
/// when the identity itself breaks the schema or no repair validates. Whatever happened
/// is noted on the call ([`super::CallExprs::set_repair`]: the request record's
/// `answer_repair`), and counted when the executor answers ([`answered`]).
pub(super) fn render(
    rt: &crate::MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    at: (&str, &Value),
    response_schema: Option<&Value>,
) -> Option<GeneratedResponse> {
    let mut repair = None;
    let answer = render_noting(rt, plan, call, at, response_schema, &mut repair);
    // Noted whether or not anything rendered: a call that goes on to generation for it
    // still counts its record as unrepairable.
    if answer.is_none()
        && let Some((outcome, members)) = repair.as_mut()
    {
        *outcome = redraw::UNREPAIRABLE;
        members.clear();
    }
    plan.exprs.set_repair(repair);
    answer
}

/// [`render`], setting `repair` to what became of an acknowledgement that failed
/// validation: its outcome and the members re-drawn.
fn render_noting(
    rt: &crate::MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    (id, record): (&str, &Value),
    response_schema: Option<&Value>,
    repair: &mut Option<(&'static str, Vec<String>)>,
) -> Option<GeneratedResponse> {
    let op = plan.op;
    if let Some(none) = bodiless(op) {
        return Some(none);
    }
    if op.identity.as_ref().is_some_and(|i| i.record.is_some()) {
        let held = StoredResource {
            resource_type: op.resource_type.clone().unwrap_or_default(),
            id: id.to_owned(),
            data: record.clone(),
        };
        // The envelope's required members nothing tells are drawn as a read's are
        // ([`mb_engine::Engine::envelope_draws`]), when each is a scalar (a `status`, a
        // receipt) or an empty list (GraphQL's `userErrors`): an object or array beside
        // the record (Zendesk's `audit`) is another resource the write made, not
        // metadata to draw.
        return serve_held(rt, op, &held, None, response_schema).or_else(|| {
            let drawn = rt.engine.envelope_draws(&rt.bundle, op, response_schema);
            let scalars = drawn
                .values()
                .all(|v| !(v.is_object() || v.as_array().is_some_and(|a| !a.is_empty())));
            (scalars && !drawn.is_empty())
                .then(|| serve_held(rt, op, &held, Some(&drawn), response_schema))
                .flatten()
        });
    }
    let bundle = &rt.bundle;
    let variant = op.success_output()?;
    let schema = response_schema.or(variant.schema.as_ref())?;
    let declares = |name: &str| property_schema(bundle, schema, name).is_some();
    let mut shown: Map<String, Value> = record
        .as_object()
        .map(|members| {
            members
                .iter()
                .filter(|(name, _)| declares(name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    // A nested identity (`info.label`) is in the member that holds it.
    if let Some(field) = op.identity.as_ref().map(|i| i.field.as_str())
        && !field.contains('.')
        && declares(field)
        && !shown.contains_key(field)
    {
        shown.insert(field.to_owned(), Value::String(id.to_owned()));
    }
    // An answer declaring every member of the record is the record.
    let whole = record
        .as_object()
        .is_some_and(|members| members.keys().all(|name| declares(name)));
    if whole
        && let Some(answer) =
            rt.engine
                .serve_stored(bundle, op, &Value::Object(shown.clone()), response_schema)
    {
        return Some(answer);
    }
    // An acknowledgement: the faker's answer, the same for the same call whether or not
    // the mock sets a seed, with the record's members laid over it.
    let status = match variant.status {
        mb_mcm::StatusSelector::Code(code) => code,
        _ => 200,
    };
    let ctx = mb_engine::GenerationContext {
        response_schema,
        ..mb_engine::GenerationContext::default()
    };
    let seeded;
    let call = match (mb_engine::seed::request_seed(call), rt.engine.faker_seed()) {
        (None, None) => {
            let mut with_seed = call.clone();
            with_seed.headers.insert(
                mb_engine::SEED_HEADER.to_owned(),
                Value::String(mb_engine::DEFAULT_FAKER_SEED.to_string()),
            );
            seeded = with_seed;
            &seeded
        }
        _ => call,
    };
    let faked = rt.engine.fallback(bundle, op, variant, status, call, &ctx);
    let serve = |answer: &Map<String, Value>| {
        rt.engine
            .serve_stored(bundle, op, &Value::Object(answer.clone()), response_schema)
    };
    let drawn = match faked.body? {
        // A successful write's acknowledgement says so: a boolean answer is `true`.
        Value::Bool(_) => {
            return rt
                .engine
                .serve_stored(bundle, op, &Value::Bool(true), response_schema);
        }
        Value::Object(drawn) => drawn,
        other => return rt.engine.serve_stored(bundle, op, &other, response_schema),
    };
    // The faker's booleans say the write succeeded, at the top and in its envelope.
    let mut affirmed = drawn.clone();
    affirm(&mut affirmed, 2);
    let mut held = affirmed.clone();
    // An answer holding nothing of the record at its top holds it in the one envelope
    // member that declares the record's identity (`{success, data: {id}}`).
    let at_top = !shown.is_empty();
    let identity =
        records::identity_member(rt, op, op.resource_type.as_deref().unwrap_or_default());
    let envelope: Vec<String> = held
        .iter()
        .filter(|(_, value)| value.is_object())
        .filter_map(|(name, _)| {
            let member = property_schema(bundle, schema, name)?;
            property_schema(bundle, &member, identity).map(|_| name.clone())
        })
        .collect();
    // The record's own id as it holds it (`7`, not `"7"`), else the id it is held by.
    let own_id = record
        .get(identity)
        .filter(|held| !held.is_null())
        .cloned()
        .unwrap_or_else(|| Value::String(id.to_owned()));
    // The one envelope member declaring the identity, for an answer holding nothing of
    // the record at its top, or not its identity.
    let id_envelope = match envelope.as_slice() {
        [name] if !at_top || !declares(identity) => {
            property_schema(bundle, schema, name).map(|m| (name, m))
        }
        _ => None,
    };
    if let Some((name, member)) = &id_envelope
        && let Some(Value::Object(inner)) = held.get_mut(*name)
    {
        if !at_top && let Some(members) = record.as_object() {
            for (key, value) in members {
                if property_schema(bundle, member, key).is_some() {
                    inner.insert(key.clone(), value.clone());
                }
            }
        }
        inner.insert(identity.to_owned(), own_id.clone());
    }
    held.extend(shown.clone());
    // Failing that: the faker's answer with the record's identity alone, at its top or
    // in its envelope; the faker's answer as drawn.
    let mut identified = affirmed.clone();
    if let Some(field) = op.identity.as_ref().map(|i| i.field.as_str())
        && let Some(value) = shown.get(field)
    {
        identified.insert(field.to_owned(), value.clone());
    }
    if let Some((name, _)) = &id_envelope
        && let Some(Value::Object(inner)) = identified.get_mut(*name)
    {
        inner.insert(identity.to_owned(), own_id);
    }
    if let Some(answer) = serve(&held) {
        return Some(answer);
    }
    // What names the record, where the answer holds the record's members: at its top,
    // or in the envelope holding it.
    let t = op.resource_type.as_deref().unwrap_or_default();
    let envelope_at = id_envelope
        .as_ref()
        .map(|(name, _)| format!("/{}", name.replace('~', "~0").replace('/', "~1")));
    let record_at = match &envelope_at {
        Some(at) if !at_top => at.as_str(),
        _ => "",
    };
    let places: Vec<&str> = std::iter::once("").chain(envelope_at.as_deref()).collect();
    let answer = Value::Object(held.clone());
    let identities = redraw::pointers_of(&redraw::naming_members(rt, op, t), &places, &answer);
    let seed = mb_engine::seed::request_seed(call)
        .or(rt.engine.faker_seed())
        .unwrap_or(mb_engine::DEFAULT_FAKER_SEED);
    let schemas = redraw::Schemas {
        schema,
        response: response_schema,
    };
    let record_place = redraw::RecordAt {
        resource_type: t,
        id,
        at: record_at,
    };
    let outcome = redraw::repaired(rt, op, (&schemas, &record_place), (held, &identities), seed);
    let (served, noted) = match outcome {
        redraw::Repair::Redrawn(answer, members) => match serve(&answer) {
            Some(served) => (Some(served), (redraw::MEMBERS_REDRAWN, members)),
            None => (None, (redraw::UNREPAIRABLE, Vec::new())),
        },
        other => (None, (other.outcome(), Vec::new())),
    };
    let fell_back = served.is_none();
    tracing::info!(
        mock = %rt.name,
        operation = %op.id,
        outcome = noted.0,
        members = ?noted.1,
        "the stored record broke the answer's schema"
    );
    *repair = Some(noted);
    if !fell_back {
        return served;
    }
    // Failing the repair: the faker's answer with the identity alone (none when the
    // identity is what breaks it), then as drawn, each saying so.
    let broken = match repair
        .as_ref()
        .is_some_and(|(o, _)| *o == redraw::IDENTITY_INVALID)
    {
        true => "the stored identity breaks the answer's schema",
        false => "the stored record breaks the answer's schema",
    };
    if let Some(mut served) = serve(&identified) {
        served.fallback_reason = Some(format!(
            "{broken}: the faker's answer with the record's identity"
        ));
        return Some(served);
    }
    let mut served = serve(&drawn)?;
    served.fallback_reason = Some(format!("{broken}: the faker's answer"));
    Some(served)
}

/// Sets the boolean members of `answer`, and of its object members down to `depth`
/// levels in all, to `true`.
fn affirm(answer: &mut Map<String, Value>, depth: usize) {
    if depth == 0 {
        return;
    }
    for value in answer.values_mut() {
        match value {
            Value::Bool(_) => *value = Value::Bool(true),
            Value::Object(inner) => affirm(inner, depth - 1),
            _ => {}
        }
    }
}
