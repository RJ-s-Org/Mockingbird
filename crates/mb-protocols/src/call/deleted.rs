//! What a delete leaves: a marker that the record is gone, for what would bring it
//! back. With `unknown_ids: generate` a read of a missing id is generated and stored,
//! as is whatever a later answer mentions, so a delete alone did not stick.
//!
//! After a 2xx delete, a marker for the id goes under the store's reserved type
//! ([`mb_state::INTERNAL_TYPE`]), like the write memory's documents: Redis, SQL and
//! memory all keep it, sessions, TTLs, resets and fixtures clear it, and it is no
//! resource (not exported, counted or seeded, and on no change feed). A read of a
//! marked id answers as `unknown_ids: not_found` does, seeding skips it, and a
//! generated list loses its item when the answer stays valid. A create or an update
//! storing that id clears it. What is stored always answers: a marker only speaks for
//! an id the session does not hold.
//!
//! A marker goes by a hash, as a path's value may be a secret, and is a document of
//! its own, so concurrent deletes lose none. It carries the concrete parent path, id
//! or name alike: removing a user from `/x/1/members/{user_id}` says nothing of
//! `/x/2/members`. So another route to the same record (a flat `/tasks/5` for
//! `/projects/1/tasks/5`) is not marked either. The bound is kept by an index, read, changed and written back: two deletes at the same
//! moment can lose an entry, which only leaves a marker it does not count.
//!
//! An LLM and `consistency: on` only: offline answers, and those with consistency
//! off, are as they were. Not GraphQL, which stores what its answers hold under their
//! own types.

use mb_engine::consistency::list_items_mut;
use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::*;

/// Markers kept per session, the newest.
const MAX_DELETED: usize = 1024;
/// The document listing them, oldest first, and the prefix of their ids. No memory
/// key starts so: those start with a resource type, and a type comes from a path.
const INDEX: &str = "#deleted";

/// The members that count a list when they stand beside it, lower case with no `_`.
const COUNT_NAMES: [&str; 5] = ["total", "count", "totalcount", "totalresults", "totalitems"];

/// Whether this call keeps and honours markers.
pub(super) fn applies(rt: &MockRuntime, plan: &CallPlan<'_>) -> bool {
    rt.consistency
        && plan.engine.has_provider()
        && !matches!(plan.op.binding, Binding::Graphql { .. })
}

fn marker_key(t: &str, scope: Option<&str>, id: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [t, scope.unwrap_or_default(), id] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let hash: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{INDEX}/{hash}")
}

/// The concrete parent of what `op` reads or lists: `/projects/1/tasks` for a route's
/// `/projects/{p}/tasks/{id}` and `/projects/{p}/tasks` alike, with a list's extension
/// left out, as `/tasks/{id}.json` lists at `/tasks.json`. None when the parent has no
/// parameter, or `call` does not give one.
pub(super) fn scope(op: &Operation, call: &OperationCall) -> Option<String> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let segments = path_segments(path);
    let target = target_param(op).map(|param| format!("{{{param}}}"));
    let at = target
        .as_ref()
        .and_then(|param| segments.iter().position(|s| s.contains(param.as_str())));
    let parent = &segments[..at.unwrap_or(segments.len())];
    if !parent.iter().any(|s| s.contains('{')) {
        return None;
    }
    let mut filled = Vec::with_capacity(parent.len());
    for segment in parent {
        let mut text = (*segment).to_string();
        for (name, value) in &call.path_params {
            let param = format!("{{{name}}}");
            if text.contains(&param) {
                text = text.replace(&param, &id_string(value)?);
            }
        }
        if text.contains('{') {
            return None;
        }
        filled.push(text);
    }
    if at.is_none()
        && let Some(last) = filled.last_mut()
        && let Some((stem, _)) = last.rsplit_once('.')
    {
        *last = stem.to_string();
    }
    Some(filled.join("/"))
}

/// The parent a resource of type `t` has from where `call` stands, by its read's
/// route: the outer `None` when `call` does not say, as a mention from elsewhere.
fn scope_of_type(rt: &MockRuntime, t: &str, call: &OperationCall) -> Option<Option<String>> {
    let read = rt.index().by_id_read(t)?;
    let op = rt.bundle().operations.get(read)?;
    let Binding::Http { path, .. } = &op.binding else {
        return Some(None);
    };
    let segments = path_segments(path);
    let parent_has_params = target_param(op).is_some_and(|param| {
        let param = format!("{{{param}}}");
        segments
            .iter()
            .take_while(|s| !s.contains(param.as_str()))
            .any(|s| s.contains('{'))
    });
    match scope(op, call) {
        Some(found) => Some(Some(found)),
        None if parent_has_params => None,
        None => Some(None),
    }
}

/// Marks what a delete of `id` on `op`'s route removed: `id`, and what the path called
/// it, which may be a key its reads go by, both under the route's parent.
pub(super) async fn mark(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    call: &OperationCall,
    t: &str,
    id: &str,
) {
    let parent = scope(op, call);
    let named = target_id(op, call).unwrap_or_else(|| id.to_string());
    let mut keys = vec![marker_key(t, parent.as_deref(), &named)];
    if named != id {
        keys.push(marker_key(t, parent.as_deref(), id));
    }
    for key in &keys {
        let marker = StoredResource {
            resource_type: INTERNAL_TYPE.to_string(),
            id: key.clone(),
            data: serde_json::json!({"deleted": true}),
        };
        state.upsert(session, marker).await;
    }
    let mut listed: Vec<String> = match state.get(session, INTERNAL_TYPE, INDEX).await {
        Some(doc) => serde_json::from_value(doc.data).unwrap_or_default(),
        None => Vec::new(),
    };
    listed.retain(|held| !keys.contains(held));
    listed.extend(keys);
    let excess = listed.len().saturating_sub(MAX_DELETED);
    for oldest in listed.drain(..excess) {
        state.delete(session, INTERNAL_TYPE, &oldest).await;
    }
    let index = StoredResource {
        resource_type: INTERNAL_TYPE.to_string(),
        id: INDEX.to_string(),
        data: Value::from(listed),
    };
    state.upsert(session, index).await;
}

