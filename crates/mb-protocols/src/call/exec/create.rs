//! Creates (step 2 of the plan): the request's record, its minted id, its initial state
//! and the members [`mb_engine::Engine::fill`] writes, stored and answered.
//!
//! - The record is what the request holds at the described `request.record`, else the
//!   body: the members the type's records declare (all of them when it declares none),
//!   with those of an object member the type does not declare itself (an input wrapper,
//!   GraphQL's `input: {...}`) one level down, the members a declared query parameter
//!   names (`POST /boards?name=…`), and the path values naming its parent or a member
//!   it declares (undescribed, a member named for the type whose read by id the route
//!   runs through: `deal_id` under `/deals/{id}/lines`). A path value is set at its member's dotted path
//!   ([`super::super::path_set`]): one naming an object member goes into the member the
//!   description names below it, and wins over anything fill would write.
//! - Its id is the one the request asks for (a client-chosen id, a path value naming the
//!   record), else one minted in the identity header the answer names it in, else one
//!   drawn as the type declares its identity, in its pattern.
//! - A request member that is a reference where the type's reads hold the record (a
//!   text or number where they hold an object, an array of them for an array of
//!   objects) is never stored as sent ([`reshape`]): it becomes the record the session
//!   holds under that id (by its patterned form too), projected to the member's schema,
//!   else it is removed for fill to write. Any other mismatch is left for the gate below.
//! - A member the request did not send whose schema decides its value (a `const`, a
//!   one-value `enum`: GraphQL's `__typename`; a `default`, of the record's schema or
//!   the request's) takes that value, at any depth the record holds ([`preset`]),
//!   optional members included. An array item declaring the type's id-shaped identity
//!   member, and not another type's record, gets a minted id ([`mint_items`]).
//! - A member a path may name the record by besides its id (a described alternate, a
//!   member its routes read it by, a compound parameter's later key: `name` of
//!   `{app_id_or_name}`, [`Shape::key_members`]) is unique among the type's records: a
//!   create sending a value another record of its collection holds is refused with the
//!   `409` or `422` it declares; one not sent is minted when it is an integer (one past
//!   the highest held), else filled and drawn again where a record holds the value.
//! - Then fill writes ([`gaps`]) what the type's reads (its read by id and every list of
//!   it) and the create's answer, when that is the record, require and nothing above
//!   gives (a member, or what an object member the request sent lacks), and every other
//!   member they declare that nothing set, optional ones too, at the top and below it
//!   ([`Unsent::All`]), but the server's own (the id, the lifecycle member a described
//!   `initial` sets, the derived members, the described concurrency token), a
//!   `writeOnly` member, and a reference to another record the request could have sent
//!   ([`unsent_reference`]); and what an array item or a nullable object the request
//!   sent lacks, item by item ([`lacking`]). What fill
//!   leaves is settled ([`settle_filled`]). The lifecycle's `initial` (where it fits
//!   the member, [`initial`]) and the tokens' first values ([`initial_token`]) are set
//!   before fill, and a member named for a path parameter is the record it names
//!   ([`path_references`]). A request member of another JSON type than the reads hold
//!   is converted where nothing is lost ([`converted`]), else left for the gate.
//! - Before it is stored, the record is checked against every read of its type
//!   ([`read_schemas`]), as those reads render it: one a read would not render (a
//!   reference where the read holds an object, an array item lacking a required id)
//!   is never stored, and the create generates (`record_invalid`, with `render_error`
//!   naming the first violation). So does one whose answer does not render
//!   (`render_failed`).
//! - The answer is [`super::render`]'s (no body, the record, or an acknowledgement
//!   drawn from its own schema with the record's members laid over it); a described
//!   long-running answer holds the record as its result, and is done: the record is
//!   stored when the create is accepted.
//!
//! Bulk creates, a create naming a record the session holds, and one repeating a value
//! the contract says is unique that the generated path refuses (`repeated_value`), go on
//! to generation too.

use mb_engine::consistency::{property_schema, required_members};
use mb_engine::error_body::{ErrorCause, RefusalFacts};
use mb_engine::{FillContext, GeneratedResponse};
use mb_mcm::Operation;
use mb_mcm::world::OpKind;
use serde_json::{Map, Value};
use tracing::Instrument;

