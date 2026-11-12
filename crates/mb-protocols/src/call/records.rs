//! Identity and record pointers read from the effective world description.
//!
//! Each function answers from the description where it says something tier 0 does not,
//! and from today's heuristic where it is silent, so a runtime with no world and one
//! serving tier 0 answer alike:
//!
//! - [`identity_header`]: the header a create names its new record's id in: the
//!   description's `answer.identity_header` (a name, or `false` for none) where it
//!   differs from tier 0's, else the detector ([`mb_engine::headers::identity_header`],
//!   which tier 0 restates). [`header_ids`] mints, places and stores through it.
//! - [`alternates`] and [`extra_fields`]: the members a path value may name a record by
//!   besides the ones each lookup site already tries.
//! - [`request_record`]: where a create's request holds its one record.
//! - [`created`]: the records a create described as bulk or long-running makes, and
//!   [`mint_created`] the ids of those that hold none.
//! - [`patterned`], [`bare_id`] and [`held_by_key`]: a key read by a path value tries
//!   the other form of a patterned id (`7` and `folders/7`).
//!
//! How `claude/header-ids-alt-keys` (PR #254) landed, and what that means here:
//!
//! - Fix 1 (a header-only create id) is [`header_ids`]: it mints the id, places the
//!   header and returns the id `write_state` stores the request under, all inside
//!   `complete`. The header name is now read through [`identity_header`]; the
//!   detector moved to `mb_mcm::identity_headers` so tier 0 can restate it. No
//!   `created_id` carrier is needed: the header is placed before the handler sees the
//!   answer.
//! - Fix 5 (a path value naming a record by either of two members) is
//!   [`alternate_members`]/[`by_alternate_member`]: a separate lookup, after
//!   `read_context`'s field lists miss, whose members come from the target path
//!   parameter's name or description (`templateIdOrAlias`). They are per path
//!   parameter, not per type, so tier 0 cannot restate them and emits no
//!   `identity.alternates`. That lookup stays as it merged (it takes one holding
//!   record only, unlike `by_path_fields`' first match, so folding it in would change
//!   answers); described alternates are tried after it, as one more `by_path_fields`
//!   attempt ([`extra_fields`]). An author can add members but cannot remove a
//!   guessed one.

use mb_engine::headers::{IdentityHeader as Header, IdentityHeaderKind};
use mb_mcm::world::{IdentityHeader, Pointer};

use super::*;

/// The header `op`'s answer of `status` names a created record's id in: the one the
/// description names where it differs from tier 0's (`None` for `false`), else the
/// detector's. A described header the answer does not declare is a string.
pub(super) fn identity_header(rt: &MockRuntime, op: &Operation, status: u16) -> Option<Header> {
    match rt.described().changed_identity_header(&op.id) {
        None => mb_engine::headers::identity_header(op, status),
        Some(IdentityHeader::Off) => None,
        Some(IdentityHeader::Named(name)) => {
            let declared = op
                .output_for_status(status)
                .and_then(|o| o.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)));
            let (name, schema) = match declared {
                Some((n, s)) => (n.clone(), s.clone()),
                None => (name.clone(), serde_json::json!({"type": "string"})),
            };
            let kind = match name.eq_ignore_ascii_case("location") {
                true => IdentityHeaderKind::Location,
                false => IdentityHeaderKind::Id,
            };
            Some(Header { name, kind, schema })
        }
    }
}

/// The members of type `t`'s records the description says a path value may also name
/// one by; none where it says nothing (tier 0 emits none, see the module doc).
pub(super) fn alternates(rt: &MockRuntime, t: &str) -> Vec<String> {
    rt.described()
        .described_alternates(t)
        .map(<[String]>::to_vec)
        .unwrap_or_default()
}

/// The members a lookup site tries after its own (`already`) miss: type `t`'s identity
/// member where the description changed it (a record stored before the change is keyed
/// by the old one, so only a scan by the new one finds it), then the described
/// [`alternates`]. Empty with no world and under tier 0.
pub(super) fn extra_fields(rt: &MockRuntime, t: &str, already: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let member = rt
        .described()
        .changed_identity_member(t)
        .map(str::to_string);
    for field in member.into_iter().chain(alternates(rt, t)) {
        if !already.contains(&field.as_str()) && !out.contains(&field) {
            out.push(field);
        }
    }
    out
}

