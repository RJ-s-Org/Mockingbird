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
//! A lookup by value can bring a record back under a new id: after a delete, Auth0's
//! `/users-by-email?email=` answered a user the model made up, holding the deleted
//! user's email. So a delete of a stored record also marks, by hash, the text of its
//! identifying members: its identity, and the members a lookup of its type keys on, a
//! list read whose required query parameter names the member (see
//! [`BundleIndex::lookup_members`]). An optional filter (`state`, `status`) is no key,
//! as many records share its value. Value markers have a bound of their own,
//! [`MAX_DELETED_VALUES`], so they never push an id's marker out. A generated list item
//! of that type that the session does not hold, whose such member holds a marked value
//! that the request names too, goes as a marked id does. A create storing a record with
//! that value clears its marker, so a record made again with the same email stays.
//!
//! An LLM and `consistency: on` only: offline answers, and those with consistency
//! off, are as they were. Of GraphQL, only a root delete by id marks, and only a root
//! read by id honours the marker ([`marks_by_id`]): GraphQL stores what its answers
//! hold under their own types, so its lists and mentions are left as they are.

use mb_engine::consistency::{list_items_mut, list_member};
use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::*;

/// Markers kept per session, the newest.
const MAX_DELETED: usize = 1024;
/// The document listing them, oldest first, and the prefix of their ids. No memory
/// key starts so: those start with a resource type, and a type comes from a path.
const INDEX: &str = "#deleted";
/// Value markers kept per session, the newest, apart from the ids'.
const MAX_DELETED_VALUES: usize = 1024;
/// The document listing them, oldest first.
const VALUES_INDEX: &str = "#deleted-values";

/// The members that count a list when they stand beside it, lower case with no `_`.
const COUNT_NAMES: [&str; 5] = ["total", "count", "totalcount", "totalresults", "totalitems"];

/// Whether this call keeps and honours markers.
pub(super) fn applies(rt: &MockRuntime, plan: &CallPlan<'_>) -> bool {
    marks_by_id(rt, plan) && !matches!(plan.op.binding, Binding::Graphql { .. })
}

/// Whether a delete by id marks what it removed and a read by id honours the marker:
/// [`applies`], and GraphQL's root fields too, whose delete by id leaves a marker
/// like a REST one, and whose read of a marked id the session does not hold answers
/// `null` and an error. The rest stays REST's: a GraphQL answer's lists and mentions
/// are not checked against markers.
pub(super) fn marks_by_id(rt: &MockRuntime, plan: &CallPlan<'_>) -> bool {
    rt.consistency && plan.engine.has_provider()
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

/// The marker of value `value` of member `member` of a deleted `t` under `scope`.
fn value_key(t: &str, scope: Option<&str>, member: &str, value: &str) -> String {
    let mut hasher = Sha256::new();
    for part in ["value", t, scope.unwrap_or_default(), member, value] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let hash: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{INDEX}/{hash}")
}

/// Members whose text a delete of a `t` marks, at most.
const MAX_MARKED_VALUES: usize = 8;

/// The members of a `t` that identify it: its identity, and those a lookup of its type
/// keys on.
fn identifying(rt: &MockRuntime, t: &str) -> Vec<String> {
    let index = rt.index();
    let mut members: Vec<String> = index.identity_fields(t).to_vec();
    for member in index.lookup_members(t) {
        if !members.contains(member) {
            members.push(member.clone());
        }
    }
    members
}

/// The text `data` holds in `t`'s identifying members, with the member.
fn identifying_values<'a>(rt: &MockRuntime, t: &str, data: &'a Value) -> Vec<(String, &'a str)> {
    identifying(rt, t)
        .into_iter()
        .filter_map(|member| {
            let value = mb_mcm::member_at(data, &member)?
                .as_str()
                .filter(|v| !v.is_empty())?;
            Some((member, value))
        })
        .take(MAX_MARKED_VALUES)
        .collect()
}

/// Strings a request holds, at most this many, and how deep its body is walked.
const MAX_NAMED_VALUES: usize = 64;
const MAX_NAMED_DEPTH: usize = 4;

/// The text a request names: in its path, its query and its body.
fn named_values(call: &OperationCall) -> HashSet<String> {
    fn walk(value: &Value, depth: usize, out: &mut HashSet<String>) {
        if depth > MAX_NAMED_DEPTH || out.len() >= MAX_NAMED_VALUES {
            return;
        }
        match value {
            Value::String(text) => {
                out.insert(text.clone());
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, depth + 1, out)),
            Value::Object(members) => members.values().for_each(|v| walk(v, depth + 1, out)),
            _ => {}
        }
    }
    let mut out = HashSet::new();
    for value in call.path_params.values().chain(call.query.values()) {
        walk(value, 0, &mut out);
    }
    if let Some(body) = &call.body {
        walk(body, 0, &mut out);
    }
    out
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
    let read = *rt.index().detail_reads.get(t)?;
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

/// Marks what a delete of `id` on `op`'s route removed: `id`, what the path called it,
/// which may be a key its reads go by, and the identifying text of `held`, what the
/// session held under it, all under the route's parent.
pub(super) async fn mark(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (op, call): (&Operation, &OperationCall),
    t: &str,
    id: &str,
    held: Option<&StoredResource>,
) {
    let rt = state.rt;
    let parent = scope(op, call);
    let named = target_id(op, call).unwrap_or_else(|| id.to_string());
    let mut keys = vec![marker_key(t, parent.as_deref(), &named)];
    if named != id {
        keys.push(marker_key(t, parent.as_deref(), id));
    }
    let mut values = Vec::new();
    if let Some(held) = held.filter(|h| h.resource_type == t) {
        for (member, value) in identifying_values(rt, t, &held.data) {
            let key = value_key(t, parent.as_deref(), &member, value);
            if !values.contains(&key) {
                values.push(key);
            }
        }
    }
    // Each kind under its own bound: values never push an id's marker out.
    write_markers(state, session, INDEX, keys, MAX_DELETED).await;
    write_markers(state, session, VALUES_INDEX, values, MAX_DELETED_VALUES).await;
}

