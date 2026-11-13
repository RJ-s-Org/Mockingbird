//! A held record that lacks members its read requires, completed from state rather than
//! generated again.
//!
//! A record is stored by whatever wrote it, in that writer's shape: a GraphQL create in
//! a `shares` group stores a pull request without REST's `assignee`, `_links` or `head`.
//! A read over REST that finds it does not render, and generating the read would
//! replace every stored value with the model's (the head branch the client pushed
//! included). So a read of the executor completes the record instead, at the top and
//! below it (an object or a list's items it holds, required or not), against the record
//! schema of the read's answer ([`super::create::gaps`], what that schema requires only;
//! for a GraphQL selection, what it selects, [`selected_gaps`]):
//!
//! - a member that may be `null` is `null`: nothing says what it holds;
//! - every other one is written as a create's fill writes it ([`mb_engine::Engine::fill`]:
//!   the faker's ids, numbers, moments and objects, defaults and enums, what the caller's
//!   own answers say of an object of its schema), the model asked only for free text,
//!   with a schema of those members alone; what fill leaves is settled as a create's is;
//! - the URL members it wrote are made to show the record's place ([`links::settle`]).
//!
//! A GraphQL selection asks for members a record may lack without breaking its schema
//! (a nullable field, optional in the narrowed schema). Each one the record or an object
//! it holds lacks is answered `null` as the record is rendered ([`null_selected`]), the
//! members the description derives included (a `cost` its expression writes without a
//! nullable `totalTaxAmount`); a non-null one is completed as above. Neither reaches
//! into another record the answer embeds (an object of a type with an identity, below
//! the top): it lacking what the selection asks still generates the read, over the
//! held record, as one stored by a smaller selection does, and a member holding other
//! records (a connection of notes held on their own) is left for the read to find them.
//! A missing non-null list (of values, not records) is empty. Nor is a selection that
//! renames a field completed ([`CallPlan::renames`]: its keys are no members of the
//! record). With a model, a selection asking for what the record lacks generates over
//! it as before (the model writes what nothing stated), but for a nullable member of
//! one the description computes, whose expression states the whole value: that one
//! is `null` either way. Without one, generation could only draw faker values over
//! the record, so it is completed from state.
//!
//! It is tried only when what keeps the record from rendering is members it lacks
//! ([`only_lacking`]): a stored value of another type than the read declares is nothing
//! completion fixes, and the read generates.
//!
//! What the record holds is never changed: only members it lacks are added, so repeated
//! reads answer alike:
//! - a record the session holds under the read's own type and key is stored completed,
//!   when it is still as it was read (a compare and set, as [`super::linked`] stores
//!   what it names), so a write another call made meanwhile is never undone;
//! - any other (a record another mock of the group stored, which the read found in its
//!   own shape) stays the writer's, and no copy of it is stored beside it: the
//!   completion is kept on its own ([`Kept`], by the reading mock, type and key) with
//!   the record as the read found it. A later read of the record unchanged answers that
//!   completion again, with no fill; once the writer changed it, what the record now
//!   holds wins, what was completed before fills what it still lacks, and only what is
//!   left is filled anew.
//!
//! With no model to answer (none configured, or the provider marked unusable), a list
//! of the executor completes each record it keeps the same way, against its items'
//! schema ([`completed_item`]), rather than the faker drawing the page whole; with a
//! model the page is generated in one call, not a fill per record.

use mb_engine::FillContext;
use mb_mcm::Protocol;
use mb_state::{INTERNAL_TYPE, StoredResource};
use serde_json::{Map, Value, json};
use tracing::Instrument;

use super::create::{self, Unsent};
use super::redraw;
use crate::call::bundle_index::record_schema_of;
use crate::call::{CallPlan, OperationCall, StateAccess, links, state_span, write_memory};

