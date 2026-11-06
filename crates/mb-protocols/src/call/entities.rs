//! Storing the records an answer holds: wrapped ids, own reads and nested entities.

use super::*;

/// Fields that identify a wrapped created object, in order: Slack's
/// message is keyed by `ts`, Twilio's resources by `sid`.
pub(super) const WRAPPED_IDS: [&str; 4] = ["id", "ts", "sid", "uuid"];

/// Stores each object a create's answer wraps under a member `<x>` (Slack's
/// `chat.postMessage` → `{ok, ts, message: {...}}`) as a resource of type
/// `<x>s` when a read lists `<x>s` (`conversations.history`'s `messages`),
/// so that read is generated knowing it. The create's own member, one of the
/// operation's own type (`resource_type`), replaces what the session holds.
/// Any other is a mention, stored under what [`seed_entities`] requires of
/// one: it replaces what is held only when that cannot key it, having no
/// `id` or `number` (Slack's `ts`), and otherwise is stored only when new.
/// The member `stored`, the record the operation's identity is of, was stored
/// with the create.
pub(super) async fn store_wrapped(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (resource_type, stored): (Option<&str>, Option<&str>),
    generated: &GeneratedResponse,
    schema: Option<&Value>,
    collection: Option<&Collection>,
) {
    let Some(Value::Object(body)) = generated.body.as_ref() else {
        return;
    };
    let rt = state.rt;
    let answer = Place::of(rt, schema);
    let listed = &rt.index().listed;
    for (member, value) in body {
        let plural = format!("{member}s");
        if !value.is_object() || !listed.contains(&plural) || stored == Some(member.as_str()) {
            continue;
        }
        let Some(id) = WRAPPED_IDS
            .iter()
            .find_map(|f| value.get(*f).and_then(id_string))
        else {
            continue;
        };
        let own = resource_type == Some(member.as_str()) || resource_type == Some(plural.as_str());
        let unkeyed = value.get("id").is_none() && value.get("number").is_none();
        let resource = StoredResource {
            resource_type: plural,
            id,
            data: value.clone(),
        };
        if own {
            let stored = resource.clone();
            let took = state.upsert(session, resource).await;
            if took && state.tracks_deleted {
                let scope = collection.map(|c| c.path.as_str());
                deleted::clear(state, session, &stored.resource_type, scope, &stored).await;
            }
            if took && let Some(collection) = collection {
                rt.collections()
                    .record(session, &stored, collection.clone(), true);
            }
        } else if rt.consistency
            && !transient_fallback(generated)
            && entity_fits_from(
                rt,
                &resource.resource_type,
                &resource.data,
                // The member's own inline schema says what it is too.
                answer
                    .property(member)
                    .filter(|s| Place::of(rt, Some(s)).defs.is_empty()),
            )
        {
            if unkeyed {
                let gone = state.tracks_deleted
                    && deleted::mention_is_deleted(
                        state,
                        session,
                        &resource.resource_type,
                        &resource.id,
                    )
                    .await;
                if !gone {
                    state.upsert(session, resource).await;
                }
            } else {
                state.insert_seeded(session, resource).await;
            }
        }
    }
}

