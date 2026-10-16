//! HAR (1.1/1.2) importer: groups the recorded JSON traffic of the dominant
//! origin into operations with inferred path parameters, schemas and
//! redacted examples. Headers and cookies are never read.

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use url::Url;

use super::infer::{infer_scalar, infer_schema};
use super::model::{
    DraftBody, DraftOperation, DraftParam, DraftResponse, DraftWarning, EXCERPT_CHARS,
    MAX_PROVENANCE, ParamLocation, Provenance, RecordedImport,
};
use super::paths::{infer_templates, is_id_like, normalize_path};
use super::redact::{is_sensitive_name, redact};
use super::{MAX_EXAMPLES, SmartError};

/// Most bodies fed to schema inference per request body or response.
const MAX_SCHEMA_SAMPLES: usize = 20;
/// Confidence of an operation whose path has an inferred slug parameter.
const SLUG_CONFIDENCE: f32 = 0.8;
/// Path suffixes of static assets, which are skipped.
const ASSET_SUFFIXES: [&str; 13] = [
    ".js", ".css", ".map", ".png", ".jpg", ".jpeg", ".gif", ".svg", ".ico", ".woff", ".woff2",
    ".ttf", ".html",
];

/// One recorded exchange that survived parsing and filtering.
struct Entry {
    index: usize,
    method: String,
    url: Url,
    origin: String,
    path: String,
    query: Vec<(String, String)>,
    request_body: Option<(String, Value)>,
    status: u16,
    response: ResponseBody,
}

enum ResponseBody {
    /// 204 or an empty body.
    Empty,
    /// A JSON media type; the body when it parses.
    Json(String, Option<Value>),
    /// Anything else.
    Other(String),
}

/// Imports a HAR document.
pub fn import_har(bytes: &[u8]) -> Result<RecordedImport, SmartError> {
    let doc: Value =
        serde_json::from_slice(bytes).map_err(|e| SmartError::Unreadable(e.to_string()))?;
    let raw_entries = doc
        .pointer("/log/entries")
        .and_then(Value::as_array)
        .ok_or_else(|| SmartError::Unsupported("not a HAR file".to_string()))?;

    let mut skipped = 0usize;
    let mut parsed: Vec<Entry> = Vec::new();
    for (index, raw) in raw_entries.iter().enumerate() {
        match parse_entry(index, raw) {
            Some(entry) if is_api_entry(&entry) => parsed.push(entry),
            _ => skipped += 1,
        }
    }

    let mut warnings = Vec::new();
    if skipped > 0 {
        warnings.push(DraftWarning::new(
            "entries_skipped",
            format!(
                "skipped {skipped} {} (static assets, non-JSON traffic or unreadable entries)",
                plural(skipped)
            ),
        ));
    }

    let origin = dominant_origin(&parsed);
    let mut others: BTreeMap<String, usize> = BTreeMap::new();
    for entry in &parsed {
        if Some(&entry.origin) != origin.as_ref() {
            *others.entry(entry.origin.clone()).or_default() += 1;
        }
    }
    if !others.is_empty() {
        let list: Vec<String> = others
            .iter()
            .map(|(origin, count)| format!("{origin} ({count})"))
            .collect();
        warnings.push(DraftWarning::new(
            "other_origins",
            format!(
                "dropped entries on other origins (count): {}",
                list.join(", ")
            ),
        ));
    }
    let kept: Vec<Entry> = parsed
        .into_iter()
        .filter(|e| Some(&e.origin) == origin.as_ref())
        .collect();

    let paths: Vec<String> = kept
        .iter()
        .map(|e| e.path.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let templates = infer_templates(&paths);
    let mut groups: BTreeMap<(String, String), Vec<&Entry>> = BTreeMap::new();
    for entry in &kept {
        let template = templates
            .get(&entry.path)
            .map_or_else(|| entry.path.clone(), |t| t.template.clone());
        groups
            .entry((template, entry.method.clone()))
            .or_default()
            .push(entry);
    }
    let operations: Vec<DraftOperation> = groups
        .into_iter()
        .map(|((path, method), entries)| build_operation(method, path, &entries))
        .collect();

    let host = kept
        .first()
        .and_then(|e| e.url.host_str().map(str::to_string));
    Ok(RecordedImport {
        title: host.map_or_else(|| "HAR import".to_string(), |h| format!("{h} (HAR import)")),
        servers: origin.into_iter().collect(),
        operations,
        warnings,
    })
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "entry" } else { "entries" }
}

