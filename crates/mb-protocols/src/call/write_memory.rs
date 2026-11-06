//! What a 2xx write did not say, kept for the next generation of its resource.
//!
//! A create answering `{id, key}` or an action answering 204 leaves a stored
//! copy that does not hold what the request asked. A 204 update's members that the
//! read's schema declares go into the copy at once; what could not be applied is
//! remembered for the resource the write targets, and for the parent its path names
//! when the write is a create, a delete or an action answering nothing, under a reserved type
//! of the state store ([`mb_state::INTERNAL_TYPE`]). So Redis, SQL and memory all
//! keep it, sessions, TTLs and resets clear it, and it is no resource (not exported,
//! counted or seeded, and on no change feed). A remembered action publishes a change
//! of its target, for replicas, feeds and triggers; telling a parent changes nothing
//! it holds, and its read looks in the shared store before it serves, on any node.
//!
//! A create also tells the held records its request refers to by value, by the
//! matching [`world`] shows a request's references with: a reply the message it
//! answers, a refund its payment. Those change only by the effects their next read's
//! answer lists, and that read answers the stored copy (see [`Earlier::by_effects`]). No
//! generation whose answer replaces the stored copy is told such a request: a direct
//! update of the record settles as though it were not there, a read with other requests
//! to catch up on is told only those, and it waits for a read with nothing else. A
//! parent and those keep, with the request, the record it created, which the read
//! catching up is shown as new (see [`created_since`]).
//!
//! The next generation for that resource is shown the requests; a read, which could
//! otherwise answer from state and never see them, must generate. Its validated
//! answer replaces the stored copy and the memory goes.
//!
//! The cost: one catch-up succeeds per batch of remembered writes, and writes
//! landing between attempts make more. A failing catch-up tries [`MAX_ATTEMPTS`]
//! times, each up to two LLM calls (the answer and its repair), then state answers
//! until a later write adds a request and resets the count. Two reads at once can
//! each generate.
//!
//! The document is read, changed and written back, as an effect's merge is, so two
//! writes to one resource at the same moment can lose one request, and a count can
//! miss a failure. No lock: what is lost is a hint to a mock's LLM.
//!
//! A write that left no record a list read shows (`POST /users/{id}/roles` answering
//! 204, Spotify's add answering a snapshot id) is also kept for the path it was made to,
//! and told to the list read of that path. Only told, as what may since have changed: the
//! read is not forced and nothing stands over its answer. Its query is never kept, and of
//! its body only what the contract declares of the write's request or the list's items,
//! past the wider checks of [`for_context`], as the rest may be anything. The memory goes with the path's parent, and a write goes
//! with the child it named. A write is kept the same way for the list reads one level
//! under its path's parent that its route or a member of its body names (see
//! [`tell_siblings`]): `POST /lists/{id}/relationships/profiles` for `/lists/{id}/profiles`.
//!
//! HTTP-bound operations only: GraphQL stores what its answers hold under their
//! own types, and gRPC and SOAP are not remembered for, so their prompts and their
//! reads are as they were.

use mb_engine::REDACTED;
use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::*;

/// Requests kept per resource, the newest.
const MAX_REMEMBERED: usize = 5;
/// Largest request body kept as it is, as serialized JSON. A larger one is cut.
const MAX_BODY_BYTES: usize = 4 * 1024;
/// A body serialized larger than this is not even previewed: the PII scrubber
/// skips very long strings, so a cut of one could keep what it did not see.
const MAX_PREVIEWED_BYTES: usize = 64 * 1024;
/// Largest memory of one resource, as serialized JSON. Oldest requests go first.
const MAX_BYTES: usize = 16 * 1024;
/// Catch-ups that may fail before a read stops trying, until a later write.
const MAX_ATTEMPTS: u64 = 2;

/// What a generation was told of one resource's remembered requests.
#[derive(Default)]
pub(super) struct Earlier {
    /// Where they are kept.
    key: Option<String>,
    /// The requests as kept, oldest first, each telling apart a repeat.
    pub(super) shown: Vec<Value>,
    /// The requests as a prompt shows them: with no time, or no prompt would
    /// ever repeat, and order says what time would.
    pub(super) told: Vec<Value>,
    /// What the latest write said of the resource itself, if it was an update's or
    /// a create's own body: all an answer has to keep. Last write wins, so any later
    /// write replaces or clears it, and older bodies are only told.
    pub(super) own: Option<Value>,
    /// Every request kept only referred to the resource (see [`after_write`]): it
    /// changes only by the effects a read's answer lists, and the stored copy stands.
    /// Never for a direct update of the resource, nor for a read with other requests to
    /// catch up on: those are told none of what only referred (see [`recall`]).
    pub(super) by_effects: bool,
    /// Catch-ups that failed since the last write.
    attempts: u64,
    /// This call's catch-up failed, so its answer is not one to repeat.
    failed_now: bool,
    /// What the store holds that the read could serve, were it not behind them:
    /// it answers when the LLM fails, as a faker must not invent a change.
    pub(super) stored_answer: Option<Box<GeneratedResponse>>,
}

/// One served write, for [`after_write`].
pub(super) struct Written<'a> {
    pub(super) call: &'a OperationCall,
    pub(super) generated: &'a GeneratedResponse,
    /// The id the call named or found, before any new record of its own.
    pub(super) acted_on: Option<&'a str>,
    /// What the session held for this caller under that id.
    pub(super) held: Option<&'a StoredResource>,
    /// The key holds another owner's resource.
    pub(super) foreign_key: bool,
    /// A new record that could not be stored.
    pub(super) unstored: bool,
    pub(super) earlier: &'a Earlier,
    /// The write is a client's retry (see [`is_retry`]).
    pub(super) retry: bool,
}

/// Where a resource's memory is kept. By its stored id when there is one; a value
/// no resource holds only came from a path, and may be the secret
/// (`POST /tokens/{token}/revoke`), so it goes by a hash.
fn memory_key(resource_type: &str, id: &str, stored: bool) -> String {
    if stored {
        return format!("{resource_type}/{id}");
    }
    let digest = Sha256::digest(id.as_bytes());
    let hash: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{resource_type}/#{hash}")
}

/// Whether this call may use the memory at all: with no LLM there is nothing to
/// tell, and an offline read answers as it always did.
pub(super) fn applies(rt: &MockRuntime, plan: &CallPlan<'_>) -> bool {
    rt.consistency
        && plan.engine.has_provider()
        && matches!(plan.op.binding, Binding::Http { .. })
        && matches!(
            plan.op.kind,
            OperationKind::Query | OperationKind::Command(_)
        )
}

/// The requests remembered for the resource `op` targets. A name that may be
/// another owner's, with nothing held to say whose, has none: its requests would
/// be told to the wrong owner's generation.
pub(super) async fn recall(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target_id: Option<&str>,
    held: bool,
) -> Earlier {
    let rt = state.rt;
    let (true, Some(t), Some(id)) = (
        applies(rt, plan),
        plan.op.resource_type.as_deref(),
        target_id,
    ) else {
        return Earlier::default();
    };
    if !held && guarded(rt, plan.op, call).is_some() {
        return Earlier::default();
    }
    let key = memory_key(t, id, held);
    let memory = read_memory(state, &plan.session, &key).await;
    let referring = |r: &Value| r.get(REFERENCED) == Some(&Value::Bool(true));
    let all_referring = !memory.requests.is_empty() && memory.requests.iter().all(referring);
    // A read, or a direct update of the record (not an action, which is as it was), is
    // never told what only referred to the record where its answer replaces the stored
    // copy: those requests change it by effects alone, on a read told of nothing else.
    // So a read with others to catch up on is told those, a direct update is told only
    // those, and what only referred stays for the read after them.
    let op = plan.op;
    let direct = op.kind == OperationKind::Command(Effect::Update) && !op.is_action();
    let (shown, by_effects) = if direct || (op.kind == OperationKind::Query && !all_referring) {
        let ordinary = memory.requests.into_iter().filter(|r| !referring(r));
        (ordinary.collect::<Vec<_>>(), false)
    } else {
        (memory.requests, all_referring)
    };
    let told = shown
        .iter()
        .map(|request| {
            let mut told = request.clone();
            if let Value::Object(members) = &mut told {
                for untold in UNTOLD {
                    members.remove(untold);
                }
            }
            told
        })
        .collect();
    Earlier {
        key: Some(key),
        by_effects,
        shown,
        told,
        own: memory.own,
        attempts: memory.attempts,
        failed_now: false,
        stored_answer: None,
    }
}

/// Whether a write is remembered that stored `held` does not show yet, or one made to
/// the path of `call`, which a read of a member of `held` is behind.
pub(super) async fn is_behind(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: &StoredResource,
) -> bool {
    if !applies(state.rt, plan) {
        return false;
    }
    let key = memory_key(&held.resource_type, &held.id, true);
    !read_memory(state, &plan.session, &key)
        .await
        .requests
        .is_empty()
        || !recall_route(state, plan, call).await.is_empty()
}

/// One resource's memory, as kept.
#[derive(Default)]
struct Memory {
    requests: Vec<Value>,
    attempts: u64,
    own: Option<Value>,
}

async fn read_memory(state: &mut StateAccess<'_>, session: &SessionKey, key: &str) -> Memory {
    let Some(doc) = state.get(session, INTERNAL_TYPE, key).await else {
        return Memory::default();
    };
    let requests = doc.data.get("requests").and_then(Value::as_array).cloned();
    Memory {
        requests: requests.unwrap_or_default(),
        attempts: doc
            .data
            .get("attempts")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        own: doc.data.get("own").cloned(),
    }
}

async fn write_memory(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    key: &str,
    memory: &Memory,
) {
    let mut data = serde_json::json!({ "requests": memory.requests, "attempts": memory.attempts });
    if let Some(own) = &memory.own {
        data["own"] = own.clone();
    }
    state
        .upsert(
            session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_string(),
                id: key.to_string(),
                data,
            },
        )
        .await;
}

/// The schema of what `t`'s read by path answers, else its list items or
/// singleton read (see [`entity_fits`]).
pub(super) fn read_schema<'a>(rt: &'a MockRuntime, t: &str) -> Option<&'a Value> {
    let index = rt.index();
    // A record its read answers in an envelope or an array has its own schema.
    if let Some(record) = index.record_schemas.get(t) {
        return Some(record);
    }
    match index.detail_reads.get(t) {
        Some(i) => rt
            .bundle()
            .operations
            .get(*i)?
            .success_output()?
            .schema
            .as_ref(),
        None => index.other_shapes.get(t).map(|(_, schema)| schema),
    }
}

/// Whether `op`'s success answer is what its resource type's read by path
/// answers, so that it can replace what the session holds.
fn reads_the_resource(rt: &MockRuntime, op: &Operation) -> bool {
    is_read_schema(rt, op, op.success_output())
}

/// Whether an answer of `output` has the schema of `op`'s type's read by path.
fn is_read_schema(
    rt: &MockRuntime,
    op: &Operation,
    output: Option<&mb_mcm::OutputVariant>,
) -> bool {
    let Some(t) = op.resource_type.as_deref() else {
        return false;
    };
    let detail = rt.index().detail_reads.get(t).and_then(|i| {
        rt.bundle()
            .operations
            .get(*i)?
            .success_output()?
            .schema
            .as_ref()
    });
    let own = output.and_then(|o| o.schema.as_ref());
    if detail.is_some() && detail == own {
        return true;
    }
    // An answer holding the type's record, as its read holds it or otherwise
    // (Zendesk's `{ticket, audit}` beside `{ticket}`), holds the whole record.
    let (Some(record), Some(identity), Some(own)) =
        (rt.index().record_schemas.get(t), op.identity.as_ref(), own)
    else {
        return false;
    };
    record_schema_of(rt.bundle(), identity, own).as_ref() == Some(record)
}

impl Earlier {
    /// Whether this read must generate though the store could answer it.
    pub(super) fn forces(
        &self,
        rt: &MockRuntime,
        plan: &CallPlan<'_>,
        call: &OperationCall,
    ) -> bool {
        !self.shown.is_empty()
            && !self.capped()
            && matches!(plan.op.kind, OperationKind::Query)
            && plain(plan)
            && mb_engine::seed::request_seed(call).is_none()
            && reads_the_resource(rt, plan.op)
    }

    /// Whether `generated` may be repeated from the response cache: not a failed
    /// catch-up's, which is to try again, until it may not.
    pub(super) fn cacheable(&self, generated: &GeneratedResponse) -> bool {
        self.shown.is_empty()
            || self.capped()
            || !(self.failed_now || transient_fallback(generated))
    }

    /// Whether catching up has failed as often as it may.
    pub(super) fn capped(&self) -> bool {
        self.attempts >= MAX_ATTEMPTS
    }
}

/// Counts a read's failed catch-up in its memory, logging the last there will be.
pub(super) async fn failed_attempt(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    earlier: &mut Earlier,
) {
    if earlier.shown.is_empty()
        || !matches!(plan.op.kind, OperationKind::Query)
        || !plain(plan)
        || earlier.capped()
    {
        return;
    }
    let Some(key) = earlier.key.clone() else {
        return;
    };
    let mut memory = read_memory(state, &plan.session, &key).await;
    memory.attempts += 1;
    let attempts = memory.attempts;
    earlier.attempts = attempts;
    earlier.failed_now = true;
    write_memory(state, &plan.session, &key, &memory).await;
    if attempts >= MAX_ATTEMPTS {
        tracing::warn!(
            mock = %state.rt.name,
            operation = %plan.op.id,
            attempts,
            "gave up catching up with earlier requests until a later write"
        );
    }
}

