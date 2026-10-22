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

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use async_graphql::SelectionField;
use serde_json::{Map, Value, json};

/// The fields selected below one field, merged across fragments, with their
/// arguments.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Selection {
    /// The field's name (empty for a hand-built root).
    pub name: String,
    /// Literal argument values (variables resolved).
    pub args: Map<String, Value>,
    /// Children by response key (alias or name), merged across fragments.
    pub fields: BTreeMap<String, Selection>,
    /// The response key of the first child, in document order, with the
    /// same name and arguments: this child is not generated, it copies
    /// that one's value.
    pub shares: Option<String>,
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
            let earlier = order[..i].iter().find(|k| {
                sel.fields.get(*k).is_some_and(|e| {
                    e.shares.is_none() && e.name == child.name && e.args == child.args
                })
            });
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

    /// Merges `other`'s children (by response key) and missing arguments.
    fn merge(&mut self, other: Selection) {
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
        if key != "__typename" && listed(&child.name) {
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
        let Some(schema) = props.get(&child.name) else {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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
}
