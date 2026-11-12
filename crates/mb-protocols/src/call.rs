//! The per-call pipeline every adapter shares: admission, data-plane auth,
//! override headers and scenario lookup ([`plan`]), state reads and writes around generation
//! ([`generate_once`], [`open_stream`]), state change feeds
//! ([`change_feed`]) and usage/request-log reporting ([`report`]).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::http::HeaderMap;
use mb_engine::consistency::{
    ENVELOPE_KEYS, item_identity, item_schema, list_items, list_members, property_schema,
};
use mb_engine::error_body::{ErrorCause, RefusalFacts};
use mb_engine::{
    EffectOutcome, EngineError, FAKER_INVALID_REASON, GeneratedResponse, GenerationContext,
    OFFLINE_REASON, OperationCall, ResourceEffect, Source,
};
use mb_ext::{CallInfo, Rejection, Tenancy, UsageEvent};
use mb_llm::Usage;
use mb_mcm::{Binding, Effect, Operation, OperationKind, Protocol};
use mb_state::{CacheEpoch, CachedResponse, SessionKey, StateError, StoredResource};
use serde_json::{Map, Value};
use tracing::Instrument;

use crate::chaos::{self, CHAOS_HEADER, ChaosDecision};
pub(crate) use crate::dp_auth::Unauthorized;
use crate::dp_auth::authenticate;
use crate::world_gaps::GapCode;
use crate::{
    MockRuntime, RequestKind, RequestRecord, STREAM_ITEMS_HEADER, ScenarioRuntime, UnknownIds,
};

mod admission;
mod bundle_index;
mod canonical;
mod collections;
mod computed;
mod creates;
mod cursors;
mod deleted;
mod described;
mod effects;
mod entities;
mod envelopes;
mod exec;
mod exprs;
mod header_ids;
mod lifecycle;
mod list_exprs;
mod list_query;
mod lists;
mod lookup;
mod minted;
#[cfg(test)]
mod owner_tie_tests;
mod owners;
mod pages;
mod ranges;
mod records;
mod relations;
mod repeats;
mod replays;
mod reporting;
mod reused_ids;
mod runs;
mod session_memory;
pub(crate) mod shared;
mod side_writes;
mod state_access;
mod state_lists;
#[cfg(any(feature = "sse", feature = "graphql", feature = "grpc"))]
mod streams;
mod tokens;
mod transitions;
mod unknown_ids;
mod value_index;
mod variants;
mod world;
mod write_memory;
mod writes;

pub(crate) use admission::*;
pub(crate) use bundle_index::*;
pub(crate) use canonical::*;
pub(crate) use collections::*;
use creates::*;
pub(crate) use described::Described;
pub(crate) use effects::*;
use entities::*;
pub(crate) use exec::pool as exec_pool;
pub(crate) use exprs::CallExprs;
#[cfg(feature = "graphql")]
pub(crate) use exprs::seed_notes;
pub(crate) use list_query::undescribed_params;
use lookup::*;
use owners::*;
#[cfg(feature = "graphql")]
pub(crate) use relations::referenced;
use relations::*;
pub(crate) use reporting::*;
pub(crate) use session_memory::*;
pub(crate) use state_access::*;
#[cfg(any(feature = "sse", feature = "graphql", feature = "grpc"))]
pub(crate) use streams::*;
pub(crate) use value_index::{ValueFeed, ValueIndex};
pub(crate) use variants::Variants;
pub(crate) use writes::*;

const SESSION_HEADER: &str = "x-mockingbird-session";
const STATUS_HEADER: &str = "x-mockingbird-status";
const LATENCY_HEADER: &str = "x-mockingbird-latency";
const SCENARIO_HEADER: &str = "x-mockingbird-scenario";
pub(crate) const DEFAULT_SESSION: &str = "default";
const MAX_LATENCY: Duration = Duration::from_secs(30);
const MAX_SESSION_LEN: usize = 128;
const MAX_STREAM_ITEMS: usize = 10_000;
const LIST_CONTEXT_LIMIT: usize = 20;

/// List members of a read's answer whose stored items join its context.
const MAX_LIST_MEMBERS: usize = 3;
/// Usage-event source (the `source` label of `mb_requests_total`) for calls
/// answered without generating a body: refused (auth, quota, rate limit,
/// invalid request or override, unknown scenario) or answered natively (a
/// forced or injected error the contract declares no response for).
const NOT_GENERATED_SOURCE: &str = "not_generated";

/// The id of the resource this operation reads, updates or deletes: the
/// identity's path parameter, else its input field in the request body,
/// else, for HTTP updates and deletes whose identity names no path
/// parameter, the last path parameter. A create whose identity names both
/// takes the input field first, unless its route names the record by that
/// path parameter (a `PUT` to the record's own URI, see
/// [`Operation::route_names_record`]): a `POST`'s path parameter names where
/// the record goes (its collection or parent), not the record it makes.
pub(crate) fn target_id(op: &Operation, call: &OperationCall) -> Option<String> {
    let identity = op.identity.as_ref();
    let path_param = identity.and_then(|i| i.path_param.as_deref());
    let from_body = || {
        identity
            .and_then(|i| i.input_field.as_deref())
            .and_then(|f| mb_mcm::member_at(call.body.as_ref()?, f))
            .and_then(id_string)
    };
    if op.kind == OperationKind::Command(Effect::Create)
        && !path_param.is_some_and(|p| op.route_names_record(p))
        && let Some(id) = from_body()
    {
        return Some(id);
    }
    if let Some(id) = path_param
        .and_then(|p| call.path_params.get(p))
        .and_then(id_string)
    {
        return Some(id);
    }
    if let Some(id) = from_body() {
        return Some(id);
    }
    if path_param.is_some()
        || !matches!(
            op.kind,
            OperationKind::Command(Effect::Update | Effect::Delete)
        )
    {
        return None;
    }
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    call.path_params
        .get(&last_path_param(path)?)
        .and_then(id_string)
}

/// [`target_id`], else, where the request body names the record by its input field and
/// leaves that out, the first identity alternate the description gives the operation's
/// type ([`records::alternates`]) that the body holds, beside where the input field would
/// be (`Item.SKU` for `Item.ItemID`), as a path value may name it by one. A create names
/// no record by an alternate: its body's values are the new record's.
pub(crate) fn named_target(
    rt: &MockRuntime,
    op: &Operation,
    call: &OperationCall,
) -> Option<String> {
    if let Some(id) = target_id(op, call) {
        return Some(id);
    }
    if op.kind == OperationKind::Command(Effect::Create) {
        return None;
    }
    let field = op.identity.as_ref()?.input_field.as_deref()?;
    let body = call.body.as_ref().filter(|b| b.is_object())?;
    let parent = field.rsplit_once('.').map(|(parent, _)| parent);
    records::alternates(rt, op.resource_type.as_deref()?)
        .iter()
        .find_map(|alternate| {
            let at = match parent {
                Some(parent) => format!("{parent}.{alternate}"),
                None => alternate.clone(),
            };
            mb_mcm::member_at(body, &at).and_then(id_string)
        })
}

/// The last parameter of a route template, when it alone names something:
/// the one parameter of its segment (`{id}`, `{Sid}.json`, `customers({id})`,
/// `key={key}`). A part of a composite key (`lines(doc={d},seq={s})`) or a
/// call's argument (`NS.getBalance(asOf={date})`) names no record by itself,
/// so it gives none.
fn last_path_param(template: &str) -> Option<String> {
    let start = template.rfind('{')?;
    let end = start + template[start..].find('}')?;
    let seg_start = template[..start].rfind('/').map_or(0, |i| i + 1);
    let seg_end = template[end..]
        .find('/')
        .map_or(template.len(), |i| end + i);
    let before = &template[seg_start..start];
    let argument = before.contains('(') && before.ends_with('=');
    let alone = template[seg_start..seg_end].matches('{').count() == 1;
    (alone && !argument).then(|| template[start + 1..end].to_string())
}

fn id_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `customerId` / `customer_id` → `customer`.
fn reference_prefix(field: &str) -> Option<&str> {
    field
        .strip_suffix("Id")
        .or_else(|| field.strip_suffix("_id"))
        .filter(|p| !p.is_empty())
}

/// Adds the static gaps of `op` (what the world description leaves open about it, see
/// [`Described::gaps`]) to `generated.world_gaps`, for the request log. Extends and
/// deduplicates, so a mark pushed earlier stays and seeding twice adds nothing.
pub(crate) fn seed_world_gaps(rt: &MockRuntime, op: &Operation, generated: &mut GeneratedResponse) {
    for code in rt.described().gaps(&op.id) {
        let name = code.as_str();
        if !generated.world_gaps.iter().any(|g| g == name) {
            generated.world_gaps.push(name.to_owned());
        }
    }
}