/// Says in the request record that `answer` is behind requests it was not able
/// to catch up with, as an outcome with nothing set.
pub(super) fn note_not_caught_up(
    answer: &mut GeneratedResponse,
    plan: &CallPlan<'_>,
    held: Option<&StoredResource>,
    earlier: &Earlier,
) {
    let (false, Some(held)) = (earlier.shown.is_empty(), held) else {
        return;
    };
    if !earlier.capped() && earlier.attempts == 0 {
        return;
    }
    let reason = if earlier.capped() {
        format!(
            "behind {} earlier requests: gave up after {MAX_ATTEMPTS} attempts, until a later write",
            earlier.shown.len()
        )
    } else {
        format!(
            "behind {} earlier requests: the catch-up failed, and a later read tries again",
            earlier.shown.len()
        )
    };
    answer.effect_outcomes.push(EffectOutcome {
        effect: ResourceEffect {
            resource_type: plan
                .op
                .resource_type
                .clone()
                .unwrap_or_else(|| held.resource_type.clone()),
            id: held.id.clone(),
            set: Map::new(),
        },
        rejected: Some(reason),
        merged: None,
        applied: Vec::new(),
    });
}

/// What a write with no answer of its own acts on, when its route does not say
/// which field its parameter names: such an operation has no identity, as nothing
/// in an answer shows one. As a delete does, it goes by what the type's reads go by
/// (Jira's `PUT /issue/{issueIdOrKey}` with a key), or it would be taken for a new
/// resource under that key, which then answers reads in place of the real one.
pub(super) async fn silent_write_target(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target_id: Option<&str>,
) -> Option<StoredResource> {
    let rt = state.rt;
    let op = plan.op;
    if !applies(rt, plan)
        || !matches!(
            op.kind,
            OperationKind::Command(Effect::Update | Effect::Other)
        )
        || op.success_output().is_none_or(|o| o.schema.is_some())
    {
        return None;
    }
    let (t, id) = (op.resource_type.as_deref()?, target_id?);
    let param = target_param(op)?;
    if id_like(&param) {
        return None;
    }
    let mut owner = guarded(rt, op, call);
    if let Some(owner) = &mut owner {
        owner.writing = true;
    }
    let fields: Vec<&str> = rt
        .index()
        .path_fields(t)
        .iter()
        .map(String::as_str)
        .collect();
    by_path_fields(state, &plan.session, t, &fields, id, owner.as_ref()).await
}

/// Whether a write leaves the stored copy to [`after_write`]: an update answering
/// nothing, which merges what it can (the record its request holds, then what the
/// description says it sets; see [`update_without_answer`]). Storing the request in
/// the copy's place would lose what it did not repeat, identity included. Offline,
/// and with consistency off, the request still replaces it. An update the
/// description calls a replace does not: [`onto_record`] replaces the copy, keeping
/// its identity, and `after_write` leaves it so.
pub(super) fn keeps_stored(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    generated: &GeneratedResponse,
    held: bool,
) -> bool {
    held && generated.body.is_none()
        && applies(rt, plan)
        && plan.op.kind == OperationKind::Command(Effect::Update)
        && !transitions::replaces(rt, plan.op)
}

/// What of an update's answer `data` (of `status`) goes onto the record `t` it is
/// stored as. An answer with the read's schema is the record. One of another schema
/// (an `Ack {ok}`) was laid over the stored copy `held` (see [`mb_engine`]'s
/// reflection), and a member stays when the record's read declares it at the top
/// level or `held` already had it: what only the answer brought adds nothing to the
/// record. Where the read declares no members, allows more by `additionalProperties`,
/// or has a `oneOf` or `anyOf` that may hold others, it is as it was.
///
/// What the description says of the update's `request` comes after that:
/// - an update it calls a `replace` stores the record its request sends (see
///   [`records::request_record`]) with only the held identity and read-only members
///   kept, so a member it does not send is gone;
/// - a `request.record` pointer that resolves lays the members of the record it
///   points at onto the record, as far as the read declares them.
///
/// A result that breaks the type's schema keeps the merge as it was.
pub(super) fn onto_record(
    rt: &MockRuntime,
    op: &Operation,
    (t, held): (&str, Option<&Value>),
    (data, status): (Value, u16),
    request: Option<&Value>,
) -> Value {
    let merged = answered_onto_record(rt, op, (t, held), data, status);
    let Some(request) = request else {
        return merged;
    };
    let inner = records::request_record(rt, op, request);
    let replace = transitions::replaces(rt, op);
    if !replace && std::ptr::eq(inner, request) {
        return merged;
    }
    let bundle = rt.bundle();
    let schema = read_schema(rt, t);
    let pinned = pinned_members(rt, t);
    let result = if replace {
        let level = Level::of(bundle, schema);
        let stays = |key: &str| {
            pinned.contains(&key) || level.named(key).is_some_and(|s| read_only(bundle, Some(s)))
        };
        let mut kept = Map::new();
        for source in [Some(&merged), held].into_iter().flatten() {
            for (key, value) in source.as_object().into_iter().flatten() {
                if stays(key) {
                    kept.insert(key.clone(), value.clone());
                }
            }
        }
        merge_declared(bundle, schema, &Value::Object(kept), inner, &pinned).0
    } else {
        merge_declared(bundle, schema, &merged, inner, &pinned).0
    };
    if entity_fits(rt, t, &result) {
        result
    } else {
        tracing::info!(
            mock = %rt.name,
            operation = %op.id,
            "the record a described request leaves breaks its schema: merged as answered"
        );
        merged
    }
}

/// [`onto_record`]'s merge of the answer alone.
fn answered_onto_record(
    rt: &MockRuntime,
    op: &Operation,
    (t, held): (&str, Option<&Value>),
    data: Value,
    status: u16,
) -> Value {
    if is_read_schema(rt, op, op.output_for_status(status)) {
        return data;
    }
    let level = Level::of(rt.bundle(), read_schema(rt, t));
    let Value::Object(members) = data else {
        return data;
    };
    if level.union || level.properties.is_empty() || level.map.is_some() {
        return Value::Object(members);
    }
    let declared = |key: &str| level.properties.iter().any(|p| p.contains_key(key));
    let had = |key: &str| held.and_then(|h| h.get(key)).is_some();
    Value::Object(
        members
            .into_iter()
            .filter(|(k, _)| declared(k) || had(k))
            .collect(),
    )
}

/// `set`, an effect's members for a record of type `t`, with each top-level member the
/// read schema does not declare at its top level, but exactly one of its object members
/// does (`frozen` of a record keeping it under `attributes`), moved under that member:
/// the effect wrote it one object too high. What the effect already wrote under that
/// member wins over the moved one. An object member holding a record of its own
/// ([`is_a_record`]) is no home, and no member is moved onto one of the type's identity
/// fields (`info.name`): an effect repeating the record's key at the top level does not
/// thereby set its identity. Nor is a member moved whose value its place there does not
/// admit ([`fits_schema`]): a string `owner` is not the object `links.owner`, and moved
/// there would break the record, and with it the whole effect. A member no object member declares, or more than one
/// does, stays where it was written, and so does all of `set` where the read's top level
/// names no members, is a union, or is a map, any of which may hold it as written.
pub(super) fn rehomed_members(
    rt: &MockRuntime,
    t: &str,
    set: Map<String, Value>,
) -> Map<String, Value> {
    let bundle = rt.bundle();
    let top = Level::of(bundle, read_schema(rt, t));
    if top.union || top.map.is_some() || top.properties.is_empty() {
        return set;
    }
    let mut homes: Vec<(&str, Level<'_>)> = Vec::new();
    for (name, schema) in top.properties.iter().flat_map(|p| p.iter()) {
        if homes.iter().any(|(n, _)| *n == name.as_str()) {
            continue;
        }
        let level = Level::of(bundle, Some(schema));
        if !level.properties.is_empty() && !is_a_record(rt, schema, &level) {
            homes.push((name.as_str(), level));
        }
    }
    let at_top = |key: &str| top.properties.iter().any(|p| p.contains_key(key));
    let identity = rt.index().identity_fields(t);
    let mut out = Map::new();
    let mut moved = Vec::new();
    for (key, value) in set {
        if key.contains('.') || at_top(&key) {
            out.insert(key, value);
            continue;
        }
        let declaring: Vec<&(&str, Level<'_>)> = homes
            .iter()
            .filter(|(_, level)| level.properties.iter().any(|p| p.contains_key(&key)))
            .collect();
        match declaring.as_slice() {
            [(home, level)]
                if !level.union
                    && !identity.contains(&format!("{home}.{key}"))
                    && fits_schema(bundle, level.named(&key), &value) =>
            {
                moved.push(((*home).to_string(), key, value));
            }
            _ => {
                out.insert(key, value);
            }
        }
    }
    for (home, key, value) in moved {
        let slot = out.entry(home).or_insert_with(|| Value::Object(Map::new()));
        match slot {
            Value::Object(members) => {
                members.entry(key).or_insert(value);
            }
            // The effect gave the member another shape: the moved one stays as written.
            _ => {
                out.entry(key).or_insert(value);
            }
        }
    }
    out
}

/// Whether `value` is valid by `schema`, a member's place in a record's schema: a value
/// of another shape (a string where the place holds an object) was meant for something
/// else. A place with no schema to go by takes anything.
fn fits_schema(bundle: &mb_mcm::Bundle, schema: Option<&Value>, value: &Value) -> bool {
    schema.is_none_or(|schema| {
        mb_engine::SchemaValidator::new(&bundle.with_reachable_defs(schema))
            .is_ok_and(|v| v.is_valid(value))
    })
}

/// Whether an object member of `schema` (whose [`Level`] is `level`) holds a record of its
/// own rather than its record's members: it declares an `id` of its own (a related record
/// held inline, `customer: {id, name}`), or it is a type's record schema (`keeper:
/// Keeper`, read by `/keepers/{email}`).
fn is_a_record(rt: &MockRuntime, schema: &Value, level: &Level<'_>) -> bool {
    if level.named("id").is_some() {
        return true;
    }
    let bundle = rt.bundle();
    fn resolved<'a>(bundle: &'a mb_mcm::Bundle, schema: &'a Value) -> &'a Value {
        let mut at = schema;
        for _ in 0..8 {
            match def_name(at).and_then(|d| bundle.defs.get(d)) {
                Some(target) => at = target,
                None => break,
            }
        }
        at
    }
    let own = resolved(bundle, schema);
    let index = rt.index();
    index
        .detail_reads
        .keys()
        .chain(index.other_shapes.keys())
        .filter_map(|t| read_schema(rt, t))
        .any(|record| resolved(bundle, record) == own)
}

/// What one level of a schema says of its members, by its `$ref`s and `allOf`s. A
/// `oneOf` or `anyOf` branch says nothing for certain: another may be the one.
#[derive(Default)]
struct Level<'a> {
    /// Each `properties` object.
    properties: Vec<&'a Map<String, Value>>,
    /// An `additionalProperties` that allows more, and its schema if it has one.
    map: Option<Option<&'a Value>>,
    /// It has a `oneOf` or `anyOf`.
    union: bool,
}

impl<'a> Level<'a> {
    fn of(bundle: &'a mb_mcm::Bundle, schema: Option<&'a Value>) -> Self {
        fn visit<'a>(
            bundle: &'a mb_mcm::Bundle,
            schema: &'a Value,
            depth: usize,
            out: &mut Level<'a>,
        ) {
            if depth > 8 {
                return;
            }
            if let Some(target) = def_name(schema).and_then(|d| bundle.defs.get(d)) {
                visit(bundle, target, depth + 1, out);
                return;
            }
            out.properties
                .extend(schema.get("properties").and_then(Value::as_object));
            match schema.get("additionalProperties") {
                Some(Value::Bool(true)) => out.map = out.map.or(Some(None)),
                Some(map @ Value::Object(_)) => out.map = Some(Some(map)),
                _ => {}
            }
            out.union |= schema.get("oneOf").is_some() || schema.get("anyOf").is_some();
            for branch in schema
                .get("allOf")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                visit(bundle, branch, depth + 1, out);
            }
        }
        let mut out = Self::default();
        if let Some(schema) = schema {
            visit(bundle, schema, 0, &mut out);
        }
        out
    }

    /// The schema `properties` give member `key`, leaving maps aside.
    fn named(&self, key: &str) -> Option<&'a Value> {
        self.properties.iter().find_map(|p| p.get(key))
    }

    /// What it says of member `key`: `None` when it does not declare it, and an
    /// inner `None` when it allows it with no schema to go by.
    fn member(&self, key: &str) -> Option<Option<&'a Value>> {
        self.properties
            .iter()
            .find_map(|p| p.get(key))
            .map(Some)
            .or(self.map)
    }
}

/// Whether the contract says a client does not set this member.
fn read_only(bundle: &mb_mcm::Bundle, schema: Option<&Value>) -> bool {
    schema_parts(bundle, schema)
        .0
        .iter()
        .any(|p| p.get("readOnly").and_then(Value::as_bool) == Some(true))
}

