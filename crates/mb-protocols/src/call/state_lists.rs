//! Lists answered from state, for operations the world description pages, filters,
//! scopes or orders.
//!
//! The state-first executor answers a list of a type with an identity from its first
//! call (`exec/list.rs`), through this module's [`page`] and marks; what follows is
//! the path of a list it leaves to generation.
//!
//! A list read is answered from the session's records, before any model call, when:
//! - the description sets its operation's `paging`, `filters` (non-empty), `scope` or
//!   `order` ([`ListQuery::describes_query`]; a soft delete alone is not enough);
//! - the session has listed that collection before, by a mark kept in the state store
//!   ([`mark_listed`]), so the mark survives the rebuild every world revision makes;
//! - the envelope can be filled, and the answer validates.
//!
//! Otherwise the list is generated and seeded as before. The first list of a collection
//! in a session is generated; the mark is written when it is seeded, only for a list
//! whose query asks for nothing but a page (a filtered answer shows part of the
//! collection only), and holds the ids of the records that list placed there, the
//! rest a page cut left out included. Each create landing in a marked collection adds
//! its id ([`mark_created`]). A collection is keyed by its path with the path's values
//! (`/parents/p1/children`), so two parents' lists have separate marks.
//!
//! A held record of the list's item type is in the collection when its id is in the
//! mark, or [`lists::in_collection`] says so (a route with no path parameter is one
//! collection; else this process's placement; else a member tied to each path
//! parameter holding its value). The records go in the order the mark placed them (a
//! seeded list's own order, then each create's), the others least recently written
//! after them, each as the items declare it, then each described key replaces only its own heuristic, as
//! [`list_query`] says: the described filters, else the name-matched ones; the
//! described scope as well; the soft delete always; the described order, else the
//! store's; the described page, else the page size a parameter asks for. A next page
//! of a cursor, next-URL or header-cursor paging is a [`pages`] token, kept with the
//! rest; a page number or offset is answered from state again, and its next position
//! is written at the described `response` member or `response_header` when it fits,
//! else into the next page's URL in the answer's one member for it
//! ([`pages::with_next_link`]); with none, and a `next` member beside the list left
//! `null`, the page is not served, as it would say no page follows.
//! A page with no next position (the last, or one not cut) never carries the value the
//! envelope filled that `response` member with (its contract example, or a fixed
//! value), which no client could follow: the member is left out, else `null`, else
//! empty text, the first the answer validates with. When none does, the filled value
//! stays and the answer is marked `no_page_end` in its request record's `world_gap`.
//! Every page counts the list alike: the one integer member beside the list that the
//! schema declares and names as its count ([`deleted::count_member`]), required or
//! not, holds what the query keeps before the page is cut, where the answer validates
//! so; a token's later pages keep the first page's envelope, so its count too.
//!
//! At most [`MAX_SCAN`] records are read, and a mark holds at most that many ids: a
//! collection past the cap is generated again, so a list from state never drops a
//! member silently. A record deleted since is not held, so it is not listed. Two
//! creates into one collection at the same moment may race on the mark and lose one
//! id: that record is still listed when its members name the parent, and otherwise
//! leaves the list until the collection is listed and seeded again.
//!
//! An LLM, `consistency: on`, HTTP routes and plain calls only (no scenario or forced
//! status), as [`deleted::applies`] and [`pages`] say.

use mb_state::INTERNAL_TYPE;

use super::list_query::ListQuery;
use super::*;

/// The id prefix of a collection's mark: `#` starts no `<type>/<id>` of the write
/// memory.
const PREFIX: &str = "#listed/";

/// What a collection's mark holds: the ids placed in it, and whether more were than
/// [`MAX_SCAN`].
#[derive(Debug, Default)]
struct Mark {
    ids: Vec<String>,
    over: bool,
}

