//! Postman collection (v2.0/v2.1) importer: folders become tags, requests
//! become operations with inferred path parameters, and saved examples
//! become responses. `auth` blocks and headers other than an example's
//! `Content-Type` are never read.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use super::SmartError;
use super::infer::{base_mime, distinct_examples, infer_scalar, schema_of, typed_examples};
use super::model::{
    DraftBody, DraftOperation, DraftParam, DraftResponse, DraftWarning, EXCERPT_CHARS,
    MAX_PROVENANCE, ParamLocation, Provenance, RecordedImport,
};
use super::paths::{
    Shape, infer_templates_with, is_id_like, normalize_path, path_key, slug_warning,
};
use super::redact::{REDACTED, is_sensitive_name, redact};

/// Confidence of an operation whose path has an inferred slug parameter.
const SLUG_CONFIDENCE: f32 = 0.8;
const UNSUPPORTED_VERSION: &str = "only Postman collection v2.0 and v2.1 are supported";
const JSON: &str = "application/json";

/// A request body as read from the collection.
enum Body {
    /// A raw body that parses as JSON, redacted.
    Json(Value),
    /// Form keys (in order) and an example with sensitive values redacted.
    Form {
        content_type: &'static str,
        keys: Vec<String>,
        example: Map<String, Value>,
    },
}

impl Body {
    fn content_type(&self) -> &'static str {
        match self {
            Body::Json(_) => JSON,
            Body::Form { content_type, .. } => content_type,
        }
    }
}

/// One saved example (`response[]` entry).
struct Example {
    status: u16,
    content_type: Option<String>,
    /// The body when it parses as JSON, redacted.
    body: Option<Value>,
    /// Normalized path of its `originalRequest`.
    path: Option<String>,
}

/// One request item.
struct Item {
    method: String,
    /// Normalized path, possibly with `{name}` parameters.
    path: String,
    /// Enabled query parameters; values may be absent.
    query: Vec<(String, Option<String>)>,
    /// `url.variable` values by name.
    variables: BTreeMap<String, String>,
    body: Option<Body>,
    examples: Vec<Example>,
    /// The top-level folder.
    tag: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    location: String,
    excerpt: String,
}

/// Imports a Postman collection (v2.0 or v2.1).
///
/// # Errors
///
/// [`SmartError::Unreadable`] when the bytes are not JSON, and
/// [`SmartError::Unsupported`] when they are not a v2.0 or v2.1 collection.
pub fn import_postman(bytes: &[u8]) -> Result<RecordedImport, SmartError> {
    let doc: Value =
        serde_json::from_slice(bytes).map_err(|e| SmartError::Unreadable(e.to_string()))?;
    let schema = doc
        .pointer("/info/schema")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !(schema.contains("v2.0.0") || schema.contains("v2.1.0")) {
        return Err(SmartError::Unsupported(UNSUPPORTED_VERSION.to_string()));
    }
    let title = doc
        .pointer("/info/name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Postman import")
        .to_string();

    let mut items = Vec::new();
    let mut skipped = 0usize;
    if let Some(children) = doc.get("item").and_then(Value::as_array) {
        walk(children, &mut Vec::new(), &mut items, &mut skipped);
    }

    let mut warnings = Vec::new();
    if skipped > 0 {
        warnings.push(DraftWarning::new(
            "entries_skipped",
            format!(
                "skipped {skipped} {} without a usable method or URL",
                if skipped == 1 { "request" } else { "requests" }
            ),
        ));
    }

    // Template and concrete paths are inferred together, so a concrete
    // `/pets/42` joins `/pets/{petId}`.
    let mut all_paths: BTreeSet<String> = BTreeSet::new();
    for item in &items {
        all_paths.insert(item.path.clone());
        all_paths.extend(item.examples.iter().filter_map(|e| e.path.clone()));
    }
    let all_paths: Vec<String> = all_paths.into_iter().collect();
    // The first JSON success body of each path, from the examples.
    let mut shapes: BTreeMap<String, Shape> = BTreeMap::new();
    for item in &items {
        for example in &item.examples {
            if let (200..=299, Some(body)) = (example.status, &example.body) {
                let path = example.path.as_ref().unwrap_or(&item.path);
                shapes
                    .entry(path.clone())
                    .or_insert_with(|| Shape::of(body));
            }
        }
    }
    let templates = infer_templates_with(&all_paths, &shapes);
    let template_of = |path: &str| {
        templates
            .get(path)
            .map_or_else(|| path.to_string(), |t| t.template.clone())
    };

    // Group by merge key, in collection order; the first template wins.
    let mut keys: Vec<(String, String)> = Vec::new();
    let mut groups: Vec<(String, Vec<&Item>)> = Vec::new();
    for item in &items {
        let template = template_of(&item.path);
        let key = path_key(&item.method, &template);
        match keys.iter().position(|k| *k == key) {
            Some(i) => groups[i].1.push(item),
            None => {
                keys.push(key);
                groups.push((template, vec![item]));
            }
        }
    }
    let mut operations = Vec::new();
    for (template, items) in groups {
        let (operation, slug) = build_operation(&template, &items, &template_of);
        warnings.extend(slug);
        operations.push(operation);
    }

    Ok(RecordedImport {
        title,
        servers: Vec::new(),
        operations,
        warnings,
    })
}

