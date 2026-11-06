//! Records related to an answer: parents, references, referrers and siblings.

use super::*;

/// Resources the response should stay consistent with, nearest first: what
/// the request names, then recent items for a list query. [`follow_hops`]
/// adds what those refer to. With consistency off, only the `<x>Id` members
/// and global ids of the query and body are followed.
pub(super) async fn related(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    target_id: Option<&str>,
    target: Option<&StoredResource>,
    call: &OperationCall,
) -> (Vec<StoredResource>, Followed) {
    let rt = state.rt;
    let index = rt.consistency.then(|| rt.index());
    let mut references = Vec::new();
    if index.is_some() {
        for (field, value) in &call.path_params {
            if let Some(id) = id_string(value) {
                references_in(field, &Value::String(id), 0, index, &mut references);
            }
        }
    }
    for (field, value) in &call.query {
        references_in(field, value, 0, index, &mut references);
    }
    if let Some(Value::Object(fields)) = &call.body {
        for (field, value) in fields {
            references_in(field, value, 0, index, &mut references);
        }
    }
    let mut out: Vec<StoredResource> = Vec::new();
    let mut seen = Vec::new();
    follow(state, session, references, &mut seen, target, &mut out).await;
    // A name only means something under its owner: another's is not named.
    if index.is_some() {
        for (field, value) in &call.path_params {
            let (Some(owner), Some(name)) = (
                Some(field.as_str())
                    .filter(|f| !id_like(f))
                    .and_then(|f| owner_of_param(op, call, f)),
                id_string(value),
            ) else {
                continue;
            };
            out.retain(|r| r.id != name || key_fits(rt, session, r, &owner));
        }
    }
    let named = out.len();
    if matches!(op.kind, OperationKind::Query)
        && target_id.is_none()
        && let Some(t) = op.resource_type.as_deref()
    {
        let mut listed = state.list(session, t, LIST_CONTEXT_LIMIT).await;
        // Lists the answer holds that other reads stored: Plaid's
        // `/auth/get` answers the `accounts` that `/accounts/get` seeded.
        let members = rt
            .index()
            .operation(&op.id)
            .map(|i| rt.index().list_members(i))
            .unwrap_or_default();
        let others = members
            .iter()
            .filter(|m| m.as_str() != t && !ENVELOPE_KEYS.contains(&m.as_str()));
        for member in others.take(MAX_LIST_MEMBERS) {
            if listed.len() >= LIST_CONTEXT_LIMIT {
                break;
            }
            let room = LIST_CONTEXT_LIMIT - listed.len();
            listed.extend(state.list(session, member, room).await);
        }
        for item in listed {
            if !holds(&out, &item) {
                out.push(item);
            }
        }
    }
    (out, Followed { named, seen })
}

/// What [`related`] followed, for [`follow_hops`] to go on from.
pub(super) struct Followed {
    /// How many of the related resources the request itself names; they
    /// come first.
    pub(super) named: usize,
    /// The references looked up so far (see [`follow`]).
    pub(super) seen: Vec<Reference>,
}

/// Adds to `out`, after what the request names, the parents its path names by
/// name where a lookup by id found nothing: `{repo}` in
/// `/repos/{owner}/{repo}/issues` is a repository stored under its numeric id
/// (see [`NAME_FIELDS`]). Only a path parameter [`related`] looked up as a
/// reference, other than the one naming the target (already searched for), so
/// this stays within [`MAX_REFERENCES`]. Separate from [`related`], for what
/// [`follow_hops`] is. Nothing with consistency off.
pub(super) async fn parents_by_name(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    call: &OperationCall,
    target: Option<&StoredResource>,
    followed: &mut Followed,
    out: &mut Vec<StoredResource>,
) {
    let rt = state.rt;
    if !rt.consistency {
        return;
    }
    let own = op.identity.as_ref().and_then(|i| i.path_param.as_deref());
    for (field, value) in &call.path_params {
        let (Some(id), false) = (id_string(value), own == Some(field.as_str())) else {
            continue;
        };
        let Some(t) = entity_type(rt.index(), None, field) else {
            continue;
        };
        let reference = (vec![t.clone()], id.clone());
        let found = |r: &StoredResource| r.resource_type == t && r.id == id;
        if !followed.seen.contains(&reference)
            || target.is_some_and(found)
            || out[..followed.named].iter().any(found)
        {
            continue;
        }
        let owner = owner_of_param(op, call, field);
        let names = name_fields(field);
        let parent = match by_path_fields(state, session, &t, &names, &id, owner.as_ref()).await {
            Some(parent) => Some(parent),
            None => {
                let tried = (names.as_slice(), id.as_str());
                records::by_extra_fields(state, session, &t, tried, owner.as_ref()).await
            }
        };
        let Some(parent) = parent else {
            continue;
        };
        let is_target =
            target.is_some_and(|r| r.resource_type == parent.resource_type && r.id == parent.id);
        if is_target || holds(&out[..followed.named], &parent) {
            continue;
        }
        // Listed already, it moves up among what the request names.
        out.retain(|r| r.resource_type != parent.resource_type || r.id != parent.id);
        out.insert(followed.named, parent);
        followed.named += 1;
    }
}

