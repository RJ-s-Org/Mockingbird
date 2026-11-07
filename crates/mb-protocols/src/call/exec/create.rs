//! Creates (step 2 of the plan): the request's record, its minted id, its initial state
//! and the members [`mb_engine::Engine::fill`] writes, stored and answered.
//!
//! - The record is what the request holds at the described `request.record`, else the
//!   body: the members the type's records declare (all of them when it declares none),
//!   with those of an object member the type does not declare itself (an input wrapper,
//!   GraphQL's `input: {...}`) one level down, and the path values naming its parent or
//!   a member it declares.
//! - Its id is the one the request asks for (a client-chosen id, a path value naming the
//!   record), else one minted in the identity header the answer names it in, else one
//!   drawn as the type declares its identity, in its pattern.
//! - What the type's reads require (its read by id and every list of it, so that any
//!   later read renders it), and what the create's answer requires when that is the
//!   record, that nothing above gives is filled ([`gaps`]: a member, or what an object
//!   member the request sent lacks), but the server's own members: the id,
//!   the lifecycle member a described `initial` sets, the derived members. The
//!   lifecycle's `initial` is then set. Before fill, a member the request did not send
//!   whose schema decides its value (a `const`, a one-value `enum`: GraphQL's
//!   `__typename`; a `default`) takes that value, at any depth the record holds
//!   ([`preset`]), optional members included.
//! - Before it is stored, the record is checked against every read of its type
//!   ([`read_schemas`]), as those reads render it: one a read would not render (a
//!   reference where the read holds an object, an array item lacking a required id)
//!   is never stored, and the create generates (`record_invalid`, with `render_error`
//!   naming the first violation). So does one whose answer does not render.
//! - The answer is [`super::render`]'s (no body, the record, or an acknowledgement
//!   drawn from its own schema with the record's members laid over it); a described
//!   long-running answer holds the record as its result, and is done: the record is
//!   stored when the create is accepted.
//!
//! Bulk creates, a create naming a record the session holds, and one repeating a value
//! the contract says is unique that the generated path refuses (`repeated_value`), go on
//! to generation too.

use mb_engine::consistency::{property_schema, required_members};
use mb_engine::{FillContext, GeneratedResponse};
use mb_mcm::Operation;
use mb_mcm::world::OpKind;
use serde_json::{Map, Value};
use tracing::Instrument;

use super::writes::written;
use super::{Fallback, Held, Ran, Write, answered, commit, fell_back};
use crate::call::bundle_index::record_schema_of;
use crate::call::entities::type_check;
use crate::call::owners::schema_members;
use crate::call::writes::type_members;
use crate::call::{
    CallPlan, OperationCall, StateAccess, derived, header_ids, id_string, lifecycle, records,
    repeats, request_values, serve_held, state_lists, state_span, target_id, value_index,
};

/// Whether the executor takes these calls.
pub(super) const EXECUTES: bool = true;