/// Walks `item` arrays depth first; `folders` is the current folder path.
fn walk(children: &[Value], folders: &mut Vec<String>, out: &mut Vec<Item>, skipped: &mut usize) {
    for child in children {
        let name = child
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(grandchildren) = child.get("item").and_then(Value::as_array) {
            folders.push(name.unwrap_or("").to_string());
            walk(grandchildren, folders, out, skipped);
            folders.pop();
        } else if let Some(request) = child.get("request") {
            match parse_item(child, request, name, folders) {
                Some(item) => out.push(item),
                None => *skipped += 1,
            }
        }
    }
}

fn parse_item(
    item: &Value,
    request: &Value,
    name: Option<&str>,
    folders: &[String],
) -> Option<Item> {
    // A request may be just its URL, which implies GET.
    let (method, url) = match request {
        Value::String(_) => ("GET".to_string(), request),
        _ => {
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .trim()
                .to_ascii_uppercase();
            (method, request.get("url")?)
        }
    };
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let url = parse_url(url)?;
    let mut location: Vec<&str> = folders.iter().map(String::as_str).collect();
    location.push(name.unwrap_or(""));
    let examples = item
        .get("response")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(parse_example).collect())
        .unwrap_or_default();
    Some(Item {
        excerpt: format!("{method} {}", sanitize_url(&url.raw))
            .chars()
            .take(EXCERPT_CHARS)
            .collect(),
        method,
        path: url.path,
        query: url.query,
        variables: url.variables,
        body: request.get("body").and_then(parse_body),
        examples,
        tag: folders.first().filter(|f| !f.is_empty()).cloned(),
        summary: name.map(str::to_string),
        description: request.get("description").and_then(description),
        location: location.join(" / "),
    })
}

