//! Lists (step 4 of the plan): the session's records of the list's type, from the first
//! call, with no LLM call.
//!
//! The records are the store's records of the type in the collection the route names,
//! in the order the collection's [`state_lists`] mark placed them (each create's, once
//! the collection was listed), then those no create stored (seeded) least recently
//! written first, then the created in the order they were created
//! ([`state_lists::note_created`]), so a later write does not move a record:
//! - a route with path parameters holds a record its mark holds, one this runtime
//!   placed under it, or one whose member tied to each parameter holds its value
//!   ([`lists::in_collection`]), or whose described `parent.member` holds the value of
//!   the parent's `path_param`;
//! - a route with none holds every record of the type when it is the type's one list
//!   with no path parameter (`BundleIndex::unscoped_lists`); another (`/me/tasks`
//!   beside `/tasks`) shows a part of them only, and falls back (`unscoped_list`)
//!   unless the request gives the described scope.
//!
//! An empty session lists nothing. Each answer marks the collection listed
//! ([`state_lists::mark_listed`]), so it survives a rebuild of the runtime.
//!
//! The query then narrows them as [`list_query`] says: the described scope and filters,
//! the soft delete unless its `listed_with` parameter is sent, the name-matched filters
//! of [`lists::filters_of`] for parameters the description does not name. Then the page
//! is cut and rendered by [`state_lists::page`]: derived members filled, the described
//! order, the count, a page number or offset from state, a next page of a cursor,
//! next-URL or header cursor as a [`pages`] token (whose later pages [`pages::serve`]
//! answers here). A page cut with nowhere to say where the rest is is served whole
//! under a described paging (as a generated one is), and cut to the size a parameter
//! the description does not name asks for. Envelope members beside the list come from
//! what the schema fixes, else the faker. A list paged by its `Range` header is then
//! ordered and cut by it ([`ranges::serve`]: a `206` when more remain, whose
//! `Next-Range` the HTTP handler sets), and a first page the query leaves nothing of
//! answers the bodiless `204` the read declares beside its list
//! ([`lists::no_content`]).
//!
//! It falls back, and the call is generated as before, for:
//! - `undescribed_param`: a query parameter that is no described filter, scope or
//!   paging parameter, no name-matched filter and no page size, unless its value asks
//!   nothing (blank, its declared `default`, or the first page of a page position);
//! - `unknown_token`: a described cursor no kept cut of this session minted;
//! - `unscoped_list`: see above;
//! - `over_cap`: more than [`MAX_SCAN`] records of the type;
//! - `not_http`: another binding than an HTTP route, or a list that is no read;
//! - `render_failed`: a page that does not validate, or no envelope;
//! - `no_next_place`: a page number or offset with a next page, no place to say where
//!   it is, and a `next` member left `null`, which would say no page follows.

use std::collections::HashMap;

use mb_engine::{GeneratedResponse, GenerationContext, SchemaValidator};
use mb_mcm::world::OpKind;
use mb_mcm::{Binding, Operation, OperationKind, StatusSelector};
use serde_json::{Map, Value};
use tracing::Instrument;

use super::{Held, Ran, answered, fell_back};
use crate::MockRuntime;
use crate::call::state_lists::Shape;
use crate::call::{
    CallPlan, MAX_SCAN, OperationCall, StateAccess, cursors, id_string, item_identity, list_query,
    lists, pages, property_schema, ranges, runs, schema_parts, state_lists, state_span,
};

/// Whether the executor takes these calls.
pub(super) const EXECUTES: bool = true;

/// Whether `op` is a list whose items have an identity, which a list operation itself
/// has none of: the effective world describes one for its type.
pub(super) fn identified(rt: &MockRuntime, op: &Operation, kind: OpKind) -> bool {
    kind == OpKind::List
        && item_type(rt, op)
            .and_then(|t| rt.described().type_desc(t))
            .and_then(|d| d.identity.as_ref())
            .is_some_and(|i| i.member.is_some())
}