/// Executes the call.
pub(super) async fn run(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: Held<'_>,
    response_schema: Option<&Value>,
) -> Ran {
    let rt = state.rt;
    let op = plan.op;
    let kind = OpKind::Create;
    let fall = |reason: &str| {
        fell_back(rt, plan, kind, reason);
        Ran::Generate(None)
    };
    let desc = rt.described().op(&op.id);
    if desc.is_some_and(|d| {
        d.request.as_ref().is_some_and(|r| r.records.is_some())
            || d.answer.as_ref().is_some_and(|a| a.records.is_some())
    }) {
        return fall("bulk");
    }
    let Some(t) = desc
        .and_then(|d| d.r#type.as_deref())
        .or(op.resource_type.as_deref())
    else {
        return fall(Fallback::NoIdentity.as_str());
    };
    // A create naming a record the session holds repeats it; one whose key holds
    // another owner's record does not make it.
    let named = target_id(op, call);
    if named.is_some() && held.target_id.is_none() {
        return fall("foreign_key");
    }
    let asked = named.or_else(|| mb_engine::guards::asked_identity(rt.bundle(), op, call));
    let repeats = match asked.as_deref() {
        Some(id) if held.target.is_none() => records::held_by_key(state, &plan.session, t, id)
            .instrument(state_span())
            .await
            .is_some(),
        _ => held.target.is_some(),
    };
    if repeats {
        return fall("held");
    }
    // A value the contract says is unique, held by a record in the create's collection:
    // the generated path refuses it ([`repeats::enforced`]), so it takes the create.
    if refuses_a_repeat(state, plan, call, t)
        .instrument(state_span())
        .await
    {
        return fall("repeated_value");
    }
    let shape = Shape::of(rt, op, t, response_schema);
    let request = call
        .body
        .as_ref()
        .map(|b| records::request_record(rt, op, b));
    let Some(mut record) = shape.request_record(request) else {
        return fall("no_record");
    };
    shape.path_members(rt, t, call, &mut record);

    // The id: the one asked for, else minted in the header the answer names it in, else
    // drawn as the type declares it.
    let field = identity_member(rt, op, t);
    let status = op.success_output().map_or(200, |o| match o.status {
        mb_mcm::StatusSelector::Code(code) => code,
        _ => 200,
    });
    let mut minted_headers = Vec::new();
    let id = match asked {
        Some(asked) => match mb_mcm::member_at(&Value::Object(record.clone()), field) {
            Some(own) if id_string(own).as_deref() == Some(asked.as_str()) => own.clone(),
            _ => call
                .path_params
                .values()
                .find(|v| id_string(v).as_deref() == Some(asked.as_str()))
                .cloned()
                .unwrap_or(Value::String(asked)),
        },
        None => {
            let mut probe = probe(status);
            let session = &plan.session;
            let page_url = plan.page_url.as_deref();
            let minted = header_ids::mint(rt, state, (session, op, t, call), &mut probe, page_url)
                .instrument(state_span())
                .await;
            match minted {
                Some(id) => {
                    minted_headers = probe.headers;
                    id
                }
                None => {
                    let carried = request_values(op, call);
                    let drawn =
                        records::mint_one(state, session, (t, field), &carried, &mut Vec::new())
                            .instrument(state_span())
                            .await;
                    match drawn {
                        Some(id) => id,
                        None => return fall("no_id"),
                    }
                }
            }
        }
    };
    let Some(key) = id_string(&id) else {
        return fall("no_id");
    };
    let mut data = Value::Object(record);
    if !mb_mcm::set_member_at(&mut data, field, id) {
        return fall("no_id");
    }

    // The members nobody else decides, then the initial state.
    let owned = server_owned(rt, t, field, request);
    // A member the request did not send whose schema decides its value (a `const`, a
    // one-value `enum`: GraphQL's `__typename`; a `default`) takes it, at any depth.
    for schema in &shape.schemas {
        preset(rt.bundle(), schema, &mut data, &owned, GAP_DEPTH);
    }
    // So does one whose request schema gives the value the server assumes when it is
    // left out (harvest's `is_active: {default: true}`), where the record holds it.
    preset_from_request(rt, op, &shape, &mut data, &owned);
    let missing = gaps(rt, &shape.schemas, &data, &owned);
    let mut usage = mb_llm::Usage::default();
    if !missing.is_empty()
        && let Value::Object(known) = &mut data
    {
        let names: Vec<String> = missing.iter().map(|(name, _)| name.clone()).collect();
        let filled = plan
            .engine
            .fill(
                rt.bundle(),
                &fill_schema(&missing),
                known,
                &names,
                FillContext { op, call },
            )
            .await;
        usage = filled.usage;
        fill_into(known, filled.members);
    }
    lifecycle::initial(rt, t, &mut data, request);

    // Its derived members are answered from what state holds (its children, if any),
    // never stored; the record as stored is answered when the filled one does not render.
    let filled = derived::filled_new(
        state,
        &plan.session,
        &mb_state::StoredResource {
            resource_type: t.to_owned(),
            id: key.clone(),
            data: data.clone(),
        },
    )
    .instrument(state_span())
    .await;
    // A record a later read would not render is never stored: the create generates.
    if let Some(why) = unreadable(rt, t, &data, filled.as_ref().map(|f| &f.data)) {
        invalid(rt, plan, &why);
        return Ran::Generate(None);
    }
    let answer = filled
        .and_then(|f| written(rt, plan, call, (&key, &f.data), response_schema))
        .or_else(|| written(rt, plan, call, (&key, &data), response_schema))
        .or_else(|| enveloped(rt, op, (&key, t, &data), response_schema));
    let Some(mut answer) = answer else {
        invalid(rt, plan, "answer does not render");
        return Ran::Generate(None);
    };
    answer.usage = usage;
    if minted_headers.is_empty() {
        // An answer naming its record serves its id in the header that names it.
        let named = op.identity.as_ref().map_or("id", |i| i.field.as_str());
        let names = answer
            .body
            .as_ref()
            .and_then(|b| mb_mcm::member_at(b, named))
            .and_then(id_string)
            .is_some_and(|named| named == key);
        if names {
            let session = &plan.session;
            let page_url = plan.page_url.as_deref();
            header_ids::mint(rt, state, (session, op, t, call), &mut answer, page_url)
                .instrument(state_span())
                .await;
        }
    } else {
        answer.headers.extend(minted_headers);
    }
    let write = Write::Put { id: key, data };
    let answer = commit(state, plan, call, (t, None), write, answer).await;
    answered(rt, plan, kind);
    Ran::Answered(Box::new(answer))
}

/// Whether the generated path refuses `call`, a create of type `t`, for a value the
/// contract says is unique that a record in its collection holds: what
/// [`value_index::same_values`] shows a create, enforced ([`repeats::enforced`]) as it
/// is on a success that is no answer of the holder.
async fn refuses_a_repeat(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    t: &str,
) -> bool {
    let same: Vec<(String, String, String)> =
        value_index::same_values(state, plan, call, |_, _| false)
            .await
            .into_iter()
            .map(|(hit, member)| (hit.resource_type, hit.id, member))
            .collect();
    if same.is_empty() {
        return false;
    }
    let found = repeats::enforced(state, (&plan.session, plan.op, t), (call, None), &same).await;
    matches!(found, Some(repeats::Enforced::Refuse { .. }))
}

/// The member type `t`'s records are keyed by: the described type's identity member,
/// else `op`'s identity field, else `id`.
pub(super) fn identity_member<'r>(
    rt: &'r crate::MockRuntime,
    op: &'r Operation,
    t: &str,
) -> &'r str {
    rt.described()
        .type_desc(t)
        .and_then(|d| d.identity.as_ref()?.member.as_deref())
        .unwrap_or_else(|| records::identity_member(rt, op, t))
}

