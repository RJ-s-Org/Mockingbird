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
//!   unless the request gives the described scope. A search, whose query requires a
//!   parameter (`/search/tickets?query=`), is no second list beside the type's written
//!   collection (`/tickets` with its create); beside another list it counts.
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
//! A described `list.where` keeps only the records it is `true` for, each as held,
//! before anything else narrows them ([`list_exprs`]). The query then narrows them as
//! [`list_query`] says: the described scope and filters
//! (a `grammar` filter's expression among them; every key bracketed below a
//! `deep_object` one is its own),
//! the soft delete unless its `listed_with` parameter is sent, the name-matched filters
//! of [`lists::filters_of`] for parameters the description does not name (a described
//! `fields` parameter is no filter: it narrows each item served, [`super::fields`]). Then the page
//! is cut and rendered by [`state_lists::page`]: derived members filled, the order a
//! sort parameter's `list.sort` gives ([`list_exprs`]), else the described order, the
//! count, a page number or offset from state, a next page of a cursor,
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
//! - `undescribed_param`: a query parameter that is no described filter, scope,
//!   paging or sort parameter, no name-matched filter, no page size and none the
//!   description `ignores`, unless its value asks nothing (blank, its declared `default`, or the
//!   first page of a page position); or a member of a request body (a list asked by a
//!   POST, a SOAP or gRPC request) that no described parameter names or holds, looked
//!   into where one names a member below it ([`list_query::body_pointer`]), unless it
//!   is `null`, blank, empty or its declared `default`;
//! - `unparsed_filter`: a described `grammar` filter's value that does not parse
//!   ([`list_query`]), which the executor never guesses at;
//! - `never_written`, `scope_unresolved`: see above;
//! - `unknown_token`: a described cursor no kept cut of this session minted;
//! - `unscoped_list`: see above;
//! - `never_listed`: a scope with no member, by a value the items' parent was issued
//!   ([`state_lists::issued_scope`]), whose collection no list marked yet: the first
//!   list of it generates, and marks it;
//! - `over_cap`: more than [`MAX_SCAN`] records of the type;
//! - `not_http`: a GraphQL field (whose selection narrows its answer) or a stream, or a
//!   list that is no read. A SOAP or gRPC list reads its request body as a `POST` list
//!   does, and with no path it holds every record of its type when it is the type's
//!   only list;
//! - `render_failed`: a page that does not validate, or no envelope;
//! - `no_next_place`: a page number or offset with a next page, no place to say where
//!   it is, and a `next` member left `null`, which would say no page follows.

use std::collections::HashMap;

use mb_engine::consistency::list_items_mut;
use mb_engine::{GeneratedResponse, GenerationContext, SchemaValidator};
use mb_mcm::world::OpKind;
use mb_mcm::{Binding, Operation, OperationKind, StatusSelector};
use serde_json::{Map, Value};
use tracing::Instrument;