/// `request` on `stored`, member by member at any depth, for the members `schema`
/// declares and a client may set; the merged value, and what is left of `request`.
/// `pinned` members are not changed, or told of. An object under a `oneOf` or
/// `anyOf` is not merged, as its members could mix with another branch's.
fn merge_declared(
    bundle: &mb_mcm::Bundle,
    schema: Option<&Value>,
    stored: &Value,
    request: &Value,
    pinned: &[&str],
) -> (Value, Option<Value>) {
    let (Value::Object(held), Value::Object(fields)) = (stored, request) else {
        return (stored.clone(), Some(request.clone()));
    };
    let level = Level::of(bundle, schema);
    let mut merged = held.clone();
    let mut rest = Map::new();
    for (key, value) in fields {
        if pinned.contains(&key.as_str()) && held.contains_key(key) {
            continue;
        }
        let Some(inner) = level.member(key) else {
            rest.insert(key.clone(), value.clone());
            continue;
        };
        if read_only(bundle, inner) {
            continue;
        }
        match (held.get(key), inner) {
            (Some(below), Some(_)) if below.is_object() && value.is_object() => {
                if Level::of(bundle, inner).union {
                    rest.insert(key.clone(), value.clone());
                    continue;
                }
                let (member, left) = merge_declared(bundle, inner, below, value, &[]);
                merged.insert(key.clone(), member);
                if let Some(left) = left {
                    rest.insert(key.clone(), left);
                }
            }
            _ => {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    let mut merged = Value::Object(merged);
    // A key held in an object member (`metadata.name`) is pinned where it sits.
    for field in pinned
        .iter()
        .filter(|f| f.contains('.') && !held.contains_key(**f))
    {
        if let Some(kept) = mb_mcm::member_at(stored, field) {
            mb_mcm::set_member_at(&mut merged, field, kept.clone());
        }
    }
    (merged, (!rest.is_empty()).then_some(Value::Object(rest)))
}

/// The part of `request` that `schema` declares and a client may set, by the same
/// rules as [`merge_declared`]: what a write says of the resource itself.
fn declared_part(
    bundle: &mb_mcm::Bundle,
    schema: Option<&Value>,
    request: &Value,
    pinned: &[&str],
) -> Option<Value> {
    let Value::Object(fields) = request else {
        return None;
    };
    let level = Level::of(bundle, schema);
    let mut kept = Map::new();
    for (key, value) in fields {
        if pinned.contains(&key.as_str()) {
            continue;
        }
        let Some(inner) = level.member(key) else {
            continue;
        };
        if read_only(bundle, inner) {
            continue;
        }
        if inner.is_some() && value.is_object() {
            if Level::of(bundle, inner).union {
                continue;
            }
            if let Some(part) = declared_part(bundle, inner, value, &[]) {
                kept.insert(key.clone(), part);
            }
        } else {
            kept.insert(key.clone(), value.clone());
        }
    }
    (!kept.is_empty()).then_some(Value::Object(kept))
}

/// What a merge or a body leaves to the resource's identity: the members the
/// contract identifies it and reads it by.
pub(super) fn pinned_members<'a>(rt: &'a MockRuntime, t: &str) -> Vec<&'a str> {
    let index = rt.index();
    ["id", "number"]
        .into_iter()
        .chain(index.identity_fields(t).iter().map(String::as_str))
        .chain(index.path_fields(t).iter().map(String::as_str))
        .collect()
}

/// What `t`'s `request` says of the resource itself, if that fits the caps. One that
/// does not is no own body at all: a part of it, or a mark of it, is not the resource.
fn own_part(rt: &MockRuntime, t: &str, request: &Value) -> Option<Value> {
    let pinned = pinned_members(rt, t);
    declared_part(rt.bundle(), read_schema(rt, t), request, &pinned)
        .filter(|own| own.to_string().len() <= MAX_BODY_BYTES)
}

/// A 204 update's `request` on stored `held`, as far as its read's schema declares
/// it and the result still validates (each top-level member on its own, when not
/// all together); the merged copy, and what could not be applied.
fn apply_update(
    rt: &MockRuntime,
    held: &StoredResource,
    request: &Value,
) -> (Value, Option<Value>) {
    let t = held.resource_type.as_str();
    let schema = read_schema(rt, t);
    let pinned = pinned_members(rt, t);
    let (merged, rest) = merge_declared(rt.bundle(), schema, &held.data, request, &pinned);
    if entity_fits(rt, t, &merged) {
        return (merged, rest);
    }
    let (Value::Object(fields), Value::Object(whole)) = (request, &merged) else {
        return (held.data.clone(), Some(request.clone()));
    };
    let mut kept = held.data.clone();
    let mut left: Map<String, Value> = rest
        .and_then(|r| r.as_object().cloned())
        .unwrap_or_default();
    for key in fields.keys() {
        let Some(member) = whole.get(key) else {
            continue;
        };
        if held.data.get(key) == Some(member) {
            continue;
        }
        let Value::Object(members) = &mut kept else {
            break;
        };
        let previous = members.insert(key.clone(), member.clone());
        if entity_fits(rt, t, &kept) {
            continue;
        }
        if let Value::Object(members) = &mut kept {
            match previous {
                Some(previous) => {
                    members.insert(key.clone(), previous);
                }
                None => {
                    members.remove(key);
                }
            }
        }
        if let Some(value) = fields.get(key) {
            left.insert(key.clone(), value.clone());
        }
    }
    (kept, (!left.is_empty()).then_some(Value::Object(left)))
}

/// Whether `name` is one of the mock's own credential names.
fn is_own_credential(rt: &MockRuntime, name: &str) -> bool {
    let own = rt.credential_names();
    own.headers.contains(&name.to_ascii_lowercase()) || own.query.contains(name)
}

/// Whether a member named `name` holds a credential, when a contract does not say.
pub(super) fn is_credential_name(rt: &MockRuntime, name: &str) -> bool {
    is_sensitive(name) || is_own_credential(rt, name)
}

/// Whether `value` under `name` is a count, not a credential: `max_tokens: 100`.
fn is_count(name: &str, value: &Value) -> bool {
    let mut normalized = String::with_capacity(name.len());
    mb_mcm::credentials::normalize_into(name, &mut normalized);
    value.is_number() && mb_mcm::credentials::is_count_name(&normalized)
}

/// Whether `schema` says its value is not to be shown again (see
/// [`mb_mcm::credentials::schema_marks_secret`], which a prompt's headers go by too).
fn schema_hides(bundle: &mb_mcm::Bundle, schema: Option<&Value>) -> bool {
    schema.is_some_and(|s| mb_mcm::credentials::schema_marks_secret(bundle, s))
}

/// `value` with every credential replaced, at any depth. The contract speaks first:
/// what either schema marks `writeOnly` or `format: password`, and the mock's own
/// credential names. With `read`, for a record's own body, a member that schema declares
/// otherwise is one the API returns, so only a credential word in its name hides it
/// (see [`mb_mcm::credentials::hides_a_returned_member`]). Otherwise, as the last
/// resort, a name holding one does, or a `{"name": "Authorization", "value": ...}` pair.
pub(super) fn redact_credentials(
    rt: &MockRuntime,
    value: &mut Value,
    schema: Option<&Value>,
    read: Option<&Value>,
) {
    redact_by_names(rt, value, schema, read, false);
}

/// Whether `members` are a pair whose label says its `value` is a credential, by any of
/// the members a label goes under: `{"key": "X-Api-Key", "value": ...}`.
fn labels_a_credential(rt: &MockRuntime, members: &Map<String, Value>) -> bool {
    members.contains_key("value")
        && ["name", "key", "header"]
            .iter()
            .filter_map(|n| members.get(*n)?.as_str())
            .any(|label| {
                is_own_credential(rt, label) || mb_mcm::credentials::hides_in_context(label)
            })
}

/// [`redact_credentials`], with `context` by the wider names of what a model is shown
/// beyond its own request (see [`for_context`]), where `key` and `sid` stay on what the
/// contract says they identify.
fn redact_by_names(
    rt: &MockRuntime,
    value: &mut Value,
    schema: Option<&Value>,
    read: Option<&Value>,
    context: bool,
) {
    let bundle = rt.bundle();
    let any_name = |name: &str| {
        if context {
            is_own_credential(rt, name) || mb_mcm::credentials::hides_in_context(name)
        } else {
            is_credential_name(rt, name)
        }
    };
    match value {
        Value::Object(members) => {
            let named_pair = if context {
                labels_a_credential(rt, members)
            } else {
                ["name", "key", "header"]
                    .iter()
                    .find_map(|n| members.get(*n)?.as_str())
                    .is_some_and(any_name)
            };
            if named_pair && let Some(paired) = members.get_mut("value") {
                *paired = Value::from(REDACTED);
            }
            let (parts, _) = schema_parts(bundle, schema);
            let returned = Level::of(bundle, read);
            let (_, read_defs) = schema_parts(bundle, read);
            // What the read's own type is identified by is that, whatever it is called.
            let identifies = |key: &str| {
                read_defs
                    .iter()
                    .filter_map(|def| rt.index().types_by_def.get(*def))
                    .any(|t| pinned_members(rt, t).contains(&key) && identity_stays(t, key))
            };
            for (key, member) in members.iter_mut() {
                let inner = parts.iter().find_map(|p| p.get("properties")?.get(key));
                let read_inner = returned.named(key);
                let by_name = !is_count(key, member)
                    && match read_inner {
                        Some(_) if context => {
                            mb_mcm::credentials::hides_returned_in_context(key) && !identifies(key)
                        }
                        Some(_) => mb_mcm::credentials::hides_a_returned_member(key),
                        None => any_name(key),
                    };
                if schema_hides(bundle, inner)
                    || schema_hides(bundle, read_inner)
                    || is_own_credential(rt, key)
                    || by_name
                {
                    *member = Value::from(REDACTED);
                } else {
                    redact_by_names(rt, member, inner, read_inner, context);
                }
            }
        }
        Value::Array(items) => {
            let inner = schema_parts(bundle, schema)
                .0
                .iter()
                .find_map(|p| p.get("items"));
            let read_inner = schema_parts(bundle, read)
                .0
                .iter()
                .find_map(|p| p.get("items"));
            for item in items {
                redact_by_names(rt, item, inner, read_inner, context);
            }
        }
        _ => {}
    }
}

/// A request body as it may be kept: credentials redacted, then the request
/// log's own redaction, then the PII the engine scrubs from answers and effects.
/// Only a record's `own` body has its read schema to go by. One that is only told, an
/// action's or a child's to its parent, gets every rule by name.
fn scrubbed(rt: &MockRuntime, plan: &CallPlan<'_>, body: &Value, own: bool) -> Value {
    let read = plan
        .op
        .resource_type
        .as_deref()
        .filter(|_| own)
        .and_then(|t| read_schema(rt, t));
    let mut body = body.clone();
    redact_credentials(rt, &mut body, plan.op.input.body.as_ref(), read);
    if let Some(recorder) = &rt.recorder {
        recorder.redact(&mut body);
    }
    plan.engine.scrub(&mut body);
    body
}

/// Whether identity member `field` of type `t` is shown though its name could hold a
/// credential: `key` and `sid`, and not on a type that is itself one, as what names an
/// `api_keys` record is the secret. Any other name holding a credential is not.
fn identity_stays(t: &str, field: &str) -> bool {
    let hidden = mb_mcm::credentials::hides_in_context;
    !hidden(field) || (mb_mcm::credentials::may_be_an_identity(field) && !hidden(t))
}

/// `value` as a model may be shown it beyond its own request, in `<earlier_answers>`,
/// `<earlier_writes>` and a record matched by value, of type `t`: by `schema`'s and
/// `read`'s marks, the wider names and a value's own shape. What is hidden is left out,
/// as a marker would only be repeated. What identifies the record stays as it is (see
/// [`identity_stays`]), or a model could not name it. `None` when nothing is left.
pub(super) fn for_context(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    t: &str,
    value: &Value,
    schema: Option<&Value>,
    read: Option<&Value>,
) -> Option<Value> {
    let mut kept = value.clone();
    redact_by_names(rt, &mut kept, schema, read, true);
    redact_shapes(&mut kept);
    if let Some(recorder) = &rt.recorder {
        recorder.redact(&mut kept);
    }
    plan.engine.scrub(&mut kept);
    if kept.is_object() && value.is_object() {
        for field in pinned_members(rt, t) {
            let shaped = |v: &Value| {
                v.as_str()
                    .is_some_and(mb_mcm::credentials::is_credential_value)
            };
            if let Some(identity) =
                mb_mcm::member_at(value, field).filter(|v| identity_stays(t, field) && !shaped(v))
            {
                mb_mcm::set_member_at(&mut kept, field, identity.clone());
            }
        }
    }
    mb_engine::without_redacted(&kept)
}

/// [`for_context`] of a record matched by value: by its type's read.
pub(super) fn matched_for_context(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    t: &str,
    data: &Value,
) -> Option<Value> {
    for_context(rt, plan, t, data, None, read_schema(rt, t))
}

/// What the contract declares of `value`, by the list's `items` or the write's `request`
/// schema: members either names, not marked as hidden by either and not named as a
/// credential, unless it is the type's identity (see [`identity_stays`]). A map's other
/// members may hold anything, so they are not told.
pub(super) fn declared_plainly(
    rt: &MockRuntime,
    t: &str,
    (items, request): (Option<&Value>, Option<&Value>),
    value: &Value,
    depth: usize,
) -> Option<Value> {
    let bundle = rt.bundle();
    let (items_parts, _) = schema_parts(bundle, items);
    let (request_parts, _) = schema_parts(bundle, request);
    match value {
        _ if depth > 8 => None,
        // Before a member goes: losing only its label would leave the secret.
        Value::Object(members) if labels_a_credential(rt, members) => None,
        Value::Object(members) => {
            let identity = pinned_members(rt, t);
            let member_of = |parts: &[&'_ Value], key: &str| {
                parts
                    .iter()
                    .find_map(|p| p.get("properties")?.get(key).cloned())
            };
            let kept: Map<String, Value> = members
                .iter()
                .filter_map(|(key, member)| {
                    let in_items = member_of(&items_parts, key);
                    let in_request = member_of(&request_parts, key);
                    if in_items.is_none() && in_request.is_none() {
                        return None;
                    }
                    let own =
                        depth == 0 && identity.contains(&key.as_str()) && identity_stays(t, key);
                    let named = is_own_credential(rt, key)
                        || if in_items.is_some() {
                            mb_mcm::credentials::hides_returned_in_context(key)
                        } else {
                            mb_mcm::credentials::hides_in_context(key)
                        };
                    if schema_hides(bundle, in_items.as_ref())
                        || schema_hides(bundle, in_request.as_ref())
                        || (named && !own)
                    {
                        return None;
                    }
                    let inner = (in_items.as_ref(), in_request.as_ref());
                    let member = declared_plainly(rt, t, inner, member, depth + 1)?;
                    Some((key.clone(), member))
                })
                .collect();
            (!kept.is_empty()).then_some(Value::Object(kept))
        }
        Value::Array(list) => {
            let inner_of = |parts: &[&'_ Value]| parts.iter().find_map(|p| p.get("items").cloned());
            let (in_items, in_request) = (inner_of(&items_parts), inner_of(&request_parts));
            if in_items.is_none() && in_request.is_none() {
                return None;
            }
            let inner = (in_items.as_ref(), in_request.as_ref());
            let kept: Vec<Value> = list
                .iter()
                .filter_map(|item| declared_plainly(rt, t, inner, item, depth + 1))
                .collect();
            (!kept.is_empty()).then_some(Value::Array(kept))
        }
        scalar => Some(scalar.clone()),
    }
}

/// What a list read is told of a write's `body`, of the `request` schema: an object as
/// one of its items, or a list of those or of what the request's schema declares, by
/// [`declared_plainly`]. Nothing else, however it reads.
fn told_to_list(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    list: &WrittenList,
    body: &Value,
    request: Option<&Value>,
) -> Option<Value> {
    let t = list.resource_type.as_str();
    let items = Some(&list.items);
    let told = |one: &Value, schema: Option<&Value>| for_context(rt, plan, t, one, schema, items);
    match body {
        Value::Array(list) => {
            let (parts, _) = schema_parts(rt.bundle(), request);
            let each = parts.iter().find_map(|p| p.get("items"));
            let kept: Vec<Value> = list
                .iter()
                .filter(|item| item.is_object() || each.is_some())
                .filter_map(|item| declared_plainly(rt, t, (items, each), item, 0))
                .filter_map(|item| told(&item, each))
                .collect();
            (!kept.is_empty()).then_some(Value::Array(kept))
        }
        Value::Object(_) => {
            declared_plainly(rt, t, (items, request), body, 0).and_then(|one| told(&one, request))
        }
        _ => None,
    }
}

/// `value` with every string shaped like a credential replaced, at any depth, as no name
/// has to give those away (see [`mb_mcm::credentials::is_credential_value`]). For
/// [`for_context`].
fn redact_shapes(value: &mut Value) {
    match value {
        Value::String(text) if mb_mcm::credentials::is_credential_value(text) => {
            *value = Value::from(REDACTED);
        }
        Value::Array(items) => items.iter_mut().for_each(redact_shapes),
        Value::Object(members) => members.values_mut().for_each(redact_shapes),
        _ => {}
    }
}

/// `body` within [`MAX_BODY_BYTES`]: as it is, or cut to a string that says so.
fn within_cap(body: Value) -> Value {
    let text = body.to_string();
    if text.len() <= MAX_BODY_BYTES {
        return body;
    }
    if text.len() > MAX_PREVIEWED_BYTES {
        return serde_json::json!({"truncated": true, "bytes": text.len()});
    }
    let mut end = MAX_BODY_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Value::String(format!("{}[truncated]", &text[..end]))
}

/// Whether `stored` shows what `request` said: the same value at the same path. A
/// value elsewhere is not it, and what was redacted cannot be compared, so it is
/// never shown.
fn carries(request: &Value, stored: &Value) -> bool {
    match request {
        Value::Object(members) => members
            .iter()
            .all(|(key, inner)| stored.get(key).is_some_and(|held| carries(inner, held))),
        other => other == stored,
    }
}

/// Whether all `value` holds is what was redacted: a catch-up could show none of it.
fn only_redacted(value: &Value) -> bool {
    match value {
        Value::String(text) => text == REDACTED,
        Value::Object(members) => members.values().all(only_redacted),
        Value::Array(items) => items.iter().all(only_redacted),
        _ => false,
    }
}

/// The id of what a create stored: the answer's (or a body-less answer's
/// request's) identity field, else that of the member wrapping its own type.
fn created_id(op: &Operation, stored: &Value) -> Option<String> {
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    if let Some(id) = mb_mcm::member_at(stored, field).and_then(id_string) {
        return Some(id);
    }
    let t = op.resource_type.as_deref()?;
    let members = stored.as_object()?;
    let wrapped = members
        .iter()
        .find(|(member, value)| {
            value.is_object() && (member.as_str() == t || format!("{member}s") == t)
        })?
        .1;
    WRAPPED_IDS
        .iter()
        .find_map(|f| wrapped.get(*f).and_then(id_string))
}

/// The nearest resource before what `op` targets that its path names and the
/// session holds, other than the target: the capture of
/// `/captures/{id}/refunds`, by the parameter's name or the collection before it.
/// A name must be the path's owner's to count (see [`key_fits`]).
async fn parent_of(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    call: &OperationCall,
    own: Option<(&str, &str)>,
) -> Option<StoredResource> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let rt = state.rt;
    let segments = path_segments(path);
    let target = match op.kind {
        OperationKind::Command(Effect::Create) => None,
        _ => target_param(op),
    };
    for (at, segment) in segments.iter().enumerate().rev() {
        let Some(param) = segment_param(segment).filter(|p| target.as_deref() != Some(*p)) else {
            continue;
        };
        let Some(value) = call.path_params.get(param).and_then(id_string) else {
            continue;
        };
        let collection = at
            .checked_sub(1)
            .map(|before| segments[before])
            .filter(|s| segment_param(s).is_none());
        let names = [
            reference_prefix(param),
            Some(param).filter(|p| !mb_mcm::is_generic_id(p)),
            collection.and_then(|c| c.strip_suffix('s')),
            collection,
        ];
        let owner = owner_of_param(op, call, param).filter(|_| !id_like(param));
        for name in names.into_iter().flatten() {
            let Some(t) = entity_type(rt.index(), None, name) else {
                continue;
            };
            let direct = state
                .get(session, &t, &value)
                .await
                .filter(|hit| owner.as_ref().is_none_or(|o| key_fits(rt, session, hit, o)));
            let found = match direct {
                Some(hit) => Some(hit),
                None if id_like(param) => None,
                None => {
                    let mut fields = name_fields(param);
                    fields.extend(rt.index().path_fields(&t).iter().map(String::as_str));
                    by_path_fields(state, session, &t, &fields, &value, owner.as_ref()).await
                }
            };
            if let Some(hit) = found
                && own != Some((t.as_str(), hit.id.as_str()))
            {
                return Some(hit);
            }
        }
    }
    None
}

/// Adds `entry` to a memory, within the caps, with what it said of the resource
/// itself in place of any older own body; `retry`: the write is a client's retry. A new request is worth trying for, so the
/// count of failed catch-ups starts over.
async fn remember(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    key: &str,
    entry: &Value,
    own: Option<Value>,
    retry: bool,
) {
    let mut memory = read_memory(state, session, key).await;
    // A client's retry of the newest request (see [`is_retry`]) is one request, kept as
    // the later, so that retries push no other out. A deliberate repeat is two.
    if retry
        && memory
            .requests
            .last()
            .is_some_and(|newest| same_request(newest, entry))
    {
        memory.requests.pop();
    }
    memory.requests.push(entry.clone());
    let excess = memory.requests.len().saturating_sub(MAX_REMEMBERED);
    memory.requests.drain(..excess);
    memory.attempts = 0;
    memory.own = own;
    let size = |requests: &[Value], own: Option<&Value>| {
        serde_json::json!({ "requests": requests, "own": own })
            .to_string()
            .len()
    };
    while memory.requests.len() > 1 && size(&memory.requests, memory.own.as_ref()) > MAX_BYTES {
        memory.requests.remove(0);
    }
    write_memory(state, session, key, &memory).await;
}

/// Whether two kept requests are the same but for when they were made.
fn same_request(one: &Value, other: &Value) -> bool {
    let timeless = |v: &Value| {
        let mut v = v.clone();
        if let Value::Object(members) = &mut v {
            members.remove("at");
        }
        v
    };
    timeless(one) == timeless(other)
}

/// Ends what an older own body may overlay: a later write has the last word.
async fn clear_own(state: &mut StateAccess<'_>, session: &SessionKey, key: &str) {
    let mut memory = read_memory(state, session, key).await;
    if memory.own.take().is_some() {
        write_memory(state, session, key, &memory).await;
    }
}

/// Makes `own` what a memory's catch-up keeps, when there is still one to come.
async fn set_own(state: &mut StateAccess<'_>, session: &SessionKey, key: &str, own: Option<Value>) {
    let mut memory = read_memory(state, session, key).await;
    if !memory.requests.is_empty() && memory.own != own {
        memory.own = own;
        write_memory(state, session, key, &memory).await;
    }
}

/// Takes from a memory the requests `shown`, and nothing a write added since: this
/// answer was not told of those, whose own body, if any, stands.
async fn forget_shown(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    key: &str,
    shown: &[Value],
) {
    let mut memory = read_memory(state, session, key).await;
    memory.requests.retain(|r| !shown.contains(r));
    if memory.requests.is_empty() {
        state.delete(session, INTERNAL_TYPE, key).await;
    } else {
        memory.attempts = 0;
        write_memory(state, session, key, &memory).await;
    }
}

/// Whether an answer's effect changed that resource.
fn effect_applied(generated: &GeneratedResponse, resource_type: &str, id: &str) -> bool {
    generated.effect_outcomes.iter().any(|o| {
        o.rejected.is_none() && o.effect.resource_type == resource_type && o.effect.id == id
    })
}

/// What to remember for a write's target.
struct Tell {
    id: String,
    /// What a prompt is shown of the request.
    body: Option<Value>,
    /// What the write said of the resource itself (see [`Earlier::own`]).
    own: Option<Value>,
    /// Whether `id` is a stored resource's.
    stored: bool,
    /// Whether what is stored is unchanged, so a change is still to be published.
    publish: bool,
}

/// After a 2xx write: applies what a 204 update can, and remembers its request for
/// the resource it targets, unless that resource already shows it:
///
/// - a create, when its answer is shorter than its read and does not hold what
///   the request said (see [`carries`]). One answering the read's own schema was
///   written with the request in front of it, as a regeneration would be, and
///   what that schema has no member for no read could show;
/// - an update answering its read's schema replaced the target and was told what
///   was remembered, which is now done. A shorter answer, or a faker's standing in
///   for the LLM, is taken as a create's. One answering nothing merges the members
///   the read declares and is remembered for the rest (see [`apply_update`]);
/// - any other action, unless an effect of its answer changed the target.
///
/// A write with a response body and no request body said what it did and asked
/// nothing that could be lost, and the LLM shown its target listed no change, so
/// it is not remembered for the target.
///
/// The parent is told of a create, even with no body, the path being all there is
/// (`POST /captures/{id}/refund`), of a delete, naming the stored record it removed
/// (what counts or lists its children no longer holds it), and of an action answering
/// nothing: an answer about a child says nothing to it. An update of a child does not
/// tell it, or every nested write would cost it a regeneration. Telling it changes
/// nothing it holds, so it publishes nothing: its read looks in the shared store
/// before it serves, on any node.
///
/// Only an update's or a create's body is the resource's own, for a catch-up to
/// keep, and only the latest write's: last write wins. A child's or an action's is
/// told, and is about something else, and like an applied effect it ends what an
/// older body may overlay. A fully applied update, with memory left, puts its own body
/// in that place, or a model obeying what is told could go back on it.
pub(super) async fn after_write(
    state: &mut StateAccess<'_>,
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    w: &Written<'_>,
) {
    let op = plan.op;
    let session = &plan.session;
    let OperationKind::Command(effect) = op.kind else {
        return;
    };
    let (true, Some(t)) = (applies(rt, plan), op.resource_type.as_deref()) else {
        return;
    };
    // Refused for another's key, or a new record with nowhere to go.
    if w.foreign_key || w.unstored || !(200..300).contains(&w.generated.status) {
        return;
    }
    let generated = w.generated;
    let raw = w
        .call
        .body
        .as_ref()
        .filter(|b| !b.is_null() && b.as_object().is_none_or(|o| !o.is_empty()));
    let request = raw.map(|b| scrubbed(rt, plan, b, true));
    let told = raw.map(|b| scrubbed(rt, plan, b, false));
    let answered = generated.body.is_some() && !transient_fallback(generated);
    let telling = request.is_some() || generated.body.is_none();
    // A name that may be another's, with nothing held, is nobody's to remember.
    let acted_on = w
        .acted_on
        .filter(|_| w.held.is_some() || guarded(rt, op, w.call).is_none());
    let whole = answered && is_read_schema(rt, op, op.output_for_status(generated.status));
    // The answer holds what the request said, as what the write stored.
    let carried = |stored: Option<&Value>| match (&request, stored) {
        (Some(request), Some(stored)) => {
            (answered || generated.body.is_none()) && carries(request, stored)
        }
        _ => false,
    };
    let own_body = || request.as_ref().and_then(|r| own_part(rt, t, r));
    let mut target: Option<Tell> = None;
    match effect {
        Effect::Create => {
            // A body-less answer stores the request, which then holds itself.
            let stored = generated.body.as_ref().or(request.as_ref());
            if telling && !whole && !carried(stored) {
                target = stored
                    .and_then(|stored| created_id(op, stored))
                    .map(|id| Tell {
                        id,
                        body: request.clone(),
                        own: own_body(),
                        stored: true,
                        publish: false,
                    });
            }
        }
        Effect::Update if generated.new_record => {}
        Effect::Update if whole => {
            if let Some(key) = w
                .earlier
                .key
                .as_deref()
                .filter(|_| !w.earlier.shown.is_empty())
            {
                forget_shown(state, session, key, &w.earlier.shown).await;
                clear_own(state, session, key).await;
            }
        }
        Effect::Update if generated.body.is_some() => {
            if telling
                && !carried(generated.body.as_ref())
                && let Some(id) = acted_on
            {
                target = Some(Tell {
                    id: id.to_string(),
                    body: request.clone(),
                    own: own_body(),
                    stored: true,
                    publish: false,
                });
            }
        }
        Effect::Update => {
            target =
                update_without_answer(state, rt, plan, w, raw, request.as_ref(), acted_on).await;
        }
        Effect::Delete => {
            if let Some(id) = acted_on {
                let key = memory_key(t, id, w.held.is_some());
                state.delete(session, INTERNAL_TYPE, &key).await;
            }
        }
        Effect::Other => {
            if telling
                && let Some(id) = acted_on
                && !effect_applied(generated, t, id)
            {
                target = Some(Tell {
                    id: id.to_string(),
                    body: told.clone(),
                    own: None,
                    stored: w.held.is_some(),
                    publish: true,
                });
            }
        }
    }
    // Last write wins: what an older body said of what this write touched is over.
    for outcome in generated
        .effect_outcomes
        .iter()
        .filter(|o| o.rejected.is_none())
    {
        let key = memory_key(&outcome.effect.resource_type, &outcome.effect.id, true);
        clear_own(state, session, &key).await;
    }
    if target.is_none()
        && let Some(key) = w.earlier.key.as_deref()
    {
        match effect {
            // Handled with its merge.
            Effect::Update if generated.body.is_none() => {}
            // A short answer holding what it was asked is applied in full.
            Effect::Update if !generated.new_record && !whole && generated.body.is_some() => {
                set_own(state, session, key, own_body()).await;
            }
            _ => clear_own(state, session, key).await,
        }
    }
    // A delete of what the session never held has nothing to name, and its parent's
    // stored copy stands.
    let tells_parent = effect == Effect::Create
        || (effect == Effect::Delete && w.held.is_some())
        || (effect == Effect::Other && generated.body.is_none());
    let parent = if tells_parent {
        let own = match effect {
            Effect::Create => target.as_ref().map(|tell| tell.id.as_str()),
            _ => w.acted_on,
        };
        parent_of(state, session, op, w.call, own.map(|id| (t, id)))
            .await
            .filter(|p| !effect_applied(generated, &p.resource_type, &p.id))
    } else {
        None
    };
    // The record a create stored, which what it was made under or refers to may count.
    let created = match effect {
        Effect::Create => generated
            .body
            .as_ref()
            .or(request.as_ref())
            .and_then(|stored| created_id(op, stored)),
        _ => None,
    };
    let referenced = match created.as_deref() {
        Some(id) => {
            let told_already = |kind: &str, other: &str| {
                (kind == t && other == id)
                    || parent
                        .as_ref()
                        .is_some_and(|p| p.resource_type == kind && p.id == other)
            };
            world::referenced_by_value(state, plan, w.call, told_already)
                .await
                .into_iter()
                .filter(|r| !effect_applied(generated, &r.resource_type, &r.id))
                .collect()
        }
        None => Vec::new(),
    };
    if target.is_none() && parent.is_none() && referenced.is_empty() {
        return;
    }
    let created = created.as_deref().map(|id| (t, id));
    if let Some(tell) = target {
        let key = memory_key(t, &tell.id, tell.stored);
        remember(
            state,
            session,
            &key,
            &memory_entry(op, tell.body, None),
            tell.own,
            w.retry,
        )
        .await;
        if tell.publish
            && let Some(held) = w.held
        {
            // A described transition or change stored the record anew: that copy is
            // the one published, not the one held before.
            let current = match transitions::sets_members(rt, op) {
                true => state.get(session, t, &held.id).await,
                false => None,
            };
            state
                .upsert(session, current.unwrap_or_else(|| held.clone()))
                .await;
        }
    }
    // Told as a child is to what it is made under: last write wins. A delete names the
    // stored record it removed, which its path's template does not.
    let mut entry = memory_entry(op, told, created);
    if effect == Effect::Delete
        && let (Some(held), Value::Object(members)) = (w.held, &mut entry)
    {
        members.insert(
            DELETED.to_string(),
            serde_json::json!({"type": held.resource_type, "id": held.id}),
        );
    }
    if let Some(parent) = &parent {
        let key = memory_key(&parent.resource_type, &parent.id, true);
        remember(state, session, &key, &entry, None, w.retry).await;
    }
    // What it only refers to is told so, and changes by effects alone (see
    // [`Earlier::by_effects`]): what an older body says of it still stands.
    if let Value::Object(members) = &mut entry {
        members.insert(REFERENCED.to_string(), Value::Bool(true));
    }
    for related in &referenced {
        let key = memory_key(&related.resource_type, &related.id, true);
        let own = read_memory(state, session, &key).await.own;
        remember(state, session, &key, &entry, own, w.retry).await;
    }
}

/// What a kept request says beside what a prompt is told of it (see [`recall`]): when it
/// was made, and the record it created, if any.
const UNTOLD: [&str; 3] = ["at", CREATED, REFERENCED];

/// The member of a kept request saying it only referred to the record it is kept for.
const REFERENCED: &str = "referenced";

/// The member of a kept request naming the record it created, as `{type, id}`.
const CREATED: &str = "created";

/// The member of a kept delete naming the stored record it removed, as `{type, id}`:
/// told, as what the parent it is kept for no longer holds.
const DELETED: &str = "deleted";

/// One remembered request, as a prompt is told it but for what is [`UNTOLD`]: with
/// `created`, the record it created, which the read it is told to is shown as new.
fn memory_entry(op: &Operation, body: Option<Value>, created: Option<(&str, &str)>) -> Value {
    let mut entry = Map::new();
    entry.insert("operation".to_string(), Value::from(op.id.as_str()));
    entry.insert("path".to_string(), Value::from(route(op)));
    if let Some(body) = body {
        entry.insert("body".to_string(), within_cap(body));
    }
    if let Some((t, id)) = created {
        entry.insert(
            CREATED.to_string(),
            serde_json::json!({"type": t, "id": id}),
        );
    }
    entry.insert(
        "at".to_string(),
        Value::from(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
    );
    Value::Object(entry)
}

/// Where the writes to the path `call` was made to are kept, when its route has a list
/// read they could reach: by a hash, as a path's values may be the secret.
fn route_memory_key(rt: &MockRuntime, op: &Operation, call: &OperationCall) -> Option<String> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let index = rt.index();
    if !index.written_lists.contains_key(path) && !index.told_lists.contains(path) {
        return None;
    }
    let filled = fill_path(call, &path_segments(path))?;
    Some(memory_key("#route", &filled.path, false))
}

/// What a list read is told of the writes to its own path, oldest first.
pub(super) async fn recall_route(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Vec<Value> {
    let rt = state.rt;
    if !applies(rt, plan) || !matches!(plan.op.kind, OperationKind::Query) {
        return Vec::new();
    }
    let Some(key) = route_memory_key(rt, plan.op, call) else {
        return Vec::new();
    };
    let mut requests = read_memory(state, &plan.session, &key).await.requests;
    for request in &mut requests {
        if let Value::Object(members) = request {
            members.remove("at");
        }
    }
    requests
}

/// The records the requests a read is told of created (see [`memory_entry`]), oldest
/// first, as a record matched by value is shown ([`matched_for_context`]): what was made
/// under or refers to the target since it was stored, so that what counts them moves.
/// Taken out of `related`, where they would be told to stay unchanged. One gone since is
/// not shown. A read pays at most one store read per request it is told of, and none with
/// no record created.
pub(super) async fn created_since(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    earlier: &Earlier,
    target: Option<&StoredResource>,
    related: &mut Vec<StoredResource>,
) -> Vec<StoredResource> {
    let rt = state.rt;
    let mut out: Vec<StoredResource> = Vec::new();
    for entry in &earlier.shown {
        let Some(created) = entry.get(CREATED) else {
            continue;
        };
        let (Some(t), Some(id)) = (
            created.get("type").and_then(Value::as_str),
            created.get("id").and_then(Value::as_str),
        ) else {
            continue;
        };
        let is = |r: &StoredResource| r.resource_type == t && r.id == id;
        if target.is_some_and(is) || out.iter().any(is) {
            continue;
        }
        if let Some(at) = related.iter().position(is) {
            out.push(related.remove(at));
            continue;
        }
        let Some(held) = state.get(&plan.session, t, id).await else {
            continue;
        };
        if let Some(data) = matched_for_context(rt, plan, t, &held.data) {
            out.push(StoredResource { data, ..held });
        }
    }
    out
}

/// Whether two path templates' segments are the same route: parameters may be named
/// differently, as the same position is the same value.
pub(super) fn same_route(one: &[&str], other: &[&str]) -> bool {
    one.len() == other.len()
        && one
            .iter()
            .zip(other)
            .all(|(a, b)| a == b || (segment_param(a).is_some() && segment_param(b).is_some()))
}

/// Whether a kept body says `value`, as a member or an item.
fn mentions(body: &Value, value: &str) -> bool {
    match body {
        Value::Object(members) => members.values().any(|m| mentions(m, value)),
        Value::Array(items) => items.iter().any(|m| mentions(m, value)),
        other => id_string(other).as_deref() == Some(value),
    }
}

/// After a 2xx delete: what is kept for a list under what it deleted goes, for routes
/// whose path below it has no other parameter to know (the memory is keyed by a hash of
/// the whole path). Of a list it was a child of, the writes naming that child go. What
/// this misses is told as what may have changed.
async fn forget_deleted_route(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) {
    let rt = state.rt;
    let Binding::Http { path, .. } = &plan.op.binding else {
        return;
    };
    let segments = path_segments(path);
    let Some(filled) = fill_path(call, &segments).map(|f| f.path) else {
        return;
    };
    let index = rt.index();
    for list in index.written_lists.keys().chain(index.told_lists.iter()) {
        let below = path_segments(list);
        if below.len() > segments.len() && same_route(&segments, &below[..segments.len()]) {
            let rest = &below[segments.len()..];
            if rest.iter().all(|s| segment_param(s).is_none()) {
                let key = memory_key("#route", &format!("{filled}/{}", rest.join("/")), false);
                state.delete(&plan.session, INTERNAL_TYPE, &key).await;
            }
            continue;
        }
        let (Some((last, parent)), Some((parent_path, _))) =
            (segments.split_last(), filled.rsplit_once('/'))
        else {
            continue;
        };
        let child = segment_param(last)
            .and_then(|param| call.path_params.get(param))
            .and_then(id_string);
        let Some(child) = child.filter(|_| same_route(parent, &below)) else {
            continue;
        };
        let key = memory_key("#route", parent_path, false);
        let mut memory = read_memory(state, &plan.session, &key).await;
        let before = memory.requests.len();
        memory
            .requests
            .retain(|r| !r.get("body").is_some_and(|body| mentions(body, &child)));
        if memory.requests.len() == before {
            continue;
        }
        if memory.requests.is_empty() {
            state.delete(&plan.session, INTERNAL_TYPE, &key).await;
        } else {
            write_memory(state, &plan.session, &key, &memory).await;
        }
    }
}

/// Keeps a 2xx write for the list read of its path, unless it stored a record of that
/// list's type, which the read shows as a related resource. `refused`: it stored nothing
/// for being another's or having nowhere to go. A delete also forgets what it outdated.
pub(super) async fn after_route_write(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    generated: &GeneratedResponse,
    (refused, retry): (bool, bool),
) {
    let rt = state.rt;
    let op = plan.op;
    if refused
        || !applies(rt, plan)
        || !matches!(op.kind, OperationKind::Command(_))
        || !(200..300).contains(&generated.status)
    {
        return;
    }
    note_last_write(state, plan, call).await;
    if op.kind == OperationKind::Command(Effect::Delete) {
        forget_deleted_route(state, plan, call).await;
    }
    tell_siblings(state, plan, call, generated, retry).await;
    let (Some(key), Some(list)) = (
        route_memory_key(rt, op, call),
        rt.index().written_lists.get(&route(op)),
    ) else {
        return;
    };
    let stored = generated.body.as_ref().or(call.body.as_ref());
    let of_the_list = op.resource_type.as_deref() == Some(list.resource_type.as_str());
    if of_the_list && stored.is_some_and(|stored| created_id(op, stored).is_some()) {
        return;
    }
    let body = call
        .body
        .as_ref()
        .and_then(|body| told_to_list(rt, plan, list, body, op.input.body.as_ref()));
    let entry = memory_entry(op, body, None);
    remember(state, &plan.session, &key, &entry, None, retry).await;
}

/// Where the session's latest write a client may retry is kept (see [`retry_hash`]),
/// beside its other memory: resets, TTLs and every node see it.
const LAST_WRITE: &str = "#last-write";

/// How long after a write its repeat counts as a client's retry, in milliseconds.
const RETRY_WINDOW_MS: i64 = 30_000;

/// The header a `POST` names its retries by.
const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// A hash of what makes a write one a client retries: a `PUT` or a `DELETE`, which
/// HTTP makes idempotent, or a `POST` sending an `Idempotency-Key` header, by its
/// method, operation, filled path, body and that key. A hash, as those may hold a
/// secret. `None` for any other write: two such are two writes.
fn retry_hash(op: &Operation, call: &OperationCall) -> Option<String> {
    let Binding::Http { method, path } = &op.binding else {
        return None;
    };
    let key = call
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(IDEMPOTENCY_KEY))
        .and_then(|(_, value)| value.as_str());
    match (method, key) {
        (mb_mcm::HttpMethod::Put | mb_mcm::HttpMethod::Delete, _)
        | (mb_mcm::HttpMethod::Post, Some(_)) => {}
        _ => return None,
    }
    let filled = fill_path(call, &path_segments(path))?;
    let body = call.body.as_ref().map(Value::to_string).unwrap_or_default();
    let key = key.unwrap_or_default();
    let text = format!("{method:?}\n{}\n{}\n{body}\n{key}", op.id, filled.path);
    let digest = Sha256::digest(text.as_bytes());
    Some(digest[..16].iter().map(|b| format!("{b:02x}")).collect())
}

/// Keeps the session's latest 2xx write, when a client may retry it, for [`is_retry`];
/// any other write ends what was kept, as a retry repeats the latest write.
async fn note_last_write(state: &mut StateAccess<'_>, plan: &CallPlan<'_>, call: &OperationCall) {
    match retry_hash(plan.op, call) {
        Some(hash) => {
            let at = chrono::Utc::now().timestamp_millis();
            state
                .upsert(
                    &plan.session,
                    StoredResource {
                        resource_type: INTERNAL_TYPE.to_string(),
                        id: LAST_WRITE.to_string(),
                        data: serde_json::json!({ "write": hash, "at": at }),
                    },
                )
                .await;
        }
        None => {
            state.delete(&plan.session, INTERNAL_TYPE, LAST_WRITE).await;
        }
    }
}

/// Whether `call` is a client's retry of the session's latest 2xx write: one a client
/// may retry (see [`retry_hash`]), the same, within [`RETRY_WINDOW_MS`]. A retried write
/// answering no body is asked for no effects again, which could apply twice, and the
/// request it repeats is remembered once.
pub(super) async fn is_retry(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> bool {
    if !applies(state.rt, plan) || !matches!(plan.op.kind, OperationKind::Command(_)) {
        return false;
    }
    let Some(hash) = retry_hash(plan.op, call) else {
        return false;
    };
    let Some(doc) = state.get(&plan.session, INTERNAL_TYPE, LAST_WRITE).await else {
        return false;
    };
    let now = chrono::Utc::now().timestamp_millis();
    doc.data.get("write").and_then(Value::as_str) == Some(hash.as_str())
        && doc
            .data
            .get("at")
            .and_then(Value::as_i64)
            .is_some_and(|at| (0..=RETRY_WINDOW_MS).contains(&(now - at)))
}

/// Whether the answer the engine serves `plan` has no body to generate: the variant of
/// its forced status, else its success, declares no schema (see
/// [`mb_engine::Engine::generate`]).
pub(super) fn answers_no_body(plan: &CallPlan<'_>) -> bool {
    let op = plan.op;
    let variant = match plan.forced_status {
        Some(status) => op.output_for_status(status),
        None => op.success_output(),
    };
    variant.is_some_and(|v| v.schema.is_none())
}

/// How deep a write's body is looked into for a member naming a sibling list: the
/// body's own members and theirs (`{"ticket": {"comment": ...}}`).
const SIBLING_MEMBER_DEPTH: usize = 2;

/// Keeps a 2xx write for the list reads one level under its path's parent (see
/// [`SiblingList`]), each by its own path's key, so that its read is told it as a write
/// to its own route is. Told to a list:
///
/// - whose path ends in the segment the write's route ends in
///   (`POST /lists/{id}/relationships/profiles` to `/lists/{id}/profiles`): with what
///   the request says, as a write to the list's own route;
/// - else, for a write on the parent itself, whose last segment a member of the body
///   holding an object or a list names by its stem (`PUT /tickets/{id}` with
///   `ticket.comment`, to `/tickets/{id}/comments`): that member's value alone, by the
///   schema the request declares it with. A member on a sibling route
///   (`PUT /racks/{id}/featured` with `book`) is no membership of the parent's list.
///
/// Not when it stored a record of the list's type, which the read shows. Only told,
/// past the checks a write to the list's own route has: nothing is forced or changed.
async fn tell_siblings(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    generated: &GeneratedResponse,
    retry: bool,
) {
    let rt = state.rt;
    let op = plan.op;
    let path = route(op);
    let Some(siblings) = rt.index().sibling_lists.get(&path) else {
        return;
    };
    let segments = path_segments(&path);
    let stored = generated.body.as_ref().or(call.body.as_ref());
    let created = stored.is_some_and(|stored| created_id(op, stored).is_some());
    let request = op.input.body.as_ref();
    for sibling in siblings {
        if created && op.resource_type.as_deref() == Some(sibling.list.resource_type.as_str()) {
            continue;
        }
        let body = if sibling.by_route {
            call.body
                .as_ref()
                .and_then(|body| told_to_list(rt, plan, &sibling.list, body, request))
        } else {
            // Only a write on the parent itself (see [`SiblingList`]), by the list's segment.
            let stems: Vec<String> = sibling
                .rest
                .last()
                .map(|s| type_stem(s))
                .into_iter()
                .collect();
            let Some((member, schema)) = call
                .body
                .as_ref()
                .and_then(|body| member_named(rt, body, request, &stems, 0))
            else {
                continue;
            };
            let Some(told) = told_to_list(rt, plan, &sibling.list, member, schema.as_ref()) else {
                continue;
            };
            Some(told)
        };
        let Some(parent) = fill_path(call, &segments[..sibling.prefix]) else {
            continue;
        };
        let filled = format!("{}/{}", parent.path, sibling.rest.join("/"));
        let key = memory_key("#route", &filled, false);
        remember(
            state,
            &plan.session,
            &key,
            &memory_entry(op, body, None),
            None,
            retry,
        )
        .await;
    }
}

/// The first member of `body`, at most [`SIBLING_MEMBER_DEPTH`] deep, holding an object
/// or a list, whose name has one of `stems` (see [`type_stem`]), with the schema `schema` declares it by. In the
/// order the body holds its members, the shallower first.
fn member_named<'a>(
    rt: &MockRuntime,
    body: &'a Value,
    schema: Option<&Value>,
    stems: &[String],
    depth: usize,
) -> Option<(&'a Value, Option<Value>)> {
    let Value::Object(members) = body else {
        return None;
    };
    let bundle = rt.bundle();
    let (parts, _) = schema_parts(bundle, schema);
    let declared = |key: &str| {
        parts
            .iter()
            .find_map(|p| p.get("properties")?.get(key).cloned())
    };
    if let Some((key, member)) = members.iter().find(|(key, member)| {
        (member.is_object() || member.is_array()) && stems.contains(&type_stem(key))
    }) {
        return Some((member, declared(key)));
    }
    if depth + 1 >= SIBLING_MEMBER_DEPTH {
        return None;
    }
    members.iter().find_map(|(key, member)| {
        member_named(rt, member, declared(key).as_ref(), stems, depth + 1)
    })
}

/// What an update answering nothing leaves to tell its target, having merged what
/// it could into the stored copy (which publishes the change). One the session did
/// not hold stored its request as it came, so that holds itself; one with no body
/// has only its path to tell.
///
/// What the description says of the update applies here too: its request's record
/// is where `request.record` points (see [`records::request_record`]), and its
/// `transition` and `changes` are set on the merged copy (see [`transitions::apply`]).
/// A replace was stored by `write_state` (see [`keeps_stored`]), and is left
/// as it stored it.
async fn update_without_answer(
    state: &mut StateAccess<'_>,
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    w: &Written<'_>,
    raw: Option<&Value>,
    request: Option<&Value>,
    acted_on: Option<&str>,
) -> Option<Tell> {
    let session = &plan.session;
    let op = plan.op;
    let Some(held) = w.held else {
        return match raw {
            Some(_) => None,
            None => acted_on.map(|id| Tell {
                id: id.to_string(),
                body: None,
                own: None,
                stored: false,
                publish: true,
            }),
        };
    };
    let t = held.resource_type.as_str();
    let Some(raw) = raw else {
        // Nothing sent: only what the description says the update sets.
        let mut data = held.data.clone();
        transitions::apply(rt, op, w.call.body.as_ref(), &mut data);
        let changed = data != held.data;
        if changed {
            store_merged(state, rt, session, held, data).await;
        }
        return acted_on.map(|id| Tell {
            id: id.to_string(),
            body: None,
            own: None,
            stored: true,
            publish: !changed,
        });
    };
    let raw = records::request_record(rt, op, raw);
    let request = request.map(|r| records::request_record(rt, op, r));
    if transitions::replaces(rt, op) {
        // Stored whole by `write_state`: merging onto the copy held before would bring
        // back what the replace dropped.
        let own = request.and_then(|r| own_part(rt, t, r));
        set_own(state, session, &memory_key(t, &held.id, true), own).await;
        return None;
    }
    // The path says whose it is: a member saying otherwise is not applied or told.
    let raw = &without_contradictions(rt, plan, w.call, held, raw);
    let request = request.map(|r| without_contradictions(rt, plan, w.call, held, r));
    let (mut merged, rest) = apply_update(rt, held, raw);
    transitions::apply(rt, op, w.call.body.as_ref(), &mut merged);
    let changed = merged != held.data;
    if changed {
        store_merged(state, rt, session, held, merged.clone()).await;
    }
    let own = request.as_ref().and_then(|r| own_part(rt, t, r));
    // What is left, as it may be kept: every credential has gone by now, since a
    // scrubbed request has the same shape, and of the rest only what is told (see
    // [`told_of_update`]). What the copy holds has nothing to tell, and what was
    // redacted nothing a catch-up could show.
    let kept = match (&request, &rest) {
        (Some(request), Some(rest)) => told_of_update(rt, plan, t, &kept_part(request, rest)),
        _ => None,
    };
    let Some(kept) = kept.filter(|kept| !carries(kept, &merged) && !only_redacted(kept)) else {
        // Applied in full, but what is still told is older: this has the last word.
        set_own(state, session, &memory_key(t, &held.id, true), own).await;
        return None;
    };
    Some(Tell {
        id: acted_on?.to_string(),
        body: Some(kept),
        own,
        stored: true,
        publish: !changed,
    })
}

/// Stores `data` as `held`'s copy, keeping its place in the collections.
async fn store_merged(
    state: &mut StateAccess<'_>,
    rt: &MockRuntime,
    session: &SessionKey,
    held: &StoredResource,
    data: Value,
) {
    let resource = StoredResource {
        data,
        ..held.clone()
    };
    if state.upsert(session, resource.clone()).await {
        rt.collections().carry(session, held, &resource);
    }
}

/// Whether `data` says it is not `owner`'s, as a write taking it would see (see
/// [`known_foreign`]), with no record of where it was stored to go by.
fn says_foreign(rt: &MockRuntime, r: &StoredResource, owner: &Owner) -> bool {
    match owner_evidence(rt, r, owner) {
        Some(shown) => !shown,
        None => links_elsewhere(r, owner),
    }
}

/// `body` without the members that would make `held` say it is another owner's than
/// the path names (see [`guarded`]).
fn without_contradictions(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: &StoredResource,
    body: &Value,
) -> Value {
    let (Some(owner), Value::Object(fields), Value::Object(stored)) =
        (guarded(rt, plan.op, call), body, &held.data)
    else {
        return body.clone();
    };
    if says_foreign(rt, held, &owner) {
        return body.clone();
    }
    let kept = fields.iter().filter(|(key, value)| {
        let mut data = stored.clone();
        data.insert((*key).clone(), (*value).clone());
        let candidate = StoredResource {
            data: Value::Object(data),
            ..held.clone()
        };
        !says_foreign(rt, &candidate, &owner)
    });
    Value::Object(kept.map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// What a 204 update's `rest`, the part of its scrubbed request it could not apply to
/// `t`'s stored copy, tells that copy's reads: as a list read is told of a write, only
/// the members the read's or the request's schema declares (see [`declared_plainly`]),
/// past the wider checks of [`for_context`]. An undeclared member may be anything, a
/// `code` or a `value` included, so it is not told. `None` when nothing is left.
fn told_of_update(rt: &MockRuntime, plan: &CallPlan<'_>, t: &str, rest: &Value) -> Option<Value> {
    let read = read_schema(rt, t);
    let request = plan.op.input.body.as_ref();
    declared_plainly(rt, t, (read, request), rest, 0)
        .and_then(|told| for_context(rt, plan, t, &told, request, read))
}

/// The part of scrubbed `request` that `rest` is the raw form of.
fn kept_part(request: &Value, rest: &Value) -> Value {
    let (Value::Object(scrubbed), Value::Object(raw)) = (request, rest) else {
        return request.clone();
    };
    let kept = raw
        .iter()
        .filter_map(|(key, inner)| {
            let member = scrubbed.get(key)?;
            Some((key.clone(), kept_part(member, inner)))
        })
        .collect();
    Value::Object(kept)
}

/// The route a call was made to: its template, as a path's values and its query may
/// carry credentials, and the memory belongs to the resource they named.
fn route(op: &Operation) -> String {
    match &op.binding {
        Binding::Http { path, .. } => path.clone(),
        _ => String::new(),
    }
}

/// What the identity of `held` says, over `answer`: a regeneration is of the same
/// resource, however the LLM wrote its id or the key its path reads by.
fn keeping_identity(rt: &MockRuntime, held: &StoredResource, answer: &Value) -> Value {
    let mut answer = answer.clone();
    let index = rt.index();
    let t = &held.resource_type;
    let fields = ["id", "number"]
        .into_iter()
        .chain(index.identity_fields(t).iter().map(String::as_str))
        .chain(index.path_fields(t).iter().map(String::as_str));
    if answer.is_object() {
        for field in fields {
            if let Some(value) = mb_mcm::member_at(&held.data, field) {
                mb_mcm::set_member_at(&mut answer, field, value.clone());
            }
        }
    }
    answer
}

/// What the engine asks of an update's stored target, for its answer to settle on it.
pub(super) struct SettleGuard<'a> {
    pub(super) rt: &'a MockRuntime,
    pub(super) plan: &'a CallPlan<'a>,
    pub(super) call: &'a OperationCall,
    pub(super) held: &'a StoredResource,
    /// The concurrency tokens the update advances (see [`tokens`]).
    pub(super) advancing: &'a [String],
}

impl std::fmt::Debug for SettleGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettleGuard")
            .field("operation", &self.plan.op.id)
            .field("id", &self.held.id)
            .finish_non_exhaustive()
    }
}

