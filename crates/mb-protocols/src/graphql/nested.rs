//! Fields below a query's root answered from what the session holds.
//!
//! A root field is one engine call; what it selects below is generated with
//! it. Without this, a record the session holds is invisible as soon as a
//! client reaches it through another object: GitHub's
//! `repository(owner:, name:) { issue(number:) }` or `issues(states:)`
//! would show an issue the model made up, not the one `createIssue` stored.
//!
//! So after generating, the answer is walked against the schema:
//!
//! - an object field of a type with an `id`, read with arguments
//!   (`issue(number: 7)`, the root `repository(owner:, name:)`), takes the
//!   stored record those arguments match: each argument naming a member of
//!   the type (or, for a list argument such as `states`, its singular
//!   member) must equal it. An interface or union field
//!   (`issueOrPullRequest`) searches every member type and answers the one
//!   it finds.
//! - a list of such records below a stored object, as a plain list or a
//!   connection (`nodes`, `edges[].node`, `totalCount`, `pageInfo`), is
//!   answered from the records the session holds that refer to that object,
//!   filtered by the same rule and cut to `first`/`last`, as soon as it
//!   holds one: an empty list when none match. Before then the generated
//!   items are kept, minus those the arguments rule out, and stored after.
//!
//! A stored record is drawn into the generated shape: the generated item
//! with its id (or another one, or the faker's when there is none) under
//! the record's members.

use std::collections::{BTreeSet, HashMap};

use mb_engine::faker::Faker;
use mb_mcm::Bundle;
use mb_state::{SessionKey, StoredResource};
use serde_json::{Map, Value, json};

use super::select::{Selection, narrow, rekey};
use super::state::{by_field_name, def_name, list_item, object_type};
use crate::MockRuntime;
use crate::call;

/// Records read per type, at most.
const MAX_HELD: usize = 200;

/// Items a list answers when its arguments set no `first` or `last`.
const DEFAULT_PAGE: usize = 100;

/// Bounds the walk (recursive types).
const MAX_DEPTH: usize = 12;

/// The definition `schema` refers to, through a nullable `anyOf`.
pub(crate) fn def_of<'a>(bundle: &'a Bundle, schema: &Value) -> Option<&'a Value> {
    bundle.defs.get(def_ref(schema)?)
}

pub(crate) fn def_ref(schema: &Value) -> Option<&str> {
    let mut schema = schema;
    for _ in 0..4 {
        if let Some(name) = def_name(schema) {
            return Some(name);
        }
        schema = schema
            .get("anyOf")
            .and_then(Value::as_array)?
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) != Some("null"))?;
    }
    None
}

/// Whether `op` answers an object of type `t` (lowercased) itself.
pub(crate) fn answers_type(op: &mb_mcm::Operation, t: &str) -> bool {
    op.success_output()
        .and_then(|o| o.schema.as_ref())
        .and_then(def_ref)
        .is_some_and(|name| name.to_lowercase() == t)
}

/// Whether a property schema is a scalar or an enum: no object, union or
/// list (through a nullable `anyOf`).
pub(crate) fn is_scalar(schema: &Value, defs: &Map<String, Value>) -> bool {
    let mut schema = schema;
    for _ in 0..4 {
        if schema.get("items").is_some() || schema.get("properties").is_some() {
            return false;
        }
        if let Some(name) = def_name(schema) {
            return defs.get(name).is_some_and(|def| {
                def.get("properties").is_none()
                    && def.get("oneOf").is_none()
                    && def.get("items").is_none()
            });
        }
        match schema.get("anyOf").and_then(Value::as_array) {
            Some(branches) => {
                match branches
                    .iter()
                    .find(|b| b.get("type").and_then(Value::as_str) != Some("null"))
                {
                    Some(b) => schema = b,
                    None => return true,
                }
            }
            None => return true,
        }
    }
    true
}

/// The object types with an `id` a schema stands for: the type it refers
/// to, or the members of the interface or union it refers to.
fn keyed_types(bundle: &Bundle, schema: &Value) -> Vec<String> {
    let Some(name) = def_ref(schema) else {
        return Vec::new();
    };
    let Some(def) = bundle.defs.get(name) else {
        return Vec::new();
    };
    let keyed = |def: &Value| {
        def.get("properties")
            .and_then(Value::as_object)
            .is_some_and(|p| p.contains_key("id"))
    };
    if let Some(members) = def.get("oneOf").and_then(Value::as_array) {
        return members
            .iter()
            .filter_map(def_name)
            .filter(|m| bundle.defs.get(*m).is_some_and(keyed))
            .map(str::to_string)
            .collect();
    }
    if keyed(def) {
        vec![name.to_string()]
    } else {
        Vec::new()
    }
}