/// Reads the target and related resources, generates one response, writes
/// state for a 2xx and keeps the session alive.
#[cfg(any(feature = "http-rest", feature = "grpc", feature = "soap"))]
pub(crate) async fn generate_once(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Result<GeneratedResponse, EngineError> {
    generate_narrowed(rt, plan, call, None).await
}

/// The declared `ETag` response header of `generated`, with its record's concurrency
/// token, when one is paired with a request header (see [`tokens`]).
#[cfg(feature = "http-rest")]
pub(crate) async fn entity_tag(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    generated: &GeneratedResponse,
) -> Option<(String, String)> {
    tokens::entity_tag(rt, plan, call, generated).await
}

/// The `Next-Range` header of a `206` page of a list paged by its `Range` header (see
/// [`ranges`]).
#[cfg(feature = "http-rest")]
pub(crate) fn next_range(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    generated: &GeneratedResponse,
) -> Option<(String, String)> {
    ranges::next_range(rt, plan, call, generated)
}

/// The stored target and related resources a read-only generation of `call`
/// takes into account (as [`generate_once`] reads them, but for the hops,
/// which its callers do not read); the session is marked used.
#[cfg(feature = "graphql")]
pub(crate) async fn read_state(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> (Option<StoredResource>, Vec<StoredResource>) {
    let mut state = StateAccess::new(rt);
    let target_id = target_id(plan.op, call);
    let (target, related, _) = read_context(
        &mut state,
        &plan.session,
        plan.op,
        target_id.as_deref(),
        call,
    )
    .instrument(state_span())
    .await;
    state.touch(&plan.session).instrument(state_span()).await;
    (target, related)
}

/// [`generate_once`] with a 2xx response schema narrowed to what the caller
/// reads (see [`GenerationContext::response_schema`]).
pub(crate) async fn generate_narrowed(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    response_schema: Option<&Value>,
) -> Result<GeneratedResponse, EngineError> {
    let mut answer = match prepare(rt, plan, call, response_schema).await {
        Prepared::Answered(mut answer) => {
            seed_world_gaps(rt, plan.op, &mut answer);
            *answer
        }
        Prepared::Pending(pending) => {
            generate_pending(rt, plan, call, *pending, response_schema).await?
        }
    };
    exprs::seed_notes(plan, &mut answer);
    Ok(answer)
}

/// Generates the answer of a [`prepare`]d call, then [`complete`]s it.
pub(crate) async fn generate_pending(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    mut pending: Pending<'_>,
    response_schema: Option<&Value>,
) -> Result<GeneratedResponse, EngineError> {
    // A read in another language restates its held record (see [`variants`]): the record
    // is its target, and only its text is the answer's own. Not while writes the record
    // does not show yet are remembered: it catches up with them as any read does, and
    // what it answers is no variant to keep.
    if pending.variant && !pending.earlier.told.is_empty() {
        pending.variant = false;
    }
    let target = pending.target.as_ref();
    let representation = pending
        .variant
        .then(|| variants::representation(plan.op, call))
        .flatten();
    let guard = target
        .filter(|_| response_schema.is_none() && !pending.variant)
        .map(|held| write_memory::SettleGuard {
            rt,
            plan,
            call,
            held,
            advancing: &pending.tokens.advancing,
        });
    // A client's retry of a write answering no body is shown nothing beyond its target,
    // so it is asked for no effects a second time (see [`write_memory::is_retry`]).
    pending.retry = write_memory::is_retry(&mut pending.state, plan, call)
        .instrument(state_span())
        .await;
    let repeat = pending.retry && write_memory::answers_no_body(plan);
    // A read in another language restates its held record in that language: it is shown
    // that record and the writes it must still reflect, not the session's other records
    // and answers, which only lengthen its prompt.
    let restated = pending.target.as_ref().filter(|_| pending.variant);
    let (related, created_since): (&[StoredResource], &[StoredResource]) = if repeat {
        (&[], &[])
    } else if restated.is_some() {
        (&[], &pending.created_since)
    } else {
        (&pending.related, &pending.created_since)
    };
    let session_answers = |values| if restated.is_some() { &[][..] } else { values };
    let ctx = GenerationContext {
        related,
        target,
        forced_status: plan.forced_status,
        scenario: plan.scenario.map(|s| &s.spec),
        response_schema,
        earlier_requests: &pending.earlier.told,
        earlier_own_body: pending.earlier.own.as_ref(),
        earlier_answers: session_answers(&pending.earlier_answers[..]),
        earlier_writes: session_answers(&pending.earlier_writes[..]),
        earlier_origins: session_answers(&pending.earlier_origins[..]),
        served_cursors: &pending.served_cursors,
        created_since,
        alongside: &pending.alongside,
        same_values: &pending.same_values,
        changes_by_effects: write_memory::by_effects(
            &pending.earlier,
            target.filter(|_| !pending.variant),
        ),
        update_guard: guard.as_ref().map(|g| g as &dyn mb_engine::UpdateGuard),
        answer_records: rt
            .described()
            .op(&plan.op.id)
            .and_then(|d| d.answer.as_ref())
            .and_then(|a| a.records.as_ref()),
        restates: representation.as_deref(),
    };
    let mut generated = plan.engine.generate(&rt.bundle, plan.op, call, ctx).await?;
    // The request log counts the executor's fill as well as the generation.
    generated.usage = generated.usage + pending.fill_usage;
    if generated.source == Source::Fallback {
        write_memory::failed_attempt(&mut pending.state, plan, &mut pending.earlier).await;
    }
    // The LLM failed a read that only generated to catch up: what is stored
    // still answers, as the faker must not invent a change. Nothing was stored
    // or marked, so the next read tries again, up to the cap.
    if generated.source == Source::Fallback
        && let Some(stored) = pending.earlier.stored_answer.take()
    {
        tracing::warn!(
            mock = %rt.name,
            operation = %plan.op.id,
            reason = generated.fallback_reason.as_deref().unwrap_or_default(),
            "read could not catch up with earlier requests; answered from state"
        );
        pending
            .state
            .touch(&plan.session)
            .instrument(state_span())
            .await;
        let mut answer = GeneratedResponse {
            usage: generated.usage,
            ..*stored
        };
        write_memory::note_not_caught_up(
            &mut answer,
            plan,
            pending.target.as_ref(),
            &pending.earlier,
        );
        return Ok(plan
            .engine
            .encode_base64(&rt.bundle, plan.op, response_schema, answer));
    }
    Ok(complete(rt, plan, call, pending, generated, response_schema).await)
}

/// What [`prepare`] found: an answer that needs no generation, or the
/// state a generation must take into account.
pub(crate) enum Prepared<'a> {
    Answered(Box<GeneratedResponse>),
    Pending(Box<Pending<'a>>),
}

/// A call to generate, then hand to [`complete`].
pub(crate) struct Pending<'a> {
    state: StateAccess<'a>,
    target_id: Option<String>,
    pub(crate) target: Option<StoredResource>,
    pub(crate) related: Vec<StoredResource>,
    /// What the writes the target does not show yet created (see
    /// [`write_memory::created_since`]).
    created_since: Vec<StoredResource>,
    /// The target's key holds another owner's resource, not the target.
    foreign_key: bool,
    /// The cache epoch before any state was read, and the answer's key,
    /// when the answer is cached.
    cache: Option<(CacheEpoch, String)>,
    /// The writes the target does not show yet.
    earlier: write_memory::Earlier,
    /// What routes with no parameters answered earlier in the session.
    earlier_answers: Vec<Value>,
    /// Whether only a read has written to the session (see [`world::list_session`]).
    unlisted: bool,
    /// The answers that minted values this call carries, with their requests.
    earlier_origins: Vec<Value>,
    /// The hidden cursor parameters this call carries with a value the session served
    /// (see [`cursors`]).
    served_cursors: Vec<String>,
    /// What was written to this list read's own path earlier in the session.
    earlier_writes: Vec<Value>,
    /// Those of `related` a create's own collection holds (see [`created_alongside`]).
    alongside: Vec<(String, String)>,
    /// Those of `related` of a create's type that hold a value of its request, with the
    /// member (see [`shown_same_values`]).
    same_values: Vec<(String, String, String)>,
    /// What a write found of its target's concurrency tokens (see [`tokens`]).
    tokens: tokens::Tokens,
    /// The write is a client's retry of the session's latest (see
    /// [`write_memory::is_retry`]).
    retry: bool,
    /// What a fill the executor ran before falling back to generation spent.
    fill_usage: Usage,
    /// A read in another language than its held target (see [`variants`]): its record
    /// is kept as that language's variant, and the target stays.
    variant: bool,
    /// This call claimed the retry of a fallback cached under its key (see
    /// [`mb_state::ResponseCache::claim_retry`]).
    retrying_fallback: bool,
}

/// Whether a GraphQL delete that removed record `id` of type `t` here leaves the marker
/// its `write_by_id` would have left, had the record still been held when it ran: by id,
/// and unless its payload contradicts it (see [`deleted::marks_by_id`]).
#[cfg_attr(not(feature = "graphql"), allow(unused_variables))]
fn graphql_marks(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    t: &str,
    id: &str,
    generated: &GeneratedResponse,
) -> bool {
    #[cfg(feature = "graphql")]
    {
        matches!(plan.op.binding, Binding::Graphql { .. })
            && deleted::marks_by_id(rt, plan)
            && !crate::graphql::contradicts(generated.body.as_ref(), Some(t), id)
    }
    #[cfg(not(feature = "graphql"))]
    {
        false
    }
}

/// Reads the call's state and answers what needs no generation: an unknown
/// id (404), a stored resource, a cached answer. An answer's base64 fields are
/// valid base64 whatever made it (see [`mb_engine::Engine::encode_base64`]), as
/// are [`complete`]'s.
pub(crate) async fn prepare<'a>(
    rt: &'a MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    response_schema: Option<&Value>,
) -> Prepared<'a> {
    match prepare_state(rt, plan, call, response_schema).await {
        Prepared::Answered(answer) => Prepared::Answered(Box::new(plan.engine.encode_base64(
            &rt.bundle,
            plan.op,
            response_schema,
            *answer,
        ))),
        pending @ Prepared::Pending(_) => pending,
    }
}

