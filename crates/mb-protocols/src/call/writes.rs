//! Writing an answer's record to state: creates, updates, deletes and actions.

use super::*;

/// Who made a state write: the client's call, or list seeding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteOrigin {
    Call,
    Seeded,
}

/// A 2xx answer to write to state, and where it came from.
pub(super) struct WriteRequest<'a> {
    pub(super) session: &'a SessionKey,
    pub(super) op: &'a Operation,
    pub(super) resource_type: &'a str,
    pub(super) target_id: Option<String>,
    pub(super) generated: &'a GeneratedResponse,
    pub(super) request_body: Option<&'a Value>,
    /// The call's whole request body, which a described change's `from_request`
    /// pointer reads (`request_body` may be the record it holds).
    pub(super) call_body: Option<&'a Value>,
    /// The call's query, which a create's record is noted as made under (see
    /// [`Collections::note_scope`]).
    pub(super) query: Option<&'a Map<String, Value>>,
    /// Where the path puts what this writes (see [`collection_of`]).
    pub(super) collection: Option<Collection>,
    pub(super) origin: WriteOrigin,
    /// The session held what this writes to, so it cannot be marked deleted.
    pub(super) held: bool,
    /// The id a create's answer named its record by in a header (see [`header_ids`]).
    pub(super) header_id: Option<Value>,
    /// The records a create's `request.records` makes, with the ids
    /// [`records::mint_created`] gave them; empty when its request's items are not
    /// the records made.
    pub(super) request_records: Vec<Value>,
}

/// What [`write_state`] did with an answer: how many records it located and handed to
/// the store (whatever the store then answered), and whether a create or update found
/// no record of its type in the answer or the request (the `no_record` world gap).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct WriteOutcome {
    pub(super) stored: usize,
    pub(super) no_record: bool,
}

impl WriteOutcome {
    /// One record located and handed to the store.
    const ONE: Self = Self {
        stored: 1,
        no_record: false,
    };
    /// No record of the type found where one was written.
    const NO_RECORD: Self = Self {
        stored: 0,
        no_record: true,
    };
}