impl mb_engine::UpdateGuard for SettleGuard<'_> {
    fn settles(&self, status: u16) -> bool {
        is_read_schema(
            self.rt,
            self.plan.op,
            self.plan.op.output_for_status(status),
        )
    }

    fn keep_identity(&self, candidate: Value) -> Value {
        keeping_identity(self.rt, self.held, &candidate)
    }

    fn keeps_owner(&self, candidate: &Value) -> bool {
        keeps_owner(self.rt, self.plan, self.call, self.held, candidate)
    }

    fn advances(&self, member: &str) -> bool {
        self.advancing.iter().any(|m| m == member)
    }
}

/// What of an effect's `set` may go on the target its update just stored: what the read
/// schema declares at the top level (all of it where the schema names no members or a
/// union may hold others), and of that not a path member, not what the request names
/// (in its body or its query, at the top level or one object down, `attributes.title`,
/// see [`Request::unsettled`]) where the answer of `status` settles it, and not what
/// would show another owner. An answer with the read's schema settles all the
/// request names; one of another schema (an acknowledgement `{id, name}`) only what it
/// declares, or all where it is a union or a map: a request member it leaves out reaches
/// the record by an effect alone, and only with the value the request sent. So does one
/// the request wraps one object down (`{incident: {status}}`), which no answer settles. A
/// `readOnly` member may go on:
/// a client cannot set it, and an effect is the server saying what followed. The check on
/// identity comes before this, as for any effect.
pub(super) fn own_effect_members(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: &StoredResource,
    set: &Map<String, Value>,
    status: u16,
) -> OwnMembers {
    let t = held.resource_type.as_str();
    let bundle = rt.bundle();
    let level = Level::of(bundle, read_schema(rt, t));
    let output = plan.op.output_for_status(status);
    let whole = is_read_schema(rt, plan.op, output);
    let request = Request {
        bundle,
        body: call.body.as_ref(),
        query: &call.query,
        record: read_schema(rt, t),
        answer: (!whole).then(|| output.and_then(|o| o.schema.as_ref())),
        held: &held.data,
        replaces: matches!(
            plan.op.binding,
            Binding::Http {
                method: mb_mcm::HttpMethod::Put,
                ..
            }
        ),
    };
    let declaring = !level.union && !level.properties.is_empty();
    let declared = |key: &str| level.properties.iter().find_map(|p| p.get(key));
    let index = rt.index();
    let paths = index.path_fields(t);
    // A member named for a parameter of this call's path, and holding its value.
    let shows_path = |key: &str| {
        held.data.get(key).and_then(id_string).is_some_and(|value| {
            call.path_params.iter().any(|(param, given)| {
                named_for_param(key, param) && id_string(given).as_ref() == Some(&value)
            })
        })
    };
    set.iter()
        .filter(|(key, _)| !declaring || declared(key).is_some())
        .filter(|(key, _)| !paths.contains(*key) && !shows_path(key))
        .filter_map(|(key, value)| Some((key, request.unsettled(&[key.as_str()], value)?)))
        .map(|(key, value)| {
            let whole = match (request.replaces, request.sent(&[key.as_str()])) {
                (true, Some(_)) => replacing(rt, held.data.get(key), &value, declared(key)),
                _ => None,
            };
            match whole {
                Some(whole) => (key, whole, true),
                None => (key, value, false),
            }
        })
        .filter(|(key, value, whole)| {
            // The member as it would merge (see [`merge_at`]), not as written: one a
            // `PUT` replaces whole goes where nothing is held.
            let mut alone = held.data.clone();
            if let Value::Object(members) = &mut alone {
                let was = if *whole { None } else { members.get(*key) };
                let merged = merge_at(rt, was, value, declared(key));
                match merged {
                    Some(merged) => members.insert((*key).clone(), merged),
                    None => return false,
                };
            }
            keeps_owner(rt, plan, call, held, &alone)
        })
        .fold(OwnMembers::default(), |mut out, (key, value, whole)| {
            if whole {
                out.whole.push(key.clone());
            }
            out.set.insert(key.clone(), value);
            out
        })
}