/// [`prepare`] but for the base64 fields of its answer.
async fn prepare_state<'a>(
    rt: &'a MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    response_schema: Option<&Value>,
) -> Prepared<'a> {
    let op = plan.op;
    let session = &plan.session;
    let mut state = StateAccess::new(rt);
    // A write's records of its type are the app's own (see [`unknown_ids`]).
    if matches!(op.kind, OperationKind::Command(_)) {
        state.owns = op.resource_type.clone();
    }
    state.tracks_deleted = deleted::applies(rt, plan);
    if state.tracks_deleted {
        state.call_params = Some(Box::new(OperationCall {
            path_params: call.path_params.clone(),
            ..OperationCall::default()
        }));
    }
    let resource_type = op.resource_type.as_deref();
    // A repeat of a described idempotency key: the first call's answer, as it was.
    if let Some(replay) = replays::replayed(&mut state, plan, call)
        .instrument(state_span())
        .await
    {
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(Box::new(replay));
    }
    let mut target_id = named_target(rt, op, call);
    // Before any state is read: an answer generated from older state must
    // not be cached after a write that lands meanwhile.
    let since = rt.state.response_cache().map(|c| c.epoch(session));
    let (target, mut related, mut followed) =
        read_context(&mut state, session, op, target_id.as_deref(), call)
            .instrument(state_span())
            .await;
    let target = match target {
        Some(hit) => Some(hit),
        None => {
            write_memory::silent_write_target(&mut state, plan, call, target_id.as_deref())
                .instrument(state_span())
                .await
        }
    };
    // What the path named, before a record found by another member takes its place.
    let named_id = target_id.clone();
    // Found by another field (an issue's `number`): writes go to its id.
    if let Some(found) = &target {
        target_id = Some(found.id.clone());
    }
    // What the target's key holds is another owner's: a write must leave it.
    let foreign_key = !matches!(op.kind, OperationKind::Query)
        && target.is_none()
        && guarded(rt, op, call).is_some()
        && match (resource_type, target_id.as_deref()) {
            (Some(t), Some(id)) => records::held_by_key(&mut state, session, t, id)
                .await
                .is_some(),
            _ => false,
        };

    if rt.unknown_ids != UnknownIds::Generate
        && matches!(op.kind, OperationKind::Query)
        && plan.forced_status.is_none()
        && targets_by_path(rt, op)
        && target.is_none()
        && state.available()
        && let (Some(t), Some(id)) = (resource_type, target_id.as_deref())
        && unknown_ids::not_found(&mut state, session, t)
            .instrument(state_span())
            .await
        && !(guarded(rt, op, call).is_some()
            && any_owner_holds(
                &mut state,
                session,
                t,
                id,
                target_param(op).as_deref().unwrap_or_default(),
            )
            .await)
    {
        let detail = format!("{t} {id} does not exist in session {}", session.session);
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(Box::new(
            nothing_held(rt, plan, call, response_schema, &detail).await,
        ));
    }
    // The same for a list scoped by a value the server issued its items' parent: no
    // parent holds it, so no collection answers to it.
    if plan.forced_status.is_none()
        && state.available()
        && let Some((parent, param, value)) = state_lists::issued_scope(rt, op, call)
        && records::by_extra_fields(&mut state, session, &parent, (&[], &value), None)
            .instrument(state_span())
            .await
            .is_none()
    {
        let detail = format!(
            "no {parent} has {param} {value} in session {}",
            session.session
        );
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(Box::new(
            nothing_held(rt, plan, call, response_schema, &detail).await,
        ));
    }
    // A value the server issues for a record ([`exec::issued_aliases`], Plaid's
    // `access_token`) that names no held record was never issued: whatever
    // `unknown_ids` says, no record answers to it.
    if plan.forced_status.is_none()
        && target.is_none()
        && state.available()
        && let Some(t) = resource_type
        && let Some(member) = rt
            .described()
            .op(&op.id)
            .and_then(|d| d.target.as_ref())
            .and_then(|target| target.member.as_deref())
        && let Some(id) = target_id.as_deref()
        && exec::issued_aliases(rt, t).iter().any(|a| a == member)
    {
        let detail = format!("no {t} has {member} {id} in session {}", session.session);
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(Box::new(
            nothing_held(rt, plan, call, response_schema, &detail).await,
        ));
    }
    // Whatever `unknown_ids` says: this one is known to be gone. A record found by
    // another member than its key (`/issues/{issue_iid}`) is not what the path named
    // when that was deleted: another issue can share the `iid`.
    if matches!(op.kind, OperationKind::Query)
        && plan.forced_status.is_none()
        && target
            .as_ref()
            .is_none_or(|found| Some(found.id.as_str()) != named_id.as_deref())
        && (state.tracks_deleted || deleted::marks_by_id(rt, plan))
        && let (Some(t), Some(id)) = (resource_type, named_id.as_deref())
        && deleted::is_deleted(
            &mut state,
            session,
            t,
            deleted::scope(op, call).as_deref(),
            id,
        )
        .await
    {
        let detail = format!("{t} {id} was deleted in session {}", session.session);
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(Box::new(
            nothing_held(rt, plan, call, response_schema, &detail).await,
        ));
    }

    // A read whose `If-None-Match` holds the stored record's entity tag: `304`, at once,
    // unless writes the stored copy does not show yet make the read catch up.
    if !foreign_key
        && let Some(held) = &target
        && let Some(answer) = tokens::not_modified(&mut state, plan, call, held)
            .instrument(state_span())
            .await
        && !write_memory::recall(&mut state, plan, call, target_id.as_deref(), true)
            .instrument(state_span())
            .await
            .forces(rt, plan, call)
    {
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(Box::new(answer));
    }

    // A write sending a token its record held before is refused before generating.
    let mut tokens = tokens::Tokens::default();
    if !foreign_key
        && tokens::applies(rt, plan)
        && let Some(held) = &target
    {
        match tokens::check(&mut state, plan, call, held)
            .instrument(state_span())
            .await
        {
            Ok(found) => tokens = found,
            Err(refusal) => {
                state.touch(session).instrument(state_span()).await;
                return Prepared::Answered(Box::new(refusal));
            }
        }
    }

    // A create or update repeating a described unique set is refused before generating.
    if !foreign_key
        && let Some(conflict) = repeats::described_conflict(
            &mut state,
            plan,
            call,
            (target_id.as_deref(), target.as_ref()),
        )
        .instrument(state_span())
        .await
        && let Some(refusal) = plan
            .engine
            .refuse_held(
                rt.bundle(),
                op,
                call,
                conflict.code,
                ErrorCause::Taken,
                &RefusalFacts::taken(&conflict.taken),
            )
            .await
    {
        replays::keep(&mut state, plan, call, &refusal)
            .instrument(state_span())
            .await;
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(Box::new(refusal));
    }

    let target_missing = target.is_none();
    // A read naming a seed asks for that seed's answer, not for what a plain
    // read answered; a resource stored by a write or a fixture still stands.
    let target = target.filter(|stored| {
        !(matches!(op.kind, OperationKind::Query)
            && mb_engine::seed::request_seed(call).is_some()
            && rt.own_reads().holds(session, stored))
    });

    // The state-first executor ([`exec`]): a call the description classifies is
    // answered from the store; any other, and one it falls back on, goes on below. Whether
    // a stored answer stands depends on what is remembered; otherwise what is remembered
    // waits until the cache has had its say.
    // A read in another language than the held record was stored in: that language's
    // variant, or a generation that knows the record (see [`variants`]).
    let variant = target
        .as_ref()
        .and_then(|held| rt.variants().of(session, op, call, held));
    let held = exec::Held {
        target: match &variant {
            Some(variants::Variant::Kept(kept)) => Some(kept),
            _ => target.as_ref(),
        },
        target_id: target_id.as_deref().filter(|_| !foreign_key),
        generates: matches!(variant, Some(variants::Variant::New)),
    };
    let (recalled, fill_usage) =
        match exec::run(&mut state, plan, call, held, response_schema).await {
            exec::Ran::Answered(served) => {
                state.touch(session).instrument(state_span()).await;
                return Prepared::Answered(served);
            }
            exec::Ran::Generate(recalled) => (recalled, Usage::default()),
            exec::Ran::Filled(usage) => (None, usage),
        };
    if target.is_none()
        && let Some(served) = member_read(&mut state, plan, call, response_schema)
            .instrument(state_span())
            .await
    {
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(Box::new(served));
    }

    // The next page of a list this session cut, by the token it minted.
    if target.is_none()
        && let Some(served) = pages::serve(&mut state, plan, call)
            .instrument(state_span())
            .await
    {
        cursors::after_answer(&mut state, plan, &served)
            .instrument(state_span())
            .await;
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(Box::new(served));
    }

    // A described list of a collection the session listed before: from its records.
    if target.is_none()
        && let Some(served) = state_lists::answer(&mut state, plan, call)
            .instrument(state_span())
            .await
    {
        cursors::after_answer(&mut state, plan, &served)
            .instrument(state_span())
            .await;
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(Box::new(served));
    }

    let is_query = matches!(op.kind, OperationKind::Query);
    let cache = rt
        .state
        .response_cache()
        .filter(|_| is_query && rt.consistency && plain(plan));
    let key = cache.map(|_| {
        format!(
            "{}:{}",
            rt.bundle.source_digest,
            cache_key(&rt.bundle, op, call, response_schema)
        )
    });
    let mut retrying_fallback = false;
    if let (Some(cache), Some(key)) = (cache, &key)
        && let Some(hit) = cache.get(session, key)
        // A fallback for an answer the model did not give in time does not
        // stick: the next repeat generates again, while repeats arriving
        // during that retry are answered with it. Other fallbacks (a provider
        // that is down, an answer that never validates) stay, so they do not
        // cost a call per read.
        && !(hit.fallback_reason.as_deref().is_some_and(timed_out) && {
            retrying_fallback = cache.claim_retry(session, key, retry_window(plan));
            retrying_fallback
        })
    {
        // A repeat of a fallback is answered at once, and still told apart.
        let (source, fallback_reason) = match hit.fallback_reason {
            Some(reason) => (
                Source::Fallback,
                Some(format!("repeat of an earlier fallback: {reason}")),
            ),
            None => (Source::Cache, None),
        };
        let served = GeneratedResponse {
            status: hit.status,
            content_type: hit.content_type,
            body: hit.body,
            source,
            usage: Usage::default(),
            fallback_reason,
            effects: Vec::new(),
            effect_outcomes: Vec::new(),
            dropped_effects: 0,
            new_record: false,
            patched: None,
            headers: Vec::new(),
            world_gaps: Vec::new(),
            exec_notes: serde_json::Map::new(),
        };
        // The answer's items were seeded when it was generated; writes and
        // resets that could remove them also clear the cache.
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(Box::new(served));
    }

    let earlier = match recalled {
        Some(earlier) => earlier,
        None => {
            write_memory::recall(
                &mut state,
                plan,
                call,
                target_id.as_deref().filter(|_| !foreign_key),
                target.is_some(),
            )
            .instrument(state_span())
            .await
        }
    };

    // Last, so that a 404, a stored or a cached answer costs none of this.
    let ((earlier_answers, unlisted), earlier_writes, (earlier_origins, served_cursors), alongside) =
        async {
            parents_by_name(
                &mut state,
                session,
                op,
                call,
                target.as_ref(),
                &mut followed,
                &mut related,
            )
            .await;
            world::named_by_value(
                &mut state,
                plan,
                call,
                target.as_ref(),
                &followed,
                &mut related,
            )
            .await;
            follow_hops(&mut state, session, target.as_ref(), followed, &mut related).await;
            if is_query
                && target_missing
                && let Some(id) = target_id.as_deref()
            {
                for resource in referrers(&mut state, session, op, id).await {
                    if !holds(&related, &resource) {
                        related.push(resource);
                    }
                }
            }
            let alongside =
                created_alongside(&mut state, plan, call, target.as_ref(), &mut related).await;
            let same_values =
                shown_same_values(&mut state, plan, call, target.as_ref(), &mut related).await;
            (
                world::recall_answers(&mut state, plan).await,
                write_memory::recall_route(&mut state, plan, call).await,
                (
                    minted::recall(&mut state, plan, call).await,
                    cursors::recall(&mut state, plan, call).await,
                ),
                (alongside, same_values),
            )
        }
        .instrument(state_span())
        .await;
    let (alongside, same_values) = alongside;
    if let (Some(variants::Variant::New), Some(held)) = (&variant, target.as_ref())
        && !holds(&related, held)
    {
        related.insert(0, held.clone());
    }
    let created_since =
        write_memory::created_since(&mut state, plan, &earlier, target.as_ref(), &mut related)
            .instrument(state_span())
            .await;

    Prepared::Pending(Box::new(Pending {
        state,
        target_id,
        target,
        related,
        created_since,
        foreign_key,
        cache: since.zip(key),
        retrying_fallback,
        earlier,
        earlier_answers,
        unlisted,
        earlier_writes,
        earlier_origins,
        served_cursors,
        alongside,
        same_values,
        tokens,
        retry: false,
        fill_usage,
        variant: matches!(variant, Some(variants::Variant::New)),
    }))
}