fn parse_entry(index: usize, raw: &Value) -> Option<Entry> {
    let request = raw.get("request")?;
    let response = raw.get("response")?;
    let method = request.get("method")?.as_str()?.trim().to_ascii_uppercase();
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let url = Url::parse(request.get("url")?.as_str()?).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let status = u16::try_from(response.get("status")?.as_u64()?).ok()?;
    if !(100..=599).contains(&status) {
        return None;
    }
    let query = request
        .get("queryString")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|q| {
                    Some((
                        q.get("name")?.as_str()?.to_string(),
                        q.get("value")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Entry {
        index,
        origin: url.origin().ascii_serialization(),
        path: normalize_path(url.path()),
        method,
        query,
        request_body: request.get("postData").and_then(request_body),
        status,
        response: response_body(status, response.get("content")),
        url,
    })
}

/// A JSON request body: a JSON (or absent) media type and text that parses.
fn request_body(post: &Value) -> Option<(String, Value)> {
    let mime = post.get("mimeType").and_then(Value::as_str).map(base_mime);
    if mime
        .as_deref()
        .is_some_and(|m| !m.is_empty() && !is_json_mime(m))
    {
        return None;
    }
    let mut body: Value = serde_json::from_str(post.get("text")?.as_str()?).ok()?;
    redact(&mut body);
    let mime = mime
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "application/json".to_string());
    Some((mime, body))
}

fn response_body(status: u16, content: Option<&Value>) -> ResponseBody {
    let text = content
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if status == 204 || text.is_empty() {
        return ResponseBody::Empty;
    }
    let mime = content
        .and_then(|c| c.get("mimeType"))
        .and_then(Value::as_str)
        .map(base_mime)
        .unwrap_or_default();
    if !is_json_mime(&mime) {
        return ResponseBody::Other(mime);
    }
    let base64 = content
        .and_then(|c| c.get("encoding"))
        .and_then(Value::as_str)
        == Some("base64");
    let body = if base64 {
        STANDARD
            .decode(text.trim())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    } else {
        serde_json::from_str::<Value>(text).ok()
    };
    ResponseBody::Json(
        mime,
        body.map(|mut b| {
            redact(&mut b);
            b
        }),
    )
}

fn base_mime(raw: &str) -> String {
    raw.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn is_json_mime(mime: &str) -> bool {
    mime == "application/json" || mime.ends_with("+json")
}

/// Not a static asset, and JSON (or empty) traffic in one direction.
fn is_api_entry(entry: &Entry) -> bool {
    let path = entry.path.to_ascii_lowercase();
    if ASSET_SUFFIXES.iter().any(|s| path.ends_with(s)) {
        return false;
    }
    !matches!(entry.response, ResponseBody::Other(_)) || entry.request_body.is_some()
}

/// The origin with the most entries; ties go to the one seen first.
fn dominant_origin(entries: &[Entry]) -> Option<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for entry in entries {
        match counts.iter_mut().find(|(o, _)| *o == entry.origin) {
            Some((_, n)) => *n += 1,
            None => counts.push((entry.origin.clone(), 1)),
        }
    }
    let mut best: Option<(String, usize)> = None;
    for (origin, n) in counts {
        if best.as_ref().is_none_or(|(_, b)| n > *b) {
            best = Some((origin, n));
        }
    }
    best.map(|(origin, _)| origin)
}

fn build_operation(method: String, path: String, entries: &[&Entry]) -> DraftOperation {
    let (mut parameters, slug) = path_parameters(&path, entries);
    parameters.extend(query_parameters(entries));
    let target_url = |e: &Entry| {
        let mut url = e.url.clone();
        url.set_query(None);
        url.set_fragment(None);
        url
    };
    let provenance = entries
        .iter()
        .take(MAX_PROVENANCE)
        .map(|e| Provenance {
            chunk: None,
            location: format!("entry {}", e.index),
            excerpt: format!("{} {} → {}", e.method, target_url(e), e.status)
                .chars()
                .take(EXCERPT_CHARS)
                .collect(),
        })
        .collect();
    DraftOperation {
        method,
        path,
        operation_id: None,
        summary: None,
        description: None,
        tags: Vec::new(),
        parameters,
        request: request(entries),
        responses: responses(entries),
        confidence: if slug { SLUG_CONFIDENCE } else { 1.0 },
        provenance,
    }
}

/// Path parameters in path order, and whether any of them is a slug (a
/// parameter with a concrete value that is not id-like).
fn path_parameters(template: &str, entries: &[&Entry]) -> (Vec<DraftParam>, bool) {
    let template_segments: Vec<&str> = template.split('/').filter(|s| !s.is_empty()).collect();
    let concrete: Vec<Vec<&str>> = entries
        .iter()
        .map(|e| e.path.split('/').filter(|s| !s.is_empty()).collect())
        .collect();
    let mut slug = false;
    let mut params = Vec::new();
    for (pos, segment) in template_segments.iter().enumerate() {
        let Some(name) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
            continue;
        };
        let values: Vec<&str> = concrete
            .iter()
            .filter_map(|segs| segs.get(pos).copied())
            .collect();
        slug |= values.iter().any(|v| !is_id_like(v));
        let schema = infer_scalar(&values);
        let examples = typed_examples(&values, &schema);
        params.push(DraftParam {
            name: name.to_string(),
            location: ParamLocation::Path,
            required: true,
            description: None,
            schema: Some(schema),
            examples,
        });
    }
    (params, slug)
}

