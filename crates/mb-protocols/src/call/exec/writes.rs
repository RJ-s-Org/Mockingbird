//! Updates, replaces and deletes (step 3 of the plan), each of the held target, with no
//! LLM call: the record the write leaves is stored through [`commit`] and rendered into
//! the operation's answer ([`render`]).
//!
//! - An update merges the record its request holds (and the record members its query
//!   names) into the held one, at any depth, as a JSON merge patch does (see
//!   [`Merge::onto`] for `null`s and unions). Then come the described transition and
//!   changes ([`transitions::apply`]).
//! - A replace stores the record its request holds, keeping the held identity and
//!   read-only members, and the lifecycle state and derived members it does not send.
//!   One that leaves a required member out generates (`needs_fill`) until fill fills it.
//! - Either keeps the record's identity and read-only members, a member saying the
//!   record is another owner's than the path names, and mints the concurrency tokens it
//!   advances ([`tokens::advance`]).
//!
//! Either generates instead, as today, when it cannot tell what the write does: on an
//! action's route (`action_route`); for a request holding no object (`unmerged_body`),
//! a member the record does not declare (`undeclared_member`) or a new value of a
//! read-only one (`read_only_member`); for a result with a schema violation the held
//! record did not have (`adds_violation`); for a record with writes remembered for it
//! that a read is still to catch up on (`behind`); or for a token only a model moves
//! (`model_token`).
//!
//! The answer is [`render`]'s; a described long-running answer holds the record as its
//! result, and is done: the write landed when it was accepted.

use mb_engine::GeneratedResponse;
use mb_engine::consistency::{declared_members, property_schema, required_members};
use mb_mcm::world::OpKind;
use mb_state::StoredResource;
use serde_json::{Map, Value, json};
use tracing::Instrument;

use super::{Fallback, Held, Ran, Write, answered, commit, fell_back, render};
use crate::call::bundle_index::schema_parts;
use crate::call::entities::added_violation;
use crate::call::write_memory::{self, read_only, read_schema, without_contradictions};
use crate::call::{
    CallPlan, OperationCall, StateAccess, id_string, lifecycle, records, state_span, tokens,
    transitions,
};

/// Whether the executor takes updates and replaces.
pub(super) const EXECUTES: bool = true;

/// Executes the call.
pub(super) async fn run(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: Held<'_>,
    response_schema: Option<&Value>,
    kind: OpKind,
) -> Ran {
    match kind {
        OpKind::Delete => delete(state, plan, call, held, response_schema, kind).await,
        _ => update(state, plan, call, held, response_schema, kind).await,
    }
}

/// An update or a replace (`kind`) of the held target: the record it leaves, stored and
/// answered.
async fn update(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: Held<'_>,
    response_schema: Option<&Value>,
    kind: OpKind,
) -> Ran {
    let rt = state.rt;
    let op = plan.op;
    // A route acting on a record named earlier in its path (`POST /orders/{id}/confirm`)
    // is an action whatever its method, which an update's merge does not tell.
    if op.is_action() {
        fell_back(rt, plan, kind, "action_route");
        return Ran::Generate(None);
    }
    let (Some(stored), Some(id)) = (held.target, held.target_id) else {
        fell_back(rt, plan, kind, Fallback::NotHeld.as_str());
        return Ran::Generate(None);
    };
    let t = stored.resource_type.as_str();
    let mut data = match changed(rt, (plan, call), stored, kind == OpKind::Replace) {
        Ok(data) => data,
        Err(reason) => {
            fell_back(rt, plan, kind, reason);
            return Ran::Generate(None);
        }
    };
    transitions::apply(rt, op, call.body.as_ref(), &mut data);
    if added_violation(rt, t, &stored.data, &data).is_some() {
        fell_back(rt, plan, kind, "adds_violation");
        return Ran::Generate(None);
    }
    // Writes the held record does not show yet (an action the model has not answered
    // for it), or that only referred to it, are the model's to catch up on: committing
    // would forget them.
    if write_memory::remembers(state, &plan.session, t, id)
        .instrument(state_span())
        .await
    {
        fell_back(rt, plan, kind, Fallback::Behind.as_str());
        return Ran::Generate(None);
    }
    // Prepare refused a stale token; what the update advances is read again here.
    let advancing = match tokens::applies(rt, plan) {
        true => tokens::check(state, plan, call, stored)
            .instrument(state_span())
            .await
            .ok(),
        false => None,
    };
    if let Some(advancing) = &advancing {
        // A token known only by how the model moved it before is the model's to move.
        if !advancing.all_mintable() {
            fell_back(rt, plan, kind, "model_token");
            return Ran::Generate(None);
        }
        tokens::advance(rt, (t, id), advancing, &stored.data, &mut data);
    }
    let Some(answer) = written(rt, plan, call, (id, &data), response_schema) else {
        fell_back(rt, plan, kind, Fallback::RenderFailed.as_str());
        return Ran::Generate(None);
    };
    let write = Write::Put {
        id: id.to_owned(),
        data,
    };
    let answer = commit(state, plan, call, (t, Some(stored)), write, answer).await;
    // The tokens it moved are confirmed: a later write sending an old value is stale.
    if let Some(advancing) = &advancing {
        tokens::after_update(state, &plan.session, stored, advancing, None)
            .instrument(state_span())
            .await;
    }
    answered(rt, plan, kind);
    Ran::Answered(Box::new(answer))
}