/// A held record completed for a read.
pub(super) struct Completed {
    /// The record with what it lacked.
    pub(super) record: StoredResource,
    /// What the fill's model call cost; nothing when none was made.
    pub(super) usage: mb_llm::Usage,
    /// Whether the session holds the record under the read's own type and key.
    own: bool,
    /// Whether it is a completion kept before, of the record as it is now.
    reused: bool,
    /// Whether it completes the record for a list's items ([`completed_item`]), kept
    /// apart from a read's: an item's schema may require less than a read's.
    item: bool,
}

/// What a completion of a record held under another key (another mock's) is kept as,
/// in the state store's reserved type ([`INTERNAL_TYPE`]): the record as the read found
/// it (`basis`) and as completed (`data`).
struct Kept {
    basis: Value,
    data: Value,
}

/// The reserved document a completion of `read` is kept under by the reading mock
/// `mock`, for a read of it or (`item`) a list's items.
fn kept_key(mock: &str, read: &StoredResource, item: bool) -> String {
    let base = format!("#completed/{mock}/{}/{}", read.resource_type, read.id);
    match item {
        true => format!("{base}/item"),
        false => base,
    }
}

/// Whether the session holds `read` under its own type and key, as read.
async fn own(state: &mut StateAccess<'_>, plan: &CallPlan<'_>, read: &StoredResource) -> bool {
    state
        .get(&plan.session, &read.resource_type, &read.id)
        .instrument(state_span())
        .await
        .is_some_and(|current| current.data == read.data)
}

/// The completion kept of `read`, if any.
async fn kept(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    read: &StoredResource,
    item: bool,
) -> Option<Kept> {
    let key = kept_key(&state.rt.name, read, item);
    let doc = state
        .get(&plan.session, INTERNAL_TYPE, &key)
        .instrument(state_span())
        .await?;
    Some(Kept {
        basis: doc.data.get("basis")?.clone(),
        data: doc.data.get("data")?.clone(),
    })
}

/// Whether every violation keeping `stored` from rendering as `op`'s answer is a
/// member it lacks (`required`), followed into the branch of a union the value is of
/// ([`redraw::narrowed`]). The envelope's members are drawn as a read's are when it
/// has one.
fn only_lacking(
    rt: &crate::MockRuntime,
    op: &mb_mcm::Operation,
    stored: &StoredResource,
    response_schema: Option<&Value>,
) -> bool {
    let (engine, bundle) = (&rt.engine, rt.bundle());
    let Some(schema) = response_schema.or_else(|| op.success_output()?.schema.as_ref()) else {
        return false;
    };
    let drawn = engine.envelope_draws(bundle, op, response_schema);
    let answer = engine
        .held_answer(bundle, op, &stored.data, None, response_schema)
        .or_else(|| engine.held_answer(bundle, op, &stored.data, Some(&drawn), response_schema));
    let Some(answer) = answer else {
        return false;
    };
    let Some(found) = engine.stored_violations(bundle, op, &answer, response_schema) else {
        return false;
    };
    lacking_only(rt, schema, &answer, found, 0)
}

/// Whether `found`, the violations of `value` against `schema`, are all members it
/// lacks (`required`), followed into the branch of a union the value is of
/// ([`redraw::narrowed`]) and into the type of a nullable member (`anyOf` its type or
/// `null`, a GraphQL field's), at any depth.
fn lacking_only(
    rt: &crate::MockRuntime,
    schema: &Value,
    value: &Value,
    found: Vec<(String, String)>,
    depth: usize,
) -> bool {
    let (engine, bundle) = (&rt.engine, rt.bundle());
    let found: Vec<(String, String)> = found
        .into_iter()
        .flat_map(|(at, keyword)| redraw::narrowed(rt, schema, value, at, keyword, 0))
        .collect();
    !found.is_empty()
        && found.iter().all(|(at, keyword)| match keyword.as_str() {
            "required" => true,
            "anyOf" | "oneOf" if depth < create::GAP_DEPTH => {
                let (Some(inner_value), Some(union)) = (
                    value.pointer(at),
                    engine.schema_at(bundle, schema, at, value),
                ) else {
                    return false;
                };
                let inner = create::non_null(bundle, &union);
                inner != union
                    && engine
                        .schema_violations(bundle, &inner, inner_value)
                        .is_some_and(|v| lacking_only(rt, &inner, inner_value, v, depth + 1))
            }
            _ => false,
        })
}