/// Writes the markers `keys` and lists them in the index document `index`, which keeps
/// the newest `max` and ends the markers past it.
async fn write_markers(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    index: &str,
    keys: Vec<String>,
    max: usize,
) {
    if keys.is_empty() {
        return;
    }
    for key in &keys {
        let marker = StoredResource {
            resource_type: INTERNAL_TYPE.to_string(),
            id: key.clone(),
            data: serde_json::json!({"deleted": true}),
        };
        state.upsert(session, marker).await;
    }
    let mut listed: Vec<String> = match state.get(session, INTERNAL_TYPE, index).await {
        Some(doc) => serde_json::from_value(doc.data).unwrap_or_default(),
        None => Vec::new(),
    };
    listed.retain(|held| !keys.contains(held));
    listed.extend(keys);
    let excess = listed.len().saturating_sub(max);
    for oldest in listed.drain(..excess) {
        state.delete(session, INTERNAL_TYPE, &oldest).await;
    }
    let document = StoredResource {
        resource_type: INTERNAL_TYPE.to_string(),
        id: index.to_string(),
        data: Value::from(listed),
    };
    state.upsert(session, document).await;
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

/// Ends the markers of what was just stored, its id and its identifying text, under
/// `scope` and where its read would look. The index keeps its entries, which a later
/// delete moves on or the bound takes out.
pub(super) async fn clear(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    scope: Option<&str>,
    stored: &StoredResource,
) {
    let rt = state.rt;
    let read = state
        .call_params
        .as_ref()
        .and_then(|call| scope_of_type(rt, t, call))
        .flatten();
    let mut scopes = vec![scope];
    if read.as_deref() != scope {
        scopes.push(read.as_deref());
    }
    let values = identifying_values(rt, t, &stored.data);
    for scope in scopes {
        state
            .delete(session, INTERNAL_TYPE, &marker_key(t, scope, &stored.id))
            .await;
        for (member, value) in &values {
            state
                .delete(session, INTERNAL_TYPE, &value_key(t, scope, member, value))
                .await;
        }
    }
}

/// Takes marked ids out of a generated list answer, and the items the session does
/// not hold that carry a marked value `call` names, each only if the answer stays
/// valid, and corrects the count beside the list that is clearly its own.
pub(super) async fn drop_from_list(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (plan, call): (&CallPlan<'_>, &OperationCall),
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
    let field = item_identity(rt.bundle(), op, body);
    let mut gone = Vec::new();
    for (at, item) in items.iter().enumerate() {
        if let Some(id) = mb_mcm::member_at(item, &field).and_then(id_string)
            && is_deleted(state, session, t, scope, &id).await
        {
            gone.push(at);
        }
    }
    let item_type = rt
        .index()
        .item_type(rt.bundle(), op, body)
        .unwrap_or_else(|| t.to_string());
    let named = named_values(call);
    for (at, item) in items.iter().enumerate() {
        if named.is_empty() {
            break;
        }
        if gone.contains(&at) {
            continue;
        }
        let mut marked = false;
        for (member, value) in identifying_values(rt, &item_type, item) {
            if named.contains(value)
                && state
                    .get(
                        session,
                        INTERNAL_TYPE,
                        &value_key(&item_type, scope, &member, value),
                    )
                    .await
                    .is_some()
            {
                marked = true;
                break;
            }
        }
        let held = match mb_mcm::member_at(item, &field).and_then(id_string) {
            Some(id) if marked => state.get(session, &item_type, &id).await.is_some(),
            _ => false,
        };
        if marked && !held {
            gone.push(at);
        }
    }
    gone.sort_unstable();
    gone.dedup();
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
pub(super) fn with_count_corrected(
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
    let (name, value) = list_count(rt, op, old, list, was)?;
    let mut corrected = new.clone();
    corrected.insert(name.to_string(), Value::from(value.checked_sub(removed)?));
    Some(Value::Object(corrected))
}

/// `body` with the count of its list raised to `known`, when it counts less: the one
/// member [`with_count_corrected`] would lower. With none, nothing says what it counts.
pub(super) fn with_count_at_least(
    rt: &MockRuntime,
    op: &Operation,
    body: &Value,
    known: usize,
) -> Option<Value> {
    let Value::Object(members) = body else {
        return None;
    };
    let list = list_member(body)?;
    let was = members.get(list)?.as_array()?.len();
    let (name, value) = list_count(rt, op, members, list, was)?;
    let known = u64::try_from(known).ok()?;
    if value >= known {
        return None;
    }
    let mut raised = members.clone();
    raised.insert(name.to_string(), Value::from(known));
    Some(Value::Object(raised))
}

/// The count of list member `list` of answer `members`, which holds `was` items: the
/// one member beside it that the contract declares an integer, is named as a count of
/// it (`total`, `<list>_count`) and is at least what the list holds, with its value.
fn list_count<'a>(
    rt: &MockRuntime,
    op: &Operation,
    members: &'a Map<String, Value>,
    list: &str,
    was: usize,
) -> Option<(&'a str, u64)> {
    let old = members;
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
    Some((name.as_str(), value.as_u64()?))
}