/// The record an update (a replace, with `replace`) of `stored` leaves, before its
/// described changes; `Err` with the reason the executor cannot tell it.
fn changed(
    rt: &crate::MockRuntime,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    stored: &StoredResource,
    replace: bool,
) -> Result<Value, &'static str> {
    let op = plan.op;
    let t = stored.resource_type.as_str();
    let empty = Value::Object(Map::new());
    let sent = call
        .body
        .as_ref()
        .map_or(&empty, |b| sent_record(rt, op, b));
    // A member saying the record is another owner's than the path names is not set:
    // the path wins.
    let sent = without_contradictions(rt, plan, call, stored, sent);
    let (Value::Object(sent), Value::Object(held)) = (&sent, &stored.data) else {
        return Err("unmerged_body");
    };
    let bundle = rt.bundle();
    let schema = read_schema(rt, t);
    let member = |name: &str| schema.and_then(|s| property_schema(bundle, s, name));
    let identity = identity_members(rt, op, t);
    let read_only = |name: &str| read_only(bundle, member(name).as_ref());
    // What a client does not set: the identity, and the members the contract keeps.
    let kept = |name: &str| identity.iter().any(|field| field == name) || read_only(name);
    let declared = schema.and_then(|s| declared_members(bundle, s));
    if let Some(declared) = &declared
        && sent
            .keys()
            .any(|name| !kept(name) && !declared.contains(name))
    {
        return Err("undeclared_member");
    }
    // A record member the query names is set as the body sets it (Trello's
    // `PUT /cards/{id}?name=…`).
    let mut sent = sent.clone();
    for (name, value) in &call.query {
        if !sent.contains_key(name) && declared.as_ref().is_some_and(|d| d.contains(name)) {
            sent.insert(name.clone(), value.clone());
        }
    }
    // An echo of a member the contract keeps is no change; a new value of one is the
    // client's word against the contract's.
    let echoes = |name: &str, value: &Value| {
        held.get(name)
            .is_some_and(|now| now == value || id_string(now) == id_string(value))
    };
    if sent.iter().any(|(name, value)| {
        read_only(name) && !identity.iter().any(|f| f == name) && !echoes(name, value)
    }) {
        return Err("read_only_member");
    }
    sent.retain(|name, _| !kept(name));
    let mut out = match replace {
        true => Map::new(),
        false => held.clone(),
    };
    // The request body's own schema, where the body is the record.
    let request = match call.body.as_ref() {
        Some(body) if std::ptr::eq(sent_record(rt, op, body), body) => op.input.body.as_ref(),
        _ => None,
    };
    let merge = Merge {
        rt,
        merge_patch: is_merge_patch(call),
    };
    merge.onto(&mut out, &sent, (schema, request));
    // An identity held inside an object member (`info.label`) stays where it sits.
    let mut out = Value::Object(out);
    for field in identity.iter().filter(|f| f.contains('.')) {
        if let Some(value) = mb_mcm::member_at(&stored.data, field) {
            mb_mcm::set_member_at(&mut out, field, value.clone());
        }
    }
    let Value::Object(mut out) = out else {
        return Err("unmerged_body");
    };
    if replace {
        let described = rt.described().type_desc(t);
        let lifecycle = described
            .and_then(|d| d.lifecycle.as_ref())
            .map(|l| l.member.as_str());
        let derived = |name: &str| described.is_some_and(|d| d.derived.contains_key(name));
        for (name, value) in held {
            let unsent =
                (derived(name) || lifecycle == Some(name.as_str())) && !out.contains_key(name);
            if kept(name) || unsent {
                out.insert(name.clone(), value.clone());
            }
        }
        if let Some(schema) = schema
            && required_members(bundle, schema)
                .iter()
                .any(|name| !out.contains_key(name))
        {
            return Err("needs_fill");
        }
    }
    Ok(Value::Object(out))
}