/// The members of a created record of type `t` the server decides, which fill never
/// writes: its identity `field`, the lifecycle member a described `initial` sets (the
/// request not naming it), and the members the description derives.
fn server_owned(
    rt: &crate::MockRuntime,
    t: &str,
    field: &str,
    request: Option<&Value>,
) -> Vec<String> {
    let mut owned = vec![field.to_string()];
    let Some(desc) = rt.described().type_desc(t) else {
        return owned;
    };
    if let Some(lifecycle) = desc.lifecycle.as_ref().filter(|l| l.initial.is_some())
        && !request.is_some_and(|r| mb_mcm::member_at(r, &lifecycle.member).is_some())
    {
        owned.push(lifecycle.member.clone());
    }
    for member in desc.derived.keys() {
        owned.push(member.split('.').next().unwrap_or(member).to_string());
    }
    owned
}

/// What a create's record and answer declare.
struct Shape {
    /// The members of type `t`'s records, each with its schema: those its reads declare,
    /// then those of the record in the create's answer, when that answer is the record.
    members: Vec<(String, Value)>,
    /// The schemas the record is read by ([`read_schemas`]), and the create's answer's
    /// when it is the record: what fill completes it against ([`gaps`]).
    schemas: Vec<Value>,
}

impl Shape {
    /// The shape of `op`'s creates of type `t`: the members of the type's reads
    /// ([`read_schemas`]), then those of the create's answer (`response_schema`, the one
    /// the call asks for, else the declared one) when it is the record. Its
    /// answer is the record when the type has no read to tell its records by, or every
    /// member the answer's record declares is one the type's records declare too.
    fn of(
        rt: &crate::MockRuntime,
        op: &Operation,
        t: &str,
        response_schema: Option<&Value>,
    ) -> Self {
        let bundle = rt.bundle();
        // The answer as the call asks for it (a GraphQL selection), else as declared.
        let answered = response_schema
            .or_else(|| op.success_output()?.schema.as_ref())
            .and_then(
                |s| match op.identity.as_ref().filter(|i| i.record.is_some()) {
                    Some(identity) => record_schema_of(bundle, identity, s),
                    None => Some(s.clone()),
                },
            );
        let typed = type_members(rt, t);
        let answers_record = typed.is_empty()
            || schema_members(rt, answered.as_ref())
                .iter()
                .all(|m| typed.iter().any(|d| d == m));
        let mut schemas = read_schemas(rt, t);
        schemas.extend(answered.filter(|_| answers_record));
        let schemas: Vec<Value> = schemas.iter().map(|s| non_null(bundle, s)).collect();
        Self {
            members: members_of(rt, &schemas),
            schemas,
        }
    }