/// Writes a 2xx answer to state: a call's create, update or delete, or
/// (`Seeded`) every identified item of a list answer that the session does
/// not hold yet.
pub(super) async fn write_state(state: &mut StateAccess<'_>, w: WriteRequest<'_>) -> WriteOutcome {
    let WriteRequest {
        session,
        op,
        resource_type,
        target_id,
        generated,
        request_body,
        call_body,
        query,
        collection,
        origin,
        held: was_held,
        header_id,
        request_records,
    } = w;
    let rt = state.rt;
    if origin == WriteOrigin::Seeded {
        let Some(items) = generated.body.as_ref().and_then(list_items) else {
            return WriteOutcome::default();
        };
        let field = item_identity(
            state.rt.bundle(),
            op,
            generated.body.as_ref().unwrap_or(&Value::Null),
        );
        let mut seeded = Vec::new();
        for item in items {
            let Some(id) = mb_mcm::member_at(item, &field).and_then(id_string) else {
                continue;
            };
            // Marked under this list's parent; a mention is seen to on insert.
            if state.tracks_deleted {
                let parent = state
                    .call_params
                    .as_ref()
                    .and_then(|call| deleted::scope(op, call));
                if deleted::is_deleted(state, session, resource_type, parent.as_deref(), &id).await
                {
                    continue;
                }
            }
            seeded.push(StoredResource {
                resource_type: resource_type.into(),
                id,
                data: item.clone(),
            });
        }
        // The list's items read back in its order.
        let outcome = WriteOutcome {
            stored: seeded.len(),
            no_record: false,
        };
        let stored = state.insert_seeded_all(session, seeded.clone()).await;
        for (stored, inserted) in seeded.into_iter().zip(stored) {
            let Some(collection) = collection.clone() else {
                continue;
            };
            let id = stored.id.clone();
            if inserted {
                rt.collections().remember(session, &stored, collection);
            } else if let Some(held) = state.get(session, resource_type, &id).await
                && rt.collections().get(session, &held).is_none()
                && held.data == stored.data
            {
                // A mention stored it first: this list says where it belongs.
                rt.collections().remember(session, &held, collection);
            }
        }
        return outcome;
    }
    let is_create = op.kind == OperationKind::Command(Effect::Create);
    // A create takes its record from where the request holds it (`request.record`).
    let request_body = match request_body {
        Some(body) if is_create => Some(records::request_record(rt, op, body)),
        other => other,
    };
    // A create the description says makes several records, or a long-running one.
    if is_create {
        let made = records::created(
            rt,
            op,
            (request_body, &request_records),
            generated.body.as_ref(),
        );
        if !made.is_empty() {
            let field = records::identity_member(rt, op, resource_type);
            let mut outcome = WriteOutcome::default();
            for data in made {
                let Some(id) = mb_mcm::member_at(&data, field).and_then(id_string) else {
                    continue;
                };
                outcome.stored += 1;
                state_lists::mark_created(
                    state,
                    session,
                    (op, resource_type),
                    collection.as_ref(),
                    &id,
                )
                .await;
                let place = (collection.clone(), query, was_held);
                store_one(state, session, (op, resource_type), (id, data), place).await;
            }
            if outcome.stored > 0 {
                return outcome;
            }
        }
    }
    let data = generated.body.clone().or_else(|| request_body.cloned());
    // A create that named its record in a header: the request is the record, under the
    // header's id.
    let header_id = header_id.and_then(|value| Some((id_string(&value)?, value)));
    let (id, data) = match op.kind {
        OperationKind::Command(Effect::Create) if header_id.is_some() => {
            let Some((id, value)) = header_id else {
                return WriteOutcome::NO_RECORD;
            };
            let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
            let record = header_ids::record(rt, resource_type, request_body, field, value);
            (id, Some(record))
        }
        OperationKind::Command(Effect::Create) => {
            let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
            let Some(data) = data else {
                return WriteOutcome::NO_RECORD;
            };
            match mb_mcm::member_at(&data, field).and_then(id_string) {
                Some(id) => (id, Some(data)),
                // An envelope the contract does not say holds the record: the one
                // member of the answer that is a record of the type and holds what
                // the request sent is, when nothing is held under its id.
                // It is inserted as a mention is: never over a held record, nor one the
                // session deleted, and as no record this create made.
                None => {
                    let (Some(answer), Some(sent)) = (generated.body.as_ref(), request_body) else {
                        return WriteOutcome::NO_RECORD;
                    };
                    let want = Holding::Sent(sent);
                    let on = (op, generated.status);
                    let Some(data) = fitting_member(rt, on, resource_type, answer, want) else {
                        return WriteOutcome::NO_RECORD;
                    };
                    let Some(id) = mb_mcm::member_at(&data, field).and_then(id_string) else {
                        return WriteOutcome::NO_RECORD;
                    };
                    let resource = StoredResource {
                        resource_type: resource_type.into(),
                        id,
                        data,
                    };
                    let stored = resource.clone();
                    if state.insert_seeded(session, resource).await
                        && let Some(collection) = collection
                    {
                        rt.collections().remember(session, &stored, collection);
                    }
                    return WriteOutcome::ONE;
                }
            }
        }
        OperationKind::Command(Effect::Update) => {
            let (Some(id), Some(data)) = (target_id, data) else {
                return WriteOutcome::default();
            };
            // An answer of another schema adds nothing of its own to the held record.
            let mut data = match &generated.body {
                // A replace answering nothing replaces the held record all the same.
                body if was_held
                    && !generated.new_record
                    && (body.is_some() || transitions::replaces(rt, op)) =>
                {
                    let held = state.get(session, resource_type, &id).await;
                    let on = (resource_type, held.as_ref().map(|h| &h.data));
                    let answered = (data, generated.status);
                    write_memory::onto_record(rt, op, on, answered, request_body)
                }
                // With nothing held, an answer that does not suit the type (a task
                // acknowledging the update) is no record of it: nothing is stored,
                // unless it is an envelope holding the record, the one member of it
                // that suits the type and carries the target's id.
                Some(_) if !entity_fits(rt, resource_type, &data) => {
                    let want = Holding::Target(id.as_str());
                    let on = (op, generated.status);
                    match fitting_member(rt, on, resource_type, &data, want) {
                        Some(record) => record,
                        None => return WriteOutcome::NO_RECORD,
                    }
                }
                _ => data,
            };
            // What the description says the update sets, whatever was answered.
            if !generated.new_record {
                transitions::apply(rt, op, call_body, &mut data);
            }
            (id, Some(data))
        }
        // An action the description says sets members of its target: set on the held
        // record. Any other action stores nothing here.
        OperationKind::Command(Effect::Other)
            if !generated.new_record && transitions::sets_members(rt, op) =>
        {
            let Some(id) = target_id else {
                return WriteOutcome::default();
            };
            let Some(held) = state.get(session, resource_type, &id).await else {
                return WriteOutcome::default();
            };
            let mut data = held.data;
            if transitions::apply(rt, op, call_body, &mut data).is_empty() {
                return WriteOutcome::default();
            }
            (id, Some(data))
        }
        OperationKind::Command(Effect::Delete) => {
            let Some(id) = target_id else {
                return WriteOutcome::default();
            };
            // A described soft delete keeps the held record, in its deleted state.
            if lifecycle::exclusion(rt, resource_type).is_some()
                && let Some(held) = state.get(session, resource_type, &id).await
            {
                let mut data = held.data;
                if lifecycle::soft_delete(rt, resource_type, &mut data) {
                    let place = (collection, None, true);
                    store_one(state, session, (op, resource_type), (id, data), place).await;
                    return WriteOutcome::default();
                }
            }
            (id, None)
        }
        _ => return WriteOutcome::default(),
    };
    match data {
        Some(mut data) => {
            // A created record starts in its type's described initial state.
            if is_create {
                lifecycle::initial(rt, resource_type, &mut data, request_body);
                // A create into a listed collection is one of its members.
                state_lists::mark_created(
                    state,
                    session,
                    (op, resource_type),
                    collection.as_ref(),
                    &id,
                )
                .await;
            }
            let place = (collection, query, was_held);
            store_one(state, session, (op, resource_type), (id, data), place).await;
            WriteOutcome::ONE
        }
        None => {
            let gone = state.get(session, resource_type, &id).await;
            state.delete(session, resource_type, &id).await;
            if let Some(gone) = gone {
                rt.collections().forget(session, &gone);
            }
            WriteOutcome::default()
        }
    }
}