/// Stores a generated read by id as the resource it read, when it answers
/// with its type's by-path schema: otherwise a later answer's mention would
/// be stored and change what repeating this read returns. It goes under the
/// target id when the path names the identity. A path parameter naming no
/// field goes under its own id, or under the parameter's value when that is
/// what its `id` holds. After an earlier parameter (`/repos/{o}/{r}`,
/// `/orgs/{o}/teams/{t}`) that value is no key, or one owner's answer would
/// answer for another's: such an answer stores nothing, the response cache
/// keeping its repeat. So does one naming its target after an owner that it
/// does not show (see [`shows_owner`]).
/// A route read by another member (`/issues/{number}`, `/teams/{team_slug}`) goes
/// under its answer's own `id` when that member holds the path's value, which
/// the engine sets there, and the answer shows the path's owner, if it has one.
pub(super) async fn seed_own_read(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    op: &Operation,
    call: &OperationCall,
    target_id: Option<&str>,
    generated: &GeneratedResponse,
) {
    let (true, Some(t), Some(target_id), Some(identity), Some(body)) = (
        matches!(op.kind, OperationKind::Query),
        op.resource_type.as_deref(),
        target_id,
        op.identity.as_ref(),
        generated.body.as_ref(),
    ) else {
        return;
    };
    let index = rt.index();
    let detail_schema = index.detail_reads.get(t).and_then(|i| {
        rt.bundle()
            .operations
            .get(*i)?
            .success_output()?
            .schema
            .as_ref()
    });
    let own_schema = op
        .output_for_status(generated.status)
        .and_then(|o| o.schema.as_ref());
    if detail_schema.is_none() || detail_schema != own_schema {
        return;
    }
    let named = identity.path_member();
    let id = if named != identity.field {
        // Read by another member than its key (`/issues/{issue_iid}` by `iid`): the path's
        // value is in that member, and the record goes under its own key.
        let value = |member: &str| mb_mcm::member_at(body, member).and_then(id_string);
        value(&identity.field).filter(|_| value(named).as_deref() == Some(target_id))
    } else if identity.path_field.is_some() {
        // A path that may hold the key or another member (Jira's `issueIdOrKey`) tells
        // neither apart, so a generated read stores nothing.
        return;
    } else if path_names_identity(identity) {
        Some(target_id.to_string())
    } else {
        mb_mcm::member_at(body, &identity.field)
            .and_then(id_string)
            .filter(|id| id != target_id || !follows_a_param(op, identity))
    };
    let Some(id) = id else {
        return;
    };
    let resource = StoredResource {
        resource_type: t.to_string(),
        id,
        data: body.clone(),
    };
    if (named != identity.field || !path_names_identity(identity))
        && let Some(owner) = owner_of(op, call)
        && !shows_owner(rt, &resource, &owner)
    {
        return;
    }
    if state.insert_seeded(session, resource.clone()).await {
        rt.own_reads().remember(session, &resource);
        if let Some(collection) = collection_of(op, call) {
            rt.collections().remember(session, &resource, collection);
        }
        return;
    }
    // What is held is shorter than this read (see [`is_summary`]): this answer
    // was generated knowing it, and a later read can be served.
    if let Some(held) = state.get(session, t, &resource.id).await
        && !entity_fits(rt, t, &held.data)
        && entity_fits(rt, t, &resource.data)
        && state.upsert(session, resource.clone()).await
    {
        rt.own_reads().remember(session, &resource);
        if let Some(collection) = collection_of(op, call) {
            rt.collections().remember(session, &resource, collection);
        }
    }
}

/// How deep an answer is walked for entities: containers below its root.
pub(super) const MAX_ENTITY_DEPTH: usize = 6;

/// Entities one answer may seed.
pub(super) const MAX_ENTITIES: usize = 100;

/// Stores what a 2xx answer mentions so that a later read returns it: each
/// object below the answer's root with a known type (see
/// [`entity_type`]), keyed by the field its type's read by path goes by, else an
/// `id`, a `number` or another field that type is identified by,
/// and a value its type's read would serve or a shorter form
/// of one (see [`entity_fits`], [`is_summary`]). What the session already
/// holds is left as it is.
pub(super) async fn seed_entities(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    body: &Value,
    schema: Option<&Value>,
    created: Option<(&str, &Collection)>,
) {
    let mut walk = EntityWalk {
        rt,
        considered: 0,
        found: Vec::new(),
    };
    walk.visit(body, schema, "", 0);
    // One answer's records read back in its order.
    let found = walk.found;
    let stored = state.insert_seeded_all(session, found.clone()).await;
    for (resource, stored) in found.iter().zip(stored) {
        if stored
            && let Some((_, collection)) = created.filter(|(t, _)| *t == resource.resource_type)
        {
            rt.collections()
                .remember(session, resource, collection.clone());
        }
    }
}

/// One walk of an answer for [`seed_entities`].
pub(super) struct EntityWalk<'a> {
    pub(super) rt: &'a MockRuntime,
    /// Objects taken for entities, stored or refused.
    pub(super) considered: usize,
    pub(super) found: Vec<StoredResource>,
}

impl<'a> EntityWalk<'a> {
    pub(super) fn visit(
        &mut self,
        value: &Value,
        schema: Option<&'a Value>,
        member: &str,
        depth: usize,
    ) {
        if depth > MAX_ENTITY_DEPTH || self.considered >= MAX_ENTITIES {
            return;
        }
        let bundle: &'a mb_mcm::Bundle = self.rt.bundle();
        let (parts, defs) = schema_parts(bundle, schema);
        match value {
            Value::Array(items) => {
                let item = parts.iter().find_map(|p| p.get("items"));
                for value in items {
                    self.visit(value, item, member, depth + 1);
                }
            }
            Value::Object(fields) => {
                if depth > 0 {
                    self.consider(value, schema, &defs, member);
                }
                for (field, value) in fields {
                    if !value.is_object() && !value.is_array() {
                        continue;
                    }
                    let inner = parts
                        .iter()
                        .find_map(|p| p.get("properties")?.get(field.as_str()));
                    // `data` in Stripe's `charges: {data: [...]}` holds `charges`.
                    let named = if ENVELOPE_KEYS.contains(&field.as_str()) {
                        member
                    } else {
                        field
                    };
                    self.visit(value, inner, named, depth + 1);
                }
            }
            _ => {}
        }
    }