impl Mark {
    fn of(value: &Value) -> Option<Self> {
        let ids = value
            .get("ids")?
            .as_array()?
            .iter()
            .filter_map(|id| id.as_str().map(str::to_string))
            .collect();
        let over = value.get("over").and_then(Value::as_bool).unwrap_or(false);
        Some(Self { ids, over })
    }

    /// Adds `ids`; past [`MAX_SCAN`], the collection is over the cap.
    fn add(&mut self, ids: impl IntoIterator<Item = String>) {
        for id in ids {
            if self.over {
                return;
            }
            if self.ids.contains(&id) {
                continue;
            }
            if self.ids.len() >= MAX_SCAN {
                self.over = true;
                return;
            }
            self.ids.push(id);
        }
    }
}

/// Whether lists of `op` may be answered from state, and marked.
fn applies(rt: &MockRuntime, plan: &CallPlan<'_>) -> bool {
    deleted::applies(rt, plan) && matches!(plan.op.binding, Binding::Http { .. }) && plain(plan)
}

/// The key of the collection `op` lists or writes in: the path with its values
/// (`collection`), else the route's path.
fn collection_key(op: &Operation, collection: Option<&Collection>) -> Option<String> {
    if let Some(collection) = collection {
        return Some(collection.path.clone());
    }
    match &op.binding {
        Binding::Http { path, .. } => Some(path.trim_end_matches('/').to_string()),
        _ => None,
    }
}

/// The id of the mark of type `t`'s collection `key`.
fn mark_id(t: &str, key: &str) -> String {
    format!("{PREFIX}{t}/{key}")
}

async fn read_mark(state: &mut StateAccess<'_>, session: &SessionKey, id: &str) -> Option<Mark> {
    let doc = state.get(session, INTERNAL_TYPE, id).await?;
    Mark::of(&doc.data)
}

async fn write_mark(state: &mut StateAccess<'_>, session: &SessionKey, id: String, mark: &Mark) {
    let document = StoredResource {
        resource_type: INTERNAL_TYPE.to_string(),
        id,
        data: serde_json::json!({"ids": mark.ids, "over": mark.over}),
    };
    state.upsert(session, document).await;
}

/// Whether the description pages, filters, scopes or orders some list of type `t`.
fn described_list_of(rt: &MockRuntime, t: &str) -> bool {
    rt.world().is_some()
        && rt.bundle().operations.iter().any(|op| {
            matches!(op.kind, OperationKind::Query)
                && op.identity.is_none()
                && op.resource_type.as_deref() == Some(t)
                && rt.described().op(&op.id).is_some_and(|d| {
                    d.paging.is_some()
                        || !d.filters.is_empty()
                        || d.scope.is_some()
                        || d.order.is_some()
                })
        })
}

/// Whether `call` asks for nothing of the list but a page: each query parameter is
/// the described paging's, or one that pages the list by its name.
fn asks_only_a_page(rt: &MockRuntime, op: &Operation, call: &OperationCall, q: &ListQuery) -> bool {
    if call.query.is_empty() {
        return true;
    }
    let size = list_query::requested_page(rt, op, call).map(|(name, _)| name);
    let paging = runs::Paging::of(rt.bundle(), op, size.as_deref());
    call.query
        .keys()
        .all(|param| q.pages_with(param) || paging.pages_list(param))
}

/// Marks the collection list `call` names as listed, with the ids of the items of
/// `generated`, its seeded answer, and `rest`, those a kept page cut left out (see the
/// module's doc). Only for a described list asking for nothing but a page.
pub(super) async fn mark_listed(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: &str,
    generated: &GeneratedResponse,
    rest: Vec<String>,
) {
    let rt = state.rt;
    let op = plan.op;
    if !applies(rt, plan) || op.identity.is_some() {
        return;
    }
    let Some(q) = list_query::of(rt, op, call).filter(ListQuery::describes_query) else {
        return;
    };
    if !asks_only_a_page(rt, op, call, &q) {
        return;
    }
    let Some(key) = collection_key(op, collection_of(op, call).as_ref()) else {
        return;
    };
    let body = generated.body.as_ref().unwrap_or(&Value::Null);
    let field = item_identity(rt.bundle(), op, body);
    let page = list_items(body)
        .into_iter()
        .flatten()
        .filter_map(|item| mb_mcm::member_at(item, &field).and_then(id_string));
    let id = mark_id(t, &key);
    let mut mark = read_mark(state, &plan.session, &id)
        .await
        .unwrap_or_default();
    mark.add(page.chain(rest));
    write_mark(state, &plan.session, id, &mark).await;
}

