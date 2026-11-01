//! What a 2xx write did not say, kept for the next generation of its resource.
//!
//! A create answering `{id, key}` or an action answering 204 leaves a stored
//! copy that does not hold what the request asked. A 204 update's members that the
//! read's schema declares go into the copy at once; what could not be applied is
//! remembered for the resource the write targets, and for the parent its path names
//! when the write is a create or an action answering nothing, under a reserved type
//! of the state store ([`mb_state::INTERNAL_TYPE`]). So Redis, SQL and memory all
//! keep it, sessions, TTLs and resets clear it, and it is no resource (not exported,
//! counted or seeded, and on no change feed). A remembered action publishes a change
//! of its target, for replicas, feeds and triggers; telling a parent changes nothing
//! it holds, and its read looks in the shared store before it serves, on any node.
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
fn applies(rt: &MockRuntime, plan: &CallPlan<'_>) -> bool {
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
    let told = memory
        .requests
        .iter()
        .map(|request| {
            let mut told = request.clone();
            if let Value::Object(members) = &mut told {
                members.remove("at");
            }
            told
        })
        .collect();
    Earlier {
        key: Some(key),
        shown: memory.requests,
        told,
        own: memory.own,
        attempts: memory.attempts,
        failed_now: false,
        stored_answer: None,
    }
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
fn read_schema<'a>(rt: &'a MockRuntime, t: &str) -> Option<&'a Value> {
    let index = rt.index();
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
    detail.is_some() && detail == own
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
/// nothing, which merges what it can. Storing the request in the copy's place would
/// lose what it did not repeat, identity included. Offline, and with consistency
/// off, the request still replaces it.
pub(super) fn keeps_stored(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    generated: &GeneratedResponse,
    held: bool,
) -> bool {
    held && generated.body.is_none()
        && applies(rt, plan)
        && plan.op.kind == OperationKind::Command(Effect::Update)
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
    (
        Value::Object(merged),
        (!rest.is_empty()).then_some(Value::Object(rest)),
    )
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
fn pinned_members<'a>(rt: &'a MockRuntime, t: &str) -> Vec<&'a str> {
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
fn is_credential_name(rt: &MockRuntime, name: &str) -> bool {
    is_sensitive(name) || is_own_credential(rt, name)
}

/// Whether `value` under `name` is a count, not a credential: `max_tokens: 100`.
fn is_count(name: &str, value: &Value) -> bool {
    let mut normalized = String::with_capacity(name.len());
    mb_mcm::credentials::normalize_into(name, &mut normalized);
    value.is_number() && mb_mcm::credentials::is_count_name(&normalized)
}

/// Whether `schema` says its value is not to be shown again.
fn schema_hides(bundle: &mb_mcm::Bundle, schema: Option<&Value>) -> bool {
    schema_parts(bundle, schema).0.iter().any(|p| {
        p.get("writeOnly").and_then(Value::as_bool) == Some(true)
            || p.get("format").and_then(Value::as_str) == Some("password")
    })
}

