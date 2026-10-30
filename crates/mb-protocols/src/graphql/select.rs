//! A field's response schema narrowed to the query's selection set.
//!
//! The compiled schema of a root field reaches the whole type graph (GitHub's
//! `User` alone pulls in hundreds of types), so generating it and projecting
//! afterwards costs hundreds of thousands of prompt tokens and an answer no
//! model finishes in time. Narrowing keeps only the selected fields, inlines
//! the types they use and notes each field's arguments, so the model writes
//! exactly what the query reads.
//!
//! The narrowed schema is keyed by response key (alias or name): aliases of
//! one field with different arguments are separate properties and get
//! separate values. Aliases with the same name and arguments are generated
//! once and copied (see [`settle`]).

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use async_graphql::SelectionField;
use serde_json::{Map, Value, json};

/// The fields selected below one field, merged across fragments, with their
/// arguments.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    /// The field's name (empty for a hand-built root).
    pub name: String,
    /// Other field names under this response key, from fragments on other
    /// types (`... on Issue { label: title } ... on User { label: login }`):
    /// each object type reads the first of these names it has.
    pub also: Vec<String>,
    /// Literal argument values (variables resolved).
    pub args: Map<String, Value>,
    /// Children by response key (alias or name), merged across fragments.
    pub fields: BTreeMap<String, Selection>,
    /// The response key of the first child, in document order, with the
    /// same name and arguments: this child is not generated, it copies
    /// that one's value.
    pub shares: Option<String>,
}

/// Agrees with the derived `Eq`: arguments are hashed with their keys
/// sorted, as map equality ignores their order.
impl Hash for Selection {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.also.hash(state);
        crate::call::hash_canonical_members(&self.args, state);
        self.fields.hash(state);
        self.shares.hash(state);
    }
}

impl Selection {
    /// The selection below `field` (fragments flattened).
    pub(crate) fn of(field: &SelectionField<'_>) -> Self {
        let mut sel = Selection {
            name: field.name().to_string(),
            ..Selection::default()
        };
        // Response keys in document order of first appearance.
        let mut order: Vec<String> = Vec::new();
        for child in field.selection_set() {
            let key = child.alias().unwrap_or(child.name()).to_string();
            let entry = match sel.fields.entry(key) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    order.push(entry.key().clone());
                    let mut first = Selection {
                        name: child.name().to_string(),
                        ..Selection::default()
                    };
                    if let Ok(args) = child.arguments() {
                        for (name, value) in args {
                            first
                                .args
                                .insert(name.to_string(), value.into_json().unwrap_or(Value::Null));
                        }
                    }
                    entry.insert(first)
                }
            };
            entry.merge(Selection::of(&child));
        }
        for (i, key) in order.iter().enumerate() {
            let Some(child) = sel.fields.get(key) else {
                continue;
            };
            let earlier = order[..i]
                .iter()
                .find(|k| sel.fields.get(*k).is_some_and(|e| e.can_share(child)));
            let Some(earlier) = earlier.cloned() else {
                continue;
            };
            let below = Selection {
                fields: child.fields.clone(),
                ..Selection::default()
            };
            if let Some(target) = sel.fields.get_mut(&earlier) {
                target.merge(below);
            }
            if let Some(child) = sel.fields.get_mut(key) {
                child.shares = Some(earlier);
            }
        }
        sel
    }

    /// Whether `other` can copy this (generated) child's value: same field
    /// and arguments, and no response key below both that reads different
    /// fields.
    fn can_share(&self, other: &Selection) -> bool {
        self.shares.is_none()
            && self.name == other.name
            && self.also == other.also
            && self.args == other.args
            && compatible(self, other)
    }

    /// The field names this response key reads, first one first.
    fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.also.iter().map(String::as_str))
    }

    /// Whether any key below renames its field or copies another's value.
    pub(crate) fn aliased(&self) -> bool {
        self.fields.iter().any(|(key, c)| {
            *key != c.name || !c.also.is_empty() || c.shares.is_some() || c.aliased()
        })
    }

    /// Merges `other`'s field name (when it differs), children (by
    /// response key) and missing arguments.
    fn merge(&mut self, other: Selection) {
        for name in std::iter::once(other.name).chain(other.also) {
            if !name.is_empty() && name != self.name && !self.also.contains(&name) {
                self.also.push(name);
            }
        }
        for (key, child) in other.fields {
            match self.fields.entry(key) {
                Entry::Occupied(mut entry) => entry.get_mut().merge(child),
                Entry::Vacant(entry) => {
                    entry.insert(child);
                }
            }
        }
        for (name, value) in other.args {
            self.args.entry(name).or_insert(value);
        }
    }

    /// The children that are generated (not copies of another).
    fn generated(&self) -> impl Iterator<Item = (&String, &Selection)> {
        self.fields.iter().filter(|(_, c)| c.shares.is_none())
    }
}

