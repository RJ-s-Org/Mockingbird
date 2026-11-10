//! Creates: repeated creates, taken and minted ids, and placing the new record.

use super::*;

/// The id of `generated`'s answer, when the request did not ask for it and the
/// session already holds a resource of this type under it, or the request carries it
/// (see [`request_values`]) as the id of a record of another type the session holds
/// (see [`world::held_elsewhere`]): a new message is not the channel it names. Another
/// type's id the request does not carry says nothing, and neither does a value the
/// request carries that is no held record's (`{"name": "my-bucket"}`).
pub(super) async fn taken_id(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    resource_type: &str,
    call: &OperationCall,
    generated: &GeneratedResponse,
) -> Option<String> {
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let id = mb_mcm::member_at(generated.body.as_ref()?, field).and_then(id_string)?;
    // A create asks for an id only under a member its request declares: one it does
    // not, the API mints over (see [`mb_engine::guards::asked_identity`]).
    let requested = match op.kind {
        OperationKind::Command(Effect::Create) => {
            mb_engine::guards::asked_identity(state.rt.bundle(), op, call)
        }
        _ => call
            .body
            .as_ref()
            .and_then(|b| mb_mcm::member_at(b, field))
            .and_then(id_string),
    };
    if requested.as_deref() == Some(id.as_str()) {
        return None;
    }
    let body = generated.body.as_ref()?;
    if state.get(session, resource_type, &id).await.is_some()
        || (request_values(op, call).contains(&id)
            && !keyed_apart(state.rt, op, resource_type, field, body)
            && world::held_elsewhere(state, session, resource_type, &id).await)
    {
        return Some(id);
    }
    None
}

/// Whether `answer` also holds, beside its identity `field` (`id`), the one scalar
/// member named for its own type `t` (`seat_id` of a `seats` record), and one of the
/// type's read schema, its record schema or `op`'s own declared success answer
/// declares both: then the contract keys the record apart from `id`, which may well
/// hold another record's id (the showing booked), and its naming a held record of
/// another type is no collision. What the answer alone holds, with no schema
/// declaring it, keys nothing apart.
pub(super) fn keyed_apart(
    rt: &MockRuntime,
    op: &Operation,
    t: &str,
    field: &str,
    answer: &Value,
) -> bool {
    let Value::Object(members) = answer else {
        return false;
    };
    if field != "id" {
        return false;
    }
    let own = mb_mcm::stem(t);
    let mut keys = members.iter().filter(|(member, value)| {
        let lower = member.to_ascii_lowercase();
        let named = lower
            .strip_suffix("_id")
            .or_else(|| lower.strip_suffix("id"))
            .filter(|name| !name.is_empty())
            .is_some_and(|name| mb_mcm::stem(name) == own);
        member.as_str() != field && named && (value.is_string() || value.is_number())
    });
    let (Some((key, _)), None) = (keys.next(), keys.next()) else {
        return false;
    };
    let index = rt.index();
    let read = index.detail_reads.get(t).and_then(|i| {
        rt.bundle()
            .operations
            .get(*i)?
            .success_output()?
            .schema
            .as_ref()
    });
    let answered = op.success_output().and_then(|o| o.schema.as_ref());
    [index.record_schemas.get(t), read, answered]
        .into_iter()
        .flatten()
        .any(|schema| {
            let declared = schema_members(rt, Some(schema));
            declared.contains(&"id") && declared.contains(&key.as_str())
        })
}

/// A different identity for `record`, the record a create's answer holds (in
/// `answer`, when it holds it in an envelope or an array), drawn for `attempt`.
pub(super) fn fresh_record(
    rt: &MockRuntime,
    op: &Operation,
    status: u16,
    (record, answer): (&Value, Option<&Value>),
    attempt: u32,
) -> Option<Value> {
    let Some((identity, answer)) = op
        .identity
        .as_ref()
        .filter(|i| i.record.is_some())
        .zip(answer)
    else {
        return rt
            .engine
            .fresh_identity_at(&rt.bundle, op, status, record, attempt);
    };
    let mut held = answer.clone();
    *identity.record_in_mut(&mut held)? = record.clone();
    let fresh = rt
        .engine
        .fresh_identity_at(&rt.bundle, op, status, &held, attempt)?;
    identity.record_in(&fresh).cloned()
}