/// `stored`, the record a read of `plan` holds, with what the record schema of the
/// read's answer requires that it lacks, or a GraphQL selection selects non-null (see
/// the module). `None` for an aliased GraphQL selection, a record that is no object,
/// an answer with no record schema, a record lacking nothing that schema requires, or
/// one that does not render for another reason ([`only_lacking`]).
pub(super) async fn completed(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    stored: &StoredResource,
    response_schema: Option<&Value>,
) -> Option<Completed> {
    let rt = state.rt;
    let op = plan.op;
    let graphql = op.protocol == Protocol::Graphql;
    let answer = response_schema.or_else(|| op.success_output()?.schema.as_ref())?;
    if !stored.data.is_object()
        || (graphql && (plan.engine.has_provider() || plan.renames))
        || !only_lacking(rt, op, stored, response_schema)
    {
        return None;
    }
    let bundle = rt.bundle();
    let schema = match op.identity.as_ref().filter(|i| i.record.is_some()) {
        Some(identity) => record_schema_of(bundle, identity, answer)?,
        None => answer.clone(),
    };
    complete(state, (plan, call), stored, (schema, false)).await
}

/// `stored`, a record a list of `plan` lists, with what `item`, the schema of the
/// list's items, requires that it lacks, as [`completed`] completes a read's record.
/// `None` for a GraphQL list, a record that is no object or lacks nothing the item
/// requires, or one whose item does not validate for another reason.
pub(super) async fn completed_item(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    stored: &StoredResource,
    item: &Value,
) -> Option<Completed> {
    let rt = state.rt;
    if plan.op.protocol == Protocol::Graphql || !stored.data.is_object() {
        return None;
    }
    let projected = crate::call::lists::projected(rt.bundle(), Some(item), &stored.data, 0);
    let found = rt.engine.schema_violations(rt.bundle(), item, &projected)?;
    let mut found = found
        .into_iter()
        .flat_map(|(at, keyword)| redraw::narrowed(rt, item, &projected, at, keyword, 0))
        .peekable();
    if found.peek().is_none() || !found.all(|(_, keyword)| keyword == "required") {
        return None;
    }
    // Not one with remembered writes it does not show yet: generation catches up on
    // them, as for a read.
    if write_memory::remembers(state, &plan.session, &stored.resource_type, &stored.id)
        .instrument(state_span())
        .await
    {
        return None;
    }
    complete(state, (plan, call), stored, (item.clone(), true)).await
}