/// Query parameters sorted by name; required iff present in every entry.
fn query_parameters(entries: &[&Entry]) -> Vec<DraftParam> {
    let mut seen: BTreeMap<&str, (usize, Vec<&str>)> = BTreeMap::new();
    for entry in entries {
        let mut names_here = BTreeSet::new();
        for (name, value) in &entry.query {
            let slot = seen.entry(name.as_str()).or_default();
            if names_here.insert(name.as_str()) {
                slot.0 += 1;
            }
            slot.1.push(value.as_str());
        }
    }
    seen.into_iter()
        .map(|(name, (present, values))| {
            let mut schema = infer_scalar(&values);
            let sensitive = is_sensitive_name(name);
            let examples = if sensitive {
                // An enum would list the observed (secret) values.
                if let Some(map) = schema.as_object_mut() {
                    map.remove("enum");
                }
                Vec::new()
            } else {
                typed_examples(&values, &schema)
            };
            DraftParam {
                name: name.to_string(),
                location: ParamLocation::Query,
                required: present == entries.len(),
                description: None,
                schema: Some(schema),
                examples,
            }
        })
        .collect()
}

/// Distinct observed values, typed after the inferred scalar schema.
fn typed_examples(values: &[&str], schema: &Value) -> Vec<Value> {
    let kind = schema.get("type").and_then(Value::as_str);
    let mut out: Vec<Value> = Vec::new();
    for value in values {
        let typed = match kind {
            Some("integer") => value.parse::<i64>().ok().map(Value::from),
            Some("number") => value.parse::<f64>().ok().map(Value::from),
            Some("boolean") => value.parse::<bool>().ok().map(Value::from),
            _ => None,
        }
        .unwrap_or_else(|| Value::from(*value));
        if out.len() < MAX_EXAMPLES && !out.contains(&typed) {
            out.push(typed);
        }
    }
    out
}

fn request(entries: &[&Entry]) -> Option<DraftBody> {
    let bodies: Vec<&(String, Value)> = entries
        .iter()
        .filter_map(|e| e.request_body.as_ref())
        .collect();
    let content_type = bodies.first()?.0.clone();
    let values: Vec<&Value> = bodies.iter().map(|(_, v)| v).collect();
    Some(DraftBody {
        content_type,
        schema: Some(schema_of(&values)),
        examples: distinct_examples(&values),
    })
}

/// One response per observed status, sorted by status.
fn responses(entries: &[&Entry]) -> Vec<DraftResponse> {
    let mut by_status: BTreeMap<u16, Vec<&Entry>> = BTreeMap::new();
    for entry in entries {
        by_status.entry(entry.status).or_default().push(entry);
    }
    by_status
        .into_iter()
        .map(|(status, entries)| {
            let mut content_type = None;
            let mut bodies: Vec<&Value> = Vec::new();
            for entry in &entries {
                let (mime, body) = match &entry.response {
                    ResponseBody::Empty => continue,
                    ResponseBody::Json(mime, body) => (mime, body.as_ref()),
                    ResponseBody::Other(mime) => (mime, None),
                };
                if content_type.is_none() && !mime.is_empty() {
                    content_type = Some(mime.clone());
                }
                bodies.extend(body);
            }
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

fn schema_of(values: &[&Value]) -> Value {
    let samples: Vec<Value> = values
        .iter()
        .take(MAX_SCHEMA_SAMPLES)
        .map(|v| (*v).clone())
        .collect();
    infer_schema(&samples)
}

fn distinct_examples(values: &[&Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for value in values {
        if out.len() >= MAX_EXAMPLES {
            break;
        }
        if !out.contains(value) {
            out.push((*value).clone());
        }
    }
    out
}