/// What the path of `call`, a create of type `t`, says of the record it makes: each
/// (dotted) member with the path value it holds, the first for a member standing.
///
/// - The described parent's member, from the parent's path parameter (in the parent's
///   pattern).
/// - The member a described list scope of the type matches to a path parameter of the
///   create's route (a note's `noteable_iid` by `issue_iid`).
/// - A dotted member the description names (the parent's, a list scope's, a unique
///   set's member or scope) below an object member a path parameter is named for: the
///   parameter's words start with the object member's, and the rest, if any, start
///   with `id` or with the dotted member's own name (`team.slug` under `{team}`,
///   `owner.id` under `{owner_id_or_key}`).
///
/// A member is set at its dotted path, never as a literal `a.b` key, and the values are
/// as the path holds them: the create types them as its record declares.
pub(super) fn path_set(rt: &MockRuntime, t: &str, call: &OperationCall) -> Vec<(String, Value)> {
    let described = rt.described();
    let desc = described.type_desc(t);
    let mut out: Vec<(String, Value)> = Vec::new();
    let add = |member: &str, value: Value, out: &mut Vec<(String, Value)>| {
        if !out.iter().any(|(m, _)| m == member) {
            out.push((member.to_owned(), value));
        }
    };
    let parent = desc.and_then(|d| d.parent.as_ref());
    if let Some(parent) = parent
        && let (Some(member), Some(param)) = (&parent.member, &parent.path_param)
        && let Some(value) = call.path_params.get(param)
    {
        let value = match (&parent.pattern, id_string(value)) {
            (Some(pattern), Some(id)) if records::bare_id(pattern, &id).is_none() => {
                Value::String(records::patterned(pattern, &id))
            }
            _ => value.clone(),
        };
        add(member, value, &mut out);
    }
    let scopes = list_scopes(rt, t);
    for scope in &scopes {
        if let Some(value) = call.path_params.get(&scope.param) {
            add(&scope.member, value.clone(), &mut out);
        }
    }
    // The dotted members the description names, each under an object member a path
    // parameter may be named for.
    let mut named: Vec<&str> = parent
        .and_then(|p| p.member.as_deref())
        .into_iter()
        .collect();
    named.extend(scopes.iter().map(|s| s.member.as_str()));
    for set in desc.map(|d| d.unique.as_slice()).unwrap_or_default() {
        named.extend(set.members.iter().map(String::as_str));
        named.extend(set.scope.as_deref());
    }
    for member in named {
        let Some((head, rest)) = member.split_once('.') else {
            continue;
        };
        let found = call
            .path_params
            .iter()
            .find(|(param, _)| names_object(param, head, rest));
        if let Some((_, value)) = found {
            add(member, value.clone(), &mut out);
        }
    }
    out
}

/// Whether path parameter `param` names object member `head` by its member `rest`:
/// its words start with `head`'s, and the rest of them, if any, start with `id` or
/// with `rest`'s.
fn names_object(param: &str, head: &str, rest: &str) -> bool {
    let lower = |name: &str| -> Vec<String> {
        mb_mcm::words(name)
            .iter()
            .map(|w| w.to_ascii_lowercase())
            .collect()
    };
    let (param, head, rest) = (lower(param), lower(head), lower(rest));
    let Some(after) = param.strip_prefix(head.as_slice()) else {
        return false;
    };
    after.is_empty() || after.first().is_some_and(|w| w == "id") || after.starts_with(&rest)
}

