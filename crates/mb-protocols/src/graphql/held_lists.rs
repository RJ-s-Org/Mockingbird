//! Connections answered from the records the session holds, and lookups of records it
//! does not hold, before anything is generated.
//!
//! **Connections.** A query root field whose type is a connection (an object with
//! `edges { node }` or `nodes` of one keyed object type: one with its identity member,
//! the world's, else `id`) is answered from the session's records of the node's type
//! (`Product` → `product`) as soon as the session holds one, tagged `state`, with no
//! LLM call; holding none, it is generated as before. The records go in the order they
//! were written, least recently written first, less the soft-deleted ones (the type's
//! `lifecycle.soft_delete`, unless its `listed_with` argument asks for them), then:
//!
//! - an argument the operation's `filters` describe (`kind` `eq`, `contains`, `gte`,
//!   `lte`, `flag`, `text`) keeps the records it holds for; one naming a scalar member
//!   of the node (or, a list, its singular: `states` → `state`) keeps the records whose
//!   member equals it (one of its values). A record that does not hold the member does
//!   not match;
//! - `sortKey` (or `orderBy: {field, direction}`) orders them by the member its value
//!   names, its words compared without case or separators: `TITLE` → `title`,
//!   `CREATED_AT` → `createdAt`, a key that is the start of a member's name (`CREATED`
//!   → `createdAt`, `PRICE` → `priceRange`) by that member, down an object member to its
//!   first `min…` member, then its `amount` or `value`, then its first number, then its
//!   first scalar (`PRICE` → `priceRange.minVariantPrice.amount`). A total order:
//!   numbers, and text that reads as one (`"12.50"`), first by value, then other text
//!   without case, then anything else; a record that holds no value (or `null`) goes
//!   last, under `reverse` too. A key naming no member, or the identity (`RELEVANCE`,
//!   `BEST_SELLING`, `ID`), keeps the written order. `reverse: true` reverses the
//!   order either way;
//! - `first`/`after` and `last`/`before` cut the page, each record's cursor its id
//!   (with neither count, at most [`DEFAULT_PAGE`]; with both, `first` then `last`, as
//!   Relay does); `pageInfo` says what the cut left
//!   (`hasNextPage`, `hasPreviousPage`, `startCursor`, `endCursor`) and `totalCount`
//!   counts every record kept.
//!
//! Each node is the record in the selection's shape: its members over the faker's
//! (drawn from a seed of its type and id, so every answer of it agrees), its
//! connections answered from the items it holds as below. The answer must validate
//! against the selection's schema.
//!
//! It is generated as before (as a REST list falls back on `undescribed_param`) when an
//! argument asks something none of the above reads: a free-text `query:` the world
//! does not describe as a filter, a filter with a `grammar`, any other argument with a
//! value (null, blank and empty values ask nothing, and the operation's `ignores` are
//! read past); and for a cursor that names no record kept, more than [`MAX_RECORDS`]
//! records of the type, a scenario, a seed header, a forced status (which wins over
//! state, `200` too; as for the connections and lookups below), `consistency` off, an operation the
//! world describes as another kind than a list, and an answer that does not validate.
//!
//! **Plain lists.** A root field whose type is a plain list of one keyed object type
//! (`[Product!]`) is answered the same way, with no cursors' `pageInfo`: every record
//! kept, to [`MAX_RECORDS`], unless `first` or `limit` cuts them. A described `scope`
//! with a `member` (`eq`) filters like a described `eq` filter, for a list or a
//! connection.
//!
//! **Connections below a held record.** A connection member of a record the session
//! holds (a collection's `products`, a product's `variants`), selected below a root
//! field that answers it or below a node above, is answered from the items the record
//! holds there (`edges[].node`, `nodes`, or a plain list), each the session's own
//! record of the node's type where it holds one under that id (its members over the
//! item's), filtered, ordered and cut as above by the field's own arguments. An item
//! without an id, or an argument none of the above reads, leaves the member as it was
//! answered; so do held records of the node's type that refer to the record (an
//! issue's `repository`: they list under it, see [`super::nested`]), and a generated
//! answer that shows exactly the records the member holds, each selected scalar as the
//! session holds it (the answer the record was stored from). Below a generated answer,
//! an item the session holds no record of is shown as the answer showed it, or left
//! out when it did not: nothing is drawn for it, so nothing drawn reaches state. What
//! the record holds there counts as the whole list (a page stored by a narrower read
//! included).
//!
//! **Lookups.** A query root field answering one keyed object, read by an argument that
//! names it (the identity's input field, `id` or an inferred one such as `login`; the
//! world's target member; one of the type's described `alternates`, such as `handle`),
//! answers `null` and a `NOT_FOUND` error when `unknown_ids` resolves to 404 for the
//! type and the session holds no record of it with that value ([`unknown_lookup`]).

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use mb_engine::faker::Faker;
use mb_engine::{GeneratedResponse, OperationCall};
use mb_mcm::world::{FilterDesc, FilterKind, OpDesc, OpKind};
use mb_mcm::{Bundle, Operation, OperationKind};
use mb_state::SessionKey;
use rand::SeedableRng;
use rand::rngs::StdRng;
use serde_json::{Map, Value, json};

use super::nested::{def_of, def_ref, is_scalar};
use super::resolve::Failure;
use super::select::{Selection, rekey};
use super::state::list_item;
use crate::MockRuntime;
use crate::call::{self, CallPlan};

/// Records of one type a connection is answered from, at most: more, and it is
/// generated (as a REST list over its scan cap is).
const MAX_RECORDS: usize = 500;

/// Items a connection answers when its arguments set no `first` or `last`.
const DEFAULT_PAGE: usize = 100;

/// Bounds the walk below a node (recursive types).
const MAX_DEPTH: usize = 8;

/// What answers a root field without generating.
pub(crate) enum Held {
    /// A connection from the session's records.
    Served(Box<GeneratedResponse>),
    /// A lookup of a record the session does not hold, under `unknown_ids: 404`.
    Missing(Failure),
}

/// The answer of `plan`'s root field (`call`, selected by `sel`, its success schema
/// narrowed to `narrowed`) that needs no generation, if any: see the module's doc.
pub(crate) async fn answer(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    sel: &Selection,
    narrowed: Option<&Value>,
) -> Option<Held> {
    let op = plan.op;
    if !plain_query(rt, plan, call) {
        return None;
    }
    let empty = Map::new();
    let args = match &call.body {
        Some(Value::Object(args)) => args,
        _ => &empty,
    };
    let schema = op.success_output()?.schema.as_ref()?;
    if let Some(node) = connection_node(&rt.bundle, rt, schema) {
        let served = connection(rt, plan, sel, args, &node, narrowed?).await?;
        return Some(Held::Served(Box::new(served)));
    }
    if let Some(node) = list_node(&rt.bundle, rt, schema) {
        let served = plain_list(rt, plan, sel, args, &node, narrowed?).await?;
        return Some(Held::Served(Box::new(served)));
    }
    lookup(rt, &plan.session, op, args).await.map(Held::Missing)
}

/// Whether `plan`'s call is a query state answers as it is: `consistency` on, and no
/// scenario, seed header or forced status (which wins, `200` too).
fn plain_query(rt: &MockRuntime, plan: &CallPlan<'_>, call: &OperationCall) -> bool {
    plan.op.kind == OperationKind::Query
        && rt.consistency
        && plan.scenario.is_none()
        && plan.forced_status.is_none()
        && mb_engine::seed::request_seed(call).is_none()
}