/// `stored` with what `schema` (its record schema) requires that it lacks: the
/// completion of [`completed`] and [`completed_item`].
async fn complete(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    stored: &StoredResource,
    (schema, item): (Value, bool),
) -> Option<Completed> {
    let rt = state.rt;
    let op = plan.op;
    let graphql = op.protocol == Protocol::Graphql;
    let bundle = rt.bundle();
    let schemas = [schema];
    let t = stored.resource_type.as_str();
    // The members the description derives are answered from state, never stored: the
    // top member holding one is left as it is, as a create's fill leaves it.
    let derived = derived_members(rt, t);
    // What the read's schema requires that `record` lacks; for a GraphQL selection,
    // what it selects that may not be null.
    let gaps = |record: &Value| match graphql {
        true => selected_gaps(rt, &schemas[0], record, &derived),
        false => create::gaps(rt, (t, &schemas), record, &derived, Unsent::Required),
    };
    let mut data = stored.data.clone();
    if gaps(&data).is_empty() {
        return None;
    }
    let own = own(state, plan, stored).await;
    let earlier = match own {
        true => None,
        false => kept(state, plan, stored, item).await,
    };
    let Value::Object(known) = &mut data else {
        return None;
    };
    if let Some(earlier) = earlier {
        if earlier.basis == stored.data {
            // The record as the completion found it: that completion again, when it
            // holds what this read asks (a GraphQL selection may ask more than the one
            // it was completed for); else it is completed further and kept so.
            if gaps(&earlier.data).is_empty() {
                return Some(Completed {
                    record: StoredResource {
                        data: earlier.data,
                        ..stored.clone()
                    },
                    usage: mb_llm::Usage::default(),
                    own,
                    reused: true,
                    item,
                });
            }
            if let Value::Object(before) = earlier.data {
                *known = before;
            }
        } else if let Value::Object(before) = earlier.data {
            // The writer changed it: what it holds wins, the earlier completion fills
            // what it still lacks.
            create::fill_into(known, before);
        }
    }
    let lacking = gaps(&Value::Object(known.clone()));
    // What may be `null` is: nothing says what it holds. For a GraphQL selection, a
    // list that may not be null is empty: nothing says what items it holds.
    for (name, member) in &lacking {
        nulled(rt, (name, member), known, create::GAP_DEPTH, graphql);
    }
    let missing = gaps(&Value::Object(known.clone()));
    let mut usage = mb_llm::Usage::default();
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|(name, _)| name.clone()).collect();
        let caller = create::caller(state, plan).instrument(state_span()).await;
        let filled = plan
            .engine
            .fill(
                bundle,
                &create::fill_schema(&missing, &names),
                known,
                &names,
                FillContext {
                    op,
                    call,
                    sent: None,
                    caller: &caller,
                    states: &[],
                },
            )
            .await;
        usage = filled.usage;
        create::fill_into(known, filled.members);
        let field = create::identity_member(rt, op, t);
        create::settle_filled(rt, (field, &schemas), (&missing, &names), known);
    }
    // The URL members it wrote show the record's place; what it held stays.
    let held: Vec<String> = stored
        .data
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    links::settle(state, &plan.session, (op, call), (t, &held), &mut data)
        .instrument(state_span())
        .await;
    let added: Vec<&String> = data
        .as_object()
        .into_iter()
        .flat_map(Map::keys)
        .filter(|name| !held.contains(name))
        .collect();
    tracing::info!(
        mock = %rt.name,
        operation = %op.id,
        members = ?added,
        model_called = usage.prompt_tokens > 0 || usage.completion_tokens > 0,
        "completed a held record its read requires more of"
    );
    Some(Completed {
        record: StoredResource {
            data,
            ..stored.clone()
        },
        usage,
        own,
        reused: false,
        item,
    })
}

/// Sets to `null` member `name` of `held`, lacking from it, when `member` (its schema)
/// allows that, else, with `lists`, to an empty list when it allows that; below a
/// member `held` holds (`member` then [`create::gaps`]' schema of what it lacks), the
/// same for each member it lacks, in an object or in each item of a list, to `depth`
/// levels.
fn nulled(
    rt: &crate::MockRuntime,
    (name, member): (&str, &Value),
    held: &mut Map<String, Value>,
    depth: usize,
    lists: bool,
) {
    if depth == 0 {
        return;
    }
    match held.get_mut(name) {
        None => {
            if create::fits(rt, member, &Value::Null) {
                held.insert(name.to_owned(), Value::Null);
            } else if lists
                && items_of(member, 0).is_some()
                && create::fits(rt, member, &Value::Array(Vec::new()))
            {
                held.insert(name.to_owned(), Value::Array(Vec::new()));
            }
        }
        Some(Value::Object(inner)) => {
            for (below, schema) in lacking_members(member) {
                nulled(rt, (&below, &schema), inner, depth - 1, lists);
            }
        }
        Some(Value::Array(items)) => {
            let Some(item) = member.get("items") else {
                return;
            };
            let below = lacking_members(item);
            for item in items {
                if let Value::Object(inner) = item {
                    for (name, schema) in &below {
                        nulled(rt, (name, schema), inner, depth - 1, lists);
                    }
                }
            }
        }
        Some(_) => {}
    }
}