    /// The record `request` (the request's record) holds: the members the type
    /// declares (all of them when it declares none), and those of an object member the
    /// type does not declare, one level down, that the record holds no value for.
    /// Empty with no request; `None` for a request that is not an object.
    fn request_record(&self, request: Option<&Value>) -> Option<Map<String, Value>> {
        let sent = match request {
            None => return Some(Map::new()),
            Some(Value::Object(sent)) => sent,
            Some(_) => return None,
        };
        if self.members.is_empty() {
            return Some(sent.clone());
        }
        let declares = |name: &str| self.members.iter().any(|(m, _)| m == name);
        let mut record: Map<String, Value> = sent
            .iter()
            .filter(|(name, _)| declares(name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        let wrapped = sent
            .iter()
            .filter(|(name, _)| !declares(name))
            .filter_map(|(_, value)| value.as_object())
            .flatten();
        for (name, value) in wrapped {
            if declares(name) && !record.contains_key(name) {
                record.insert(name.clone(), value.clone());
            }
        }
        Some(record)
    }

    /// Sets on `record` what the call's path says of it: the described parent member,
    /// from the parent's path parameter (in the parent's pattern), a member the type
    /// declares under a path parameter's own name, and the member a described list
    /// scope of the type matches to a path parameter of the create's route (typed as
    /// the member is). A member the record holds already keeps its value.
    fn path_members(
        &self,
        rt: &crate::MockRuntime,
        t: &str,
        call: &OperationCall,
        record: &mut Map<String, Value>,
    ) {
        let parent = rt.described().type_desc(t).and_then(|d| d.parent.as_ref());
        if let Some(parent) = parent
            && let (Some(member), Some(param)) = (&parent.member, &parent.path_param)
            && let Some(value) = call.path_params.get(param)
            && !record.contains_key(member)
        {
            let value = match (&parent.pattern, id_string(value)) {
                (Some(pattern), Some(id)) if records::bare_id(pattern, &id).is_none() => {
                    Value::String(records::patterned(pattern, &id))
                }
                _ => value.clone(),
            };
            record.insert(member.clone(), value);
        }
        for (name, value) in &call.path_params {
            if self.members.iter().any(|(m, _)| m == name) && !record.contains_key(name) {
                record.insert(name.clone(), value.clone());
            }
        }
        // A list of the type scoped by a parameter the create's path names (a note's
        // `noteable_iid` by `issue_iid`): the created record is in that scope.
        let mut held = Value::Object(std::mem::take(record));
        for scope in list_scopes(rt, t) {
            let Some(value) = call.path_params.get(&scope.param) else {
                continue;
            };
            if mb_mcm::member_at(&held, &scope.member).is_some() {
                continue;
            }
            let declared = self
                .schemas
                .iter()
                .find_map(|s| property_schema(rt.bundle(), s, &scope.member));
            let value = match declared {
                Some(schema) => typed_as(rt.bundle(), &schema, value),
                None => value.clone(),
            };
            mb_mcm::set_member_at(&mut held, &scope.member, value);
        }
        if let Value::Object(members) = held {
            *record = members;
        }
    }
}

/// The described scopes of the lists of type `t`.
fn list_scopes<'r>(rt: &'r crate::MockRuntime, t: &str) -> Vec<&'r mb_mcm::world::ScopeDesc> {
    rt.bundle()
        .operations
        .iter()
        .filter(|op| super::list::item_type(rt, op) == Some(t))
        .filter_map(|op| {
            let desc = rt.described().op(&op.id)?;
            let kind = desc.kind.or_else(|| OpKind::of(op));
            (kind == Some(OpKind::List)).then_some(desc.scope.as_ref()?)
        })
        .collect()
}