use super::writes::written;
use super::{Fallback, Held, Ran, Write, answered, commit, fell_back};
use crate::call::bundle_index::record_schema_of;
use crate::call::entities::type_check;
use crate::call::owners::{Compound, schema_members};
use crate::call::writes::type_members;
use crate::call::{
    CallPlan, MAX_SCAN, OperationCall, StateAccess, derived, header_ids, id_string,
    in_create_collection, lifecycle, path_set, records, repeats, replays, request_values,
    serve_held, state_lists, state_span, target_id, transitions, value_index,
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
    let Some(mut record) = shape.request_record(rt, request) else {
        return fall("no_record");
    };
    // A record member the query names is set as the body sets it, as an update's is.
    shape.query_members(rt, op, call, &mut record);
    // What the request sent of the record, as the record holds it.
    let sent = Value::Object(record.clone());
    let from_path = path_set(rt, (op, t), call);
    shape.path_members(rt, call, &from_path, &mut record);
    // A value of a member that names the type's records besides their id (an
    // alternate key), held by another record of the create's collection, is refused
    // with the conflict the create declares: two records a path names alike are none.
    let field = identity_member(rt, op, t);
    let keys = shape.key_members(rt, t, field);
    let taken = repeated_key(state, plan, call, t, (&keys, &record))
        .instrument(state_span())
        .await;
    if let Some(held) = taken.clone()
        && let Some(code) = [409, 422]
            .into_iter()
            .find(|code| mb_engine::guards::declares_code(op, *code))
        && let Some(refusal) = plan
            .engine
            .refuse_held(
                rt.bundle(),
                op,
                call,
                code,
                ErrorCause::Taken,
                &RefusalFacts::taken(&[held]),
            )
            .await
    {
        replays::keep(state, plan, call, &refusal)
            .instrument(state_span())
            .await;
        answered(rt, plan, kind);
        return Ran::Answered(Box::new(refusal));
    }
    // One the create declares no refusal for is no record of state's either.
    if taken.is_some() {
        return fall("repeated_key");
    }

    // The id: the one asked for, else minted in the header the answer names it in, else
    // drawn as the type declares it.
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
                    let drawn = records::mint_one(
                        state,
                        (session, call),
                        (t, field),
                        &carried,
                        &mut Vec::new(),
                    )
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
    let Some(mut key) = id_string(&id) else {
        return fall("no_id");
    };
    let mut data = Value::Object(record);
    if !mb_mcm::set_member_at(&mut data, field, id) {
        return fall("no_id");
    }

    // The members nobody else decides, then the initial state.
    let mut owned = server_owned(rt, t, field, request);
    if let Some((member, value)) = initial(rt, t, request) {
        mb_mcm::set_member_at(&mut data, &member, value);
    }
    // Its tokens' first values; a token none can be minted for is filled as any member.
    let seed = crate::call::header_ids::MintSeed::new(rt, &plan.session, call);
    initial_token(rt, (seed, t), &mut data);
    let tokens = token_members(rt, t);
    owned.retain(|m| !tokens.contains(m) || mb_mcm::member_at(&data, m).is_some());
    // A request member that does not fit the record's is never stored as sent: the
    // record it names, when the session holds it, else left for fill.
    if let Value::Object(known) = &mut data {
        reshape(state, &plan.session, t, known, &owned)
            .instrument(state_span())
            .await;
    }
    // What the path says of the record, below its top too: the path's value wins over
    // anything fill would write.
    if let Value::Object(known) = &mut data {
        shape.path_scoped(rt, &from_path, known);
    }
    // What the description sets (`set`, `changes` read as `set`) wins over what the
    // request sent and the path says, and is never filled. One that sets the identity
    // member names the record by it.
    let set = transitions::apply(state, (plan, call), None, &mut data).await;
    for member in &set {
        if !member.contains('.') && !owned.contains(member) {
            owned.push(member.clone());
        }
    }
    if set.iter().any(|m| m == field) {
        let Some(named) = mb_mcm::member_at(&data, field).and_then(id_string) else {
            return fall("no_id");
        };
        if named != key
            && records::held_by_key(state, &plan.session, t, &named)
                .instrument(state_span())
                .await
                .is_some()
        {
            return fall("held");
        }
        key = named;
    }
    // A member the request did not send whose schema decides its value (a `const`, a
    // one-value `enum`: GraphQL's `__typename`; a `default`) takes it, at any depth.
    let fits = |schema: &Value, value: &Value| fits(rt, schema, value);
    for schema in &shape.schemas {
        preset(rt.bundle(), (schema, &fits), &mut data, &owned, GAP_DEPTH);
    }
    // So does one whose request schema gives the value the server assumes when it is
    // left out (harvest's `is_active: {default: true}`), where the record holds it.
    preset_from_request(rt, op, &shape, &mut data, &owned);
    // A nested item declaring the type's identity member gets an id of its own, when
    // that identity is an id (never a name to invent).
    let leaf = mb_mcm::leaf_member(field).to_owned();
    if id_shaped(&leaf) {
        for schema in &shape.schemas {
            mint_items(rt, (seed, t, schema), &mut data, &leaf, GAP_DEPTH);
        }
    }
    // A member naming the record besides its id (an alternate key) nobody set is unique
    // among the type's records, as an id is: an integer is minted, never filled; what
    // fill writes for another is drawn again where a record holds it.
    let filled_keys = mint_keys(state, (&plan.session, t), &keys, &mut data, &mut owned)
        .instrument(state_span())
        .await;
    // A member referring to the record a path parameter names (a comment's
    // `pullrequest` under `/pullrequests/{pull_request_id}`) is that record.
    if let Value::Object(known) = &mut data {
        path_references(state, (&plan.session, call), &shape, known, &owned)
            .instrument(state_span())
            .await;
    }
    // Below its top, every member nobody set is filled ([`Unsent::All`]); at its top,
    // what a read requires, then every other member nobody set, but a reference the
    // request could have sent.
    let request_schema = request_schema(rt, op);
    let unsent = Unsent::All(request_schema.as_ref(), Some(&sent));
    let mut missing = gaps(rt, (t, &shape.schemas), &data, &owned, unsent);
    let required: Vec<String> = missing.iter().map(|(name, _)| name.clone()).collect();
    if let Value::Object(known) = &data {
        for (name, member) in &shape.members {
            if !known.contains_key(name)
                && !owned.contains(name)
                && !missing.iter().any(|(m, _)| m == name)
                && !unsent_reference(rt, request_schema.as_ref(), name, member)
            {
                missing.push((name.clone(), member.clone()));
            }
        }
    }
    // A member a read never shows (`writeOnly`) is the client's to send, never filled.
    missing.retain(|(_, member)| !write_only(rt.bundle(), member));
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
        settle_filled(rt, (field, &shape.schemas), (&missing, &required), known);
    }
    unique_keys(state, (seed, t), &filled_keys, &mut data)
        .instrument(state_span())
        .await;

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

/// The first of `keys` (a type's alternate keys, [`Shape::key_members`]) whose value
/// `record`, a create's, sends that another record of type `t` in the create's
/// collection holds (one not soft-deleted), with that value.
async fn repeated_key(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    t: &str,
    (keys, record): (&[(String, Value)], &Map<String, Value>),
) -> Option<(String, Value)> {
    let sent: Vec<(&String, &Value)> = keys
        .iter()
        .filter_map(|(name, _)| Some((name, record.get(name)?)))
        .filter(|(_, value)| value.is_string() || value.is_number())
        .collect();
    if sent.is_empty() {
        return None;
    }
    let rt = state.rt;
    let gone = lifecycle::exclusion(rt, t);
    let listed = state.list(&plan.session, t, MAX_SCAN).await;
    for (name, value) in sent {
        let text = id_string(value);
        let holds = |r: &mb_state::StoredResource| {
            r.data.get(name).and_then(id_string) == text
                && gone.as_ref().is_none_or(|g| !g.excludes(&r.data))
                && in_create_collection(rt, &plan.session, plan.op, call, r)
        };
        if listed.iter().any(holds) {
            return Some((name.clone(), value.clone()));
        }
    }
    None
}

/// A member's name with its schema.
type Member = (String, Value);

/// Mints on `data`, a created record of type `t`, each of `keys` (its alternate keys,
/// [`Shape::key_members`]) it holds no value for and that `owned` leaves to nobody: an
/// integer member is one past the highest the type's records hold, added to `owned`, so
/// fill never writes it; any other is left for fill, which [`unique_keys`] keeps
/// unique. Returns the members left for fill.
async fn mint_keys(
    state: &mut StateAccess<'_>,
    (session, t): (&mb_state::SessionKey, &str),
    keys: &[(String, Value)],
    data: &mut Value,
    owned: &mut Vec<String>,
) -> Vec<(String, Value)> {
    let rt = state.rt;
    let bundle = rt.bundle();
    let (integers, others): (Vec<&Member>, Vec<&Member>) = keys
        .iter()
        .filter(|(name, _)| !owned.contains(name) && data.get(name).is_none_or(Value::is_null))
        .partition(|(_, member)| {
            admitted_types(bundle, member)
                .iter()
                .any(|a| a == "integer")
        });
    if !integers.is_empty() {
        let listed = state.list(session, t, MAX_SCAN).await;
        for (name, member) in integers {
            let top = listed
                .iter()
                .filter_map(|r| r.data.get(name)?.as_i64())
                .max()
                .unwrap_or(0);
            let value = Value::from(top.saturating_add(1));
            if fits(rt, member, &value)
                && let Some(record) = data.as_object_mut()
            {
                record.insert(name.clone(), value);
                owned.push(name.clone());
            }
        }
    }
    others.into_iter().cloned().collect()
}