/// A description string, or the `content` of a description object.
fn description(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(s) => s.as_str(),
        _ => value.get("content")?.as_str()?,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

struct ParsedUrl {
    /// The raw URL (or the path when the object has no `raw`).
    raw: String,
    path: String,
    query: Vec<(String, Option<String>)>,
    variables: BTreeMap<String, String>,
}

/// A v2.0 string URL or a v2.1 URL object.
fn parse_url(url: &Value) -> Option<ParsedUrl> {
    if let Some(raw) = url.as_str() {
        return Some(ParsedUrl {
            raw: raw.to_string(),
            path: normalize_path(raw_path(raw)),
            query: raw_query(raw),
            variables: BTreeMap::new(),
        });
    }
    let object = url.as_object()?;
    let raw = object.get("raw").and_then(Value::as_str).unwrap_or("");
    let path = match object.get("path") {
        Some(Value::Array(segments)) => segments
            .iter()
            .filter_map(|s| match s {
                Value::String(s) => Some(s.as_str()),
                _ => s.get("value").and_then(Value::as_str),
            })
            .collect::<Vec<_>>()
            .join("/"),
        Some(Value::String(s)) => s.clone(),
        _ if !raw.trim().is_empty() => raw_path(raw).to_string(),
        _ => return None,
    };
    let path = normalize_path(&path);
    let query = match object.get("query").and_then(Value::as_array) {
        Some(entries) => entries
            .iter()
            .filter(|q| !is_disabled(q))
            .filter_map(|q| {
                let key = q.get("key")?.as_str()?;
                let value = q.get("value").and_then(Value::as_str);
                Some((key.to_string(), value.map(str::to_string)))
            })
            .collect(),
        None => raw_query(raw),
    };
    let variables = object
        .get("variable")
        .and_then(Value::as_array)
        .map(|vars| {
            vars.iter()
                .filter_map(|v| {
                    let key = v.get("key").or_else(|| v.get("id"))?.as_str()?;
                    let value = match v.get("value")? {
                        Value::String(s) => s.clone(),
                        Value::Number(n) => n.to_string(),
                        Value::Bool(b) => b.to_string(),
                        _ => return None,
                    };
                    Some((key.to_string(), value))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ParsedUrl {
        raw: if raw.trim().is_empty() {
            path.clone()
        } else {
            raw.to_string()
        },
        path,
        query,
        variables,
    })
}

fn is_disabled(entry: &Value) -> bool {
    entry.get("disabled").and_then(Value::as_bool) == Some(true)
}

/// The path part of a raw URL: without the query and fragment, and without
/// a leading `{{…}}` host or scheme and host.
fn raw_path(raw: &str) -> &str {
    let mut rest = raw.trim();
    if let Some(i) = rest.find(['?', '#']) {
        rest = &rest[..i];
    }
    if let Some(after) = rest.strip_prefix("{{") {
        rest = after.find("}}").map_or("", |i| &after[i + 2..]);
    } else if let Some((scheme, after)) = rest.split_once("://")
        && !scheme.contains('/')
    {
        rest = after.find('/').map_or("", |i| &after[i..]);
    }
    rest
}

/// Query pairs of a raw URL (values kept as written).
fn raw_query(raw: &str) -> Vec<(String, Option<String>)> {
    let Some((_, query)) = raw.split_once('?') else {
        return Vec::new();
    };
    query
        .split('#')
        .next()
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (pair.to_string(), None),
        })
        .collect()
}

/// The raw URL for provenance, with userinfo dropped and the values of
/// sensitive query parameters redacted.
fn sanitize_url(raw: &str) -> String {
    let raw = raw.trim();
    let (base, fragment) = match raw.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (raw, None),
    };
    let (base, query) = match base.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (base, None),
    };
    let mut out = match base.split_once("://") {
        Some((scheme, after)) if !scheme.contains('/') => {
            let (authority, path) = after.split_at(after.find('/').unwrap_or(after.len()));
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            format!("{scheme}://{host}{path}")
        }
        _ => base.to_string(),
    };
    if let Some(query) = query {
        let pairs: Vec<String> = query
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((k, _)) if is_sensitive_name(k) => format!("{k}={REDACTED}"),
                _ => pair.to_string(),
            })
            .collect();
        out.push('?');
        out.push_str(&pairs.join("&"));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

fn parse_body(body: &Value) -> Option<Body> {
    match body.get("mode")?.as_str()? {
        "raw" => {
            let mut value: Value = serde_json::from_str(body.get("raw")?.as_str()?).ok()?;
            redact(&mut value);
            Some(Body::Json(value))
        }
        mode @ ("urlencoded" | "formdata") => {
            let content_type = if mode == "urlencoded" {
                "application/x-www-form-urlencoded"
            } else {
                "multipart/form-data"
            };
            let mut keys = Vec::new();
            let mut example = Map::new();
            for entry in body.get(mode)?.as_array()? {
                if is_disabled(entry) {
                    continue;
                }
                let Some(key) = entry.get("key").and_then(Value::as_str) else {
                    continue;
                };
                if !keys.iter().any(|k| k == key) {
                    keys.push(key.to_string());
                }
                let is_file = entry.get("type").and_then(Value::as_str) == Some("file");
                if is_sensitive_name(key) {
                    example.insert(key.to_string(), Value::String(REDACTED.to_string()));
                } else if !is_file && let Some(value) = entry.get("value").and_then(Value::as_str) {
                    example.insert(key.to_string(), Value::String(value.to_string()));
                }
            }
            Some(Body::Form {
                content_type,
                keys,
                example,
            })
        }
        _ => None,
    }
}