/// The type of `op`'s items: the described one, else the operation's.
pub(super) fn item_type<'a>(rt: &'a MockRuntime, op: &'a Operation) -> Option<&'a str> {
    rt.described()
        .op(&op.id)
        .and_then(|d| d.r#type.as_deref())
        .or(op.resource_type.as_deref())
}

/// Executes the call.
pub(super) async fn run(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    _held: Held<'_>,
    response_schema: Option<&Value>,
) -> Ran {
    let rt = state.rt;
    match answer(state, plan, call, response_schema)
        .instrument(state_span())
        .await
    {
        Ok(served) => {
            answered(rt, plan, OpKind::List);
            Ran::Answered(Box::new(served))
        }
        Err(reason) => {
            fell_back(rt, plan, OpKind::List, reason);
            Ran::Generate(None)
        }
    }
}

/// The list's answer from the session's records (see the module's doc), or why not.
async fn answer(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    response_schema: Option<&Value>,
) -> Result<GeneratedResponse, &'static str> {
    let rt = state.rt;
    let op = plan.op;
    let bundle = rt.bundle();
    let Binding::Http { path, .. } = &op.binding else {
        return Err("not_http");
    };
    if !matches!(op.kind, OperationKind::Query) || response_schema.is_some() {
        return Err("not_http");
    }
    // The next page of a list this session cut, by the token it minted.
    if let Some(served) = pages::serve(state, plan, call).await {
        cursors::after_answer(state, plan, &served).await;
        return Ok(served);
    }
    let t = item_type(rt, op).ok_or("render_failed")?;
    let q = list_query::of(rt, op, call);
    if q.as_ref()
        .and_then(|q| q.page.as_ref())
        .is_some_and(|p| !p.counts() && p.position.is_some())
    {
        return Err("unknown_token");
    }
    let shape = Shape::of(rt, op).ok_or("render_failed")?;
    let (item_parts, _) = schema_parts(bundle, Some(&shape.items));
    let size = list_query::requested_page(rt, op, call);
    let size_ref = size.as_ref().map(|(name, limit)| (name.as_str(), *limit));
    let knows = |param: &str| q.as_ref().is_some_and(|q| q.knows(param));
    let filters: Vec<_> = lists::filters_of(
        bundle,
        op,
        op.input.query.as_ref(),
        &item_parts,
        call,
        size_ref,
    )
    .into_iter()
    .filter(|(param, _)| !q.as_ref().is_some_and(|q| q.pages_with(param)))
    .collect();
    let paging = runs::Paging::of(bundle, op, size_ref.map(|(name, _)| name));
    let undescribed = call.query.iter().any(|(param, raw)| {
        !knows(param)
            && size_ref.is_none_or(|(name, _)| name != param)
            && !filters.iter().any(|(name, _)| name == param)
            && !asks_nothing(bundle, op, (param, raw), &paging)
    });
    if undescribed {
        return Err("undescribed_param");
    }
    let unscoped = call.path_params.is_empty()
        && !(rt.index().unscoped_lists.get(t) == Some(&1)
            && rt.index().unscoped_paths.get(path).map(String::as_str) == Some(t))
        && q.as_ref().is_none_or(|q| q.scope.is_none());
    if unscoped {
        return Err("unscoped_list");
    }
    let envelope = envelope(rt, plan, call, &shape).ok_or("render_failed")?;
    let mut records = state.list(&plan.session, t, MAX_SCAN + 1).await;
    if records.len() > MAX_SCAN {
        return Err("over_cap");
    }
    // As the collection's mark placed them; then those no create noted, least recently
    // written first; then the created, in the order they were created.
    let marked = state_lists::marked(state, (plan, call), t).await;
    let placed: HashMap<&str, usize> = marked
        .iter()
        .enumerate()
        .map(|(at, id)| (id.as_str(), at))
        .collect();
    let created = state_lists::created_order(state, &plan.session, t).await;
    let created: HashMap<&str, usize> = created
        .iter()
        .enumerate()
        .map(|(at, id)| (id.as_str(), at))
        .collect();
    records.reverse();
    records.sort_by_key(|r| match placed.get(r.id.as_str()) {
        Some(at) => (0, *at),
        None => created.get(r.id.as_str()).map_or((1, 0), |at| (2, *at)),
    });
    let field = item_identity(bundle, op, &envelope);
    let mut items = Vec::new();
    for r in &records {
        if !(placed.contains_key(r.id.as_str())
            || lists::in_collection(rt, (plan, call), t, &field, r)
            || under_parent(rt, call, t, r))
        {
            continue;
        }
        let item = lists::projected(bundle, Some(&shape.items), &r.data, 0);
        // The described keys, else the name-matched filters; and those the description
        // does not name besides.
        let kept = list_query::keeps(q.as_ref(), &item, |i| {
            filters.iter().all(|(_, f)| lists::holds_wanted(i, f))
        }) && filters
            .iter()
            .all(|(param, f)| knows(param) || lists::holds_wanted(&item, f));
        if kept {
            items.push(item);
        }
    }
    let ids: Vec<String> = items
        .iter()
        .filter_map(|item| mb_mcm::member_at(item, &field).and_then(id_string))
        .collect();
    let none_kept = items.is_empty();
    let mut served = state_lists::page(
        state,
        (plan, call),
        (t, q.as_ref()),
        (&shape, &envelope),
        items,
        true,
    )
    .await
    // Served anyway, a page is refused only when it does not render, or says no page
    // follows one that does.
    .map_err(|unserved| match unserved {
        state_lists::Unserved::Render => "render_failed",
        state_lists::Unserved::NoPlace => "no_next_place",
    })?;
    // The collection is marked listed: its creates are placed in it from now on.
    state_lists::mark_listed(state, (plan, call), t, &served, ids).await;
    // A list paged by its `Range` header: ordered and cut by it.
    ranges::serve(bundle, (plan, call), &mut served);
    // A list the query leaves nothing of answers the bodiless status declared for that.
    if none_kept {
        lists::no_content(bundle, op, &mut served);
    }
    cursors::after_answer(state, plan, &served).await;
    Ok(served)
}