/// Keeps each of `filled` (alternate keys of type `t` fill wrote on `data`,
/// [`mint_keys`]) unique: a value another record of the type holds is drawn again as
/// its schema declares it ([`crate::call::header_ids::draw_id_as`]), until no record
/// holds the draw; one no draw frees is left as filled.
async fn unique_keys(
    state: &mut StateAccess<'_>,
    (seed, t): (crate::call::header_ids::MintSeed<'_>, &str),
    filled: &[(String, Value)],
    data: &mut Value,
) {
    if filled.is_empty() {
        return;
    }
    let rt = state.rt;
    let listed = state.list(seed.session(), t, MAX_SCAN).await;
    for (name, member) in filled {
        let held: Vec<Option<String>> = listed
            .iter()
            .filter_map(|r| r.data.get(name))
            .map(id_string)
            .collect();
        let taken = |v: &Value| held.contains(&id_string(v));
        if !data.get(name).is_some_and(taken) {
            continue;
        }
        let drawn = (0..MINT_ITEM_ATTEMPTS)
            .filter_map(|_| crate::call::header_ids::draw_id_as(rt, (seed, t, name), member))
            .find(|drawn| !taken(drawn) && fits(rt, member, drawn));
        if let Some(value) = drawn
            && let Some(record) = data.as_object_mut()
        {
            record.insert(name.clone(), value);
        }
    }
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
    // The described lifecycle's `initial` state, where it fits the member ([`initial`]).
    if let Some((member, _)) = initial(rt, t, request) {
        owned.push(member);
    }
    if let Some(desc) = rt.described().type_desc(t) {
        for member in desc.derived.keys() {
            owned.push(member.split('.').next().unwrap_or(member).to_string());
        }
    }
    // A token is minted ([`initial_token`]), never filled.
    for member in token_members(rt, t) {
        if !request.is_some_and(|r| mb_mcm::member_at(r, &member).is_some()) {
            owned.push(member);
        }
    }
    owned
}

/// The lifecycle member of type `t` and the described `initial` state a created record
/// starts in, when the request does not name the member and the state fits what every
/// read declares of it; else the member is filled as any other.
fn initial(rt: &crate::MockRuntime, t: &str, request: Option<&Value>) -> Option<(String, Value)> {
    let described = rt.described();
    let lifecycle = described.type_desc(t)?.lifecycle.as_ref()?;
    let state = lifecycle.initial.as_deref()?;
    if request.is_some_and(|r| mb_mcm::member_at(r, &lifecycle.member).is_some()) {
        return None;
    }
    // `None` for a state not of the member's enum as spelled: matched below.
    let value = described.state_value(t, state);
    let bundle = rt.bundle();
    let schemas: Vec<Value> = read_schemas(rt, t)
        .iter()
        .filter_map(|s| property_schema(bundle, s, &lifecycle.member))
        .collect();
    let fits_all = |v: &Value| schemas.iter().all(|s| fits(rt, s, v));
    if let Some(value) = value.filter(|v| fits_all(v)) {
        return Some((lifecycle.member.clone(), value));
    }
    // The state as the member's enum spells it (`LIVE` for `live`, `in-progress` for
    // `IN_PROGRESS`).
    let squeezed = |text: &str| {
        text.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase()
    };
    schemas
        .iter()
        .flat_map(|s| whole_parts(bundle, s))
        .filter_map(|p| p.get("enum").and_then(Value::as_array))
        .flatten()
        .find(|v| v.as_str().is_some_and(|e| squeezed(e) == squeezed(state)) && fits_all(v))
        .map(|v| (lifecycle.member.clone(), v.clone()))
}

/// The tokens of type `t`'s records a create mints: the described concurrency member,
/// and the integer members a write carries as a token in its body
/// ([`crate::call::tokens::integer_body_tokens`]: a version).
fn token_members(rt: &crate::MockRuntime, t: &str) -> Vec<String> {
    let mut members: Vec<String> = rt
        .described()
        .type_desc(t)
        .and_then(|d| d.concurrency.as_ref()?.member.clone())
        .into_iter()
        .collect();
    for member in crate::call::tokens::integer_body_tokens(rt, t) {
        if !members.contains(&member) {
            members.push(member);
        }
    }
    members
}

/// Sets the first value of each of type `t`'s tokens ([`token_members`]) on `record`,
/// when the request sent none and a read declares the member: an integer's `1` (its
/// `minimum`, if higher), else a value drawn as the member declares it, as a minted id
/// is.
fn initial_token(
    rt: &crate::MockRuntime,
    (seed, t): (crate::call::header_ids::MintSeed<'_>, &str),
    record: &mut Value,
) {
    let bundle = rt.bundle();
    for member in token_members(rt, t) {
        if mb_mcm::member_at(record, &member).is_some() {
            continue;
        }
        let Some(schema) = read_schemas(rt, t)
            .iter()
            .find_map(|s| property_schema(bundle, s, &member))
        else {
            continue;
        };
        let parts = whole_parts(bundle, &schema);
        let integer = parts.iter().any(|p| scalar_types(p).contains(&"integer"));
        let value = match integer {
            true => {
                let floor = parts
                    .iter()
                    .find_map(|p| p.get("minimum").and_then(Value::as_i64))
                    .unwrap_or(1);
                Some(Value::from(floor.max(1)))
            }
            false => crate::call::header_ids::draw_id_as(rt, (seed, t, &member), &schema),
        };
        if let Some(value) = value.filter(|v| fits(rt, &schema, v)) {
            mb_mcm::set_member_at(record, &member, value);
        }
    }
}

/// Settles what fill wrote of `missing` on `record`: a required member (in `required`)
/// it left without a value takes `null` where its schema allows that, else `false` for a
/// boolean; a text member whose pattern opens with the identity's literal prefix and all
/// of its pattern (Stripe's `client_secret`, `^pi_…_secret_…$` after `^pi_…$`) starts
/// with the record's own id, where that still fits.
fn settle_filled(
    rt: &crate::MockRuntime,
    (field, schemas): (&str, &[Value]),
    (missing, required): (&[(String, Value)], &[String]),
    record: &mut Map<String, Value>,
) {
    let bundle = rt.bundle();
    for (name, member) in missing.iter().filter(|(n, _)| required.contains(n)) {
        if record.contains_key(name) {
            continue;
        }
        let admitted = admitted_types(bundle, member);
        if fits(rt, member, &Value::Null) {
            record.insert(name.clone(), Value::Null);
        } else if admitted.iter().any(|t| t == "boolean") && fits(rt, member, &Value::Bool(false)) {
            record.insert(name.clone(), Value::Bool(false));
        }
    }
    let id = record.get(field).and_then(id_string);
    let id_pattern = schemas
        .iter()
        .find_map(|s| record_pattern(bundle, s, field))
        .map(|p| p.trim_start_matches('^').trim_end_matches('$').to_owned());
    // The identity's pattern opens with literal text (`pi_`): a member's pattern opening
    // with the whole of it embeds the id.
    let literal = |core: &str| {
        core.chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | ':'))
            .count()
    };
    let (Some(id), Some(core)) = (id, id_pattern.filter(|c| literal(c) > 0)) else {
        return;
    };
    if !id.starts_with(&core[..literal(&core)]) {
        return;
    }
    for (name, member) in missing {
        let Some(pattern) = record_pattern(bundle, member, "") else {
            continue;
        };
        let embeds_id = pattern
            .strip_prefix('^')
            .is_some_and(|p| p.starts_with(&core))
            && pattern.trim_start_matches('^').len() > core.len();
        let Some(Value::String(drawn)) = record.get(name) else {
            continue;
        };
        if !embeds_id || drawn.starts_with(&id) || drawn.chars().count() <= id.chars().count() {
            continue;
        }
        let rest: String = drawn.chars().skip(id.chars().count()).collect();
        let formed = Value::String(format!("{id}{rest}"));
        if fits(rt, member, &formed) {
            record.insert(name.clone(), formed);
        }
    }
}