/// The described scopes of the lists of type `t`.
pub(super) fn list_scopes<'r>(rt: &'r MockRuntime, t: &str) -> Vec<&'r mb_mcm::world::ScopeDesc> {
    let described = rt.described();
    rt.bundle()
        .operations
        .iter()
        .filter_map(|op| {
            let desc = described.op(&op.id)?;
            let item = desc.r#type.as_deref().or(op.resource_type.as_deref());
            let kind = desc.kind.or_else(|| mb_mcm::world::OpKind::of(op));
            (item == Some(t) && kind == Some(mb_mcm::world::OpKind::List))
                .then_some(desc.scope.as_ref()?)
        })
        .collect()
}

/// The values a request carries in its path and, as strings, anywhere in its body. A
/// create's path value naming the record itself, and a body member named as the
/// identity at any depth (`{"book": {"id": ..}}`, or `@id` for an XML attribute), are
/// its request for an id, and left out.
pub(super) fn request_values(op: &Operation, call: &OperationCall) -> HashSet<String> {
    fn strings(value: &Value, field: &str, depth: usize, out: &mut HashSet<String>) {
        match value {
            Value::String(s) if !s.is_empty() => {
                out.insert(s.clone());
            }
            Value::Array(items) if depth < 8 => {
                for item in items {
                    strings(item, field, depth + 1, out);
                }
            }
            Value::Object(members) if depth < 8 => {
                // `@id` is how an XML attribute `id` reads (see the SOAP mapper).
                let own = |name: &str| name.strip_prefix('@').unwrap_or(name) == field;
                for (_, member) in members.iter().filter(|(name, _)| !own(name)) {
                    strings(member, field, depth + 1, out);
                }
            }
            _ => {}
        }
    }
    let identity = op.identity.as_ref();
    let own_param = identity
        .and_then(|i| i.path_param.as_deref())
        .filter(|_| matches!(op.kind, OperationKind::Command(Effect::Create)));
    let mut out: HashSet<String> = call
        .path_params
        .iter()
        .filter(|(name, _)| Some(name.as_str()) != own_param)
        .filter_map(|(_, v)| id_string(v))
        .collect();
    let field = identity.map_or("id", |i| i.field.as_str());
    if let Some(body) = &call.body {
        strings(body, field, 0, &mut out);
    }
    out
}

/// What the session holds under the id of a create's answer when that id is the one the
/// request asked for by its body's identity field ([`mb_engine::guards::asked_identity`],
/// a member the request declares): the record, when it is in the collection the create
/// runs in ([`in_create_collection`]), so that the create repeats it; else that a record
/// elsewhere holds the id. Storing the answer would replace that record either way, which
/// no create does. A create
/// sent with a declared idempotency key may be a retry the API answers as the first, and
/// an HTTP `PUT` is an upsert by its method: neither is one. (A create naming its id in
/// its path gets a fresh one drawn first, see [`unique_create_id`].)
pub(super) async fn repeated_record(
    state: &mut StateAccess<'_>,
    (session, op, resource_type, call): (&SessionKey, &Operation, &str, &OperationCall),
    generated: &GeneratedResponse,
) -> Option<Repeated> {
    let rt = state.rt;
    if !matches!(op.kind, OperationKind::Command(Effect::Create))
        || matches!(
            op.binding,
            Binding::Http {
                method: mb_mcm::HttpMethod::Put,
                ..
            }
        )
        || mb_engine::guards::carries_an_idempotency_key(
            rt.bundle(),
            op,
            call,
            super::replays::header(rt, op),
        )
    {
        return None;
    }
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let id = mb_mcm::member_at(generated.body.as_ref()?, field).and_then(id_string)?;
    if mb_engine::guards::asked_identity(rt.bundle(), op, call).as_deref() != Some(id.as_str()) {
        return None;
    }
    let held = state.get(session, resource_type, &id).await?;
    Some(if in_create_collection(rt, session, op, call, &held) {
        Repeated::Here(held)
    } else {
        Repeated::Elsewhere(id)
    })
}

/// What [`repeated_record`] found under the id a create asked for.
pub(super) enum Repeated {
    /// The record, in the collection the create runs in: the create repeats it.
    Here(StoredResource),
    /// A record under that id the create does not repeat, in another collection (or
    /// placed nowhere known, of a type also created under a parent): the create is new
    /// there, but storing it would replace that record, so it is served unstored.
    Elsewhere(String),
}