/// The top members of type `t` its description derives (`derived`, `computed`): answered
/// from state on every read, never stored.
pub(super) fn derived_members(rt: &crate::MockRuntime, t: &str) -> Vec<String> {
    rt.described()
        .type_desc(t)
        .map(|d| {
            d.derived
                .keys()
                .chain(d.computed.keys())
                .map(|m| m.split('.').next().unwrap_or(m).to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Bounds the walks over a narrowed GraphQL schema (it has no `$ref`, but may be deep).
const SELECTED_DEPTH: usize = 16;

/// The object schemas of `schema`, a GraphQL selection's narrowed (inlined) schema,
/// that describe `value`: itself and the branches of its unions and nullable wrappers
/// (`anyOf`, `oneOf`, `allOf`) that declare members, but one of another `__typename`
/// than `value` holds.
///
/// Every object type of a compiled SDL fixes its `__typename`, so a union's branches
/// tell themselves apart by it. A branch fixing none (a hand-written schema) is taken
/// as describing the value; and a value holding no `__typename` while branches of
/// several types remain cannot tell which it is, so none is taken: nothing is nulled
/// or filled in it, and a read lacking what it asks generates as before.
fn object_parts<'s>(schema: &'s Value, value: &Map<String, Value>) -> Vec<&'s Value> {
    let mut out = Vec::new();
    let mut open = vec![(schema, 0)];
    while let Some((part, depth)) = open.pop() {
        if part.get("properties").is_some_and(Value::is_object) {
            let other = part
                .pointer("/properties/__typename/const")
                .is_some_and(|t| value.get("__typename").is_some_and(|v| v != t));
            if !other {
                out.push(part);
            }
        }
        if depth < SELECTED_DEPTH {
            for key in ["anyOf", "oneOf", "allOf"] {
                for branch in part
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    open.push((branch, depth + 1));
                }
            }
        }
    }
    let types: Vec<&Value> = out
        .iter()
        .filter_map(|p| p.pointer("/properties/__typename/const"))
        .collect();
    if value.get("__typename").is_none() && types.iter().any(|t| Some(t) != types.first()) {
        return Vec::new();
    }
    out
}

/// The items schema of a list `schema` (narrowed, as [`object_parts`] reads it).
fn items_of(schema: &Value, depth: usize) -> Option<&Value> {
    if let Some(items) = schema.get("items") {
        return Some(items);
    }
    if depth >= SELECTED_DEPTH {
        return None;
    }
    ["anyOf", "oneOf", "allOf"]
        .iter()
        .filter_map(|k| schema.get(*k).and_then(Value::as_array))
        .flatten()
        .find_map(|branch| items_of(branch, depth + 1))
}

/// The GraphQL type `value` (an object of the answer, described by `parts`) is of: its
/// own `__typename`, else the one its schema fixes.
fn type_name<'v>(value: &'v Map<String, Value>, parts: &[&'v Value]) -> Option<&'v str> {
    value.get("__typename").and_then(Value::as_str).or_else(|| {
        parts
            .iter()
            .find_map(|p| p.pointer("/properties/__typename/const")?.as_str())
    })
}

/// Whether `value`, an object below the top of a GraphQL answer, is another record
/// (of a type with an identity: one the world keys, or one declaring an `id`), which a
/// completion of the record read leaves as it is.
fn embedded_record(rt: &crate::MockRuntime, value: &Map<String, Value>, parts: &[&Value]) -> bool {
    type_name(value, parts).is_some_and(|name| keyed_type(rt, name))
}

/// Whether GraphQL type `name` has an identity: the world keys it, or it declares an
/// `id`.
fn keyed_type(rt: &crate::MockRuntime, name: &str) -> bool {
    let keyed = rt
        .index()
        .types_by_def
        .get(name)
        .is_some_and(|t| !rt.index().identity_fields(t).is_empty());
    keyed
        || rt
            .bundle()
            .defs
            .get(name)
            .and_then(|def| def.get("properties"))
            .is_some_and(|p| p.get("id").is_some())
}

/// Whether `schema`, a member of a GraphQL selection's narrowed schema, answers another
/// record or holds one at any depth (a connection of notes): a held record lacking it
/// has its records elsewhere (held on their own, found by the read), never invented here.
fn holds_record(rt: &crate::MockRuntime, schema: &Value, depth: usize) -> bool {
    if depth > SELECTED_DEPTH {
        return false;
    }
    let Value::Object(obj) = schema else {
        return false;
    };
    if let Some(Value::Object(props)) = obj.get("properties") {
        let keyed = props
            .get("__typename")
            .and_then(|t| t.get("const"))
            .and_then(Value::as_str)
            .is_some_and(|name| keyed_type(rt, name));
        if keyed || props.values().any(|m| holds_record(rt, m, depth + 1)) {
            return true;
        }
    }
    obj.get("items")
        .is_some_and(|i| holds_record(rt, i, depth + 1))
        || ["anyOf", "oneOf", "allOf"]
            .iter()
            .filter_map(|k| obj.get(*k).and_then(Value::as_array))
            .flatten()
            .any(|b| holds_record(rt, b, depth + 1))
}

/// The members (but `__typename`) `parts` declare, each once.
fn selected_members<'s>(parts: &[&'s Value]) -> Vec<(&'s String, &'s Value)> {
    let mut out: Vec<(&String, &Value)> = Vec::new();
    for part in parts {
        let Some(Value::Object(props)) = part.get("properties") else {
            continue;
        };
        for (name, member) in props {
            if name != "__typename" && !out.iter().any(|(n, _)| *n == name) {
                out.push((name, member));
            }
        }
    }
    out
}