/// Adds to `out`, as gathered by [`related`], what the target and the
/// resources the request names refer to. Separate from [`related`] because
/// only a generation needs it: an answer served from the store or the cache
/// must not pay for these lookups. Nothing with consistency off.
pub(super) async fn follow_hops(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    target: Option<&StoredResource>,
    followed: Followed,
    out: &mut Vec<StoredResource>,
) {
    let rt = state.rt;
    if !rt.consistency {
        return;
    }
    let index = Some(rt.index());
    let Followed { named, mut seen } = followed;
    let (mut own, mut start, mut end) = (target, 0, named);
    for _ in 0..MAX_RELATED_HOPS {
        let mut references = Vec::new();
        for resource in own.take().into_iter().chain(&out[start..end]) {
            references_in("", &resource.data, 0, index, &mut references);
        }
        start = out.len();
        follow(state, session, references, &mut seen, target, out).await;
        end = out.len();
    }
}

/// A reference: the types to look in, and the id.
pub(super) type Reference = (Vec<String>, String);

/// Adds to `out` the resources `references` name, other than `target`.
/// `seen` holds the references looked up so far in this call: at most
/// [`MAX_REFERENCES`], so that what is followed first is what is nearest.
pub(super) async fn follow(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    references: Vec<Reference>,
    seen: &mut Vec<Reference>,
    target: Option<&StoredResource>,
    out: &mut Vec<StoredResource>,
) {
    for reference in references {
        if seen.len() >= MAX_REFERENCES {
            return;
        }
        if seen.contains(&reference) {
            continue;
        }
        seen.push(reference.clone());
        let (types, id) = reference;
        for t in types.iter().filter(|t| *t != mb_state::INTERNAL_TYPE) {
            let t = t.as_str();
            if target.is_some_and(|r| r.resource_type == t && r.id == id) {
                break;
            }
            if let Some(hit) = records::held_by_key(state, session, t, &id).await {
                let is_target =
                    target.is_some_and(|r| r.resource_type == hit.resource_type && r.id == hit.id);
                if !is_target && !holds(out, &hit) {
                    out.push(hit);
                }
                break;
            }
        }
    }
}

/// Whether `resources` already holds `resource`.
pub(super) fn holds(resources: &[StoredResource], resource: &StoredResource) -> bool {
    resources
        .iter()
        .any(|r| r.resource_type == resource.resource_type && r.id == resource.id)
}

/// Records [`created_alongside`] reads, at most.
pub(super) const MAX_CREATED_ALONGSIDE: usize = 4;

/// The path of the record a read by path parameter reads: `/captures/C1` of
/// `/captures/{capture_id}`.
pub(super) fn record_path(op: &Operation, call: &OperationCall) -> Option<String> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let param = format!("{{{}}}", target_param(op)?);
    let segments = path_segments(path);
    let at = segments.iter().position(|s| *s == param)?;
    Some(fill_path(call, &segments[..=at])?.path)
}

