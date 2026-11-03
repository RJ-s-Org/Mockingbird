//! Values a mock minted, and the answer that minted them.
//!
//! An exchange answering an `access_token`, or a create answering a `link_token`, hands
//! out a value a later request carries back, and nothing links the two: no record is
//! stored under it, and [`world`] looks up no value under a credential's name. So after
//! a generated 2xx create or action on an HTTP route, each string its answer holds under
//! a member named as a credential (see
//! [`mb_mcm::credentials::hides_returned_in_context`], not `key` or `sid`, which as often
//! identify a record), or shaped like one, is remembered when its request did not carry
//! it: only values the mock generated, never a client's own. What is kept is a hash of
//! each value, never the value, beside the answer and the request that got it, each as
//! context may show it (see [`write_memory::declared_plainly`] and
//! [`write_memory::for_context`]): the values themselves and any secret the client sent
//! are left out.
//!
//! A later request carrying one of those values is shown that answer and its request in
//! `<earlier_origins>`, one hop back and no further, as what the API said then. Only
//! told: no answer has to follow it, and nothing it says changes a record.
//!
//! One document per session, of the reserved type of the state store
//! ([`mb_state::INTERNAL_TYPE`]), with the newest [`MAX_ENTRIES`] answers: sessions, TTLs
//! and resets clear it, and it is not exported, counted or on a change feed. A contract
//! with no create or action answering such a member pays no read for it, and a call
//! carrying no string that could be one pays none either. HTTP-bound operations with an
//! LLM and consistency on only (see [`write_memory::applies`]).

use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::*;

/// Where the entries are kept: `#` starts no `<type>/<id>` of the write memory.
const MINTED_KEY: &str = "#minted";
/// Answers kept per session, the newest.
const MAX_ENTRIES: usize = 8;
/// Values remembered of one answer.
const MAX_VALUES: usize = 8;
/// Values of one request looked up.
const MAX_LOOKED_UP: usize = 16;
/// Answers shown to one generation.
const MAX_SHOWN: usize = 3;
/// Largest entry kept, as serialized JSON. A larger one is not kept: a cut answer is
/// no answer.
const MAX_ENTRY_BYTES: usize = 4 * 1024;
/// Shortest and longest string that could be a minted value, in bytes.
const MIN_VALUE_LEN: usize = 8;
const MAX_VALUE_LEN: usize = 512;
/// How deep an answer, a request or a schema is walked.
const MAX_DEPTH: usize = 8;

/// Whether a member called `name` may hold a value a mock mints for a client to send
/// back.
fn mints_by_name(name: &str) -> bool {
    mb_mcm::credentials::hides_returned_in_context(name)
        && !mb_mcm::credentials::may_be_an_identity(name)
}

/// Whether `op` may mint: a create or an action on an HTTP route whose success answer
/// declares a member [`mints_by_name`], at any depth.
pub(super) fn may_mint(bundle: &mb_mcm::Bundle, op: &Operation) -> bool {
    fn declares(bundle: &mb_mcm::Bundle, schema: Option<&Value>, depth: usize) -> bool {
        if depth > MAX_DEPTH {
            return false;
        }
        let (parts, _) = schema_parts(bundle, schema);
        parts.iter().any(|part| {
            let named = part
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|members| {
                    members.iter().any(|(name, inner)| {
                        mints_by_name(name) || declares(bundle, Some(inner), depth + 1)
                    })
                });
            named || declares(bundle, part.get("items"), depth + 1)
        })
    }
    matches!(op.binding, Binding::Http { .. })
        && matches!(
            op.kind,
            OperationKind::Command(Effect::Create | Effect::Other)
        )
        && declares(
            bundle,
            op.success_output().and_then(|o| o.schema.as_ref()),
            0,
        )
}

/// What a value is kept and looked up by.
fn hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `text` is of a length a minted value could have.
fn could_be_minted(text: &str) -> bool {
    (MIN_VALUE_LEN..=MAX_VALUE_LEN).contains(&text.len()) && !text.chars().any(char::is_whitespace)
}

/// Adds every string of `value` to `out`, at most `max`.
fn strings<'a>(value: &'a Value, depth: usize, max: usize, out: &mut Vec<&'a str>) {
    if depth > MAX_DEPTH || out.len() >= max {
        return;
    }
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => items.iter().for_each(|v| strings(v, depth + 1, max, out)),
        Value::Object(members) => members
            .values()
            .for_each(|v| strings(v, depth + 1, max, out)),
        _ => {}
    }
}

/// The strings `call` sends: its body's, its query's, its path's and its headers'.
fn sent(call: &OperationCall, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    for map in [&call.path_params, &call.query, &call.headers] {
        for value in map.values() {
            strings(value, 0, max, &mut out);
        }
    }
    if let Some(body) = &call.body {
        strings(body, 0, max, &mut out);
    }
    out
}