/// Stores the record `data` a create or update wrote under `id`, keeping its place: the
/// collection the route names (`collection`), else the one it had; a create's query
/// scope (`query`); and, for a record the session did not hold (`was_held` false), no
/// mark that the session deleted it.
async fn store_one(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (op, resource_type): (&Operation, &str),
    (id, data): (String, Value),
    (collection, query, was_held): (Option<Collection>, Option<&Map<String, Value>>, bool),
) {
    let rt = state.rt;
    let resource = StoredResource {
        resource_type: resource_type.into(),
        id,
        data,
    };
    let stored = resource.clone();
    // A route naming no collection keeps the one it had.
    let held = match &collection {
        None if rt.collections().has(session, &stored) => {
            state.get(session, resource_type, &stored.id).await
        }
        _ => None,
    };
    if state.upsert(session, resource).await {
        if state.tracks_deleted && !was_held {
            let scope = collection.as_ref().map(|c| c.path.as_str());
            deleted::clear(state, session, resource_type, scope, &stored).await;
        }
        let created = op.kind == OperationKind::Command(Effect::Create);
        match (collection, held) {
            (Some(collection), _) => {
                rt.collections()
                    .record(session, &stored, collection, created);
            }
            (None, Some(held)) => rt.collections().carry(session, &held, &stored),
            (None, None) if created => rt.collections().place_top_level(session, &stored),
            (None, None) => {}
        }
        match query {
            Some(query) if created => rt.collections().note_scope(session, &stored, query),
            _ => rt.collections().refresh_scope(session, &stored),
        }
    }
}

