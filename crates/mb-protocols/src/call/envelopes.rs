//! What an API answered around its records, for a stored record to be answered the same.
//!
//! A read whose answer holds its record in an envelope (Cal.com's `{status, data}`,
//! Pipedrive's `{success, data}`) is served from state with the envelope's other
//! required members filled in (see [`mb_engine::Engine::held_answer`]), and generated
//! when nothing says what one of them is. What the schema fixes is not always what the
//! API says: Pipedrive's `success` is a plain boolean. So a 2xx answer that held a
//! record (an object) in an envelope keeps what the envelope's other required members
//! said, per session, under the envelope's shape ([`shape`]: the record's type, its
//! member and the other members it requires):
//!
//! - for the operation that answered, all of them: a read answers again what it
//!   answered before. Its strings too (a `message`, a `request_id`), which a read of
//!   another record of the operation then answers as well: what one call said is
//!   replayed across the operation's records;
//! - for every other operation of the shape, only a string or `enum` value that the
//!   reading operation's own schema names at that member (its `enum` or `const`): a
//!   create's `status: "success"` is what a Cal.com read says too, as its `status`
//!   enumerates it. A number or a free boolean (a create's `code: 201`, `total`,
//!   `created: true`, Pipedrive's `success`) and a free string (`message: "Deal
//!   created"`) stay with the operation that answered them, so a Pipedrive read is
//!   generated the first time in a session and answered from state after.
//!
//! Nothing is kept of an answer holding no record (`{success: false, data: null}`) or
//! of a faker stand-in, which say nothing of what the API answers around a record, nor
//! of an answer whose local patch replaced or filled the record's member or changed one
//! of the envelope's other members (see [`mb_engine::PatchedMembers`]). A patch inside
//! the record (a timestamp that broke its format) leaves the rest as the model said it. Only small values are kept ([`MAX_VALUE_BYTES`]),
//! never the record, and nothing [`write_memory::redact_credentials`] would hide, at
//! any depth. A session keeps the newest [`MAX_ENTRIES`] entries.
//!
//! It is kept in the state store under its reserved type ([`mb_state::INTERNAL_TYPE`]),
//! like the write memory's documents, so every backend (memory, SQL, Redis) and every
//! node sees the same: sessions, TTLs and resets clear it, and it is not exported,
//! counted or on a change feed. An envelope requiring nothing beside its record
//! (`{payment}`) costs nothing. Otherwise a write reads the document, and writes it
//! only when what is kept changes; a stored read of an enveloped record reads it once.
//! Two writes at once can each read the document before the other writes it, so one
//! entry can be lost: this is a memory, and a lost entry costs at most a generated read.
//! With consistency off, nothing is kept or read.

use mb_state::INTERNAL_TYPE;

use super::*;

/// Where it is kept: `#` starts no `<type>/<id>` of the write memory.
const ENVELOPES_KEY: &str = "#envelopes";
/// Entries kept per session, the newest.
const MAX_ENTRIES: usize = 16;
/// Largest member value kept, as serialized JSON.
const MAX_VALUE_BYTES: usize = 256;
/// The operation an entry every operation of its shape shares is under.
const SHARED: &str = "*";

/// An envelope's shape and the members it requires beside its record.
struct Shape {
    key: String,
    member: String,
    siblings: Vec<String>,
}

/// The shape of `op`'s envelope: the record's type, the member holding it and the
/// other members the success schema requires, sorted, so that a create and a read
/// answering `{success, data}` share it. `None` when its answer holds no record in an
/// envelope, or the envelope requires nothing else.
fn shape(rt: &MockRuntime, op: &Operation) -> Option<Shape> {
    if !rt.consistency {
        return None;
    }
    let t = op.resource_type.as_deref()?;
    let member = op.identity.as_ref()?.envelope_member()?;
    let schema = op.success_output()?.schema.as_ref()?;
    let mut siblings: Vec<String> = mb_engine::consistency::required_members(rt.bundle(), schema)
        .into_iter()
        .filter(|name| name != member)
        .collect();
    if siblings.is_empty() {
        return None;
    }
    siblings.sort();
    Some(Shape {
        key: format!("{t} {member} {}", siblings.join(",")),
        member: member.to_owned(),
        siblings,
    })
}

/// Whether anything is kept for `op`'s envelope: its answer holds its record in one
/// that requires other members.
pub(super) fn applies(rt: &MockRuntime, op: &Operation) -> bool {
    shape(rt, op).is_some()
}

/// The key of `shape`'s entry for operation `op_id`.
fn entry_key(op_id: &str, shape: &Shape) -> String {
    format!("{op_id} {}", shape.key)
}