fn parse_example(example: &Value) -> Option<Example> {
    let status = u16::try_from(example.get("code")?.as_u64()?).ok()?;
    if !(100..=599).contains(&status) {
        return None;
    }
    let declared = example
        .get("header")
        .and_then(Value::as_array)
        .and_then(|headers| {
            headers.iter().find_map(|h| {
                let key = h.get("key")?.as_str()?;
                if !key.eq_ignore_ascii_case("content-type") {
                    return None;
                }
                h.get("value")?.as_str().map(base_mime)
            })
        })
        .filter(|m| !m.is_empty());
    let body = example
        .get("body")
        .and_then(Value::as_str)
        .filter(|b| !b.trim().is_empty())
        .and_then(|b| serde_json::from_str::<Value>(b).ok())
        .map(|mut b| {
            redact(&mut b);
            b
        });
    let content_type = declared.or_else(|| body.is_some().then(|| JSON.to_string()));
    let path = example
        .get("originalRequest")
        .and_then(|r| match r {
            Value::String(_) => Some(r),
            _ => r.get("url"),
        })
        .and_then(parse_url)
        .map(|u| u.path);
    Some(Example {
        status,
        content_type,
        body,
        path,
    })
}

/// One operation from the items sharing a merge key: union of examples,
/// parameters and tags; the first summary and description win. With a
/// warning when its path has slug parameters.
fn build_operation(
    template: &str,
    items: &[&Item],
    template_of: &dyn Fn(&str) -> String,
) -> (DraftOperation, Option<DraftWarning>) {
    let mut tags: Vec<String> = Vec::new();
    for tag in items.iter().filter_map(|i| i.tag.as_ref()) {
        if !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
    let (mut parameters, slugs) = path_parameters(template, items, template_of);
    parameters.extend(query_parameters(items));
    let slug = !slugs.is_empty();
    let method = items.first().map(|i| i.method.clone()).unwrap_or_default();
    let warning = slug.then(|| slug_warning(&format!("{method} {template}"), &slugs));
    let operation = DraftOperation {
        method,
        path: template.to_string(),
        operation_id: None,
        summary: items.iter().find_map(|i| i.summary.clone()),
        description: items.iter().find_map(|i| i.description.clone()),
        tags,
        parameters,
        request: request(items),
        responses: responses(items),
        confidence: if slug { SLUG_CONFIDENCE } else { 1.0 },
        provenance: items
            .iter()
            .take(MAX_PROVENANCE)
            .map(|i| Provenance {
                chunk: None,
                location: i.location.clone(),
                excerpt: i.excerpt.clone(),
            })
            .collect(),
    };
    (operation, warning)
}

fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

fn braced_name(segment: &str) -> Option<&str> {
    segment.strip_prefix('{').and_then(|s| s.strip_suffix('}'))
}

/// Path parameters in path order, and the ones inferred from a slug (a
/// literal, non-id segment of a request path) with their values.
///
/// Values come from `url.variable`, from literal request paths, and from
/// the `originalRequest` paths of examples that map onto the same template.
fn path_parameters(
    template: &str,
    items: &[&Item],
    template_of: &dyn Fn(&str) -> String,
) -> (Vec<DraftParam>, Vec<(String, Vec<String>)>) {
    let key = path_key("GET", template);
    // (concrete path, its item when it is the request path)
    let mut concrete: Vec<(&str, Option<&Item>)> = Vec::new();
    for item in items {
        concrete.push((&item.path, Some(item)));
        for example in &item.examples {
            if let Some(path) = &example.path
                && path_key("GET", &template_of(path)) == key
            {
                concrete.push((path, None));
            }
        }
    }
    let mut slugs = Vec::new();
    let mut params = Vec::new();
    for (pos, segment) in segments(template).iter().enumerate() {
        let Some(name) = braced_name(segment) else {
            continue;
        };
        let mut values: Vec<&str> = Vec::new();
        let mut slug_values: Vec<String> = Vec::new();
        for (path, request) in &concrete {
            let Some(value) = segments(path).get(pos).copied() else {
                continue;
            };
            if let Some(declared) = braced_name(value) {
                if let Some(item) = request {
                    values.extend(item.variables.get(declared).map(String::as_str));
                }
                continue;
            }
            if request.is_some() && !is_id_like(value) {
                slug_values.push(value.to_string());
            }
            values.push(value);
        }
        values.retain(|v| usable_value(v));
        params.push(scalar_param(name, ParamLocation::Path, true, &values));
        if !slug_values.is_empty() {
            slugs.push((name.to_string(), slug_values));
        }
    }
    (params, slugs)
}

/// Not empty and not a `{{variable}}` reference.
fn usable_value(value: &str) -> bool {
    !value.is_empty() && !value.contains("{{")
}

/// Query parameters in first-seen order. Never required: a collection does
/// not say which parameters the server needs.
fn query_parameters(items: &[&Item]) -> Vec<DraftParam> {
    let mut seen: Vec<(&str, Vec<&str>)> = Vec::new();
    for item in items {
        for (name, value) in &item.query {
            let index = match seen.iter().position(|(n, _)| n == name) {
                Some(i) => i,
                None => {
                    seen.push((name, Vec::new()));
                    seen.len() - 1
                }
            };
            if let Some(value) = value.as_deref().filter(|v| usable_value(v)) {
                seen[index].1.push(value);
            }
        }
    }
    seen.into_iter()
        .map(|(name, values)| scalar_param(name, ParamLocation::Query, false, &values))
        .collect()
}

/// A parameter with a schema inferred from its observed values (a string
/// without any); a sensitive name keeps no examples and no enum.
fn scalar_param(
    name: &str,
    location: ParamLocation,
    required: bool,
    values: &[&str],
) -> DraftParam {
    let (schema, examples) = if values.is_empty() {
        (json!({"type": "string"}), Vec::new())
    } else if is_sensitive_name(name) {
        let mut schema = infer_scalar(values);
        if let Some(map) = schema.as_object_mut() {
            map.remove("enum");
        }
        (schema, Vec::new())
    } else {
        let schema = infer_scalar(values);
        let examples = typed_examples(values, &schema);
        (schema, examples)
    };
    DraftParam {
        name: name.to_string(),
        location,
        required,
        description: None,
        schema: Some(schema),
        examples,
    }
}

/// The request body: the bodies sharing the first body's content type.
fn request(items: &[&Item]) -> Option<DraftBody> {
    let content_type = items.iter().find_map(|i| i.body.as_ref())?.content_type();
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    let mut examples: Vec<Value> = Vec::new();
    for body in items.iter().filter_map(|i| i.body.as_ref()) {
        if body.content_type() != content_type {
            continue;
        }
        match body {
            Body::Json(value) => examples.push(value.clone()),
            Body::Form {
                keys: form_keys,
                example,
                ..
            } => {
                keys.extend(form_keys.iter().map(String::as_str));
                if !example.is_empty() {
                    examples.push(Value::Object(example.clone()));
                }
            }
        }
    }
    let examples: Vec<&Value> = examples.iter().collect();
    let schema = if content_type == JSON {
        schema_of(&examples)
    } else {
        let properties: Map<String, Value> = keys
            .into_iter()
            .map(|k| (k.to_string(), json!({"type": "string"})))
            .collect();
        json!({"type": "object", "properties": properties})
    };
    Some(DraftBody {
        content_type: content_type.to_string(),
        schema: Some(schema),
        examples: distinct_examples(&examples),
    })
}

/// One response per example status, sorted by status.
fn responses(items: &[&Item]) -> Vec<DraftResponse> {
    let mut by_status: BTreeMap<u16, Vec<&Example>> = BTreeMap::new();
    for example in items.iter().flat_map(|i| &i.examples) {
        by_status.entry(example.status).or_default().push(example);
    }
    by_status
        .into_iter()
        .map(|(status, examples)| {
            let content_type = examples.iter().find_map(|e| e.content_type.clone());
            let bodies: Vec<&Value> = examples.iter().filter_map(|e| e.body.as_ref()).collect();
            DraftResponse {
                status,
                description: None,
                content_type,
                schema: (!bodies.is_empty()).then(|| schema_of(&bodies)),
                examples: distinct_examples(&bodies),
            }
        })
        .collect()
}