/// `value` (a path value) as member schema `schema` types it: a number for a number
/// or an integer member it parses as, text for a text member; else as it is.
fn typed_as(bundle: &mb_mcm::Bundle, schema: &Value, value: &Value) -> Value {
    let types: Vec<&str> = crate::call::schema_parts(bundle, Some(schema))
        .0
        .iter()
        .filter_map(|part| part.get("type").and_then(Value::as_str))
        .collect();
    let text = id_string(value);
    if types.contains(&"string") {
        return text.map_or_else(|| value.clone(), Value::String);
    }
    if types.contains(&"integer")
        && let Some(n) = text.as_deref().and_then(|t| t.parse::<i64>().ok())
    {
        return Value::from(n);
    }
    if types.contains(&"number")
        && let Some(n) = text.as_deref().and_then(|t| t.parse::<f64>().ok())
        && let Some(n) = serde_json::Number::from_f64(n)
    {
        return Value::Number(n);
    }
    value.clone()
}

/// The schemas records of type `t` are read by: the record its read by id (else its
/// list or singleton read) answers, and the item of each list of the type.
pub(super) fn read_schemas(rt: &crate::MockRuntime, t: &str) -> Vec<Value> {
    let mut schemas: Vec<Value> = type_check(rt, t)
        .and_then(|(read, shape)| shape.or_else(|| read.success_output()?.schema.as_ref()))
        .cloned()
        .into_iter()
        .collect();
    for op in &rt.bundle().operations {
        let kind = rt
            .described()
            .op(&op.id)
            .and_then(|d| d.kind)
            .or_else(|| OpKind::of(op));
        if kind == Some(OpKind::List)
            && super::list::item_type(rt, op) == Some(t)
            && let Some(shape) = state_lists::Shape::of(rt, op)
            && !schemas.contains(&shape.items)
        {
            schemas.push(shape.items);
        }
    }
    schemas
}

/// The members `schemas` declare, each with the first schema declaring it.
pub(super) fn members_of(rt: &crate::MockRuntime, schemas: &[Value]) -> Vec<(String, Value)> {
    let mut members: Vec<(String, Value)> = Vec::new();
    for schema in schemas {
        for name in schema_members(rt, Some(schema)) {
            if members.iter().any(|(m, _)| m == name) {
                continue;
            }
            if let Some(declared) = property_schema(rt.bundle(), schema, name) {
                members.push((name.to_string(), declared));
            }
        }
    }
    members
}