/// The concrete, keyed node type (its definition name) of the connection `schema`
/// refers to; `None` when it is no connection.
fn connection_node(bundle: &Bundle, rt: &MockRuntime, schema: &Value) -> Option<String> {
    let def = def_of(bundle, schema)?;
    let props = def.get("properties")?.as_object()?;
    let by_edges = props
        .get("edges")
        .and_then(list_item)
        .and_then(|edge| def_of(bundle, edge))
        .and_then(|edge| edge.get("properties")?.get("node"))
        .and_then(def_ref);
    let node = by_edges.or_else(|| props.get("nodes").and_then(list_item).and_then(def_ref))?;
    let node_def = bundle.defs.get(node)?;
    let node_props = node_def.get("properties")?.as_object()?;
    (node_def.get("oneOf").is_none()
        && node_props.contains_key(&identity_member(rt, &node.to_lowercase())))
    .then(|| node.to_string())
}

/// The concrete, keyed item type (its definition name) of the plain list `schema` is
/// (`[Product!]`, nullable or not); `None` when it is no list of one.
fn list_node(bundle: &Bundle, rt: &MockRuntime, schema: &Value) -> Option<String> {
    let node = def_ref(list_item(schema)?)?;
    let node_def = bundle.defs.get(node)?;
    let node_props = node_def.get("properties")?.as_object()?;
    (node_def.get("oneOf").is_none()
        && node_props.contains_key(&identity_member(rt, &node.to_lowercase())))
    .then(|| node.to_string())
}

/// The member that names a record of type `t`: the world's, else `id`.
fn identity_member(rt: &MockRuntime, t: &str) -> String {
    rt.described()
        .type_desc(t)
        .and_then(|d| d.identity.as_ref())
        .and_then(|i| i.member.clone())
        .unwrap_or_else(|| "id".to_string())
}

/// One record a connection may list: its id and its members.
#[derive(Clone)]
struct Item {
    id: String,
    data: Value,
    /// Shown as the generated answer showed it: an item of a held record's list the
    /// session holds no record of, below a generated answer (nothing is drawn for it,
    /// so nothing drawn is stored).
    as_answered: bool,
}

/// The records the walk below a node reads, by type and id.
struct Records<'a> {
    rt: &'a MockRuntime,
    by_type: HashMap<String, HashMap<String, Value>>,
}

impl Records<'_> {
    fn get(&self, t: &str, id: &str) -> Option<&Value> {
        self.by_type.get(t)?.get(id)
    }

    fn bundle(&self) -> &Bundle {
        &self.rt.bundle
    }

    /// Whether a held record of node type `node` (a definition name) refers to the
    /// record `id`: one of its object members is that record (`repository: {id}`).
    fn refer_to(&self, node: &str, id: &str) -> bool {
        self.by_type
            .get(&node.to_lowercase())
            .into_iter()
            .flat_map(HashMap::values)
            .any(|data| {
                data.as_object().is_some_and(|members| {
                    members
                        .values()
                        .any(|v| v.get("id").and_then(id_text).as_deref() == Some(id))
                })
            })
    }
}

/// The node types of the connections `sel` reaches below the object type `def`.
fn connection_types(
    rt: &MockRuntime,
    def: &str,
    sel: &Selection,
    depth: usize,
    out: &mut BTreeSet<String>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let bundle = &rt.bundle;
    let Some(props) = bundle
        .defs
        .get(def)
        .and_then(|d| d.get("properties"))
        .and_then(Value::as_object)
    else {
        return;
    };
    for child in sel.fields.values() {
        let Some(schema) = child.names().find_map(|n| props.get(n)) else {
            continue;
        };
        if let Some(node) = connection_node(bundle, rt, schema) {
            if out.insert(node.clone()) {
                for below in node_selections(child) {
                    connection_types(rt, &node, below, depth + 1, out);
                }
            }
        } else if let Some(inner) = def_ref(list_item(schema).unwrap_or(schema))
            && bundle
                .defs
                .get(inner)
                .is_some_and(|d| d.get("oneOf").is_none())
        {
            connection_types(rt, inner, child, depth + 1, out);
        }
    }
}

/// The selections of a connection's nodes: its `nodes`, and its `edges`' `node`.
fn node_selections(sel: &Selection) -> Vec<&Selection> {
    let mut out = Vec::new();
    for child in sel.fields.values() {
        match child.name.as_str() {
            "nodes" => out.push(child),
            "edges" => out.extend(child.fields.values().filter(|c| c.name == "node")),
            _ => {}
        }
    }
    out
}

/// The session's records of `types`, at most [`MAX_RECORDS`] of each.
async fn load<'a>(
    rt: &'a MockRuntime,
    session: &SessionKey,
    types: impl IntoIterator<Item = String>,
) -> Records<'a> {
    let mut by_type = HashMap::new();
    for def in types {
        let t = def.to_lowercase();
        if by_type.contains_key(&t) {
            continue;
        }
        let records: HashMap<String, Value> = call::stored_of_type(rt, session, &t, MAX_RECORDS)
            .await
            .into_iter()
            .map(|r| (r.id, r.data))
            .collect();
        by_type.insert(t, records);
    }
    Records { rt, by_type }
}

/// A root connection of node type `node` answered from the session's records.
async fn connection(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    sel: &Selection,
    args: &Map<String, Value>,
    node: &str,
    narrowed: &Value,
) -> Option<GeneratedResponse> {
    let page = held_page(rt, plan, args, node, DEFAULT_PAGE).await?;
    let mut below = BTreeSet::new();
    for node_sel in node_selections(sel) {
        connection_types(rt, node, node_sel, 1, &mut below);
    }
    let records = load(rt, &plan.session, below).await;
    let body = render_connection(&records, node, &page, (sel, None), narrowed, 0);
    plan.engine
        .serve_stored(&rt.bundle, plan.op, &body, Some(narrowed))
}

/// A root plain list of item type `node` (`[Product!]`) answered from the session's
/// records, as a connection is, all of them (to [`MAX_RECORDS`]) unless a `first` or
/// `limit` argument cuts them.
async fn plain_list(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    sel: &Selection,
    args: &Map<String, Value>,
    node: &str,
    narrowed: &Value,
) -> Option<GeneratedResponse> {
    let page = held_page(rt, plan, args, node, MAX_RECORDS).await?;
    let mut below = BTreeSet::new();
    connection_types(rt, node, sel, 1, &mut below);
    let records = load(rt, &plan.session, below).await;
    let item_schema = items_of(narrowed);
    let body = Value::Array(
        page.items
            .iter()
            .map(|item| render_node(&records, node, item, (sel, None), item_schema, 0))
            .collect(),
    );
    plan.engine
        .serve_stored(&rt.bundle, plan.op, &body, Some(narrowed))
}