/// Whether every response key below both `a` and `b` reads the same field
/// with the same arguments, recursively.
fn compatible(a: &Selection, b: &Selection) -> bool {
    a.fields.iter().all(|(key, x)| match b.fields.get(key) {
        Some(y) => x.name == y.name && x.also == y.also && x.args == y.args && compatible(x, y),
        None => true,
    })
}

/// `schema` with every object reduced to one property per response key `sel`
/// selects (and `__typename`), each with its field's schema, `$ref`s into
/// `defs` inlined. The result has no `$ref`. Objects accept extra
/// properties: projection ignores unselected fields, so an answer that
/// includes some is still a good one.
pub(crate) fn narrow(schema: &Value, defs: &Map<String, Value>, sel: &Selection) -> Value {
    let Value::Object(obj) = schema else {
        return schema.clone();
    };
    if let Some(target) = obj.get("$ref").and_then(Value::as_str) {
        return match target.strip_prefix("#/$defs/").and_then(|n| defs.get(n)) {
            Some(def) => narrow(def, defs, sel),
            None => json!({}),
        };
    }
    let mut out = Map::new();
    for (key, value) in obj {
        let narrowed = match key.as_str() {
            "anyOf" | "oneOf" | "allOf" => Value::Array(
                value
                    .as_array()
                    .map(|branches| branches.iter().map(|b| narrow(b, defs, sel)).collect())
                    .unwrap_or_default(),
            ),
            "items" => narrow(value, defs, sel),
            "properties" => Value::Object(properties(value, defs, sel)),
            "required" => Value::Array(required(value, sel)),
            "additionalProperties" => continue,
            _ => value.clone(),
        };
        out.insert(key.clone(), narrowed);
    }
    Value::Object(out)
}

/// Narrowed schemas the cache holds at most; it is emptied when full, as
/// selections are client-controlled.
const MAX_NARROWED: usize = 256;

/// Each operation's narrowed success schema by selection: a query sends the
/// same selection on every request, and narrowing GitHub's `viewer` to a
/// dozen fields takes longer than finding it again. Held by a runtime,
/// whose bundle is fixed, so the operation id is key enough.
#[derive(Debug, Default)]
pub(crate) struct NarrowCache(Mutex<Narrowed>);

#[derive(Debug, Default)]
struct Narrowed {
    by_op: HashMap<String, HashMap<Selection, Arc<Value>>>,
    len: usize,
}

impl NarrowCache {
    /// [`narrow`] of operation `op_id`'s `schema`, from the cache when it
    /// was narrowed to `sel` before. `schema` must be the same for every
    /// call with `op_id`.
    pub(crate) fn narrow(
        &self,
        op_id: &str,
        schema: &Value,
        defs: &Map<String, Value>,
        sel: &Selection,
    ) -> Arc<Value> {
        if let Ok(cache) = self.0.lock()
            && let Some(hit) = cache.by_op.get(op_id).and_then(|m| m.get(sel))
        {
            return hit.clone();
        }
        let narrowed = Arc::new(narrow(schema, defs, sel));
        if let Ok(mut cache) = self.0.lock() {
            if cache.len >= MAX_NARROWED {
                cache.by_op.clear();
                cache.len = 0;
            }
            let previous = cache
                .by_op
                .entry(op_id.to_string())
                .or_default()
                .insert(sel.clone(), narrowed.clone());
            if previous.is_none() {
                cache.len += 1;
            }
        }
        narrowed
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().map(|c| c.len).unwrap_or_default()
    }
}