use super::{Held, Ran, Stop, answered, fell_back_on};
use crate::MockRuntime;
use crate::call::state_lists::Shape;
use crate::call::{
    CallPlan, MAX_SCAN, OperationCall, StateAccess, bundle_index, cursors, id_string,
    item_identity, list_exprs, list_query, lists, pages, property_schema, ranges, records, runs,
    schema_parts, state_lists, state_span,
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
        Ok(mut served) => {
            // A described fields parameter narrows each item.
            let items = served.body.as_mut().and_then(list_items_mut);
            super::fields::narrow_items(rt, plan.op, call, items);
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
    // A SOAP or gRPC call decodes its request into a body as an HTTP one does; a
    // GraphQL field's selection narrows its answer, which a page from state does not.
    let path = match &op.binding {
        Binding::Http { path, .. } => Some(path.as_str()),
        Binding::Soap { .. } | Binding::Grpc { .. } => None,
        _ => return Err("not_http".into()),
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
    let fields_param = super::fields::param(rt, op);
    let sort_param = list_exprs::sort_param(rt, op);
    let sorts_with = |param: &str| sort_param.is_some_and(|s| list_exprs::is_sort_param(s, param));
    let knows = |param: &str| {
        q.as_ref().is_some_and(|q| q.knows(param))
            || fields_param == Some(param)
            || sorts_with(param)
    };
    let filters: Vec<_> = lists::filters_of(
        bundle,
        op,
        op.input.query.as_ref(),
        &item_parts,
        call,
        size_ref,
    )
    .into_iter()
    .filter(|(param, _)| !q.as_ref().is_some_and(|q| q.pages_with(param)) && !sorts_with(param))
    .collect();
    let paging = runs::Paging::of(bundle, op, size_ref.map(|(name, _)| name));
    let undescribed = call.query.iter().find(|(param, raw)| {
        !knows(param)
            && size_ref.is_none_or(|(name, _)| name != param.as_str())
            && !filters.iter().any(|(name, _)| name == *param)
            && !asks_nothing(bundle, op, (param, raw), &paging)
    });
    // A list asked by a body (a POST query, a SOAP request) reads the members the
    // description names; another that asks something is undescribed.
    let body_asks = call.body.as_ref().and_then(|body| {
        // The sort parameter, sent as a body member, is read.
        match (sort_param, body.as_object()) {
            (Some(param), Some(members)) if members.contains_key(param) => {
                let mut body = body.clone();
                if let Some(members) = body.as_object_mut() {
                    members.remove(param);
                }
                unread(bundle, op, q.as_ref(), &body, "")
            }
            _ => unread(bundle, op, q.as_ref(), body, ""),
        }
    });
    if let Some((on, _)) = undescribed {
        return Err(Stop::on("undescribed_param", on));
    }
    if let Some(on) = body_asks {
        return Err(Stop::on("undescribed_param", &on));
    }
    // A described filter expression that does not parse: never a guess.
    if let Some(on) = q.as_ref().and_then(|q| q.unparsed.as_deref()) {
        return Err(Stop::on("unparsed_filter", on));
    }
    // A scope by a value the items' parent was issued: the parent's collection, which
    // a first list of it (generated) marks; until then the store cannot tell it.
    let issued = state_lists::issued_scope(rt, op, call).is_some();
    if issued && !state_lists::listed(state, (plan, call), t).await {
        return Err("never_listed".into());
    }
    let unscoped = call.path_params.is_empty()
        && !issued
        && !only_unscoped(rt, op, path, t)
        && q.as_ref().is_none_or(|q| q.scope.is_none());
    // A type the session never wrote: the mock's record pool, when it holds it. Its
    // records then answer every unscoped list of the type in the session.
    let scoped = q.as_ref().is_some_and(|q| q.scope.is_some());
    let pooled = super::pool::list(state, plan, call, t, scoped).await;
    if unscoped && !pooled {
        return Err("unscoped_list".into());
    }
    let envelope = envelope(rt, plan, call, &shape).ok_or(Stop::from("render_failed"))?;
    let envelope = with_held_parent(state, (plan, call), t, &shape, envelope).await;
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
    // The record as held, beside its item, only for computed members to read.
    let computes = crate::call::computed::describes(rt, t);
    // The described default filter, over each record as held.
    let mut listed_where = list_exprs::Where::of(state, rt, (plan, call)).await;
    let mut in_scope = false;
    // What completing records cost, and how many were.
    let mut filled = mb_llm::Usage::default();
    let mut completions = 0;
    // No model to answer: none configured, or the provider marked unusable for now.
    let local = !plan.engine.has_provider() || plan.engine.provider_blocked().is_some();
    for r in &records {
        let placed_here = placed.contains_key(r.id.as_str());
        // Another parent's records are in no collection of this one.
        if !(placed_here
            || !issued
                && (lists::in_collection(rt, (plan, call), t, &field, r)
                    || under_parent(rt, call, t, r)))
        {
            continue;
        }
        in_scope = true;
        if let Some(w) = listed_where.as_mut()
            && !w.keeps(state, plan, &r.data).await
        {
            continue;
        }
        let item = lists::projected(bundle, Some(&shape.items), &r.data, 0);
        // The described keys, else the name-matched filters; and those the description
        // does not name besides.
        let keeps = |item: &Value| {
            list_query::keeps(q.as_ref(), item, |i| {
                filters.iter().all(|(_, f)| lists::holds_wanted(i, f))
            }) && filters
                .iter()
                .all(|(param, f)| knows(param) || lists::holds_wanted(item, f))
        };
        if !keeps(&item) {
            continue;
        }
        // With no model, a kept record lacking what an item requires is completed, as a
        // read of it is ([`super::held_gaps`]), rather than the page drawn whole by the
        // faker; at most [`MAX_COMPLETED`] in one list, past which the page is drawn as
        // before. A model generates the page in one call instead of a fill per record.
        let done = match local && completions < MAX_COMPLETED {
            true => super::held_gaps::completed_item(state, (plan, call), r, &shape.items).await,
            false => None,
        };
        let (r, item) = match &done {
            Some(done) => {
                completions += 1;
                super::held_gaps::keep(state, plan, r, done).await;
                filled = filled + done.usage;
                let item = lists::projected(bundle, Some(&shape.items), &done.record.data, 0);
                if !keeps(&item) {
                    continue;
                }
                (&done.record, item)
            }
            None => (r, item),
        };
        {
            let held = if computes {
                r.data.clone()
            } else {
                Value::Null
            };
            items.push((item, held));
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
        .filter_map(|(item, _)| mb_mcm::member_at(item, &field).and_then(id_string))
        .collect();
    let none_kept = items.is_empty();
    let sort = list_exprs::sort_keys(state, (plan, call), &shape.items, &items).await;
    let mut served = state_lists::page(
        state,
        (plan, call),
        (t, q.as_ref()),
        (&shape, &envelope),
        items,
        (sort.as_deref(), true),
    )
    .await
    // Served anyway, a page is refused only when it does not render, or says no page
    // follows one that does.
    .map_err(|unserved| match unserved {
        state_lists::Unserved::Render => Stop::from("render_failed"),
        state_lists::Unserved::NoPlace => Stop::from("no_next_place"),
    })?;
    served.usage = served.usage + filled;
    // The collection is marked listed: its creates are placed in it from now on.
    state_lists::mark_listed(state, (plan, call), t, &served, ids).await;
    // A list paged by its `Range` header: ordered and cut by it.
    ranges::serve(bundle, (plan, call), &mut served);
    // What the envelope says of a listed record names one the page lists, and what it
    // says of another type's record names one it lists beside, or the session listed.
    if let Some(body) = served.body.as_mut() {
        with_listed_references(rt, &shape, t, body);
        let drawn = with_held_beside(state, (plan, call), (t, &shape), body, false).await;
        with_beside_references(state, (plan, call), (t, &drawn), body, false).await;
    }
    // A list the query leaves nothing of answers the bodiless status declared for that.
    if none_kept {
        lists::no_content(bundle, op, &mut served);
    }
    cursors::after_answer(state, plan, &served).await;
    Ok(served)
}

/// Whether list `op` at `path` (none for a SOAP or gRPC operation), of items of type
/// `t`, is the type's one list with no path parameter: where the contract's schemas
/// name its items' type (`BundleIndex::unscoped_lists`), the only such list of that
/// type, which is `t`; where they name none (a lean schema of the record, SendGrid's
/// `TemplateLean` under `result`), and for an operation with no path, the only one the
/// description gives type `t`. A search (one whose query requires a parameter,
/// [`bundle_index::searches`]) beside the type's written collection is no second list
/// of the type (`BundleIndex::sole_unscoped`).
/// The most records one list completes ([`super::held_gaps::completed_item`]): each may
/// take a fill, so a collection of many records held in part is generated instead.
const MAX_COMPLETED: usize = 50;

fn only_unscoped(rt: &MockRuntime, op: &Operation, path: Option<&str>, t: &str) -> bool {
    let index = rt.index();
    let search = bundle_index::searches(op);
    let written = path.is_some_and(|path| index.written_lists.contains_key(path));
    if let Some(path) = path.filter(|path| index.unscoped_paths.contains_key(*path)) {
        return index.sole_unscoped(path, t);
    }
    let described = rt
        .bundle()
        .operations
        .iter()
        .filter(|other| search || !written || !bundle_index::searches(other))
        .filter(|op| {
            let kind = rt
                .described()
                .op(&op.id)
                .and_then(|d| d.kind)
                .or_else(|| OpKind::of(op));
            let unscoped = match &op.binding {
                Binding::Http { path, .. } => !path.contains('{'),
                Binding::Soap { .. } | Binding::Grpc { .. } => true,
                _ => false,
            };
            kind == Some(OpKind::List) && unscoped && item_type(rt, op) == Some(t)
        })
        .count();
    described == 1
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

/// The first member of request `body` (an object, at body pointer `at`) that asks
/// something of the list and that the description does not read, by its pointer
/// without the leading `/` (`filter`, `ActiveList/Include`): an object member the
/// description reads some member of is looked into. A member at its declared `default`
/// asks nothing.
fn unread(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    q: Option<&list_query::ListQuery>,
    body: &Value,
    at: &str,
) -> Option<String> {
    let members = body.as_object()?;
    members.iter().find_map(|(name, value)| {
        let here = format!("{at}/{}", name.replace('~', "~0").replace('/', "~1"));
        if !list_query::asks(value) || q.is_some_and(|q| q.reads_body(&here)) {
            return None;
        }
        if value.is_object() && q.is_some_and(|q| q.reads_below(&here)) {
            return unread(bundle, op, q, value, &here);
        }
        let declared = list_query::declared(bundle, op, &here);
        let (parts, _) = schema_parts(bundle, declared.as_ref());
        if parts.iter().any(|p| p.get("default") == Some(value)) {
            return None;
        }
        Some(here.trim_start_matches('/').to_owned())
    })
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

/// `envelope` with its one top member beside the list that is a record of the type of
/// the items' described parent (Plaid's `item` beside an Item's `accounts`) filled from
/// that parent as held, when the call names it ([`held_parent`]). A member is of the
/// parent's type as an answer's nested record is ([`bundle_index::entity_type`]): by
/// its definition, else by its name; of several such, the one named for the type is
/// it, and with none or more than one so named, nothing is filled. Only a member the
/// envelope holds is filled: the held record's members that its schema declares go over
/// what the envelope was filled with, which keeps the rest, and the member is changed
/// only when it then suits its schema.
async fn with_held_parent(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: &str,
    shape: &Shape,
    mut envelope: Value,
) -> Value {
    let rt = state.rt;
    let (Some(list), Some(parent_type)) = (
        shape.top_member(),
        rt.described()
            .type_desc(t)
            .and_then(|d| d.parent.as_ref())
            .map(|link| link.r#type.clone()),
    ) else {
        return envelope;
    };
    let Some((name, member_schema)) = parent_member(rt, shape, list, &parent_type, &envelope)
    else {
        return envelope;
    };
    let Some(parent) = held_parent(state, (plan, call), t).await else {
        return envelope;
    };
    let bundle = rt.bundle();
    let Value::Object(held) = lists::projected(bundle, Some(&member_schema), &parent.data, 0)
    else {
        return envelope;
    };
    let Some(members) = envelope.as_object_mut() else {
        return envelope;
    };
    let Some(mut filled) = members.get(&name).and_then(Value::as_object).cloned() else {
        return envelope;
    };
    filled.extend(held);
    let filled = Value::Object(filled);
    if plan
        .engine
        .schema_violations(bundle, &member_schema, &filled)
        .is_some_and(|violations| violations.is_empty())
    {
        members.insert(name, filled);
    }
    envelope
}

/// The one top member of `envelope` beside `list` that is a record of type
/// `parent_type` (see [`with_held_parent`]), with its schema: of those its definition
/// or its name says are of the type, the one its name says is, else the only one.
fn parent_member(
    rt: &MockRuntime,
    shape: &Shape,
    list: &str,
    parent_type: &str,
    envelope: &Value,
) -> Option<(String, Value)> {
    let bundle = rt.bundle();
    let index = rt.index();
    let held = envelope.as_object()?;
    let (parts, _) = schema_parts(bundle, Some(shape.schema()));
    let mut by_def = Vec::new();
    let mut by_name = Vec::new();
    let names = parts
        .iter()
        .filter_map(|p| p.get("properties").and_then(Value::as_object))
        .flat_map(|p| p.keys())
        .filter(|name| name.as_str() != list && held.contains_key(name.as_str()));
    for name in names {
        let Some(member_schema) = property_schema(bundle, shape.schema(), name) else {
            continue;
        };
        let named = bundle_index::entity_type(index, None, name).as_deref() == Some(parent_type);
        let (_, defs) = schema_parts(bundle, Some(&member_schema));
        let defined = defs
            .iter()
            .find_map(|d| index.types_by_def.get(*d))
            .is_some_and(|d| d == parent_type);
        let found = (name.clone(), member_schema.clone());
        if named {
            by_name.push(found);
        } else if defined {
            by_def.push(found);
        }
    }
    let mut chosen = if by_name.is_empty() { by_def } else { by_name };
    match chosen.len() {
        1 => chosen.pop(),
        _ => None,
    }
}

/// The held parent of the items of type `t` the call names: by a value it was issued
/// ([`state_lists::issued_scope`], Plaid's `access_token`), else by the path parameter
/// of the type's described `parent` link.
async fn held_parent(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: &str,
) -> Option<mb_state::StoredResource> {
    let rt = state.rt;
    if let Some((parent, _, value)) = state_lists::issued_scope(rt, plan.op, call) {
        return records::by_extra_fields(state, &plan.session, &parent, (&[], &value), None).await;
    }
    let link = rt.described().type_desc(t)?.parent.as_ref()?;
    let id = call
        .path_params
        .get(link.path_param.as_deref()?)
        .and_then(id_string)?;
    state.get(&plan.session, &link.r#type, &id).await
}

/// A generated answer `body` to list `call`, before it is served and recorded, made to
/// agree with the session as a page from state does: its parent-shaped envelope member
/// is the held parent ([`with_held_parent`]), what its envelope says of a listed
/// record names one it lists ([`with_listed_references`]), its arrays of another type's
/// records hold those the session listed under the same parent ([`with_held_beside`]),
/// and its references to that type name one of them ([`with_beside_references`]). Only
/// for a list whose items have a described identity ([`identified`]). Whether it
/// changed `body`.
pub(in crate::call) async fn agree_generated(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    body: &mut Value,
) -> bool {
    let rt = state.rt;
    let op = plan.op;
    let kind = rt
        .described()
        .op(&op.id)
        .and_then(|d| d.kind)
        .or_else(|| OpKind::of(op));
    if kind != Some(OpKind::List) || !identified(rt, op, OpKind::List) {
        return false;
    }
    let (Some(t), Some(shape)) = (item_type(rt, op), Shape::of(rt, op)) else {
        return false;
    };
    let before = body.clone();
    let answer = std::mem::take(body);
    *body = with_held_parent(state, (plan, call), t, &shape, answer).await;
    with_listed_references(rt, &shape, t, body);
    let drawn = with_held_beside(state, (plan, call), (t, &shape), body, true).await;
    with_beside_references(state, (plan, call), (t, &drawn), body, true).await;
    *body != before
}

/// `body`, a list answer to `call` whose items are of type `t`, with each reference to a
/// record of another described type naming one of that type that the session listed
/// under the same issued parent and still holds ([`state_lists::issued_marked`]: the
/// Item's accounts `/accounts/get` listed; at most [`MAX_HELD_BESIDE`]), else, with
/// none listed there or when the answer keeps records of the type it drew beside its
/// items (`drawn`, as [`with_held_beside`] left them), one the answer lists beside its
/// items ([`state_lists::beside`]: a sync's `accounts`) or one the session holds. A
/// sync's transactions then name the Item's accounts, not ones the answer drew.
///
/// For each described type `u` other than `t` whose identity member refers to it
/// (`account_id` to `account`, as [`with_listed_references`] reads it), that member is a
/// reference wherever it sits outside the arrays of `u` records, in an object holding a
/// member `u`'s records do not declare (a transaction's `amount`). Each reference naming
/// none of those records names, instead, the one at its position among the references,
/// cycling. With none listed, nothing changes. A page from state (`items` false) keeps
/// its items as held: only what its envelope drew is pointed.
async fn with_beside_references(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    (t, drawn): (&str, &[String]),
    body: &mut Value,
    items: bool,
) {
    let rt = state.rt;
    let Some(world) = rt.described().world().cloned() else {
        return;
    };
    let beside = state_lists::beside(rt, plan.op, t, body);
    let list = state_lists::list_top(rt, plan.op, body).filter(|_| !items);
    for (u, desc) in &world.types {
        let Some(member) = desc
            .identity
            .as_ref()
            .and_then(|i| i.member.as_deref())
            .filter(|m| u != t && !m.contains('/'))
        else {
            continue;
        };
        let refers = crate::call::reference_prefix(member).is_some_and(|prefix| {
            crate::call::effects::type_stem(prefix) == crate::call::effects::type_stem(u)
        });
        if !refers {
            continue;
        }
        let arrays: Vec<&str> = beside
            .iter()
            .filter(|b| b.t == *u)
            .map(|b| b.member.as_str())
            .collect();
        // The parent's records of `u` the session listed and still holds are all it
        // has: a reference names one of them, as held (text or a number), unless the
        // answer keeps records of `u` it drew beside its items, which it then names.
        let mut targets: Vec<(String, Value)> = Vec::new();
        if !drawn.contains(u) {
            let marked = state_lists::issued_marked(state, (plan, call), u).await;
            for id in marked.into_iter().take(MAX_HELD_BESIDE) {
                if let Some(held) = state.get(&plan.session, u, &id).await {
                    let value = held
                        .data
                        .get(member)
                        .filter(|v| id_string(v).as_deref() == Some(id.as_str()))
                        .cloned()
                        .unwrap_or_else(|| Value::String(id.clone()));
                    targets.push((id, value));
                }
            }
        }
        let (ids, kept) = match targets.is_empty() {
            false => (targets, Vec::new()),
            true => {
                let ids: Vec<(String, Value)> = arrays
                    .iter()
                    .filter_map(|name| body.get(*name)?.as_array())
                    .flatten()
                    .filter_map(|record| {
                        let id = record.get(member)?;
                        Some((id_string(id)?, id.clone()))
                    })
                    .collect();
                // A reference naming a record the session holds names one already.
                let mut kept: Vec<String> = Vec::new();
                for value in references(body, member, &arrays) {
                    if let Some(id) = id_string(&value)
                        && !ids.iter().any(|(listed, _)| *listed == id)
                        && !kept.contains(&id)
                        && state.get(&plan.session, u, &id).await.is_some()
                    {
                        kept.push(id);
                    }
                }
                (ids, kept)
            }
        };
        if ids.is_empty() {
            continue;
        }
        let listed = Listed {
            member: member.to_owned(),
            declared: record_members(rt, u),
            ids,
            kept,
        };
        let Some(members) = body.as_object_mut() else {
            return;
        };
        let mut at = 0;
        for (name, value) in members.iter_mut() {
            if !arrays.contains(&name.as_str()) && list.as_deref() != Some(name.as_str()) {
                repoint(value, &listed, &mut at, 0);
            }
        }
    }
}

/// The values of each `member` in `body` outside its top members `skipped`, at most
/// [`MAX_REFERENCE_DEPTH`] deep.
fn references(body: &Value, member: &str, skipped: &[&str]) -> Vec<Value> {
    fn walk(value: &Value, member: &str, depth: usize, out: &mut Vec<Value>) {
        if depth > MAX_REFERENCE_DEPTH {
            return;
        }
        match value {
            Value::Array(items) => items.iter().for_each(|v| walk(v, member, depth + 1, out)),
            Value::Object(members) => {
                for (name, held) in members {
                    match name == member {
                        true => out.push(held.clone()),
                        false => walk(held, member, depth + 1, out),
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (name, value) in body.as_object().into_iter().flatten() {
        if !skipped.contains(&name.as_str()) {
            walk(value, member, 0, &mut out);
        }
    }
    out
}

/// The most held records [`with_held_beside`] puts in one array.
const MAX_HELD_BESIDE: usize = 100;

/// `body`, a list answer of type `t` records, with each array of another type's records
/// beside them ([`state_lists::beside`]) holding the session's records of that type
/// instead of drawn ones: those listed under the same issued parent
/// ([`state_lists::issued_marked`]), else, for a page from state (not `generated`),
/// those the page's records name, at most [`MAX_HELD_BESIDE`]; one no longer held is
/// left out. Left as drawn when none is held, or the array, so filled, does not suit
/// its schema. A generated answer's drawn records beside its items are then neither
/// served nor stored where the session listed the parent's. The types of the arrays
/// left as drawn are answered, for [`with_beside_references`] to name what they hold.
async fn with_held_beside(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    (t, shape): (&str, &Shape),
    body: &mut Value,
    generated: bool,
) -> Vec<String> {
    let rt = state.rt;
    let bundle = rt.bundle();
    let mut drawn = Vec::new();
    for found in state_lists::beside(rt, plan.op, t, body) {
        if !drawn.contains(&found.t) {
            drawn.push(found.t.clone());
        }
        let mut ids = state_lists::issued_marked(state, (plan, call), &found.t).await;
        if ids.is_empty() && !generated {
            let items = shape.items_in(body).cloned().unwrap_or_default();
            for item in &items {
                if let Some(id) = item.get(&found.identity).and_then(id_string)
                    && !ids.contains(&id)
                {
                    ids.push(id);
                }
            }
        }
        if ids.is_empty() {
            continue;
        }
        let Some(array_schema) = property_schema(bundle, shape.schema(), &found.member) else {
            continue;
        };
        let (parts, _) = schema_parts(bundle, Some(&array_schema));
        let item_schema = parts.iter().find_map(|p| p.get("items")).cloned();
        let mut records = Vec::with_capacity(ids.len());
        for id in ids.iter().take(MAX_HELD_BESIDE) {
            if let Some(held) = state.get(&plan.session, &found.t, id).await {
                records.push(lists::projected(
                    bundle,
                    item_schema.as_ref(),
                    &held.data,
                    0,
                ));
            }
        }
        if records.is_empty() {
            continue;
        }
        let filled = Value::Array(records);
        let fits = plan
            .engine
            .schema_violations(bundle, &array_schema, &filled)
            .is_some_and(|violations| violations.is_empty());
        if fits && let Some(members) = body.as_object_mut() {
            members.insert(found.member, filled);
            drawn.retain(|u| *u != found.t);
        }
    }
    drawn
}

/// The members a record of type `u` declares: by its read by path, else by the items of
/// a list of it. Empty when neither says.
fn record_members(rt: &MockRuntime, u: &str) -> std::collections::HashSet<String> {
    let bundle = rt.bundle();
    let schema = rt.index().record_schemas.get(u).cloned().or_else(|| {
        bundle
            .operations
            .iter()
            .filter(|op| item_type(rt, op) == Some(u))
            .filter(|op| {
                rt.described()
                    .op(&op.id)
                    .and_then(|d| d.kind)
                    .or_else(|| OpKind::of(op))
                    == Some(OpKind::List)
            })
            .find_map(|op| Shape::of(rt, op).map(|shape| shape.items))
    });
    schema
        .and_then(|s| mb_engine::consistency::declared_members(bundle, &s))
        .unwrap_or_default()
}

/// Nested members deeper than this in an envelope are not looked into for references.
const MAX_REFERENCE_DEPTH: usize = 8;

/// What [`repoint`] reads: the listed type's identity member, the members its items'
/// schema declares, and the listed records' ids.
struct Listed {
    /// The identity member of the listed type (`account_id`).
    member: String,
    /// The members the list's item schema declares.
    declared: std::collections::HashSet<String>,
    /// Each listed record's id, as text and as held.
    ids: Vec<(String, Value)>,
    /// Ids a reference may keep, which none is pointed at: records the session holds.
    kept: Vec<String>,
}

/// `body`, a list answer of `shape` whose items are of type `t`, with each reference to
/// a record of the type outside its list naming one it lists (Plaid's
/// `numbers.ach[].account_id` beside `accounts`).
///
/// A reference is a member named as the type's described identity member is, where
/// that name refers to the type (`account_id` to `account`, as `reference_prefix` and
/// `type_stem` read it; `id` refers to no type), in a top member other than the list's
/// and the parent-shaped one ([`parent_member`]), and below. An object holding that
/// member whose every member is one the list's item schema declares is a record of
/// the type, whole or in part (a `modified` record, a `removed` stub naming one), not a
/// reference to one: it and what it holds are left as they are. The reference sits in
/// an object holding something the item schema does not declare (`ach`'s `routing`).
///
/// Each reference naming no listed record names, instead, the listed record at its
/// position among the references, cycling through them. Nothing is dropped, and with
/// no record listed, or no members declared for the items, nothing changes.
fn with_listed_references(rt: &MockRuntime, shape: &Shape, t: &str, body: &mut Value) {
    let Some(list) = shape.top_member() else {
        return;
    };
    let Some(member) = rt
        .described()
        .type_desc(t)
        .and_then(|d| d.identity.as_ref())
        .and_then(|i| i.member.as_deref())
        .filter(|m| !m.contains('/'))
    else {
        return;
    };
    let refers = crate::call::reference_prefix(member).is_some_and(|prefix| {
        crate::call::effects::type_stem(prefix) == crate::call::effects::type_stem(t)
    });
    if !refers {
        return;
    }
    let Some(declared) = mb_engine::consistency::declared_members(rt.bundle(), &shape.items) else {
        return;
    };
    let ids: Vec<(String, Value)> = shape
        .items_in(body)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get(member)?;
            Some((id_string(id)?, id.clone()))
        })
        .collect();
    if ids.is_empty() {
        return;
    }
    let parent = rt
        .described()
        .type_desc(t)
        .and_then(|d| d.parent.as_ref())
        .and_then(|link| parent_member(rt, shape, list, &link.r#type, body))
        .map(|(name, _)| name);
    let listed = Listed {
        member: member.to_owned(),
        declared,
        ids,
        kept: Vec::new(),
    };
    let Some(members) = body.as_object_mut() else {
        return;
    };
    let mut at = 0;
    for (name, value) in members.iter_mut() {
        if name != list && parent.as_deref() != Some(name.as_str()) {
            repoint(value, &listed, &mut at, 0);
        }
    }
}

/// Points each reference in `value` (at nesting `depth`, see
/// [`with_listed_references`]) holding an id no listed record has, nor one of
/// `listed.kept`, at the listed record
/// at its position `at` among them all, when that record's id is held as the reference
/// is (a string or a number).
fn repoint(value: &mut Value, listed: &Listed, at: &mut usize, depth: usize) {
    if depth > MAX_REFERENCE_DEPTH {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                repoint(item, listed, at, depth + 1);
            }
        }
        Value::Object(members) => {
            // A record of the listed type, or a part of one: no reference.
            if members.contains_key(&listed.member)
                && members.keys().all(|name| listed.declared.contains(name))
            {
                return;
            }
            for (name, held) in members.iter_mut() {
                if *name != listed.member {
                    repoint(held, listed, at, depth + 1);
                    continue;
                }
                let Some(id) = id_string(held) else {
                    continue;
                };
                if !listed.ids.iter().any(|(listed_id, _)| *listed_id == id)
                    && !listed.kept.contains(&id)
                    && let Some((_, named)) = listed.ids.get(*at % listed.ids.len())
                    && named.is_string() == held.is_string()
                {
                    *held = named.clone();
                }
                *at += 1;
            }
        }
        _ => {}
    }
}

/// Whether member `name` of answer `schema` may be `null` (a `previous` link of a
/// first page): a page from state says nothing there rather than what the faker draws.
fn admits_null(rt: &MockRuntime, schema: &Value, name: &str) -> bool {
    let bundle = rt.bundle();
    property_schema(bundle, schema, name)
        .and_then(|member| SchemaValidator::for_output(&bundle.with_reachable_defs(&member)).ok())
        .is_some_and(|validator| validator.is_valid(&Value::Null))
}