/// One more `by_path_fields` attempt with [`extra_fields`], when there are any.
pub(super) async fn by_extra_fields(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    (already, value): (&[&str], &str),
    owner: Option<&Owner>,
) -> Option<StoredResource> {
    let extra = extra_fields(state.rt, t, already);
    if extra.is_empty() {
        return None;
    }
    let fields: Vec<&str> = extra.iter().map(String::as_str).collect();
    by_path_fields(state, session, t, &fields, value, owner).await
}

/// The value at `op`'s described `request.record` in `body`, else the whole body (no
/// pointer described, or one this body does not hold).
pub(super) fn request_record<'a>(rt: &MockRuntime, op: &Operation, body: &'a Value) -> &'a Value {
    let Some(pointer) = rt
        .described()
        .op(&op.id)
        .and_then(|d| d.request.as_ref())
        .and_then(|r| r.record.as_ref())
    else {
        return body;
    };
    match body.pointer(pointer.as_str()) {
        Some(record) => record,
        None => {
            tracing::info!(
                mock = %rt.name,
                operation = %op.id,
                pointer = %pointer.as_str(),
                "request.record does not resolve in this body: the whole body is the record"
            );
            body
        }
    }
}

/// The member type `t`'s records are keyed by on `op`: its identity's field, else the
/// type's effective identity member, else `id`.
pub(super) fn identity_member<'a>(rt: &'a MockRuntime, op: &'a Operation, t: &str) -> &'a str {
    op.identity
        .as_ref()
        .map(|i| i.field.as_str())
        .or_else(|| {
            rt.described()
                .type_desc(t)?
                .identity
                .as_ref()?
                .member
                .as_deref()
        })
        .unwrap_or("id")
}

/// The records a create described as bulk or long-running makes, each an object: the
/// items at `answer.records`, else those at `request.records` (`minted`, the request's
/// items with the ids [`mint_created`] gave them, when it gave any), else the
/// `long_running.result` member of the answer. Empty when the description says none of
/// these, or they hold no object; the create's one record (`answer.record`, which
/// [`mb_mcm::world::apply`] wrote into the bundle, or a header-named one) is then found
/// as before.
pub(super) fn created(
    rt: &MockRuntime,
    op: &Operation,
    (request, minted): (Option<&Value>, &[Value]),
    answer: Option<&Value>,
) -> Vec<Value> {
    let Some(desc) = rt.described().op(&op.id) else {
        return Vec::new();
    };
    let answered = desc.answer.as_ref();
    let from_answer = items(answer, answered.and_then(|a| a.records.as_ref()));
    if !from_answer.is_empty() {
        return from_answer;
    }
    if !minted.is_empty() {
        return minted.to_vec();
    }
    let from_request = items(
        request,
        desc.request.as_ref().and_then(|r| r.records.as_ref()),
    );
    if !from_request.is_empty() {
        return from_request;
    }
    answered
        .and_then(|a| a.long_running.as_ref())
        .zip(answer)
        .and_then(|(lr, answer)| mb_mcm::member_at(answer, &lr.result))
        .filter(|r| r.is_object())
        .map(|r| vec![r.clone()])
        .unwrap_or_default()
}

/// A bulk create's `request.records` pointer, and its `answer.records` one when the
/// description says it too.
fn bulk_pointers<'a>(
    rt: &'a MockRuntime,
    op: &Operation,
) -> Option<(&'a Pointer, Option<&'a Pointer>)> {
    let desc = rt.described().op(&op.id)?;
    let sent = desc.request.as_ref()?.records.as_ref()?;
    let answered = desc.answer.as_ref().and_then(|a| a.records.as_ref());
    Some((sent, answered))
}

/// The records `call`, a bulk create's (see [`bulk_pointers`]), sends at
/// `request.records`; empty when it sends none.
fn sent_records(rt: &MockRuntime, op: &Operation, call: &OperationCall) -> Vec<Value> {
    let Some((sent, _)) = bulk_pointers(rt, op) else {
        return Vec::new();
    };
    items(
        call.body.as_ref().map(|b| request_record(rt, op, b)),
        Some(sent),
    )
}