/// What a read of type `t` requires that `record` does not hold, but what the server
/// decides ([`server_owned`], by identity member `field`): what
/// [`mb_engine::Engine::fill`] writes for a replace ([`gaps`]).
pub(super) fn unread(
    rt: &crate::MockRuntime,
    t: &str,
    field: &str,
    record: &Value,
) -> Vec<(String, Value)> {
    let owned = server_owned(rt, t, field, Some(record));
    gaps(rt, &read_schemas(rt, t), record, &owned)
}

/// The deepest an object member's own requirements are followed.
const GAP_DEPTH: usize = 4;

/// What `schemas` require that `record` does not hold, each top member with the schema
/// fill writes it by: a member it lacks (but those in `owned`), with its own schema;
/// an object member it holds lacking members its schema requires, with an object
/// schema of those alone, at any depth (to [`GAP_DEPTH`]). Merged into the record by
/// [`fill_into`].
pub(super) fn gaps(
    rt: &crate::MockRuntime,
    schemas: &[Value],
    record: &Value,
    owned: &[String],
) -> Vec<(String, Value)> {
    let bundle = rt.bundle();
    let mut out: Vec<(String, Value)> = Vec::new();
    for schema in schemas {
        let Some(Value::Object(properties)) = lacking(bundle, schema, record, GAP_DEPTH)
            .as_ref()
            .and_then(|s| s.get("properties"))
            .cloned()
        else {
            continue;
        };
        for (name, member) in properties {
            if !owned.contains(&name) && !out.iter().any(|(n, _)| *n == name) {
                out.push((name, member));
            }
        }
    }
    out
}

/// An object schema of what `schema` requires that `value` does not hold (see
/// [`gaps`]); `None` when it holds all of it.
fn lacking(bundle: &mb_mcm::Bundle, schema: &Value, value: &Value, depth: usize) -> Option<Value> {
    let Value::Object(held) = value else {
        return None;
    };
    let mut properties = Map::new();
    for name in required_members(bundle, schema) {
        let Some(member) = property_schema(bundle, schema, &name) else {
            continue;
        };
        match held.get(&name) {
            None => {
                properties.insert(name, member);
            }
            Some(inner @ Value::Object(_)) if depth > 1 => {
                if let Some(below) = lacking(bundle, &member, inner, depth - 1) {
                    properties.insert(name, below);
                }
            }
            Some(_) => {}
        }
    }
    let names: Vec<String> = properties.keys().cloned().collect();
    (!properties.is_empty())
        .then(|| serde_json::json!({"type": "object", "properties": properties, "required": names}))
}

/// `filled` (fill's answer for [`gaps`]) into `record`: a member it lacks is set, and
/// an object member it holds takes what it lacks of the filled one, at any depth.
pub(super) fn fill_into(record: &mut Map<String, Value>, filled: Map<String, Value>) {
    for (name, value) in filled {
        match (record.get_mut(&name), value) {
            (None, value) => {
                record.insert(name, value);
            }
            (Some(Value::Object(held)), Value::Object(below)) => fill_into(held, below),
            (Some(_), _) => {}
        }
    }
}

/// An object schema declaring `members`, which fill writes by.
pub(super) fn fill_schema(members: &[(String, Value)]) -> Value {
    let properties: Map<String, Value> = members.iter().cloned().collect();
    serde_json::json!({"type": "object", "properties": properties})
}