/// What [`own_effect_members`] lets go on an update's own target.
#[derive(Default)]
pub(super) struct OwnMembers {
    /// The members, as they go on (see [`merge_at`]).
    pub(super) set: Map<String, Value>,
    /// Those a `PUT` replaces whole (see [`replacing`]): each goes where nothing is
    /// held, keeping nothing of the held member it does not carry.
    pub(super) whole: Vec<String>,
}

/// What an update's request sent, and what its answer settles of it (see
/// [`own_effect_members`]).
struct Request<'a> {
    bundle: &'a mb_mcm::Bundle,
    body: Option<&'a Value>,
    query: &'a Map<String, Value>,
    /// The record's read schema: a body member it declares is the record's, not a wrapper.
    record: Option<&'a Value>,
    /// The answer's schema where it is not the read's, which settles all the request
    /// names: `None` for that, `Some(None)` for an answer of no schema.
    answer: Option<Option<&'a Value>>,
    /// The record as the store holds it, before the update's effects.
    held: &'a Value,
    /// The update is a `PUT`: each top-level member it names replaces the held one.
    replaces: bool,
}

impl<'a> Request<'a> {
    /// What the request sent for the record's member at `path`: in its body at that
    /// path, or one object down where the body wraps the record in a member the record
    /// does not declare (`{data: {attributes: …}}` for `attributes`), and in its query
    /// for a top-level member. The flag says it was found wrapped.
    fn sent(&self, path: &[&str]) -> Option<(&'a Value, bool)> {
        let at = |root: &'a Value| -> Option<&'a Value> {
            path.iter()
                .try_fold(root, |value, key| value.as_object()?.get(*key))
        };
        let body = self.body.filter(|b| b.is_object());
        let wrapped = || {
            let members = body?.as_object()?;
            let first = path.first()?;
            if members.contains_key(*first) {
                return None;
            }
            let record = Level::of(self.bundle, self.record);
            members
                .iter()
                .filter(|(key, v)| v.is_object() && record.member(key).is_none())
                .find_map(|(_, v)| at(v))
        };
        let query = || match path {
            [key] => self.query.get(*key),
            _ => None,
        };
        body.and_then(at)
            .map(|v| (v, false))
            .or_else(|| wrapped().map(|v| (v, true)))
            .or_else(|| query().map(|v| (v, false)))
    }

