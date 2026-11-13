//! A parent's embedded child list and its child records as one set of facts.
//!
//! A record may embed records of another type that the contract also serves on their
//! own: a TMDB show lists its `seasons`, each one `/tv/{id}/season/{season_number}`
//! reads; a Stripe subscription holds its `items` as a list object
//! (`{object: "list", data: [...], has_more, url}`) whose `url` names the route that
//! lists them. Generated apart, the two contradict each other: the show lists season
//! `14` with id `66222`, and `/season/1` answers id `76941`. So where the contract binds
//! an embed to a child type ([`ChildEmbeds::of`]), the child records are the facts and
//! the embed is drawn from them:
//!
//! - storing or reading the parent stores each child its embed lists that the session
//!   does not hold yet, under the parent ([`sync`]); a held child is never overwritten;
//! - a read of a child the session does not hold, which a held parent embeds, finds
//!   it there: it is stored, and the read serves it ([`settle_target`]; what it lacks
//!   of its own read's schema is completed as any held record is, [`super::exec`]);
//! - a parent's answer draws its embed from the children held under it, in the
//!   embed's order then the rest, each projected onto the embed's item schema: a
//!   plain array lists them all (at most the schema's `maxItems`, else
//!   [`MAX_PLAIN`]); a list object holds its first page (its `data`'s `maxItems`,
//!   else Stripe's default [`LIST_PAGE`]), `has_more` when more are held, `total_count`
//!   when it has one, and a `url` naming the child list route for this parent;
//! - a child written or generated is added to its held parent's embed at once, with
//!   the parent stored only when it is still as it was read (a compare and set, as
//!   [`super::exec`]'s server-set references are), so a write another call made
//!   meanwhile is never undone; a deleted one leaves it;
//! - a plain read of the child list route for a parent (no filter or cursor but the
//!   parent's, a `limit` at most) answers the children held under it, at most a page;
//! - children stored from a parent's answer or a child list the faker made
//!   ([`FillerChildren`], [`note_listed`]) are filler: once a child of that parent is
//!   held that is not (one read or written on its own route, or stored from any other
//!   answer), the filler leaves the parent's embed and its child list, so the parent
//!   lists the children the session read, not invented ones.
//!
//! An embed binds when (all of):
//! - it is a top-level member of the parent's own read (by path), an array or a list
//!   object holding `data` items;
//! - its items are the child type's records: their `$ref` is one of the child read's
//!   definitions, or, written inline, they declare the child's key and identity, and at
//!   least half of what they declare the child's record declares too;
//! - the child is served under the parent: it is parent-scoped under the parent's own
//!   read path ([`super::scopes`]), or its records name the parent in a member (one
//!   named for the parent's path parameter or type) that a child route under the
//!   parent's path, or a child list's query parameter of that name, narrows by;
//! - exactly one child type so binds the member (two would be a guess), and the
//!   description does not hold that type inside the member (`parent.embedded`, whose
//!   items have no records of their own).
//!
//! A shape alone binds only a member named for the child type (`seasons` of
//! `season`): GitHub's gist `forks` look like its `comments`, and are not.

use std::collections::{HashMap, HashSet};

use mb_mcm::{Binding, Bundle, Effect, HttpMethod, Operation, OperationKind};
use mb_state::{SessionKey, StoredResource};
use serde_json::{Map, Value};

use super::bundle_index::schema_parts;
use super::scopes::ParentScopes;
use super::{CallPlan, MockRuntime, OperationCall, StateAccess, id_string};

/// Items a plain-array embed lists at most, when its schema sets no `maxItems`.
pub(crate) const MAX_PLAIN: usize = 100;

/// A list object's page size when its `data` sets no `maxItems`: Stripe's default.
pub(crate) const LIST_PAGE: usize = 10;

/// Children of one parent read to draw its embed, at most.
const MAX_CHILDREN: usize = 1_000;

/// Held parents scanned for the one embedding a child, when nothing names it.
const MAX_PARENTS: usize = 200;

/// Filler children [`FillerChildren`] holds at most; it is emptied when full.
const MAX_FILLER: usize = 4096;

/// The children stored from a parent's embed or a child list that the faker made (no
/// model wrote it, no request sent it): filler, which a held child that is not filler
/// replaces in the parent's embed and child list. A child is no longer filler once a
/// call on its own route reads or writes it. Only a hint: after a restart, on another node or once
/// emptied, every child counts as real and is kept.
#[derive(Debug, Default)]
pub(crate) struct FillerChildren(std::sync::Mutex<HashSet<Filler>>);

/// A filler child: its type, its parent (the scope values of a scoped child, else the
/// parent's id) and its key there.
#[derive(Debug, PartialEq, Eq, Hash)]
struct Filler {
    session: SessionKey,
    child: String,
    parent: String,
    key: String,
}

impl FillerChildren {
    fn note(&self, fillers: Vec<Filler>) {
        let Ok(mut all) = self.0.lock() else {
            return;
        };
        if all.len() + fillers.len() > MAX_FILLER {
            all.clear();
        }
        all.extend(fillers);
    }

    fn backed(&self, filler: &Filler) {
        if let Ok(mut all) = self.0.lock() {
            all.remove(filler);
        }
    }

    fn holds(&self, filler: &Filler) -> bool {
        self.0.lock().is_ok_and(|all| all.contains(filler))
    }

    fn any_of(&self, s: &SessionKey, child: &str) -> bool {
        self.0
            .lock()
            .is_ok_and(|all| all.iter().any(|f| f.child == child && &f.session == s))
    }
}

/// The parent a filler child of `embed` is noted under: the scope values of a scoped
/// child, else the parent's id.
fn filler_parent(embed: &Embed, parent_id: &str, values: Option<&[String]>) -> String {
    match (&embed.link, values) {
        (Link::Scoped, Some(values)) => values.join(&super::scopes::SEP.to_string()),
        _ => parent_id.to_string(),
    }
}

/// The note of `embed`'s child of key `key` under `parent`, as filler.
fn filler(
    s: &SessionKey,
    embed: &Embed,
    (parent_id, values): (&str, Option<&[String]>),
    key: String,
) -> Filler {
    Filler {
        session: s.clone(),
        child: embed.child.clone(),
        parent: filler_parent(embed, parent_id, values),
        key,
    }
}

/// How a child's records name their parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Link {
    /// Stored under the parent's path values ([`super::scopes`]).
    Scoped,
    /// A member of the child holds the parent's id.
    Member(String),
}