/// Stored `stored` served as `op`'s answer, in the envelope its read holds it in (see
/// [`mb_engine::Engine::held_answer`]), with what `kept` says the session's answers of
/// that envelope said besides the record ([`envelopes`]). `None` when the envelope
/// cannot be told, or the answer does not suit the schema.
fn serve_held(
    rt: &MockRuntime,
    op: &Operation,
    stored: &StoredResource,
    kept: Option<&Map<String, Value>>,
    response_schema: Option<&Value>,
) -> Option<GeneratedResponse> {
    // What the server issued for the record is no member of it: a read never shows it.
    let issued = exec::issued_aliases(rt, &stored.resource_type);
    let shown;
    let data = match &stored.data {
        Value::Object(members) if issued.iter().any(|a| members.contains_key(a)) => {
            let mut members = members.clone();
            members.retain(|name, _| !issued.contains(name));
            shown = Value::Object(members);
            &shown
        }
        data => data,
    };
    let answer = rt
        .engine
        .held_answer(&rt.bundle, op, data, kept, response_schema)?;
    rt.engine
        .serve_stored(&rt.bundle, op, &answer, response_schema)
}

/// The answer to a read of a record the session does not hold, or deleted: a read
/// answering the records an id has as an array has none (`[]`), when its schema allows
/// that; any other answers its `404`.
async fn nothing_held(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    response_schema: Option<&Value>,
    detail: &str,
) -> GeneratedResponse {
    let op = plan.op;
    let lists = op
        .identity
        .as_ref()
        .is_some_and(|i| i.record == Some(mb_mcm::RecordAt::Item));
    if lists
        && let Some(none) =
            rt.engine
                .serve_stored(&rt.bundle, op, &Value::Array(Vec::new()), response_schema)
    {
        return none;
    }
    // A status the description gives a missing record, in the body its response
    // declares.
    if let Some(code) = lifecycle::not_found(rt, op)
        && let Some(refusal) = plan
            .engine
            .refuse_held(
                &rt.bundle,
                op,
                call,
                code,
                ErrorCause::Missing,
                &RefusalFacts::message(detail),
            )
            .await
    {
        return refusal;
    }
    // The engine the provider policy chose for the call writes a declared body's code.
    plan.engine.not_found(&rt.bundle, op, call, detail).await
}

/// A read of a member of its parent (see [`BundleIndex::member_reads`]), answered by
/// that member of the stored parent: Spotify's `/playlists/{id}/items` by the stored
/// playlist's `tracks`. Not with a query, and not while a write the parent's record
/// does not show yet is remembered for the parent or for this route, as the member is
/// then behind it.
async fn member_read(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    response_schema: Option<&Value>,
) -> Option<GeneratedResponse> {
    let rt = state.rt;
    let op = plan.op;
    // A query asks for another view of the member (a page, a filter): it generates.
    if !(rt.consistency
        && matches!(op.kind, OperationKind::Query)
        && plain(plan)
        && call.query.is_empty()
        && mb_engine::seed::request_seed(call).is_none())
    {
        return None;
    }
    let index = rt.index();
    let read = index.member_reads.get(&index.operation(&op.id)?)?;
    let parent_type = rt
        .bundle()
        .operations
        .get(read.parent)?
        .resource_type
        .as_deref()?;
    let id = call.path_params.get(&read.param).and_then(id_string)?;
    let parent = state.get(&plan.session, parent_type, &id).await?;
    let member = parent.data.get(&read.member)?;
    if write_memory::is_behind(state, plan, call, &parent).await {
        return None;
    }
    rt.engine
        .serve_stored(&rt.bundle, op, member, response_schema)
}

/// Whether `answer` fits `plan`'s operation's schema for `status`: the narrowed
/// `response_schema` when given, else the output variant `status` selects.
fn answer_fits(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    status: u16,
    answer: &Value,
    response_schema: Option<&Value>,
) -> bool {
    let op = plan.op;
    let variant = op.output_for_status(status);
    let success = op.success_output();
    // The success variant (or a narrowed schema) has a cached validator.
    if response_schema.is_some()
        || variant.is_some_and(|v| success.is_some_and(|s| std::ptr::eq(v, s)))
    {
        return plan
            .engine
            .serve_stored(&rt.bundle, op, answer, response_schema)
            .is_some();
    }
    let Some(schema) = variant.and_then(|v| v.schema.as_ref()) else {
        return false;
    };
    mb_engine::SchemaValidator::for_output(&rt.bundle.with_reachable_defs(schema))
        .is_ok_and(|validator| validator.is_valid(answer))
}