/// The members type `t`'s schema declares at its top level, as [`entity_fits`] reads
/// that schema; none for a type with nothing to check against.
pub(super) fn type_members(rt: &MockRuntime, t: &str) -> Vec<String> {
    let index = rt.index();
    let bundle = rt.bundle();
    let (read, shape) = match index.detail_reads.get(t) {
        Some(i) => (Some(*i), index.record_schemas.get(t)),
        None => match index.other_shapes.get(t) {
            Some((i, schema)) => (Some(*i), Some(schema)),
            None => (None, None),
        },
    };
    let Some(op) = read.and_then(|i| bundle.operations.get(i)) else {
        return Vec::new();
    };
    let schema = shape.or_else(|| op.success_output()?.schema.as_ref());
    Place::of(rt, schema)
        .parts
        .iter()
        .filter_map(|p| p.get("properties").and_then(Value::as_object))
        .flat_map(|p| p.keys().cloned())
        .collect()
}

/// What the record [`fitting_member`] finds must hold.
#[derive(Clone, Copy)]
pub(super) enum Holding<'a> {
    /// An update's target id: as its identity's key or the member the path names, or,
    /// with no identity, as any of its single values.
    Target(&'a str),
    /// A create's request: a single value it sent, under the same member name.
    Sent(&'a Value),
}

/// The one member of an object `answer` (`op`'s, of `status`), one level down, that is
/// a record of type `t` ([`entity_fits`]) and holds what `want` says: the record an
/// envelope the contract does not describe holds (`{result: {...}, success}`). A member
/// the answer's schema declares as another type's definition is not one. `None` when no
/// member or several are such records.
pub(super) fn fitting_member(
    rt: &MockRuntime,
    (op, status): (&Operation, u16),
    t: &str,
    answer: &Value,
    want: Holding<'_>,
) -> Option<Value> {
    let Value::Object(members) = answer else {
        return None;
    };
    let holds = |record: &Value| match want {
        Holding::Target(id) => {
            let is_id = |v: &Value| id_string(v).is_some_and(|v| v == id);
            match &op.identity {
                Some(identity) => [identity.field.as_str(), identity.path_member()]
                    .iter()
                    .any(|f| mb_mcm::member_at(record, f).is_some_and(is_id)),
                None => record.as_object().is_some_and(|r| r.values().any(is_id)),
            }
        }
        Holding::Sent(sent) => {
            let (Some(record), Some(sent)) = (record.as_object(), sent.as_object()) else {
                return false;
            };
            let declared = type_members(rt, t);
            let mut named = sent.iter().filter(|(name, value)| {
                !value.is_null()
                    && !value.is_object()
                    && !value.is_array()
                    && declared.contains(*name)
            });
            let first = named.next();
            first.is_some()
                && first
                    .into_iter()
                    .chain(named)
                    .all(|(name, value)| record.get(name) == Some(value))
        }
    };
    let bundle = rt.bundle();
    let index = rt.index();
    let schema = op.output_for_status(status).and_then(|o| o.schema.as_ref());
    let another_type = |member: &str| {
        let Some(declared) = schema.and_then(|s| property_schema(bundle, s, member)) else {
            return false;
        };
        let (_, defs) = schema_parts(bundle, Some(&declared));
        defs.iter()
            .filter_map(|d| index.types_by_def.get(*d))
            .any(|of| of != t)
    };
    let mut fitting = members.iter().filter(|(member, v)| {
        v.is_object() && holds(v) && !another_type(member) && entity_fits(rt, t, v)
    });
    match (fitting.next(), fitting.next()) {
        (Some((_, record)), None) => Some(record.clone()),
        _ => None,
    }
}