/// The `pattern` of `schema`'s member `field`, or of `schema` itself for an empty
/// `field`.
fn record_pattern(bundle: &mb_mcm::Bundle, schema: &Value, field: &str) -> Option<String> {
    let member = match field {
        "" => schema.clone(),
        name => property_schema(bundle, schema, name)?,
    };
    whole_parts(bundle, &member)
        .iter()
        .find_map(|p| p.get("pattern").and_then(Value::as_str).map(str::to_owned))
}

/// Sets on `record` the members nothing set that refer to the record a path parameter of
/// `call` names: a member named for the parameter (`pullrequest` for
/// `pull_request_id`, letters compared without case or separators) takes the record the
/// session holds under that id, projected to its schema, else for an object member an
/// object holding the id, else the value itself where it fits.
async fn path_references(
    state: &mut StateAccess<'_>,
    (session, call): (&mb_state::SessionKey, &OperationCall),
    shape: &Shape,
    record: &mut Map<String, Value>,
    owned: &[String],
) {
    let rt = state.rt;
    let bundle = rt.bundle();
    let squeezed = |name: &str| {
        name.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase()
    };
    for (param, value) in &call.path_params {
        let named = squeezed(crate::call::reference_prefix(param).unwrap_or(param));
        let Some((name, member)) = shape
            .members
            .iter()
            .find(|(name, _)| squeezed(name) == named || squeezed(name) == squeezed(param))
        else {
            continue;
        };
        if record.contains_key(name) || owned.contains(name) {
            continue;
        }
        let set = match holds_object(bundle, &non_null(bundle, member)) {
            true => match embed(state, session, (name, member), value).await {
                Some(held) => Some(held),
                None => {
                    let item = non_null(bundle, member);
                    property_schema(bundle, &item, "id").map(|id| {
                        let mut object = Map::new();
                        object.insert("id".to_owned(), typed_as(bundle, &id, value));
                        Value::Object(object)
                    })
                }
            },
            false => Some(typed_as(bundle, member, value)).filter(|v| fits(rt, member, v)),
        };
        if let Some(set) = set {
            record.insert(name.clone(), set);
        }
    }
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
    fn request_record(
        &self,
        rt: &crate::MockRuntime,
        request: Option<&Value>,
    ) -> Option<Map<String, Value>> {
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
        // A wrapper's member is the record's only where it fits it: `from: {type: "user"}`
        // names the sender, not the record's `type`.
        let fits_member = |name: &str, value: &Value| {
            self.members
                .iter()
                .find(|(m, _)| m == name)
                .is_some_and(|(_, schema)| shaped(rt, (schema, false), value))
        };
        for (name, value) in wrapped {
            if declares(name) && !record.contains_key(name) && fits_member(name, value) {
                record.insert(name.clone(), value.clone());
            }
        }
        Some(record)
    }

    /// The top-level members besides identity `field` that a path may name a record
    /// of type `t` by, each with its schema: the described alternates, the members its
    /// routes read it by ([`crate::call::BundleIndex::identity_fields`]), and the later
    /// keys of a compound parameter its read by id takes (`name` of
    /// `{app_id_or_name}`), where the type declares them.
    fn key_members(&self, rt: &crate::MockRuntime, t: &str, field: &str) -> Vec<(String, Value)> {
        let index = rt.index();
        let mut names: Vec<String> = records::alternates(rt, t);
        names.extend(index.identity_fields(t).iter().cloned());
        let param = index
            .detail_reads
            .get(t)
            .and_then(|i| rt.bundle().operations.get(*i))
            .and_then(|read| read.identity.as_ref()?.path_param.clone());
        if let Some(compound) = param.as_deref().and_then(Compound::of) {
            let stem = format!("{}_", compound.stem);
            for key in compound.keys.iter().skip(1) {
                names.extend(key.strip_prefix(&stem).map(str::to_owned));
            }
        }
        let mut out: Vec<(String, Value)> = Vec::new();
        for name in names {
            if name == field || out.iter().any(|(n, _)| *n == name) {
                continue;
            }
            if let Some((_, member)) = self.members.iter().find(|(m, _)| *m == name) {
                out.push((name, member.clone()));
            }
        }
        out
    }

    /// Sets on `record` each member the type declares that a query parameter `op`
    /// declares names (`POST /boards?name=…`), typed as the member is, when the
    /// request's record holds no value for it.
    fn query_members(
        &self,
        rt: &crate::MockRuntime,
        op: &Operation,
        call: &OperationCall,
        record: &mut Map<String, Value>,
    ) {
        let bundle = rt.bundle();
        let Some(params) = op.input.query.as_ref() else {
            return;
        };
        for (name, value) in &call.query {
            if record.contains_key(name) || property_schema(bundle, params, name).is_none() {
                continue;
            }
            if let Some((_, member)) = self.members.iter().find(|(m, _)| m == name) {
                record.insert(name.clone(), typed_as(bundle, member, value));
            }
        }
    }

    /// Sets on `record` a member the type declares under a path parameter's own name,
    /// from that parameter, and the top-level members of what the path says of it
    /// (`from_path`, [`path_set`]: the described parent member, a list scope's member),
    /// typed as the member is, when the record holds no value for them. They are set
    /// before [`reshape`], which makes one naming an object member the record it names,
    /// or leaves it for fill; a dotted one is set after ([`Shape::path_scoped`]).
    fn path_members(
        &self,
        rt: &crate::MockRuntime,
        call: &OperationCall,
        from_path: &[(String, Value)],
        record: &mut Map<String, Value>,
    ) {
        let top = from_path.iter().filter(|(member, _)| !member.contains('.'));
        self.set_from_path(rt, top, record);
        for (name, value) in &call.path_params {
            if self.members.iter().any(|(m, _)| m == name) && !record.contains_key(name) {
                record.insert(name.clone(), value.clone());
            }
        }
    }

    /// Sets on `record` the dotted members of what the path says of it (`from_path`,
    /// [`path_set`]: a dotted parent or scope member, a described member of an object
    /// member a path parameter is named for), each at its dotted path and typed as the
    /// member is. An object member holding no object (`null`, or the path's own value
    /// as sent, which [`reshape`] left) becomes one, so the value goes into the
    /// described member, never over the object.
    fn path_scoped(
        &self,
        rt: &crate::MockRuntime,
        from_path: &[(String, Value)],
        record: &mut Map<String, Value>,
    ) {
        let dotted = from_path.iter().filter(|(member, _)| member.contains('.'));
        self.set_from_path(rt, dotted, record);
    }

    /// Sets each of `values` on `record` at its (dotted) member, typed as the member
    /// is, unless the record holds a value there.
    fn set_from_path<'v>(
        &self,
        rt: &crate::MockRuntime,
        values: impl Iterator<Item = &'v (String, Value)>,
        record: &mut Map<String, Value>,
    ) {
        let bundle = rt.bundle();
        let mut held = Value::Object(std::mem::take(record));
        for (member, value) in values {
            if mb_mcm::member_at(&held, member).is_some_and(|v| !v.is_null()) {
                continue;
            }
            let declared = self
                .schemas
                .iter()
                .find_map(|s| property_schema(bundle, s, member));
            let typed = match declared {
                Some(schema) => typed_as(bundle, &schema, value),
                None => value.clone(),
            };
            open_steps(&mut held, member, value);
            mb_mcm::set_member_at(&mut held, member, typed);
        }
        if let Value::Object(members) = held {
            *record = members;
        }
    }
}

