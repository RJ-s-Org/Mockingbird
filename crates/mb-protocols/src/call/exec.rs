//! The state-first executor (`docs/superpowers/specs/2026-11-06-state-first-engine-design.md`):
//! for a resource operation the effective world description classifies, the store
//! decides the answer and the stored record is rendered into the response schema.
//! Everything it cannot classify, and every call it falls back on, takes the generated
//! path unchanged.
//!
//! [`run`] is the seam [`super::prepare`] calls once the call's target, refusals and
//! tokens are settled. A classified call is logged at `info` with the field `exec` (its
//! kind) when the executor answers it, and with `exec_fallback` (a [`Fallback`]) when it
//! goes on to generation, a read's `render_failed` with `render_error` saying why; executor
//! answers are tagged `state`, as stored answers are.
//!
//! An executor answer is final: a write is stored through [`commit`], which keeps only
//! the side effects of a write (store, publish, webhooks, idempotency replay, cache),
//! never through [`super::complete`]'s reconciliation of a generated answer.

use mb_engine::GeneratedResponse;
use mb_engine::consistency::property_schema;
use mb_mcm::world::OpKind;
use mb_mcm::{Effect, OperationKind};
use mb_state::StoredResource;
use serde_json::{Map, Value};
use tracing::Instrument;

use super::write_memory::Earlier;
use super::writes::store_one;
use super::{
    CallPlan, OperationCall, StateAccess, collection_of, deleted, derived, envelopes,
    graphql_marks, minted, plain, records, replays, serve_held, state_lists, state_span, target_id,
    tokens, world, write_memory,
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
    } else if !desc
        .and_then(|d| d.r#type.as_deref())
        .or(op.resource_type.as_deref())
        .is_some_and(|t| identified(rt, op, t))
    {
        Some(Fallback::NoIdentity)
    } else if !matches!(kind, OpKind::Create | OpKind::List) && target_id(op, call).is_none() {
        Some(Fallback::NoTarget)
    } else {
        None
    };
    Some(match fallback {
        Some(reason) => Err((kind, reason)),
        None => Ok(kind),
    })
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
    match classify(state.rt, plan, call) {
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
    // A missing record the session knows of (deleted, or unknown under
    // `unknown_ids: 404`) was refused before this; any other generates.
    let Some(stored) = held.target else {
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
    let derived = derived::filled(state, &plan.session, stored)
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
    tracing::info!(
        mock = %rt.name,
        operation = %plan.op.id,
        exec = kind_name(kind),
        exec_fallback = reason,
        "not answered by the executor"
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
}

/// Applies a write the executor decided, with the side effects every write has, and
/// returns `answer` as it is: the session listed, the write noted as the session's
/// latest and told to its list routes' memory, the record stored in its collection (a create noted as a
/// member of its listed collection), a removal marked deleted under its route's parent
/// and its tokens ended, the requests write memory kept for it forgotten (the executor's
/// record is authoritative), the entity tag's state, the answer kept for a repeat of its
/// idempotency key, the session's cached answers cleared, the webhooks sent, and the
/// session kept alive. The store publishes the change.
pub(super) async fn commit(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    (t, held): (&str, Option<&StoredResource>),
    write: Write,
    answer: GeneratedResponse,
) -> GeneratedResponse {
    async {
        let rt = state.rt;
        let op = plan.op;
        let session = &plan.session;
        let collection = collection_of(op, call);
        let id = match write {
            Write::Put { id, data } => {
                if op.kind == OperationKind::Command(Effect::Create) {
                    state_lists::mark_created(state, session, (op, t), collection.as_ref(), &id)
                        .await;
                }
                let place = (collection, Some(&call.query), held.is_some());
                store_one(state, session, (op, t), (id.clone(), data), place).await;
                id
            }
            Write::Remove { id } => {
                state.delete(session, t, &id).await;
                if let Some(gone) = held {
                    rt.collections().forget(session, gone);
                }
                if state.tracks_deleted || graphql_marks(rt, plan, t, &id, &answer) {
                    deleted::mark(state, session, (op, call), t, &id, held).await;
                }
                tokens::forget(state, session, t, &id).await;
                id
            }
        };
        // What earlier answers said of a deleted record goes, a soft-deleted one too.
        world::after_delete(state, plan, Some(&id), &answer).await;
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
        crate::webhooks::emit::emit(rt, &plan.engine, op, call, &answer, Some(&id), session);
        answer
    }
    .instrument(state_span())
    .await
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
    })
}

/// `record` (of the call's target type) as the call's operation answers a write of it
/// (spec §4): no body when its success response declares none; the record in the
/// envelope the operation holds it in; the record itself when the schema declares every
/// member it holds; else an acknowledgement (`{ok, id, rev}`, `{deleted: true}`,
/// `{success, data: {id}}`). An acknowledgement is the faker's answer for the call,
/// seeded by the mock's or the request's seed, else by the default one, so a call
/// answers alike in every run. Its boolean members (at the top and one member down) are
/// `true`, as the write succeeded; the record's members it declares at its top, and the
/// operation's identity member, take the record's values; and an answer holding none of
/// them at its top holds the record's members and id in the one envelope member whose
/// schema declares the record's identity. Failing validation, it tries every boolean
/// `true`, then the faker's answer with the identity alone, then as drawn. `None` only
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
        return serve_held(rt, op, &held, None, response_schema);
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
    if let Some(field) = op.identity.as_ref().map(|i| i.field.as_str())
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
    if !at_top
        && let [name] = envelope.as_slice()
        && let Some(member) = property_schema(bundle, schema, name)
        && let Some(Value::Object(inner)) = held.get_mut(name)
    {
        if let Some(members) = record.as_object() {
            for (key, value) in members {
                if property_schema(bundle, &member, key).is_some() {
                    inner.insert(key.clone(), value.clone());
                }
            }
        }
        // The record's own id as it holds it (`7`, not `"7"`), else the id it is held by.
        if !record.get(identity).is_some_and(|held| !held.is_null()) {
            inner.insert(identity.to_owned(), Value::String(id.to_owned()));
        }
    }
    held.extend(shown.clone());
    // Failing that: every boolean `true`; as drawn and held; the faker's answer with the
    // record's identity alone; the faker's answer as drawn.
    let mut all_true = held.clone();
    affirm(&mut all_true, 1);
    let mut identified = affirmed.clone();
    if let Some(field) = op.identity.as_ref().map(|i| i.field.as_str())
        && let Some(value) = shown.get(field)
    {
        identified.insert(field.to_owned(), value.clone());
    }
    [&held, &all_true, &identified, &drawn]
        .into_iter()
        .find_map(serve)
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