/// The ids the mark of the collection list `call` names holds, in the order it placed
/// them; empty when it has none, or one over [`MAX_SCAN`].
pub(super) async fn marked(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: &str,
) -> Vec<String> {
    let Some(key) = collection_key(plan.op, collection_of(plan.op, call).as_ref()) else {
        return Vec::new();
    };
    match read_mark(state, &plan.session, &mark_id(t, &key)).await {
        Some(mark) if !mark.over => mark.ids,
        _ => Vec::new(),
    }
}

/// Whether the collection `call`'s path names was listed from state: it has a mark,
/// though the mark may hold no id.
pub(super) async fn listed(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: &str,
) -> bool {
    let Some(key) = collection_key(plan.op, collection_of(plan.op, call).as_ref()) else {
        return false;
    };
    read_mark(state, &plan.session, &mark_id(t, &key))
        .await
        .is_some()
}

/// The id of the note that the session deleted a record of type `t`.
const DELETED_PREFIX: &str = "#deleted/";

/// Notes that the session deleted a record of type `t`, so the executor's lists know it
/// held the type ([`deleted_some`]) once its records are gone. Only with `consistency:
/// on`, for a type the executor lists ([`exec::lists`]).
pub(super) async fn note_deleted(state: &mut StateAccess<'_>, session: &SessionKey, t: &str) {
    let rt = state.rt;
    if !rt.consistency || !exec::lists(rt, t) {
        return;
    }
    let key = format!("{DELETED_PREFIX}{t}");
    if read_mark(state, session, &key).await.is_none() {
        write_mark(state, session, key, &Mark::default()).await;
    }
}

/// Whether the session deleted a record of type `t` ([`note_deleted`]).
pub(super) async fn deleted_some(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
) -> bool {
    read_mark(state, session, &format!("{DELETED_PREFIX}{t}"))
        .await
        .is_some()
}

/// The id prefix of a type's creation order.
const CREATED_PREFIX: &str = "#created/";

/// Adds `id`, a record of type `t` a create stored, last to the type's creation order,
/// which the executor's lists follow for records no collection mark places, so a
/// later write of a record does not move it. At most [`MAX_SCAN`] ids, the oldest
/// dropped first: an id that is no longer in it lists before those that are, as it was
/// created before them. Only with `consistency: on`, for a type the executor lists
/// ([`exec::lists`]).
pub(super) async fn note_created(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    id: &str,
) {
    let rt = state.rt;
    if !rt.consistency || !exec::lists(rt, t) {
        return;
    }
    let key = format!("{CREATED_PREFIX}{t}");
    let mut order = read_mark(state, session, &key).await.unwrap_or_default();
    if order.ids.iter().any(|held| held == id) {
        return;
    }
    if order.ids.len() >= MAX_SCAN {
        order.ids.drain(..=order.ids.len() - MAX_SCAN);
    }
    order.ids.push(id.to_string());
    write_mark(state, session, key, &order).await;
}

/// The ids of type `t`'s records in the order creates stored them ([`note_created`]).
pub(super) async fn created_order(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
) -> Vec<String> {
    read_mark(state, session, &format!("{CREATED_PREFIX}{t}"))
        .await
        .map(|order| order.ids)
        .unwrap_or_default()
}