/// Where the children of one parent are listed, for a list object's `url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListRoute {
    /// A route under the parent's path; its parameters are the parent's.
    Path(String),
    /// A route narrowed by a query parameter holding the parent's id.
    Query(String, String),
}

/// An embed of a parent's member bound to a child type.
#[derive(Debug, Clone)]
pub(crate) struct Embed {
    /// The parent type.
    pub(crate) parent: String,
    /// The parent's member holding the embed.
    pub(crate) member: String,
    /// Whether the member is a list object (`{object, data, has_more, url}`).
    pub(crate) list: bool,
    /// The child type.
    pub(crate) child: String,
    /// The member an embedded item and a child record are matched by: the key the
    /// child's own route reads it by (`season_number`, `id`).
    pub(crate) key: String,
    /// The child's identity member, which it is stored under.
    pub(crate) id_field: String,
    /// How the child names its parent.
    pub(crate) link: Link,
    /// The members the embed's items declare; empty when it declares none.
    item_members: Vec<String>,
    /// Items the embed holds at most: a list object's page, a plain array's cap.
    pub(crate) page: usize,
    /// Whether the list object declares `total_count`.
    total_count: bool,
    /// The parent's own read path.
    pub(crate) parent_path: String,
    /// The member the parent's own read names it by (its key under its own parent, or
    /// its identity).
    parent_key: String,
    /// Where the parent's children are listed, when a route does.
    pub(crate) list_route: Option<ListRoute>,
    /// Whether the items were matched by definition, not by their shape.
    pub(crate) by_def: bool,
}

/// The embeds a contract binds (see the module's doc).
#[derive(Debug, Default)]
pub(crate) struct ChildEmbeds {
    by_parent: HashMap<String, Vec<Embed>>,
}

/// What a child type offers for an embed to bind to.
struct Child<'a> {
    t: &'a str,
    defs: HashSet<String>,
    members: HashSet<String>,
    key: String,
    id_field: String,
}

impl ChildEmbeds {
    /// The embeds `bundle` binds, given its detail reads, the types of its definitions,
    /// the record schemas of its enveloped reads and its parent-scoped types.
    pub(crate) fn of(
        bundle: &Bundle,
        detail_reads: &HashMap<String, usize>,
        types_by_def: &HashMap<String, String>,
        record_schemas: &HashMap<String, Value>,
        scopes: &ParentScopes,
    ) -> Self {
        let record = |t: &str| -> Option<(&Operation, Value)> {
            let op = bundle.operations.get(*detail_reads.get(t)?)?;
            let schema = record_schemas
                .get(t)
                .cloned()
                .or_else(|| op.success_output()?.schema.clone())?;
            Some((op, schema))
        };
        let mut children: Vec<Child<'_>> = Vec::new();
        for t in detail_reads.keys() {
            let Some((op, schema)) = record(t) else {
                continue;
            };
            let Some(identity) = op.identity.as_ref() else {
                continue;
            };
            let (parts, defs) = schema_parts(bundle, Some(&schema));
            let mut own_defs: HashSet<String> = defs.iter().map(|d| (*d).to_string()).collect();
            own_defs.extend(
                types_by_def
                    .iter()
                    .filter(|(_, ty)| *ty == t)
                    .map(|(d, _)| d.clone()),
            );
            let key = scopes
                .get(t)
                .map_or_else(|| identity.path_member(), |s| s.key_member())
                .to_string();
            children.push(Child {
                t,
                defs: own_defs,
                members: members_of(&parts),
                key,
                id_field: identity.field.clone(),
            });
        }
        children.sort_by(|a, b| a.t.cmp(b.t));
        let gets: Vec<(&Operation, &str)> = bundle
            .operations
            .iter()
            .filter_map(|op| match &op.binding {
                Binding::Http {
                    method: HttpMethod::Get,
                    path,
                } if op.kind == OperationKind::Query => Some((op, path.as_str())),
                _ => None,
            })
            .collect();
        let mut by_parent: HashMap<String, Vec<Embed>> = HashMap::new();
        let mut parents: Vec<&String> = detail_reads.keys().collect();
        parents.sort();
        for p in parents {
            let Some((op, schema)) = record(p) else {
                continue;
            };
            let Binding::Http { path: p_path, .. } = &op.binding else {
                continue;
            };
            let Some(parent_key) = scopes
                .get(p)
                .map(|s| s.key_member().to_string())
                .or_else(|| Some(op.identity.as_ref()?.path_member().to_string()))
            else {
                continue;
            };
            let (parts, _) = schema_parts(bundle, Some(&schema));
            let mut names: Vec<(&String, &Value)> = parts
                .iter()
                .filter_map(|part| part.get("properties").and_then(Value::as_object))
                .flatten()
                .collect();
            names.sort_by(|a, b| a.0.cmp(b.0));
            names.dedup_by(|a, b| a.0 == b.0);
            for (member, member_schema) in names {
                let Some(shape) = embed_shape(bundle, member_schema) else {
                    continue;
                };
                let (item_parts, item_defs) = schema_parts(bundle, Some(&shape.items));
                let item_members = members_of(&item_parts);
                let mut found: Vec<Embed> = Vec::new();
                for c in children.iter().filter(|c| c.t != p) {
                    if !item_members.contains(&c.key) {
                        continue;
                    }
                    let by_def = item_defs.iter().any(|d| c.defs.contains(*d));
                    let by_shape = item_defs.is_empty()
                        && names_kind(member, c.t)
                        && item_members.contains(&c.id_field)
                        && 2 * item_members.intersection(&c.members).count() >= item_members.len();
                    if !(by_def || by_shape) {
                        continue;
                    }
                    let Some((link, list_route)) = linked(scopes, &gets, (p, p_path), c) else {
                        continue;
                    };
                    let mut item_members: Vec<String> = item_members.iter().cloned().collect();
                    item_members.sort();
                    found.push(Embed {
                        parent: p.clone(),
                        member: member.clone(),
                        list: shape.list,
                        child: c.t.to_string(),
                        key: c.key.clone(),
                        id_field: c.id_field.clone(),
                        link,
                        item_members,
                        page: shape.max_items.unwrap_or(match shape.list {
                            true => LIST_PAGE,
                            false => MAX_PLAIN,
                        }),
                        total_count: shape.total_count,
                        parent_path: p_path.clone(),
                        parent_key: parent_key.clone(),
                        list_route,
                        by_def,
                    });
                }
                // A definition's match over a shape's; two of one kind are a guess.
                let strong = found.iter().filter(|e| e.by_def).count();
                let pick = match strong {
                    1 => found.into_iter().find(|e| e.by_def),
                    0 if found.len() == 1 => found.pop(),
                    _ => None,
                };
                if let Some(embed) = pick {
                    by_parent.entry(p.clone()).or_default().push(embed);
                }
            }
        }
        Self { by_parent }
    }

