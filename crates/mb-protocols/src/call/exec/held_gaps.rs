//! A held record that lacks members its read requires, completed from state rather than
//! generated again.
//!
//! A record is stored by whatever wrote it, in that writer's shape: a GraphQL create in
//! a `shares` group stores a pull request without REST's `assignee`, `_links` or `head`.
//! A read over REST that finds it does not render, and generating the read would
//! replace every stored value with the model's (the head branch the client pushed
//! included). So a read of the executor (not a GraphQL selection, see
//! [`super::selected_held`]) completes the record instead, at the top and below it
//! (an object or a list's items it holds, required or not), against the record schema
//! of the read's answer ([`super::create::gaps`], what that schema requires only):
//!
//! - a member that may be `null` is `null`: nothing says what it holds;
//! - every other one is written as a create's fill writes it ([`mb_engine::Engine::fill`]:
//!   the faker's ids, numbers, moments and objects, defaults and enums, what the caller's
//!   own answers say of an object of its schema), the model asked only for free text,
//!   with a schema of those members alone; what fill leaves is settled as a create's is;
//! - the URL members it wrote are made to show the record's place ([`links::settle`]).
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
    let mut found = found
        .into_iter()
        .flat_map(|(at, keyword)| redraw::narrowed(rt, schema, &answer, at, keyword, 0))
        .peekable();
    found.peek().is_some() && found.all(|(_, keyword)| keyword == "required")
}

/// `stored`, the record a read of `plan` holds, with what the record schema of the
/// read's answer requires that it lacks (see the module). `None` for a GraphQL read, a
/// record that is no object, an answer with no record schema, a record lacking
/// nothing that schema requires, or one that does not render for another reason
/// ([`only_lacking`]).
pub(super) async fn completed(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    stored: &StoredResource,
    response_schema: Option<&Value>,
) -> Option<Completed> {
    let rt = state.rt;
    let op = plan.op;
    if op.protocol == Protocol::Graphql
        || !stored.data.is_object()
        || !only_lacking(rt, op, stored, response_schema)
    {
        return None;
    }
    let bundle = rt.bundle();
    let answer = response_schema.or_else(|| op.success_output()?.schema.as_ref())?;
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
    let bundle = rt.bundle();
    let schemas = [schema];
    let t = stored.resource_type.as_str();
    // The members the description derives are answered from state, never stored: the
    // top member holding one is left as it is, as a create's fill leaves it.
    let derived: Vec<String> = rt
        .described()
        .type_desc(t)
        .map(|d| {
            d.derived
                .keys()
                .chain(d.computed.keys())
                .map(|m| m.split('.').next().unwrap_or(m).to_owned())
                .collect()
        })
        .unwrap_or_default();
    let mut data = stored.data.clone();
    if create::gaps(rt, (t, &schemas), &data, &derived, Unsent::Required).is_empty() {
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
        // The record as the completion found it: that completion again.
        if earlier.basis == stored.data {
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
        // The writer changed it: what it holds wins, the earlier completion fills what
        // it still lacks.
        if let Value::Object(before) = earlier.data {
            create::fill_into(known, before);
        }
    }
    let lacking = create::gaps(
        rt,
        (t, &schemas),
        &Value::Object(known.clone()),
        &derived,
        Unsent::Required,
    );
    // What may be `null` is: nothing says what it holds.
    for (name, member) in &lacking {
        nulled(rt, name, member, known, create::GAP_DEPTH);
    }
    let missing = create::gaps(
        rt,
        (t, &schemas),
        &Value::Object(known.clone()),
        &derived,
        Unsent::Required,
    );
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
/// allows that; below a member `held` holds (`member` then [`create::gaps`]' schema of
/// what it lacks), the same for each member it lacks, in an object or in each item of
/// a list, to `depth` levels.
fn nulled(
    rt: &crate::MockRuntime,
    name: &str,
    member: &Value,
    held: &mut Map<String, Value>,
    depth: usize,
) {
    if depth == 0 {
        return;
    }
    match held.get_mut(name) {
        None => {
            if create::fits(rt, member, &Value::Null) {
                held.insert(name.to_owned(), Value::Null);
            }
        }
        Some(Value::Object(inner)) => {
            for (below, schema) in lacking_members(member) {
                nulled(rt, &below, &schema, inner, depth - 1);
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
                        nulled(rt, name, schema, inner, depth - 1);
                    }
                }
            }
        }
        Some(_) => {}
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