/// `record` in the envelope `op`'s answer holds it in, the envelope's members nothing
/// else gives drawn as a read's are (`true` for a boolean, else the faker's): a created
/// record's envelope (`{success, result: [...]}`) has no answer of the session to
/// take them from. `None` for an answer that holds no record.
fn enveloped(
    rt: &crate::MockRuntime,
    op: &Operation,
    (id, t, record): (&str, &str, &Value),
    response_schema: Option<&Value>,
) -> Option<GeneratedResponse> {
    op.identity.as_ref()?.record.as_ref()?;
    let drawn = rt.engine.envelope_draws(rt.bundle(), op, response_schema);
    let held = mb_state::StoredResource {
        resource_type: t.to_owned(),
        id: id.to_owned(),
        data: record.clone(),
    };
    (!drawn.is_empty())
        .then(|| serve_held(rt, op, &held, Some(&drawn), response_schema))
        .flatten()
}

/// `schema` without the `null` it admits: the one other branch of an `anyOf` or `oneOf`
/// whose others are `null` (a GraphQL nullable object), else `schema` itself.
fn non_null(bundle: &mb_mcm::Bundle, schema: &Value) -> Value {
    let derefed = crate::call::bundle_index::def_name(schema)
        .and_then(|def| bundle.defs.get(def))
        .unwrap_or(schema);
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(branches)) = derefed.get(key) {
            let others: Vec<&Value> = branches
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) != Some("null"))
                .collect();
            if let [only] = others.as_slice()
                && others.len() < branches.len()
            {
                return (*only).clone();
            }
        }
    }
    schema.clone()
}

/// Why `record` (with its derived members, `derived`, when there are any) would not
/// render into a read of type `t` ([`read_schemas`]): the first violation
/// (`<pointer> <keyword>`) of the first read schema neither validates against, as a
/// read's `render_error` gives it. `None` when every read renders it.
fn unreadable(
    rt: &crate::MockRuntime,
    t: &str,
    record: &Value,
    derived: Option<&Value>,
) -> Option<String> {
    let engine = &rt.engine;
    for schema in read_schemas(rt, t) {
        let Some(found) = engine.schema_violations(rt.bundle(), &schema, record) else {
            continue;
        };
        if found.is_empty()
            || derived.is_some_and(|d| {
                engine
                    .schema_violations(rt.bundle(), &schema, d)
                    .is_some_and(|v| v.is_empty())
            })
        {
            continue;
        }
        return found
            .into_iter()
            .next()
            .map(|(pointer, keyword)| match pointer.as_str() {
                "" => format!("<root> {keyword}"),
                at => format!("{at} {keyword}"),
            });
    }
    None
}

/// Logs that a create's record is not stored ([`Fallback::RecordInvalid`]), and why.
fn invalid(rt: &crate::MockRuntime, plan: &CallPlan<'_>, why: &str) {
    tracing::info!(
        mock = %rt.name,
        operation = %plan.op.id,
        exec = "create",
        exec_fallback = Fallback::RecordInvalid.as_str(),
        render_error = %why,
        "not answered by the executor"
    );
}