/// A bulk create described with both `request.records` and `answer.records` answers
/// one record per record its request sends, in order: the answer's array is cut to as
/// many object items as the request sends, and one it lacks is a copy of its last
/// object item without an identity (an empty object when it holds none), for
/// [`mint_created`] to give one and [`set_each`] to set. Nothing changes when the
/// request sends no record or the answer holds no array there.
pub(super) fn align_bulk(
    rt: &MockRuntime,
    (op, t, call): (&Operation, &str, &OperationCall),
    generated: &mut GeneratedResponse,
) {
    let Some((_, Some(at))) = bulk_pointers(rt, op) else {
        return;
    };
    let wanted = sent_records(rt, op, call).len();
    if wanted == 0 {
        return;
    }
    let field = identity_member(rt, op, t);
    let Some(Value::Array(answered)) = generated
        .body
        .as_mut()
        .and_then(|b| b.pointer_mut(at.as_str()))
    else {
        return;
    };
    answered.retain(Value::is_object);
    answered.truncate(wanted);
    let mut copy = answered
        .last()
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if let Value::Object(members) = &mut copy {
        members.remove(field);
    }
    while answered.len() < wanted {
        answered.push(copy.clone());
    }
}

/// Applies a bulk create's described initial state and `set` to each record it makes,
/// as a create's on its one record: the `set` evaluated once per record,
/// `request.body` the record its request sends at the same place. The records are the
/// answer's at `answer.records` ([`align_bulk`] made them one per record sent), else
/// `minted`, the request's own ([`mint_created`]), which are stored as they are.
///
/// A `set` that gives a record another identity is checked as a single create's is:
/// an identity the session holds in the create's collection answers that record, as
/// an idempotent create, and one held elsewhere, or given to an earlier record of the
/// same call (which that record keeps), is answered as set. Answers the places, among
/// the object records made, of those not to be stored.
pub(super) async fn set_each(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: &str,
    (generated, minted): (&mut GeneratedResponse, &mut [Value]),
) -> Vec<usize> {
    let rt = state.rt;
    let op = plan.op;
    let mut unstored = Vec::new();
    let Some((_, answered_at)) = bulk_pointers(rt, op) else {
        return unstored;
    };
    let sent = sent_records(rt, op, call);
    if sent.is_empty() {
        return unstored;
    }
    let sets = super::exprs::sets(rt, &op.id);
    let field = identity_member(rt, op, t);
    let answered = answered_at
        .zip(generated.body.as_mut())
        .and_then(|(at, body)| body.pointer_mut(at.as_str()))
        .and_then(Value::as_array_mut)
        .filter(|a| a.iter().any(Value::is_object));
    let made: &mut [Value] = match answered {
        Some(answered) => answered,
        None => minted,
    };
    // Each identity a record of this call is stored under, with that record.
    let mut kept: Vec<(String, Value)> = Vec::new();
    let records = made.iter_mut().filter(|r| r.is_object());
    for (at, (record, request)) in records.zip(sent).enumerate() {
        super::lifecycle::initial(rt, t, record, Some(&request));
        let before = mb_mcm::member_at(record, field).cloned();
        if sets {
            let one = OperationCall {
                body: Some(request),
                ..call.clone()
            };
            plan.exprs.forget_set();
            super::transitions::apply_on_create(state, (plan, &one), record, false).await;
        }
        let id = mb_mcm::member_at(record, field).and_then(id_string);
        let moved = mb_mcm::member_at(record, field) != before.as_ref();
        if let Some(id) = id.filter(|_| moved) {
            if let Some((_, earlier)) = kept.iter().find(|(k, _)| *k == id) {
                tracing::info!(
                    mock = %rt.name,
                    operation = %op.id,
                    id = %log_capped(&id),
                    "a bulk create's set repeats an identity an earlier record of the call has: not stored"
                );
                *record = earlier.clone();
                unstored.push(at);
                continue;
            }
            if let Some(held) = state.get(&plan.session, t, &id).await {
                tracing::info!(
                    mock = %rt.name,
                    operation = %op.id,
                    id = %log_capped(&id),
                    "a bulk create's set names a held record: not stored over it"
                );
                if super::owners::in_create_collection(rt, &plan.session, op, call, &held) {
                    *record = held.data;
                }
                unstored.push(at);
                continue;
            }
        }
        if let Some(id) = mb_mcm::member_at(record, field).and_then(id_string) {
            kept.push((id, record.clone()));
        }
    }
    // What a later step of the call evaluates reads the call's whole request again.
    plan.exprs.forget_set();
    unstored
}