/// Writes a generated answer's state (a 2xx command's resource, a list
/// read's seeded items), keeps the session alive, sends webhooks and caches
/// a read's answer. `response_schema` is the 2xx schema narrowed to what the caller
/// reads, when it was (see [`generate_narrowed`]).
pub(crate) async fn complete(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    pending: Pending<'_>,
    mut generated: GeneratedResponse,
    response_schema: Option<&Value>,
) -> GeneratedResponse {
    seed_world_gaps(rt, plan.op, &mut generated);
    let Pending {
        mut state,
        mut target_id,
        target,
        related,
        created_since,
        foreign_key,
        cache,
        mut earlier,
        unlisted,
        same_values,
        tokens,
        retry,
        variant,
        retrying_fallback,
        ..
    } = pending;
    let acted_on = target_id.clone();
    let op = plan.op;
    let session = &plan.session;
    // A record of the answer reusing another's id gets one of its own, before anything
    // stores or serves it.
    if rt.consistency
        && matches!(generated.source, Source::Llm | Source::Repaired)
        && (200..300).contains(&generated.status)
        && let Some(body) = generated.body.as_mut()
    {
        let schema = response_schema.or_else(|| {
            op.output_for_status(generated.status)
                .and_then(|o| o.schema.as_ref())
        });
        let skip = op.resource_type.as_deref().zip(acted_on.as_deref());
        // A record named by another member than its key (`product(handle:)`).
        let named_by = op
            .identity
            .as_ref()
            .filter(|i| i.path_param.is_none())
            .and_then(|i| i.input_field.as_deref().filter(|f| *f != i.field))
            .zip(acted_on.as_deref());
        reused_ids::remint(&mut state, session, rt, body, schema, (skip, named_by))
            .instrument(state_span())
            .await;
    }
    let resource_type = op.resource_type.as_deref();
    let is_query = matches!(op.kind, OperationKind::Query);
    let collection = collection_of(op, call).filter(|_| rt.consistency);
    // A new record whose id no draw could free: the answer is served, and
    // nothing else comes of it.
    let mut unstored = false;
    let mut set_moved_identity = false;
    let mut header_id: Option<Value> = None;
    // A bulk create's request records, with the ids minted for them.
    let mut request_records: Vec<Value> = Vec::new();
    // What state keeps of an answer holding its record in an envelope or a one-item
    // array is the record, and of a request the record it holds the same way.
    let held_in = op.identity.as_ref().filter(|i| i.record.is_some());
    let mut answered = held_in
        .filter(|_| (200..300).contains(&generated.status))
        .and_then(|identity| {
            let answer = generated.body.take()?;
            generated.body = identity.record_in(&answer).cloned();
            Some(answer)
        });
    let record_call = match (held_in, call.body.as_ref()) {
        (Some(identity), Some(request)) if answered.is_some() => Some(OperationCall {
            body: identity
                .record_in(request)
                .cloned()
                .or(Some(request.clone())),
            ..call.clone()
        }),
        _ => None,
    };
    let state_call = record_call.as_ref().unwrap_or(call);
    // What a kept page cut of a list left out, which the list's mark holds too.
    let mut listed_rest: Vec<String> = Vec::new();
    async {
        // What the list's query asks for, before its items are seeded or cached. An
        // answer holding one record (`/users/me` answering `{user}`) is no list.
        let fallback = transient_fallback(&generated);
        if is_query
            && target_id.is_none()
            && answered.is_none()
            && (200..300).contains(&generated.status)
            && deleted::applies(rt, plan)
            && let Some(body) = generated.body.as_mut()
        {
            // The model's own next page, after a rest the mock served: not those again,
            // and its token as the model gave it.
            let followed = pages::followed(&mut state, (plan, call), body).await;
            // A list answer that left its list member out (a model writing only what
            // the schema requires) holds an empty one, which the store may fill.
            let absent = match followed {
                None => lists::absent_list(rt, plan, body),
                Some(_) => None,
            };
            // The store first: a filter sees what the session wrote since.
            lists::overlay_stored(&mut state, (plan, call), resource_type, body).await;
            if let Some(followed) = &followed {
                pages::drop_served(plan, rt, body, followed);
            }
            let cut = lists::narrow(rt, plan, call, body);
            // A full page whose rest the model left out, but the store holds.
            let cut = match (fallback, followed.is_some()) {
                (false, false) => match absent.as_deref() {
                    Some(member) => {
                        lists::stored_page(&mut state, (plan, call), resource_type, body, member)
                            .await
                    }
                    None => {
                        lists::stored_rest(&mut state, (plan, call), resource_type, body, cut).await
                    }
                },
                _ => None,
            };
            let kept = match cut {
                Some(cut) => {
                    let field = item_identity(
                        rt.bundle(),
                        op,
                        generated.body.as_ref().unwrap_or(&Value::Null),
                    );
                    listed_rest = cut
                        .rest
                        .iter()
                        .filter_map(|item| mb_mcm::member_at(item, &field).and_then(id_string))
                        .collect();
                    pages::mint(&mut state, (plan, call), resource_type, cut, &mut generated).await
                }
                None => false,
            };
            // A first page kept nothing: no run of an earlier one serves its next pages,
            // and a token of the model's it keeps leads on to nothing the mock served.
            if !kept {
                listed_rest.clear();
                pages::no_next_header(rt, (op, call), &mut generated.headers);
                runs::forget(&mut state, (plan, call)).await;
                if followed.is_none()
                    && let Some(body) = generated.body.as_ref()
                {
                    pages::forget_followed(&mut state, (plan, call), body).await;
                }
            }
        }
        if (200..300).contains(&generated.status)
            && let Some(t) = resource_type
        {
            if is_query && target_id.is_none() && answered.is_none() && state.tracks_deleted {
                let scope = deleted::scope(op, call);
                deleted::drop_from_list(
                    &mut state,
                    session,
                    (plan, call),
                    t,
                    scope.as_deref(),
                    &mut generated,
                )
                .await;
            }
            // A list left with nothing answers the bodiless status declared for that.
            if is_query
                && target_id.is_none()
                && answered.is_none()
                && plan.forced_status.is_none()
                && plan.scenario.is_none()
                && deleted::applies(rt, plan)
            {
                lists::no_content(rt.bundle(), op, &mut generated);
            }
            if matches!(op.kind, OperationKind::Command(Effect::Create)) {
                // The answer as the model gave it, before a fresh id replaces one a stored
                // record holds: whether it is that record (see [`repeats`]).
                let as_answered = match same_values.is_empty() {
                    true => None,
                    false => generated.body.clone(),
                };
                // What the description says of the answer: a long-running create is done.
                records::settle_create(rt, op, &mut generated);
                // Each record a bulk or long-running create makes holds an id.
                request_records =
                    records::mint_created(&mut state, (session, op, t, state_call), &mut generated)
                        .await;
                // A record answered with no identity (a model writing only what the
                // schema requires) would not be stored: it is given one.
                mint_create_id(
                    rt,
                    &mut state,
                    (session, op, t, state_call),
                    &mut generated,
                    answered.as_ref(),
                )
                .await;
                unique_create_id(
                    rt,
                    &mut state,
                    (session, op, t, state_call),
                    &mut generated,
                    answered.as_ref(),
                )
                .await;
                next_numbers(rt, &mut state, session, t, &mut generated).await;
                // An answer naming its record only in a header: the id is minted there.
                header_id = header_ids::mint(
                    rt,
                    &mut state,
                    (session, op, t, state_call),
                    &mut generated,
                    plan.page_url.as_deref(),
                )
                .await;
                // A create answered for the id of a record held already does not replace
                // it: nothing comes of it, and the answer agrees with the store. It is the
                // refusal the prompt offered on that record, when it did; otherwise (a
                // forced status, a scenario, no refusal declared or offered) the held
                // record, under the status answered, as an idempotent create answers.
                let repeated =
                    repeated_record(&mut state, (session, op, t, state_call), &generated).await;
                // Held in another collection (another parent's label of the same name):
                // the answer is served, but stored over that record it is not.
                if let Some(Repeated::Elsewhere(id)) = &repeated {
                    unstored = true;
                    tracing::info!(
                        mock = %rt.name,
                        operation = %op.id,
                        id = %log_capped(id),
                        "create not stored: its id is held in another collection"
                    );
                }
                let here = matches!(repeated, Some(Repeated::Here(_)));
                if let Some(Repeated::Here(held)) = repeated {
                    unstored = true;
                    drop_all_effects(rt, op, &held.id, &mut generated);
                    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
                    // An answer that is the held record as stored (an "it exists" answer)
                    // is right: it stands, neither refused nor replaced.
                    let as_held = repeats::answers_the_held(generated.body.as_ref(), &held, field);
                    let offered = same_values
                        .iter()
                        .any(|(st, id, member)| st == t && *id == held.id && member == field);
                    let refusal = (offered && !as_held)
                        .then(|| plan.engine.refuse_repeat(rt.bundle(), op, call, &generated))
                        .flatten();
                    match refusal {
                        None if as_held => {}
                        Some(mut refusal) => {
                            refusal.effect_outcomes =
                                std::mem::take(&mut generated.effect_outcomes);
                            refusal.dropped_effects = generated.dropped_effects;
                            answered = None;
                            generated = refusal;
                        }
                        None => generated.body = Some(held.data),
                    }
                }
                // A success for a create that repeats a value the contract says is
                // unique, which a stored record in its collection holds: the refusal
                // offered, enforced (see [`repeats`]).
                let enforced = match !here && (200..300).contains(&generated.status) {
                    true => {
                        repeats::enforced(
                            &mut state,
                            (session, op, t),
                            (call, as_answered.as_ref()),
                            &same_values,
                        )
                        .await
                    }
                    false => None,
                };
                match enforced {
                    // The holder itself, as stored: served as the model answered it, and
                    // stored over nothing.
                    Some(repeats::Enforced::Held { id, stored }) => {
                        unstored = true;
                        drop_all_effects(rt, op, &id, &mut generated);
                        generated.body = stored.or(as_answered);
                    }
                    Some(repeats::Enforced::Refuse { code, member, held }) => {
                        if let Some(refusal) = plan.engine.refuse_repeated_value(
                            rt.bundle(),
                            op,
                            call,
                            (code, &member),
                            &generated,
                        ) {
                            unstored = true;
                            answered = None;
                            repeats::replace(rt, op, (&member, &held), &mut generated, refusal);
                        }
                    }
                    None => {}
                }
            }
            // An action's new record goes under its own id, not over another's.
            if generated.new_record {
                unstored = !place_new_record(
                    rt,
                    &mut state,
                    (session, op, t, state_call),
                    &mut generated,
                    answered.as_ref(),
                )
                .await;
                if !unstored {
                    next_numbers(rt, &mut state, session, t, &mut generated).await;
                }
                let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
                let own = generated
                    .body
                    .as_ref()
                    .and_then(|b| mb_mcm::member_at(b, field));
                if let Some(id) = own.and_then(id_string) {
                    target_id = Some(id);
                }
            }
            // A created record shows the described initial state it is stored in.
            // The answer is the record here (unwrapped from where the identity
            // says the answer holds it). A GraphQL record holds only what its
            // selection asked for, and names its type.
            let names_type = |record: &Value| {
                record
                    .get("__typename")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.eq_ignore_ascii_case(t))
            };
            if op.kind == OperationKind::Command(Effect::Create)
                && !unstored
                && (200..300).contains(&generated.status)
                && let Some(record) = generated.body.as_mut()
                && (entities::entity_fits(rt, t, record) || names_type(record))
            {
                let request = call
                    .body
                    .as_ref()
                    .map(|b| records::request_record(rt, op, b));
                lifecycle::initial(rt, t, record, request);
                // What the description sets on a created record holds on one the
                // executor left to generation too: in the answer, and so in what is
                // stored from it.
                let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
                let before = mb_mcm::member_at(record, field).cloned();
                transitions::apply_on_create(&mut state, (plan, call), record, false)
                    .instrument(state_span())
                    .await;
                set_moved_identity = mb_mcm::member_at(record, field) != before.as_ref();
            }
            // A `set` that gave the record another identity is checked again: a record
            // held under it is not stored over. The answer is the refusal of a repeat
            // the contract declares, else the held record, as an idempotent create.
            if set_moved_identity {
                let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
                let set_id = generated
                    .body
                    .as_ref()
                    .and_then(|b| mb_mcm::member_at(b, field))
                    .and_then(id_string);
                let held = match set_id {
                    Some(id) => state.get(session, t, &id).await,
                    None => None,
                };
                let repeated = held.map(|held| {
                    if owners::in_create_collection(rt, session, op, state_call, &held) {
                        Repeated::Here(held)
                    } else {
                        Repeated::Elsewhere(held.id)
                    }
                });
                match repeated {
                    Some(Repeated::Here(held)) => {
                        unstored = true;
                        drop_all_effects(rt, op, &held.id, &mut generated);
                        match plan.engine.refuse_repeat(rt.bundle(), op, call, &generated) {
                            Some(mut refusal) => {
                                refusal.effect_outcomes =
                                    std::mem::take(&mut generated.effect_outcomes);
                                refusal.dropped_effects = generated.dropped_effects;
                                answered = None;
                                generated = refusal;
                            }
                            None => generated.body = Some(held.data),
                        }
                    }
                    Some(Repeated::Elsewhere(_)) => unstored = true,
                    None => {}
                }
            }
            // What the description says an update or action sets on its target shows
            // in the answer's record too.
            if !generated.new_record {
                let before = target.as_ref().map(|t| &t.data);
                transitions::onto_answer(
                    &mut state,
                    (plan, call),
                    (before, target_id.as_deref()),
                    &mut generated,
                )
                .instrument(state_span())
                .await;
            }
            // A faker list standing in for a failed LLM call seeds nothing,
            // or its items would join the session's later LLM lists.
            let seeding = is_query
                && rt.consistency
                && target_id.is_none()
                && !transient_fallback(&generated);
            // A single answer with an identity of its own is one record, not a list of
            // what it holds (a weather reading's `weather` conditions). It is stored
            // under its identity only on a route with no path parameter, which then
            // reads it: one with a parameter that is not the identity (TMDB's
            // `/movie/{movie_id}/credits`) would make a type of what no read finds, and
            // one with no parameters at all (`/me`) is an earlier answer (see
            // [`world`]) instead.
            let (in_path, single_read) = match &op.binding {
                Binding::Http { path, .. } => (
                    path.contains('{'),
                    rt.index().single_reads.contains_key(path),
                ),
                _ => (false, false),
            };
            let own_record = op
                .identity
                .as_ref()
                .filter(|i| seeding && i.path_param.is_none())
                .and_then(|i| i.id_in(generated.body.as_ref()?).and_then(id_string));
            if let Some(id) = own_record {
                if let Some(body) = generated.body.clone().filter(|_| !in_path && !single_read) {
                    let resource = StoredResource {
                        resource_type: t.to_string(),
                        id,
                        data: body,
                    };
                    state.insert_seeded(session, resource).await;
                }
            } else if (!is_query || seeding)
                && !unstored
                && !write_memory::keeps_stored(rt, plan, &generated, target.is_some())
            {
                let origin = if is_query {
                    WriteOrigin::Seeded
                } else {
                    WriteOrigin::Call
                };
                let outcome = write_state(
                    &mut state,
                    WriteRequest {
                        session,
                        op,
                        resource_type: t,
                        // What the key holds is another's: this write has no
                        // key of its own to go under.
                        target_id: target_id.clone().filter(|_| !foreign_key),
                        generated: &generated,
                        request_body: state_call.body.as_ref(),
                        call_body: call.body.as_ref(),
                        called: Some((plan, call)),
                        query: Some(&call.query),
                        collection: collection.clone(),
                        origin,
                        held: target.is_some() && !generated.new_record,
                        header_id: header_id.clone(),
                        request_records: std::mem::take(&mut request_records),
                    },
                )
                .await;
                // A described list's collection is listed now: later lists of it are
                // answered from state.
                if origin == WriteOrigin::Seeded {
                    let rest = std::mem::take(&mut listed_rest);
                    state_lists::mark_listed(&mut state, (plan, call), t, &generated, rest).await;
                }
                // A create or update that found no record of its type to store: the
                // description leaves where its record is open. A type with no identity
                // is marked `no_identity` already, which says why; with no world
                // attached nothing is marked, as no static gap is.
                let writes_record = matches!(
                    op.kind,
                    OperationKind::Command(Effect::Create | Effect::Update)
                );
                let mark = GapCode::NoRecord.as_str();
                let marked =
                    |code: GapCode| generated.world_gaps.iter().any(|g| g == code.as_str());
                if writes_record
                    && outcome.no_record
                    && rt.world().is_some()
                    && !marked(GapCode::NoRecord)
                    && !marked(GapCode::NoIdentity)
                {
                    generated.world_gaps.push(mark.to_string());
                }
                if op.kind == OperationKind::Command(Effect::Delete)
                    && !foreign_key
                    && let Some(id) = target_id.as_deref()
                    && (state.tracks_deleted || graphql_marks(rt, plan, t, id, &generated))
                {
                    deleted::mark(&mut state, session, (op, call), t, id, target.as_ref()).await;
                }
                // The operation's effects, once its own write of its target landed (a
                // removal marked deleted), as the executor's commit runs them: here, the
                // one place a generated answer's target is written. The answer's
                // computed members are filled below, reading the session as the effects
                // left it. Unlike the executor's update, whose tokens advance before it
                // stores, a generated update's tokens are minted after its effects run
                // (`tokens::after_update`, below): its effects read them as held before.
                if origin == WriteOrigin::Call
                    && let Some(written) = &outcome.target
                    && state.available()
                {
                    let (wt, wid) = (written.resource_type.as_str(), written.id.as_str());
                    side_writes::apply(&mut state, (plan, call), wt, (wid, &written.data)).await;
                }
            }
        }
        // A list paged by its `Range` header: ordered and cut from what the session
        // holds, once its own items are seeded.
        if is_query
            && target_id.is_none()
            && answered.is_none()
            && (200..300).contains(&generated.status)
        {
            ranges::apply(&mut state, (plan, call), resource_type, &mut generated).await;
        }
        // What else the envelope said, for a stored record to be answered the same.
        if let Some(answer) = answered.as_ref().filter(|_| !unstored) {
            envelopes::remember(&mut state, plan, answer, &generated).await;
        }
        if matches!(op.kind, OperationKind::Command(Effect::Create))
            && (200..300).contains(&generated.status)
            && !unstored
        {
            // The record an envelope holds is stored already, under its identity.
            let wrapped = answered.as_ref().map(|answer| GeneratedResponse {
                body: Some(answer.clone()),
                ..generated.clone()
            });
            let own = held_in.and_then(mb_mcm::Identity::envelope_member);
            store_wrapped(
                &mut state,
                session,
                (resource_type, own),
                wrapped.as_ref().unwrap_or(&generated),
                op.output_for_status(generated.status)
                    .and_then(|o| o.schema.as_ref()),
                collection.as_ref(),
            )
            .await;
        }
        // A read in another language than its target: its record is that language's.
        if variant
            && plain(plan)
            && (200..300).contains(&generated.status)
            && !transient_fallback(&generated)
            && let (Some(held), Some(record)) = (target.as_ref(), generated.body.as_ref())
        {
            rt.variants().keep(session, (op, call), held, record);
        }
        // A request naming a seed asks for that seed's answer, and a query for
        // one view: the cache keeps those apart, so neither is the resource.
        if rt.consistency
            && plain(plan)
            && !variant
            && mb_engine::seed::request_seed(call).is_none()
            // A catch-up's answer has the whole resource's schema, whatever the
            // query asked: it settles as a plain read does.
            && (call.query.is_empty()
                || (is_query && !earlier.shown.is_empty() && target.is_some()))
            && !transient_fallback(&generated)
            && (200..300).contains(&generated.status)
            && !matches!(op.binding, Binding::Graphql { .. })
        {
            if is_query && !earlier.shown.is_empty() {
                write_memory::settle_read(
                    &mut state,
                    plan,
                    call,
                    (target_id.as_deref(), target.as_ref()),
                    &mut earlier,
                    &mut generated,
                )
                .await;
            } else {
                seed_own_read(
                    &mut state,
                    session,
                    rt,
                    op,
                    call,
                    target_id.as_deref(),
                    &generated,
                )
                .await;
            }
        }
        // An operation with no resource type is stateless, GraphQL stores the
        // objects of its answers under their own types, and a delete's answer
        // may name what it just removed.
        if (200..300).contains(&generated.status)
            && resource_type.is_some()
            && rt.consistency
            && !transient_fallback(&generated)
            && !matches!(op.binding, Binding::Graphql { .. })
            && !matches!(op.kind, OperationKind::Command(Effect::Delete))
            && !unstored
            && let Some(body) = generated.body.as_ref()
        {
            let schema = op
                .output_for_status(generated.status)
                .and_then(|o| o.schema.as_ref());
            // A create's answer may wrap its own resource.
            let created = resource_type
                .filter(|_| matches!(op.kind, OperationKind::Command(Effect::Create)))
                .zip(collection.as_ref());
            let body = answered.as_ref().unwrap_or(body);
            seed_entities(&mut state, session, rt, body, schema, created).await;
        }
        // Last: an effect on what this call just stored sees it.
        if rt.consistency && (200..300).contains(&generated.status) {
            // An effect on an update's own target goes on what was just stored, under
            // the protections the answer has.
            let acted = resource_type.zip(acted_on.as_deref());
            let own = acted
                .filter(|_| op.kind == OperationKind::Command(Effect::Update) && !op.is_action());
            let shown = shown_types(rt, target.as_ref(), &related, &created_since);
            resolve_effect_types(&mut state, session, rt, &shown, &mut generated.effects).await;
            let shown_records = shown_records(target.as_ref(), &related, &created_since);
            resolve_effect_ids(&mut state, session, &shown_records, &mut generated.effects).await;
            write_memory::effects_on_target(&earlier, target.as_ref(), &mut generated.effects);
            if let Some(t) =
                resource_type.filter(|_| op.kind == OperationKind::Command(Effect::Create))
            {
                let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
                let created = generated
                    .body
                    .as_ref()
                    .and_then(|b| mb_mcm::member_at(b, field))
                    .and_then(id_string);
                let effects = std::mem::take(&mut generated.effects);
                let (kept, dropped) =
                    sibling_effects(t, created.as_deref(), state_call.body.as_ref(), effects);
                generated.effects = kept;
                generated.dropped_effects += dropped.len();
                generated
                    .effect_outcomes
                    .extend(dropped.into_iter().map(|effect| {
                        EffectOutcome {
                            effect,
                            rejected: Some(
                                "a create copies its request onto no other record of its type"
                                    .to_string(),
                            ),
                            merged: None,
                            applied: Vec::new(),
                        }
                    }));
            }
            let outcomes = apply_effects(
                &mut state,
                session,
                rt,
                &generated.effects,
                (own, acted),
                (plan, call, generated.status),
            )
            .await;
            generated.effect_outcomes.extend(outcomes);
            write_memory::serve_own_effects(rt, op, own, &mut generated);
            if generated.dropped_effects > 0 {
                metrics::counter!(
                    EFFECTS_METRIC,
                    "mock" => rt.name.clone(),
                    "outcome" => "rejected"
                )
                .increment(generated.dropped_effects as u64);
            }
            if is_query {
                write_memory::settle_by_effects(
                    &mut state,
                    plan,
                    &earlier,
                    target.as_ref(),
                    &mut generated,
                )
                .await;
            }
        }
        // After the effects, which may advance a token too.
        if (200..300).contains(&generated.status)
            && !unstored
            && !foreign_key
            && matches!(
                generated.source,
                Source::Llm | Source::Patched | Source::Repaired
            )
            && let Some(before) = target.as_ref()
            && op.kind == OperationKind::Command(Effect::Update)
            && !generated.new_record
        {
            tokens::after_update(
                &mut state,
                session,
                before,
                &tokens,
                generated.body.as_mut(),
            )
            .await;
        }
        // Any delete of a record ends its tokens, whatever answered it.
        if (200..300).contains(&generated.status)
            && rt.consistency
            && !foreign_key
            && matches!(op.binding, Binding::Http { .. })
            && op.kind == OperationKind::Command(Effect::Delete)
            && let Some(before) = target.as_ref()
        {
            tokens::forget(&mut state, session, &before.resource_type, &before.id).await;
        }
        // After the effects: one on the resource means it shows the write.
        write_memory::after_write(
            &mut state,
            rt,
            plan,
            &write_memory::Written {
                call: state_call,
                generated: &generated,
                acted_on: acted_on.as_deref(),
                held: target.as_ref(),
                foreign_key,
                unstored,
                earlier: &earlier,
                retry,
            },
        )
        .await;
        write_memory::after_route_write(
            &mut state,
            plan,
            call,
            &generated,
            (foreign_key || unstored, retry),
        )
        .await;
        // What state the record's entity tag stands for now, for a later `304`: the
        // record acted on, or the one a create answered.
        if (200..300).contains(&generated.status)
            && !unstored
            && !foreign_key
            && let Some(t) = resource_type
        {
            let created = || {
                let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
                generated
                    .body
                    .as_ref()
                    .filter(|_| op.kind == OperationKind::Command(Effect::Create))
                    .and_then(|b| mb_mcm::member_at(b, field))
                    .and_then(id_string)
            };
            if let Some(id) = target_id.clone().or_else(created) {
                tokens::after_answer(&mut state, plan, t, &id).await;
            }
        }
        // A held record's computed members, from the session as state now has it.
        if (200..300).contains(&generated.status)
            && !unstored
            && !foreign_key
            && let Some(t) = resource_type
            && computed::describes(rt, t)
            && match op.kind {
                OperationKind::Command(Effect::Create) => true,
                OperationKind::Command(Effect::Delete) => false,
                _ => target.is_some(),
            }
            && let Some(body) = generated.body.as_mut()
            && entity_fits(rt, t, body)
        {
            let before = body.clone();
            let set = computed::fill(&mut state, (plan, call), t, body).await;
            // The answer it ends up in, put back in its envelope, still fits the
            // operation's schema for its status; else the model's values stay.
            if !set.is_empty() {
                let whole = match (held_in, answered.as_ref()) {
                    (Some(identity), Some(answer)) => {
                        let mut whole = answer.clone();
                        identity
                            .record_in_mut(&mut whole)
                            .map(|slot| *slot = body.clone())
                            .map(|()| whole)
                    }
                    _ => Some(body.clone()),
                };
                let fits = whole.is_some_and(|whole| {
                    answer_fits(rt, plan, generated.status, &whole, response_schema)
                });
                if !fits {
                    tracing::info!(
                        mock = %rt.name,
                        operation = %op.id,
                        "an answer with its computed members does not fit its schema: not set"
                    );
                    *body = before;
                }
            }
        }
        // Served as it was answered, holding the record as state now has it.
        if let (Some(identity), Some(mut answer)) = (held_in, answered.take()) {
            if let (Some(record), Some(slot)) =
                (generated.body.take(), identity.record_in_mut(&mut answer))
            {
                *slot = record;
            }
            generated.body = Some(answer);
        }
        world::list_session(&mut state, plan, unlisted, &generated).await;
        minted::after_answer(&mut state, plan, call, &generated).await;
        cursors::after_answer(&mut state, plan, &generated).await;
        world::after_answer(&mut state, plan, call, &generated).await;
        world::after_delete(&mut state, plan, acted_on.as_deref(), &generated).await;
        // The final answer, for a repeat of a described idempotency key.
        replays::keep(&mut state, plan, call, &generated).await;
        state.touch(session).await;
    }
    .instrument(state_span())
    .await;
    // Any successful command may change what a cached read would answer,
    // even one that stored nothing or stored another resource type (Slack's
    // `chat.postMessage` and `conversations.history`).
    if !is_query
        && (200..300).contains(&generated.status)
        && let Some(response_cache) = rt.state.response_cache()
    {
        response_cache.clear_session(session);
    }
    // Every caller of `complete` is one of these adapters.
    #[cfg(any(
        feature = "http-rest",
        feature = "graphql",
        feature = "grpc",
        feature = "soap"
    ))]
    if !unstored {
        crate::webhooks::emit::emit(
            rt,
            &plan.engine,
            op,
            call,
            &generated,
            target_id.as_deref(),
            session,
        );
    }
    #[cfg(any(
        feature = "http-rest",
        feature = "graphql",
        feature = "grpc",
        feature = "soap"
    ))]
    crate::webhooks::emit::emit_effects(rt, op, &generated.effect_outcomes, session);

    // Whatever made it, a base64 field is served (and cached) as valid base64.
    let generated = plan
        .engine
        .encode_base64(&rt.bundle, op, response_schema, generated);
    // Seeded first, then cached: a later list never clears a cached one.
    // A fallback is cached too: repeats are answered with it at once, except
    // that a timed-out one is retried by the next repeat (see `prepare`), so
    // only repeats arriving during that retry are. Not one of a read still
    // owing earlier requests, which is to try again.
    if let (Some(response_cache), Some((since, key))) = (rt.state.response_cache(), cache) {
        if (200..300).contains(&generated.status) && earlier.cacheable(&generated) {
            response_cache.put_if_current(
                session,
                key.clone(),
                resource_type,
                CachedResponse {
                    status: generated.status,
                    content_type: generated.content_type.clone(),
                    body: generated.body.clone(),
                    fallback_reason: transient_fallback(&generated)
                        .then(|| generated.fallback_reason.clone())
                        .flatten(),
                },
                since,
            );
        }
        // A retry of a cached fallback that cached nothing in its place
        // leaves the next repeat to try again; a real answer clears the
        // backoff of one that kept timing out.
        if retrying_fallback {
            response_cache.release_retry(session, &key, generated.fallback_reason.is_none());
        }
    }
    generated
}