    /// The embeds of parent type `t`.
    pub(crate) fn of_parent(&self, t: &str) -> &[Embed] {
        self.by_parent.get(t).map_or(&[], Vec::as_slice)
    }

    /// The embeds of child type `t`.
    pub(crate) fn of_child<'a>(&'a self, t: &'a str) -> impl Iterator<Item = &'a Embed> + 'a {
        self.by_parent
            .values()
            .flatten()
            .filter(move |e| e.child == t)
    }

    /// Every embed bound, sorted by parent and member.
    #[cfg(test)]
    pub(crate) fn all(&self) -> Vec<&Embed> {
        let mut out: Vec<&Embed> = self.by_parent.values().flatten().collect();
        out.sort_by(|a, b| (&a.parent, &a.member).cmp(&(&b.parent, &b.member)));
        out
    }
}

/// Whether member `member` is named for type `t`: the two, lower case with `-` as `_` and
/// in the singular (`categories`, `addresses`, `seasons`), are one, or one ends in `_` and the other (`seasons` of
/// `season`, `items` of `subscription_items`).
fn names_kind(member: &str, t: &str) -> bool {
    let norm = |s: &str| {
        let s = s.to_ascii_lowercase().replace('-', "_");
        if let Some(stem) = s.strip_suffix("ies") {
            return format!("{stem}y");
        }
        if let Some(stem) = s.strip_suffix("sses") {
            return format!("{stem}ss");
        }
        if s.ends_with("ss") {
            return s;
        }
        s.strip_suffix('s').map_or(s.clone(), str::to_string)
    };
    let (m, t) = (norm(member), norm(t));
    !m.is_empty() && (m == t || t.ends_with(&format!("_{m}")) || m.ends_with(&format!("_{t}")))
}

/// The member names `parts` declare.
fn members_of(parts: &[&Value]) -> HashSet<String> {
    parts
        .iter()
        .filter_map(|part| part.get("properties").and_then(Value::as_object))
        .flat_map(|p| p.keys().cloned())
        .collect()
}

/// What an embed member is.
struct Shape {
    /// Its items' schema.
    items: Value,
    /// Whether it is a list object.
    list: bool,
    /// The items' `maxItems`.
    max_items: Option<usize>,
    /// Whether the list object declares `total_count`.
    total_count: bool,
}

/// `schema`, a parent member's, as an embed: an array, or a list object (an object
/// declaring `data`, an array, and `has_more` or `url`).
fn embed_shape(bundle: &Bundle, schema: &Value) -> Option<Shape> {
    let (parts, _) = schema_parts(bundle, Some(schema));
    let max = |array: &Value| {
        array
            .get("maxItems")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
    };
    if let Some(array) = parts.iter().find(|p| p.get("items").is_some()) {
        return Some(Shape {
            items: array.get("items")?.clone(),
            list: false,
            max_items: max(array),
            total_count: false,
        });
    }
    let props = parts
        .iter()
        .filter_map(|p| p.get("properties").and_then(Value::as_object))
        .find(|p| {
            p.contains_key("data") && (p.contains_key("has_more") || p.contains_key("url"))
        })?;
    let (data_parts, _) = schema_parts(bundle, props.get("data"));
    let array = data_parts.iter().find(|p| p.get("items").is_some())?;
    Some(Shape {
        items: array.get("items")?.clone(),
        list: true,
        max_items: max(array),
        total_count: props.contains_key("total_count"),
    })
}

/// A route's segments, parameters as `{}`.
fn shape_of(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| match s.contains('{') {
            true => "{}".to_string(),
            false => s.to_string(),
        })
        .collect()
}

/// The names of a route's parameters, in order.
fn params_of(path: &str) -> Vec<String> {
    path.split('{')
        .skip(1)
        .filter_map(|rest| rest.split_once('}').map(|(name, _)| name.to_string()))
        .collect()
}

/// How child `c` names parent `p` (read at `p_path`), and where a parent's children
/// are listed, when it does (see the module's doc).
fn linked(
    scopes: &ParentScopes,
    gets: &[(&Operation, &str)],
    (p, p_path): (&str, &str),
    c: &Child<'_>,
) -> Option<(Link, Option<ListRoute>)> {
    let parent = shape_of(p_path);
    // The child's routes under the parent's path.
    let under: Vec<&str> = gets
        .iter()
        .filter(|(op, _)| op.resource_type.as_deref() == Some(c.t))
        .map(|(_, path)| *path)
        .filter(|path| {
            let shape = shape_of(path);
            shape.len() > parent.len() && shape[..parent.len()] == parent[..]
        })
        .collect();
    // A list of the children: one literal segment past the parent's path.
    let list_under = under
        .iter()
        .find(|path| {
            let shape = shape_of(path);
            shape.len() == parent.len() + 1 && shape.last().is_some_and(|s| s != "{}")
        })
        .map(|path| ListRoute::Path((*path).to_string()));
    if let Some(scope) = scopes.get(c.t) {
        let names = params_of(p_path);
        let params: Map<String, Value> = names
            .iter()
            .map(|n| (n.clone(), Value::String("x".into())))
            .collect();
        let values = scope.of_route(p_path, &params)?;
        return (values.len() == names.len() && !names.is_empty() && !under.is_empty())
            .then_some((Link::Scoped, list_under));
    }
    // A member of the child named for the parent's last parameter or its type.
    let last = params_of(p_path).pop();
    let singular = p.strip_suffix('s').unwrap_or(p);
    let named: Vec<String> = last
        .into_iter()
        .chain([p.to_string(), singular.to_string()])
        .filter(|m| *m != c.key && *m != c.id_field && c.members.contains(m))
        .collect();
    let member = named.first()?.clone();
    if !under.is_empty() {
        return Some((Link::Member(member), list_under));
    }
    // Else a list of the children narrowed by that member's query parameter.
    let query = gets.iter().find(|(op, path)| {
        op.resource_type.as_deref() == Some(c.t)
            && !path.contains('{')
            && op
                .input
                .query
                .as_ref()
                .and_then(|q| q.get("properties"))
                .and_then(Value::as_object)
                .is_some_and(|q| q.contains_key(&member))
    })?;
    Some((
        Link::Member(member.clone()),
        Some(ListRoute::Query(query.1.to_string(), member)),
    ))
}