/// Adds `id`, a record of type `t` that `op` created in `collection`, to that
/// collection's mark, when it has one (see the module's doc), and last to the type's
/// creation order ([`note_created`]).
pub(super) async fn mark_created(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (op, t): (&Operation, &str),
    collection: Option<&Collection>,
    id: &str,
) {
    note_created(state, session, t, id).await;
    let rt = state.rt;
    if !rt.consistency || !described_list_of(rt, t) {
        return;
    }
    let Some(key) = collection_key(op, collection) else {
        return;
    };
    let mark_id = mark_id(t, &key);
    let Some(mut mark) = read_mark(state, session, &mark_id).await else {
        return;
    };
    if mark.over || mark.ids.iter().any(|held| held == id) {
        return;
    }
    mark.add([id.to_string()]);
    write_mark(state, session, mark_id, &mark).await;
}

/// Where `op`'s list answer `schema` holds its items: the member path (`answer.records`,
/// else its one list member; empty for an answer that is the array), with the array's
/// schema.
fn list_place(rt: &MockRuntime, op: &Operation, schema: &Value) -> Option<(Vec<String>, Value)> {
    let bundle = rt.bundle();
    let described = rt
        .described()
        .op(&op.id)
        .and_then(|d| d.answer.as_ref())
        .and_then(|a| a.records.as_ref())
        .filter(|p| !p.is_root())
        .map(mb_mcm::world::Pointer::tokens);
    let path = match described {
        Some(path) => path,
        None => {
            let (parts, _) = schema_parts(bundle, Some(schema));
            if parts.iter().any(|p| p.get("items").is_some()) {
                return Some((Vec::new(), schema.clone()));
            }
            let members = list_members(bundle, schema);
            let [member] = members.as_slice() else {
                return None;
            };
            vec![member.clone()]
        }
    };
    let mut at = schema.clone();
    for name in &path {
        at = property_schema(bundle, &at, name)?;
    }
    Some((path, at))
}

/// `items` placed at `path` of `envelope`, the whole answer for an empty path.
fn with_items(envelope: &Value, path: &[String], items: Vec<Value>) -> Option<Value> {
    match path.is_empty() {
        true => Some(Value::Array(items)),
        false => pages::placed(envelope, path, Some(Value::Array(items))),
    }
}

/// `body` with `total` at the member counting its list at `path` (see
/// [`deleted::count_member`]), declared by `schema`, the answer's; `None` when the
/// answer is the list, or nothing beside it counts it.
fn with_count(
    bundle: &mb_mcm::Bundle,
    schema: &Value,
    body: &Value,
    path: &[String],
    total: usize,
) -> Option<Value> {
    let (list, parents) = path.split_last()?;
    let mut envelope = schema.clone();
    for name in parents {
        envelope = property_schema(bundle, &envelope, name)?;
    }
    let member = deleted::count_member(bundle, &envelope, list)?;
    let at: Vec<String> = parents.iter().cloned().chain([member]).collect();
    pages::placed(body, &at, Some(Value::from(total)))
}

/// The mark a page from state carries in its request record's `world_gap` when the
/// schema of the described paging `response` member admits no value saying there is no
/// next page, so the value the envelope was filled with stays.
const NO_PAGE_END: &str = "no_page_end";

/// `body` with no next page at `path`, the described paging `response` member, in the
/// order tried: the member left out, `null`, then empty text.
fn page_ends(body: &Value, path: &[String]) -> Vec<Value> {
    [None, Some(Value::Null), Some(Value::String(String::new()))]
        .into_iter()
        .filter_map(|end| pages::placed(body, path, end))
        .collect()
}

/// Where `op`'s list answer holds its items ([`list_place`]), with the schemas a page
/// from state is rendered by.
pub(super) struct Shape {
    /// The answer's schema.
    schema: Value,
    /// The member path of the items; empty for an answer that is the array.
    path: Vec<String>,
    /// The array's schema.
    array: Value,
    /// The schema of one item.
    pub(super) items: Value,
}