/// The page of the session's records of node type `node` a root list read with
/// `args` answers, at most `page_size` with no count asked; `None` when it is
/// generated instead (see the module's doc).
async fn held_page(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    args: &Map<String, Value>,
    node: &str,
    page_size: usize,
) -> Option<Page> {
    let op = plan.op;
    let desc = rt.described().op(&op.id);
    if desc
        .and_then(|d| d.kind)
        .is_some_and(|kind| kind != OpKind::List)
    {
        return None;
    }
    let t = node.to_lowercase();
    let mut records =
        call::graphql_reads::held_of_type(rt, &plan.session, &t, MAX_RECORDS + 1).await;
    if records.is_empty() || records.len() > MAX_RECORDS {
        return None;
    }
    // Most recently written first, as the store lists them: the written order is the reverse.
    records.reverse();
    let mut ask = Ask::of(rt, desc, node, args)?;
    if ask.first.is_none() && ask.last.is_none() {
        ask.first = Some(page_size);
    }
    let items: Vec<Item> = records
        .into_iter()
        // A soft-deleted record is left out, as a REST list leaves it out.
        .filter(|r| !call::graphql_reads::soft_deleted(rt, &t, &r.data, args))
        .map(|r| Item {
            id: r.id,
            data: r.data,
            as_answered: false,
        })
        .collect();
    ask.page(&rt.bundle, rt, node, items)
}

/// Each connection selected below `body`, the root answer of `plan`'s query (keyed by
/// response key, for `sel`; `generated` unless served from state), when it is a record
/// the session holds: answered from the items that record holds there (see the
/// module's doc). Anything else is left as answered, and so is a generated connection
/// that shows the very records the held one holds (the answer it was stored from).
pub(crate) async fn below_held(
    rt: &MockRuntime,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    sel: &Selection,
    (body, generated): (&mut Value, bool),
) {
    let op = plan.op;
    if !plain_query(rt, plan, call) {
        return;
    }
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let bundle = &rt.bundle;
    if connection_node(bundle, rt, schema).is_some() {
        return;
    }
    let Some(def) = def_ref(schema).filter(|d| {
        bundle
            .defs
            .get(*d)
            .is_some_and(|def| def.get("oneOf").is_none())
    }) else {
        return;
    };
    let mut types = BTreeSet::new();
    connection_types(rt, def, sel, 1, &mut types);
    if types.is_empty() {
        return;
    }
    let t = def.to_lowercase();
    let member = identity_member(rt, &t);
    let id = sel
        .fields
        .iter()
        .find(|(_, c)| c.name == member)
        .and_then(|(key, _)| body.get(key))
        .or_else(|| body.get(&member))
        .and_then(id_text);
    let Some(id) = id else {
        return;
    };
    let Some(held) = call::stored(rt, &plan.session, &t, &id).await else {
        return;
    };
    let records = load(rt, &plan.session, types).await;
    let narrowed = rt.narrowed().narrow(&op.id, schema, &bundle.defs, sel);
    let mut out = body.clone();
    connections_below(
        &records,
        (def, &held.id, generated),
        &held.data,
        sel,
        &narrowed,
        &mut out,
        1,
    );
    if out != *body
        && plan
            .engine
            .serve_stored(bundle, op, &out, Some(&narrowed))
            .is_some()
    {
        *body = out;
    }
}

/// The text of an id value.
fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// What a connection's arguments ask of its records.
#[derive(Default)]
struct Ask {
    first: Option<usize>,
    last: Option<usize>,
    after: Option<String>,
    before: Option<String>,
    /// The sort key the request names, and whether it asks largest first.
    sort: Option<(String, bool)>,
    reverse: bool,
    filters: Vec<Filter>,
}

/// One filter of a connection's arguments.
struct Filter {
    /// The (dotted) member it tests; every string member for a `text` one with none.
    member: Option<String>,
    test: Test,
}

enum Test {
    /// Equal to one of these.
    Eq(Vec<Value>),
    Contains(Value),
    Gte(Value),
    Lte(Value),
    Flag(bool),
    Text(String),
}

/// Whether an argument's value asks nothing: `null`, blank, empty.
fn asks_nothing(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(members) => members.is_empty(),
        _ => false,
    }
}

impl Ask {
    /// What `args` ask of a connection of node type `node`, described by `desc` (a root
    /// field's operation; none below one); `None` when an argument asks something none
    /// of the rules reads.
    fn of(
        rt: &MockRuntime,
        desc: Option<&OpDesc>,
        node: &str,
        args: &Map<String, Value>,
    ) -> Option<Self> {
        let bundle = &rt.bundle;
        // The soft delete's own parameter is read where its records are left out.
        let listed_with = call::graphql_reads::listed_with(rt, &node.to_lowercase());
        let props = bundle
            .defs
            .get(node)
            .and_then(|d| d.get("properties"))
            .and_then(Value::as_object)?;
        let count = |v: &Value| v.as_u64().and_then(|n| usize::try_from(n).ok());
        let mut ask = Ask::default();
        for (name, value) in args {
            match name.as_str() {
                "first" | "limit" if !value.is_null() => ask.first = Some(count(value)?),
                "last" if !value.is_null() => ask.last = Some(count(value)?),
                "after" if !value.is_null() => ask.after = Some(id_text(value)?),
                "before" if !value.is_null() => ask.before = Some(id_text(value)?),
                "first" | "limit" | "last" | "after" | "before" => {}
                "reverse" => ask.reverse = value.as_bool().unwrap_or(false),
                "sortKey" | "sort_key" => {
                    if let Some(key) = value.as_str() {
                        ask.sort = Some((key.to_string(), false));
                    }
                }
                "orderBy" | "order_by" if value.is_object() => {
                    let field = value.get("field").and_then(Value::as_str)?;
                    let desc = value
                        .get("direction")
                        .and_then(Value::as_str)
                        .is_some_and(|d| d.eq_ignore_ascii_case("desc"));
                    ask.sort = Some((field.to_string(), desc));
                }
                _ if asks_nothing(value) => {}
                _ if desc.is_some_and(|d| d.ignores.iter().any(|p| p == name)) => {}
                _ if listed_with.as_deref() == Some(name.as_str()) => {}
                _ => ask.filters.push(filter(bundle, desc, props, name, value)?),
            }
        }
        Some(ask)
    }

    /// The page of `items` (of node type `node`, in written order) the request asks
    /// for; `None` when a cursor names no item kept.
    fn page(
        &self,
        bundle: &Bundle,
        rt: &MockRuntime,
        node: &str,
        items: Vec<Item>,
    ) -> Option<Page> {
        let mut kept: Vec<Item> = items
            .into_iter()
            .filter(|item| self.filters.iter().all(|f| f.keeps(&item.data)))
            .collect();
        let identity = identity_member(rt, &node.to_lowercase());
        if let Some((key, desc)) = &self.sort
            && let Some(path) = sort_path(bundle, node, key).filter(|p| *p != [identity.clone()])
        {
            // Largest first when the key or `reverse` asks it (not both); a record
            // without the value last either way.
            let desc = *desc != self.reverse;
            let value = |item: &Item| at_path(&item.data, &path).filter(|v| !v.is_null()).cloned();
            kept.sort_by(|a, b| sort_order(value(a).as_ref(), value(b).as_ref(), desc));
        } else if self.reverse {
            kept.reverse();
        }
        let total = kept.len();
        let at = |cursor: &Option<String>| match cursor {
            Some(cursor) => kept.iter().position(|i| &i.id == cursor).map(Some),
            None => Some(None),
        };
        let after = at(&self.after)?;
        let before = at(&self.before)?;
        let mut start = after.map_or(0, |i| i + 1);
        let mut end = before.unwrap_or(total).max(start);
        // `first` cuts first, then `last` of what it left (Relay's order).
        match self.first {
            Some(n) => end = end.min(start + n),
            None if self.last.is_none() => end = end.min(start + DEFAULT_PAGE),
            None => {}
        }
        if let Some(n) = self.last {
            start = start.max(end.saturating_sub(n));
        }
        let has_previous = start > 0;
        let has_next = end < total;
        Some(Page {
            items: kept.drain(start..end).collect(),
            total,
            has_previous,
            has_next,
        })
    }
}

