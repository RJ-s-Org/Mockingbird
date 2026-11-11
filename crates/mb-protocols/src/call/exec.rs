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
//! `read_only_member`), a read's `render_failed` with `render_error` saying why; executor
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
        }
    }
}

mod action;
mod create;
pub(super) use create::issued_aliases;
mod embedded;
mod fields;
mod list;
mod patches;
mod queue;
mod writes;

pub(super) use create::{initial_token, unreadable};
pub(super) use queue::forget as forget_queue;

/// What [`run`] did with a call.
pub(super) enum Ran {
    /// The call is answered.
    Answered(Box<GeneratedResponse>),
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
        .is_some_and(|t| match kind {
            OpKind::List => op.identity.is_some() || list::identified(rt, op, kind),
            _ => identified(rt, op, t),
        })
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

/// Whether records of type `t` have an identity: `op`'s own (where its answer holds
/// the record), else the description's, else that of the type's read by id.
fn identified(rt: &crate::MockRuntime, op: &mb_mcm::Operation, t: &str) -> bool {
    op.identity.is_some()
        || rt
            .described()
            .type_desc(t)
            .is_some_and(|d| d.identity.is_some())
        || rt
            .index()
            .detail_reads
            .get(t)
            .and_then(|i| rt.bundle().operations.get(*i))
            .is_some_and(|read| read.identity.is_some())
}

/// The schemas records of type `t` are read by: its read by id's record and each of
/// its lists' items ([`create::read_schemas`]).
pub(super) fn read_schemas(rt: &crate::MockRuntime, t: &str) -> Vec<Value> {
    create::read_schemas(rt, t)
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
    // One held in its parent's array member is that item.
    let within = embedded::current(state, plan, call, &held).await;
    // A missing record the session knows of (deleted, or unknown under
    // `unknown_ids: 404`) was refused before this; any other generates.
    let Some(stored) = held.target.or(within.as_ref().map(|e| &e.child)) else {
        fell_back(rt, plan, kind, Fallback::NotHeld.as_str());
        return Ran::Generate(None);
    };
    let read = held_read(
        state,
        plan,
        call,
        stored,
        held.target_id,
        response_schema,
        true,
    );
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
            plan.exprs.fell_back(Fallback::RenderFailed.as_str(), None);
            tracing::info!(
                mock = %rt.name,
                operation = %plan.op.id,
                exec = kind_name(kind),
                exec_fallback = Fallback::RenderFailed.as_str(),
                render_error = %why,
                "not answered by the executor"
            );
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
    match kept.as_ref().and_then(|kept| held(Some(kept))).or(bare) {
        Some(mut served) => {
            write_memory::note_not_caught_up(&mut served, plan, Some(stored), &earlier);
            // A described fields parameter narrows the record the answer holds.
            fields::narrow_read(rt, op, call, served.body.as_mut());
            HeldRead::Served(Box::new(served))
        }
        None => HeldRead::Unrendered(Some(earlier), why(kept.as_ref())),
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

/// Logs that the executor answered the call.
fn answered(rt: &crate::MockRuntime, plan: &CallPlan<'_>, kind: OpKind) {
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
pub(super) fn render(
    rt: &crate::MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    (id, record): (&str, &Value),
    response_schema: Option<&Value>,
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
    [&held, &identified, &drawn].into_iter().find_map(serve)
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