    /// Whether the answer settles the request's member at `path`: one with the read's
    /// schema settles all it names, another what it declares along `path`, or all
    /// below a union or a map. A member the request sent `wrapped` it does not: the
    /// engine settles an answer, and applies the request onto it, by the request's
    /// members at the record's own paths alone, so what a wrapper holds reaches the
    /// record by an effect or not at all.
    fn answers(&self, path: &[&str], wrapped: bool) -> bool {
        if wrapped {
            return false;
        }
        let Some(mut schema) = self.answer else {
            return true;
        };
        for key in path {
            let level = Level::of(self.bundle, schema);
            if level.union || level.map.is_some() {
                return true;
            }
            match level.named(key) {
                Some(inner) => schema = Some(inner),
                None => return false,
            }
        }
        true
    }

    /// The part of `value`, an effect's member at `path`, the request does not settle:
    /// a member the request sent is the answer's where the answer settles it, and
    /// takes the effect only where the effect agrees with what the request sent. An
    /// object the request sent as one is told member by member, at any depth, so a
    /// member written one object down is held to the request's member there, but only
    /// where the record's object merges member by member ([`Self::merges`]): what is
    /// left of an object replaced whole (a map, `metadata: {plan, locale}`) would
    /// replace it, the request's own keys gone, so such an object is told whole. Where
    /// the update is no `PUT` replacing it and the place declares no members (a map),
    /// what is told of it is laid onto the held object (see [`patched`]): an agreeing
    /// effect that writes only the request's keys keeps the ones it held.
    /// `None` when nothing is left.
    fn unsettled(&self, path: &[&str], value: &Value) -> Option<Value> {
        let Some((given, wrapped)) = self.sent(path) else {
            return Some(value.clone());
        };
        let answered = self.answers(path, wrapped);
        if let (Value::Object(_), Value::Object(members)) = (given, value)
            && self.merges(path)
        {
            let kept: Map<String, Value> = members
                .iter()
                .filter_map(|(key, inner)| {
                    let mut deeper = path.to_vec();
                    deeper.push(key.as_str());
                    Some((key.clone(), self.unsettled(&deeper, inner)?))
                })
                .collect();
            return (!kept.is_empty()).then_some(Value::Object(kept));
        }
        let replaced = self.replaces && path.len() == 1;
        if answered || !agrees(given, value, replaced) {
            return None;
        }
        let level = Level::of(self.bundle, self.schema_at(path));
        let had = path
            .iter()
            .try_fold(self.held, |v, key| v.as_object()?.get(*key));
        match (given, value, had) {
            (Value::Object(sent), Value::Object(set), Some(Value::Object(had)))
                if !replaced && !level.union && level.properties.is_empty() =>
            {
                Some(Value::Object(patched(had, sent, set)))
            }
            _ => Some(value.clone()),
        }
    }