/// Drops every effect of `generated`, a create's answer that is not stored as it
/// repeats the record held under `id` (see [`repeated_record`]), and says so in its
/// outcomes: a duplicate changes nothing.
pub(super) fn drop_all_effects(
    rt: &MockRuntime,
    op: &Operation,
    id: &str,
    generated: &mut GeneratedResponse,
) {
    const REASON: &str = "the create repeats a stored record's id: nothing is stored";
    let dropped = std::mem::take(&mut generated.effects);
    generated.dropped_effects += dropped.len();
    generated
        .effect_outcomes
        .extend(dropped.into_iter().map(|effect| EffectOutcome {
            effect,
            rejected: Some(REASON.to_string()),
            merged: None,
            applied: Vec::new(),
        }));
    tracing::info!(
        mock = %rt.name,
        operation = %op.id,
        id = %log_capped(id),
        "create not stored: its id is a stored record's"
    );
}

/// Whether `set`, what an effect sets a member to, repeats `asked`, what the request
/// gives that member: equal (numbers by value), or, for objects, holding at any depth a
/// value the request holds at the same place (`{"parent": {"id": "tr_1"}, "note": ..}`).
pub(super) fn copies_value(set: &Value, asked: &Value, depth: usize) -> bool {
    match (set, asked) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Object(set), Value::Object(asked)) if depth < 8 => set
            .iter()
            .any(|(k, v)| asked.get(k).is_some_and(|a| copies_value(v, a, depth + 1))),
        _ => set == asked,
    }
}

/// Splits `effects`, those of a create of `resource_type` whose answer's record is
/// `created`, into what may apply and what is rejected. On another record of its own
/// type, a member whose value copies what the request gives that member (see
/// [`copies_value`]) is the request copied onto a sibling, whatever the value: the
/// sibling's own id is copied as much as any (Mollie's second payment setting the
/// first one's amount, a request's `parent_id` making its parent its own parent).
/// When every copied value names that sibling (the request's reference to it), only
/// those members are taken out and the rest applies (a reply's count on the record it
/// replies to). When any copied value does not, the effect describes the new record
/// under a sibling's id (its alias, its name, its defaults): it is rejected whole, so
/// none of the new record's defaults land on the sibling. An effect copying nothing
/// applies (clearing `is_default` on the old default), and an effect with nothing left
/// is rejected. Effects on the created record and on other types are left as they
/// are. Each rejected part is returned as an effect of the members taken out.
pub(super) fn sibling_effects(
    resource_type: &str,
    created: Option<&str>,
    body: Option<&Value>,
    effects: Vec<ResourceEffect>,
) -> (Vec<ResourceEffect>, Vec<ResourceEffect>) {
    let empty = Map::new();
    let body = match body {
        Some(Value::Object(members)) => members,
        _ => &empty,
    };
    let mut kept = Vec::with_capacity(effects.len());
    let mut dropped = Vec::new();
    for mut e in effects {
        if e.resource_type != resource_type || created == Some(e.id.as_str()) {
            kept.push(e);
            continue;
        }
        let (copied, rest): (Map<String, Value>, Map<String, Value>) = std::mem::take(&mut e.set)
            .into_iter()
            .partition(|(k, v)| body.get(k).is_some_and(|asked| copies_value(v, asked, 0)));
        if copied.values().any(|v| !names_record(v, &e.id, 0)) {
            let mut set = copied;
            set.extend(rest);
            dropped.push(ResourceEffect { set, ..e });
            continue;
        }
        if !copied.is_empty() {
            dropped.push(ResourceEffect {
                set: copied,
                ..e.clone()
            });
        }
        if !rest.is_empty() {
            kept.push(ResourceEffect { set: rest, ..e });
        }
    }
    (kept, dropped)
}