/// Adds to `related` what creates put in the collection under a parent path that a
/// create or a read runs in (`/captures/C1/refund`), newest first: a second refund is
/// shown the first. A read of one record by its path (`/captures/C1`) is shown, the
/// same way, what creates put under that path, so a capture's read sees its refunds.
/// The record must still be what was created there, or [`Collections`] no longer
/// places it; it is scrubbed as a record found by value is
/// ([`write_memory::matched_for_context`]). Context only, and only where
/// [`write_memory::applies`] holds. For a create, the type and id of every record of
/// `related` that creates put in its own collection, already shown or added here: what
/// it can conflict with by being of their type (see
/// [`mb_engine::GenerationContext::alongside`]).
pub(super) async fn created_alongside(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target: Option<&StoredResource>,
    related: &mut Vec<StoredResource>,
) -> Vec<(String, String)> {
    let rt = state.rt;
    let op = plan.op;
    let mut marked = Vec::new();
    if !write_memory::applies(rt, plan)
        || !matches!(
            op.kind,
            OperationKind::Query | OperationKind::Command(Effect::Create)
        )
    {
        return marked;
    }
    let create = matches!(op.kind, OperationKind::Command(Effect::Create));
    let alongside = collection_of(op, call);
    let beneath = matches!(op.kind, OperationKind::Query)
        .then(|| record_path(op, call))
        .flatten()
        .map(|path| format!("{path}/"));
    if alongside.is_none() && beneath.is_none() {
        return marked;
    }
    let fits = |c: &Collection| {
        alongside.as_ref() == Some(c) || beneath.as_ref().is_some_and(|p| c.path.starts_with(p))
    };
    let session = &plan.session;
    let created = rt
        .collections()
        .created_in(session, fits, MAX_CREATED_ALONGSIDE);
    for (t, id) in created {
        let shown = |r: &StoredResource| r.resource_type == t && r.id == id;
        if target.is_some_and(shown) {
            continue;
        }
        if related.iter().any(shown) {
            // Already shown, by value, as a scrubbed copy: whether this collection
            // still holds it is the stored copy's to say.
            if create
                && let Some(held) = state.get(session, &t, &id).await
                && rt
                    .collections()
                    .get(session, &held)
                    .is_some_and(|c| fits(&c))
            {
                marked.push((t.clone(), id.clone()));
            }
            continue;
        }
        let Some(held) = state.get(session, &t, &id).await else {
            continue;
        };
        if !rt
            .collections()
            .get(session, &held)
            .is_some_and(|c| fits(&c))
        {
            continue;
        }
        if let Some(data) = write_memory::matched_for_context(rt, plan, &t, &held.data) {
            if create {
                marked.push((t.clone(), id.clone()));
            }
            related.push(StoredResource { data, ..held });
        }
    }
    marked
}

/// Puts first in `related` the records of the type a create makes that already hold a
/// value of its request under the same member ([`value_index::same_values`]), so a
/// prompt shows them whatever else it shows, and returns each one's type, id and member
/// (see [`mb_engine::GenerationContext::same_values`]). A record `related` holds already
/// keeps the copy it has; another is scrubbed as a record found by value is
/// ([`write_memory::matched_for_context`]), and left out when nothing of it may be shown.
/// Nothing, and `related` as it was, when there is none.
pub(super) async fn shown_same_values(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target: Option<&StoredResource>,
    related: &mut Vec<StoredResource>,
) -> Vec<(String, String, String)> {
    let rt = state.rt;
    let known = |t: &str, id: &str| target.is_some_and(|r| r.resource_type == t && r.id == id);
    let found = value_index::same_values(state, plan, call, known).await;
    let mut first = Vec::with_capacity(found.len());
    let mut marked = Vec::with_capacity(found.len());
    for (hit, member) in found {
        let shown = match related
            .iter()
            .position(|r| r.resource_type == hit.resource_type && r.id == hit.id)
        {
            Some(at) => related.remove(at),
            None => {
                let Some(data) =
                    write_memory::matched_for_context(rt, plan, &hit.resource_type, &hit.data)
                else {
                    continue;
                };
                StoredResource { data, ..hit }
            }
        };
        marked.push((shown.resource_type.clone(), shown.id.clone(), member));
        first.push(shown);
    }
    related.splice(0..0, first);
    marked
}

/// The stored resources that refer to `id` of `op`'s resource type, for a
/// read of an id the session does not hold: at most [`MAX_SCAN`] resources
/// of the types the contract says can refer to it are read, newest first,
/// and at most what a prompt shows ([`mb_engine::prompt::MAX_RELATED`]) kept.
pub(super) async fn referrers(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    id: &str,
) -> Vec<StoredResource> {
    let rt = state.rt;
    let Some(wanted) = op.resource_type.as_deref().filter(|_| rt.consistency) else {
        return Vec::new();
    };
    let index = rt.index();
    let types = index.referrers(wanted);
    let each = MAX_SCAN / types.len().clamp(1, MAX_REFERENCES);
    let mut out = Vec::new();
    for t in types.iter().take(MAX_REFERENCES) {
        for resource in state.list(session, t, each).await {
            let mut references = Vec::new();
            references_in("", &resource.data, 0, Some(index), &mut references);
            let refers = references
                .iter()
                .any(|(types, to)| to == id && types.iter().any(|t| t == wanted));
            if refers {
                out.push(resource);
                if out.len() >= mb_engine::prompt::MAX_RELATED {
                    return out;
                }
            }
        }
    }
    out
}