/// How a request's members go onto a record.
struct Merge<'r> {
    rt: &'r crate::MockRuntime,
    /// The request says its body is a JSON merge patch (RFC 7396).
    merge_patch: bool,
}

impl Merge<'_> {
    /// `sent` onto `record`, member by member, the schemas each level of the record
    /// and of the request has given: an object merges into the one held, at any depth,
    /// but one of a union is set whole, as its members could mix with another
    /// branch's. A `null` removes the member in a merge patch, and a map's key; elsewhere it is the value where the
    /// record's schema takes `null`, removes the member where only the request's does,
    /// and is no value at all otherwise (the member stays).
    fn onto(
        &self,
        record: &mut Map<String, Value>,
        sent: &Map<String, Value>,
        (schema, request): (Option<&Value>, Option<&Value>),
    ) {
        let bundle = self.rt.bundle();
        for (name, value) in sent {
            let held_schema = schema.and_then(|s| property_schema(bundle, s, name));
            let sent_schema = request.and_then(|s| property_schema(bundle, s, name));
            let takes_null = |s: &Option<Value>| s.as_ref().is_some_and(|s| self.takes_null(s));
            if value.is_null() && !takes_null(&held_schema) {
                // A key of a map (a member no schema declares) goes, as no value can
                // stand for it.
                if self.merge_patch || takes_null(&sent_schema) || held_schema.is_none() {
                    record.remove(name);
                }
                continue;
            }
            let union = schema_parts(bundle, held_schema.as_ref())
                .0
                .iter()
                .any(|part| part.get("oneOf").is_some() || part.get("anyOf").is_some());
            match (record.get_mut(name), value) {
                (Some(Value::Object(below)), Value::Object(members)) if !union => {
                    let schemas = (held_schema.as_ref(), sent_schema.as_ref());
                    self.onto(below, members, schemas);
                }
                _ => {
                    record.insert(name.clone(), value.clone());
                }
            }
        }
    }

    /// Whether member schema `schema` takes `null`.
    fn takes_null(&self, schema: &Value) -> bool {
        mb_engine::SchemaValidator::new(&self.rt.bundle().with_reachable_defs(schema))
            .is_ok_and(|v| v.is_valid(&Value::Null))
    }
}

/// The members that name a record of type `t`: the one `op` keys it by, and the one its
/// read by id does. Another member a path may name it by (an alternate key, a name) is
/// the client's to change.
fn identity_members(rt: &crate::MockRuntime, op: &mb_mcm::Operation, t: &str) -> Vec<String> {
    let mut fields = vec![records::identity_member(rt, op, t).to_owned()];
    let read = rt
        .index()
        .detail_reads
        .get(t)
        .and_then(|i| rt.bundle().operations.get(*i))
        .and_then(|read| read.identity.as_ref());
    if let Some(identity) = read
        && !fields.contains(&identity.field)
    {
        fields.push(identity.field.clone());
    }
    fields
}

