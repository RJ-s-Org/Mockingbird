//! Page cursors the session's answers served, so a prompt may show them.
//!
//! A prompt hides a query parameter named as a credential, which hides Zoom's
//! `next_page_token` too: the model then answers a next page as the first. A parameter
//! that is the cursor of a member of its list's answer
//! ([`mb_engine::prompt::hidden_cursor_pairs`]) is shown only when the value it carries
//! is one an answer of the same operation served at that member earlier in the session,
//! the model's or a token the mock minted ([`pages`]): never a value only the client
//! knows, which may be a credential whatever it is named.
//!
//! After each 2xx answer of such an operation, a hash of the value at each paired member
//! is kept (never the value), with the operation and the parameter, the newest
//! [`MAX_VALUES`] per session, in one document of the store's reserved type
//! ([`mb_state::INTERNAL_TYPE`]): sessions, TTLs and resets clear it. An operation with
//! no such pair, or a call carrying none of its parameters, pays no read. HTTP-bound
//! operations with an LLM and consistency on only (see [`write_memory::applies`]).

use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::*;

/// Where the hashes are kept: `#` starts no `<type>/<id>` of the write memory.
const KEY: &str = "#cursors";
/// Hashes kept per session, the newest.
const MAX_VALUES: usize = 64;

/// What a served cursor is kept and looked up by: the operation, the parameter and the
/// value.
fn hash(op: &str, param: &str, value: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [op, param, value] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// The text `value` holds, when not blank.
fn text(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

async fn read(state: &mut StateAccess<'_>, session: &SessionKey) -> Vec<Value> {
    let doc = state.get(session, INTERNAL_TYPE, KEY).await;
    doc.and_then(|doc| doc.data.get("values")?.as_array().cloned())
        .unwrap_or_default()
}

/// After a 2xx answer `generated` to `call`: keeps what it holds at the members paired
/// with a hidden cursor parameter (see the module's doc).
pub(super) async fn after_answer(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    generated: &GeneratedResponse,
) {
    let rt = state.rt;
    let op = plan.op;
    if !write_memory::applies(rt, plan) || !(200..300).contains(&generated.status) {
        return;
    }
    let Some(body) = generated.body.as_ref() else {
        return;
    };
    let pairs = mb_engine::prompt::hidden_cursor_pairs(rt.bundle(), op);
    let fresh: Vec<String> = pairs
        .iter()
        .filter_map(|(param, path)| {
            let at = path.iter().try_fold(body, |at, name| at.get(name));
            text(at).map(|value| hash(&op.id, param, value))
        })
        .collect();
    if fresh.is_empty() {
        return;
    }
    let session = &plan.session;
    let mut values = read(state, session).await;
    let before = values.len();
    for value in fresh {
        let value = Value::String(value);
        if !values.contains(&value) {
            values.push(value);
        }
    }
    if values.len() == before {
        return;
    }
    let excess = values.len().saturating_sub(MAX_VALUES);
    values.drain(..excess);
    state
        .upsert(
            session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_string(),
                id: KEY.to_string(),
                data: serde_json::json!({ "values": values }),
            },
        )
        .await;
}

/// The hidden cursor parameters `call` carries with a value the session's answers of
/// its operation served at their member (see the module's doc).
pub(super) async fn recall(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Vec<String> {
    let rt = state.rt;
    let op = plan.op;
    if !write_memory::applies(rt, plan) {
        return Vec::new();
    }
    let carried: Vec<(String, String)> = mb_engine::prompt::hidden_cursor_pairs(rt.bundle(), op)
        .into_iter()
        .filter_map(|(param, _)| {
            let value = text(call.query.get(&param))?;
            let hashed = hash(&op.id, &param, value);
            Some((param, hashed))
        })
        .collect();
    if carried.is_empty() {
        return Vec::new();
    }
    let values = read(state, &plan.session).await;
    carried
        .into_iter()
        .filter(|(_, hashed)| values.iter().any(|v| v.as_str() == Some(hashed)))
        .map(|(param, _)| param)
        .collect()
}