/// Whether stored `r`, of type `t`, names the parent the call's path names: its type's
/// described `parent.member` holds the value of the parent's `path_param`.
fn under_parent(
    rt: &MockRuntime,
    call: &OperationCall,
    t: &str,
    r: &mb_state::StoredResource,
) -> bool {
    let Some(parent) = rt.described().type_desc(t).and_then(|d| d.parent.as_ref()) else {
        return false;
    };
    let (Some(member), Some(param)) = (&parent.member, &parent.path_param) else {
        return false;
    };
    let Some(wanted) = call.path_params.get(param).and_then(id_string) else {
        return false;
    };
    mb_mcm::member_at(&r.data, member)
        .and_then(id_string)
        .is_some_and(|held| held == wanted)
}

/// Whether query value `raw` of `param` asks nothing of the list: blank, the parameter's
/// declared `default`, or the first page of a page position (its declared `minimum`,
/// else `1` for a page number and `0` for an offset).
fn asks_nothing(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    (param, raw): (&str, &Value),
    paging: &runs::Paging,
) -> bool {
    // Text, numbers and booleans compare as their text.
    let text_of = |v: &Value| match v {
        Value::Bool(b) => Some(b.to_string()),
        other => id_string(other).map(|t| t.trim().to_string()),
    };
    let Some(text) = text_of(raw) else {
        return false;
    };
    if text.is_empty() {
        return true;
    }
    let declared = op
        .input
        .query
        .as_ref()
        .and_then(|q| q.get("properties"))
        .and_then(|props| props.get(param));
    let (parts, _) = schema_parts(bundle, declared);
    let at = |keyword: &str| parts.iter().find_map(|p| p.get(keyword));
    if at("default").and_then(text_of).as_deref() == Some(text.as_str()) {
        return true;
    }
    if !paging.pages_list(param) {
        return false;
    }
    let Ok(n) = text.parse::<u64>() else {
        return false;
    };
    match at("minimum").and_then(Value::as_u64) {
        Some(minimum) => n == minimum,
        None => n == u64::from(param.to_ascii_lowercase().contains("page")),
    }
}

/// The envelope of an empty page of `shape`: its other required members as the schema
/// fixes them, else as the faker draws them for the call, a flag saying more pages
/// follow (`has_more`) at `false` until the page says otherwise.
fn envelope(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    shape: &Shape,
) -> Option<Value> {
    if let Some(envelope) = shape.envelope(rt, plan, None) {
        return Some(envelope);
    }
    let variant = plan.op.success_output()?;
    let status = match variant.status {
        StatusSelector::Code(code) => code,
        StatusSelector::Default => 200,
    };
    let faked = plan.engine.fallback(
        rt.bundle(),
        plan.op,
        variant,
        status,
        call,
        &GenerationContext::default(),
    );
    let mut kept: Map<String, Value> = faked.body?.as_object()?.clone();
    for (name, value) in &mut kept {
        if value.is_boolean() && name.to_ascii_lowercase().contains("more") {
            *value = Value::Bool(false);
        } else if admits_null(rt, shape.schema(), name) {
            *value = Value::Null;
        }
    }
    shape.envelope(rt, plan, Some(&kept))
}

/// Whether member `name` of answer `schema` may be `null` (a `previous` link of a
/// first page): a page from state says nothing there rather than what the faker draws.
fn admits_null(rt: &MockRuntime, schema: &Value, name: &str) -> bool {
    let bundle = rt.bundle();
    property_schema(bundle, schema, name)
        .and_then(|member| SchemaValidator::for_output(&bundle.with_reachable_defs(&member)).ok())
        .is_some_and(|validator| validator.is_valid(&Value::Null))
}
