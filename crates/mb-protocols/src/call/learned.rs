//! A resource's identity learned from what a running mock has seen, where the
//! contract's names do not give it: `/things/{id}` over a schema with `ref` and no `id`.
//!
//! Only a type that a route reads, updates or deletes by a last path parameter can be
//! found again, so only such a type is held (`/token`-style answers are not), and only
//! one none of whose routes has an identity of its own. An enveloped or an array answer
//! is not one of these: it names no member at its top level, and a stored record could
//! not answer its read (see [`BundleIndex::open_types`]).
//!
//! A create with nothing to go under is held under a key of its own, in the store's
//! reserved type ([`mb_state::INTERNAL_TYPE`]), like the write memory's documents and
//! the deleted markers: Redis, SQL and memory all keep it, sessions, TTLs, resets and
//! fixtures clear it, and it is no resource (not exported, counted or seeded, and on no
//! change feed). So the key is shown to no one, and until it is learned the record is in
//! no export. A list or a lookup of the type is shown the record, by its data alone.
//!
//! A later path value on that type's route is looked for in the top-level members of
//! what is held and stored. Exactly one record holding it, in exactly one member, makes
//! that member the type's identity in this session, also in the store, so any node knows
//! it. Every record of the type then goes under it, taking its memory along and ending a
//! marker on that id, as a create storing the id would. The operation is then answered
//! with that identity, so what any identity gives follows: reading from state, settling
//! an update, delete markers, write memory and owner checks.
//!
//! What cannot be a key does not count: a value the create's own request said (the path
//! then says only that the client knows what it sent), a member the contract gives a
//! fixed set of values, and a member two records hold alike. A member that is wrong all
//! the same gives way when a path names another one, and is not taken again.
//!
//! The scan looks at the newest [`MAX_SCAN`] of each kind, and costs one reserved read
//! plus up to two listings on a read of such a type that misses. A contract whose names
//! give an identity costs none of it. With `consistency: off` nothing is held or learned.

use mb_mcm::Identity;
use mb_state::INTERNAL_TYPE;

use super::*;

/// The prefix of the document naming a type's learned member.
const LEARNED: &str = "#identity/";
/// The prefix of a held record's key. No memory key starts so: those start with a
/// resource type, and a type comes from a path.
const HELD: &str = "#pending/";

/// What a session has learned of a type.
#[derive(Default)]
struct Known {
    field: Option<String>,
    /// Members taken once and given up.
    rejected: Vec<String>,
}

/// A record of the type: held under a key of its own, or stored.
struct Seen {
    id: String,
    data: Value,
    held: bool,
    /// The members the create's request said, as they are in `data`.
    said: Vec<String>,
    collection: Option<Collection>,
}

fn learned_key(t: &str) -> String {
    format!("{LEARNED}{t}")
}

fn held_prefix(t: &str) -> String {
    format!("{HELD}{t}/")
}

async fn known(state: &mut StateAccess<'_>, session: &SessionKey, t: &str) -> Known {
    let Some(doc) = state.get(session, INTERNAL_TYPE, &learned_key(t)).await else {
        return Known::default();
    };
    let names = |member: &str| -> Vec<String> {
        doc.data
            .get(member)
            .cloned()
            .and_then(|names| serde_json::from_value(names).ok())
            .unwrap_or_default()
    };
    Known {
        field: doc
            .data
            .get("field")
            .and_then(Value::as_str)
            .map(str::to_owned),
        rejected: names("rejected"),
    }
}

/// The parameter a route's last segment is.
fn by_path_param(op: &Operation) -> Option<String> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    path.rsplit('/')
        .find(|s| !s.is_empty())
        .filter(|last| last.contains('{'))
        .and_then(|_| last_path_param(path))
}

fn with_identity(op: &Operation, field: &str, path_param: Option<String>) -> Operation {
    let mut op = op.clone();
    op.identity = Some(Identity {
        path_param,
        field: field.to_owned(),
        input_field: None,
        path_field: None,
    });
    op
}