/// The filter argument `name` asks for: the one `desc` describes, else one naming a
/// scalar member of the node (`props`), or its singular for a list; `None` for any
/// other, and for a described filter with a `grammar`.
fn filter(
    bundle: &Bundle,
    desc: Option<&OpDesc>,
    props: &Map<String, Value>,
    name: &str,
    value: &Value,
) -> Option<Filter> {
    if let Some(described) = desc.and_then(|d| d.filters.iter().find(|f| f.param == name)) {
        return described_filter(described, value);
    }
    let values = match value {
        Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    };
    // A described scope by a member (`eq`) keeps the records whose member holds it; a
    // `prefix` one, or one naming no member, is no test this reads.
    if let Some(scope) = desc
        .and_then(|d| d.scope.as_ref())
        .filter(|scope| scope.param == name)
    {
        let member = scope
            .member
            .clone()
            .filter(|_| scope.kind.is_none_or(|k| k == mb_mcm::world::ScopeKind::Eq))?;
        return Some(Filter {
            member: Some(member),
            test: Test::Eq(values),
        });
    }
    let singular = name.strip_suffix('s').filter(|_| value.is_array());
    let member = [Some(name), singular]
        .into_iter()
        .flatten()
        .find(|m| props.get(*m).is_some_and(|s| is_scalar(s, &bundle.defs)))?;
    Some(Filter {
        member: Some(member.to_string()),
        test: Test::Eq(values),
    })
}

/// The test a described filter makes with `value`.
fn described_filter(f: &FilterDesc, value: &Value) -> Option<Filter> {
    if f.grammar.is_some() {
        return None;
    }
    let member = Some(f.member.clone().unwrap_or_else(|| f.param.clone()));
    let test = match f.kind? {
        FilterKind::Eq => Test::Eq(match value {
            Value::Array(items) => items.clone(),
            other => vec![other.clone()],
        }),
        FilterKind::Contains => Test::Contains(value.clone()),
        FilterKind::Gte => Test::Gte(value.clone()),
        FilterKind::Lte => Test::Lte(value.clone()),
        FilterKind::Flag => Test::Flag(match value {
            Value::Bool(b) => *b,
            Value::String(s) => s.eq_ignore_ascii_case("true"),
            _ => return None,
        }),
        FilterKind::Text => {
            let text = value.as_str()?.to_lowercase();
            return Some(Filter {
                member: f.member.clone(),
                test: Test::Text(text),
            });
        }
    };
    Some(Filter { member, test })
}

impl Filter {
    fn keeps(&self, data: &Value) -> bool {
        let held = self
            .member
            .as_deref()
            .and_then(|m| at_path(data, &dotted(m)));
        match &self.test {
            Test::Text(text) => match held {
                Some(v) => v.as_str().is_some_and(|s| s.to_lowercase().contains(text)),
                None if self.member.is_none() => holds_text(data, text, 0),
                None => false,
            },
            _ if held.is_none_or(Value::is_null) => false,
            Test::Eq(values) => held.is_some_and(|h| values.iter().any(|v| loose_eq(h, v))),
            Test::Contains(want) => match held {
                Some(Value::Array(items)) => items.iter().any(|i| loose_eq(i, want)),
                Some(Value::String(s)) => want.as_str().is_some_and(|w| s.contains(w)),
                _ => false,
            },
            Test::Gte(bound) => {
                held.is_some_and(|h| compare_both(h, bound) != Some(Ordering::Less))
            }
            Test::Lte(bound) => {
                held.is_some_and(|h| compare_both(h, bound) != Some(Ordering::Greater))
            }
            Test::Flag(flag) => held.and_then(Value::as_bool) == Some(*flag),
        }
    }
}

/// Whether a string member of `data` (a level of objects down at most two) holds `text`.
fn holds_text(data: &Value, text: &str, depth: usize) -> bool {
    match data {
        Value::String(s) => s.to_lowercase().contains(text),
        Value::Object(members) if depth <= 2 => {
            members.values().any(|v| holds_text(v, text, depth + 1))
        }
        _ => false,
    }
}

/// `a` and `b` as numbers (a number, or text that reads as one), compared; `None` when
/// either is not, or both are text that do not compare (then by text).
fn compare_both(a: &Value, b: &Value) -> Option<Ordering> {
    match (number(a), number(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y),
        _ => match (a.as_str(), b.as_str()) {
            (Some(x), Some(y)) => Some(x.cmp(y)),
            _ => None,
        },
    }
}

/// Two records' sort values (`None` for one holding none, or `null`): those holding
/// one first, ordered by [`compare`] (reversed when `desc`), then those holding none.
fn sort_order(a: Option<&Value>, b: Option<&Value>, desc: bool) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => {
            let order = compare(x, y);
            if desc { order.reverse() } else { order }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Two values in a sort, a total order: by kind first (numbers, and text that reads as
/// one; then other text; then anything else), numbers by value, text without case and
/// then as written, anything else by its JSON text.
fn compare(a: &Value, b: &Value) -> Ordering {
    let kind = |v: &Value| match (number(v), v) {
        (Some(_), _) => 0,
        (None, Value::String(_)) => 1,
        _ => 2,
    };
    kind(a)
        .cmp(&kind(b))
        .then_with(|| match (number(a), number(b)) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            _ => {
                let (x, y) = (text(a), text(b));
                x.to_lowercase()
                    .cmp(&y.to_lowercase())
                    .then_with(|| x.cmp(&y))
            }
        })
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
        _ => None,
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Equal by value: `"3"` is `3`, and enum-like text compares without case.
fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x == y || x.eq_ignore_ascii_case(y),
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::Array(_) | Value::Object(_), _) | (_, Value::Array(_) | Value::Object(_)) => a == b,
        _ => text(a) == text(b),
    }
}

fn dotted(member: &str) -> Vec<String> {
    member.split('.').map(str::to_string).collect()
}

fn at_path<'v>(data: &'v Value, path: &[String]) -> Option<&'v Value> {
    path.iter().try_fold(data, |v, segment| v.get(segment))
}