/// `__typename` if listed, then every generated response key whose field
/// name is listed.
fn required(value: &Value, sel: &Selection) -> Vec<Value> {
    let listed = |name: &str| {
        value
            .as_array()
            .is_some_and(|names| names.iter().any(|n| n.as_str() == Some(name)))
    };
    let mut out = Vec::new();
    if listed("__typename") {
        out.push(json!("__typename"));
    }
    for (key, child) in sel.generated() {
        if key != "__typename" && child.names().any(listed) {
            out.push(json!(key));
        }
    }
    out
}

fn properties(value: &Value, defs: &Map<String, Value>, sel: &Selection) -> Map<String, Value> {
    let Some(props) = value.as_object() else {
        return Map::new();
    };
    let mut out = Map::new();
    if let Some(typename) = props.get("__typename") {
        out.insert("__typename".into(), typename.clone());
    }
    for (key, child) in sel.generated() {
        if key == "__typename" {
            continue;
        }
        // Another member of a union may not have this field.
        let Some(schema) = child.names().find_map(|n| props.get(n)) else {
            continue;
        };
        let mut narrowed = narrow(schema, defs, child);
        if let Value::Object(obj) = &mut narrowed
            && !child.args.is_empty()
        {
            let note = format!(
                "Queried with arguments {}",
                Value::Object(child.args.clone())
            );
            let description = match obj.get("description").and_then(Value::as_str) {
                Some(d) => format!("{d} {note}"),
                None => note,
            };
            obj.insert("description".into(), json!(description));
        }
        out.insert(key.clone(), narrowed);
    }
    out
}

/// A body keyed by field names (generated in full) re-keyed by response
/// key: each selected key takes its field name's member, recursively
/// through objects and lists; `__typename` is kept. A value whose
/// selection has no fields is returned unchanged.
pub(crate) fn rekey(value: Value, sel: &Selection) -> Value {
    if sel.fields.is_empty() {
        return value;
    }
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(|v| rekey(v, sel)).collect()),
        Value::Object(obj) => {
            let mut out = Map::new();
            if let Some(typename) = obj.get("__typename") {
                out.insert("__typename".into(), typename.clone());
            }
            for (key, child) in &sel.fields {
                if let Some(member) = child.names().find_map(|n| obj.get(n)) {
                    out.insert(key.clone(), rekey(member.clone(), child));
                }
            }
            Value::Object(out)
        }
        other => other,
    }
}

/// A narrowed body (keyed by response key) made whole. The engine overlays
/// the stored `target` of a read and the forced `identity` field by field
/// name; here each generated key takes its field's member of `target`
/// (re-keyed below), and each key reading the identity field takes the
/// body's member under that name. Then every child with `shares` gets a
/// copy of the shared key's value (recursively, through lists).
pub(crate) fn settle(
    value: &mut Value,
    sel: &Selection,
    target: Option<&Value>,
    identity: Option<&str>,
) {
    if let Value::Object(obj) = value {
        if let Some(Value::Object(stored)) = target {
            for (key, child) in sel.generated() {
                if let Some(member) = child.names().find_map(|n| stored.get(n)) {
                    obj.insert(key.clone(), rekey(member.clone(), child));
                }
            }
        }
        // The identity's member is ambiguous when another field's key took
        // its name.
        if let Some(field) = identity
            && sel.fields.get(field).is_none_or(|c| c.name == field)
            && let Some(id) = obj.get(field).cloned()
        {
            for (key, child) in sel.generated() {
                if child.name == field {
                    obj.insert(key.clone(), id.clone());
                }
            }
        }
    }
    copy_shared(value, sel);
}