/// The object items of the array at `at` in `doc`.
fn items(doc: Option<&Value>, at: Option<&Pointer>) -> Vec<Value> {
    let Some(Value::Array(items)) = doc.zip(at).and_then(|(d, p)| d.pointer(p.as_str())) else {
        return Vec::new();
    };
    items.iter().filter(|i| i.is_object()).cloned().collect()
}

/// How many ids are drawn for one record before giving up on one the session does not
/// hold.
const MINT_ATTEMPTS: u32 = 8;

/// Gives each record [`created`] finds for `op`'s create (of type `t`) that holds no
/// identity value one, drawn as a header-named create's id is
/// ([`header_ids::draw_record_id`]: as the type declares the member, in its pattern),
/// one the session holds no record of the type under, the request does not carry and
/// no other record of this create was given. An item of `answer.records` or the
/// `long_running.result` gets it in the answer, where the client reads it; the items
/// of `request.records`, when they are the records made, are returned with theirs, for
/// [`write_state`] to store (empty otherwise). A record no id can be drawn for is left
/// as it is, and not stored.
pub(super) async fn mint_created(
    state: &mut StateAccess<'_>,
    (session, op, t, call): (&SessionKey, &Operation, &str, &OperationCall),
    generated: &mut GeneratedResponse,
) -> Vec<Value> {
    let rt = state.rt;
    let Some(desc) = rt.described().op(&op.id) else {
        return Vec::new();
    };
    let field = identity_member(rt, op, t);
    let lacks = |record: &Value| mb_mcm::member_at(record, field).is_none_or(Value::is_null);
    let carried = request_values(op, call);
    let mut given: Vec<String> = Vec::new();
    let answered = desc.answer.as_ref();
    // The answer's records: each gets its id where the answer holds it.
    let at = answered.and_then(|a| a.records.as_ref());
    if !items(generated.body.as_ref(), at).is_empty() {
        let Some(Value::Array(records)) = at
            .zip(generated.body.as_mut())
            .and_then(|(p, body)| body.pointer_mut(p.as_str()))
        else {
            return Vec::new();
        };
        for record in records.iter_mut().filter(|r| r.is_object() && lacks(r)) {
            if let Some(id) =
                mint_one(state, (session, call), (t, field), &carried, &mut given).await
            {
                mb_mcm::set_member_at(record, field, id);
            }
        }
        return Vec::new();
    }
    // The request's records: each is stored with its id.
    let request = call.body.as_ref().map(|b| request_record(rt, op, b));
    let mut sent = items(
        request,
        desc.request.as_ref().and_then(|r| r.records.as_ref()),
    );
    if !sent.is_empty() {
        for record in sent.iter_mut().filter(|r| lacks(r)) {
            if let Some(id) =
                mint_one(state, (session, call), (t, field), &carried, &mut given).await
            {
                mb_mcm::set_member_at(record, field, id);
            }
        }
        return sent;
    }
    // A long-running create's result.
    let Some(lr) = answered.and_then(|a| a.long_running.as_ref()) else {
        return Vec::new();
    };
    let lacking = generated
        .body
        .as_ref()
        .and_then(|body| mb_mcm::member_at(body, &lr.result))
        .is_some_and(|r| r.is_object() && lacks(r));
    if lacking
        && let Some(id) = mint_one(state, (session, call), (t, field), &carried, &mut given).await
        && let Some(result) = generated
            .body
            .as_mut()
            .and_then(|body| mb_mcm::member_at_mut(body, &lr.result))
    {
        mb_mcm::set_member_at(result, field, id);
    }
    Vec::new()
}