    pub(super) fn consider(
        &mut self,
        value: &Value,
        schema: Option<&Value>,
        defs: &[&str],
        member: &str,
    ) {
        let index = self.rt.index();
        let def = defs
            .iter()
            .copied()
            .find(|d| index.types_by_def.contains_key(*d));
        let Some(resource_type) = entity_type(index, def, member) else {
            return;
        };
        // As the type's routes key it, so a record is stored once, else as a list
        // with no identity of its type keys one.
        let own = index
            .detail_reads
            .get(&resource_type)
            .and_then(|i| self.rt.bundle().operations.get(*i)?.identity.as_ref())
            .map(|identity| identity.field.as_str());
        let identity = index.identity_fields(&resource_type).iter();
        let Some(id) = own
            .into_iter()
            .chain(["id", "number"])
            .chain(identity.map(String::as_str))
            .find_map(|f| mb_mcm::member_at(value, f).and_then(id_string))
        else {
            return;
        };
        if self.considered >= MAX_ENTITIES {
            return;
        }
        self.considered += 1;
        // An inline object's own schema says what it is too; a `$ref` is the
        // definition's, which may be another type's.
        let came_from = schema.filter(|_| defs.is_empty());
        if entity_fits_from(self.rt, &resource_type, value, came_from)
            || is_summary(self.rt, &resource_type, defs, value)
        {
            self.found.push(StoredResource {
                resource_type,
                id,
                data: value.clone(),
            });
        }
    }
}

/// Whether `value` suits resource type `t`: its by-path read would serve it
/// (stored unservable, a later read would fall to the faker), else its list
/// items or singleton read accept it. A type with neither has nothing to
/// check against, so nothing suits it. An object must also show it is one: a
/// schema requiring nothing would take any object (an acknowledgement for the
/// settings it updated), so one of its members must be a member the type's
/// schema declares (see [`shows_its_type`]).
pub(super) fn entity_fits(rt: &MockRuntime, t: &str, value: &Value) -> bool {
    entity_fits_from(rt, t, value, None)
}

/// [`entity_fits`] of `value` found at a place of schema `came_from` in an answer:
/// a member that place declares shows the type as well (Slack's `message`, an
/// inline object, of a `messages` type whose read is another API's).
pub(super) fn entity_fits_from(
    rt: &MockRuntime,
    t: &str,
    value: &Value,
    came_from: Option<&Value>,
) -> bool {
    let index = rt.index();
    let bundle = rt.bundle();
    let (read, shape) = match index.detail_reads.get(t) {
        // A record its read answers in an envelope or an array suits its own schema.
        Some(i) => (Some(*i), index.record_schemas.get(t)),
        None => match index.other_shapes.get(t) {
            Some((i, schema)) => (Some(*i), Some(schema)),
            None => (None, None),
        },
    };
    let Some(op) = read.and_then(|i| bundle.operations.get(i)) else {
        return false;
    };
    let schema = shape.or_else(|| op.success_output()?.schema.as_ref());
    shows_its_type(rt, schema, came_from, value)
        && rt.engine.serve_stored(bundle, op, value, shape).is_some()
}

/// Whether `value` holds a member the type's `schema` declares, or one `came_from`
/// (the place it was found at) declares. True for anything but an object, and for a
/// type whose schema declares no member (a map, a free-form object, `properties: {}`):
/// there is nothing to tell by.
pub(super) fn shows_its_type(
    rt: &MockRuntime,
    schema: Option<&Value>,
    came_from: Option<&Value>,
    value: &Value,
) -> bool {
    let Value::Object(members) = value else {
        return true;
    };
    let declared = |schema: Option<&Value>| -> Vec<String> {
        Place::of(rt, schema)
            .parts
            .iter()
            .filter_map(|p| p.get("properties").and_then(Value::as_object))
            .flat_map(|p| p.keys().cloned())
            .collect()
    };
    let own = declared(schema);
    if own.is_empty() {
        return true;
    }
    let found = came_from.map(|s| declared(Some(s))).unwrap_or_default();
    members.keys().any(|k| own.contains(k) || found.contains(k))
}

/// Whether `value`, shaped by `defs`, is a shorter form of resource type `t`:
/// valid as one of those definitions, which its by-path read's schema is
/// composed of (a search's `FlightOffer`, of `Offer`). It is stored though
/// that read would not serve it: the read is generated knowing it and keeps
/// what it holds, and the answer takes its place (see [`seed_own_read`]).
pub(super) fn is_summary(rt: &MockRuntime, t: &str, defs: &[&str], value: &Value) -> bool {
    let bundle = rt.bundle();
    let Some(op) = rt
        .index()
        .detail_reads
        .get(t)
        .and_then(|i| bundle.operations.get(*i))
    else {
        return false;
    };
    let read = op.success_output().and_then(|o| o.schema.as_ref());
    let (_, read_defs) = schema_parts(bundle, read);
    let Some(def) = defs.iter().find(|d| read_defs.contains(d)) else {
        return false;
    };
    let shape = serde_json::json!({"$ref": format!("#/$defs/{def}")});
    rt.engine
        .serve_stored(bundle, op, value, Some(&shape))
        .is_some()
}