/// The stored resources `args` refers to by `<x>Id` members and global
/// ids (see [`references_in`]), for a GraphQL answer to agree with them.
#[cfg(feature = "graphql")]
pub(crate) async fn referenced(
    rt: &MockRuntime,
    session: &SessionKey,
    args: &Map<String, Value>,
) -> Vec<StoredResource> {
    let mut references = Vec::new();
    for (field, value) in args {
        references_in(field, value, 0, None, &mut references);
    }
    let mut state = StateAccess::new(rt);
    let mut out: Vec<StoredResource> = Vec::new();
    async {
        for (types, id) in references.into_iter().take(MAX_REFERENCES) {
            for t in types {
                if let Some(hit) = state.get(session, &t, &id).await {
                    if !out.iter().any(|r| r.id == hit.id) {
                        out.push(hit);
                    }
                    break;
                }
            }
        }
        state.touch(session).await;
    }
    .instrument(state_span())
    .await;
    out
}

/// Reference lookups per call, at most.
pub(super) const MAX_REFERENCES: usize = 16;

/// How far references are followed out of what a request names and of its
/// target: to what those refer to, and no further.
pub(super) const MAX_RELATED_HOPS: usize = 1;

/// The resources `value` (under member `field`) refers to, as the types to
/// look in and the id, here or in nested objects and lists (GraphQL input
/// objects, `lines: [{merchandiseId}]`): a `<x>Id`/`<x>_id` member (types
/// `<x>s`, `<x>`), or a global id `gid://<app>/<Type>/…` (type `<type>`).
/// With `index`, which is `None` when consistency is off, also a `<x>Id`
/// holding a number, and a string member named after a resource type
/// (`customer`, `payment_intent`; see [`entity_type`]).
pub(super) fn references_in(
    field: &str,
    value: &Value,
    depth: usize,
    index: Option<&BundleIndex>,
    out: &mut Vec<Reference>,
) {
    if depth > 4 || out.len() >= MAX_REFERENCES {
        return;
    }
    match value {
        Value::String(id) => {
            if let Some(t) = global_id_type(id) {
                out.push((vec![t], id.clone()));
            } else if let Some(prefix) = reference_prefix(field) {
                out.push((reference_types(index, prefix), id.clone()));
            } else if let Some(t) = index.and_then(|i| entity_type(i, None, field)) {
                out.push((vec![t], id.clone()));
            }
        }
        Value::Number(id) if index.is_some() => {
            if let Some(prefix) = reference_prefix(field) {
                out.push((reference_types(index, prefix), id.to_string()));
            }
        }
        Value::Array(items) => {
            for item in items {
                references_in(field, item, depth + 1, index, out);
            }
        }
        Value::Object(obj) => {
            for (k, v) in obj {
                references_in(k, v, depth + 1, index, out);
            }
        }
        _ => {}
    }
}

/// The types a `<prefix>Id` member refers to: `<prefix>s` and `<prefix>`, then, with
/// `index`, each type of the contract of the same stem ([`type_stem`]), so that
/// `eventTypeId` and `event_type_id` find `event-types`. Sorted after the first two.
pub(super) fn reference_types(index: Option<&BundleIndex>, prefix: &str) -> Vec<String> {
    let mut types = vec![format!("{prefix}s"), prefix.to_string()];
    let Some(index) = index else {
        return types;
    };
    let stem = type_stem(prefix);
    let mut known: Vec<&String> = index
        .read_types
        .iter()
        .chain(index.detail_reads.keys())
        .chain(&index.effect_types)
        .chain(
            index
                .listed
                .iter()
                .filter(|t| !ENVELOPE_KEYS.contains(&t.as_str())),
        )
        .filter(|t| !types.contains(t) && type_stem(t) == stem)
        .collect();
    known.sort();
    known.dedup();
    types.extend(known.into_iter().cloned());
    types
}

/// The type of a global id (`gid://shopify/ProductVariant/42` →
/// `productvariant`), as GraphQL answers are stored.
pub(super) fn global_id_type(id: &str) -> Option<String> {
    let rest = id.strip_prefix("gid://")?;
    let mut parts = rest.split('/');
    let _app = parts.next()?;
    let t = parts.next().filter(|t| !t.is_empty())?;
    parts.next()?;
    Some(t.to_lowercase())
}