/// Whether `id` of type `t` is marked under `scope`.
pub(super) async fn is_deleted(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    scope: Option<&str>,
    id: &str,
) -> bool {
    state
        .get(session, INTERNAL_TYPE, &marker_key(t, scope, id))
        .await
        .is_some()
}

/// Whether something an answer mentions is marked, where this call's path can say which
/// parent it has.
pub(super) async fn mention_is_deleted(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    id: &str,
) -> bool {
    let rt = state.rt;
    let Some(parent) = state
        .call_params
        .as_ref()
        .and_then(|call| scope_of_type(rt, t, call))
    else {
        return false;
    };
    is_deleted(state, session, t, parent.as_deref(), id).await
}

/// Ends the marker of what was just stored under `id`, under `scope` and where its read
/// would look. The index keeps its entry, which a later delete moves on or the bound
/// takes out.
pub(super) async fn clear(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    scope: Option<&str>,
    id: &str,
) {
    let rt = state.rt;
    let read = state
        .call_params
        .as_ref()
        .and_then(|call| scope_of_type(rt, t, call))
        .flatten();
    state
        .delete(session, INTERNAL_TYPE, &marker_key(t, scope, id))
        .await;
    if read.as_deref() != scope {
        state
            .delete(session, INTERNAL_TYPE, &marker_key(t, read.as_deref(), id))
            .await;
    }
}

/// Takes marked ids out of a generated list answer, each only if the answer stays
/// valid, and corrects the count beside the list that is clearly its own.
pub(super) async fn drop_from_list(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    plan: &CallPlan<'_>,
    t: &str,
    scope: Option<&str>,
    generated: &mut GeneratedResponse,
) {
    let rt = state.rt;
    let op = plan.op;
    let Some(body) = generated.body.as_ref() else {
        return;
    };
    let Some(items) = list_items(body) else {
        return;
    };
    let field = match state.learned_field(t) {
        Some(learned) => learned.to_string(),
        None => item_identity(rt.bundle(), op, body),
    };
    let mut gone = Vec::new();
    for (at, item) in items.iter().enumerate() {
        if let Some(id) = item.get(&field).and_then(id_string)
            && is_deleted(state, session, t, scope, &id).await
        {
            gone.push(at);
        }
    }
    if gone.is_empty() {
        return;
    }
    let was = items.len();
    let mut kept = body.clone();
    let mut removed = 0;
    for at in gone.into_iter().rev() {
        let mut candidate = kept.clone();
        if let Some(list) = list_items_mut(&mut candidate) {
            list.remove(at);
        }
        if plan
            .engine
            .serve_stored(rt.bundle(), op, &candidate, None)
            .is_some()
        {
            kept = candidate;
            removed += 1;
        }
    }
    if removed == 0 {
        return;
    }
    if let Some(counted) = with_count_corrected(rt, op, body, &kept, was, removed)
        && plan
            .engine
            .serve_stored(rt.bundle(), op, &counted, None)
            .is_some()
    {
        kept = counted;
    }
    generated.body = Some(kept);
}

/// `kept` with the count of the list that lost `removed` of its `was` items lowered:
/// the one member beside it that the contract declares an integer, is named as a
/// count of it (`total`, `<list>_count`) and is at least what the list held. With
/// none, or two to choose from, nothing says what it counts.
fn with_count_corrected(
    rt: &MockRuntime,
    op: &Operation,
    before: &Value,
    kept: &Value,
    was: usize,
    removed: u64,
) -> Option<Value> {
    let (Value::Object(old), Value::Object(new)) = (before, kept) else {
        return None;
    };
    let list = old.iter().find_map(|(key, value)| {
        let shorter = value.as_array()?.len() != new.get(key)?.as_array()?.len();
        shorter.then_some(key.as_str())
    })?;
    let plain = |name: &str| name.to_ascii_lowercase().replace('_', "");
    let own = [
        format!("{}count", plain(list)),
        format!("total{}", plain(list)),
    ];
    let schema = op.success_output().and_then(|o| o.schema.as_ref());
    let (parts, _) = schema_parts(rt.bundle(), schema);
    let declares_integer = |name: &str| {
        parts
            .iter()
            .find_map(|p| p.get("properties")?.get(name))
            .is_some_and(|member| {
                let (parts, _) = schema_parts(rt.bundle(), Some(member));
                parts.iter().any(|p| match p.get("type") {
                    Some(Value::String(t)) => t == "integer",
                    Some(Value::Array(types)) => types.iter().any(|t| t == "integer"),
                    _ => false,
                })
            })
    };
    let mut counts = old.iter().filter(|(name, value)| {
        let name_plain = plain(name);
        (COUNT_NAMES.contains(&name_plain.as_str()) || own.contains(&name_plain))
            && value.as_u64().is_some_and(|n| n >= was as u64)
            && declares_integer(name)
    });
    let (name, value) = counts.next()?;
    if counts.next().is_some() {
        return None;
    }
    let mut corrected = new.clone();
    corrected.insert(name.clone(), Value::from(value.as_u64()? - removed));
    Some(Value::Object(corrected))
}