/// Sets in `value` what `schema` decides of the members it lacks ([`decided`]), and
/// so in the objects it holds, and the items of its arrays, to `depth` levels; at its
/// top, never a member in `owned` (the server's own).
fn preset(
    bundle: &mb_mcm::Bundle,
    schema: &Value,
    value: &mut Value,
    owned: &[String],
    depth: usize,
) {
    if depth == 0 {
        return;
    }
    let parts = whole_parts(bundle, schema);
    match value {
        Value::Object(held) => {
            for part in &parts {
                let Some(Value::Object(properties)) = part.get("properties") else {
                    continue;
                };
                for (name, member) in properties {
                    if owned.contains(name) {
                        continue;
                    }
                    match held.get_mut(name) {
                        None => {
                            if let Some(decided) = decided(bundle, member) {
                                held.insert(name.clone(), decided);
                            }
                        }
                        Some(inner) => preset(bundle, member, inner, &[], depth - 1),
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in parts.iter().filter_map(|p| p.get("items")) {
                for held in items.iter_mut() {
                    preset(bundle, item, held, &[], depth - 1);
                }
            }
        }
        _ => {}
    }
}

/// Sets in `record` what `op`'s request schema (at the described `request.record`)
/// decides of the members the request left out ([`preset`]): a member the type's records
/// declare (`shape`) whose value, so set, fits the record's own schema of it.
fn preset_from_request(
    rt: &crate::MockRuntime,
    op: &Operation,
    shape: &Shape,
    record: &mut Value,
    owned: &[String],
) {
    let bundle = rt.bundle();
    let Some(body) = op.input.body.as_ref() else {
        return;
    };
    let pointer = rt
        .described()
        .op(&op.id)
        .and_then(|d| d.request.as_ref())
        .and_then(|r| r.record.as_ref())
        .map(|p| p.as_str().to_owned());
    let schema = match pointer.as_deref() {
        None | Some("") => body.clone(),
        Some(at) => {
            let steps = at.trim_start_matches('/').replace('/', ".");
            match property_schema(bundle, body, &steps) {
                Some(schema) => schema,
                None => return,
            }
        }
    };
    let mut preset_copy = record.clone();
    preset(bundle, &schema, &mut preset_copy, owned, GAP_DEPTH);
    let (Value::Object(set), Value::Object(held)) = (preset_copy, record) else {
        return;
    };
    for (name, value) in set {
        if held.get(&name) == Some(&value) {
            continue;
        }
        let fits = shape
            .members
            .iter()
            .find(|(m, _)| *m == name)
            .and_then(|(_, member)| rt.engine.schema_violations(bundle, member, &value))
            .is_some_and(|v| v.is_empty());
        if fits {
            held.insert(name, value);
        }
    }
}

/// The value member schema `schema` decides: its `const`, else its one `enum` value,
/// else its `default` (never `null`).
fn decided(bundle: &mb_mcm::Bundle, schema: &Value) -> Option<Value> {
    let parts = whole_parts(bundle, schema);
    let one = |key: &str| parts.iter().find_map(|p| p.get(key).cloned());
    one("const")
        .or_else(|| {
            parts.iter().find_map(|p| match p.get("enum") {
                Some(Value::Array(values)) if values.len() == 1 => values.first().cloned(),
                _ => None,
            })
        })
        .or_else(|| one("default"))
        .filter(|v| !v.is_null())
}

/// The parts every value of `schema` meets: itself, what its `$ref`s and `allOf`s name,
/// and the one branch of an `anyOf` or `oneOf` whose others are `null`; never one of
/// several branches a value may take.
fn whole_parts<'a>(bundle: &'a mb_mcm::Bundle, schema: &'a Value) -> Vec<&'a Value> {
    fn visit<'a>(
        bundle: &'a mb_mcm::Bundle,
        schema: &'a Value,
        depth: usize,
        out: &mut Vec<&'a Value>,
    ) {
        if depth > 8 {
            return;
        }
        if let Some(target) =
            crate::call::bundle_index::def_name(schema).and_then(|def| bundle.defs.get(def))
        {
            visit(bundle, target, depth + 1, out);
            return;
        }
        out.push(schema);
        for branch in schema
            .get("allOf")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            visit(bundle, branch, depth + 1, out);
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(Value::Array(branches)) = schema.get(key) {
                let others: Vec<&Value> = branches
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) != Some("null"))
                    .collect();
                if let [only] = others.as_slice() {
                    visit(bundle, only, depth + 1, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    visit(bundle, schema, 0, &mut out);
    out
}

/// A success answer with no body, for [`header_ids::mint`] to mint an id into.
fn probe(status: u16) -> GeneratedResponse {
    GeneratedResponse {
        status,
        content_type: None,
        body: None,
        source: mb_engine::Source::State,
        usage: mb_llm::Usage::default(),
        fallback_reason: None,
        effects: Vec::new(),
        effect_outcomes: Vec::new(),
        dropped_effects: 0,
        new_record: false,
        patched: None,
        headers: Vec::new(),
        world_gaps: Vec::new(),
    }
}