/// The value a child's link member holds: an id, or an expanded object's `id`.
fn link_value(value: &Value) -> Option<String> {
    id_string(value).or_else(|| value.get("id").and_then(id_string))
}

/// The items an embed holds in `data`, its parent's record.
fn items_of<'v>(embed: &Embed, data: &'v Value) -> Option<&'v Vec<Value>> {
    let at = data.get(&embed.member)?;
    match embed.list {
        true => at.get("data")?.as_array(),
        false => at.as_array(),
    }
}

/// The value an item or record holds for the embed's key.
fn key_of(embed: &Embed, value: &Value) -> Option<String> {
    mb_mcm::member_at(value, &embed.key).and_then(id_string)
}

/// The route of `op`, when it is an HTTP one.
fn route_of(op: &Operation) -> Option<&str> {
    match &op.binding {
        Binding::Http { path, .. } => Some(path),
        _ => None,
    }
}

/// The parent values a scoped child of `embed` takes in this call, by its route.
fn scope_values(
    rt: &MockRuntime,
    embed: &Embed,
    op: &Operation,
    call: &OperationCall,
) -> Option<Vec<String>> {
    let scope = rt.index().parent_scopes().get(&embed.child)?;
    scope.of_route(route_of(op)?, &call.path_params)
}

/// The children of `parent` the session holds for `embed`: under the parent's path
/// values `values` (a scoped child), else naming its id.
async fn held_children(
    state: &mut StateAccess<'_>,
    s: &SessionKey,
    embed: &Embed,
    parent: &StoredResource,
    values: Option<&[String]>,
) -> Vec<StoredResource> {
    let mut held = match &embed.link {
        Link::Scoped => match values {
            Some(values) => state.list_in(s, &embed.child, values, MAX_CHILDREN).await,
            None => Vec::new(),
        },
        Link::Member(m) => state
            .list(s, &embed.child, MAX_CHILDREN)
            .await
            .into_iter()
            .filter(|r| r.data.get(m).and_then(link_value).as_deref() == Some(parent.id.as_str()))
            .collect(),
    };
    // Filler leaves once a child that is not filler is held.
    let fillers = state.rt.filler_children();
    if fillers.any_of(s, &embed.child) {
        let is_filler: Vec<bool> = held
            .iter()
            .map(|c| {
                key_of(embed, &c.data)
                    .is_some_and(|key| fillers.holds(&filler(s, embed, (&parent.id, values), key)))
            })
            .collect();
        if is_filler.iter().any(|f| !f) {
            let mut at = is_filler.into_iter();
            held.retain(|_| !at.next().unwrap_or(false));
        }
    }
    held
}

/// `child` as an item of `embed`: the members its items declare, from the child, else
/// from what the embed held of it (`before`).
fn projected(embed: &Embed, child: &Value, before: Option<&Value>) -> Value {
    if embed.item_members.is_empty() {
        return child.clone();
    }
    let mut out = Map::new();
    for name in &embed.item_members {
        if let Some(v) = child.get(name).or_else(|| before.and_then(|b| b.get(name))) {
            out.insert(name.clone(), v.clone());
        }
    }
    Value::Object(out)
}

/// The concrete list route of `embed` for `parent` (under path values `values`).
fn list_url(embed: &Embed, parent: &StoredResource, values: Option<&[String]>) -> Option<String> {
    match embed.list_route.as_ref()? {
        ListRoute::Query(path, param) => Some(format!("{path}?{param}={}", parent.id)),
        ListRoute::Path(path) => {
            let fill: Vec<String> = match (&embed.link, values) {
                (Link::Scoped, Some(values)) => values.to_vec(),
                _ => vec![parent.id.clone()],
            };
            let mut out = String::new();
            let mut rest = path.as_str();
            let mut at = 0;
            while let Some(open) = rest.find('{') {
                let close = open + rest[open..].find('}')?;
                out.push_str(&rest[..open]);
                out.push_str(fill.get(at)?);
                at += 1;
                rest = &rest[close + 1..];
            }
            out.push_str(rest);
            (at == fill.len()).then_some(out)
        }
    }
}

/// `parent`'s record with `embed` drawn from `children` (see the module's doc), the
/// embed's own items first in their order.
fn drawn(
    embed: &Embed,
    parent: &StoredResource,
    values: Option<&[String]>,
    children: &[StoredResource],
) -> Value {
    let mut data = parent.data.clone();
    let before: Vec<Value> = items_of(embed, &parent.data).cloned().unwrap_or_default();
    let mut used: HashSet<usize> = HashSet::new();
    let mut items: Vec<Value> = Vec::new();
    for item in &before {
        let key = key_of(embed, item);
        let id = mb_mcm::member_at(item, &embed.id_field).and_then(id_string);
        // By the key the child's route reads it by, else by its identity.
        let same = |c: &StoredResource, member: &str, want: &Option<String>| {
            want.is_some() && mb_mcm::member_at(&c.data, member).and_then(id_string) == *want
        };
        if let Some(at) = children
            .iter()
            .position(|c| same(c, &embed.key, &key))
            .or_else(|| children.iter().position(|c| same(c, &embed.id_field, &id)))
            && used.insert(at)
        {
            items.push(projected(embed, &children[at].data, Some(item)));
        }
    }
    let mut rest: Vec<&StoredResource> = children
        .iter()
        .enumerate()
        .filter(|(at, _)| !used.contains(at))
        .map(|(_, c)| c)
        .collect();
    rest = in_order(embed, rest);
    items.extend(rest.into_iter().map(|c| projected(embed, &c.data, None)));
    let total = items.len();
    items.truncate(embed.page);
    let Some(record) = data.as_object_mut() else {
        return data;
    };
    if !embed.list {
        record.insert(embed.member.clone(), Value::Array(items));
        return data;
    }
    let mut list = match record.get(&embed.member) {
        Some(Value::Object(list)) => list.clone(),
        _ => {
            let mut list = Map::new();
            list.insert("object".into(), Value::String("list".into()));
            list
        }
    };
    list.insert("has_more".into(), Value::Bool(total > embed.page));
    if embed.total_count || list.contains_key("total_count") {
        list.insert("total_count".into(), Value::from(total));
    }
    if let Some(url) = list_url(embed, parent, values) {
        list.insert("url".into(), Value::String(url));
    }
    list.insert("data".into(), Value::Array(items));
    record.insert(embed.member.clone(), Value::Object(list));
    data
}

