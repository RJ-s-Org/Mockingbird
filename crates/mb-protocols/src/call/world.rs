//! What a generation is shown of the session's world beyond what its route names.
//!
//! A request can refer to a record by a value under any member (Slack's `timestamp`
//! and `thread_ts`), which no rule by name resolves. So a body or query scalar that
//! could be an id is looked up as one, in the types an effect can name, and a hit joins
//! the related resources: the model sees it and may state a change, which goes through
//! the effects path as any other. Only a record's stored id, or a path field
//! [`PathAliases`] has seen it hold, counts as a match, so what a record merely says
//! elsewhere names nothing. A value must hold a digit or a separator, so a common word
//! (`closed`) is no reference, and a word-only id is not found this way; a date can match.
//! A hit is scrubbed for the prompt, and an effect goes on the stored copy. A call pays at
//! most [`MAX_LOOKUPS`] reads, and none when no value could be an id or when its answer
//! needs no generation.
//!
//! An answer of a `GET` route with no parameters that returns a single object
//! (`/users/@me`, Slack's `auth.test`) says who the caller is, and is shown to later
//! generations, its own route's too, as what the API said and may since have changed, so
//! a `/me` keeps its identity when a write clears the cache. Deleting the record it is
//! about ends it. Indirect changes are not tracked. It is kept under the reserved type of the state
//! store ([`mb_state::INTERNAL_TYPE`]), like the write memory's documents: sessions, TTLs
//! and resets clear it, and it is not exported, counted or on a change feed. A contract
//! with no such route pays no read for it.
//!
//! Neither moves or changes a record. HTTP-bound operations with an LLM and consistency
//! on only (see [`write_memory::applies`]): the prompts of GraphQL, gRPC and SOAP are as
//! they were.

use mb_state::INTERNAL_TYPE;

use super::*;

/// Shortest string that could be an id, in bytes. A number must have as many digits.
const MIN_ID_LEN: usize = 6;
const MIN_ID_NUMBER: u64 = 100_000;
/// Longest string that could be an id, in bytes.
const MAX_ID_LEN: usize = 128;
/// Values of one request looked up, top-level ones first.
const MAX_VALUES: usize = 8;
/// How deep a request is walked for them.
const MAX_VALUE_DEPTH: usize = 4;
/// Store reads per call for them.
const MAX_LOOKUPS: usize = 32;
/// Records they may add to a prompt.
const MAX_MATCHES: usize = 6;

/// Where the answers are kept: `#` starts no `<type>/<id>` of the write memory.
const ANSWERS_KEY: &str = "#answers";
/// Answers kept per session, the newest.
const MAX_ANSWERS: usize = 3;
/// Largest answer kept, as serialized JSON. A larger one is not kept at all: a cut one
/// is no answer, and it is not likely to say who the caller is.
const MAX_ANSWER_BYTES: usize = 2 * 1024;

/// `value` as an id it could be: a string of an id's length with no space in it and a
/// digit or a separator (`thread_ts`'s value, `PROJ-1`, not `closed`), or a whole number
/// too large to be a count.
fn id_like_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text)
            if (MIN_ID_LEN..=MAX_ID_LEN).contains(&text.len())
                && !text.chars().any(char::is_whitespace)
                && text.chars().any(|c| !c.is_alphabetic()) =>
        {
            Some(text.clone())
        }
        Value::Number(number) => number
            .as_u64()
            .filter(|n| *n >= MIN_ID_NUMBER)
            .map(|n| n.to_string()),
        _ => None,
    }
}

