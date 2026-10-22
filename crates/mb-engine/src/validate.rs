use std::collections::HashSet;

use jsonschema::error::ValidationErrorKind;
use serde_json::Value;

use crate::EngineError;

const MAX_ERRORS: usize = 10;

/// JSON Schema (draft 2020-12) validator over a schema that carries its own
/// `$defs`.
pub struct SchemaValidator {
    inner: jsonschema::Validator,
    schema: Value,
}

impl SchemaValidator {
    pub fn new(schema_with_defs: &Value) -> Result<Self, EngineError> {
        let inner = jsonschema::draft202012::new(schema_with_defs)
            .map_err(|e| EngineError::Schema(e.to_string()))?;
        Ok(Self {
            inner,
            schema: schema_with_defs.clone(),
        })
    }

    /// `instance` with the errors a fresh value can fix patched locally: a
    /// `null` the schema forbids is removed where it is an optional property,
    /// else replaced by `fill` of the expected schema; a value failing
    /// `pattern`, `maxLength`, `minLength`, `enum`, `format` or `const` is
    /// replaced by `fill` of the schema holding that keyword. `fill` gets the
    /// schema with the root's `$defs`. `None` unless such errors are all that
    /// is wrong and the patched value is valid.
    pub fn patch(&self, instance: &Value, mut fill: impl FnMut(&Value) -> Value) -> Option<Value> {
        let mut nulls = Vec::new();
        let mut refills = Vec::new();
        for e in self.inner.iter_errors(instance) {
            let pointer = e.instance_path().as_str().to_string();
            let expected = || self.subschema(e.evaluation_path().as_str());
            match e.kind() {
                ValidationErrorKind::Type { .. } if e.instance().is_null() => {
                    nulls.push((pointer, expected()?));
                }
                ValidationErrorKind::Pattern { .. }
                | ValidationErrorKind::MaxLength { .. }
                | ValidationErrorKind::MinLength { .. }
                | ValidationErrorKind::Enum { .. }
                | ValidationErrorKind::Format { .. }
                | ValidationErrorKind::Constant { .. } => refills.push((pointer, expected()?)),
                _ => return None,
            }
        }
        if nulls.is_empty() && refills.is_empty() {
            return None;
        }
        let mut out = instance.clone();
        for (pointer, expected) in &refills {
            *out.pointer_mut(pointer)? = fill(expected);
        }
        for (pointer, _) in &nulls {
            let (parent, key) = pointer.rsplit_once('/')?;
            if let Some(Value::Object(map)) = out.pointer_mut(parent) {
                map.remove(&unescape(key));
            }
        }
        let missing: HashSet<String> = self
            .inner
            .iter_errors(&out)
            .filter_map(|e| match e.kind() {
                ValidationErrorKind::Required { property } => Some(format!(
                    "{}/{}",
                    e.instance_path().as_str(),
                    escape(property.as_str()?)
                )),
                _ => None,
            })
            .collect();
        for (pointer, expected) in &nulls {
            let dropped = out.pointer(pointer).is_none();
            if dropped && !missing.contains(pointer) {
                continue;
            }
            let value = fill(expected);
            let (parent, key) = pointer.rsplit_once('/')?;
            match out.pointer_mut(parent)? {
                Value::Object(map) => {
                    map.insert(unescape(key), value);
                }
                Value::Array(items) => *items.get_mut(key.parse::<usize>().ok()?)? = value,
                _ => return None,
            }
        }
        self.validate(&out).ok().map(|()| out)
    }

    /// The schema whose keyword failed at `evaluation_path` (which follows
    /// `$ref`s), with the root's `$defs`.
    fn subschema(&self, evaluation_path: &str) -> Option<Value> {
        let segments: Vec<String> = evaluation_path.split('/').skip(1).map(unescape).collect();
        let (_keyword, path) = segments.split_last()?;
        let mut node = &self.schema;
        for segment in path {
            node = match segment.as_str() {
                "$ref" => {
                    let name = node.get("$ref")?.as_str()?.strip_prefix("#/$defs/")?;
                    self.schema.get("$defs")?.get(name)?
                }
                _ => match node {
                    Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
                    _ => node.get(segment)?,
                },
            };
        }
        let mut expected = node.clone();
        if let (Value::Object(map), Some(defs)) = (&mut expected, self.schema.get("$defs")) {
            map.insert("$defs".into(), defs.clone());
        }
        Some(expected)
    }

    /// Returns up to 10 violations formatted as `"<instance path>: <message>"`.
    pub fn validate(&self, instance: &Value) -> Result<(), Vec<String>> {
        let errors: Vec<String> = self
            .inner
            .iter_errors(instance)
            .take(MAX_ERRORS)
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn unescape(segment: &str) -> String {
    segment.replace("~1", "/").replace("~0", "~")
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn validator() -> SchemaValidator {
        SchemaValidator::new(&json!({
            "$ref": "#/$defs/Movie",
            "$defs": {
                "Movie": {
                    "type": "object",
                    "required": ["title", "images"],
                    "properties": {
                        "title": {"type": "string"},
                        "collection": {"$ref": "#/$defs/Collection"},
                        "images": {"type": "array", "items": {"type": "string"}}
                    }
                },
                "Collection": {"type": "object", "properties": {"id": {"type": "integer"}}}
            }
        }))
        .unwrap()
    }

    fn fill(schema: &Value) -> Value {
        match schema.get("type").and_then(Value::as_str) {
            Some("string") => json!("filled"),
            Some("object") => json!({}),
            _ => json!(0),
        }
    }

    #[test]
    fn optional_nulls_are_dropped_and_required_ones_filled() {
        let v = validator();
        let out = v
            .patch(
                &json!({"title": null, "collection": null, "images": ["a", null]}),
                fill,
            )
            .unwrap();
        assert_eq!(out, json!({"title": "filled", "images": ["a", "filled"]}));
    }

    #[test]
    fn a_ref_target_is_filled_from_its_definition() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "required": ["c"],
            "properties": {"c": {"$ref": "#/$defs/C"}},
            "$defs": {"C": {"type": "object", "required": ["id"], "properties": {"id": {"type": "integer"}}}}
        }))
        .unwrap();
        let out = v
            .patch(&json!({"c": null}), |s| {
                assert!(s.get("$defs").is_some(), "{s}");
                json!({"id": 1})
            })
            .unwrap();
        assert_eq!(out, json!({"c": {"id": 1}}));
    }

    #[test]
    fn other_errors_are_not_patched() {
        let v = validator();
        assert!(
            v.patch(&json!({"title": 1, "collection": null, "images": []}), fill)
                .is_none()
        );
        assert!(
            v.patch(&json!({"title": "t", "images": []}), fill)
                .is_none(),
            "nothing to patch"
        );
    }

    #[test]
    fn pattern_length_enum_and_const_errors_are_refilled() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "pattern": "^/x$"},
                "code": {"type": "string", "maxLength": 2},
                "kind": {"enum": ["a", "b"]},
                "v": {"const": 1}
            }
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"path": "/y", "code": "long", "kind": "c", "v": 2}),
                |s| {
                    s.get("const")
                        .or_else(|| s.get("enum").and_then(|e| e.get(0)))
                        .cloned()
                        .unwrap_or_else(|| {
                            if s.get("pattern").is_some() {
                                json!("/x")
                            } else {
                                json!("ok")
                            }
                        })
                },
            )
            .unwrap();
        assert_eq!(
            out,
            json!({"path": "/x", "code": "ok", "kind": "a", "v": 1})
        );
    }
}