/// Whether `value` names the record under `id`: that id itself (a string, or a number
/// written as one), or an object holding it at any depth (`{"parent": {"id": "tr_1"}}`).
fn names_record(value: &Value, id: &str, depth: usize) -> bool {
    match value {
        Value::Object(members) if depth < 8 => {
            members.values().any(|v| names_record(v, id, depth + 1))
        }
        _ => id_string(value).as_deref() == Some(id),
    }
}

/// Gives a create's answer a fresh identity when its generated one is taken (see
/// [`taken_id`]).
pub(super) async fn unique_create_id(
    rt: &MockRuntime,
    state: &mut StateAccess<'_>,
    (session, op, resource_type, call): (&SessionKey, &Operation, &str, &OperationCall),
    generated: &mut GeneratedResponse,
    answer: Option<&Value>,
) {
    if taken_id(state, session, op, resource_type, call, generated)
        .await
        .is_none()
    {
        return;
    }
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let Some(body) = generated.body.as_ref() else {
        return;
    };
    let carried = request_values(op, call);
    for _ in 0..3 {
        let Some(fresh) = fresh_record(rt, op, generated.status, (body, answer), 0) else {
            return;
        };
        let Some(fresh_id) = mb_mcm::member_at(&fresh, field).and_then(id_string) else {
            return;
        };
        if !carried.contains(&fresh_id)
            && state.get(session, resource_type, &fresh_id).await.is_none()
        {
            generated.body = Some(fresh);
            return;
        }
    }
}

/// Gives a create's 2xx answer an identity when its record holds none (or `null`) and
/// the request asked for none: no client-chosen id (see [`mb_engine::guards::asked_identity`])
/// and no path value naming the record. The faker draws it as the record's schema declares
/// the member (its `format`, `pattern`), each retry anew, until it is one the session
/// holds no record of the type under and the request does not carry; with none to be
/// had, or a member the schema does not declare, the answer stays as it is.
pub(super) async fn mint_create_id(
    rt: &MockRuntime,
    state: &mut StateAccess<'_>,
    (session, op, resource_type, call): (&SessionKey, &Operation, &str, &OperationCall),
    generated: &mut GeneratedResponse,
    answer: Option<&Value>,
) {
    let identity = op.identity.as_ref();
    let field = identity.map_or("id", |i| i.field.as_str());
    let Some(body) = generated.body.as_ref().filter(|b| b.is_object()) else {
        return;
    };
    if mb_mcm::member_at(body, field).is_some_and(|v| !v.is_null()) {
        return;
    }
    let named_by_path = identity
        .and_then(|i| i.path_param.as_deref())
        .is_some_and(|param| call.path_params.contains_key(param));
    if named_by_path || mb_engine::guards::asked_identity(rt.bundle(), op, call).is_some() {
        return;
    }
    let carried = request_values(op, call);
    for attempt in 0..3 {
        let draw = fresh_record(rt, op, generated.status, (body, answer), attempt);
        let Some(drawn) = draw
            .as_ref()
            .and_then(|d| mb_mcm::member_at(d, field))
            .and_then(id_string)
        else {
            return;
        };
        // A type whose identity has a pattern holds the id in that form; one its
        // schema refuses (a pattern past the member's `maxLength`) is not given.
        let (draw, drawn) = match records::in_pattern(rt, resource_type, &drawn) {
            Some(formed) => {
                let mut record = draw;
                if let Some(record) = record.as_mut() {
                    mb_mcm::set_member_at(record, field, Value::String(formed.clone()));
                }
                if !identity_holds(rt, op, resource_type, field, &formed) {
                    return;
                }
                (record, formed)
            }
            None => (draw, drawn),
        };
        if !carried.contains(&drawn) && state.get(session, resource_type, &drawn).await.is_none() {
            tracing::debug!(
                mock = %rt.name,
                operation = %op.id,
                id = %log_capped(&drawn),
                "create answered with no identity: one was drawn"
            );
            generated.body = draw;
            return;
        }
    }
}