/// One id for a record of type `t` keyed by `field` (see [`mint_created`]), noted in
/// `given`; `None` when none could be drawn.
pub(super) async fn mint_one(
    state: &mut StateAccess<'_>,
    (session, call): (&SessionKey, &OperationCall),
    (t, field): (&str, &str),
    carried: &HashSet<String>,
    given: &mut Vec<String>,
) -> Option<Value> {
    let seed = header_ids::MintSeed::new(state.rt, session, call);
    let shown = header_ids::record_id_examples(state.rt, t, field);
    let shape = match header_ids::record_id_shape(state.rt, t, field) {
        Some(shape) => Some(shape),
        None => header_ids::session_id_shape(state, session, t, field).await,
    };
    for _ in 0..MINT_ATTEMPTS {
        let Some(id) =
            header_ids::draw_record_id(state.rt, seed, (t, field), (shape.as_ref(), &shown))
        else {
            continue;
        };
        let Some(text) = id_string(&id) else {
            continue;
        };
        if carried.contains(&text)
            || given.contains(&text)
            || state.get(session, t, &text).await.is_some()
        {
            continue;
        }
        given.push(text);
        return Some(id);
    }
    None
}

/// Settles a create's answer as the description says before it is stored: a
/// long-running answer whose `done` member is a boolean is done, since the mock holds
/// its result already.
pub(super) fn settle_create(rt: &MockRuntime, op: &Operation, generated: &mut GeneratedResponse) {
    let Some(lr) = rt
        .described()
        .op(&op.id)
        .and_then(|d| d.answer.as_ref())
        .and_then(|a| a.long_running.as_ref())
    else {
        return;
    };
    let Some(body) = generated.body.as_mut() else {
        return;
    };
    if mb_mcm::member_at(body, &lr.done).is_some_and(Value::is_boolean) {
        mb_mcm::set_member_at(body, &lr.done, Value::Bool(true));
    }
}

/// Type `t`'s effective identity pattern (`folders/{id}`), when it has one.
pub(super) fn pattern<'a>(rt: &'a MockRuntime, t: &str) -> Option<&'a str> {
    rt.described().identity_pattern(t)
}

/// `id` in `pattern`'s form: `folders/{id}` with `7` is `folders/7`.
pub(super) fn patterned(pattern: &str, id: &str) -> String {
    pattern.replacen("{id}", id, 1)
}

/// The bare id `value` holds in `pattern`'s form (`7` of `folders/7`); `None` when it is
/// not in that form or holds nothing there.
pub(super) fn bare_id(pattern: &str, value: &str) -> Option<String> {
    let (prefix, suffix) = pattern.split_once("{id}")?;
    let bare = value.strip_prefix(prefix)?.strip_suffix(suffix)?;
    (!bare.is_empty()).then(|| bare.to_string())
}

/// `id` in type `t`'s identity pattern, when the type has one and `id` is not in it.
pub(super) fn in_pattern(rt: &MockRuntime, t: &str, id: &str) -> Option<String> {
    let pattern = pattern(rt, t)?;
    bare_id(pattern, id)
        .is_none()
        .then(|| patterned(pattern, id))
}

/// The record of type `t` keyed by the path value `id`: under `id` itself, else, when
/// the type's identity has a pattern, under its other form (the bare id of a patterned
/// value, the patterned form of a bare one). The hit carries the key it is stored
/// under. With no pattern this is the one key read.
pub(super) async fn held_by_key(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    id: &str,
) -> Option<StoredResource> {
    if let Some(hit) = state.get(session, t, id).await {
        return Some(hit);
    }
    let pattern = pattern(state.rt, t)?.to_string();
    let other = bare_id(&pattern, id).unwrap_or_else(|| patterned(&pattern, id));
    if other == id {
        return None;
    }
    state.get(session, t, &other).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_patterned_id_and_its_bare_form_convert_both_ways() {
        assert_eq!(patterned("folders/{id}", "7"), "folders/7");
        assert_eq!(bare_id("folders/{id}", "folders/7").as_deref(), Some("7"));
        assert_eq!(bare_id("folders/{id}", "7"), None);
        assert_eq!(bare_id("folders/{id}", "folders/"), None);
        assert_eq!(bare_id("f-{id}.v1", "f-a.v1").as_deref(), Some("a"));
    }
}