impl Shape {
    /// The shape of `op`'s success answer; `None` when it holds no one list.
    pub(super) fn of(rt: &MockRuntime, op: &Operation) -> Option<Self> {
        let schema = op.success_output()?.schema.as_ref()?;
        let (path, array) = list_place(rt, op, schema)?;
        let (array_parts, _) = schema_parts(rt.bundle(), Some(&array));
        let items = array_parts.iter().find_map(|p| p.get("items"))?.clone();
        Some(Self {
            schema: schema.clone(),
            path,
            array,
            items,
        })
    }

    /// The answer's schema.
    pub(super) fn schema(&self) -> &Value {
        &self.schema
    }

    /// The members above the list: empty for a list at the envelope's top, or an answer
    /// that is the array.
    pub(super) fn parents(&self) -> &[String] {
        self.path.split_last().map_or(&[], |(_, parents)| parents)
    }

    /// The envelope of an empty page, its other required members filled as a held
    /// answer's are, from `kept` first (see [`mb_engine::Engine::fill_envelope`]);
    /// `None` when one cannot be.
    pub(super) fn envelope(
        &self,
        rt: &MockRuntime,
        plan: &CallPlan<'_>,
        kept: Option<&Map<String, Value>>,
    ) -> Option<Value> {
        let Some(first) = self.path.first() else {
            return Some(Value::Array(Vec::new()));
        };
        let mut top = Map::new();
        plan.engine
            .fill_envelope(rt.bundle(), &self.schema, &mut top, first, kept)?;
        with_items(&Value::Object(top), &self.path, Vec::new())
    }
}

/// Why [`page`] serves no page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Unserved {
    /// The page does not validate against the answer's schema.
    Render,
    /// The page drops records past its size, and no token or run can serve them.
    NoPlace,
}