/// The record `body` holds: at the described `request.record`, else where the
/// operation's answer holds its record when the request holds one there too, else the
/// body.
fn sent_record<'a>(rt: &crate::MockRuntime, op: &mb_mcm::Operation, body: &'a Value) -> &'a Value {
    let described = records::request_record(rt, op, body);
    if !std::ptr::eq(described, body) {
        return described;
    }
    op.identity
        .as_ref()
        .filter(|i| i.record.is_some())
        .and_then(|i| i.record_in(body))
        .unwrap_or(body)
}

/// Whether `call` says its body is a JSON merge patch (RFC 7396).
fn is_merge_patch(call: &OperationCall) -> bool {
    call.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            && value.as_str().is_some_and(|v| {
                v.split(';').next().is_some_and(|t| {
                    t.trim()
                        .eq_ignore_ascii_case("application/merge-patch+json")
                })
            })
    })
}

/// What a write leaving `record` answers: [`render`]'s answer, but where the
/// description says the answer is long-running, the faker's operation holding `record`
/// as its result, and done (the write landed when it was accepted). `None` when that
/// does not suit the schema.
fn written(
    rt: &crate::MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    (id, record): (&str, &Value),
    response_schema: Option<&Value>,
) -> Option<GeneratedResponse> {
    let op = plan.op;
    let Some(lr) = rt
        .described()
        .op(&op.id)
        .and_then(|d| d.answer.as_ref())
        .and_then(|a| a.long_running.as_ref())
    else {
        return render(rt, plan, call, (id, record), response_schema);
    };
    let variant = op.success_output()?;
    let status = match variant.status {
        mb_mcm::StatusSelector::Code(code) => code,
        _ => 200,
    };
    let ctx = mb_engine::GenerationContext {
        response_schema,
        ..mb_engine::GenerationContext::default()
    };
    let mut answer = rt
        .engine
        .fallback(rt.bundle(), op, variant, status, call, &ctx);
    mb_mcm::set_member_at(answer.body.as_mut()?, &lr.result, record.clone());
    records::settle_create(rt, op, &mut answer);
    rt.engine
        .serve_stored(rt.bundle(), op, answer.body.as_ref()?, response_schema)
}

/// A delete of the held target: removed, or moved to its type's soft-deleted state,
/// answered with the operation's declared delete answer.
async fn delete(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: Held<'_>,
    response_schema: Option<&Value>,
    kind: OpKind,
) -> Ran {
    let rt = state.rt;
    let (Some(stored), Some(id)) = (held.target, held.target_id) else {
        fell_back(rt, plan, kind, Fallback::NotHeld.as_str());
        return Ran::Generate(None);
    };
    let t = stored.resource_type.as_str();
    let mut data = stored.data.clone();
    let soft = lifecycle::soft_delete(rt, t, &mut data);
    let Some(answer) = delete_answer(rt, plan, call, (id, &data), response_schema) else {
        fell_back(rt, plan, kind, Fallback::RenderFailed.as_str());
        return Ran::Generate(None);
    };
    let write = match soft {
        true => Write::Put {
            id: id.to_owned(),
            data,
        },
        false => Write::Remove { id: id.to_owned() },
    };
    let answer = commit(state, plan, call, (t, Some(stored)), write, answer).await;
    answered(rt, plan, kind);
    Ran::Answered(Box::new(answer))
}

/// What a delete answers: an empty object when its success response's schema declares
/// no members, else its answer rendered from the record as the delete leaves it.
fn delete_answer(
    rt: &crate::MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    (id, data): (&str, &Value),
    response_schema: Option<&Value>,
) -> Option<GeneratedResponse> {
    let op = plan.op;
    let free_form = op
        .success_output()
        .and_then(|v| response_schema.or(v.schema.as_ref()))
        .is_some_and(|schema| declared_members(&rt.bundle, schema).is_none());
    if free_form && op.identity.as_ref().is_none_or(|i| i.record.is_none()) {
        return rt
            .engine
            .serve_stored(&rt.bundle, op, &json!({}), response_schema);
    }
    render(rt, plan, call, (id, data), response_schema)
}