/// `body`, a held record rendered for a GraphQL selection (`schema`, narrowed to it),
/// with each member the selection asks for that it lacks set to `null` where its schema
/// allows that: nothing says what it holds. At any depth, in an object it holds or each
/// item of a list, but not in another record it embeds ([`embedded_record`]), not for a
/// member holding records ([`holds_record`]); the caller leaves an aliased selection
/// out ([`CallPlan::renames`]). With `within`, only below those top members (the ones the
/// description computes, whose expression states the whole value). See the module.
pub(super) fn null_selected(
    rt: &crate::MockRuntime,
    schema: &Value,
    body: &mut Value,
    within: Option<&[String]>,
) {
    match (within, body) {
        (None, body) => null_below(rt, schema, body, 0),
        (Some(within), Value::Object(obj)) => {
            let parts = object_parts(schema, obj);
            for (name, member) in selected_members(&parts) {
                if let Some(inner) = obj.get_mut(name)
                    && within.contains(name)
                {
                    null_below(rt, member, inner, 1);
                }
            }
        }
        _ => {}
    }
}

fn null_below(rt: &crate::MockRuntime, schema: &Value, value: &mut Value, depth: usize) {
    if depth > SELECTED_DEPTH {
        return;
    }
    match value {
        Value::Object(obj) => {
            let parts = object_parts(schema, obj);
            if depth > 0 && embedded_record(rt, obj, &parts) {
                return;
            }
            for (name, member) in selected_members(&parts) {
                match obj.get_mut(name) {
                    None => {
                        if !holds_record(rt, member, 0) && create::fits(rt, member, &Value::Null) {
                            obj.insert(name.clone(), Value::Null);
                        }
                    }
                    Some(inner @ (Value::Object(_) | Value::Array(_))) => {
                        null_below(rt, member, inner, depth + 1);
                    }
                    Some(_) => {}
                }
            }
        }
        Value::Array(items) => {
            if let Some(item) = items_of(schema, 0) {
                for value in items {
                    null_below(rt, item, value, depth + 1);
                }
            }
        }
        _ => {}
    }
}