/// Makes each step above dotted `member` of `record` that holds `null` or the path's
/// `value` itself an empty object, so `member` can be set below it.
fn open_steps(record: &mut Value, member: &str, value: &Value) {
    let mut at = record;
    let mut steps: Vec<&str> = member.split('.').collect();
    steps.pop();
    for step in steps {
        let Some(object) = at.as_object_mut() else {
            return;
        };
        let next = object
            .entry(step.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        let stands_in =
            next.is_null() || (!next.is_object() && id_string(next) == id_string(value));
        if stands_in {
            *next = Value::Object(Map::new());
        }
        at = next;
    }
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
    read_shapes(rt, t).into_iter().map(|(s, _)| s).collect()
}

/// [`read_schemas`], each with whether it is a list's item, which a list renders a
/// record into as it projects it ([`crate::call::lists::projected`]).
fn read_shapes(rt: &crate::MockRuntime, t: &str) -> Vec<(Value, bool)> {
    let mut schemas: Vec<(Value, bool)> = type_check(rt, t)
        .and_then(|(read, shape)| shape.or_else(|| read.success_output()?.schema.as_ref()))
        .cloned()
        .map(|s| (s, false))
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
            && !schemas.iter().any(|(s, _)| *s == shape.items)
        {
            schemas.push((shape.items, true));
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
    gaps(
        rt,
        (t, &read_schemas(rt, t)),
        record,
        &owned,
        Unsent::Required,
    )
}

/// The deepest an object member's own requirements are followed.
const GAP_DEPTH: usize = 6;

/// Which members below a record's top [`gaps`] takes.
#[derive(Clone, Copy)]
pub(super) enum Unsent<'r> {
    /// Only those a schema requires (what a replace's reads need).
    Required,
    /// Every declared one nothing set (a create's), optional ones too, of an object
    /// the request sent, but a `writeOnly` member and a reference the request could
    /// have sent ([`unsent_reference`]): with the request's schema and the request's
    /// value at that level, when it has them. An object the request did not send (one
    /// the server's own members opened, such as a `sys` holding the id) is completed
    /// with what it requires only.
    All(Option<&'r Value>, Option<&'r Value>),
}

/// What `schemas` require that `record` does not hold, each top member with the schema
/// fill writes it by: a member it lacks (but those in `owned`), with its own schema;
/// an object member it holds lacking members its schema requires (with [`Unsent::All`],
/// any member it declares), with an object schema of those alone, at any depth (to
/// [`GAP_DEPTH`]). Merged into the record by [`fill_into`].
pub(super) fn gaps(
    rt: &crate::MockRuntime,
    (t, schemas): (&str, &[Value]),
    record: &Value,
    owned: &[String],
    unsent: Unsent<'_>,
) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = Vec::new();
    for schema in schemas {
        let Some(Value::Object(properties)) = lacking((rt, t), schema, record, GAP_DEPTH, unsent)
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

/// An object schema of what `schema` requires that `value` does not hold, and below
/// the top, with [`Unsent::All`], what else it declares that `value` does not hold
/// (see [`gaps`]); `None` when it holds all of it.
fn lacking(
    (rt, t): (&crate::MockRuntime, &str),
    schema: &Value,
    value: &Value,
    depth: usize,
    unsent: Unsent<'_>,
) -> Option<Value> {
    let bundle = rt.bundle();
    // A record embedded below the top (of another type, or this one: a folder's
    // `parent`): its id is that record's, not this create's to invent (what else it
    // lacks is filled).
    let foreign: Vec<String> = (depth < GAP_DEPTH)
        .then(|| record_def(bundle, schema))
        .flatten()
        .and_then(|def| rt.index().types_by_def.get(&def))
        .map(|of| rt.index().identity_fields(of).to_vec())
        .unwrap_or_default();
    let schema = non_null(bundle, schema);
    match value {
        Value::Object(held) => {
            let required = required_members(bundle, &schema);
            // A create's every declared member, the required first: one it lacks is
            // taken below the top only (the create takes those of its top itself), and
            // what an object it holds lacks at any depth.
            let mut names = required.clone();
            let (request, sent) = match unsent {
                Unsent::All(request, sent) => (request, sent),
                Unsent::Required => (None, None),
            };
            if matches!(unsent, Unsent::All(..)) {
                for part in whole_parts(bundle, &schema) {
                    let Some(Value::Object(declared)) = part.get("properties") else {
                        continue;
                    };
                    for name in declared.keys() {
                        if !names.contains(name) {
                            names.push(name.clone());
                        }
                    }
                }
            }
            let mut properties = Map::new();
            for name in names {
                if foreign.contains(&name) {
                    continue;
                }
                let Some(member) = property_schema(bundle, &schema, &name) else {
                    continue;
                };
                // The request's schema of this member, for what lies below it.
                let request_below = request.and_then(|r| property_schema(bundle, r, &name));
                let below = match unsent {
                    Unsent::All(..) => {
                        Unsent::All(request_below.as_ref(), sent.and_then(|v| v.get(&name)))
                    }
                    Unsent::Required => Unsent::Required,
                };
                match held.get(&name) {
                    None if required.contains(&name) => {
                        properties.insert(name, member);
                    }
                    None if depth == GAP_DEPTH || !sent.is_some_and(Value::is_object) => {}
                    None => {
                        let left = write_only(bundle, &member)
                            || unsent_reference(rt, request, &name, &member);
                        if !left {
                            properties.insert(name, member);
                        }
                    }
                    Some(inner @ (Value::Object(_) | Value::Array(_))) if depth > 1 => {
                        if let Some(below) = lacking((rt, t), &member, inner, depth - 1, below) {
                            properties.insert(name, below);
                        }
                    }
                    Some(_) => {}
                }
            }
            let names: Vec<String> = properties.keys().cloned().collect();
            (!properties.is_empty()).then(
                || serde_json::json!({"type": "object", "properties": properties, "required": names}),
            )
        }
        // An array the request sent: what its items lack, as an array of as many items,
        // each of the members any of them lacks ([`fill_into`] merges them by position).
        Value::Array(items) if !items.is_empty() => {
            let item = whole_parts(bundle, &schema)
                .iter()
                .find_map(|p| p.get("items").cloned())?;
            let (item_request, sent) = match unsent {
                Unsent::All(request, sent) => (
                    request.and_then(|r| {
                        whole_parts(bundle, r)
                            .iter()
                            .find_map(|p| p.get("items").cloned())
                    }),
                    sent.and_then(Value::as_array),
                ),
                Unsent::Required => (None, None),
            };
            let mut properties = Map::new();
            for (at, held) in items.iter().enumerate() {
                let below = match unsent {
                    Unsent::All(..) => {
                        Unsent::All(item_request.as_ref(), sent.and_then(|s| s.get(at)))
                    }
                    Unsent::Required => Unsent::Required,
                };
                let Some(Value::Object(below)) = lacking((rt, t), &item, held, depth, below)
                    .as_ref()
                    .and_then(|s| s.get("properties"))
                    .cloned()
                else {
                    continue;
                };
                for (name, member) in below {
                    properties.entry(name).or_insert(member);
                }
            }
            let names: Vec<String> = properties.keys().cloned().collect();
            (!properties.is_empty()).then(|| {
                serde_json::json!({
                    "type": "array",
                    "minItems": items.len(),
                    "maxItems": items.len(),
                    "items": {"type": "object", "properties": properties, "required": names},
                })
            })
        }
        _ => None,
    }
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
            // An array's items take what each lacks of the filled item at its position.
            (Some(Value::Array(held)), Value::Array(below)) => {
                for (item, filled) in held.iter_mut().zip(below) {
                    if let (Value::Object(item), Value::Object(filled)) = (item, filled) {
                        fill_into(item, filled);
                    }
                }
            }
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
/// render into a read of type `t` ([`read_shapes`]), checked as that read checks it: a
/// list's item projected to its schema first, as the list projects it. The first
/// violation (`<pointer> <keyword>`) of the first read schema neither validates
/// against, as a read's `render_error` gives it. `None` when every read renders it.
fn unreadable(
    rt: &crate::MockRuntime,
    t: &str,
    record: &Value,
    derived: Option<&Value>,
) -> Option<String> {
    let engine = &rt.engine;
    let bundle = rt.bundle();
    for (schema, item) in read_shapes(rt, t) {
        let as_read = |value: &Value| match item {
            true => crate::call::lists::projected(bundle, Some(&schema), value, 0),
            false => value.clone(),
        };
        let Some(found) = engine.schema_violations(bundle, &schema, &as_read(record)) else {
            continue;
        };
        if found.is_empty()
            || derived.is_some_and(|d| {
                engine
                    .schema_violations(bundle, &schema, &as_read(d))
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
    plan.exprs.fell_back(Fallback::RecordInvalid.as_str(), None);
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
    (schema, fits): (&Value, &Fits<'_>),
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
                            if let Some(decided) = decided(bundle, member)
                                && fits(member, &decided)
                            {
                                held.insert(name.clone(), decided);
                            }
                        }
                        Some(inner) => preset(bundle, (member, fits), inner, &[], depth - 1),
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in parts.iter().filter_map(|p| p.get("items")) {
                for held in items.iter_mut() {
                    preset(bundle, (item, fits), held, &[], depth - 1);
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
    let Some(schema) = request_schema(rt, op) else {
        return;
    };
    let mut preset_copy = record.clone();
    let fits = |schema: &Value, value: &Value| fits(rt, schema, value);
    preset(bundle, (&schema, &fits), &mut preset_copy, owned, GAP_DEPTH);
    let (Value::Object(set), Value::Object(held)) = (preset_copy, record) else {
        return;
    };
    for (name, value) in set {
        if held.get(&name) == Some(&value) {
            continue;
        }
        let declared = shape.members.iter().find(|(m, _)| *m == name);
        if declared.is_some_and(|(_, member)| fits(member, &value)) {
            held.insert(name, value);
        }
    }
}

/// Reshapes the members of `record` (the request's) that do not fit what the reads of
/// type `t` declare of them ([`shaped`]), but those in `owned`: a value naming a record the
/// session holds (a reference where the record holds the object: Buildkite's
/// `pipeline` slug, Lob's `to` address id) becomes that record, projected to the
/// member's schema, and so does each item of an array of them; any other is removed,
/// for fill to write. A member whose every schema it fits is kept as sent.
async fn reshape(
    state: &mut StateAccess<'_>,
    session: &mb_state::SessionKey,
    t: &str,
    record: &mut Map<String, Value>,
    owned: &[String],
) {
    let rt = state.rt;
    let bundle = rt.bundle();
    let reads = read_shapes(rt, t);
    let names: Vec<String> = record.keys().cloned().collect();
    for name in names {
        if owned.contains(&name) {
            continue;
        }
        let schemas: Vec<(Value, bool)> = reads
            .iter()
            .filter_map(|(s, item)| Some((property_schema(bundle, s, &name)?, *item)))
            .collect();
        let fits_all = |value: &Value| {
            schemas
                .iter()
                .all(|(s, item)| shaped(rt, (s, *item), value))
        };
        let Some(sent) = record.get(&name).cloned() else {
            continue;
        };
        if schemas.is_empty() || fits_all(&sent) {
            continue;
        }
        let Some((member, _)) = schemas.first() else {
            continue;
        };
        // A reference where the read holds the record (a scalar for an object, an array
        // of scalars for an array of objects) is the record it names; a value of another
        // JSON type is converted with nothing lost (`"a,b"` for `["a", "b"]`), else left
        // for fill. A value of the member's own type breaking it otherwise (an enum's
        // case, a format, a length) is the request's, and the gate refuses it.
        if !references_object(bundle, member, &sent) {
            // With no conversion losing nothing, the value stays as sent and the gate
            // refuses the record: client data is never replaced by an invented value.
            if of_another_type(bundle, member, &sent)
                && let Some(value) = converted(bundle, member, &sent).filter(|c| fits_all(c))
            {
                record.insert(name, value);
            }
            continue;
        }
        let embedded = match &sent {
            Value::Array(items) => {
                let item = whole_parts(bundle, member)
                    .iter()
                    .find_map(|p| p.get("items").cloned());
                let mut out = Vec::new();
                for value in items {
                    let one = match &item {
                        Some(item) => embed(state, session, (&name, item), value).await,
                        None => None,
                    };
                    match one {
                        Some(one) => out.push(one),
                        None => {
                            out.clear();
                            break;
                        }
                    }
                }
                (!out.is_empty() || items.is_empty()).then_some(Value::Array(out))
            }
            value => embed(state, session, (&name, member), value).await,
        };
        // A reference to no held record names none the mock knows: `null` where the
        // member takes it, else left for fill.
        match embedded.filter(|e| fits_all(e)) {
            Some(value) => {
                record.insert(name, value);
            }
            None if fits_all(&Value::Null) => {
                record.insert(name, Value::Null);
            }
            None => {
                record.remove(&name);
            }
        }
    }
}

/// The record a request value under `name` names, as `schema` (an object's) holds it:
/// a text or number that is the id of a record the session holds, of the type the
/// member's schema is the record of, else of a type named for the member, projected to
/// `schema`. `None` for a value that names none, or a schema of no object.
async fn embed(
    state: &mut StateAccess<'_>,
    session: &mb_state::SessionKey,
    (name, schema): (&str, &Value),
    value: &Value,
) -> Option<Value> {
    let rt = state.rt;
    let bundle = rt.bundle();
    let id = match value {
        Value::String(_) | Value::Number(_) => id_string(value)?,
        _ => return None,
    };
    let object = whole_parts(bundle, schema).iter().any(|p| {
        p.get("properties").is_some() || p.get("type").and_then(Value::as_str) == Some("object")
    });
    if !object {
        return None;
    }
    let index = rt.index();
    let def = record_def(bundle, schema);
    let def = def.as_deref();
    let mut types: Vec<String> = crate::call::entity_type(index, def, name)
        .into_iter()
        .collect();
    for t in crate::call::relations::reference_types(Some(index), name) {
        if !types.contains(&t) {
            types.push(t);
        }
    }
    for t in types.iter().take(MAX_EMBED_TYPES) {
        if let Some(held) = records::held_by_key(state, session, t, &id).await {
            return Some(crate::call::lists::projected(
                bundle,
                Some(schema),
                &held.data,
                0,
            ));
        }
    }
    None
}

/// Whether `value` is a reference where `schema` holds a record: a text or number where
/// it holds an object, an array of them where it holds an array of objects.
fn references_object(bundle: &mb_mcm::Bundle, schema: &Value, value: &Value) -> bool {
    let scalar = |v: &Value| matches!(v, Value::String(_) | Value::Number(_));
    match value {
        Value::Array(items) => {
            !items.is_empty()
                && items.iter().all(scalar)
                && whole_parts(bundle, schema)
                    .iter()
                    .filter_map(|p| p.get("items"))
                    .any(|item| holds_object(bundle, item))
        }
        v => scalar(v) && holds_object(bundle, schema),
    }
}

/// Whether every value of `schema` is an object.
fn holds_object(bundle: &mb_mcm::Bundle, schema: &Value) -> bool {
    whole_parts(bundle, schema).iter().any(|p| {
        p.get("properties").is_some() || p.get("type").and_then(Value::as_str) == Some("object")
    })
}

/// Whether `value`, a member's, is of the shape `schema` (a read's member schema; a
/// list item's, `item`, projected first, as the list projects it) declares: it breaks
/// nothing but what it lacks (`required`), which fill writes.
fn shaped(rt: &crate::MockRuntime, (schema, item): (&Value, bool), value: &Value) -> bool {
    let value = match item {
        true => crate::call::lists::projected(rt.bundle(), Some(schema), value, 0),
        false => value.clone(),
    };
    let bundle = rt.bundle();
    let only_lacking = |schema: &Value| {
        rt.engine
            .schema_violations(bundle, schema, &value)
            .is_some_and(|found| found.iter().all(|(_, keyword)| keyword == "required"))
    };
    // A nullable object (`anyOf: [object, null]`) is checked as its object.
    only_lacking(schema) || (!value.is_null() && only_lacking(&non_null(bundle, schema)))
}

/// The JSON types `schema` admits (`object`, `array`, `string`, `integer`, `number`,
/// `boolean`, `null`), through its `$ref`s, `allOf`s and every branch of an `anyOf` or
/// `oneOf`; empty when it says none (any value).
fn admitted_types(bundle: &mb_mcm::Bundle, schema: &Value) -> Vec<String> {
    fn visit(bundle: &mb_mcm::Bundle, schema: &Value, depth: usize, out: &mut Vec<String>) {
        if depth > 8 {
            return;
        }
        if let Some(target) =
            crate::call::bundle_index::def_name(schema).and_then(|def| bundle.defs.get(def))
        {
            return visit(bundle, target, depth + 1, out);
        }
        let mut add = |t: &str| {
            if !out.iter().any(|o| o == t) {
                out.push(t.to_owned());
            }
        };
        match schema.get("type") {
            Some(Value::String(t)) => add(t),
            Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).for_each(&mut add),
            _ => {}
        }
        if schema.get("properties").is_some() {
            add("object");
        }
        if schema.get("items").is_some() {
            add("array");
        }
        if schema.get("nullable").and_then(Value::as_bool) == Some(true) {
            add("null");
        }
        for key in ["allOf", "anyOf", "oneOf"] {
            for branch in schema
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                visit(bundle, branch, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    visit(bundle, schema, 0, &mut out);
    out
}

/// Whether `value` is of a JSON type `schema` does not admit at all ([`admitted_types`]):
/// an array where it holds text, text where it holds an array. A value of an admitted
/// type that breaks the schema otherwise (an enum's case, a format) is not.
fn of_another_type(bundle: &mb_mcm::Bundle, schema: &Value, value: &Value) -> bool {
    let admitted = admitted_types(bundle, schema);
    if admitted.is_empty() {
        return false;
    }
    let has = |t: &str| admitted.iter().any(|a| a == t);
    let of = match value {
        Value::Null => has("null"),
        Value::Bool(_) => has("boolean"),
        Value::Number(n) if n.is_i64() || n.is_u64() => has("integer") || has("number"),
        Value::Number(_) => has("number"),
        Value::String(_) => has("string"),
        Value::Array(_) => has("array"),
        Value::Object(_) => has("object"),
    };
    !of
}

/// `value` as `schema` holds it, with nothing lost: a number as its text; text that
/// is exactly a number (`"94107"`, not `"0.50"`) as that number; `"true"` and `"false"`
/// as booleans; text as the array of its comma-separated parts (`"bug,ui"`, split on
/// commas alone), where `schema` holds an array of text. `None` for any other.
fn converted(bundle: &mb_mcm::Bundle, schema: &Value, value: &Value) -> Option<Value> {
    let admitted = admitted_types(bundle, schema);
    let has = |t: &str| admitted.iter().any(|a| a == t);
    match value {
        Value::Number(n) if has("string") => Some(Value::String(n.to_string())),
        Value::String(text)
            if has("integer") && text.parse::<i64>().is_ok_and(|n| n.to_string() == *text) =>
        {
            text.parse::<i64>().ok().map(Value::from)
        }
        Value::String(text) if has("number") => {
            let n = text
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)?;
            (n.to_string() == *text).then_some(Value::Number(n))
        }
        Value::String(text) if has("boolean") && matches!(text.as_str(), "true" | "false") => {
            Some(Value::Bool(text == "true"))
        }
        Value::String(text) if has("array") => {
            let item = whole_parts(bundle, &non_null(bundle, schema))
                .iter()
                .find_map(|p| p.get("items").cloned())?;
            (admitted_types(bundle, &item) == ["string"]).then(|| {
                Value::Array(
                    text.split(',')
                        .map(|part| Value::String(part.to_owned()))
                        .collect(),
                )
            })
        }
        _ => None,
    }
}

/// The `$defs` name of the record `schema` holds: its `$ref`'s, also through a lone
/// `allOf` or a nullable's one other branch (`allOf: [$ref], nullable`,
/// `anyOf: [$ref, null]`).
fn record_def(bundle: &mb_mcm::Bundle, schema: &Value) -> Option<String> {
    let def_of = |schema: &Value| -> Option<String> {
        let lone = || match schema
            .get("allOf")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
        {
            Some([only]) => crate::call::bundle_index::def_name(only),
            _ => None,
        };
        crate::call::bundle_index::def_name(schema)
            .or_else(lone)
            .map(str::to_owned)
    };
    def_of(schema).or_else(|| def_of(&non_null(bundle, schema)))
}

/// The types [`embed`] looks a reference up in, at most.
const MAX_EMBED_TYPES: usize = 8;

/// Whether a value (the second) validates against a member schema (the first).
type Fits<'a> = dyn Fn(&Value, &Value) -> bool + 'a;

/// Whether `value` validates against member schema `schema`.
fn fits(rt: &crate::MockRuntime, schema: &Value, value: &Value) -> bool {
    rt.engine
        .schema_violations(rt.bundle(), schema, value)
        .is_some_and(|v| v.is_empty())
}

/// Mints an id ([`crate::call::header_ids::draw_id_as`]) for each array item `value`
/// holds, as `schema` declares it, whose schema declares member `field` (the type's
/// identity member: a line item's `id`) and that lacks it, at any depth (to `depth`);
/// unique among the items of its array.
fn mint_items(
    rt: &crate::MockRuntime,
    (seed, t, schema): (crate::call::header_ids::MintSeed<'_>, &str, &Value),
    value: &mut Value,
    field: &str,
    depth: usize,
) {
    if depth == 0 {
        return;
    }
    let bundle = rt.bundle();
    let parts = whole_parts(bundle, schema);
    match value {
        Value::Object(held) => {
            for part in &parts {
                let Some(Value::Object(properties)) = part.get("properties") else {
                    continue;
                };
                for (name, member) in properties {
                    if let Some(inner) = held.get_mut(name) {
                        mint_items(rt, (seed, t, member), inner, field, depth - 1);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in parts.iter().filter_map(|p| p.get("items")) {
                // An item whose schema is another type's record is that record: its id
                // is not this create's to mint.
                let other = crate::call::bundle_index::def_name(item)
                    .and_then(|def| rt.index().types_by_def.get(def))
                    .is_some_and(|of| of != t);
                let declared = property_schema(bundle, item, field).filter(|_| !other);
                let mut given: Vec<Value> =
                    items.iter().filter_map(|i| i.get(field).cloned()).collect();
                for held in items.iter_mut() {
                    if let (Some(declared), Value::Object(members)) = (&declared, &mut *held)
                        && !members.contains_key(field)
                        && let Some(id) = (0..MINT_ITEM_ATTEMPTS)
                            .filter_map(|_| {
                                crate::call::header_ids::draw_id_as(rt, (seed, t, field), declared)
                            })
                            .find(|id| !given.contains(id))
                    {
                        given.push(id.clone());
                        members.insert(field.to_owned(), id);
                    }
                    mint_items(rt, (seed, t, item), held, field, depth - 1);
                }
            }
        }
        _ => {}
    }
}

/// Whether identity member `field` names an id (`id`, `uuid`, `line_item_id`, `gid`),
/// which [`mint_items`] mints, not a name or a key a client chooses.
fn id_shaped(field: &str) -> bool {
    let lower = field.to_ascii_lowercase();
    lower == "id"
        || lower.ends_with("_id")
        || field.ends_with("Id")
        || lower.ends_with("uuid")
        || lower == "gid"
}

/// Draws [`mint_items`] tries for an id no other item of its array holds.
const MINT_ITEM_ATTEMPTS: usize = 8;

/// The schema of the record `op`'s request holds: its body's, at the described
/// `request.record`.
fn request_schema(rt: &crate::MockRuntime, op: &Operation) -> Option<Value> {
    let body = op.input.body.as_ref()?;
    let pointer = rt
        .described()
        .op(&op.id)
        .and_then(|d| d.request.as_ref())
        .and_then(|r| r.record.as_ref())
        .map(|p| p.as_str().to_owned());
    match pointer.as_deref() {
        None | Some("") => Some(body.clone()),
        Some(at) => {
            let steps = at.trim_start_matches('/').replace('/', ".");
            property_schema(rt.bundle(), body, &steps)
        }
    }
}

/// Whether member `name` (of schema `member`) is a reference to another record the
/// request could have sent and did not, which fill leaves absent (a deal's `org_id`,
/// its `tag_ids`): a text or integer member (`null` allowed), or an array of them, that
/// the request's schema declares, named as a reference (`<x>_id`, `<x>Id`, `<x>_ids`,
/// `<x>Ids`) or for a type of the contract.
fn unsent_reference(
    rt: &crate::MockRuntime,
    request: Option<&Value>,
    name: &str,
    member: &Value,
) -> bool {
    let bundle = rt.bundle();
    let id_typed = |schema: &Value| {
        whole_parts(bundle, schema).iter().any(|p| {
            scalar_types(p)
                .iter()
                .any(|t| matches!(*t, "string" | "integer"))
        })
    };
    let parts = whole_parts(bundle, member);
    let many = parts.iter().filter_map(|p| p.get("items")).any(id_typed);
    let referential = crate::call::reference_prefix(name).is_some()
        || name
            .strip_suffix("_ids")
            .or_else(|| name.strip_suffix("Ids"))
            .is_some_and(|p| !p.is_empty())
        || crate::call::entity_type(rt.index(), None, name).is_some();
    (id_typed(member) || many)
        && request.is_some_and(|r| property_schema(bundle, r, name).is_some())
        && referential
}

/// The types a schema part's `type` names but `null`: `"string"`, or the others of
/// `["integer", "null"]`.
fn scalar_types(part: &Value) -> Vec<&str> {
    match part.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(Value::as_str)
            .filter(|t| *t != "null")
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether `schema` marks its member `writeOnly`: what a read never shows.
fn write_only(bundle: &mb_mcm::Bundle, schema: &Value) -> bool {
    whole_parts(bundle, schema)
        .iter()
        .any(|p| p.get("writeOnly").and_then(Value::as_bool) == Some(true))
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
        exec_notes: serde_json::Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_type_list_names_its_scalar_types_but_null() {
        assert_eq!(
            scalar_types(&serde_json::json!({"type": "string"})),
            vec!["string"]
        );
        assert_eq!(
            scalar_types(&serde_json::json!({"type": ["integer", "null"]})),
            vec!["integer"]
        );
        assert!(scalar_types(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn only_an_id_like_identity_is_minted_for_items() {
        for id in ["id", "uuid", "line_item_id", "lineItemId", "gid"] {
            assert!(id_shaped(id), "{id}");
        }
        for name in ["name", "key", "slug", "email"] {
            assert!(!id_shaped(name), "{name}");
        }
    }
}