/// Stores the children `parent`'s embeds list that the session does not hold yet
/// (unless `stores` is false; noted as filler when the faker made the parent's
/// answer, `filler_made`), draws each embed from the held children, and stores the parent
/// so drawn when it changed and is still as read. The parent as it is now.
pub(super) async fn sync(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    parent: StoredResource,
    (stores, filler_made): (bool, bool),
) -> StoredResource {
    let rt = state.rt;
    let embeds = of_parent(rt, &parent.resource_type);
    if embeds.is_empty() || !parent.data.is_object() {
        return parent;
    }
    let s = &plan.session;
    let mut data = parent.data.clone();
    for embed in &embeds {
        let values = match embed.link {
            Link::Scoped => match scope_values(rt, embed, plan.op, call) {
                Some(values) => Some(values),
                None => continue,
            },
            Link::Member(_) => None,
        };
        let values = values.as_deref();
        let current = StoredResource {
            data: data.clone(),
            ..parent.clone()
        };
        let mut held = held_children(state, s, embed, &current, values).await;
        if stores && let Some(items) = items_of(embed, &current.data) {
            let known: HashSet<String> =
                held.iter().filter_map(|c| key_of(embed, &c.data)).collect();
            let mut fresh: Vec<StoredResource> = Vec::new();
            for item in items {
                let (Some(key), Some(id)) = (
                    key_of(embed, item),
                    mb_mcm::member_at(item, &embed.id_field).and_then(id_string),
                ) else {
                    continue;
                };
                if known.contains(&key)
                    || fresh
                        .iter()
                        .any(|f| key_of(embed, &f.data).as_deref() == Some(key.as_str()))
                {
                    continue;
                }
                let mut child = item.clone();
                if let (Link::Member(m), Some(record)) = (&embed.link, child.as_object_mut()) {
                    record
                        .entry(m.clone())
                        .or_insert_with(|| Value::String(parent.id.clone()));
                }
                fresh.push(StoredResource {
                    resource_type: embed.child.clone(),
                    id,
                    data: child,
                });
            }
            if !fresh.is_empty() {
                let keys: Vec<Option<String>> =
                    fresh.iter().map(|f| key_of(embed, &f.data)).collect();
                let stored = state.insert_seeded_all(s, fresh).await;
                // Stored from a faker-made answer: filler; from any other, not (a note
                // left from an earlier record of that key goes).
                let mut notes = Vec::new();
                for (key, took) in keys.into_iter().zip(&stored) {
                    if let (Some(key), true) = (key, *took) {
                        let note = filler(s, embed, (&parent.id, values), key);
                        match filler_made {
                            true => notes.push(note),
                            false => rt.filler_children().backed(&note),
                        }
                    }
                }
                rt.filler_children().note(notes);
                if stored.iter().any(|took| *took) {
                    // A seeded record does not clear the cached answers of its type.
                    if let Some(cache) = rt.state.response_cache() {
                        cache.invalidate_type(s, &embed.child);
                    }
                    held = held_children(state, s, embed, &current, values).await;
                }
            }
        }
        // Nothing held and nothing stored: the embed stays as it was answered.
        if held.is_empty() && stores {
            continue;
        }
        data = drawn(embed, &current, values, &held);
    }
    if data == parent.data {
        return parent;
    }
    let drawn = StoredResource {
        data,
        ..parent.clone()
    };
    // Compare and set: stored only over the parent as it was read.
    match state.get(s, &parent.resource_type, &parent.id).await {
        Some(now) if now.data == parent.data => {
            if state.upsert(s, drawn.clone()).await {
                drawn
            } else {
                parent
            }
        }
        Some(now) => now,
        None => drawn,
    }
}

/// Children held, oldest first (a held list answers the most recent first); by their
/// key where every one is numbered.
fn in_order<'a>(embed: &Embed, mut rest: Vec<&'a StoredResource>) -> Vec<&'a StoredResource> {
    rest.reverse();
    let numbers: Option<Vec<f64>> = rest
        .iter()
        .map(|c| mb_mcm::member_at(&c.data, &embed.key).and_then(Value::as_f64))
        .collect();
    if let Some(numbers) = numbers {
        let mut paired: Vec<(f64, &StoredResource)> = numbers.into_iter().zip(rest).collect();
        paired.sort_by(|a, b| a.0.total_cmp(&b.0));
        rest = paired.into_iter().map(|(_, c)| c).collect();
    }
    rest
}

/// The parent's id a call's `route` names, where it runs through the parent's read path:
/// the value of its parameter last in that path.
fn parent_param(embed: &Embed, route: &str, call: &OperationCall) -> Option<String> {
    let parent = shape_of(&embed.parent_path);
    let shape = shape_of(route);
    if shape.len() <= parent.len() || shape[..parent.len()] != parent[..] {
        return None;
    }
    let at = shape[..parent.len()].iter().filter(|s| *s == "{}").count();
    let name = params_of(route).get(at.checked_sub(1)?)?.clone();
    call.path_params.get(&name).and_then(id_string)
}

/// A listed parent: its id, the scope values of a scoped child, and the query
/// parameter naming it, if one does.
type ListParent<'e> = (String, Option<Vec<String>>, Option<&'e str>);

/// The parent a call on a child list route of `embed`'s lists the children of.
fn list_parent<'e>(
    rt: &MockRuntime,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    embed: &'e Embed,
) -> Option<ListParent<'e>> {
    let (route, list_route) = (route_of(plan.op)?, embed.list_route.as_ref()?);
    match list_route {
        ListRoute::Query(path, param) if path == route => Some((
            call.query.get(param).and_then(id_string)?,
            None,
            Some(param.as_str()),
        )),
        ListRoute::Path(path) if shape_of(path) == shape_of(route) => match embed.link {
            Link::Scoped => {
                let values = scope_values(rt, embed, plan.op, call)?;
                Some((values.last()?.clone(), Some(values), None))
            }
            Link::Member(_) => Some((parent_param(embed, route, call)?, None, None)),
        },
        _ => None,
    }
}

