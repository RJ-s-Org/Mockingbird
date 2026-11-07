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
//!   lifecycle's `initial` is then set. A member whose schema fixes its value (a
//!   `const`, a one-value `enum`: GraphQL's `__typename`) takes that value.
//! - The answer is [`super::render`]'s (no body, the record, or an acknowledgement
//!   drawn from its own schema with the record's members laid over it); a described
//!   long-running answer holds the record as its result, and is done: the record is
//!   stored when the create is accepted.
//!
//! Bulk creates, a create naming a record the session holds, and one whose answer does
//! not render, go on to generation.

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
    CallPlan, OperationCall, StateAccess, header_ids, id_string, lifecycle, records,
    request_values, state_lists, state_span, target_id,
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
    let shape = Shape::of(rt, op, t);
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
    // A member whose schema fixes its value (a GraphQL `__typename`) takes it.
    if let Value::Object(known) = &mut data {
        for (name, schema) in &shape.members {
            if !known.contains_key(name)
                && !owned.contains(name)
                && let Some(value) = fixed(schema)
            {
                known.insert(name.clone(), value);
            }
        }
    }
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

    let Some(mut answer) = written(rt, plan, call, (&key, &data), response_schema) else {
        return fall(Fallback::RenderFailed.as_str());
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
    /// ([`read_schemas`]), then those of the create's answer when it is the record. Its
    /// answer is the record when the type has no read to tell its records by, or every
    /// member the answer's record declares is one the type's records declare too.
    fn of(rt: &crate::MockRuntime, op: &Operation, t: &str) -> Self {
        let bundle = rt.bundle();
        let answered = op
            .success_output()
            .and_then(|o| o.schema.as_ref())
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
    /// from the parent's path parameter (in the parent's pattern), and a member the
    /// type declares under a path parameter's own name. A member the record holds
    /// already keeps its value.
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
    }
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

/// The value member schema `schema` fixes: its `const`, else its one `enum` value.
fn fixed(schema: &Value) -> Option<Value> {
    schema
        .get("const")
        .cloned()
        .or_else(|| match schema.get("enum") {
            Some(Value::Array(values)) if values.len() == 1 => values.first().cloned(),
            _ => None,
        })
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