    /// The record's schema at `path`, where it declares one.
    fn schema_at(&self, path: &[&str]) -> Option<&'a Value> {
        let mut schema = self.record;
        for key in path {
            schema = Some(Level::of(self.bundle, schema).named(key)?);
        }
        schema
    }

    /// Whether an effect's object at `path` merges into the record's member by member,
    /// both on the store (see [`merge_at`]) and as the request went on the answer:
    /// the record's place there declares members and is no union, and it
    /// is not a top-level member a `PUT` replaced whole, whether its answer settles it
    /// or an effect alone carries it (see [`replacing`]).
    fn merges(&self, path: &[&str]) -> bool {
        if self.replaces && path.len() == 1 {
            return false;
        }
        let Some(schema) = self.schema_at(path) else {
            return false;
        };
        let level = Level::of(self.bundle, Some(schema));
        !level.union && !level.properties.is_empty()
    }
}

/// `set`, what an agreeing effect wrote of an object the request sent as `sent`, laid
/// onto `had`, the object held there, as a merge patch goes: a key the effect leaves out
/// stays, one the request sent as `null` and the effect leaves out goes, and an object
/// both hold under a key the request sent as an object is laid on the same way.
fn patched(
    had: &Map<String, Value>,
    sent: &Map<String, Value>,
    set: &Map<String, Value>,
) -> Map<String, Value> {
    let mut out = had.clone();
    out.retain(|key, _| set.contains_key(key) || !matches!(sent.get(key), Some(Value::Null)));
    for (key, value) in set {
        let deeper = match (out.get(key), sent.get(key), value) {
            (Some(Value::Object(had)), Some(Value::Object(sent)), Value::Object(set)) => {
                Value::Object(patched(had, sent, set))
            }
            _ => value.clone(),
        };
        out.insert(key.clone(), deeper);
    }
    out
}

/// `value`, an effect's object for a top-level member a `PUT` sent, as it replaces the
/// held one whole where the member's place declares members (which [`merge_at`] would
/// merge member by member); none for any other place, where the effect's value is put
/// as it merges. A held member the value leaves out is gone, at every depth of a
/// declared object both hold, but for a required one, left as held: without it the
/// record would break its schema, and the effect be refused whole.
fn replacing(
    rt: &MockRuntime,
    held: Option<&Value>,
    value: &Value,
    schema: Option<&Value>,
) -> Option<Value> {
    let (Some(Value::Object(had)), Value::Object(set)) = (held, value) else {
        return None;
    };
    let place = Place::of_held(rt, schema, held);
    if !place.declares() {
        return None;
    }
    let mut out = Map::new();
    for (key, inner) in set {
        let deeper = replacing(rt, had.get(key), inner, place.property(key));
        out.insert(key.clone(), deeper.unwrap_or_else(|| inner.clone()));
    }
    for (key, was) in had {
        if !set.contains_key(key) && place.requires(key) {
            out.insert(key.clone(), was.clone());
        }
    }
    Some(Value::Object(out))
}

/// Whether `value`, what an effect set, says what `given`, what a request sent, did:
/// one value (see [`same_value`]), or, for an object that does not `replace` the held
/// one, an object holding each member the request sent as it sent it, where a `null`
/// the request sent is a member the effect leaves out or clears.
fn agrees(given: &Value, value: &Value, replace: bool) -> bool {
    match (given, value) {
        (Value::Object(sent), Value::Object(set)) if !replace => {
            sent.iter().all(|(key, inner)| match (inner, set.get(key)) {
                (Value::Null, None | Some(Value::Null)) => true,
                (_, Some(was)) => agrees(inner, was, false),
                (_, None) => false,
            })
        }
        _ => same_value(given, value),
    }
}

