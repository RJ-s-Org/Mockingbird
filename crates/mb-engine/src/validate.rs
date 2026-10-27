use std::collections::HashSet;

use jsonschema::error::ValidationErrorKind;
use serde_json::Value;

use crate::EngineError;

const MAX_ERRORS: usize = 10;

/// A string longer than this, where the schema sets no `maxLength`, is a
/// runaway generation.
pub const RUNAWAY_CHARS: usize = 2_000;

/// What a runaway string is cut to.
pub const TRUNCATED_CHARS: usize = 200;

/// Schema nodes visited when looking up the schema of one location.
const MAX_SCHEMA_HOPS: usize = 64;

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

    /// `instance` with every string longer than [`RUNAWAY_CHARS`] whose
    /// schema location declares no `maxLength` cut to its first
    /// [`TRUNCATED_CHARS`] characters; `None` unless something was cut and
    /// the result validates.
    pub fn truncate_runaway(&self, instance: &Value) -> Option<Value> {
        let mut long = Vec::new();
        collect_long_strings(instance, String::new(), &mut long);
        let mut out = instance.clone();
        let mut cut = false;
        for pointer in long {
            if self.declares_max_length(&pointer) {
                continue;
            }
            if let Some(Value::String(text)) = out.pointer_mut(&pointer) {
                *text = text.chars().take(TRUNCATED_CHARS).collect();
                cut = true;
            }
        }
        (cut && self.validate(&out).is_ok()).then_some(out)
    }

    /// Whether any schema reachable for the instance location `pointer`
    /// (through `$ref`, `allOf`, `oneOf`, `anyOf`) declares `maxLength`.
    fn declares_max_length(&self, pointer: &str) -> bool {
        let mut nodes = self.expand(vec![&self.schema]);
        for segment in pointer.split('/').skip(1).map(unescape) {
            let index = segment.parse::<usize>().ok();
            let next: Vec<&Value> = nodes
                .iter()
                .filter_map(|node| {
                    let property = node.get("properties").and_then(|p| p.get(&segment));
                    let additional = node.get("additionalProperties").filter(|a| a.is_object());
                    let item = index.and_then(|i| {
                        node.get("prefixItems")
                            .and_then(|p| p.get(i))
                            .or_else(|| node.get("items"))
                    });
                    property.or(item).or(additional)
                })
                .collect();
            nodes = self.expand(next);
            if nodes.is_empty() {
                return false;
            }
        }
        nodes.iter().any(|node| node.get("maxLength").is_some())
    }

    /// `nodes` and every schema they reach through `$ref` (to `$defs`),
    /// `allOf`, `oneOf` and `anyOf`, bounded.
    fn expand<'a>(&'a self, mut pending: Vec<&'a Value>) -> Vec<&'a Value> {
        let mut out = Vec::new();
        while let Some(node) = pending.pop() {
            if out.len() >= MAX_SCHEMA_HOPS {
                break;
            }
            out.push(node);
            if let Some(target) = node
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/$defs/"))
                .and_then(|name| self.schema.get("$defs")?.get(name))
            {
                pending.push(target);
            }
            for key in ["allOf", "oneOf", "anyOf"] {
                if let Some(Value::Array(members)) = node.get(key) {
                    pending.extend(members);
                }
            }
        }
        out
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

/// Pointers of every string in `value` longer than [`RUNAWAY_CHARS`].
fn collect_long_strings(value: &Value, pointer: String, out: &mut Vec<String>) {
    match value {
        Value::String(text) if text.chars().count() > RUNAWAY_CHARS => out.push(pointer),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                collect_long_strings(item, format!("{pointer}/{i}"), out);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                collect_long_strings(item, format!("{pointer}/{}", escape(key)), out);
            }
        }
        _ => {}
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