/// Adds what could be an id in `value`, under member `name`, to `out`: an object's own
/// scalars before what it nests. What a credential's member holds is not looked up, but
/// for a word as often an identity (`key`, `sid`), as a lookup shows nothing of the value.
fn id_values(rt: &MockRuntime, name: &str, value: &Value, depth: usize, out: &mut Vec<String>) {
    let credential = write_memory::is_credential_name(rt, name)
        && !mb_mcm::credentials::may_be_an_identity(name);
    if depth > MAX_VALUE_DEPTH || credential {
        return;
    }
    match value {
        Value::Object(members) => {
            let (nested, scalars): (Vec<_>, Vec<_>) = members
                .iter()
                .partition(|(_, inner)| inner.is_object() || inner.is_array());
            for (key, inner) in scalars.into_iter().chain(nested) {
                id_values(rt, key, inner, depth + 1, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                id_values(rt, name, item, depth + 1, out);
            }
        }
        scalar => {
            if out.len() < MAX_VALUES
                && let Some(id) = id_like_value(scalar)
                && !out.contains(&id)
            {
                out.push(id);
            }
        }
    }
}

/// The types to look in: what the session last wrote, then the contract's order.
fn types_to_search(rt: &MockRuntime, session: &SessionKey) -> Vec<String> {
    let mut types = rt.written_types().of(session);
    let all = &rt.index().effect_types;
    types.retain(|t| all.contains(t));
    let rest: Vec<String> = all.iter().filter(|t| !types.contains(t)).cloned().collect();
    types.extend(rest);
    types
}

/// Adds to `out`, after what the request names by member, the records a body or query
/// value is the identity of (see the module's doc), each type a value is found in.
pub(super) async fn named_by_value(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target: Option<&StoredResource>,
    followed: &mut Followed,
    out: &mut Vec<StoredResource>,
) {
    let rt = state.rt;
    if !write_memory::applies(rt, plan) {
        return;
    }
    let mut values = Vec::new();
    for (name, value) in &call.query {
        id_values(rt, name, value, 0, &mut values);
    }
    if let Some(body) = &call.body {
        id_values(rt, "", body, 0, &mut values);
    }
    if values.is_empty() {
        return;
    }
    let session = &plan.session;
    let index = rt.index();
    let mut lookups = 0;
    let mut found: Vec<StoredResource> = Vec::new();
    'types: for t in types_to_search(rt, session) {
        for value in &values {
            if lookups >= MAX_LOOKUPS || found.len() >= MAX_MATCHES {
                break 'types;
            }
            let shown = |r: &StoredResource| r.resource_type == t && r.id == *value;
            if target.is_some_and(shown) || out[..followed.named].iter().any(shown) {
                continue;
            }
            lookups += 1;
            let mut hit = state.get(session, &t, value).await;
            // Stored under another id, and known to hold this one in a path field.
            for field in index.path_fields(&t) {
                if hit.is_some() || lookups >= MAX_LOOKUPS {
                    break;
                }
                let Some(id) = rt.path_aliases().id_of(session, &t, field, value) else {
                    continue;
                };
                lookups += 1;
                hit = state
                    .get(session, &t, &id)
                    .await
                    .filter(|r| r.data.get(field).and_then(id_string).as_ref() == Some(value));
            }
            if let Some(hit) = hit
                && !target.is_some_and(|r| r.resource_type == t && r.id == hit.id)
                && !holds(&out[..followed.named], &hit)
                && !holds(&found, &hit)
            {
                let data = write_memory::scrubbed_for_prompt(rt, plan, &t, &hit.data);
                found.push(StoredResource { data, ..hit });
            }
        }
    }
    // Listed already, they move up among what the request names.
    out.retain(|r| !holds(&found, r));
    let named = followed.named;
    followed.named += found.len();
    out.splice(named..named, found);
}

/// What routes with no parameters answered earlier in this session, oldest first.
pub(super) async fn recall_answers(state: &mut StateAccess<'_>, plan: &CallPlan<'_>) -> Vec<Value> {
    let rt = state.rt;
    if !write_memory::applies(rt, plan) || rt.index().single_reads.is_empty() {
        return Vec::new();
    }
    read_answers(state, &plan.session).await
}

/// Whether a kept answer is that of `path`.
fn of_path(entry: &Value, path: &str) -> bool {
    entry.get("path").and_then(Value::as_str) == Some(path)
}