/// A name's words, lowercased and joined: `CREATED_AT` and `createdAt` are `createdat`.
fn words(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The member path a sort key names in node type `node` (see the module's doc).
fn sort_path(bundle: &Bundle, node: &str, key: &str) -> Option<Vec<String>> {
    let want = words(key);
    if want.is_empty() {
        return None;
    }
    let props = bundle.defs.get(node)?.get("properties")?.as_object()?;
    let exact = props.iter().find(|(name, _)| words(name) == want);
    let prefix = || {
        props
            .iter()
            .find(|(name, _)| *name != "__typename" && words(name).starts_with(&want))
    };
    let (name, schema) = exact.or_else(prefix)?;
    descend(bundle, vec![name.clone()], schema, 0)
}

/// `path`, ending at a member of `schema`, continued down to a scalar.
fn descend(
    bundle: &Bundle,
    path: Vec<String>,
    schema: &Value,
    depth: usize,
) -> Option<Vec<String>> {
    if list_item(schema).is_some() {
        return None;
    }
    if is_scalar(schema, &bundle.defs) {
        return Some(path);
    }
    if depth >= 3 {
        return None;
    }
    let props = def_of(bundle, schema)?.get("properties")?.as_object()?;
    let rank = |(name, schema): &(&String, &Value)| {
        let w = words(name);
        if w.starts_with("min") {
            0
        } else if w == "amount" || w == "value" {
            1
        } else if matches!(
            schema.get("type").and_then(Value::as_str),
            Some("number" | "integer")
        ) {
            2
        } else {
            3
        }
    };
    let mut members: Vec<(&String, &Value)> = props
        .iter()
        .filter(|(name, _)| *name != "__typename")
        .collect();
    members.sort_by_key(rank);
    members.into_iter().find_map(|(name, schema)| {
        let mut path = path.clone();
        path.push(name.clone());
        descend(bundle, path, schema, depth + 1)
    })
}

/// One page of a connection.
struct Page {
    items: Vec<Item>,
    /// The items kept, on every page.
    total: usize,
    has_previous: bool,
    has_next: bool,
}

/// The object branch of a (nullable) narrowed schema.
fn object_branch(schema: &Value) -> &Value {
    match schema.get("anyOf").and_then(Value::as_array) {
        Some(branches) => branches
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) != Some("null"))
            .unwrap_or(schema),
        None => schema,
    }
}

fn property<'s>(schema: &'s Value, key: &str) -> Option<&'s Value> {
    object_branch(schema).get("properties")?.get(key)
}

fn items_of(schema: &Value) -> Option<&Value> {
    object_branch(schema).get("items")
}

/// The faker's value for the narrowed `schema`, drawn from a seed of `label`.
fn fake(schema: &Value, label: &[&str]) -> Value {
    // A fixed hash: the same record fills alike in every build and process.
    let mut rng = StdRng::seed_from_u64(mb_engine::seed::derive(0, label, &[]));
    Faker::new(MAX_DEPTH + 4)
        .with_hints(mb_engine::FakerHints {
            objects: true,
            ..Default::default()
        })
        .fake(schema, &mut rng)
}

/// `page`, a connection of node type `node`, in the shape `sel` selects (keyed by
/// response key), `schema` its narrowed schema; each node over what `answered` (the
/// connection as answered before, when it was) gave the node of its id.
fn render_connection(
    records: &Records<'_>,
    node: &str,
    page: &Page,
    (sel, answered): (&Selection, Option<&Value>),
    schema: &Value,
    depth: usize,
) -> Value {
    let bases = answered_nodes(records, node, sel, answered);
    let base = |item: &Item, key: &str| bases.get(&(key.to_string(), item.id.clone()));
    let first_id = page.items.first().map_or("", |i| i.id.as_str());
    let mut out = match fake(schema, &[node, "connection", first_id]) {
        Value::Object(obj) => obj,
        _ => Map::new(),
    };
    let cursor = |item: Option<&Item>| item.map_or(Value::Null, |i| json!(i.id));
    for (key, child) in &sel.fields {
        match child.name.as_str() {
            "nodes" => {
                let item_schema = property(schema, key).and_then(items_of);
                let nodes = page
                    .items
                    .iter()
                    .map(|item| {
                        let base = base(item, key);
                        render_node(records, node, item, (child, base), item_schema, depth)
                    })
                    .collect();
                out.insert(key.clone(), Value::Array(nodes));
            }
            "edges" => {
                let edge_schema = property(schema, key).and_then(items_of);
                let edges = page
                    .items
                    .iter()
                    .map(|item| {
                        let mut edge = match edge_schema.map(|s| fake(s, &[node, "edge", &item.id]))
                        {
                            Some(Value::Object(edge)) => edge,
                            _ => Map::new(),
                        };
                        for (edge_key, edge_sel) in &child.fields {
                            match edge_sel.name.as_str() {
                                "cursor" => {
                                    edge.insert(edge_key.clone(), json!(item.id));
                                }
                                "node" => {
                                    let node_schema =
                                        edge_schema.and_then(|s| property(s, edge_key));
                                    let base = base(item, &format!("{key}.{edge_key}"));
                                    let rendered = render_node(
                                        records,
                                        node,
                                        item,
                                        (edge_sel, base),
                                        node_schema,
                                        depth,
                                    );
                                    edge.insert(edge_key.clone(), rendered);
                                }
                                _ => {}
                            }
                        }
                        Value::Object(edge)
                    })
                    .collect();
                out.insert(key.clone(), Value::Array(edges));
            }
            "pageInfo" => {
                let mut info = match out.remove(key) {
                    Some(Value::Object(info)) => info,
                    _ => Map::new(),
                };
                for (info_key, info_sel) in &child.fields {
                    let value = match info_sel.name.as_str() {
                        "hasNextPage" => json!(page.has_next),
                        "hasPreviousPage" => json!(page.has_previous),
                        "startCursor" => cursor(page.items.first()),
                        "endCursor" => cursor(page.items.last()),
                        _ => continue,
                    };
                    info.insert(info_key.clone(), value);
                }
                out.insert(key.clone(), Value::Object(info));
            }
            "totalCount" => {
                out.insert(key.clone(), json!(page.total));
            }
            _ => {}
        }
    }
    Value::Object(out)
}