/// Children of type `t` (`stored`, their records) a child list the faker made stored:
/// filler under the parent the list is of, as a parent's embed the faker made stores
/// (see [`FillerChildren`]).
pub(super) fn note_listed(
    rt: &MockRuntime,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: &str,
    stored: &[&Value],
) {
    if stored.is_empty() || !applies(rt, plan.op) {
        return;
    }
    let s = &plan.session;
    for embed in &of_child(rt, t) {
        let Some((parent_id, values, _)) = list_parent(rt, (plan, call), embed) else {
            continue;
        };
        let notes = stored
            .iter()
            .filter_map(|data| key_of(embed, data))
            .map(|key| filler(s, embed, (&parent_id, values.as_deref()), key))
            .collect();
        rt.filler_children().note(notes);
    }
}

/// A plain read of a child list route of `embed`'s (no cursor or filter but the
/// parent's): `body`, its answer, holds the children held under the parent, at most a
/// page (`limit`, else the embed's), when the session holds any. Whether it was one.
async fn listed(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    embed: &Embed,
    body: &mut Value,
) -> bool {
    let Some((parent_id, values, own)) = list_parent(state.rt, (plan, call), embed) else {
        return false;
    };
    let narrowed = call
        .query
        .keys()
        .any(|k| Some(k.as_str()) != own && k != "limit" && !k.starts_with("expand"));
    if narrowed {
        return false;
    }
    let parent = StoredResource {
        resource_type: embed.parent.clone(),
        id: parent_id,
        data: Value::Null,
    };
    let held = held_children(state, &plan.session, embed, &parent, values.as_deref()).await;
    if held.is_empty() {
        return false;
    }
    let items: Vec<Value> = in_order(embed, held.iter().collect())
        .into_iter()
        .map(|c| c.data.clone())
        .collect();
    let total = items.len();
    let limit = call
        .query
        .get("limit")
        .and_then(|l| l.as_u64().or_else(|| l.as_str()?.parse().ok()))
        .and_then(|l| usize::try_from(l).ok())
        .filter(|l| *l > 0);
    match body {
        Value::Object(list) if list.get("data").is_some_and(Value::is_array) => {
            let page = limit.unwrap_or(LIST_PAGE);
            list.insert("has_more".into(), Value::Bool(total > page));
            if list.contains_key("total_count") {
                list.insert("total_count".into(), Value::from(total));
            }
            if let Some(url) = list_url(embed, &parent, values.as_deref()) {
                list.insert("url".into(), Value::String(url));
            }
            list.insert(
                "data".into(),
                Value::Array(items.into_iter().take(page).collect()),
            );
        }
        Value::Array(_) => {
            let page = limit.unwrap_or(MAX_PLAIN);
            *body = Value::Array(items.into_iter().take(page).collect());
        }
        _ => return false,
    }
    true
}

/// The held parent of `embed` a route names by `value`: under that id, else the one
/// holding it in the member its own read names it by (a season read by its number).
async fn held_parent(
    state: &mut StateAccess<'_>,
    s: &SessionKey,
    embed: &Embed,
    value: &str,
) -> Option<StoredResource> {
    if let Some(found) = state.get(s, &embed.parent, value).await {
        return Some(found);
    }
    let mut holding = state
        .list(s, &embed.parent, MAX_PARENTS)
        .await
        .into_iter()
        .filter(|p| {
            mb_mcm::member_at(&p.data, &embed.parent_key)
                .and_then(id_string)
                .as_deref()
                == Some(value)
        });
    let found = holding.next()?;
    holding.next().is_none().then_some(found)
}

/// The held parent of a child `embed` names in this call: by the call's route (a
/// scoped child, or one under the parent's path), else by the child's link member in
/// `data`, else the one held parent embedding an item of key `key`.
async fn parent_of(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    embed: &Embed,
    (data, key): (Option<&Value>, Option<&str>),
) -> Option<StoredResource> {
    let rt = state.rt;
    let s = &plan.session;
    let route = route_of(plan.op)?;
    let parent_id = match &embed.link {
        Link::Scoped => scope_values(rt, embed, plan.op, call)?.pop(),
        Link::Member(m) => parent_param(embed, route, call)
            .or_else(|| data.and_then(|d| d.get(m)).and_then(link_value)),
    };
    if let Some(id) = parent_id {
        return held_parent(state, s, embed, &id).await;
    }
    let key = key?;
    let mut holding = state
        .list(s, &embed.parent, MAX_PARENTS)
        .await
        .into_iter()
        .filter(|p| {
            items_of(embed, &p.data).is_some_and(|items| {
                items
                    .iter()
                    .any(|i| key_of(embed, i).as_deref() == Some(key))
            })
        });
    let found = holding.next()?;
    holding.next().is_none().then_some(found)
}

/// The call's target, settled with its embeds (see the module's doc): a read of a held
/// parent answers its embeds drawn from its held children; a call naming a child the
/// session does not hold, which its held parent embeds, takes that child, stored. A
/// child so read or written on its own route is no longer filler.
pub(super) async fn settle_target(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    target_id: Option<&str>,
    target: Option<StoredResource>,
) -> Option<StoredResource> {
    let target = settled(state, (plan, call), target_id, target).await;
    if let (Some(t), Some(child)) = (plan.op.resource_type.as_deref(), target.as_ref())
        && applies(state.rt, plan.op)
    {
        backed(state, (plan, call), t, &child.data).await;
    }
    target
}

/// A child of type `t` (its record `data`) a call on its own route read or wrote: not
/// filler any more.
async fn backed(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: &str,
    data: &Value,
) {
    let rt = state.rt;
    let s = &plan.session;
    for embed in &of_child(rt, t) {
        if !rt.filler_children().any_of(s, t) {
            return;
        }
        let Some(key) = key_of(embed, data) else {
            continue;
        };
        let values = scope_values(rt, embed, plan.op, call);
        let parent_id = match embed.link {
            Link::Scoped if values.is_none() => continue,
            Link::Scoped => String::new(),
            // A parent not held: the one its link names (a child list noted it so).
            Link::Member(ref m) => {
                match parent_of(state, (plan, call), embed, (Some(data), Some(&key))).await {
                    Some(parent) => parent.id,
                    None => match data.get(m).and_then(link_value) {
                        Some(id) => id,
                        None => continue,
                    },
                }
            }
        };
        let note = filler(s, embed, (&parent_id, values.as_deref()), key);
        rt.filler_children().backed(&note);
    }
}

