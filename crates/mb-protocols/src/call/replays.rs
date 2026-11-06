//! Idempotent replays of a described idempotency header.
//!
//! An operation whose world description names an idempotency header
//! (`operations.<op>.idempotency.header`) answers a call that repeats a key already
//! answered in the session with that first answer: its status, content type, body and
//! headers, served from state before anything else of the call is read, generated or
//! stored. A repeat with another body still answers as the first call did, as an API
//! keeping idempotency keys does.
//!
//! The answer kept is the call's final one, a success or a refusal ([`keep`]), so a
//! retry of a refused create is refused the same way even after what refused it is
//! gone. A server error, a transient fallback, a scenario's or a forced status's answer
//! is not kept: a retry of it is a new request.
//!
//! Nothing is inferred: a header only the name rule of
//! [`mb_engine::guards::carries_an_idempotency_key`] takes for a key replays nothing,
//! and with no description every call is as before.
//!
//! Keys live under the store's reserved type ([`mb_state::INTERNAL_TYPE`]) as
//! `#replay/<op id>/<sha256 of the header value>`, so every backend and node sees them
//! and resets and TTLs clear them; a session keeps [`MAX_KEYS`], the newest.

use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::*;

/// Keys a session keeps answers for, the newest.
const MAX_KEYS: usize = 1024;
/// The document listing them, oldest first, and the prefix of their ids.
const INDEX: &str = "#replay";
/// Longest header value taken for a key.
const MAX_KEY_CHARS: usize = 1024;

/// The header the world description names as `op`'s idempotency key.
pub(super) fn header<'a>(rt: &'a MockRuntime, op: &Operation) -> Option<&'a str> {
    rt.described()
        .op(&op.id)?
        .idempotency
        .as_ref()
        .map(|h| h.header.as_str())
}

/// The document id of the answer kept for `call`'s key, when `plan`'s operation has a
/// described idempotency header, the call sends it, and its answer may be kept or
/// replayed (state on, no scenario or forced status).
fn key_of(rt: &MockRuntime, plan: &CallPlan<'_>, call: &OperationCall) -> Option<String> {
    if !rt.consistency || !plain(plan) {
        return None;
    }
    let name = header(rt, plan.op)?;
    let value = call
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| match v {
            Value::String(s) => Some(s.trim().to_owned()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .filter(|v| !v.is_empty() && v.chars().count() <= MAX_KEY_CHARS)?;
    let digest = Sha256::digest(value.as_bytes());
    let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("{INDEX}/{}/{hash}", plan.op.id))
}

/// The answer kept for `call`'s idempotency key, when an earlier call of the operation
/// in this session sent the same key (see the module's documentation).
pub(super) async fn replayed(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Option<GeneratedResponse> {
    let rt = state.rt;
    let id = key_of(rt, plan, call)?;
    let kept = state.get(&plan.session, INTERNAL_TYPE, &id).await?;
    let data = kept.data;
    let status = data
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok())?;
    let headers = data
        .get("headers")
        .and_then(Value::as_array)
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|pair| {
                    let name = pair.get(0)?.as_str()?;
                    let value = pair.get(1)?.as_str()?;
                    Some((name.to_owned(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    tracing::info!(
        mock = %rt.name,
        operation = %plan.op.id,
        status,
        "idempotent replay: answered as the first call with this key"
    );
    Some(GeneratedResponse {
        status,
        content_type: data
            .get("content_type")
            .and_then(Value::as_str)
            .map(str::to_owned),
        body: data.get("body").cloned().filter(|b| !b.is_null()),
        source: Source::State,
        usage: Usage::default(),
        fallback_reason: None,
        effects: Vec::new(),
        effect_outcomes: Vec::new(),
        dropped_effects: 0,
        new_record: false,
        patched: None,
        headers,
        world_gaps: Vec::new(),
    })
}

/// Keeps `answer`, `call`'s final answer, for a later call sending the same
/// idempotency key (see the module's documentation).
pub(super) async fn keep(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    answer: &GeneratedResponse,
) {
    let rt = state.rt;
    if answer.status >= 500 || transient_fallback(answer) {
        return;
    }
    let Some(id) = key_of(rt, plan, call) else {
        return;
    };
    let session = &plan.session;
    let fresh = state.get(session, INTERNAL_TYPE, &id).await.is_none();
    let headers: Vec<Value> = answer
        .headers
        .iter()
        .map(|(name, value)| serde_json::json!([name, value]))
        .collect();
    let kept = state
        .upsert(
            session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_owned(),
                id: id.clone(),
                data: serde_json::json!({
                    "status": answer.status,
                    "content_type": answer.content_type,
                    "body": answer.body,
                    "headers": headers,
                }),
            },
        )
        .await;
    if !kept || !fresh {
        return;
    }
    let mut listed: Vec<String> = match state.get(session, INTERNAL_TYPE, INDEX).await {
        Some(doc) => serde_json::from_value(doc.data).unwrap_or_default(),
        None => Vec::new(),
    };
    listed.retain(|held| *held != id);
    listed.push(id);
    let excess = listed.len().saturating_sub(MAX_KEYS);
    for oldest in listed.drain(..excess) {
        state.delete(session, INTERNAL_TYPE, &oldest).await;
    }
    state
        .upsert(
            session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_owned(),
                id: INDEX.to_owned(),
                data: Value::from(listed),
            },
        )
        .await;
}