/// The answer to list `call` from the session's records (see the module's doc), tagged
/// `State`; `None` when it is not one to answer so, the collection was not listed this
/// session, or the answer does not validate: the list is then generated.
pub(super) async fn answer(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Option<GeneratedResponse> {
    let rt = state.rt;
    let op = plan.op;
    let session = &plan.session;
    if !matches!(op.kind, OperationKind::Query)
        || op.identity.is_some()
        || !applies(rt, plan)
        || mb_engine::seed::request_seed(call).is_some()
    {
        return None;
    }
    let q = list_query::of(rt, op, call).filter(ListQuery::describes_query)?;
    let t = op.resource_type.as_deref()?;
    // A token no kept cut served ([`pages::serve`] ran first) is the model's.
    if q.page
        .as_ref()
        .is_some_and(|p| !p.counts() && p.position.is_some())
    {
        return None;
    }
    let bundle = rt.bundle();
    let shape = Shape::of(rt, op)?;
    let (item_parts, _) = schema_parts(bundle, Some(&shape.items));
    let size = list_query::requested_page(rt, op, call);
    let size_ref = size.as_ref().map(|(name, limit)| (name.as_str(), *limit));
    let query = op.input.query.as_ref();
    let filters: Vec<_> = lists::filters_of(bundle, op, query, &item_parts, call, size_ref)
        .into_iter()
        .filter(|(param, _)| !q.pages_with(param))
        .collect();
    // A value no record can be shown to answer (a search, an undescribed position).
    if call.query.keys().any(|param| {
        !filters.iter().any(|(name, _)| name == param)
            && !q.knows(param)
            && size_ref.is_none_or(|(name, _)| name != param)
    }) {
        return None;
    }
    let envelope = shape.envelope(rt, plan, None)?;
    let key = collection_key(op, collection_of(op, call).as_ref())?;
    let mark = read_mark(state, session, &mark_id(t, &key)).await?;
    if mark.over {
        return None;
    }
    let mut records = state.list(session, t, MAX_SCAN + 1).await;
    if records.len() > MAX_SCAN {
        return None;
    }
    // The collection's own order: as its mark placed them (a seeded list's order, then
    // each create's), then the rest least recently written first.
    records.reverse();
    let placed: HashMap<&str, usize> = mark
        .ids
        .iter()
        .enumerate()
        .map(|(at, id)| (id.as_str(), at))
        .collect();
    records.sort_by_key(|r| placed.get(r.id.as_str()).copied().unwrap_or(usize::MAX));
    let field = item_identity(bundle, op, &envelope);
    let mut items = Vec::new();
    for r in &records {
        if !placed.contains_key(r.id.as_str())
            && !lists::in_collection(rt, (plan, call), t, &field, r)
        {
            continue;
        }
        let item = lists::projected(bundle, Some(&shape.items), &r.data, 0);
        let kept = list_query::keeps(Some(&q), &item, |i| {
            filters.iter().all(|(_, f)| lists::holds_wanted(i, f))
        });
        if kept {
            items.push(item);
        }
    }
    page(
        state,
        (plan, call),
        (t, Some(&q)),
        (&shape, &envelope),
        items,
        false,
    )
    .await
    .ok()
}

/// The page list `call` asks for of `items`, the records of type `t` its query keeps,
/// each as the items declare it, in the collection's order, placed in `envelope` (an
/// empty page of `shape`): each item's derived members filled, ordered by `q`'s
/// described order, counted, and cut by its described page (a page number or offset
/// from state, a token's later pages kept as a [`pages`] cut), else by the page size a
/// parameter asks for (see the module's doc). Tagged `State`. A cut with no place for
/// a token, nor a run, is not served, unless `served_anyway`: then a described page is
/// served whole (narrowed and ordered, but not cut, as a generated one is), and a page
/// size the description does not name cuts the page.
pub(super) async fn page(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    (t, q): (&str, Option<&ListQuery>),
    (shape, envelope): (&Shape, &Value),
    mut items: Vec<Value>,
    served_anyway: bool,
) -> Result<GeneratedResponse, Unserved> {
    let rt = state.rt;
    let op = plan.op;
    let bundle = rt.bundle();
    let (schema, path) = (&shape.schema, &shape.path);
    let (array_parts, _) = schema_parts(bundle, Some(&shape.array));
    let size = list_query::requested_page(rt, op, call);
    // Each item's derived members, from one listing per child type.
    derived::fill_page(state, &plan.session, t, &mut items, Some(&shape.items)).await;
    if let Some(q) = q {
        list_query::sort(q, &mut items);
    }
    // What the query keeps, before the page is cut: the list's count on every page.
    let total = items.len();
    let mut next: Option<usize> = None;
    let mut cut = None;
    let ask = q.and_then(|q| q.page.as_ref());
    let page: Vec<Value> = match (q, ask) {
        (Some(q), Some(ask)) if ask.counts() => {
            let (page, more) = list_query::page(q, &items);
            let size = ask.size.unwrap_or_default();
            next = more
                .and_then(|end| end.parse::<usize>().ok())
                .map(|end| match ask.style {
                    mb_mcm::world::PagingStyle::Page if size > 0 => end / size + ask.first(),
                    _ => end,
                });
            page.to_vec()
        }
        (_, Some(ask)) => match (ask.size.filter(|s| *s > 0), ask.size_param.clone()) {
            (Some(size), Some(size_param)) if items.len() > size => {
                let rest = items.split_off(size);
                cut = Some((rest, size, size_param));
                items
            }
            _ => items,
        },
        (_, None) => match size.as_ref() {
            Some((size_param, limit)) => match lists::page_size(*limit, &array_parts) {
                Some(size) if items.len() > size => {
                    let rest = items.split_off(size);
                    cut = Some((rest, size, size_param.clone()));
                    items
                }
                _ => items,
            },
            None => items,
        },
    };
    let all: Option<Vec<Value>> = match &cut {
        Some((rest, _, _)) if served_anyway && ask.is_some() => {
            Some(page.iter().chain(rest).cloned().collect())
        }
        _ => None,
    };
    let mut body = with_items(envelope, path, page).ok_or(Unserved::Render)?;
    let valid = |candidate: &Value| plan.engine.serve_stored(bundle, op, candidate, None);
    if let Some(counted) =
        with_count(bundle, schema, &body, path, total).filter(|c| valid(c).is_some())
    {
        body = counted;
    }
    // The paging member the envelope was filled with (a fixed value, or the contract's
    // example) is no position of this list: with no next page to write there, it ends.
    let at_next = ask
        .and_then(|ask| ask.response.as_deref())
        .map(|member| member.split('.').map(str::to_string).collect::<Vec<_>>());
    let mut no_end = false;
    if let Some(at) = at_next.as_deref().filter(|_| next.is_none())
        && at.iter().try_fold(&body, |v, name| v.get(name)).is_some()
    {
        match page_ends(&body, at)
            .into_iter()
            .find(|end| valid(end).is_some())
        {
            Some(end) => body = end,
            None => {
                tracing::info!(
                    operation = %op.id,
                    world_gap = NO_PAGE_END,
                    "no value of the list's next-page member says there is no next page; the page from state keeps the one filled"
                );
                no_end = true;
            }
        }
    }
    let mut header = None;
    if let (Some(next), Some(ask)) = (next, ask) {
        if let Some(at) = &at_next {
            body = [Value::from(next), Value::String(next.to_string())]
                .into_iter()
                .filter_map(|value| pages::placed(&body, at, Some(value)))
                .find(|candidate| valid(candidate).is_some())
                .ok_or(Unserved::Render)?;
        }
        if let Some(name) = &ask.response_header {
            let value = next.to_string();
            if !pages::header_fits(rt, op, name, &value) {
                return Err(Unserved::Render);
            }
            header = Some((name.clone(), value));
        }
        // With no described place for it, a page number or offset goes in the next
        // page's URL, where the answer has one member for it (NetBox's `next`); a
        // `next` beside the list left `null` would say no page follows, so the page is
        // not served then.
        if at_next.is_none() && ask.response_header.is_none() {
            let list = path.first();
            let linked = match (ask.request.as_deref(), plan.page_url.as_deref(), list) {
                (Some(param), Some(url), Some(list)) => pages::with_next_link(
                    bundle,
                    (op, call, url),
                    (schema, list),
                    &body,
                    (param, &next.to_string()),
                ),
                _ => None,
            };
            match linked {
                Some(candidates) => {
                    body = candidates
                        .into_iter()
                        .find(|candidate| valid(candidate).is_some())
                        .ok_or(Unserved::Render)?;
                }
                None if list.is_some_and(|list| pages::ends_by_next(&body, list)) => {
                    return Err(Unserved::NoPlace);
                }
                None => {}
            }
        }
        // The one flag beside the list saying more follow (`has_more`) says so.
        if let [flag] = pages::booleans_at(bundle, op, shape.parents()).as_slice()
            && flag.to_ascii_lowercase().contains("more")
            && let Some(more) =
                pages::with_more(&body, shape.parents(), flag).filter(|c| valid(c).is_some())
        {
            body = more;
        }
    }
    let mut served = valid(&body).ok_or(Unserved::Render)?;
    served.headers.extend(header);
    if no_end {
        served.world_gaps.push(NO_PAGE_END.to_string());
    }
    if let Some((rest, size, size_param)) = cut {
        let cut = lists::Cut {
            stored: rest.len(),
            rest,
            size,
            size_param,
            truncated: false,
            unnarrowed: None,
            uncut: all
                .and_then(|all| with_items(&body, path, all))
                .filter(|whole| valid(whole).is_some()),
        };
        // No place for a token, nor a run: the page would drop the rest, so it goes to
        // the model, unless it is served anyway ([`pages::mint`] put a whole one in
        // place).
        if !pages::mint(state, (plan, call), Some(t), cut, &mut served).await && !served_anyway {
            return Err(Unserved::NoPlace);
        }
    }
    pages::no_next_header(rt, (op, call), &mut served.headers);
    Ok(served)
}