/// What `schema`, the record schema of a GraphQL selection, selects that `record` lacks
/// and that may not be `null` (what it requires), as [`create::gaps`] answers it: each
/// top member (but those in `owned`) with the schema fill writes it by, an object or
/// list it holds with an object schema of what it lacks. Another record it embeds is
/// left as it is ([`embedded_record`]).
fn selected_gaps(
    rt: &crate::MockRuntime,
    schema: &Value,
    record: &Value,
    owned: &[String],
) -> Vec<(String, Value)> {
    let Some(Value::Object(properties)) = selected_lacking(rt, schema, record, 0)
        .as_ref()
        .and_then(|s| s.get("properties"))
        .cloned()
    else {
        return Vec::new();
    };
    properties
        .into_iter()
        .filter(|(name, _)| !owned.contains(name))
        .collect()
}

/// [`selected_gaps`] at one level: an object schema of what `value` lacks, an array
/// schema of as many items for a list (merged by position, [`create::fill_into`]),
/// `None` when it lacks nothing.
fn selected_lacking(
    rt: &crate::MockRuntime,
    schema: &Value,
    value: &Value,
    depth: usize,
) -> Option<Value> {
    if depth > SELECTED_DEPTH {
        return None;
    }
    match value {
        Value::Object(obj) => {
            let parts = object_parts(schema, obj);
            if depth > 0 && embedded_record(rt, obj, &parts) {
                return None;
            }
            let required: Vec<&str> = parts
                .iter()
                .filter_map(|p| p.get("required").and_then(Value::as_array))
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let mut properties = Map::new();
            // The type an object a GraphQL answer requires is fixed: one stored in
            // another mock's shape (REST's, which has none) takes it.
            if required.contains(&"__typename")
                && !obj.contains_key("__typename")
                && let Some(typename) = parts
                    .iter()
                    .find_map(|p| p.pointer("/properties/__typename"))
            {
                properties.insert("__typename".to_owned(), typename.clone());
            }
            for (name, member) in selected_members(&parts) {
                match obj.get(name) {
                    None if required.contains(&name.as_str()) && !holds_record(rt, member, 0) => {
                        properties.insert(name.clone(), member.clone());
                    }
                    Some(inner @ (Value::Object(_) | Value::Array(_))) => {
                        if let Some(below) = selected_lacking(rt, member, inner, depth + 1) {
                            properties.insert(name.clone(), below);
                        }
                    }
                    _ => {}
                }
            }
            let names: Vec<String> = properties.keys().cloned().collect();
            (!properties.is_empty())
                .then(|| json!({"type": "object", "properties": properties, "required": names}))
        }
        Value::Array(items) if !items.is_empty() => {
            let item = items_of(schema, 0)?;
            let mut properties = Map::new();
            for held in items {
                let Some(Value::Object(below)) = selected_lacking(rt, item, held, depth + 1)
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
                json!({
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

/// The members of [`create::gaps`]' object schema of what a held object lacks.
fn lacking_members(schema: &Value) -> Vec<(String, Value)> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// Keeps `completed`, `read` completed (see the module): a record the session holds
/// under the read's own type and key is stored completed when it is still as it was
/// read (a compare and set, so a write another call made since keeps what it wrote);
/// any other's completion is kept beside the record as read, unless it is the one kept
/// already. Whether anything was stored.
pub(super) async fn keep(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    read: &StoredResource,
    completed: &Completed,
) -> bool {
    if completed.reused {
        return false;
    }
    if !completed.own {
        let doc = StoredResource {
            resource_type: INTERNAL_TYPE.to_owned(),
            id: kept_key(&state.rt.name, read, completed.item),
            data: json!({"basis": read.data, "data": completed.record.data}),
        };
        return state
            .upsert(&plan.session, doc)
            .instrument(state_span())
            .await;
    }
    let Some(current) = state
        .get(&plan.session, &read.resource_type, &read.id)
        .instrument(state_span())
        .await
    else {
        return false;
    };
    if current.data != read.data {
        return false;
    }
    state
        .upsert(&plan.session, completed.record.clone())
        .instrument(state_span())
        .await
}