/// Whether `given`, what a request sent, and `value`, what an effect set, are one value:
/// equal, or scalars of one text (a query's `5` for a body's `5`).
fn same_value(given: &Value, value: &Value) -> bool {
    let text = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(_) | Value::Bool(_) => Some(v.to_string()),
        _ => None,
    };
    given == value || text(given).is_some_and(|g| text(value).as_ref() == Some(&g))
}

/// What the applied effects of an update's answer set on `own`, the target it just stored,
/// shown in the answer the client is served, as the store now has it. An answer settling
/// on its target keeps only what the request names (see [`mb_engine::UpdateGuard`]), and
/// a request may name what no member of the read is (`state_event: close` for `state`),
/// so an effect is how what followed reaches the store; it reaches the answer too. Only
/// the members an effect applied ([`EffectOutcome::applied`], what [`own_effect_members`]
/// allowed) go, as they now stand, and only on an answer with the read's schema that
/// still suits the type with them. A rejected effect shows nothing.
pub(super) fn serve_own_effects(
    rt: &MockRuntime,
    op: &Operation,
    own: Option<(&str, &str)>,
    generated: &mut GeneratedResponse,
) {
    let Some((t, id)) = own else {
        return;
    };
    if !is_read_schema(rt, op, op.output_for_status(generated.status)) {
        return;
    }
    let mut set = Map::new();
    for outcome in &generated.effect_outcomes {
        let (None, Some(Value::Object(merged))) = (&outcome.rejected, &outcome.merged) else {
            continue;
        };
        if outcome.effect.resource_type != t || outcome.effect.id != id {
            continue;
        }
        for key in &outcome.applied {
            if let Some(value) = merged.get(key) {
                set.insert(key.clone(), value.clone());
            }
        }
    }
    let Some(Value::Object(body)) = generated.body.as_ref() else {
        return;
    };
    if set.iter().all(|(k, v)| body.get(k) == Some(v)) {
        return;
    }
    let mut served = body.clone();
    served.extend(set);
    let served = Value::Object(served);
    if entity_fits(rt, t, &served) {
        generated.body = Some(served);
    } else {
        // Stored, but not served: the answer and the store now differ.
        tracing::warn!(
            mock = %rt.name,
            operation = %op.id,
            resource_type = %t,
            "an update's applied effects on its own target do not fit its answer; served without them"
        );
    }
}

/// Whether `member` is named for path parameter `param`: as [`tied_to_param`] has it,
/// or as its last word, as `slug` is for `team_slug`.
fn named_for_param(member: &str, param: &str) -> bool {
    let last = param
        .rfind(|c: char| c == '_' || c == '-' || c.is_uppercase())
        .map_or(param, |at| param[at..].trim_start_matches(['_', '-']));
    tied_to_param(member, param) || member.eq_ignore_ascii_case(last)
}

/// Whether `answer` is still `held`'s owner's: by the check a later read makes (see
/// [`key_fits`]), and not linking elsewhere (see [`known_foreign`]).
pub(super) fn keeps_owner(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: &StoredResource,
    answer: &Value,
) -> bool {
    let Some(owner) = guarded(rt, plan.op, call) else {
        return true;
    };
    let candidate = StoredResource {
        data: answer.clone(),
        ..held.clone()
    };
    key_fits(rt, &plan.session, &candidate, &owner)
        && !known_foreign(rt, &plan.session, &candidate, &owner)
}

/// Whether this read of `held` changes it only by its answer's effects (see
/// [`Earlier::by_effects`]), so that only those on it may apply.
pub(super) fn by_effects(earlier: &Earlier, held: Option<&StoredResource>) -> bool {
    earlier.by_effects && held.is_some() && !earlier.shown.is_empty()
}

/// The effects a read changing `held` by effects alone may apply: only those on `held`,
/// as the requests it was told of only referred to it.
pub(super) fn effects_on_target(
    earlier: &Earlier,
    held: Option<&StoredResource>,
    effects: &mut Vec<ResourceEffect>,
) {
    if let (true, Some(held)) = (by_effects(earlier, held), held) {
        effects.retain(|e| e.resource_type == held.resource_type && e.id == held.id);
    }
}

/// After a generated 2xx read of `held` that changes by effects alone, once those applied:
/// it answers the stored copy as it now stands, whatever else the answer said, and the
/// requests it was told of are done. A failed generation never gets here (the stored
/// copy answers and the next read tries again, up to the cap).
pub(super) async fn settle_by_effects(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    earlier: &Earlier,
    held: Option<&StoredResource>,
    generated: &mut GeneratedResponse,
) {
    let (true, Some(held), Some(key)) = (by_effects(earlier, held), held, earlier.key.as_deref())
    else {
        return;
    };
    if !(200..300).contains(&generated.status) || transient_fallback(generated) {
        return;
    }
    let session = &plan.session;
    let current = state.get(session, &held.resource_type, &held.id).await;
    if let Some(current) = current {
        generated.body = Some(current.data);
    }
    forget_shown(state, session, key, &earlier.shown).await;
}

/// After a generated read that was told of remembered requests: when it is the
/// whole resource, its answer takes the stored copy's place and those requests
/// are done. A write landing during the generation leaves them: this answer did not
/// know of it. An answer that cannot be stored, or that stops showing the resource's
/// owner, is a failed attempt: the requests stay, and the stored copy answers.
pub(super) async fn settle_read(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target: (Option<&str>, Option<&StoredResource>),
    earlier: &mut Earlier,
    generated: &mut GeneratedResponse,
) {
    let rt = state.rt;
    let op = plan.op;
    let session = &plan.session;
    let (target_id, target) = target;
    let (Some(t), Some(id), Some(key)) =
        (op.resource_type.as_deref(), target_id, earlier.key.clone())
    else {
        return;
    };
    if !reads_the_resource(rt, op) {
        seed_own_read(state, session, rt, op, call, target_id, generated).await;
        return;
    }
    let Some(held) = target else {
        seed_own_read(state, session, rt, op, call, target_id, generated).await;
        let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
        let own = generated
            .body
            .as_ref()
            .and_then(|b| mb_mcm::member_at(b, field))
            .and_then(id_string);
        for stored in [Some(id.to_string()), own].into_iter().flatten() {
            if state.get(session, t, &stored).await.is_some() {
                forget_shown(state, session, &key, &earlier.shown).await;
                break;
            }
        }
        return;
    };
    // Settled by its effects instead (see [`settle_by_effects`]).
    if earlier.by_effects {
        return;
    }
    let Some(answer) = generated.body.as_ref() else {
        return;
    };
    let answer = keeping_identity(rt, held, answer);
    if !entity_fits(rt, t, &answer) || !keeps_owner(rt, plan, call, held, &answer) {
        failed_attempt(state, plan, earlier).await;
        if let Some(stored) = earlier.stored_answer.take() {
            *generated = GeneratedResponse {
                usage: generated.usage,
                ..*stored
            };
            // The caller works on the record an answer holds, and puts it back.
            if let Some(identity) = op.identity.as_ref().filter(|i| i.record.is_some()) {
                generated.body = generated
                    .body
                    .as_ref()
                    .and_then(|b| identity.record_in(b).cloned());
            }
            note_not_caught_up(generated, plan, Some(held), earlier);
        }
        return;
    }
    let current = state.get(session, t, &held.id).await;
    if current.as_ref().map(|c| &c.data) != Some(&held.data) {
        return;
    }
    let resource = StoredResource {
        resource_type: t.to_string(),
        id: held.id.clone(),
        data: answer.clone(),
    };
    // An answer that changes nothing is no change to publish, or to move in a list.
    if answer != held.data {
        if !state.upsert(session, resource.clone()).await {
            return;
        }
        rt.collections().carry(session, held, &resource);
    }
    generated.body = Some(answer);
    forget_shown(state, session, &key, &earlier.shown).await;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::http::HeaderMap;
    use mb_engine::{Engine, EngineConfig};
    use mb_llm::{Provider, ScriptedProvider};
    use mb_state::{MemoryStore, StateStore};
    use serde_json::json;

    use super::*;

    const SDL: &str = "
type Item { id: ID! title: String! }
type Query { item(id: ID!): Item }
type Mutation { createItem(title: String!): Item }
";
    const PROTO: &str = r#"
syntax = "proto3";
package items;
message Item { string id = 1; string title = 2; }
message GetItemRequest { string id = 1; }
message CreateItemRequest { string title = 1; }
service Items {
  rpc GetItem(GetItemRequest) returns (Item);
  rpc CreateItem(CreateItemRequest) returns (Item);
}
"#;

    /// Runs a write of `contract`'s first command and returns the store's memory.
    async fn memory_after_a_write(
        contract: &[u8],
        answer: Value,
    ) -> (Vec<StoredResource>, Binding) {
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(answer.to_string())]));
        let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(60)));
        let rt = MockRuntime::new(
            "m",
            Arc::new(mb_import::compile(contract).unwrap()),
            Arc::new(Engine::new(
                Some(provider as Arc<dyn Provider>),
                EngineConfig::default(),
            )),
            Arc::clone(&store),
        );
        let op = rt
            .bundle()
            .operations
            .iter()
            .find(|o| matches!(o.kind, OperationKind::Command(_)))
            .unwrap();
        let plan = plan(&rt, op, &HeaderMap::new(), Presented::default())
            .await
            .unwrap();
        let call = OperationCall {
            operation_id: op.id.clone(),
            body: Some(json!({"title": "never echoed", "name": "never echoed"})),
            ..OperationCall::default()
        };
        generate_once(&rt, &plan, &call).await.unwrap();
        let session = plan.session.clone();
        let memory = store.list(&session, INTERNAL_TYPE, 10).await.unwrap();
        (memory, op.binding.clone())
    }

    /// The memory is for HTTP-bound operations: gRPC, SOAP and GraphQL calls
    /// through the same pipeline remember nothing, so they generate as before.
    #[tokio::test]
    async fn only_http_operations_are_remembered() {
        let wsdl = format!(
            "{}/../../fixtures/wsdl/accounts-rpc.wsdl",
            env!("CARGO_MANIFEST_DIR")
        );
        let (memory, binding) =
            memory_after_a_write(PROTO.as_bytes(), json!({"id": "i1", "title": "x"})).await;
        assert!(matches!(binding, Binding::Grpc { .. }), "{binding:?}");
        assert!(memory.is_empty(), "{memory:?}");

        let (memory, binding) =
            memory_after_a_write(SDL.as_bytes(), json!({"id": "i1", "title": "x"})).await;
        assert!(matches!(binding, Binding::Graphql { .. }), "{binding:?}");
        assert!(memory.is_empty(), "{memory:?}");

        let (memory, binding) = memory_after_a_write(
            &std::fs::read(wsdl).unwrap(),
            json!({"id": "a1", "name": "x"}),
        )
        .await;
        assert!(matches!(binding, Binding::Soap { .. }), "{binding:?}");
        assert!(memory.is_empty(), "{memory:?}");
    }

    #[test]
    fn carries_compares_the_same_value_at_the_same_path() {
        let answer = json!({"id": "1", "customer": {"name": "Ann"}, "count": 2, "note": null});
        assert!(carries(&json!({"customer": {"name": "Ann"}}), &answer));
        assert!(carries(&json!({"count": 2, "note": null}), &answer));
        assert!(!carries(&json!({"customerName": "Ann"}), &answer));
        assert!(!carries(&json!({"customer": {"name": "Bo"}}), &answer));
        assert!(!carries(&json!({"quantity": 2}), &answer));
        assert!(!carries(&json!({"gone": null}), &answer));
        assert!(!carries(&json!({"password": REDACTED}), &answer));
    }

    #[test]
    fn a_body_over_the_cap_is_cut_on_a_character_boundary() {
        let kept = json!({"note": "short"});
        assert_eq!(within_cap(kept.clone()), kept);
        let cut = within_cap(json!({"note": "é".repeat(MAX_BODY_BYTES)}));
        let text = cut.as_str().unwrap();
        assert!(text.ends_with("[truncated]") && text.len() <= MAX_BODY_BYTES + 16);
        let marker = within_cap(json!({"note": "y".repeat(MAX_PREVIEWED_BYTES)}));
        assert_eq!(marker["truncated"], true);
    }

    #[test]
    fn credentials_are_redacted_at_any_depth() {
        let mut rt = MockRuntime::new(
            "m",
            Arc::new(mb_import::compile(SDL.as_bytes()).unwrap()),
            Arc::new(Engine::new(None, EngineConfig::default())),
            Arc::new(MemoryStore::new(Duration::from_secs(60))),
        );
        rt.set_data_plane_auth(crate::DataPlaneAuth {
            mode: crate::DataPlaneAuthMode::ApiKey,
            header: "x-house-pass".into(),
        });
        let mut body = json!({
            "a": {"api_key": "k", "b": [{"Password": "p", "c": 1}]},
            "x-house-pass": "h",
            "privateKey": "pk",
            "total_tokens": 7,
            "authored": "kept",
            "pairs": [{"key": "Cookie", "value": "c"}, {"key": "colour", "value": "red"}],
        });
        redact_credentials(&rt, &mut body, None, None);
        assert_eq!(
            body,
            json!({
                "a": {"api_key": REDACTED, "b": [{"Password": REDACTED, "c": 1}]},
                "x-house-pass": REDACTED,
                "privateKey": REDACTED,
                "total_tokens": 7,
                "authored": "kept",
                "pairs": [{"key": "Cookie", "value": REDACTED}, {"key": "colour", "value": "red"}],
            })
        );
    }
}