/// After a 2xx answer holding its record in an envelope: keeps what the envelope's
/// other required members said (see the module's documentation).
pub(super) async fn remember(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    answer: &Value,
    generated: &GeneratedResponse,
) {
    let rt = state.rt;
    let op = plan.op;
    let (true, false, Some(shape)) = (
        (200..300).contains(&generated.status),
        generated.source == Source::Fallback,
        shape(rt, op),
    ) else {
        return;
    };
    if generated.source == Source::Patched && !patch_spares(generated, &shape) {
        return;
    }
    let holds_a_record = op
        .identity
        .as_ref()
        .and_then(|i| i.record_in(answer))
        .is_some_and(Value::is_object);
    let (true, Some(envelope)) = (holds_a_record, answer.as_object()) else {
        return;
    };
    let schema = op.success_output().and_then(|o| o.schema.as_ref());
    let said: Map<String, Value> = shape
        .siblings
        .iter()
        .filter_map(|name| Some((name.clone(), envelope.get(name)?.clone())))
        .filter(|(_, value)| value.to_string().len() <= MAX_VALUE_BYTES)
        .collect();
    // What redaction would change is a credential's, or holds one.
    let mut redacted = Value::Object(said.clone());
    write_memory::redact_credentials(rt, &mut redacted, schema, None);
    let own: Map<String, Value> = said
        .into_iter()
        .filter(|(name, value)| redacted.get(name) == Some(value))
        .collect();
    let shared: Map<String, Value> = own
        .iter()
        .filter(|(name, value)| may_share(rt, schema, name, value))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let mut entries = vec![(entry_key(&op.id, &shape), own)];
    if !shared.is_empty() {
        entries.push((entry_key(SHARED, &shape), shared));
    }
    entries.retain(|(_, members)| !members.is_empty());
    if entries.is_empty() {
        return;
    }
    let held = read(state, &plan.session).await;
    let mut kept: Vec<(String, Map<String, Value>)> = held
        .iter()
        .filter(|(key, _)| !entries.iter().any(|(k, _)| k == key))
        .cloned()
        .collect();
    kept.extend(entries);
    let from = kept.len().saturating_sub(MAX_ENTRIES);
    let kept = kept.split_off(from);
    if kept == held {
        return;
    }
    let entries: Vec<Value> = kept
        .iter()
        .map(|(key, members)| serde_json::json!({"key": key, "members": members}))
        .collect();
    state
        .upsert(
            &plan.session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_string(),
                id: ENVELOPES_KEY.to_string(),
                data: serde_json::json!({ "entries": entries }),
            },
        )
        .await;
}

/// Whether a local patch of `generated` left the envelope as the model said it: it
/// replaced or filled neither the record's member nor another member of `shape`, and
/// changed none of those inside. Inside the record it may have.
fn patch_spares(generated: &GeneratedResponse, shape: &Shape) -> bool {
    let Some(patched) = generated.patched.as_ref() else {
        return false;
    };
    let touches = |name: &String| *name == shape.member || shape.siblings.contains(name);
    !patched.replaced.iter().any(touches)
        && !patched
            .changed_inside
            .iter()
            .any(|name| shape.siblings.contains(name))
}

/// Whether what member `name` said may be told to another operation of its shape, when
/// that operation's schema names it (see [`recall`]): a string, or a value the
/// answering schema enumerates. A number or a free boolean says what one call was.
fn may_share(rt: &MockRuntime, schema: Option<&Value>, name: &str, value: &Value) -> bool {
    value.is_string() || enumerated(rt, schema, name, value)
}

/// Whether `schema` names `value` itself at member `name` (an `enum` or `const`).
fn enumerated(rt: &MockRuntime, schema: Option<&Value>, name: &str, value: &Value) -> bool {
    let bundle = rt.bundle();
    schema
        .and_then(|s| mb_engine::consistency::property_schema(bundle, s, name))
        .is_some_and(|s| mb_engine::consistency::enumerates(bundle, &s, value))
}

/// What the session's answers said of the members `plan`'s envelope requires beside its
/// record: what its own operation answered, over what other operations of the shape
/// shared that the read's own schema names at that member. `None` when nothing is kept,
/// or its answer requires nothing else.
pub(super) async fn recall(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
) -> Option<Map<String, Value>> {
    let shape = shape(state.rt, plan.op)?;
    let held = read(state, &plan.session).await;
    let find = |op_id: &str| {
        let key = entry_key(op_id, &shape);
        held.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, members)| members.clone())
    };
    let own = find(&plan.op.id);
    let schema = plan.op.success_output().and_then(|o| o.schema.as_ref());
    let mut kept = find(SHARED).map(|shared| {
        shared
            .into_iter()
            .filter(|(name, value)| enumerated(state.rt, schema, name, value))
            .collect::<Map<String, Value>>()
    });
    match (&mut kept, own) {
        (Some(kept), Some(own)) => kept.extend(own),
        (None, own) => kept = own,
        (Some(_), None) => {}
    }
    kept.filter(|k| !k.is_empty())
}

/// The entries kept for `session`, oldest first.
async fn read(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
) -> Vec<(String, Map<String, Value>)> {
    let doc = state.get(session, INTERNAL_TYPE, ENVELOPES_KEY).await;
    let entries = doc
        .as_ref()
        .and_then(|doc| doc.data.get("entries")?.as_array().cloned())
        .unwrap_or_default();
    entries
        .into_iter()
        .filter_map(|entry| {
            let key = entry.get("key")?.as_str()?.to_string();
            let members = entry.get("members")?.as_object()?.clone();
            Some((key, members))
        })
        .collect()
}
