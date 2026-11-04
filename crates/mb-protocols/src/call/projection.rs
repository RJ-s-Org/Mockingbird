//! A stored read cut to the members a declared projection parameter asks for
//! (Directus's `fields=id,name`, a `fields` array), as the model honours it.
//!
//! A query parameter projects only when the contract ties it to the record's member
//! names by structure:
//!
//! - an array whose items' `enum` lists member names of the record, or
//! - an array or a string whose examples are member names: the schema's `example` or
//!   `examples`, or a code span of its description (`` `id,name` ``), comma-separated,
//!   of two or more tokens each a member name of the record (the head before a `.`).
//!
//! A sort (`` `-price,name` ``) or a choice of something else names no member list and
//! ties nothing. The request's value is then comma tokens (or an array's items), each
//! naming a declared member by its head; one naming none, or `*`, cuts nothing. A
//! plain token keeps its member whole; a dotted one (`dimensions.width`) goes through a
//! member only where the stored record holds an object there, and keeps a member
//! holding anything else as it is. A member the record does not hold is left out.
//!
//! Only a single stored record's read, served from state, is cut: never a list, and
//! nothing is written. One parameter projects at most: two that tie leave the read
//! whole. A cut answer that no longer fits the read's schema is not served, and the
//! whole record is (see the caller).

use super::*;

/// Shortest member list an example must show to tie a parameter.
const MIN_EXAMPLE_TOKENS: usize = 2;
/// Tokens of a request's value read, at most.
const MAX_TOKENS: usize = 64;
/// How deep a dotted token goes.
const MAX_DEPTH: usize = 4;

/// `stored`, cut to what `call`'s projection parameter asks for (see the module's
/// doc); `None` when nothing ties or nothing would be cut.
pub(super) fn cut(
    rt: &MockRuntime,
    op: &Operation,
    call: &OperationCall,
    stored: &StoredResource,
) -> Option<StoredResource> {
    if !matches!(op.kind, OperationKind::Query) || call.query.is_empty() {
        return None;
    }
    let Value::Object(members) = &stored.data else {
        return None;
    };
    let bundle = rt.bundle();
    let schema = write_memory::read_schema(rt, &stored.resource_type)?;
    let (parts, _) = schema_parts(bundle, Some(schema));
    let declared = member_names(&parts);
    let params = op.input.query.as_ref()?.get("properties")?.as_object()?;
    let mut asked = None;
    for (name, raw) in &call.query {
        let Some(param) = params.get(name) else {
            continue;
        };
        if !ties(bundle, param, &declared) {
            continue;
        }
        if asked.is_some() {
            return None;
        }
        asked = Some(tokens(raw)?);
    }
    let asked = asked?;
    if asked.iter().any(|token| {
        token == "*" || !declared.contains(&token.split('.').next().unwrap_or(token))
    }) {
        return None;
    }
    let paths: Vec<Vec<&str>> = asked.iter().map(|t| t.split('.').collect()).collect();
    let data = Value::Object(cut_object(members, &paths, 0));
    (data != stored.data).then(|| StoredResource {
        data,
        ..stored.clone()
    })
}

/// The member names `parts` declare.
fn member_names<'a>(parts: &[&'a Value]) -> Vec<&'a str> {
    let mut names = Vec::new();
    for name in parts
        .iter()
        .filter_map(|p| p.get("properties")?.as_object())
        .flat_map(|p| p.keys())
    {
        if !names.contains(&name.as_str()) {
            names.push(name.as_str());
        }
    }
    names
}

/// The tokens of a request's value: an array's items, or comma-separated text.
fn tokens(raw: &Value) -> Option<Vec<String>> {
    let texts: Vec<String> = match raw {
        Value::Array(items) => items.iter().filter_map(id_string).collect(),
        Value::String(text) => vec![text.clone()],
        _ => return None,
    };
    let out: Vec<String> = texts
        .iter()
        .flat_map(|text| text.split(','))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .take(MAX_TOKENS)
        .map(str::to_string)
        .collect();
    (!out.is_empty()).then_some(out)
}

/// Whether query parameter schema `param` is tied to the member names `declared`
/// (see the module's doc).
fn ties(bundle: &mb_mcm::Bundle, param: &Value, declared: &[&str]) -> bool {
    let (parts, _) = schema_parts(bundle, Some(param));
    let is = |t: &str| {
        parts
            .iter()
            .any(|p| p.get("type").and_then(Value::as_str) == Some(t))
    };
    let (array, string) = (is("array"), is("string"));
    if !array && !string {
        return false;
    }
    let names = |list: &[&str]| {
        list.len() >= MIN_EXAMPLE_TOKENS
            && list.iter().all(|token| {
                let head = token.split('.').next().unwrap_or(token);
                !head.is_empty() && declared.contains(&head)
            })
    };
    let items: Vec<&Value> = if array {
        parts
            .iter()
            .filter_map(|p| p.get("items"))
            .flat_map(|items| schema_parts(bundle, Some(items)).0)
            .collect()
    } else {
        Vec::new()
    };
    // An array of member names its items' enum lists.
    let enumerated = items.iter().any(|item| {
        item.get("enum").and_then(Value::as_array).is_some_and(|e| {
            let listed: Vec<&str> = e.iter().filter_map(Value::as_str).collect();
            listed.len() == e.len() && names(&listed)
        })
    });
    // A string with an enum is a choice, not a list.
    if string && !array && parts.iter().any(|p| p.get("enum").is_some()) {
        return false;
    }
    let mut examples: Vec<Vec<String>> = Vec::new();
    for part in &parts {
        let mut values: Vec<&Value> = part.get("example").into_iter().collect();
        values.extend(part.get("examples").and_then(Value::as_array).into_iter().flatten());
        for value in values {
            let listed: Vec<String> = match value {
                Value::String(text) => text.split(',').map(|t| t.trim().to_string()).collect(),
                Value::Array(items) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                _ => continue,
            };
            examples.push(listed);
        }
        if let Some(description) = part.get("description").and_then(Value::as_str) {
            for span in description.split('`').skip(1).step_by(2) {
                if span.contains(',') {
                    examples.push(span.split(',').map(|t| t.trim().to_string()).collect());
                }
            }
        }
    }
    enumerated
        || examples.iter().any(|listed| {
            let listed: Vec<&str> = listed.iter().map(String::as_str).collect();
            names(&listed)
        })
}