/// `value` with every credential replaced, at any depth. The contract speaks first:
/// what either schema marks `writeOnly` or `format: password`, and the mock's own
/// credential names. A member the read schema (`read`) declares otherwise is one the
/// API returns, so only a name ending in a credential word hides it. Otherwise, as the
/// last resort, a name holding one does, or a `{"name": "Authorization", "value": ...}`
/// pair.
fn redact_credentials(
    rt: &MockRuntime,
    value: &mut Value,
    schema: Option<&Value>,
    read: Option<&Value>,
) {
    let bundle = rt.bundle();
    match value {
        Value::Object(members) => {
            let named_pair = ["name", "key", "header"]
                .iter()
                .find_map(|n| members.get(*n)?.as_str())
                .is_some_and(|name| is_credential_name(rt, name));
            if named_pair && let Some(paired) = members.get_mut("value") {
                *paired = Value::from(REDACTED);
            }
            let (parts, _) = schema_parts(bundle, schema);
            let returned = Level::of(bundle, read);
            for (key, member) in members.iter_mut() {
                let inner = parts.iter().find_map(|p| p.get("properties")?.get(key));
                let read_inner = returned.named(key);
                let by_name = match read_inner {
                    Some(_) => mb_mcm::credentials::ends_in_credential(key),
                    None => is_credential_name(rt, key) && !is_count(key, member),
                };
                if schema_hides(bundle, inner)
                    || schema_hides(bundle, read_inner)
                    || is_own_credential(rt, key)
                    || by_name
                {
                    *member = Value::from(REDACTED);
                } else {
                    redact_credentials(rt, member, inner, read_inner);
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
                redact_credentials(rt, item, inner, read_inner);
            }
        }
        _ => {}
    }
}

/// A request body as it may be kept: credentials redacted, then the request
/// log's own redaction, then the PII the engine scrubs from answers and effects.
fn scrubbed(rt: &MockRuntime, plan: &CallPlan<'_>, body: &Value) -> Value {
    let mut body = body.clone();
    let read = plan
        .op
        .resource_type
        .as_deref()
        .and_then(|t| read_schema(rt, t));
    redact_credentials(rt, &mut body, plan.op.input.body.as_ref(), read);
    if let Some(recorder) = &rt.recorder {
        recorder.redact(&mut body);
    }
    plan.engine.scrub(&mut body);
    body
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
    if let Some(id) = stored.get(field).and_then(id_string) {
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
/// itself in place of any older own body. A new request is worth trying for, so the
/// count of failed catch-ups starts over.
async fn remember(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    key: &str,
    entry: &Value,
    own: Option<Value>,
) {
    let mut memory = read_memory(state, session, key).await;
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

/// Ends what an older own body may overlay: a later write has the last word.
async fn clear_own(state: &mut StateAccess<'_>, session: &SessionKey, key: &str) {
    let mut memory = read_memory(state, session, key).await;
    if memory.own.take().is_some() {
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
/// (`POST /captures/{id}/refund`), and of an action answering nothing: an answer
/// about a child says nothing to it. An update or a delete of a child does not
/// tell it, or every nested write would cost it a regeneration. Telling it changes
/// nothing it holds, so it publishes nothing: its read looks in the shared store
/// before it serves, on any node.
///
/// Only an update's or a create's body is the resource's own, for a catch-up to
/// keep, and only the latest write's: last write wins. A child's or an action's is
/// told, and is about something else, and like an applied effect or a fully applied
/// update it ends what an older body may overlay.
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
    let request = raw.map(|b| scrubbed(rt, plan, b));
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
                    body: request.clone(),
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
        && let Some(key) = w.earlier.key.as_deref().filter(|_| w.earlier.own.is_some())
    {
        clear_own(state, session, key).await;
    }
    let tells_parent =
        effect == Effect::Create || (effect == Effect::Other && generated.body.is_none());
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
    if target.is_none() && parent.is_none() {
        return;
    }
    let entry = |body: Option<Value>| {
        let mut entry = Map::new();
        entry.insert("operation".to_string(), Value::from(op.id.as_str()));
        entry.insert("path".to_string(), Value::from(route(op)));
        if let Some(body) = body {
            entry.insert("body".to_string(), within_cap(body));
        }
        entry.insert(
            "at".to_string(),
            Value::from(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
        Value::Object(entry)
    };
    if let Some(tell) = target {
        let key = memory_key(t, &tell.id, tell.stored);
        remember(state, session, &key, &entry(tell.body), tell.own).await;
        if tell.publish
            && let Some(held) = w.held
        {
            state.upsert(session, held.clone()).await;
        }
    }
    if let Some(parent) = parent {
        let key = memory_key(&parent.resource_type, &parent.id, true);
        remember(state, session, &key, &entry(request), None).await;
    }
}

/// What an update answering nothing leaves to tell its target, having merged what
/// it could into the stored copy (which publishes the change). One the session did
/// not hold stored its request as it came, so that holds itself; one with no body
/// has only its path to tell.
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
    let (Some(raw), Some(held)) = (raw, w.held) else {
        return match raw {
            Some(_) => None,
            None => acted_on.map(|id| Tell {
                id: id.to_string(),
                body: None,
                own: None,
                stored: w.held.is_some(),
                publish: true,
            }),
        };
    };
    let t = held.resource_type.as_str();
    // The path says whose it is: a member saying otherwise is not applied or told.
    let raw = &without_contradictions(rt, plan, w.call, held, raw);
    let request = request.map(|r| without_contradictions(rt, plan, w.call, held, r));
    let (merged, rest) = apply_update(rt, held, raw);
    let changed = merged != held.data;
    if changed {
        let resource = StoredResource {
            data: merged.clone(),
            ..held.clone()
        };
        if state.upsert(session, resource.clone()).await {
            rt.collections().carry(session, held, &resource);
        }
    }
    let own = request.as_ref().and_then(|r| own_part(rt, t, r));
    // What is left, as it may be kept: every credential has gone by now, since a
    // scrubbed request has the same shape. What the copy holds has nothing to tell,
    // and what was redacted nothing a catch-up could show.
    let kept = match (&request, &rest) {
        (Some(request), Some(rest)) => Some(kept_part(request, rest)),
        _ => None,
    };
    let kept = kept.filter(|kept| !carries(kept, &merged) && !only_redacted(kept))?;
    Some(Tell {
        id: acted_on?.to_string(),
        body: Some(kept),
        own,
        stored: true,
        publish: !changed,
    })
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
    if let Value::Object(members) = &mut answer {
        for field in fields {
            if let Some(value) = held.data.get(field) {
                members.insert(field.to_string(), value.clone());
            }
        }
    }
    answer
}

/// Whether `answer` is still `held`'s owner's: by the check a later read makes (see
/// [`key_fits`]), and not linking elsewhere (see [`known_foreign`]).
fn keeps_owner(
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
            .and_then(|b| b.get(field))
            .and_then(id_string);
        for stored in [Some(id.to_string()), own].into_iter().flatten() {
            if state.get(session, t, &stored).await.is_some() {
                forget_shown(state, session, &key, &earlier.shown).await;
                break;
            }
        }
        return;
    };
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