/// Whether a fallback reason says the model did not answer in time: the
/// engine's reasons for a timed-out request, stream or repair all say so.
fn timed_out(reason: &str) -> bool {
    reason.contains("timed out")
}

/// How long a retry of a cached fallback holds its claim (see
/// [`mb_state::ResponseCache::claim_retry`]): one generation's time limit.
fn retry_window(plan: &CallPlan<'_>) -> std::time::Duration {
    let config = plan.engine.config();
    config.generation_timeout.unwrap_or(config.timeout)
}

/// The records a prompt showed (see [`shown_types`]), in the order it showed them.
fn shown_records<'a>(
    target: Option<&'a StoredResource>,
    related: &'a [StoredResource],
    created_since: &'a [StoredResource],
) -> Vec<&'a StoredResource> {
    let max = mb_engine::prompt::MAX_RELATED;
    target
        .into_iter()
        .chain(related.iter().take(max))
        .chain(created_since.iter().take(max))
        .collect()
}

/// Gives each effect whose id holds no record of its type, written with punctuation
/// around it (`:722754` for `722754`), the id of the `shown` record of its type that the
/// id names with that punctuation trimmed. With none, the effect is left as it came,
/// and is rejected.
async fn resolve_effect_ids(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    shown: &[&StoredResource],
    effects: &mut [ResourceEffect],
) {
    for effect in effects.iter_mut() {
        let trimmed = effect.id.trim_matches(|c: char| !c.is_alphanumeric());
        if trimmed.is_empty() || trimmed == effect.id {
            continue;
        }
        // A type and an id name one record, however often the prompt showed it.
        if !shown
            .iter()
            .any(|r| r.resource_type == effect.resource_type && r.id == trimmed)
        {
            continue;
        }
        let id = trimmed.to_string();
        if state
            .get(session, &effect.resource_type, &effect.id)
            .await
            .is_some()
        {
            continue;
        }
        tracing::debug!(
            written = %log_capped(&effect.id),
            id = %log_capped(&id),
            "effect id trimmed to a shown record's"
        );
        effect.id = id;
    }
}