/// [`settle_target`] before the target counts as read on its own route.
async fn settled(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    target_id: Option<&str>,
    target: Option<StoredResource>,
) -> Option<StoredResource> {
    let rt = state.rt;
    let op = plan.op;
    let (true, Some(t), Some(_)) = (applies(rt, op), op.resource_type.as_deref(), route_of(op))
    else {
        return target;
    };
    // A forced status or a seed's answer is no read of the session's records.
    if plan.forced_status.is_some() || mb_engine::seed::request_seed(call).is_some() {
        return target;
    }
    match target {
        Some(parent) if op.kind == OperationKind::Query && !of_parent(rt, t).is_empty() => {
            Some(sync(state, (plan, call), parent, (true, false)).await)
        }
        Some(held) => Some(held),
        None => {
            let id = target_id?;
            if matches!(
                op.kind,
                OperationKind::Command(Effect::Create | Effect::Delete)
            ) {
                return None;
            }
            for embed in &of_child(rt, t) {
                let Some(parent) = parent_of(state, (plan, call), embed, (None, Some(id))).await
                else {
                    continue;
                };
                let Some(item) = items_of(embed, &parent.data)
                    .and_then(|items| {
                        items
                            .iter()
                            .find(|i| key_of(embed, i).as_deref() == Some(id))
                    })
                    .cloned()
                else {
                    continue;
                };
                let Some(child_id) = mb_mcm::member_at(&item, &embed.id_field).and_then(id_string)
                else {
                    continue;
                };
                sync(state, (plan, call), parent, (true, false)).await;
                if let Some(child) = state.get(&plan.session, t, &child_id).await {
                    return Some(child);
                }
            }
            None
        }
    }
}

/// After a call answered (2xx): a parent written or generated stores its embedded
/// children, and `body` (its answer's record, when it is the record) draws its embeds
/// from the held children; a child written, generated or completed is added to its held
/// parent's embed (a deleted one leaves it). `filler_made`: the faker made the answer,
/// so the children a read's embeds list are filler (not a write's: its request's are
/// real).
pub(super) async fn after_answer(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    target_id: Option<&str>,
    body: Option<&mut Value>,
    filler_made: bool,
) {
    let rt = state.rt;
    let op = plan.op;
    let (true, Some(t), Some(_)) = (applies(rt, op), op.resource_type.as_deref(), route_of(op))
    else {
        return;
    };
    // A list of children under their parent answers those it holds.
    if op.kind == OperationKind::Query && target_id.is_none() {
        if let Some(body) = body {
            for embed in &of_child(rt, t) {
                if listed(state, (plan, call), embed, body).await {
                    return;
                }
            }
        }
        return;
    }
    let delete = op.kind == OperationKind::Command(Effect::Delete);
    let mut body = body;
    let parents = of_parent(rt, t);
    // A parent: its embedded children stored, its answer's embeds drawn from them. A
    // type may be a parent and a child both (a season of a show, with its episodes).
    if !parents.is_empty() && !delete {
        let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
        let id = body
            .as_deref()
            .and_then(|b| mb_mcm::member_at(b, field))
            .and_then(id_string)
            .or_else(|| target_id.map(str::to_string));
        let held = match id.as_deref() {
            Some(id) => state.get(&plan.session, t, id).await,
            None => None,
        };
        if let (Some(id), Some(held)) = (id, held) {
            let filler_made = filler_made && op.kind == OperationKind::Query;
            let synced = sync(state, (plan, call), held, (true, filler_made)).await;
            // What the answer stored of its children (its nested records, seeded) does
            // not clear their type's cached answers: a list of them cached before.
            if let Some(cache) = rt.state.response_cache() {
                for embed in &parents {
                    cache.invalidate_type(&plan.session, &embed.child);
                }
            }
            if let Some(Value::Object(answer)) = body.as_deref_mut()
                && answer.get(field).and_then(id_string).as_deref() == Some(id.as_str())
            {
                for embed in &parents {
                    if let (Some(slot), Some(now)) = (
                        answer.get_mut(&embed.member),
                        synced.data.get(&embed.member),
                    ) {
                        *slot = now.clone();
                    }
                }
            }
        }
    }
    for embed in &of_child(rt, t) {
        let key = body
            .as_deref()
            .and_then(|b| key_of(embed, b))
            .or_else(|| target_id.map(str::to_string));
        let Some(parent) = parent_of(
            state,
            (plan, call),
            embed,
            (body.as_deref(), key.as_deref()),
        )
        .await
        else {
            continue;
        };
        if let Some(key) = key {
            let values = scope_values(rt, embed, plan.op, call);
            let note = filler(&plan.session, embed, (&parent.id, values.as_deref()), key);
            rt.filler_children().backed(&note);
        }
        sync(state, (plan, call), parent, (!delete, false)).await;
    }
}

/// Whether the description holds `embed`'s children inside the parent's member
/// (`parent.embedded`, [`super::exec`]'s records held in their parent): they have no
/// records of their own, so the embed is not drawn from any.
fn described_within(rt: &MockRuntime, embed: &Embed) -> bool {
    rt.described()
        .type_desc(&embed.child)
        .and_then(|d| d.parent.as_ref())
        .and_then(|p| p.embedded.as_deref())
        == Some(embed.member.as_str())
}

/// The embeds of parent type `t` kept in step.
fn of_parent(rt: &MockRuntime, t: &str) -> Vec<Embed> {
    rt.index()
        .child_embeds()
        .of_parent(t)
        .iter()
        .filter(|e| !described_within(rt, e))
        .cloned()
        .collect()
}

/// The embeds of child type `t` kept in step.
fn of_child(rt: &MockRuntime, t: &str) -> Vec<Embed> {
    rt.index()
        .child_embeds()
        .of_child(t)
        .filter(|e| !described_within(rt, e))
        .cloned()
        .collect()
}

/// Whether `op` acts on a type that embeds children.
pub(super) fn is_parent(rt: &MockRuntime, op: &Operation) -> bool {
    op.resource_type
        .as_deref()
        .is_some_and(|t| !of_parent(rt, t).is_empty())
}