/// The values `answer` mints (see the module's doc): under a member [`mints_by_name`],
/// or shaped like a credential, of a minted value's length, and not sent by `call`.
fn minted_values(answer: &Value, call: &OperationCall) -> Vec<String> {
    fn visit(answer: &Value, name: &str, depth: usize, out: &mut Vec<String>) {
        if depth > MAX_DEPTH || out.len() >= MAX_VALUES {
            return;
        }
        match answer {
            Value::String(text)
                if could_be_minted(text)
                    && (mints_by_name(name) || mb_mcm::credentials::is_credential_value(text)) =>
            {
                if !out.contains(text) {
                    out.push(text.clone());
                }
            }
            Value::Array(items) => items.iter().for_each(|v| visit(v, name, depth + 1, out)),
            Value::Object(members) => members
                .iter()
                .for_each(|(key, v)| visit(v, key, depth + 1, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    visit(answer, "", 0, &mut out);
    let given = sent(call, usize::MAX);
    out.retain(|value| !given.contains(&value.as_str()));
    out
}

async fn read_entries(state: &mut StateAccess<'_>, session: &SessionKey) -> Vec<Value> {
    let doc = state.get(session, INTERNAL_TYPE, MINTED_KEY).await;
    doc.and_then(|doc| doc.data.get("entries")?.as_array().cloned())
        .unwrap_or_default()
}

/// After a generated 2xx answer: remembers what it minted, if anything (see the module's
/// doc).
pub(super) async fn after_answer(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    generated: &GeneratedResponse,
) {
    let rt = state.rt;
    let op = plan.op;
    let usable = write_memory::applies(rt, plan)
        && rt.index().mints
        && may_mint(rt.bundle(), op)
        && (200..300).contains(&generated.status)
        && !matches!(generated.source, Source::State | Source::Cache);
    let Some(answer) = generated.body.as_ref().filter(|_| usable) else {
        return;
    };
    let values = minted_values(answer, call);
    if values.is_empty() {
        return;
    }
    let t = op.resource_type.as_deref().unwrap_or_default();
    let schema = op
        .output_for_status(generated.status)
        .and_then(|o| o.schema.as_ref());
    let request_schema = op.input.body.as_ref();
    let shown_answer = write_memory::declared_plainly(rt, t, (schema, None), answer, 0)
        .and_then(|a| write_memory::for_context(rt, plan, t, &a, schema, schema));
    let shown_request = call
        .body
        .as_ref()
        .and_then(|b| write_memory::declared_plainly(rt, t, (None, request_schema), b, 0))
        .and_then(|b| write_memory::for_context(rt, plan, t, &b, request_schema, None));
    if shown_answer.is_none() && shown_request.is_none() {
        return;
    }
    let mut entry = Map::new();
    let hashes: Vec<Value> = values.iter().map(|v| Value::from(hash(v))).collect();
    entry.insert("values".into(), Value::Array(hashes));
    entry.insert("operation".into(), Value::from(op.id.as_str()));
    if let Binding::Http { path, .. } = &op.binding {
        entry.insert("path".into(), Value::from(path.as_str()));
    }
    if let Some(request) = shown_request {
        entry.insert("request".into(), request);
    }
    if let Some(answer) = shown_answer {
        entry.insert("answer".into(), answer);
    }
    let entry = Value::Object(entry);
    if entry.to_string().len() > MAX_ENTRY_BYTES {
        return;
    }
    let session = &plan.session;
    let mut entries = read_entries(state, session).await;
    entries.push(entry);
    let excess = entries.len().saturating_sub(MAX_ENTRIES);
    entries.drain(..excess);
    state
        .upsert(
            session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_string(),
                id: MINTED_KEY.to_string(),
                data: serde_json::json!({ "entries": entries }),
            },
        )
        .await;
}

/// What a generation is told of the answers that minted values `call` carries, oldest
/// first, without what they are kept by.
pub(super) async fn recall(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Vec<Value> {
    let rt = state.rt;
    if !write_memory::applies(rt, plan) || !rt.index().mints {
        return Vec::new();
    }
    let mut carried: Vec<String> = Vec::new();
    for text in sent(call, MAX_LOOKED_UP * 4) {
        if carried.len() >= MAX_LOOKED_UP {
            break;
        }
        if !could_be_minted(text) {
            continue;
        }
        let hashed = hash(text);
        if !carried.contains(&hashed) {
            carried.push(hashed);
        }
    }
    if carried.is_empty() {
        return Vec::new();
    }
    let entries = read_entries(state, &plan.session).await;
    let mut shown: Vec<Value> = entries
        .into_iter()
        .filter(|entry| {
            entry
                .get("values")
                .and_then(Value::as_array)
                .is_some_and(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|v| carried.iter().any(|c| c == v))
                })
        })
        .map(|mut entry| {
            if let Value::Object(members) = &mut entry {
                members.remove("values");
            }
            entry
        })
        .collect();
    let excess = shown.len().saturating_sub(MAX_SHOWN);
    shown.drain(..excess);
    shown
}