/// Whether the path parameter carries the value state is keyed by: it is
/// the identity field itself, or id-like (`id`, `orderId`, `movie_id`). A
/// `/users/{username}` read keyed by `id` cannot tell a missing user.
fn path_names_identity(identity: &mb_mcm::Identity) -> bool {
    let Some(param) = identity.path_param.as_deref() else {
        return false;
    };
    param == mb_mcm::leaf_member(&identity.field) || identity.path_field.is_some() || id_like(param)
}

/// Whether `op`'s path parameter selects the record a missing id is missing from, for
/// `unknown_ids` (see [`unknown_ids`]): [`selects_record`], with the target path
/// parameter the world description writes for it.
fn targets_by_path(rt: &MockRuntime, op: &Operation) -> bool {
    let written = rt
        .writes_path_target(&op.id)
        .then(|| rt.described().op(&op.id))
        .flatten()
        .and_then(|d| d.target.as_ref())
        .and_then(|t| t.path_param.as_deref());
    selects_record(op, written, rt.consistency)
}

/// Whether `op`'s identity path parameter selects one record of its type, decided by
/// what it resolves to, not by its name. Where the world description writes the target's
/// path parameter (`written`), whether that is the one the identity reads. Otherwise
/// whether the identity's parameter ends the route ([`trailing_path_param`]: a literal
/// suffix of its segment aside, `{Sid}.json`), so the route ends at one record of the
/// read's type (`/v1/customers/{customer}`, `/v1/checkout/sessions/{session}`), with
/// `consistency` on: a value that is not the key is then also looked for under the
/// members a name goes by (`lookup::read_context`), which `consistency: off` skips, so a
/// miss there says nothing. A parameter further up the route, or with `consistency` off,
/// counts when its name says it holds the id ([`path_names_identity`]).
fn selects_record(op: &Operation, written: Option<&str>, consistency: bool) -> bool {
    let Some(identity) = op.identity.as_ref() else {
        return false;
    };
    if let Some(param) = written {
        return identity.path_param.as_deref() == Some(param);
    }
    let trailing = match &op.binding {
        Binding::Http { path, .. } => trailing_path_param(path),
        _ => None,
    };
    (consistency && trailing.is_some() && identity.path_param == trailing)
        || path_names_identity(identity)
}