/// The nodes of a connection of node type `node` as `answered` (keyed by `sel`'s
/// response keys) holds them, by the response key path they sit under (`nodes`,
/// `edges.node`) and their id.
fn answered_nodes(
    records: &Records<'_>,
    node: &str,
    sel: &Selection,
    answered: Option<&Value>,
) -> HashMap<(String, String), Value> {
    let mut out = HashMap::new();
    let Some(Value::Object(answered)) = answered else {
        return out;
    };
    let identity = identity_member(records.rt, &node.to_lowercase());
    let id_of = |node: &Value, node_sel: &Selection| {
        node_sel
            .fields
            .iter()
            .find(|(_, c)| c.name == identity)
            .and_then(|(k, _)| node.get(k))
            .or_else(|| node.get(&identity))
            .and_then(id_text)
    };
    for (key, child) in &sel.fields {
        let Some(Value::Array(listed)) = answered.get(key) else {
            continue;
        };
        match child.name.as_str() {
            "nodes" => {
                for v in listed {
                    if let Some(id) = id_of(v, child) {
                        out.insert((key.clone(), id), v.clone());
                    }
                }
            }
            "edges" => {
                for (edge_key, edge_sel) in child.fields.iter().filter(|(_, c)| c.name == "node") {
                    for v in listed.iter().filter_map(|e| e.get(edge_key)) {
                        if let Some(id) = id_of(v, edge_sel) {
                            out.insert((format!("{key}.{edge_key}"), id), v.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// A validator of each node place of a connection selected by `sel` (narrowed to
/// `schema`), by the response key path [`answered_nodes`] names it by.
fn node_validators(sel: &Selection, schema: &Value) -> HashMap<String, mb_engine::SchemaValidator> {
    let mut out = HashMap::new();
    for (key, child) in &sel.fields {
        let items = property(schema, key).and_then(items_of);
        let places: Vec<(String, Option<&Value>)> = match child.name.as_str() {
            "nodes" => vec![(key.clone(), items)],
            "edges" => child
                .fields
                .iter()
                .filter(|(_, c)| c.name == "node")
                .map(|(edge_key, _)| {
                    let node = items.and_then(|s| property(s, edge_key));
                    (format!("{key}.{edge_key}"), node)
                })
                .collect(),
            _ => Vec::new(),
        };
        for (path, node) in places {
            if let Some(validator) = node.and_then(|s| mb_engine::SchemaValidator::new(s).ok()) {
                out.insert(path, validator);
            }
        }
    }
    out
}

/// Whether `answered` (a connection of node type `node`, keyed by `sel`'s response
/// keys) shows the records `items` and no others, each selected scalar of a node the
/// session holds as its record holds it.
fn stands(
    records: &Records<'_>,
    node: &str,
    sel: &Selection,
    answered: Option<&Value>,
    items: &[Item],
) -> bool {
    let nodes = answered_nodes(records, node, sel, answered);
    let shown: BTreeSet<&str> = nodes.keys().map(|(_, id)| id.as_str()).collect();
    let held: BTreeSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
    if shown != held {
        return false;
    }
    let t = node.to_lowercase();
    let node_sel = |path: &str| -> Option<&Selection> {
        let mut parts = path.split('.');
        let mut at = sel.fields.get(parts.next()?)?;
        for part in parts {
            at = at.fields.get(part)?;
        }
        Some(at)
    };
    nodes.iter().all(|((path, id), shown)| {
        let (Some(record), Some(node_sel)) = (records.get(&t, id), node_sel(path)) else {
            return true;
        };
        node_sel
            .fields
            .iter()
            .filter(|(_, c)| c.fields.is_empty())
            .all(|(key, c)| match record.get(&c.name) {
                Some(held) if !held.is_null() && !held.is_object() && !held.is_array() => shown
                    .get(key)
                    .is_some_and(|v| v == held || loose_eq(v, held)),
                _ => true,
            })
    })
}

/// `item`, a record of node type `node`, in the shape `sel` selects: its members over
/// `base` (the node as answered before, keyed by response key, when it was) over the
/// faker's for `schema`, its id under every key selecting the identity, and its
/// connections answered from what it holds.
fn render_node(
    records: &Records<'_>,
    node: &str,
    item: &Item,
    (sel, base): (&Selection, Option<&Value>),
    schema: Option<&Value>,
    depth: usize,
) -> Value {
    if item.as_answered
        && let Some(base) = base
    {
        return base.clone();
    }
    let mut out = schema.map_or(Value::Object(Map::new()), |s| fake(s, &[node, &item.id]));
    if let Some(base) = base {
        out = mb_engine::merge_stored(base, out);
    }
    let stored = rekey(item.data.clone(), sel);
    let mut out = mb_engine::merge_stored(&stored, out);
    let identity = identity_member(records.rt, &node.to_lowercase());
    if let Value::Object(obj) = &mut out {
        for (key, _) in sel.fields.iter().filter(|(_, c)| c.name == identity) {
            obj.insert(key.clone(), json!(item.id));
        }
    }
    if let Some(schema) = schema {
        connections_below(
            records,
            (node, &item.id, false),
            &item.data,
            sel,
            schema,
            &mut out,
            depth + 1,
        );
    }
    out
}

/// Each connection `sel` selects of `data`, the record `parent_id` of object type `def`
/// (`out` its answer, keyed by response key, `generated` unless served from the record,
/// `schema` the narrowed schema of it), that the record holds items of and no held
/// record of the node's type refers to it: answered from those items over what `out`
/// answered (see the module's doc).
fn connections_below(
    records: &Records<'_>,
    (def, parent_id, generated): (&str, &str, bool),
    data: &Value,
    sel: &Selection,
    schema: &Value,
    out: &mut Value,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let bundle = records.bundle();
    let Some(props) = bundle
        .defs
        .get(def)
        .and_then(|d| d.get("properties"))
        .and_then(Value::as_object)
    else {
        return;
    };
    let Value::Object(obj) = out else {
        return;
    };
    for (key, child) in &sel.fields {
        let Some(field) = child.names().find_map(|n| props.get(n)) else {
            continue;
        };
        let Some(node) = connection_node(bundle, records.rt, field) else {
            continue;
        };
        // Records that name the parent are what lists under it (see `nested`).
        if records.refer_to(&node, parent_id) {
            continue;
        }
        let Some(held) = child.names().find_map(|n| data.get(n)) else {
            continue;
        };
        let Some(mut items) = held_items(records, &node, held, &child.args) else {
            continue;
        };
        let Some(child_schema) = property(schema, key) else {
            continue;
        };
        // A generated answer showing the very records the parent holds there, as the
        // session holds them, is the answer the parent was stored from (this call or
        // before): it stands, when it is whole.
        if generated
            && stands(records, &node, child, obj.get(key), &items)
            && obj.get(key).is_some_and(|answered| {
                mb_engine::SchemaValidator::new(child_schema).is_ok_and(|v| v.is_valid(answered))
            })
        {
            continue;
        }
        // Below a generated answer, an item the session holds no record of stays as the
        // answer showed it, where it showed it whole, and is left out otherwise: what
        // the answer stores is never drawn here.
        if generated {
            let t = node.to_lowercase();
            let shown = answered_nodes(records, &node, child, obj.get(key));
            let fits = node_validators(child, child_schema);
            items.retain_mut(|item| {
                item.as_answered = records.get(&t, &item.id).is_none();
                if !item.as_answered {
                    return true;
                }
                let mut at = shown
                    .iter()
                    .filter(|((_, id), _)| *id == item.id)
                    .peekable();
                at.peek().is_some()
                    && at.all(|((path, _), node)| {
                        fits.get(path.as_str()).is_some_and(|v| v.is_valid(node))
                    })
            });
        }
        let Some(page) = Ask::of(records.rt, None, &node, &child.args)
            .and_then(|ask| ask.page(bundle, records.rt, &node, items))
        else {
            continue;
        };
        let rendered = render_connection(
            records,
            &node,
            &page,
            (child, obj.get(key)),
            child_schema,
            depth,
        );
        obj.insert(key.clone(), rendered);
    }
}

/// The items a record's connection member `held` holds (`edges[].node`, `nodes`, or a
/// plain list), each the session's record of `node` under its id where it holds one
/// (that record's members over the item's); `None` when it holds no list, or an item
/// without an id.
fn held_items(
    records: &Records<'_>,
    node: &str,
    held: &Value,
    args: &Map<String, Value>,
) -> Option<Vec<Item>> {
    let listed: Vec<&Value> = match held {
        Value::Array(items) => items.iter().collect(),
        Value::Object(conn) => match (conn.get("edges"), conn.get("nodes")) {
            (Some(Value::Array(edges)), _) if !edges.is_empty() => {
                edges.iter().filter_map(|e| e.get("node")).collect()
            }
            (_, Some(Value::Array(nodes))) => nodes.iter().collect(),
            (Some(Value::Array(_)), None) => Vec::new(),
            _ => return None,
        },
        _ => return None,
    };
    let t = node.to_lowercase();
    let member = identity_member(records.rt, &t);
    let items: Option<Vec<Item>> = listed
        .into_iter()
        .map(|item| {
            let id = item.get(&member).and_then(id_text)?;
            let data = match records.get(&t, &id) {
                Some(record) => mb_engine::merge_stored(record, item.clone()),
                None => item.clone(),
            };
            Some(Item {
                id,
                data,
                as_answered: false,
            })
        })
        .collect();
    // A soft-deleted record is left out, as a REST list leaves it out.
    let mut items = items?;
    items.retain(|i| !call::graphql_reads::soft_deleted(records.rt, &t, &i.data, args));
    Some(items)
}

/// The `NOT_FOUND` error of a lookup of a `def` (a definition name, `Product`) by
/// argument `arg` at `value` that names no record: the field answers `null` beside it.
pub(crate) fn not_found(def: &str, arg: &str, value: &Value) -> Failure {
    Failure::new(
        404,
        format!(
            "Could not resolve to a {def} with the {arg} of '{}'.",
            text(value)
        ),
    )
}

/// The `NOT_FOUND` answer of a lookup of an object of type `def` (a definition name)
/// by `by`, each an argument naming a member of the type with its value, when
/// `unknown_ids` resolves to 404 for the type ([`call::graphql_reads::answers_404`]) and the session holds
/// no record of it (nor the mocks it shares with, as a read of the type finds theirs)
/// whose member holds any of those values (text compared without case); the identity
/// member also matches a record's id. `None` when such a record is held, the type
/// holds more records than are scanned, or `unknown_ids` generates. Usable for any
/// lookup by arguments, below a root field too.
pub(crate) async fn unknown_lookup(
    rt: &MockRuntime,
    session: &SessionKey,
    def: &str,
    by: &[(&str, &Value)],
) -> Option<Failure> {
    let (arg, value) = *by.first()?;
    let t = def.to_lowercase();
    if !call::graphql_reads::answers_404(rt, session, &t).await {
        return None;
    }
    let member = identity_member(rt, &t);
    let named = |id: &str, data: &Value| {
        by.iter().any(|(name, value)| {
            (*name == member && id_text(value).as_deref() == Some(id))
                || data.get(*name).is_some_and(|held| loose_eq(held, value))
        })
    };
    for (name, value) in by {
        if *name == member
            && let Some(id) = id_text(value)
            && call::stored(rt, session, &t, &id).await.is_some()
        {
            return None;
        }
    }
    let own = call::stored_of_type(rt, session, &t, MAX_RECORDS + 1).await;
    if own.len() > MAX_RECORDS {
        return None;
    }
    if own.iter().any(|r| named(&r.id, &r.data)) {
        return None;
    }
    let props = rt
        .bundle
        .defs
        .get(def)
        .and_then(|d| d.get("properties"))
        .and_then(Value::as_object);
    if let Some(props) = props {
        for other in call::shared::equivalents(rt, &t) {
            let theirs = call::stored_with_parents(rt, session, &other, MAX_RECORDS + 1).await;
            if theirs.len() > MAX_RECORDS {
                return None;
            }
            for (_, record) in theirs {
                let data = call::shared::reshape(props, &rt.bundle.defs, &record.data);
                let id = data
                    .get(&member)
                    .and_then(id_text)
                    .unwrap_or_else(|| record.id.clone());
                if named(&id, &data) {
                    return None;
                }
            }
        }
    }
    Some(not_found(def, arg, value))
}

/// [`unknown_lookup`] for root query `op`, read with `args`: the arguments naming its
/// record are its identity's input field, the world's target member, the type's
/// described alternates and its identity member, each one the type declares, sent with
/// a scalar value.
async fn lookup(
    rt: &MockRuntime,
    session: &SessionKey,
    op: &Operation,
    args: &Map<String, Value>,
) -> Option<Failure> {
    let identity = op.identity.as_ref()?;
    let schema = op.success_output()?.schema.as_ref()?;
    let def = def_ref(schema)?;
    let node = rt.bundle.defs.get(def)?;
    if node.get("oneOf").is_some() {
        return None;
    }
    let props = node.get("properties")?.as_object()?;
    let t = def.to_lowercase();
    let mut names: Vec<String> = Vec::new();
    names.extend(identity.input_field.clone());
    names.extend(
        rt.described()
            .op(&op.id)
            .and_then(|d| d.target.as_ref())
            .and_then(|target| target.member.clone()),
    );
    names.extend(
        rt.described()
            .described_alternates(&t)
            .unwrap_or_default()
            .iter()
            .cloned(),
    );
    // A lookup by the identity itself keeps its answer: generated, or the 404 of a
    // record the session deleted.
    let member = identity_member(rt, &t);
    names.retain(|n| *n != member);
    let given = |value: &Value| {
        matches!(value, Value::String(s) if !s.is_empty()) || matches!(value, Value::Number(_))
    };
    let mut by: Vec<(&str, &Value)> = args
        .iter()
        .filter(|(name, value)| {
            names.iter().any(|n| n == *name) && props.contains_key(name.as_str()) && given(value)
        })
        .map(|(name, value)| (name.as_str(), value))
        .collect();
    // No described key: the arguments naming a scalar member of the type, as below the
    // root (`repository(owner:, name:)` → `name`; `owner` is an object).
    if by.is_empty() && !args.get(&member).is_some_and(given) {
        let defs = &rt.bundle.defs;
        by = args
            .iter()
            .filter(|(name, value)| {
                **name != member
                    && given(value)
                    && props
                        .get(name.as_str())
                        .is_some_and(|p| is_scalar(p, defs) && !super::select::is_enum(p, defs))
            })
            .map(|(name, value)| (name.as_str(), value))
            .collect();
    }
    unknown_lookup(rt, session, def, &by).await
}

/// A lookup below the root field, keyed by value (a [`super::select::lookup_arguments`]
/// field returning one nullable object: `user(login:) { repository(name: "nope") }`).
struct NestedLookup {
    /// Response keys and list indexes from the root field's answer to the field.
    path: Vec<String>,
    /// The definition name of the object it returns.
    def: String,
    by: Vec<(String, Value)>,
}

/// Bounds [`nested_lookups`] (recursive types).
const MAX_NESTED_DEPTH: usize = 12;

/// The nested lookups `value`, an answer to `sel` for an object of `schema` keyed by
/// response key, answers with an object, outermost first.
fn nested_lookups(
    bundle: &Bundle,
    schema: &Value,
    sel: &Selection,
    value: &Value,
    path: &mut Vec<String>,
    out: &mut Vec<NestedLookup>,
    depth: usize,
) {
    if depth > MAX_NESTED_DEPTH || sel.fields.is_empty() {
        return;
    }
    match value {
        Value::Array(items) => {
            if let Some(item) = list_item(schema) {
                for (i, v) in items.iter().enumerate() {
                    path.push(i.to_string());
                    nested_lookups(bundle, item, sel, v, path, out, depth + 1);
                    path.pop();
                }
            }
        }
        Value::Object(obj) => {
            let Some((_, def)) = super::state::object_type(bundle, schema, obj) else {
                return;
            };
            let Some(props) = def.get("properties").and_then(Value::as_object) else {
                return;
            };
            for (key, child) in &sel.fields {
                let (Some(member), Some(field)) = (obj.get(key), props.get(&child.name)) else {
                    continue;
                };
                path.push(key.clone());
                if member.is_object()
                    && nullable(field)
                    && let Some(def) = def_ref(field)
                {
                    let by: Vec<(String, Value)> =
                        super::select::lookup_arguments(field, &bundle.defs, child)
                            .into_iter()
                            .filter_map(|arg| {
                                let value = child.args.get(arg)?;
                                (matches!(value, Value::String(s) if !s.is_empty())
                                    || value.is_number())
                                .then(|| (arg.to_string(), value.clone()))
                            })
                            .collect();
                    if !by.is_empty() {
                        out.push(NestedLookup {
                            path: path.clone(),
                            def: def.to_string(),
                            by,
                        });
                    }
                }
                nested_lookups(bundle, field, child, member, path, out, depth + 1);
                path.pop();
            }
        }
        _ => {}
    }
}

/// Whether a member's schema allows `null` (a nullable GraphQL field).
fn nullable(schema: &Value) -> bool {
    schema
        .get("anyOf")
        .and_then(Value::as_array)
        .is_some_and(|branches| {
            branches
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("null"))
        })
}

/// The lookups below `plan`'s root field (`call`, selected by `sel`) that `body`, its
/// answer keyed by response key, answers with a record the session does not hold
/// ([`unknown_lookup`]: `user(login: "a") { repository(name: "nope") }` under
/// `unknown_ids: 404`): each is set to `null` in `body` and returned with its
/// `NOT_FOUND`, by its path below the root field (`repository`, `items.0.owner`). A
/// lookup inside one already missing is not looked at. Nothing for a query a
/// scenario, seed header or forced status answers.
pub(crate) async fn nested_missing(
    rt: &MockRuntime,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    sel: &Selection,
    body: &mut Value,
) -> Vec<(String, Failure)> {
    if !plain_query(rt, plan, call) {
        return Vec::new();
    }
    let Some(schema) = plan.op.success_output().and_then(|o| o.schema.as_ref()) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    nested_lookups(
        &rt.bundle,
        schema,
        sel,
        body,
        &mut Vec::new(),
        &mut found,
        0,
    );
    let mut missing: Vec<(Vec<String>, Failure)> = Vec::new();
    // Every item of a list asks the same lookup: decided once.
    let mut decided: HashMap<(String, String), bool> = HashMap::new();
    for lookup in found {
        if missing.iter().any(|(p, _)| lookup.path.starts_with(p)) {
            continue;
        }
        let by: Vec<(&str, &Value)> = lookup.by.iter().map(|(a, v)| (a.as_str(), v)).collect();
        let asked = (lookup.def.clone(), json!(lookup.by).to_string());
        let unknown = match decided.get(&asked) {
            Some(unknown) => *unknown,
            None => {
                let unknown = unknown_lookup(rt, &plan.session, &lookup.def, &by)
                    .await
                    .is_some();
                decided.insert(asked, unknown);
                unknown
            }
        };
        if unknown {
            let (arg, value) = by[0];
            missing.push((lookup.path, not_found(&lookup.def, arg, value)));
        }
    }
    for (path, _) in &missing {
        let pointer: String = path
            .iter()
            .map(|s| format!("/{}", s.replace('~', "~0").replace('/', "~1")))
            .collect();
        if let Some(member) = body.pointer_mut(&pointer) {
            *member = Value::Null;
        }
    }
    missing
        .into_iter()
        .map(|(path, failure)| (path.join("."), failure))
        .collect()
}

/// The objects of query `op`'s generated answer (`placed`, each with whether it sits
/// inside a list) the session is seeded with. A model's answer seeds them all. The
/// faker's (no model, [`call::graphql_reads::no_model`]) seeds the list items it drew
/// (of a root list or connection, or below a lookup alike) only of a type the session
/// holds no other record of: in an empty session a catalog list is seeded, so a read of
/// what it showed agrees with it, while invented items never join records the session
/// already lists from state. What the answer itself holds does not count as held, so
/// the call's own earlier writes (the engine's seeding of the same answer) decide
/// nothing. Objects outside lists are seeded as before.
pub(crate) async fn seedable(
    rt: &MockRuntime,
    session: &SessionKey,
    generated: &GeneratedResponse,
    placed: Vec<(mb_state::StoredResource, bool)>,
) -> Vec<mb_state::StoredResource> {
    if !call::graphql_reads::no_model(generated) {
        return placed.into_iter().map(|(r, _)| r).collect();
    }
    let mut own: HashMap<String, BTreeSet<String>> = HashMap::new();
    for (record, _) in &placed {
        own.entry(record.resource_type.clone())
            .or_default()
            .insert(record.id.clone());
    }
    let mut holds: HashMap<String, bool> = HashMap::new();
    let mut out = Vec::new();
    for (record, listed) in placed {
        if !listed {
            out.push(record);
            continue;
        }
        let t = record.resource_type.clone();
        let held = match holds.get(&t) {
            Some(held) => *held,
            None => {
                let ids = own.get(&t).map_or(0, BTreeSet::len);
                let held = call::stored_of_type(rt, session, &t, ids + 1)
                    .await
                    .iter()
                    .any(|r| !own.get(&t).is_some_and(|ids| ids.contains(&r.id)));
                holds.insert(t, held);
                held
            }
        };
        if !held {
            out.push(record);
        }
    }
    out
}

/// The keyed item type (its definition name) of the list or connection `schema` is,
/// when it is one of those a root field answers from state.
pub(crate) fn listed_node(rt: &MockRuntime, schema: &Value) -> Option<String> {
    connection_node(&rt.bundle, rt, schema).or_else(|| list_node(&rt.bundle, rt, schema))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_join_a_names_parts() {
        assert_eq!(words("CREATED_AT"), "createdat");
        assert_eq!(words("createdAt"), words("CREATED_AT"));
    }

    #[test]
    fn numbers_in_text_compare_as_numbers() {
        assert_eq!(compare(&json!("12.5"), &json!("3")), Ordering::Greater);
        assert_eq!(compare(&json!("apple"), &json!("Banana")), Ordering::Less);
        assert_eq!(compare(&json!(2), &json!("10")), Ordering::Less);
    }

    #[test]
    fn mixed_values_sort_in_a_total_order() {
        // "9" < "10" as numbers, "10" < "1a" as text would make a cycle: numbers come
        // before other text whatever their digits.
        let values = [
            json!("1a"),
            json!("10"),
            json!(true),
            json!("9"),
            json!("B"),
            json!("b"),
            json!(2.5),
            json!({"x": 1}),
        ];
        for a in &values {
            assert_eq!(compare(a, a), Ordering::Equal, "{a}");
            for b in &values {
                assert_eq!(compare(a, b), compare(b, a).reverse(), "{a} {b}");
                for c in &values {
                    if compare(a, b) != Ordering::Greater && compare(b, c) != Ordering::Greater {
                        assert_ne!(compare(a, c), Ordering::Greater, "{a} {b} {c}");
                    }
                }
            }
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(compare);
        assert_eq!(
            sorted,
            [
                json!(2.5),
                json!("9"),
                json!("10"),
                json!("1a"),
                json!("B"),
                json!("b"),
                json!(true),
                json!({"x": 1})
            ]
        );
    }

    #[test]
    fn a_record_without_the_value_sorts_last_both_ways() {
        let (one, two) = (json!(1), json!(2));
        let mut held = vec![None, Some(&two), Some(&one)];
        held.sort_by(|a, b| sort_order(*a, *b, false));
        assert_eq!(held, [Some(&one), Some(&two), None]);
        held.sort_by(|a, b| sort_order(*a, *b, true));
        assert_eq!(held, [Some(&two), Some(&one), None]);
    }

    #[test]
    fn blank_values_ask_nothing() {
        assert!(asks_nothing(&json!(null)));
        assert!(asks_nothing(&json!(" ")));
        assert!(asks_nothing(&json!([])));
        assert!(!asks_nothing(&json!("x")));
        assert!(!asks_nothing(&json!(false)));
    }
}