/// Whether type `t`'s records hold `id` as their identity `field` as `op`'s answer and
/// the type's read declare it (any id when neither declares the member).
fn identity_holds(rt: &MockRuntime, op: &Operation, t: &str, field: &str, id: &str) -> bool {
    let bundle = rt.bundle();
    let index = rt.index();
    let read = index
        .detail_reads
        .get(t)
        .and_then(|i| bundle.operations.get(*i)?.success_output()?.schema.as_ref());
    let own = op.success_output().and_then(|o| o.schema.as_ref());
    let held = op.identity.as_ref().filter(|i| i.record.is_some());
    let own = match (own, held) {
        (Some(schema), Some(identity)) => record_schema_of(bundle, identity, schema),
        (schema, _) => schema.cloned(),
    };
    let value = Value::String(id.to_string());
    [index.record_schemas.get(t).cloned(), read.cloned(), own]
        .into_iter()
        .flatten()
        .filter_map(|schema| mb_engine::consistency::property_schema(bundle, &schema, field))
        .all(|declared| {
            mb_engine::SchemaValidator::for_output(&bundle.with_defs(&declared))
                .is_ok_and(|v| v.is_valid_output(&value))
        })
}

/// Keeps an action's new record off a stored one: an answer whose id is taken
/// takes a fresh one, each retry drawing anew. With none to be had it is not
/// stored, which the request record says, and its effects are dropped. So is an
/// effect naming the id it moved from, which meant the answer's own record.
/// Returns whether the answer can be stored.
pub(super) async fn place_new_record(
    rt: &MockRuntime,
    state: &mut StateAccess<'_>,
    (session, op, resource_type, call): (&SessionKey, &Operation, &str, &OperationCall),
    generated: &mut GeneratedResponse,
    answer: Option<&Value>,
) -> bool {
    let Some(id) = taken_id(state, session, op, resource_type, call, generated).await else {
        return true;
    };
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let mut fresh = None;
    let carried = request_values(op, call);
    if let Some(body) = generated.body.as_ref() {
        for attempt in 0..3 {
            let draw = fresh_record(rt, op, generated.status, (body, answer), attempt);
            let drawn = draw
                .as_ref()
                .and_then(|d| mb_mcm::member_at(d, field))
                .and_then(id_string);
            let Some(drawn) = drawn else {
                break;
            };
            if !carried.contains(&drawn)
                && state.get(session, resource_type, &drawn).await.is_none()
            {
                fresh = draw;
                break;
            }
        }
    }
    let stored = fresh.is_some();
    if let Some(fresh) = fresh {
        generated.body = Some(fresh);
    }
    let (dropped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut generated.effects)
        .into_iter()
        .partition(|e| !stored || (e.resource_type == resource_type && e.id == id));
    generated.effects = kept;
    generated.dropped_effects += dropped.len();
    let rejected = |effect, reason: &str| EffectOutcome {
        effect,
        rejected: Some(reason.to_string()),
        merged: None,
        applied: Vec::new(),
    };
    let reason = if stored {
        "the answer moved off this id"
    } else {
        "the answer was not stored"
    };
    generated
        .effect_outcomes
        .extend(dropped.into_iter().map(|effect| rejected(effect, reason)));
    if !stored {
        tracing::warn!(
            mock = %rt.name,
            operation = %op.id,
            id = %log_capped(&id),
            "new record not stored: its id is taken and no other could be drawn"
        );
        let note = ResourceEffect {
            resource_type: resource_type.to_string(),
            id,
            set: serde_json::Map::new(),
        };
        generated.effect_outcomes.push(rejected(
            note,
            "new record not stored: its id is taken and no other could be drawn",
        ));
    }
    stored
}

#[cfg(test)]
mod path_set_tests {
    use super::names_object;

    #[test]
    fn a_parameter_names_an_object_member_by_its_words_then_id_or_the_member() {
        assert!(names_object("owner", "owner", "id"));
        assert!(names_object("owner_id_or_key", "owner", "id"));
        assert!(names_object("ownerId", "owner", "id"));
        assert!(names_object("team_slug", "team", "slug"));
        assert!(!names_object("owner_name", "owner", "id"));
        assert!(!names_object("ownership", "owner", "id"));
        assert!(!names_object("id", "owner", "id"));
    }
}