/// `members`, keeping what `paths` (each a token split at its dots) ask for at
/// `depth`.
fn cut_object(members: &Map<String, Value>, paths: &[Vec<&str>], depth: usize) -> Map<String, Value> {
    let mut out = Map::new();
    for (name, value) in members {
        let wanted: Vec<&Vec<&str>> = paths
            .iter()
            .filter(|path| path.get(depth) == Some(&name.as_str()))
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let whole = wanted.iter().any(|path| path.len() == depth + 1);
        let kept = match value {
            Value::Object(inner) if !whole && depth + 1 < MAX_DEPTH => {
                let deeper: Vec<Vec<&str>> = wanted.into_iter().cloned().collect();
                Value::Object(cut_object(inner, &deeper, depth + 1))
            }
            other => other.clone(),
        };
        out.insert(name.clone(), kept);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: t, version: "1"}
paths:
  /things/{id}:
    get:
      operationId: getThing
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
        - {name: fields, in: query, description: "Such as `id,name`.", schema: {type: string}}
        - {name: sort, in: query, description: "Such as `-price,name`.", schema: {type: string}}
        - {name: pick, in: query, schema: {type: array, items: {type: string, enum: [id, name, size]}}}
        - {name: shown, in: query, schema: {type: string, examples: ["name,size"]}}
        - {name: mode, in: query, schema: {type: string, enum: [id, name]}}
        - {name: q, in: query, description: "Text, as `lamp, desk`.", schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}
components:
  schemas:
    Thing:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
        price: {type: number}
        size: {type: object, properties: {width: {type: number}, height: {type: number}}}
"##;

    fn runtime() -> MockRuntime {
        MockRuntime::new(
            "m",
            Arc::new(mb_import::compile(CONTRACT.as_bytes()).unwrap()),
            Arc::new(mb_engine::Engine::new(
                None,
                mb_engine::EngineConfig::default(),
            )),
            Arc::new(mb_state::MemoryStore::new(std::time::Duration::from_secs(60))),
        )
    }

    fn thing() -> StoredResource {
        StoredResource {
            resource_type: "things".into(),
            id: "t-1".into(),
            data: json!({"id": "t-1", "name": "Lamp", "price": 9.5,
                         "size": {"width": 2, "height": 5}}),
        }
    }

    fn cut_with(query: &[(&str, Value)]) -> Option<Value> {
        let rt = runtime();
        let op = rt.operation("getThing").unwrap();
        let call = OperationCall {
            operation_id: op.id.clone(),
            query: query
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            ..OperationCall::default()
        };
        cut(&rt, op, &call, &thing()).map(|r| r.data)
    }

    #[test]
    fn a_tied_parameter_cuts_the_record_to_its_members() {
        assert_eq!(
            cut_with(&[("fields", json!("id,name"))]),
            Some(json!({"id": "t-1", "name": "Lamp"}))
        );
        assert_eq!(
            cut_with(&[("fields", json!("name,size.width"))]),
            Some(json!({"name": "Lamp", "size": {"width": 2}}))
        );
        // A dotted token through a member holding no object keeps it as it is.
        assert_eq!(
            cut_with(&[("fields", json!("price.cents"))]),
            Some(json!({"price": 9.5}))
        );
        assert_eq!(
            cut_with(&[("pick", json!(["name"]))]),
            Some(json!({"name": "Lamp"}))
        );
        assert_eq!(
            cut_with(&[("shown", json!("size"))]),
            Some(json!({"size": {"width": 2, "height": 5}}))
        );
    }

    #[test]
    fn what_ties_nothing_or_names_no_member_cuts_nothing() {
        assert_eq!(cut_with(&[("sort", json!("name"))]), None);
        assert_eq!(cut_with(&[("mode", json!("name"))]), None);
        assert_eq!(cut_with(&[("q", json!("name"))]), None);
        assert_eq!(cut_with(&[("fields", json!("*"))]), None);
        assert_eq!(cut_with(&[("fields", json!("id,colour"))]), None);
        assert_eq!(cut_with(&[("fields", json!("id,name,price,size"))]), None);
        // Two parameters that tie are ambiguous.
        assert_eq!(
            cut_with(&[("fields", json!("id")), ("shown", json!("name"))]),
            None
        );
    }
}