/// What a field holds, as far as stored records go.
enum Shape {
    /// One record of one of these types.
    One(Vec<String>),
    /// A list of records of these types, of this item schema.
    List(Vec<String>, Value),
    /// A connection of these types: its nodes' schema.
    Connection(Vec<String>, Value),
    Other,
}

fn shape(bundle: &Bundle, schema: &Value) -> Shape {
    if let Some(item) = list_item(schema) {
        let types = keyed_types(bundle, item);
        return if types.is_empty() {
            Shape::Other
        } else {
            Shape::List(types, item.clone())
        };
    }
    let types = keyed_types(bundle, schema);
    if !types.is_empty() {
        return Shape::One(types);
    }
    let Some(props) = def_of(bundle, schema)
        .and_then(|d| d.get("properties"))
        .and_then(Value::as_object)
    else {
        return Shape::Other;
    };
    let node = props
        .get("nodes")
        .and_then(list_item)
        .or_else(|| {
            let edge = def_of(bundle, list_item(props.get("edges")?)?)?;
            edge.get("properties")?.get("node")
        })
        .cloned();
    match node {
        Some(node) => {
            let types = keyed_types(bundle, &node);
            if types.is_empty() {
                Shape::Other
            } else {
                Shape::Connection(types, node)
            }
        }
        None => Shape::Other,
    }
}

/// The nearest enclosing record of an answer.
#[derive(Clone)]
struct Parent {
    /// Its type's definition name.
    ty: String,
    id: String,
}

/// Answers the fields below `op`'s root in `body` (keyed by response key,
/// for `sel`, read with `args`) from the session's records, as the module
/// says.
pub(crate) async fn from_state(
    rt: &MockRuntime,
    session: &SessionKey,
    op: &mb_mcm::Operation,
    sel: &Selection,
    args: Map<String, Value>,
    body: &mut Value,
) {
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let mut types = BTreeSet::new();
    collect(&rt.bundle, schema, sel, 0, &mut types);
    let mut held = HashMap::new();
    for t in types {
        let own = t.to_lowercase();
        let mut records = call::stored_of_type(rt, session, &own, MAX_HELD).await;
        // What the mock this one shares stored, in this type's shape.
        let props = rt
            .bundle
            .defs
            .get(&t)
            .and_then(|d| d.get("properties"))
            .and_then(Value::as_object);
        if let Some(props) = props {
            for other in call::shared::equivalents(rt, &own) {
                for record in call::stored_of_type(rt, session, &other, MAX_HELD).await {
                    records.push(StoredResource {
                        resource_type: own.clone(),
                        data: call::shared::reshape(props, &rt.bundle.defs, &record.data),
                        id: record.id,
                    });
                }
            }
        }
        if !records.is_empty() {
            held.insert(t, records);
        }
    }
    // Which of several matching records a lookup takes must not depend on
    // what the query selects: count who refers to them across every type
    // whose members name a held type.
    let mut referred = Vec::new();
    for t in referring_types(&rt.bundle, &held) {
        for record in call::stored_of_type(rt, session, &t.to_lowercase(), MAX_HELD).await {
            referred.extend(refs(&record).into_iter().map(str::to_string));
        }
    }
    let mut walk = Walk {
        bundle: &rt.bundle,
        narrowed: rt.narrowed(),
        superseded: Vec::new(),
        held,
        referred,
    };
    // A root selection carries no arguments of its own.
    let sel = Selection {
        args,
        ..sel.clone()
    };
    walk.field(schema, &sel, body, None, 0);
    for (t, id) in walk.superseded {
        call::forget_stored(rt, session, &t, &id).await;
    }
}

