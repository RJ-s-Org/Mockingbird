//! A `PATCH` body's `null`s that remove a member, as a JSON merge patch (RFC 7396)
//! has them. A `null` removes the member it is sent for when the request says it is
//! a merge patch (`Content-Type: application/merge-patch+json`), or when the request
//! body's schema admits `null` there and the answer's does not: a value the record can
//! never hold can only mean "remove it". Elsewhere a `null` is a value, which the
//! answer's schema keeps or refuses as it always has.
//!
//! The removal itself is a scenario's `set` rule ([`crate::apply_set`]): a merge patch
//! of `null`s at the places removed.

use mb_mcm::{Binding, Bundle, HttpMethod, Operation, OperationKind};
use serde_json::{Map, Value};

use crate::OperationCall;
use crate::consistency::{deref, member_property_schema};

/// How deep a request body's objects are followed for `null`s.
const MAX_DEPTH: usize = 16;

/// The places (member paths) whose `null` in `call`'s body removes the member: none
/// for an operation that is not a `PATCH` update, or a body with no `null`.
/// `answer_schema` is the answer's schema, its `$ref`s resolved through `bundle`.
pub(crate) fn removals(
    bundle: &Bundle,
    op: &Operation,
    call: &OperationCall,
    answer_schema: &Value,
) -> Vec<Vec<String>> {
    let patch = matches!(
        op.binding,
        Binding::Http {
            method: HttpMethod::Patch,
            ..
        }
    );
    let Some(Value::Object(body)) = call.body.as_ref() else {
        return Vec::new();
    };
    if !patch || matches!(op.kind, OperationKind::Query) {
        return Vec::new();
    }
    let walk = Walk {
        bundle,
        merge_patch: is_merge_patch(call),
    };
    let mut found = Vec::new();
    walk.visit(
        body,
        op.input.body.as_ref(),
        Some(answer_schema),
        &mut Vec::new(),
        &mut found,
    );
    found
}

/// Whether `call` says its body is a JSON merge patch.
fn is_merge_patch(call: &OperationCall) -> bool {
    call.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            && value.as_str().is_some_and(|v| {
                v.split(';').next().is_some_and(|t| {
                    t.trim()
                        .eq_ignore_ascii_case("application/merge-patch+json")
                })
            })
    })
}

/// `body` without the places `removed` names, so what reflects it sees no `null` there.
pub(crate) fn without(body: &Value, removed: &[Vec<String>]) -> Value {
    let mut out = body.clone();
    for path in removed {
        let Some((last, parents)) = path.split_last() else {
            continue;
        };
        let parent = parents
            .iter()
            .try_fold(&mut out, |at, step| at.get_mut(step.as_str()));
        if let Some(Value::Object(members)) = parent {
            members.remove(last);
        }
    }
    out
}

/// The merge patch of `null`s at the places `removed` names, which [`crate::apply_set`]
/// applies to remove them.
pub(crate) fn as_patch(removed: &[Vec<String>]) -> Value {
    let mut patch = Value::Object(Map::new());
    for path in removed {
        let Some((last, parents)) = path.split_last() else {
            continue;
        };
        let mut at = &mut patch;
        for step in parents {
            let Value::Object(members) = at else {
                break;
            };
            at = members
                .entry(step.clone())
                .or_insert_with(|| Value::Object(Map::new()));
        }
        if let Value::Object(members) = at {
            members.insert(last.clone(), Value::Null);
        }
    }
    patch
}

struct Walk<'a> {
    bundle: &'a Bundle,
    merge_patch: bool,
}