/// The parameter a route template ends at: [`last_path_param`] when nothing but a
/// literal suffix of its own segment follows it (`/Messages/{Sid}.json`); none for
/// `/a/{x}/b`.
fn trailing_path_param(template: &str) -> Option<String> {
    let param = last_path_param(template)?;
    let end = template.rfind('}')?;
    (!template[end..].contains('/')).then_some(param)
}

/// What `unknown_ids` left unset goes by, per resource type of a contract: the types a
/// read selects one record of by a path parameter (the identity's, where it ends the
/// route or the world description writes it), and the types an
/// operation of kind create makes, each with the first such operation (by position).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UnknownIdTypes {
    /// The types read by a path parameter that selects one record.
    pub read: std::collections::BTreeSet<String>,
    /// The creatable types, with the id of their first create.
    pub creatable: std::collections::BTreeMap<String, String>,
}

/// [`UnknownIdTypes`] of `bundle` (a bundle the world description was applied to, as a
/// runtime serves it), where the description writes the target path parameter of the
/// operations `written` names ([`mb_mcm::world::written_path_targets`]), in `world`, and
/// the mock has `consistency`. The runtime decides each read the same way.
#[must_use]
pub fn unknown_id_types(
    bundle: &mb_mcm::Bundle,
    world: &mb_mcm::world::World,
    written: &std::collections::BTreeSet<String>,
    consistency: bool,
) -> UnknownIdTypes {
    use mb_mcm::world::OpKind;
    let mut out = UnknownIdTypes::default();
    for op in &bundle.operations {
        let Some(t) = op.resource_type.as_deref() else {
            continue;
        };
        match OpKind::of(op) {
            Some(OpKind::Create) => {
                out.creatable
                    .entry(t.to_owned())
                    .or_insert_with(|| op.id.clone());
            }
            Some(OpKind::Read) => {
                let param = written
                    .contains(&op.id)
                    .then(|| world.operations.get(&op.id))
                    .flatten()
                    .and_then(|d| d.target.as_ref())
                    .and_then(|t| t.path_param.as_deref());
                if selects_record(op, param, consistency) {
                    out.read.insert(t.to_owned());
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether a path parameter's name says it holds an id.
fn id_like(param: &str) -> bool {
    param.eq_ignore_ascii_case("id") || param.ends_with("Id") || param.ends_with("_id")
}

/// Whether another path parameter comes before the one naming `op`'s target:
/// that value then only means something under the earlier one.
fn follows_a_param(op: &Operation, identity: &mb_mcm::Identity) -> bool {
    let (Binding::Http { path, .. }, Some(param)) = (&op.binding, identity.path_param.as_deref())
    else {
        return false;
    };
    let own = format!("{{{param}}}");
    path.split('/')
        .take_while(|s| !s.contains(&own))
        .any(|s| s.contains('{'))
}

/// Whether no scenario applies to the call and no status is forced (an
/// injected chaos error forces one).
fn plain(plan: &CallPlan<'_>) -> bool {
    plan.scenario.is_none() && plan.forced_status.is_none()
}

/// The span of a call's state reads, or of its writes.
fn state_span() -> tracing::Span {
    tracing::info_span!("mb.state")
}

/// `reason` as a header value: printable ASCII only (anything else becomes
/// `?`), cut to 300 bytes.
#[cfg(any(feature = "http-rest", feature = "graphql"))]
pub(crate) fn reason_header(reason: &str) -> axum::http::HeaderValue {
    let clean: String = reason
        .chars()
        .map(|c| {
            if c == ' ' || c.is_ascii_graphic() {
                c
            } else {
                '?'
            }
        })
        .take(300)
        .collect();
    axum::http::HeaderValue::from_str(&clean)
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("unprintable"))
}

#[cfg(test)]
mod tests;