/// The object types (not lists) with a member naming one of `held`'s types:
/// `Issue` for `Repository` (`Issue.repository`).
fn referring_types(bundle: &Bundle, held: &HashMap<String, Vec<StoredResource>>) -> Vec<String> {
    bundle
        .defs
        .iter()
        .filter(|(_, def)| {
            def.get("properties")
                .and_then(Value::as_object)
                .is_some_and(|props| {
                    props.values().any(|p| {
                        list_item(p).is_none() && def_ref(p).is_some_and(|n| held.contains_key(n))
                    })
                })
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// The keyed types `sel` reaches below `schema`.
fn collect(
    bundle: &Bundle,
    schema: &Value,
    sel: &Selection,
    depth: usize,
    out: &mut BTreeSet<String>,
) {
    if depth > MAX_DEPTH || sel.fields.is_empty() {
        return;
    }
    let (types, inner) = match shape(bundle, schema) {
        Shape::One(types) => (types, None),
        Shape::List(types, _) | Shape::Connection(types, _) => (types, None),
        Shape::Other => (Vec::new(), Some(schema)),
    };
    let mut defs: Vec<&Value> = types.iter().filter_map(|t| bundle.defs.get(t)).collect();
    out.extend(types);
    if let Some(schema) = inner {
        let item = list_item(schema).unwrap_or(schema);
        defs.extend(def_of(bundle, item));
    }
    for def in defs {
        let Some(props) = def.get("properties").and_then(Value::as_object) else {
            continue;
        };
        for child in sel.fields.values() {
            if let Some(field) = child.names().find_map(|n| props.get(n)) {
                collect(bundle, field, child, depth + 1, out);
            }
        }
    }
}

/// One page of a list answered from state.
struct Page {
    records: Vec<(String, StoredResource)>,
    /// The records that match, on every page.
    total: usize,
    has_previous: bool,
    has_next: bool,
}

struct Walk<'a> {
    bundle: &'a Bundle,
    narrowed: &'a super::select::NarrowCache,
    /// Records the engine stored from this answer that name one found.
    superseded: Vec<(String, String)>,
    /// By definition name, most recent first.
    held: HashMap<String, Vec<StoredResource>>,
    /// The ids the session's records of types that refer to a held type
    /// (`Issue.repository`) refer to, whether or not the query selects them.
    referred: Vec<String>,
}

impl Walk<'_> {
    /// `value`, the answer for a field of `schema` selected by `sel`.
    fn field(
        &mut self,
        schema: &Value,
        sel: &Selection,
        value: &mut Value,
        parent: Option<&Parent>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH || sel.fields.is_empty() {
            return;
        }
        // A null object read by arguments may still be one the session holds.
        if value.is_null()
            && (sel.args.is_empty() || !matches!(shape(self.bundle, schema), Shape::One(_)))
        {
            return;
        }
        match shape(self.bundle, schema) {
            Shape::One(types) => {
                // The engine may have stored this very answer already: it
                // is not the record the arguments name.
                let generated = parent.is_none().then(|| generated_id(value, sel)).flatten();
                if !sel.args.is_empty()
                    && let Some((ty, record)) =
                        self.find(&types, &sel.args, parent, generated.as_deref())
                {
                    let base = Some(&*value).filter(|v| v.is_object() && typename_is(v, &ty));
                    *value = self.render(&ty, &record, sel, base);
                    // What the engine stored of this answer is a second
                    // record of the one found: it goes.
                    // Only a copy: a held record the arguments do not name
                    // (the model answered another real one's id) stays.
                    if let Some(generated) = generated.filter(|g| *g != record.id)
                        && self.held.get(&ty).into_iter().flatten().any(|r| {
                            r.id == generated && matches(&sel.args, r) == (true, sel.args.len())
                        })
                    {
                        self.superseded.push((ty.to_lowercase(), generated));
                    }
                } else if parent.is_none()
                    && !sel.args.is_empty()
                    && let Value::Object(obj) = value
                    && !obj.contains_key("id")
                    && sel.fields.get("id").is_none_or(|c| c.name == "id")
                {
                    // A record read by its arguments without its id
                    // (`repository(owner:, name:) { issues { … } }`) gets
                    // one, unseen by the client, so that it is stored and
                    // what is listed under it is stored under it.
                    obj.insert(
                        "id".into(),
                        json!(format!("mb_{:016x}", rand::random::<u64>())),
                    );
                }
                self.object(schema, sel, value, parent, depth);
            }
            Shape::List(types, item) => {
                if let (Some(parent), Value::Array(items)) = (parent, &mut *value) {
                    self.list(&types, sel, sel, items, parent);
                }
                if let Value::Array(items) = value {
                    for v in items {
                        self.object(&item, sel, v, parent, depth);
                    }
                }
            }
            Shape::Connection(types, node) => {
                if let Some(parent) = parent {
                    self.connection(schema, &types, sel, value, parent);
                }
                self.connection_items(&node, sel, value, parent, depth);
            }
            Shape::Other => match value {
                Value::Array(items) => {
                    if let Some(item) = list_item(schema) {
                        for v in items {
                            self.field(item, sel, v, parent, depth);
                        }
                    }
                }
                Value::Object(_) => self.object(schema, sel, value, parent, depth),
                _ => {}
            },
        }
    }

    /// The id of an object of type `ty` answered without one, under
    /// `parent`, from a lookup key (`Issue.number`): the stored record's
    /// under that parent with the same key, else one made of it, so that
    /// every answer naming it names the same record. `None` when the answer
    /// holds no lookup key.
    fn id_by_keys(
        &self,
        ty: &str,
        sel: &Selection,
        obj: &Map<String, Value>,
        parent: &Parent,
    ) -> Option<String> {
        // The first lookup key the answer holds (keys sort by name), so
        // that every list naming the record names it alike.
        let (key, value) = self
            .narrowed
            .lookup_keys(self.bundle, ty)?
            .iter()
            .filter(|key| *key != "id")
            .find_map(|key| {
                let value = sel
                    .fields
                    .iter()
                    .find(|(_, c)| c.name == *key)
                    .and_then(|(k, _)| obj.get(k))
                    .filter(|v| !v.is_null() && !v.is_object() && !v.is_array())?;
                Some((key.clone(), value.clone()))
            })?;
        let mut values = Map::new();
        values.insert(key, value);
        if let Some(held) = self.held.get(ty).into_iter().flatten().find(|r| {
            refs(r).contains(&parent.id.as_str()) && matches(&values, r) == (true, values.len())
        }) {
            return Some(held.id.clone());
        }
        let named: Vec<String> = values
            .iter()
            .map(|(k, v)| match v {
                Value::String(v) => format!("{k}={v}"),
                v => format!("{k}={v}"),
            })
            .collect();
        Some(format!("mb_{ty}_{}_{}", parent.id, named.join("_")))
    }

    /// The children of `value`, an object of `schema`.
    fn object(
        &mut self,
        schema: &Value,
        sel: &Selection,
        value: &mut Value,
        parent: Option<&Parent>,
        depth: usize,
    ) {
        let Value::Object(obj) = value else {
            return;
        };
        let Some((name, def)) = object_type(self.bundle, schema, obj) else {
            return;
        };
        let Some(props) = def.get("properties").and_then(Value::as_object) else {
            return;
        };
        if props.contains_key("id")
            && !obj.contains_key("id")
            && let Some(parent) = parent
            && let Some(id) = self.id_by_keys(name, sel, obj, parent)
        {
            obj.insert("id".into(), json!(id));
        }
        let own = match obj.get("id") {
            Some(Value::String(id)) if props.contains_key("id") => Some(Parent {
                ty: name.to_string(),
                id: id.clone(),
            }),
            _ => None,
        };
        let parent = own.as_ref().or(parent);
        for (key, child) in &sel.fields {
            let (Some(member), Some(field)) =
                (obj.get_mut(key), child.names().find_map(|n| props.get(n)))
            else {
                continue;
            };
            self.field(field, child, member, parent, depth + 1);
        }
    }

    /// The items of a connection `value`, walked.
    fn connection_items(
        &mut self,
        node: &Value,
        sel: &Selection,
        value: &mut Value,
        parent: Option<&Parent>,
        depth: usize,
    ) {
        let Value::Object(obj) = value else {
            return;
        };
        for (key, child) in &sel.fields {
            match (child.name.as_str(), obj.get_mut(key)) {
                ("nodes", Some(Value::Array(items))) => {
                    for v in items {
                        self.object(node, child, v, parent, depth + 1);
                    }
                }
                ("edges", Some(Value::Array(edges))) => {
                    for edge in edges {
                        for (node_key, node_sel) in &child.fields {
                            if node_sel.name == "node"
                                && let Some(v) = edge.get_mut(node_key)
                            {
                                self.object(node, node_sel, v, parent, depth + 2);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The stored record of one of `types` that `args` name, among those
    /// under `parent` (or not known to be under another).
    fn find(
        &self,
        types: &[String],
        args: &Map<String, Value>,
        parent: Option<&Parent>,
        generated: Option<&str>,
    ) -> Option<(String, StoredResource)> {
        // Of several records the arguments name (one the session uses and a
        // copy written since, by a record pool or a model's answer), the one
        // the most held records refer to; the most recent among equals.
        let mut best: Option<(usize, &String, &StoredResource)> = None;
        for ty in types {
            for record in self.held.get(ty).into_iter().flatten() {
                if generated == Some(record.id.as_str()) {
                    continue;
                }
                if parent.is_some_and(|p| !self.loosely_under(record, p)) {
                    continue;
                }
                if let (true, compared) = matches(args, record)
                    && compared > 0
                {
                    let referred = self.referrers(&record.id);
                    if best.is_none_or(|(n, _, _)| referred > n) {
                        best = Some((referred, ty, record));
                    }
                }
            }
        }
        best.map(|(_, ty, record)| (ty.clone(), record.clone()))
    }

    /// How many of the session's records refer to the record `id`.
    fn referrers(&self, id: &str) -> usize {
        self.referred.iter().filter(|r| *r == id).count()
    }

    /// Whether `record` refers to `parent`, or to no record of its type.
    fn loosely_under(&self, record: &StoredResource, parent: &Parent) -> bool {
        let refs = refs(record);
        if refs.contains(&parent.id.as_str()) {
            return true;
        }
        let others = self.held.get(&parent.ty);
        !others.is_some_and(|o| o.iter().any(|r| refs.contains(&r.id.as_str())))
    }

    /// The records of `types` that refer to `parent`, most recent first.
    fn under(&self, types: &[String], parent: &Parent) -> Vec<(String, StoredResource)> {
        types
            .iter()
            .flat_map(|ty| {
                self.held
                    .get(ty)
                    .into_iter()
                    .flatten()
                    .filter(|r| refs(r).contains(&parent.id.as_str()))
                    .map(move |r| (ty.clone(), r.clone()))
            })
            .collect()
    }

    /// The page of records a list with `args` shows; `None` when the session
    /// holds none under `parent`. A record's cursor is its id: `after` and
    /// `before` start and end the page there, `first` and `last` cut it.
    fn listed(&self, types: &[String], args: &Map<String, Value>, parent: &Parent) -> Option<Page> {
        let held = self.under(types, parent);
        if held.is_empty() {
            return None;
        }
        let matching: Vec<_> = held
            .into_iter()
            .filter(|(_, r)| matches(args, r).0)
            .collect();
        let total = matching.len();
        let at = |name: &str| {
            let cursor = args.get(name)?.as_str()?;
            matching.iter().position(|(_, r)| r.id == cursor)
        };
        let mut start = at("after").map_or(0, |i| i + 1);
        let mut end = at("before").unwrap_or(total).max(start);
        let count = |name: &str| {
            args.get(name)
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
        };
        match (count("first"), count("last")) {
            (Some(n), _) => end = end.min(start + n),
            (None, Some(n)) => start = start.max(end.saturating_sub(n)),
            (None, None) => end = end.min(start + DEFAULT_PAGE),
        }
        Some(Page {
            records: matching[start..end].to_vec(),
            total,
            has_previous: start > 0,
            has_next: end < total,
        })
    }

    /// A plain list's `items` (selected by `item_sel`, read with `sel`'s
    /// arguments) from state, or its generated items the arguments allow.
    fn list(
        &self,
        types: &[String],
        sel: &Selection,
        item_sel: &Selection,
        items: &mut Vec<Value>,
        parent: &Parent,
    ) {
        match self.listed(types, &sel.args, parent) {
            Some(page) => *items = self.render_all(&page.records, item_sel, items),
            None => items.retain(|v| allowed(&sel.args, v, item_sel)),
        }
    }

    /// A connection `value` from state, or its generated items the
    /// arguments allow.
    fn connection(
        &self,
        schema: &Value,
        types: &[String],
        sel: &Selection,
        value: &mut Value,
        parent: &Parent,
    ) {
        let Value::Object(obj) = value else {
            return;
        };
        let Some(page) = self.listed(types, &sel.args, parent) else {
            for (key, child) in &sel.fields {
                match (child.name.as_str(), obj.get_mut(key)) {
                    ("nodes", Some(Value::Array(items))) => {
                        items.retain(|v| allowed(&sel.args, v, child));
                    }
                    ("edges", Some(Value::Array(edges))) => {
                        let node = child.fields.iter().find(|(_, s)| s.name == "node");
                        if let Some((key, node_sel)) = node {
                            edges.retain(|e| {
                                e.get(key).is_none_or(|v| allowed(&sel.args, v, node_sel))
                            });
                        }
                    }
                    _ => {}
                }
            }
            return;
        };
        let records = &page.records;
        let edge_ty = def_of(self.bundle, schema)
            .and_then(|d| d.get("properties")?.get("edges"))
            .and_then(list_item)
            .and_then(def_ref);
        for (key, child) in &sel.fields {
            match child.name.as_str() {
                "nodes" => {
                    let generated = match obj.get(key) {
                        Some(Value::Array(items)) => items.clone(),
                        _ => Vec::new(),
                    };
                    obj.insert(
                        key.clone(),
                        Value::Array(self.render_all(records, child, &generated)),
                    );
                }
                "edges" => {
                    let generated = match obj.get(key) {
                        Some(Value::Array(edges)) => edges.clone(),
                        _ => Vec::new(),
                    };
                    let mut edges = Vec::new();
                    for (i, (ty, record)) in records.iter().enumerate() {
                        let mut edge = match generated.get(i) {
                            Some(Value::Object(edge)) => edge.clone(),
                            _ => edge_ty.map(|t| self.fake(t, child)).unwrap_or_default(),
                        };
                        for (edge_key, edge_sel) in &child.fields {
                            match edge_sel.name.as_str() {
                                "cursor" => {
                                    edge.insert(edge_key.clone(), json!(record.id));
                                }
                                "node" => {
                                    let base = generated
                                        .iter()
                                        .filter_map(|e| e.get(edge_key))
                                        .find(|n| same_id(n, &record.id, edge_sel));
                                    let node = self.render(ty, record, edge_sel, base);
                                    edge.insert(edge_key.clone(), node);
                                }
                                _ => {}
                            }
                        }
                        let edge = Value::Object(edge);
                        edges.push(edge);
                    }
                    obj.insert(key.clone(), Value::Array(edges));
                }
                "totalCount" => {
                    obj.insert(key.clone(), json!(page.total));
                }
                "pageInfo" => {
                    if let Some(Value::Object(info)) = obj.get_mut(key) {
                        let cursor = |r: Option<&(String, StoredResource)>| {
                            r.map_or(Value::Null, |(_, r)| json!(r.id))
                        };
                        for (info_key, info_sel) in &child.fields {
                            let set = match info_sel.name.as_str() {
                                "hasNextPage" => json!(page.has_next),
                                "hasPreviousPage" => json!(page.has_previous),
                                "startCursor" => cursor(records.first()),
                                "endCursor" => cursor(records.last()),
                                _ => continue,
                            };
                            info.insert(info_key.clone(), set);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// `records` drawn into `sel`'s shape over `generated` items.
    fn render_all(
        &self,
        records: &[(String, StoredResource)],
        sel: &Selection,
        generated: &[Value],
    ) -> Vec<Value> {
        records
            .iter()
            .enumerate()
            .map(|(i, (ty, record))| {
                let base = generated
                    .iter()
                    .find(|v| same_id(v, &record.id, sel))
                    .or_else(|| generated.get(i % generated.len().max(1)))
                    .filter(|v| typename_is(v, ty));
                self.render(ty, record, sel, base)
            })
            .collect()
    }

    /// `record` (of definition `ty`) in `sel`'s shape: `base` (or the
    /// faker's object) under the record's members.
    fn render(
        &self,
        ty: &str,
        record: &StoredResource,
        sel: &Selection,
        base: Option<&Value>,
    ) -> Value {
        // Every selected field gets a value: the faker's, under the fullest
        // record of the type the session holds (a model's answer, so dates
        // read as dates), under `base`, under the record's own members.
        let mut out = self.fake(ty, sel);
        if let Some(template) = self
            .held
            .get(ty)
            .into_iter()
            .flatten()
            .filter(|r| r.id != record.id)
            .max_by_key(|r| r.data.as_object().map_or(0, Map::len))
        {
            // Its scalars only: another record's objects and lists are not
            // this one's.
            let scalars: Map<String, Value> = template
                .data
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(_, v)| !v.is_object() && !v.is_array())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            overlay(&mut out, &Value::Object(scalars), sel);
        }
        if let Some(Value::Object(base)) = base {
            for (key, value) in base {
                out.insert(key.clone(), value.clone());
            }
        }
        out.insert("__typename".into(), json!(ty));
        overlay(&mut out, &record.data, sel);
        for (key, _) in sel.fields.iter().filter(|(_, c)| c.name == "id") {
            out.insert(key.clone(), json!(record.id));
        }
        // Unselected, the id still names the record to what is below it
        // and to what the answer stores.
        if sel.fields.get("id").is_none_or(|c| c.name == "id") {
            out.insert("id".into(), json!(record.id));
        }
        Value::Object(out)
    }

    fn fake(&self, ty: &str, sel: &Selection) -> Map<String, Value> {
        let schema = narrow(
            &json!({ "$ref": format!("#/$defs/{ty}") }),
            &self.bundle.defs,
            sel,
        );
        match Faker::new(4).fake(&schema, &mut rand::rng()) {
            Value::Object(obj) => obj,
            _ => Map::new(),
        }
    }
}

/// `data` (keyed by field name) over `out` (keyed by `sel`'s response
/// keys), recursively through objects.
fn overlay(out: &mut Map<String, Value>, data: &Value, sel: &Selection) {
    for (key, child) in &sel.fields {
        if key == "__typename" {
            continue;
        }
        let Some(member) = child.names().find_map(|n| data.get(n)) else {
            continue;
        };
        match out.get_mut(key) {
            Some(Value::Object(inner)) if member.is_object() && !child.fields.is_empty() => {
                overlay(inner, member, child);
            }
            _ => {
                out.insert(key.clone(), rekey(member.clone(), child));
            }
        }
    }
}

/// The id of the generated object `value` (keyed by `sel`'s response keys).
fn generated_id(value: &Value, sel: &Selection) -> Option<String> {
    let key = sel
        .fields
        .iter()
        .find(|(_, c)| c.name == "id")
        .map_or("id", |(k, _)| k.as_str());
    value.get(key)?.as_str().map(str::to_string)
}

/// Whether `value` (an object) is untagged or tagged `ty`.
fn typename_is(value: &Value, ty: &str) -> bool {
    value.is_object()
        && value
            .get("__typename")
            .and_then(Value::as_str)
            .is_none_or(|t| t == ty)
}

/// Whether `value`, an object keyed by `sel`'s response keys, has `id`.
fn same_id(value: &Value, id: &str, sel: &Selection) -> bool {
    sel.fields
        .iter()
        .filter(|(_, c)| c.name == "id")
        .any(|(key, _)| value.get(key).and_then(Value::as_str) == Some(id))
}

/// Whether the generated `value` (keyed by `sel`'s response keys) is one
/// `args` allow.
fn allowed(args: &Map<String, Value>, value: &Value, sel: &Selection) -> bool {
    let data = by_field_name(value, sel);
    let id = data
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    matches(
        args,
        &StoredResource {
            resource_type: String::new(),
            id,
            data,
        },
    )
    .0
}

/// Whether every argument of `args` that names a member of `record` (or,
/// for a list argument, its singular member: `states` → `state`) agrees
/// with it, and how many did.
fn matches(args: &Map<String, Value>, record: &StoredResource) -> (bool, usize) {
    let mut compared = 0;
    for (name, arg) in args {
        if arg.is_null() {
            continue;
        }
        if name == "id" {
            compared += 1;
            if !loose_eq(arg, &json!(record.id)) {
                return (false, compared);
            }
            continue;
        }
        let member = record.data.get(name).or_else(|| {
            let singular = name.strip_suffix('s').filter(|_| arg.is_array())?;
            record.data.get(singular)
        });
        let Some(member) = member else {
            continue;
        };
        let agrees = match (arg, member) {
            (Value::Object(_), _) => None,
            (Value::Array(options), m) if !m.is_object() && !m.is_array() => {
                Some(options.iter().any(|o| loose_eq(o, m)))
            }
            (Value::Array(_), _) => None,
            (a, Value::Object(obj)) => Some(obj.values().any(|v| loose_eq(a, v))),
            (_, Value::Array(_)) => None,
            (a, m) => Some(loose_eq(a, m)),
        };
        if let Some(agrees) = agrees {
            compared += 1;
            if !agrees {
                return (false, compared);
            }
        }
    }
    (true, compared)
}

fn loose_eq(a: &Value, b: &Value) -> bool {
    fn text(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
    matches!((text(a), text(b)), (Some(x), Some(y)) if x == y)
}

/// The ids `record`'s object members hold (`repository: {id}`).
fn refs(record: &StoredResource) -> Vec<&str> {
    record
        .data
        .as_object()
        .into_iter()
        .flat_map(|obj| obj.values())
        .filter_map(|v| v.get("id")?.as_str())
        .collect()
}

/// Links each of `objects`, stored by a mutation, to the records its id
/// arguments name (`createIssue(input: {repositoryId})` → the issue's
/// `repository`), through a single-object member of the record's type that
/// the named record's type fits, unless the answer set it.
pub(crate) async fn link_args(
    rt: &MockRuntime,
    session: &SessionKey,
    objects: &mut [StoredResource],
    args: &Map<String, Value>,
) {
    let mut ids: Vec<String> = Vec::new();
    let nested = args.values().filter_map(Value::as_object).flatten();
    for (name, value) in args.iter().chain(nested) {
        if let Value::String(id) = value
            && (name.ends_with("Id") || name.ends_with("_id"))
            && !id.is_empty()
        {
            ids.push(id.clone());
        }
    }
    if ids.is_empty() {
        return;
    }
    let mut found: HashMap<(String, String), bool> = HashMap::new();
    for object in objects {
        let Some(props) = rt
            .bundle
            .defs
            .iter()
            .find(|(n, _)| n.to_lowercase() == object.resource_type)
            .and_then(|(_, def)| def.get("properties")?.as_object())
        else {
            continue;
        };
        for (prop, schema) in props {
            if list_item(schema).is_some() || object.data.get(prop).is_some_and(|v| !v.is_null()) {
                continue;
            }
            let types = keyed_types(&rt.bundle, schema);
            'link: for ty in &types {
                for id in &ids {
                    if *id == object.id {
                        continue;
                    }
                    let key = (ty.to_lowercase(), id.clone());
                    let held = match found.get(&key) {
                        Some(held) => *held,
                        None => {
                            let held = call::stored(rt, session, &key.0, id).await.is_some();
                            found.insert(key, held);
                            held
                        }
                    };
                    if held && let Value::Object(data) = &mut object.data {
                        data.insert(prop.clone(), json!({"__typename": ty, "id": id}));
                        break 'link;
                    }
                }
            }
        }
    }
}

/// Gives the records a mutation answers without their ids (gh's
/// `addComment { commentEdge { node { url } } }`) an id, unseen by the
/// client, so that they are stored with what the input says: the objects
/// of a type with an `id` nearest the answer's top.
pub(crate) fn name_written(
    bundle: &Bundle,
    op: &mb_mcm::Operation,
    sel: &Selection,
    args: &Map<String, Value>,
    body: &mut Value,
) {
    // An update names its record by id: nothing new is written.
    let names_one = |args: &Map<String, Value>| args.contains_key("id");
    if names_one(args) || args.values().filter_map(Value::as_object).any(names_one) {
        return;
    }
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let mut nearest = None;
    keyed_depth(bundle, schema, sel, body, 0, &mut |_, depth| {
        if nearest.is_none_or(|n| depth < n) {
            nearest = Some(depth);
        }
    });
    let Some(nearest) = nearest else {
        return;
    };
    keyed_depth(bundle, schema, sel, body, 0, &mut |obj, depth| {
        if depth == nearest && !obj.contains_key("id") {
            obj.insert(
                "id".into(),
                json!(format!("mb_{:016x}", rand::random::<u64>())),
            );
        }
    });
}

/// Calls `f` with every object of a type with an `id` in `value` (answered
/// for `schema` and `sel`) and its depth in objects.
fn keyed_depth(
    bundle: &Bundle,
    schema: &Value,
    sel: &Selection,
    value: &mut Value,
    depth: usize,
    f: &mut dyn FnMut(&mut Map<String, Value>, usize),
) {
    if depth > MAX_DEPTH || sel.fields.is_empty() {
        return;
    }
    match value {
        Value::Array(items) => {
            if let Some(item) = list_item(schema) {
                for v in items {
                    keyed_depth(bundle, item, sel, v, depth, f);
                }
            }
        }
        Value::Object(obj) => {
            let Some((_, def)) = object_type(bundle, schema, obj) else {
                return;
            };
            let Some(props) = def.get("properties").and_then(Value::as_object) else {
                return;
            };
            if props.contains_key("id") {
                f(obj, depth);
            }
            for (key, child) in &sel.fields {
                if let (Some(member), Some(field)) =
                    (obj.get_mut(key), child.names().find_map(|n| props.get(n)))
                {
                    keyed_depth(bundle, field, child, member, depth + 1, f);
                }
            }
        }
        _ => {}
    }
}