fn copy_shared(value: &mut Value, sel: &Selection) {
    if sel.fields.is_empty() {
        return;
    }
    match value {
        Value::Array(items) => items.iter_mut().for_each(|v| copy_shared(v, sel)),
        Value::Object(obj) => {
            for (key, child) in sel.generated() {
                if let Some(member) = obj.get_mut(key) {
                    copy_shared(member, child);
                }
            }
            for (key, child) in &sel.fields {
                let Some(shared) = &child.shares else {
                    continue;
                };
                if let Some(member) = obj.get(shared) {
                    let member = member.clone();
                    obj.insert(key.clone(), member);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn sel<const N: usize>(fields: [(&str, Selection); N]) -> Selection {
        Selection {
            fields: fields
                .into_iter()
                .map(|(k, mut s)| {
                    if s.name.is_empty() {
                        s.name = k.to_string();
                    }
                    (k.to_string(), s)
                })
                .collect(),
            ..Selection::default()
        }
    }

    /// A child named `name` with `args`, below `inner`.
    fn aliased(name: &str, args: Value, inner: Selection) -> Selection {
        Selection {
            name: name.into(),
            args: args.as_object().cloned().unwrap_or_default(),
            ..inner
        }
    }

    fn leaf() -> Selection {
        Selection::default()
    }

    fn defs() -> Map<String, Value> {
        json!({
            "User": {
                "type": "object",
                "properties": {
                    "__typename": {"const": "User"},
                    "login": {"type": "string"},
                    "bio": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                    "repositories": {"$ref": "#/$defs/RepoConnection"},
                    "owner": {"$ref": "#/$defs/Owner"}
                },
                "required": ["__typename", "login", "repositories", "owner"],
                "additionalProperties": false
            },
            "RepoConnection": {
                "type": "object",
                "properties": {
                    "__typename": {"const": "RepoConnection"},
                    "totalCount": {"type": "integer"},
                    "nodes": {"type": "array", "items": {"$ref": "#/$defs/Repo"}}
                },
                "required": ["__typename", "totalCount", "nodes"]
            },
            "Repo": {
                "type": "object",
                "properties": {
                    "__typename": {"const": "Repo"},
                    "name": {"type": "string"},
                    "stars": {"type": "integer"}
                },
                "required": ["__typename", "name", "stars"]
            },
            "Owner": {"oneOf": [{"$ref": "#/$defs/User"}, {"$ref": "#/$defs/Org"}]},
            "Org": {
                "type": "object",
                "properties": {"__typename": {"const": "Org"}, "login": {"type": "string"}, "seats": {"type": "integer"}},
                "required": ["__typename", "login", "seats"]
            }
        })
        .as_object()
        .cloned()
        .unwrap_or_default()
    }

    #[test]
    fn keeps_only_selected_fields_and_inlines_refs() {
        let mut repos = sel([("totalCount", leaf()), ("nodes", sel([("name", leaf())]))]);
        repos.args.insert("first".into(), json!(100));
        let query = sel([("login", leaf()), ("repositories", repos)]);
        let out = narrow(&json!({"$ref": "#/$defs/User"}), &defs(), &query);
        assert!(!out.to_string().contains("$ref"), "{out}");
        assert_eq!(
            out["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["__typename", "login", "repositories"]
        );
        assert_eq!(
            out["required"],
            json!(["__typename", "login", "repositories"])
        );
        // Unselected fields in an answer are ignored by projection, so they
        // must not make it invalid.
        assert!(out.get("additionalProperties").is_none(), "{out}");
        let repo = &out["properties"]["repositories"];
        assert_eq!(
            repo["description"],
            "Queried with arguments {\"first\":100}"
        );
        let node = &repo["properties"]["nodes"]["items"];
        assert_eq!(node["required"], json!(["__typename", "name"]));
        assert!(node["properties"].get("stars").is_none());
    }

    #[test]
    fn abstract_types_narrow_every_member() {
        let query = sel([("owner", sel([("login", leaf())]))]);
        let out = narrow(&json!({"$ref": "#/$defs/User"}), &defs(), &query);
        let members = out["properties"]["owner"]["oneOf"].as_array().unwrap();
        assert_eq!(members.len(), 2);
        for m in members {
            assert_eq!(m["required"], json!(["__typename", "login"]), "{m}");
        }
    }

    #[test]
    fn nullable_wrappers_are_kept() {
        let query = sel([("bio", leaf())]);
        let out = narrow(&json!({"$ref": "#/$defs/User"}), &defs(), &query);
        assert_eq!(
            out["properties"]["bio"],
            json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
        );
    }

    #[test]
    fn aliases_with_different_arguments_get_their_own_properties() {
        let nodes = || sel([("nodes", sel([("name", leaf())]))]);
        let query = sel([
            (
                "small",
                aliased("repositories", json!({"first": 1}), nodes()),
            ),
            (
                "large",
                aliased("repositories", json!({"first": 5}), nodes()),
            ),
        ]);
        let out = narrow(&json!({"$ref": "#/$defs/User"}), &defs(), &query);
        let props: BTreeSet<&str> = out["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(props, BTreeSet::from(["__typename", "small", "large"]));
        let required: BTreeSet<&str> = out["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required, BTreeSet::from(["__typename", "small", "large"]));
        assert_eq!(
            out["properties"]["small"]["description"],
            "Queried with arguments {\"first\":1}"
        );
        assert_eq!(
            out["properties"]["large"]["description"],
            "Queried with arguments {\"first\":5}"
        );
        assert_eq!(
            out["properties"]["small"]["properties"]["nodes"]["items"]["required"],
            json!(["__typename", "name"])
        );
    }

    #[test]
    fn renamed_field_takes_its_field_schema() {
        let query = sel([("heading", aliased("login", json!({}), leaf()))]);
        let out = narrow(&json!({"$ref": "#/$defs/User"}), &defs(), &query);
        assert_eq!(out["properties"]["heading"], json!({"type": "string"}));
        assert!(out["properties"].get("login").is_none(), "{out}");
        assert_eq!(out["required"], json!(["__typename", "heading"]));
    }

    #[test]
    fn shared_aliases_emit_one_property() {
        let mut b = aliased("login", json!({}), leaf());
        b.shares = Some("a".into());
        let query = sel([("a", aliased("login", json!({}), leaf())), ("b", b)]);
        let out = narrow(&json!({"$ref": "#/$defs/User"}), &defs(), &query);
        assert!(out["properties"].get("b").is_none(), "{out}");
        assert_eq!(out["required"], json!(["__typename", "a"]));
    }

    #[test]
    fn rekey_maps_field_names_to_response_keys() {
        let query = sel([
            ("title", aliased("body", json!({}), leaf())),
            ("body", aliased("title", json!({}), leaf())),
        ]);
        let full = json!({"__typename": "Issue", "title": "T", "body": null, "state": "OPEN"});
        assert_eq!(
            rekey(full, &query),
            json!({"__typename": "Issue", "title": null, "body": "T"})
        );
        let names = sel([(
            "names",
            aliased(
                "nodes",
                json!({}),
                sel([("n", aliased("name", json!({}), leaf()))]),
            ),
        )]);
        assert_eq!(
            rekey(json!({"nodes": [{"name": "a"}, {"name": "b"}]}), &names),
            json!({"names": [{"n": "a"}, {"n": "b"}]})
        );
    }

    #[test]
    fn settle_copies_shared_aliases_and_reflects_stored_names() {
        let mut b = aliased("login", json!({}), leaf());
        b.shares = Some("a".into());
        let query = sel([
            ("a", aliased("login", json!({}), leaf())),
            ("b", b),
            ("ref", aliased("id", json!({}), leaf())),
        ]);
        let mut body = json!({"a": "octocat", "ref": "generated", "id": "stored"});
        settle(&mut body, &query, None, Some("id"));
        assert_eq!(body["b"], "octocat");
        assert_eq!(body["ref"], "stored");
    }

    #[test]
    fn one_key_with_different_fields_per_union_member() {
        let mut label = aliased("seats", json!({}), leaf());
        label.also = vec!["login".into()];
        let query = sel([("owner", sel([("label", label)]))]);
        let out = narrow(&json!({"$ref": "#/$defs/User"}), &defs(), &query);
        let members = out["properties"]["owner"]["oneOf"].as_array().unwrap();
        // User has no `seats`: its `label` is its `login`.
        assert_eq!(members[0]["properties"]["label"], json!({"type": "string"}));
        assert_eq!(members[0]["required"], json!(["__typename", "label"]));
        assert_eq!(
            members[1]["properties"]["label"],
            json!({"type": "integer"})
        );
        assert_eq!(
            rekey(
                json!({"__typename": "User", "login": "octo"}),
                &sel([("label", {
                    let mut l = aliased("seats", json!({}), leaf());
                    l.also = vec!["login".into()];
                    l
                })])
            ),
            json!({"__typename": "User", "label": "octo"})
        );
    }

    #[test]
    fn settle_rekeys_the_stored_target_and_the_identity() {
        let query = sel([
            ("title", aliased("body", json!({}), leaf())),
            ("body", aliased("title", json!({}), leaf())),
            (
                "repo",
                aliased(
                    "repository",
                    json!({}),
                    sel([("n", aliased("name", json!({}), leaf()))]),
                ),
            ),
            ("ref", aliased("id", json!({}), leaf())),
        ]);
        let stored = json!({"title": "T", "body": "B", "repository": {"name": "hello"}});
        let mut body =
            json!({"title": "x", "body": "y", "repo": {"n": "z"}, "ref": "r", "id": "I_1"});
        settle(&mut body, &query, Some(&stored), Some("id"));
        assert_eq!(
            body,
            json!({"title": "B", "body": "T", "repo": {"n": "hello"}, "ref": "I_1", "id": "I_1"})
        );
        // Without a stored target or identity, a model's extra member does
        // not replace an alias.
        let mut body = json!({"ref": "r", "id": "other"});
        settle(&mut body, &query, None, None);
        assert_eq!(body["ref"], "r");
    }

    #[test]
    fn narrow_cache_answers_like_narrow_once_per_selection() {
        let cache = NarrowCache::default();
        let schema = json!({"$ref": "#/$defs/User"});
        let defs = defs();
        let mut repos = sel([("totalCount", leaf())]);
        repos.args.insert("first".into(), json!(1));
        let first = sel([("login", leaf()), ("repositories", repos.clone())]);
        repos.args.insert("first".into(), json!(5));
        let five = sel([("login", leaf()), ("repositories", repos)]);

        let a = cache.narrow("viewer", &schema, &defs, &first);
        assert_eq!(*a, narrow(&schema, &defs, &first));
        // The same selection again, built anew: the same schema.
        let again = cache.narrow("viewer", &schema, &defs, &first.clone());
        assert!(Arc::ptr_eq(&a, &again));
        // Other arguments, or another operation: narrowed on their own.
        let b = cache.narrow("viewer", &schema, &defs, &five);
        assert_eq!(*b, narrow(&schema, &defs, &five));
        assert_ne!(a, b);
        let c = cache.narrow("user", &schema, &defs, &first);
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn narrow_cache_is_bounded() {
        let cache = NarrowCache::default();
        let schema = json!({"$ref": "#/$defs/User"});
        let defs = defs();
        for i in 0..(MAX_NARROWED + 10) {
            let mut repos = sel([("totalCount", leaf())]);
            repos.args.insert("first".into(), json!(i));
            let query = sel([("repositories", repos)]);
            let narrowed = cache.narrow("viewer", &schema, &defs, &query);
            assert_eq!(*narrowed, narrow(&schema, &defs, &query));
            assert!(cache.len() <= MAX_NARROWED);
        }
    }

    #[test]
    fn selection_hash_agrees_with_equality() {
        use std::hash::DefaultHasher;
        let hash = |s: &Selection| {
            let mut hasher = DefaultHasher::new();
            s.hash(&mut hasher);
            hasher.finish()
        };
        let mut a = aliased("repositories", json!({"first": 1, "after": "x"}), leaf());
        let mut b = aliased("repositories", json!({"after": "x", "first": 1}), leaf());
        assert_eq!(a, b);
        assert_eq!(hash(&a), hash(&b));
        b.args.insert("first".into(), json!(2));
        assert_ne!(hash(&a), hash(&b));
        b.args.insert("first".into(), json!(1));
        a.shares = Some("other".into());
        assert_ne!(hash(&a), hash(&b));
    }

    /// The selection of up to `width` members of `schema`'s object (three
    /// below that), `depth` levels down.
    fn first_members(
        schema: &Value,
        defs: &Map<String, Value>,
        width: usize,
        depth: usize,
    ) -> Selection {
        let resolve = |v: &Value| match v.get("$ref").and_then(Value::as_str) {
            Some(r) => defs
                .get(r.trim_start_matches("#/$defs/"))
                .cloned()
                .unwrap_or_default(),
            None => v.clone(),
        };
        let object = resolve(schema);
        let object = resolve(object.get("items").unwrap_or(&object));
        let mut out = Selection::default();
        if depth == 0 {
            return out;
        }
        if let Some(props) = object.get("properties").and_then(Value::as_object) {
            for (key, member) in props.iter().take(width) {
                let mut child = first_members(member, defs, 3, depth - 1);
                child.name = key.clone();
                out.fields.insert(key.clone(), child);
            }
        }
        out
    }

    /// Timing harness for what a GraphQL root field narrows and hashes per
    /// request: `cargo test -p mockingbird-protocols --lib --release
    /// --all-features -- --ignored --nocapture narrow_timing`.
    #[test]
    #[ignore = "timing harness"]
    fn narrow_timing() {
        use std::hash::DefaultHasher;
        let path = format!(
            "{}/../../examples/apis/github-graphql/github.graphql",
            env!("CARGO_MANIFEST_DIR")
        );
        let bundle = mb_import::compile(&std::fs::read(path).unwrap()).unwrap();
        let op = bundle
            .operations
            .iter()
            .find(|o| o.id == "Query.viewer")
            .unwrap();
        let schema = op.success_output().and_then(|o| o.schema.clone()).unwrap();
        let query = first_members(&schema, &bundle.defs, 12, 3);
        let cache = NarrowCache::default();
        let n = 2000u32;
        let time = |f: &dyn Fn()| {
            let started = std::time::Instant::now();
            for _ in 0..n {
                f();
            }
            started.elapsed() / n
        };
        let narrowed = narrow(&schema, &bundle.defs, &query);
        let fresh = time(&|| {
            std::hint::black_box(narrow(&schema, &bundle.defs, &query));
        });
        let cached = time(&|| {
            std::hint::black_box(cache.narrow(&op.id, &schema, &bundle.defs, &query));
        });
        let size = narrowed.to_string().len();
        println!(
            "{} ({size} bytes narrowed): narrow {fresh:?}, cache {cached:?}",
            op.id
        );
        let printed = time(&|| {
            let mut hasher = DefaultHasher::new();
            crate::call::canonical(&narrowed)
                .to_string()
                .hash(&mut hasher);
            std::hint::black_box(hasher.finish());
        });
        let walked = time(&|| {
            let mut hasher = DefaultHasher::new();
            crate::call::hash_canonical(&narrowed, &mut hasher);
            std::hint::black_box(hasher.finish());
        });
        println!("cache key hash: canonical text {printed:?}, walk {walked:?}");
    }
}