/// Whether embeds are kept in step for `op`: a mock keeping state, over HTTP.
fn applies(rt: &MockRuntime, op: &Operation) -> bool {
    rt.consistency && matches!(op.binding, Binding::Http { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::BundleIndex;

    fn index(contract: &str) -> BundleIndex {
        let bundle = mb_import::compile(contract.as_bytes()).unwrap();
        BundleIndex::new(&bundle)
    }

    /// A show and its seasons, with `forks` items shaped like a season but not named so.
    const SHOWS: &str = r##"
openapi: 3.0.3
info: {title: Shows, version: "1"}
paths:
  /tv/{series_id}:
    get:
      operationId: show
      parameters: [{name: series_id, in: path, required: true, schema: {type: integer}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  id: {type: integer}
                  seasons:
                    type: array
                    maxItems: 6
                    items:
                      type: object
                      properties:
                        id: {type: integer}
                        season_number: {type: integer}
                        name: {type: string}
                        episode_count: {type: integer}
                  forks:
                    type: array
                    items:
                      type: object
                      properties:
                        id: {type: integer}
                        season_number: {type: integer}
                        name: {type: string}
  /tv/{series_id}/season/{season_number}:
    get:
      operationId: season
      parameters:
        - {name: series_id, in: path, required: true, schema: {type: integer}}
        - {name: season_number, in: path, required: true, schema: {type: integer}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  id: {type: integer}
                  season_number: {type: integer}
                  name: {type: string}
                  episodes: {type: array, items: {type: object}}
"##;

    /// A subscription's `items` list object and the item list its query narrows.
    const BILLING: &str = r##"
openapi: 3.0.3
info: {title: Billing, version: "1"}
paths:
  /v1/subscriptions/{id}:
    get:
      operationId: subscription
      parameters: [{name: id, in: path, required: true, schema: {type: string, pattern: "^sub_"}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/subscription"}}}}
  /v1/subscription_items:
    get:
      operationId: items
      parameters: [{name: subscription, in: query, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  object: {type: string}
                  data: {type: array, items: {$ref: "#/components/schemas/subscription_item"}}
                  has_more: {type: boolean}
                  url: {type: string}
  /v1/subscription_items/{item}:
    get:
      operationId: item
      parameters: [{name: item, in: path, required: true, schema: {type: string, pattern: "^si_"}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/subscription_item"}}}}
components:
  schemas:
    subscription_item:
      type: object
      properties:
        id: {type: string, pattern: "^si_"}
        subscription: {type: string}
        quantity: {type: integer}
    subscription:
      type: object
      properties:
        id: {type: string, pattern: "^sub_"}
        items:
          type: object
          properties:
            object: {type: string}
            data: {type: array, items: {$ref: "#/components/schemas/subscription_item"}}
            has_more: {type: boolean}
            url: {type: string}
"##;

    #[test]
    fn a_show_s_seasons_bind_to_the_season_read_and_its_forks_do_not() {
        let index = index(SHOWS);
        let embeds = index.child_embeds().all();
        assert_eq!(embeds.len(), 1, "{embeds:?}");
        let e = embeds[0];
        assert_eq!((e.parent.as_str(), e.member.as_str()), ("tv", "seasons"));
        assert_eq!(
            (e.child.as_str(), e.key.as_str()),
            ("season", "season_number")
        );
        assert_eq!(e.link, Link::Scoped);
        assert!(!e.list && !e.by_def);
        assert_eq!(e.page, 6);
    }

    /// An embedded item is matched to a held child by its key, else by its identity; an
    /// item neither matches leaves the embed.
    #[test]
    fn an_item_is_drawn_from_the_child_of_its_key_else_its_id() {
        let index = index(SHOWS);
        let e = index.child_embeds().all()[0].clone();
        let season = |id: u64, n: u64, name: &str| StoredResource {
            resource_type: "season".into(),
            id: id.to_string(),
            data: serde_json::json!({"id": id, "season_number": n, "name": name}),
        };
        let parent = StoredResource {
            resource_type: "tv".into(),
            id: "1".into(),
            data: serde_json::json!({"id": 1, "seasons": [
                {"id": 20, "season_number": 9, "name": "by id"},
                {"id": 99, "season_number": 1, "name": "by key"},
                {"id": 77, "season_number": 7, "name": "neither"},
            ]}),
        };
        let held = [season(10, 1, "one"), season(20, 2, "two")];
        let drawn = drawn(&e, &parent, Some(&["1".to_string()]), &held);
        let got: Vec<(u64, u64)> = drawn["seasons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["id"].as_u64().unwrap(),
                    s["season_number"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(got, [(20, 2), (10, 1)], "{drawn}");
    }

    #[test]
    fn a_subscription_s_items_bind_to_its_item_list() {
        let index = index(BILLING);
        let embeds = index.child_embeds().all();
        assert_eq!(embeds.len(), 1, "{embeds:?}");
        let e = embeds[0];
        assert_eq!(e.member, "items");
        assert_eq!(e.child, "subscription_items");
        assert_eq!(e.link, Link::Member("subscription".into()));
        assert_eq!(
            e.list_route,
            Some(ListRoute::Query(
                "/v1/subscription_items".into(),
                "subscription".into()
            ))
        );
        assert!(e.list && e.by_def);
        assert_eq!(e.page, LIST_PAGE);
        let parent = StoredResource {
            resource_type: "subscriptions".into(),
            id: "sub_1".into(),
            data: Value::Null,
        };
        assert_eq!(
            list_url(e, &parent, None).as_deref(),
            Some("/v1/subscription_items?subscription=sub_1")
        );
    }

    #[test]
    fn a_member_names_its_kind_by_its_last_word() {
        assert!(names_kind("seasons", "season"));
        assert!(names_kind("items", "subscription_items"));
        assert!(names_kind("protection_rules", "protection-rules"));
        assert!(!names_kind("forks", "comments"));
        assert!(!names_kind("resources_attributes", "resources"));
        assert!(names_kind("categories", "category"));
        assert!(names_kind("addresses", "address"));
    }

    /// Two child types read as one definition: which one the items are is a guess, so
    /// the member binds to neither.
    #[test]
    fn items_two_types_read_as_bind_to_neither() {
        let contract = BILLING.replace(
            "components:",
            r##"  /v1/legacy_items/{id}:
    get:
      operationId: legacy
      parameters: [{name: id, in: path, required: true, schema: {type: string, pattern: "^si_"}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/subscription_item"}}}}
  /v1/legacy_items:
    get:
      operationId: legacy_list
      parameters: [{name: subscription, in: query, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/subscription_item"}}}}}
components:"##,
        );
        let index = index(&contract);
        assert!(
            index.child_embeds().all().is_empty(),
            "{:?}",
            index.child_embeds().all()
        );
    }
}
