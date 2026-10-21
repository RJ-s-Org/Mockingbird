//! A field's response schema narrowed to the query's selection set.
//!
//! The compiled schema of a root field reaches the whole type graph (GitHub's
//! `User` alone pulls in hundreds of types), so generating it and projecting
//! afterwards costs hundreds of thousands of prompt tokens and an answer no
//! model finishes in time. Narrowing keeps only the selected fields, inlines
//! the types they use and notes each field's arguments, so the model writes
//! exactly what the query reads.

use std::collections::BTreeMap;

use async_graphql::SelectionField;
use serde_json::{Map, Value, json};

/// The fields selected below one field, merged across fragments and aliases
/// (aliases of one field share its generated value), with their arguments.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Selection {
    pub fields: BTreeMap<String, Selection>,
    /// Literal argument values; for aliases with different arguments, the
    /// first one's.
    pub args: Map<String, Value>,
}

impl Selection {
    /// The selection below `field` (fragments flattened).
    pub(crate) fn of(field: &SelectionField<'_>) -> Self {
        let mut sel = Selection::default();
        for child in field.selection_set() {
            let entry = sel.fields.entry(child.name().to_string()).or_default();
            if entry.args.is_empty()
                && let Ok(args) = child.arguments()
            {
                for (name, value) in args {
                    entry
                        .args
                        .insert(name.to_string(), value.into_json().unwrap_or(Value::Null));
                }
            }
            entry.merge(Selection::of(&child));
        }
        sel
    }

    fn merge(&mut self, other: Selection) {
        for (name, child) in other.fields {
            self.fields.entry(name).or_default().merge(child);
        }
        for (name, value) in other.args {
            self.args.entry(name).or_insert(value);
        }
    }
}

/// `schema` with every object reduced to the properties `sel` names (and
/// `__typename`), `$ref`s into `defs` inlined. The result has no `$ref`.
/// Objects accept extra properties: projection ignores unselected fields, so
/// an answer that includes some is still a good one.
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
            "required" => Value::Array(
                value
                    .as_array()
                    .map(|names| {
                        names
                            .iter()
                            .filter(|n| n.as_str().is_some_and(|n| kept(n, sel)))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            "additionalProperties" => continue,
            _ => value.clone(),
        };
        out.insert(key.clone(), narrowed);
    }
    Value::Object(out)
}

fn kept(name: &str, sel: &Selection) -> bool {
    name == "__typename" || sel.fields.contains_key(name)
}

fn properties(value: &Value, defs: &Map<String, Value>, sel: &Selection) -> Map<String, Value> {
    let Some(props) = value.as_object() else {
        return Map::new();
    };
    props
        .iter()
        .filter(|(name, _)| kept(name, sel))
        .map(|(name, schema)| {
            let child = sel.fields.get(name);
            let mut narrowed = match child {
                Some(child) => narrow(schema, defs, child),
                None => schema.clone(),
            };
            if let (Some(child), Value::Object(obj)) = (child, &mut narrowed)
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
            (name.clone(), narrowed)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sel<const N: usize>(fields: [(&str, Selection); N]) -> Selection {
        Selection {
            fields: fields
                .into_iter()
                .map(|(n, s)| (n.to_string(), s))
                .collect(),
            args: Map::new(),
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
}
