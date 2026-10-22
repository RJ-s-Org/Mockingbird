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

    /// `instance` with every `null` the schema forbids patched: removed where
    /// it is an optional property, else replaced by `fill` of the expected
    /// schema (carrying the `$defs`). `None` unless such nulls are all that is
    /// wrong and the patched value is valid.
    pub fn patch_nulls(
        &self,
        instance: &Value,
        mut fill: impl FnMut(&Value) -> Value,
    ) -> Option<Value> {
        let mut nulls = Vec::new();
        for e in self.inner.iter_errors(instance) {
            if !(matches!(e.kind(), ValidationErrorKind::Type { .. }) && e.instance().is_null()) {
                return None;
            }
            let expected = self.subschema(e.evaluation_path().as_str())?;
            nulls.push((e.instance_path().as_str().to_string(), expected));
        }
        if nulls.is_empty() {
            return None;
        }
        let mut out = instance.clone();
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
            .patch_nulls(
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
            .patch_nulls(&json!({"c": null}), |s| {
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
            v.patch_nulls(&json!({"title": 1, "collection": null, "images": []}), fill)
                .is_none()
        );
        assert!(
            v.patch_nulls(&json!({"title": "t", "images": []}), fill)
                .is_none(),
            "nothing to patch"
        );
    }
}