/// After a 2xx delete of a record: an answer it was the identity of goes, as what it said
/// is known to be over. Any other change it says nothing of is left to its preface.
pub(super) async fn after_delete(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    deleted: Option<&str>,
    generated: &GeneratedResponse,
) {
    let rt = state.rt;
    let (OperationKind::Command(Effect::Delete), Some(deleted), true, true, false) = (
        plan.op.kind,
        deleted,
        (200..300).contains(&generated.status),
        write_memory::applies(rt, plan),
        rt.index().single_reads.is_empty(),
    ) else {
        return;
    };
    let session = &plan.session;
    let held = read_answers(state, session).await;
    let about_it = |entry: &Value| {
        let read = entry
            .get("path")
            .and_then(Value::as_str)
            .and_then(|path| rt.index().single_reads.get(path))
            .and_then(|i| rt.bundle().operations.get(*i));
        let Some((read, answer)) = read.zip(entry.get("answer")) else {
            return false;
        };
        let fields = read.resource_type.as_deref().map_or_else(
            || vec!["id", "number"],
            |t| write_memory::pinned_members(rt, t),
        );
        fields
            .iter()
            .any(|f| answer.get(*f).and_then(id_string).as_deref() == Some(deleted))
    };
    let answers: Vec<Value> = held.iter().filter(|e| !about_it(e)).cloned().collect();
    write_answers(state, session, &held, answers).await;
}

/// Replaces what is kept, when it changed.
async fn write_answers(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    held: &[Value],
    answers: Vec<Value>,
) {
    if answers == held {
        return;
    }
    if answers.is_empty() {
        state.delete(session, INTERNAL_TYPE, ANSWERS_KEY).await;
        return;
    }
    state
        .upsert(
            session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_string(),
                id: ANSWERS_KEY.to_string(),
                data: serde_json::json!({ "answers": answers }),
            },
        )
        .await;
}

async fn read_answers(state: &mut StateAccess<'_>, session: &SessionKey) -> Vec<Value> {
    let doc = state.get(session, INTERNAL_TYPE, ANSWERS_KEY).await;
    doc.and_then(|doc| doc.data.get("answers")?.as_array().cloned())
        .unwrap_or_default()
}

/// After a generated answer on a route with a single read: keeps an answer of that read's
/// schema as the route's, which a command's can be (Slack's `POST auth.test`, or a `PUT`
/// answering the new state). Any other 2xx command on the route may have changed what is
/// kept, which would then be told as true, so it goes.
pub(super) async fn after_answer(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    generated: &GeneratedResponse,
) {
    let rt = state.rt;
    let op = plan.op;
    let Binding::Http { path, .. } = &op.binding else {
        return;
    };
    let Some(read) = rt.index().single_reads.get(path) else {
        return;
    };
    let command = matches!(op.kind, OperationKind::Command(_));
    let usable = write_memory::applies(rt, plan)
        && plain(plan)
        && (200..300).contains(&generated.status)
        && (command || call.query.is_empty())
        && mb_engine::seed::request_seed(call).is_none();
    if !usable {
        return;
    }
    let read = rt.bundle().operations.get(*read);
    let read_schema = read.and_then(|read| read.success_output()?.schema.as_ref());
    let own_type = read.and_then(|read| read.resource_type.as_deref());
    let schema = op
        .output_for_status(generated.status)
        .and_then(|o| o.schema.as_ref());
    let kept = generated
        .body
        .as_ref()
        .filter(|body| {
            body.is_object()
                && generated.source != Source::Fallback
                && schema.is_some()
                && schema == read_schema
        })
        .map(|body| match own_type {
            Some(t) => write_memory::scrubbed_record(rt, plan, t, body, schema, None),
            None => write_memory::scrubbed_by(rt, plan, body, schema, None),
        })
        .filter(|body| body.to_string().len() <= MAX_ANSWER_BYTES);
    // A read that says nothing new leaves what is kept: a faker standing in for the LLM.
    // One too large to keep has outdated it.
    let said = generated.source != Source::Fallback
        && generated.body.as_ref().is_some_and(Value::is_object);
    if !command && !said {
        return;
    }
    let session = &plan.session;
    let held = read_answers(state, session).await;
    let mut answers: Vec<Value> = held.iter().filter(|e| !of_path(e, path)).cloned().collect();
    if let Some(answer) = kept {
        answers.push(serde_json::json!({"path": path, "answer": answer}));
    }
    let excess = answers.len().saturating_sub(MAX_ANSWERS);
    answers.drain(..excess);
    write_answers(state, session, &held, answers).await;
}