/// `plan`'s operation with the identity this session knows of its type, after looking
/// for what a path value on it names. `None` for an operation the contract gives an
/// identity, or a type with none known.
pub(super) async fn resolve(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Option<Operation> {
    let rt = state.rt;
    let op = plan.op;
    if !rt.consistency
        || op.identity.is_some()
        || !matches!(op.binding, Binding::Http { .. })
        || !matches!(op.kind, OperationKind::Query | OperationKind::Command(_))
    {
        return None;
    }
    let t = op
        .resource_type
        .as_deref()
        .filter(|t| rt.index().open_types.contains_key(*t))?;
    let known = known(state, &plan.session, t).await;
    let param = by_path_param(op);
    let value = param
        .as_deref()
        .and_then(|p| call.path_params.get(p))
        .and_then(id_string);
    let field = match value {
        Some(value) => {
            let probe = with_identity(op, "", param.clone());
            learn(state, &plan.session, &probe, call, t, &value, known).await
        }
        None => known.field,
    }?;
    state.learned = Some(Box::new((t.to_owned(), field.clone())));
    Some(with_identity(op, &field, param))
}

/// The member `value` names, with what that changes done; else what was known.
async fn learn(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    probe: &Operation,
    call: &OperationCall,
    t: &str,
    value: &str,
    known: Known,
) -> Option<String> {
    if known.field.is_some() && state.get(session, t, value).await.is_some() {
        return known.field;
    }
    let seen = seen(state, session, t).await;
    let scope = collection_of(probe, call);
    let Some(member) = named_member(state.rt, t, &seen, scope.as_ref(), value, &known) else {
        return known.field;
    };
    let mut rejected = known.rejected;
    rejected.extend(known.field);
    let doc = StoredResource {
        resource_type: INTERNAL_TYPE.to_owned(),
        id: learned_key(t),
        data: serde_json::json!({"field": member, "rejected": rejected}),
    };
    if !state.upsert(session, doc).await {
        return None;
    }
    for record in seen {
        rekey(state, session, t, &member, record).await;
    }
    Some(member)
}

/// The one member that the one record of `seen`, in `scope`, holds `value` in, when
/// it can be a key: no two records there hold it alike.
fn named_member(
    rt: &MockRuntime,
    t: &str,
    seen: &[Seen],
    scope: Option<&Collection>,
    value: &str,
    known: &Known,
) -> Option<String> {
    let in_scope: Vec<&Seen> = seen
        .iter()
        .filter(|r| match (scope, &r.collection) {
            (Some(scope), Some(held)) => scope.path == held.path,
            _ => true,
        })
        .collect();
    let mut hits = in_scope.iter().flat_map(|r| {
        let members = r.data.as_object().into_iter().flatten();
        members
            .filter(|(member, held)| {
                id_string(held).as_deref() == Some(value)
                    && !r.said.contains(member)
                    && known.field.as_ref() != Some(*member)
                    && !known.rejected.contains(member)
                    && !is_fixed(rt, t, member)
            })
            .map(|(member, _)| member)
    });
    let (Some(member), None) = (hits.next(), hits.next()) else {
        return None;
    };
    let mut values = HashSet::new();
    in_scope
        .iter()
        .filter_map(|r| r.data.get(member).and_then(id_string))
        .all(|held| values.insert(held))
        .then(|| member.clone())
}

/// Whether the contract gives `member` of `t` a fixed set of values.
fn is_fixed(rt: &MockRuntime, t: &str, member: &str) -> bool {
    let bundle = rt.bundle();
    let schema = write_memory::read_schema(rt, t);
    schema_parts(bundle, schema)
        .0
        .iter()
        .filter_map(|part| part.get("properties")?.get(member))
        .any(|inner| {
            schema_parts(bundle, Some(inner))
                .0
                .iter()
                .any(|p| p.get("enum").is_some() || p.get("const").is_some())
        })
}

/// What is held of type `t`, newest first.
async fn held(state: &mut StateAccess<'_>, session: &SessionKey, t: &str) -> Vec<Seen> {
    let prefix = held_prefix(t);
    let docs = state.list(session, INTERNAL_TYPE, MAX_SCAN).await;
    docs.into_iter()
        .filter(|doc| doc.id.starts_with(&prefix))
        .filter_map(|doc| {
            let mut data = doc.data;
            let said = data
                .get("said")
                .cloned()
                .and_then(|said| serde_json::from_value(said).ok())
                .unwrap_or_default();
            let collection = match (
                data.get("template").and_then(Value::as_str),
                data.get("path").and_then(Value::as_str),
            ) {
                (Some(template), Some(path)) => Some(Collection {
                    template: template.to_owned(),
                    path: path.to_owned(),
                }),
                _ => None,
            };
            Some(Seen {
                id: doc.id,
                data: data.get_mut("record").map(Value::take)?,
                held: true,
                said,
                collection,
            })
        })
        .collect()
}

/// Every record of type `t`, held or stored.
async fn seen(state: &mut StateAccess<'_>, session: &SessionKey, t: &str) -> Vec<Seen> {
    let rt = state.rt;
    let mut records = held(state, session, t).await;
    let stored = state.list(session, t, MAX_SCAN).await;
    records.extend(stored.into_iter().map(|r| {
        let collection = rt.collections().get(session, &r);
        Seen {
            id: r.id,
            data: r.data,
            held: false,
            said: Vec::new(),
            collection,
        }
    }));
    records
}

/// Puts `record` under what its `field` holds, unless it holds nothing or another
/// record is there: it then stays as it is. Its memory goes along, and a marker on
/// that id ends, as when a create stores it.
async fn rekey(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    field: &str,
    record: Seen,
) {
    let rt = state.rt;
    let Some(id) = record.data.get(field).and_then(id_string) else {
        return;
    };
    if !record.held && record.id == id {
        return;
    }
    if state.get(session, t, &id).await.is_some() {
        return;
    }
    let moved = StoredResource {
        resource_type: t.to_owned(),
        id: id.clone(),
        data: record.data,
    };
    if !state.upsert(session, moved.clone()).await {
        return;
    }
    let from = if record.held { INTERNAL_TYPE } else { t };
    state.delete(session, from, &record.id).await;
    write_memory::rekey(state, session, t, &record.id, &id).await;
    if state.tracks_deleted {
        let scope = record.collection.as_ref().map(|c| c.path.as_str());
        deleted::clear(state, session, t, scope, &id).await;
    }
    if let Some(collection) = record.collection {
        rt.collections().remember(session, &moved, collection);
    }
}

/// Holds what a create answered when it has nothing to go under; its key, for what
/// else the write leaves.
pub(super) async fn hold(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    generated: &GeneratedResponse,
    collection: Option<&Collection>,
) -> Option<String> {
    let rt = state.rt;
    let op = plan.op;
    let t = op
        .resource_type
        .as_deref()
        .filter(|t| rt.consistency && rt.index().open_types.contains_key(*t))?;
    let (OperationKind::Command(Effect::Create), Binding::Http { .. }, true) =
        (op.kind, &op.binding, (200..300).contains(&generated.status))
    else {
        return None;
    };
    let body = generated.body.as_ref()?;
    let members = body.as_object()?;
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    if members.get(field).and_then(id_string).is_some()
        || !members.values().any(|member| id_string(member).is_some())
    {
        return None;
    }
    let request = call.body.as_ref();
    let said: Vec<&String> = members
        .iter()
        .filter(|(member, held)| request.and_then(|r| r.get(*member)) == Some(*held))
        .map(|(member, _)| member)
        .collect();
    let key = format!("{}{:032x}", held_prefix(t), rand::random::<u128>());
    let mut data = serde_json::json!({"record": body, "said": said});
    if let Some(collection) = collection {
        data["template"] = Value::from(collection.template.as_str());
        data["path"] = Value::from(collection.path.as_str());
    }
    let doc = StoredResource {
        resource_type: INTERNAL_TYPE.to_owned(),
        id: key.clone(),
        data,
    };
    state.upsert(&plan.session, doc).await.then_some(key)
}

/// What is held of `t`, as a read's context shows it: by its data alone.
pub(super) async fn held_context(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
) -> Vec<StoredResource> {
    let records = held(state, session, t).await;
    records
        .into_iter()
        .map(|record| StoredResource {
            resource_type: t.to_owned(),
            id: record.id,
            data: record.data,
        })
        .collect()
}