impl Walk<'_> {
    fn visit(
        &self,
        members: &Map<String, Value>,
        request: Option<&Value>,
        answer: Option<&Value>,
        path: &mut Vec<String>,
        found: &mut Vec<Vec<String>>,
    ) {
        if path.len() >= MAX_DEPTH {
            return;
        }
        for (key, value) in members {
            let sent = request.and_then(|s| self.place(s, key));
            let held = answer.and_then(|s| self.place(s, key));
            path.push(key.clone());
            match value {
                Value::Null if self.removes(sent.as_ref(), held.as_ref()) => {
                    found.push(path.clone());
                }
                Value::Object(inner) => {
                    self.visit(inner, sent.as_ref(), held.as_ref(), path, found);
                }
                _ => {}
            }
            path.pop();
        }
    }

    /// Whether a `null` sent where the request's schema is `sent` and the answer's is
    /// `held` removes the member.
    fn removes(&self, sent: Option<&Value>, held: Option<&Value>) -> bool {
        if self.merge_patch {
            return true;
        }
        match (sent, held) {
            (Some(sent), Some(held)) => self.admits_null(sent, 0) && !self.admits_null(held, 0),
            _ => false,
        }
    }

    /// The schema of member `key` of an object of schema `schema`: the member it
    /// declares, else what its `additionalProperties` says of every other member.
    fn place(&self, schema: &Value, key: &str) -> Option<Value> {
        member_property_schema(self.bundle, schema, key).or_else(|| {
            match deref(self.bundle, schema).get("additionalProperties")? {
                Value::Bool(true) => Some(Value::Object(Map::new())),
                other @ Value::Object(_) => Some(other.clone()),
                _ => None,
            }
        })
    }

    /// Whether `schema` admits `null`: by `nullable`, a `null` type, `const` or `enum`
    /// value, a `oneOf`/`anyOf` branch that does, or by saying nothing of the value's
    /// type.
    fn admits_null(&self, schema: &Value, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        let schema = deref(self.bundle, schema);
        let Value::Object(node) = schema else {
            return schema.as_bool().unwrap_or(false);
        };
        if node.get("nullable").and_then(Value::as_bool) == Some(true)
            || node.get("const") == Some(&Value::Null)
            || node
                .get("enum")
                .and_then(Value::as_array)
                .is_some_and(|values| values.contains(&Value::Null))
        {
            return true;
        }
        let branches = ["oneOf", "anyOf"]
            .iter()
            .filter_map(|k| node.get(*k).and_then(Value::as_array))
            .flatten()
            .collect::<Vec<_>>();
        if !branches.is_empty() {
            return branches.iter().any(|b| self.admits_null(b, depth + 1));
        }
        match node.get("type") {
            Some(Value::String(t)) => t == "null",
            Some(Value::Array(types)) => types.iter().any(|t| t == "null"),
            Some(_) => false,
            None => {
                !node.contains_key("enum")
                    && !node.contains_key("const")
                    && !node.contains_key("properties")
                    && !node.contains_key("$ref")
                    && node
                        .get("allOf")
                        .and_then(Value::as_array)
                        .is_none_or(|all| all.iter().all(|b| self.admits_null(b, depth + 1)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bundle() -> Bundle {
        mb_import::compile(
            br##"
openapi: 3.0.3
info: {title: t, version: "1"}
paths: {}
"##,
        )
        .unwrap()
    }

    fn found(body: Value, request: Value, answer: Value, merge_patch: bool) -> Vec<Vec<String>> {
        let bundle = bundle();
        let walk = Walk {
            bundle: &bundle,
            merge_patch,
        };
        let mut out = Vec::new();
        let Value::Object(body) = body else {
            panic!("an object body");
        };
        walk.visit(
            &body,
            Some(&request),
            Some(&answer),
            &mut Vec::new(),
            &mut out,
        );
        out
    }

    #[test]
    fn a_null_the_request_admits_and_the_record_does_not_removes_the_member() {
        let request =
            json!({"type": "object", "additionalProperties": {"type": "string", "nullable": true}});
        let answer = json!({"type": "object", "additionalProperties": {"type": "string"}});
        assert_eq!(
            found(json!({"A": null, "B": "x"}), request.clone(), answer, false),
            [vec!["A".to_owned()]]
        );
        // A record that may hold null keeps the null as a value.
        let nullable =
            json!({"type": "object", "additionalProperties": {"type": ["string", "null"]}});
        assert!(found(json!({"A": null}), request, nullable, false).is_empty());
    }

    #[test]
    fn a_merge_patch_removes_every_null_at_any_depth() {
        let open = json!({"type": "object", "additionalProperties": true});
        let answer = json!({"type": "object", "properties": {
            "meta": {"type": "object", "properties": {"tags": {"type": "object", "additionalProperties": {"type": "string"}}}},
        }});
        assert_eq!(
            found(json!({"meta": {"tags": {"t": null}}}), open, answer, true),
            [vec!["meta".to_owned(), "tags".to_owned(), "t".to_owned()]]
        );
    }

    #[test]
    fn a_null_the_request_does_not_admit_is_left_to_validation() {
        let request = json!({"type": "object", "properties": {"name": {"type": "string"}}});
        let answer = request.clone();
        assert!(found(json!({"name": null}), request, answer, false).is_empty());
    }

    #[test]
    fn the_patch_of_the_removals_removes_them_and_keeps_the_rest() {
        let removed = vec![vec!["a".to_owned(), "b".to_owned()], vec!["c".to_owned()]];
        let body = json!({"a": {"b": 1, "k": 2}, "c": 3, "d": 4});
        assert_eq!(
            crate::apply_set(&body, &as_patch(&removed)),
            json!({"a": {"k": 2}, "d": 4})
        );
        assert_eq!(
            without(&json!({"a": {"b": null, "k": 1}, "c": null}), &removed),
            json!({"a": {"k": 1}})
        );
    }
}
