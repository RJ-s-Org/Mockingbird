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
//! A session that never wrote a record of the type (holds none, created none, deleted
//! none) falls back (`never_written`): the type may be a catalog the model makes up,
//! such as a search's offers. One whose records were all deleted lists nothing. A
//! route with path parameters whose collection holds none of the type's held records
//! falls back too (`scope_unresolved`) unless it was listed, the type's described
//! `parent` link names it, a create of the type is on its path, or a held record holds
//! a member named for each of its parameters: the records may sit under another route,
//! as a project created under its team and listed under its organization. Each answer
//! marks the collection listed
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
//!   nothing (blank, its declared `default`, or the first page of a page position); or
//!   any member of a request body (a list asked by a POST, which reads none of it, a
//!   filter or a cursor alike), unless it is `null`, blank or empty;
//! - `never_written`, `scope_unresolved`: see above;
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

use super::{Held, Ran, Stop, answered, fell_back_on};
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
        Err(stop) => {
            fell_back_on(rt, plan, OpKind::List, &stop);
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
) -> Result<GeneratedResponse, Stop<'static>> {
    let rt = state.rt;
    let op = plan.op;
    let bundle = rt.bundle();
    let Binding::Http { path, .. } = &op.binding else {
        return Err("not_http".into());
    };
    if !matches!(op.kind, OperationKind::Query) || response_schema.is_some() {
        return Err("not_http".into());
    }
    // The next page of a list this session cut, by the token it minted.
    if let Some(served) = pages::serve(state, plan, call).await {
        cursors::after_answer(state, plan, &served).await;
        return Ok(served);
    }
    let t = item_type(rt, op).ok_or(Stop::from("render_failed"))?;
    let q = list_query::of(rt, op, call);
    if q.as_ref()
        .and_then(|q| q.page.as_ref())
        .is_some_and(|p| !p.counts() && p.position.is_some())
    {
        return Err("unknown_token".into());
    }
    let shape = Shape::of(rt, op).ok_or(Stop::from("render_failed"))?;
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
    let undescribed = call.query.iter().find(|(param, raw)| {
        !knows(param)
            && size_ref.is_none_or(|(name, _)| name != param.as_str())
            && !filters.iter().any(|(name, _)| name == *param)
            && !asks_nothing(bundle, op, (param, raw), &paging)
    });
    // A list asked by a body (a POST query) reads no member of it, a filter or a
    // cursor alike: one that asks something is undescribed.
    let body_asks = call
        .body
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|members| members.iter().find(|(_, value)| asks(value)));
    if let Some((on, _)) = undescribed.or(body_asks) {
        return Err(Stop::on("undescribed_param", on));
    }
    let unscoped = call.path_params.is_empty()
        && !only_unscoped(rt, path, t)
        && q.as_ref().is_none_or(|q| q.scope.is_none());
    if unscoped {
        return Err("unscoped_list".into());
    }
    let envelope = envelope(rt, plan, call, &shape).ok_or(Stop::from("render_failed"))?;
    let mut records = state.list(&plan.session, t, MAX_SCAN + 1).await;
    if records.len() > MAX_SCAN {
        return Err("over_cap".into());
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
    // A type the session never wrote a record of, a catalog the model makes up: the
    // store cannot say it is empty.
    if records.is_empty()
        && created.is_empty()
        && !state_lists::deleted_some(state, &plan.session, t).await
    {
        return Err("never_written".into());
    }
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
    let mut in_scope = false;
    for r in &records {
        if !(placed.contains_key(r.id.as_str())
            || lists::in_collection(rt, (plan, call), t, &field, r)
            || under_parent(rt, call, t, r))
        {
            continue;
        }
        in_scope = true;
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
    // Records of the type are held, none in this collection, which was never listed and
    // which no described parent link names: they may sit under another route (a project
    // created under its team, listed under its organization), so an empty page would be
    // a guess.
    if !records.is_empty()
        && !call.path_params.is_empty()
        && !in_scope
        && !parent_named(rt, call, t)
        && !route_ties(rt, op, call, t, &records)
        && !state_lists::listed(state, (plan, call), t).await
    {
        return Err("scope_unresolved".into());
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
        state_lists::Unserved::Render => Stop::from("render_failed"),
        state_lists::Unserved::NoPlace => Stop::from("no_next_place"),
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

/// Whether the list at `path`, of items of type `t`, is the type's one list with no path
/// parameter: the only one the description gives type `t`, and, where the contract's
/// schemas name its items' type (`BundleIndex::unscoped_lists`), the only one of that
/// type too. A list answering a lean schema of the record (SendGrid's
/// `TemplateLean` under `result`) is one the schemas may not name.
fn only_unscoped(rt: &MockRuntime, path: &str, t: &str) -> bool {
    let index = rt.index();
    let alone = index
        .unscoped_paths
        .get(path)
        .is_none_or(|indexed| index.unscoped_lists.get(indexed) == Some(&1));
    let described = rt
        .bundle()
        .operations
        .iter()
        .filter(|op| {
            let kind = rt
                .described()
                .op(&op.id)
                .and_then(|d| d.kind)
                .or_else(|| OpKind::of(op));
            kind == Some(OpKind::List)
                && matches!(&op.binding, Binding::Http { path, .. } if !path.contains('{'))
                && item_type(rt, op) == Some(t)
        })
        .count();
    alone && described == 1
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

/// Whether body member value `value` asks something of the list: not `null`, blank,
/// or an empty object or array.
fn asks(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(members) => !members.is_empty(),
        _ => true,
    }
}

/// Whether the call's path names the parent of type `t` by its described link: the
/// type's `parent.member` and `parent.path_param`, a parameter the path holds.
fn parent_named(rt: &MockRuntime, call: &OperationCall, t: &str) -> bool {
    rt.described()
        .type_desc(t)
        .and_then(|d| d.parent.as_ref())
        .is_some_and(|parent| {
            parent.member.is_some()
                && parent
                    .path_param
                    .as_ref()
                    .is_some_and(|param| call.path_params.contains_key(param))
        })
}

/// Whether the list's route places the type's records itself: a create of type `t` on
/// the same path, or a held record holding a member named for each of its path
/// parameters. Then a collection of it that holds none of them is empty.
fn route_ties(
    rt: &MockRuntime,
    op: &Operation,
    call: &OperationCall,
    t: &str,
    records: &[mb_state::StoredResource],
) -> bool {
    let Binding::Http { path, .. } = &op.binding else {
        return false;
    };
    let created_here = rt.bundle().operations.iter().any(|other| {
        other.kind == OperationKind::Command(mb_mcm::Effect::Create)
            && matches!(&other.binding, Binding::Http { path: p, .. } if p == path)
            && item_type(rt, other) == Some(t)
    });
    created_here
        || records.iter().any(|r| {
            call.path_params
                .keys()
                .all(|param| mb_mcm::member_at(&r.data, param).is_some())
        })
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
