//! What an API answered around its records, for a stored record to be answered the same.
//!
//! A read whose answer holds its record in an envelope (Cal.com's `{status, data}`,
//! Pipedrive's `{success, data}`) is served from state with the envelope's other
//! required members filled in (see [`mb_engine::Engine::held_answer`]). What the schema
//! fixes is not always what the API says: Pipedrive's `success` is a plain boolean. So
//! each 2xx answer that held a record in an envelope, and was not a faker stand-in, keeps
//! the envelope's other members, per session, under the record's type and the envelope's
//! shape (its member and the members it requires, see [`shape`]): a create's
//! `{success: true, data}` is what a read of the same shape answers. Only scalars and
//! small values are kept ([`MAX_VALUE_BYTES`], [`MAX_MEMBERS`]), never the record, and
//! nothing a credential's member holds. A session keeps the newest [`MAX_SHAPES`].
//!
//! It is kept in the state store under its reserved type ([`mb_state::INTERNAL_TYPE`]),
//! like the write memory's documents, so every backend (memory, SQL, Redis) and every
//! node sees the same: sessions, TTLs and resets clear it, and it is not exported,
//! counted or on a change feed. A write costs a read of the document, and a second
//! write only when what is kept changes; a stored read of an enveloped record costs one
//! read. With consistency off, nothing is kept or read.

use mb_state::INTERNAL_TYPE;

use super::*;

/// Where it is kept: `#` starts no `<type>/<id>` of the write memory.
const ENVELOPES_KEY: &str = "#envelopes";
/// Envelope shapes kept per session, the newest.
const MAX_SHAPES: usize = 16;
/// Members kept per shape.
const MAX_MEMBERS: usize = 8;
/// Largest member value kept, as serialized JSON.
const MAX_VALUE_BYTES: usize = 256;

/// The key of `op`'s envelope, when its answer holds its record in one: the record's
/// type, the member holding it and the other members the success schema requires,
/// sorted. A create and a read answering `{success, data}` share it.
fn shape(rt: &MockRuntime, op: &Operation) -> Option<String> {
    let t = op.resource_type.as_deref()?;
    let Some(mb_mcm::RecordAt::Member(member)) = op.identity.as_ref()?.record.as_ref() else {
        return None;
    };
    let schema = op.success_output()?.schema.as_ref()?;
    let mut required: Vec<String> = mb_engine::consistency::required_members(rt.bundle(), schema)
        .into_iter()
        .filter(|name| name != member)
        .collect();
    required.sort();
    Some(format!("{t} {member} {}", required.join(",")))
}

/// What of `answer`'s envelope is kept: its members other than the record's, scalars
/// and small values only, and none a credential's.
fn kept_members(rt: &MockRuntime, member: &str, answer: &Value) -> Map<String, Value> {
    let Some(envelope) = answer.as_object() else {
        return Map::new();
    };
    envelope
        .iter()
        .filter(|(name, value)| {
            name.as_str() != member
                && !write_memory::is_credential_name(rt, name)
                && value.to_string().len() <= MAX_VALUE_BYTES
        })
        .take(MAX_MEMBERS)
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// After a 2xx answer holding its record in an envelope: keeps what else the envelope
/// said, for its shape. A faker stand-in for a failed call says nothing of the API.
pub(super) async fn remember(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    answer: &Value,
    generated: &GeneratedResponse,
) {
    let rt = state.rt;
    let op = plan.op;
    let (true, false, Some(key)) = (
        rt.consistency && (200..300).contains(&generated.status),
        generated.source == Source::Fallback,
        shape(rt, op),
    ) else {
        return;
    };
    let Some(mb_mcm::RecordAt::Member(member)) =
        op.identity.as_ref().and_then(|i| i.record.as_ref())
    else {
        return;
    };
    let members = kept_members(rt, member, answer);
    if members.is_empty() {
        return;
    }
    let held = read(state, &plan.session).await;
    if held.iter().any(|(k, v)| *k == key && *v == members) {
        return;
    }
    let mut kept: Vec<(String, Map<String, Value>)> =
        held.into_iter().filter(|(k, _)| *k != key).collect();
    kept.push((key, members));
    let from = kept.len().saturating_sub(MAX_SHAPES);
    let shapes: Vec<Value> = kept[from..]
        .iter()
        .map(|(k, v)| serde_json::json!({"shape": k, "members": v}))
        .collect();
    state
        .upsert(
            &plan.session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_string(),
                id: ENVELOPES_KEY.to_string(),
                data: serde_json::json!({ "shapes": shapes }),
            },
        )
        .await;
}

/// What the session's answers of `op`'s envelope shape said besides the record, when a
/// stored record is to be answered in one.
pub(super) async fn recall(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
) -> Option<Map<String, Value>> {
    let rt = state.rt;
    let key = shape(rt, plan.op).filter(|_| rt.consistency)?;
    read(state, &plan.session)
        .await
        .into_iter()
        .find_map(|(k, members)| (k == key).then_some(members))
}

/// The shapes kept for `session`, oldest first.
async fn read(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
) -> Vec<(String, Map<String, Value>)> {
    let doc = state.get(session, INTERNAL_TYPE, ENVELOPES_KEY).await;
    let shapes = doc
        .as_ref()
        .and_then(|doc| doc.data.get("shapes")?.as_array().cloned())
        .unwrap_or_default();
    shapes
        .into_iter()
        .filter_map(|entry| {
            let key = entry.get("shape")?.as_str()?.to_string();
            let members = entry.get("members")?.as_object()?.clone();
            Some((key, members))
        })
        .collect()
}
