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
    /// `pattern`, `maxLength`, `minLength`, `enum`, `format`, `const` or a
    /// numeric range (`minimum`, `maximum`, their exclusive forms,
    /// `multipleOf`) is
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
                | ValidationErrorKind::Constant { .. }
                | ValidationErrorKind::Minimum { .. }
                | ValidationErrorKind::Maximum { .. }
                | ValidationErrorKind::ExclusiveMinimum { .. }
                | ValidationErrorKind::ExclusiveMaximum { .. }
                | ValidationErrorKind::MultipleOf { .. } => refills.push((pointer, expected()?)),
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

    /// `instance` with every missing required property that `wanted`
    /// accepts set to `fill` of its schema (with the root's `$defs`).
    /// `wanted` gets the location of the object schema (a JSON pointer where
    /// a `$ref` restarts at its target, e.g. `/$defs/user`) and the
    /// property. `None` when nothing was filled.
    pub fn fill_required(
        &self,
        instance: &Value,
        mut wanted: impl FnMut(&str, &str) -> bool,
        mut fill: impl FnMut(&Value) -> Value,
    ) -> Option<Value> {
        let mut out = instance.clone();
        let mut filled = false;
        // A filled value may itself lack a member the next round fills.
        for _ in 0..4 {
            let mut missing = Vec::new();
            for e in self.inner.iter_errors(&out) {
                let ValidationErrorKind::Required { property } = e.kind() else {
                    continue;
                };
                let Some(property) = property.as_str() else {
                    continue;
                };
                let Some(location) = self.location(e.evaluation_path().as_str()) else {
                    continue;
                };
                if !wanted(&location, property) {
                    continue;
                }
                let Some(schema) = self.at(&format!("{location}/properties/{}", escape(property)))
                else {
                    continue;
                };
                missing.push((
                    e.instance_path().as_str().to_string(),
                    property.to_string(),
                    schema,
                ));
            }
            if missing.is_empty() {
                break;
            }
            for (pointer, property, schema) in missing {
                if let Some(Value::Object(map)) = out.pointer_mut(&pointer)
                    && !map.contains_key(&property)
                {
                    map.insert(property, fill(&schema));
                    filled = true;
                }
            }
        }
        filled.then_some(out)
    }

    /// The location of the schema holding the keyword that failed at
    /// `evaluation_path`: a JSON pointer into the schema that restarts at a
    /// `$ref`'s target.
    fn location(&self, evaluation_path: &str) -> Option<String> {
        let segments: Vec<String> = evaluation_path.split('/').skip(1).map(unescape).collect();
        let (_keyword, path) = segments.split_last()?;
        let mut location = String::new();
        let mut node = &self.schema;
        for segment in path {
            if segment == "$ref" {
                let target = node.get("$ref")?.as_str()?.strip_prefix('#')?;
                location = target.to_string();
                node = self.schema.pointer(target)?;
            } else {
                node = match node {
                    Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
                    _ => node.get(segment)?,
                };
                location.push('/');
                location.push_str(&escape(segment));
            }
        }
        Some(location)
    }

    /// The property names the root object schema declares, following
    /// `$ref`s and the branches of `allOf`/`anyOf`/`oneOf`; `None` when it
    /// declares none (a free-form object).
    pub fn declared_properties(&self) -> Option<HashSet<String>> {
        let mut names = HashSet::new();
        let mut pending = vec![&self.schema];
        let mut hops = 0;
        while let Some(node) = pending.pop() {
            hops += 1;
            if hops > MAX_SCHEMA_HOPS {
                break;
            }
            if let Some(target) = node
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix('#'))
                .and_then(|pointer| self.schema.pointer(pointer))
            {
                pending.push(target);
            }
            if let Some(Value::Object(props)) = node.get("properties") {
                names.extend(props.keys().cloned());
            }
            for keyword in ["allOf", "anyOf", "oneOf"] {
                if let Some(Value::Array(branches)) = node.get(keyword) {
                    pending.extend(branches.iter());
                }
            }
        }
        (!names.is_empty()).then_some(names)
    }

    /// The schema at `location`, with the root's `$defs`.
    fn at(&self, location: &str) -> Option<Value> {
        let mut node = self.schema.pointer(location)?.clone();
        if let (Value::Object(map), Some(defs)) = (&mut node, self.schema.get("$defs")) {
            map.insert("$defs".into(), defs.clone());
        }
        Some(node)
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

    /// Returns up to 10 violations formatted as `"<instance path>:
    /// <keyword>: <message>"`. A failed `anyOf`/`oneOf` names the errors of
    /// its closest branch; quoted values are capped.
    pub fn validate(&self, instance: &Value) -> Result<(), Vec<String>> {
        let errors: Vec<String> = self
            .inner
            .iter_errors(instance)
            .take(MAX_ERRORS)
            .map(|e| describe(&e, 0))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Longest message quoted for one violation; a longer one (it quotes a big
/// value) is told with the value masked.
const MAX_MESSAGE_CHARS: usize = 160;

/// Branch errors named for a failed `anyOf`/`oneOf`.
const MAX_BRANCH_ERRORS: usize = 3;

/// Nesting of `anyOf`/`oneOf` followed into a branch.
const MAX_BRANCH_DEPTH: usize = 4;

/// `"<pointer>: <keyword>: <message>"`; for a failed `anyOf`/`oneOf`, the
/// errors of the branch that got furthest into the value (the deepest
/// pointer, then the fewest errors).
fn describe(e: &jsonschema::ValidationError<'_>, depth: usize) -> String {
    let pointer = e.instance_path().as_str();
    let keyword = e.kind().keyword();
    if let ValidationErrorKind::AnyOf { context } | ValidationErrorKind::OneOfNotValid { context } =
        e.kind()
        && depth < MAX_BRANCH_DEPTH
        && let Some(branch) = context.iter().filter(|b| !b.is_empty()).max_by_key(|b| {
            let deepest = b
                .iter()
                .map(|e| e.instance_path().as_str().matches('/').count())
                .max()
                .unwrap_or(0);
            (deepest, std::cmp::Reverse(b.len()))
        })
    {
        let mut parts: Vec<String> = branch
            .iter()
            .take(MAX_BRANCH_ERRORS)
            .map(|e| describe(e, depth + 1))
            .collect();
        if branch.len() > MAX_BRANCH_ERRORS {
            parts.push(format!("{} more", branch.len() - MAX_BRANCH_ERRORS));
        }
        return format!(
            "{pointer}: {keyword}: no branch matches; closest: {}",
            parts.join(", ")
        );
    }
    let mut message = e.to_string();
    if message.chars().count() > MAX_MESSAGE_CHARS {
        message = e.masked().to_string();
    }
    if message.chars().count() > MAX_MESSAGE_CHARS {
        message = message.chars().take(MAX_MESSAGE_CHARS).collect::<String>() + "…";
    }
    format!("{pointer}: {keyword}: {message}")
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
    fn wanted_missing_members_are_filled_through_refs() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "required": ["items"],
            "properties": {"items": {"type": "array", "items": {"$ref": "#/$defs/Item"}}},
            "$defs": {
                "Item": {"type": "object", "required": ["sha", "author"], "properties": {
                    "sha": {"type": "string"},
                    "author": {"$ref": "#/$defs/User"}
                }},
                "User": {"type": "object", "required": ["login"], "properties": {"login": {"type": "string"}}}
            }
        }))
        .unwrap();
        let mut seen = Vec::new();
        let out = v
            .fill_required(
                &json!({"items": [{"sha": "a"}, {"sha": "b"}]}),
                |loc, p| {
                    seen.push(format!("{loc}#{p}"));
                    loc == "/$defs/Item" && p == "author"
                },
                |s| {
                    assert!(s.get("$defs").is_some());
                    json!({"login": "x"})
                },
            )
            .unwrap();
        assert_eq!(out["items"][1]["author"], json!({"login": "x"}));
        assert!(v.validate(&out).is_ok());
        assert!(seen.contains(&"/$defs/Item#author".to_string()), "{seen:?}");
        // Members the caller did not leave out stay missing.
        assert!(
            v.fill_required(
                &json!({"items": [{"author": {"login": "x"}}]}),
                |_, p| p == "author",
                |_| json!(1)
            )
            .is_none()
        );
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

    #[test]
    fn errors_name_the_deepest_branch_pointer_and_keyword() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "owner": {"anyOf": [{"type": "null"}, {"$ref": "#/$defs/User"}]}
            },
            "$defs": {
                "User": {
                    "type": "object",
                    "required": ["login"],
                    "properties": {"login": {"type": "string"}, "id": {"type": "integer"}}
                }
            }
        }))
        .unwrap();
        let big = "x".repeat(5_000);
        let errors = v
            .validate(&json!({"owner": {"login": 7, "id": 1, "bio": big}}))
            .unwrap_err();
        assert_eq!(errors.len(), 1, "{errors:?}");
        let e = &errors[0];
        assert!(e.starts_with("/owner: anyOf: "), "{e}");
        assert!(e.contains("/owner/login: type: 7 is not of type \"string\""), "{e}");
        assert!(e.len() < 400, "{e}");
    }

    #[test]
    fn long_values_are_not_quoted() {
        let v = SchemaValidator::new(&json!({"type": "integer"})).unwrap();
        let errors = v.validate(&json!({"text": "y".repeat(5_000)})).unwrap_err();
        assert!(errors[0].starts_with(": type: "), "{errors:?}");
        assert!(errors[0].len() < 300, "{errors:?}");
    }
}
