//! OpenAPI 3.0 / 3.1 -> MCM compiler.

use crate::ImportError;
use crate::heuristics::{infer_identity, infer_kind, infer_resource_type};
use crate::schema::{collect_refs, defs_name, normalize_schema};
use crate::security::compile_security;
use mb_mcm::{
    Binding, Bundle, HttpMethod, InputSchemas, Operation, OperationKind, OutputVariant, Protocol,
    StatusSelector, WebhookDef,
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

const REF_PREFIX: &str = "#/components/schemas/";

pub(crate) const METHODS: [(&str, HttpMethod); 7] = [
    ("get", HttpMethod::Get),
    ("put", HttpMethod::Put),
    ("post", HttpMethod::Post),
    ("delete", HttpMethod::Delete),
    ("patch", HttpMethod::Patch),
    ("head", HttpMethod::Head),
    ("options", HttpMethod::Options),
];

pub(crate) fn invalid(pointer: impl Into<String>, message: impl Into<String>) -> ImportError {
    ImportError::Invalid {
        pointer: pointer.into(),
        message: message.into(),
    }
}

pub(crate) fn escape(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

/// Follow a component `$ref` (`#/components/...`) until a concrete object.
pub(crate) fn deref<'a>(
    doc: &'a Value,
    mut v: &'a Value,
    at: &str,
) -> Result<&'a Value, ImportError> {
    for _ in 0..16 {
        match v.get("$ref").and_then(Value::as_str) {
            Some(r) => {
                v = r
                    .strip_prefix('#')
                    .and_then(|p| doc.pointer(p))
                    .ok_or_else(|| invalid(r, format!("unresolvable $ref referenced from {at}")))?;
            }
            None => return Ok(v),
        }
    }
    Err(invalid(at, "$ref chain too deep"))
}

pub(crate) fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

/// Assigns operation ids: explicit `operationId`s must be unique across the
/// document; generated `{method}_{path}` slugs get a numeric suffix when they
/// collide with an explicit or an earlier id.
pub(crate) struct OperationIds {
    explicit: HashSet<String>,
    used: HashSet<String>,
}

impl OperationIds {
    /// Collects the explicit ids, rejecting a duplicate with a pointer to
    /// its second occurrence.
    pub(crate) fn new(doc: &Value) -> Result<Self, ImportError> {
        let mut explicit = HashSet::new();
        if let Some(paths) = doc.get("paths").and_then(Value::as_object) {
            for (path, item) in paths {
                let item = deref(doc, item, &format!("/paths/{}", escape(path)))?;
                for (key, _) in METHODS {
                    let Some(id) = item
                        .get(key)
                        .and_then(|op| op.get("operationId"))
                        .and_then(Value::as_str)
                    else {
                        continue;
                    };
                    if !explicit.insert(id.to_owned()) {
                        return Err(invalid(
                            format!("/paths/{}/{key}/operationId", escape(path)),
                            format!("duplicate operationId {id:?}"),
                        ));
                    }
                }
            }
        }
        Ok(Self {
            explicit,
            used: HashSet::new(),
        })
    }

    pub(crate) fn assign(&mut self, op: &Value, method_key: &str, path: &str) -> String {
        if let Some(id) = op.get("operationId").and_then(Value::as_str) {
            self.used.insert(id.to_owned());
            return id.to_owned();
        }
        let base = format!("{method_key}_{}", slug(path).replace('-', "_"));
        let mut id = base.clone();
        let mut n = 2;
        while self.explicit.contains(&id) || self.used.contains(&id) {
            id = format!("{base}_{n}");
            n += 1;
        }
        self.used.insert(id.clone());
        id
    }
}

pub(crate) fn is_json_media(ct: &str) -> bool {
    let base = ct
        .split(';')
        .next()
        .unwrap_or(ct)
        .trim()
        .to_ascii_lowercase();
    base == "application/json" || base.ends_with("+json")
}

/// The form media types, whose body schemas are tagged with
/// [`CONTENT_TYPE_EXT`] so the data plane decodes them as forms.
pub(crate) const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
pub(crate) const MULTIPART_CONTENT_TYPE: &str = "multipart/form-data";
/// Extension naming the media type of a non-JSON request body schema.
pub(crate) const CONTENT_TYPE_EXT: &str = "x-content-type";

/// The server-sent events media type.
pub(crate) const EVENT_STREAM: &str = "text/event-stream";
/// Extension giving a stream's item schema explicitly.
const ITEM_SCHEMA_EXT: &str = "x-mockingbird-item-schema";
/// Extension naming the SSE `event:` of a stream's items.
pub(crate) const SSE_EVENT_EXT: &str = "x-mockingbird-sse-event";

/// Whether `ct` is `text/event-stream` (parameters allowed).
pub(crate) fn is_event_stream(ct: &str) -> bool {
    ct.split(';')
        .next()
        .unwrap_or(ct)
        .trim()
        .eq_ignore_ascii_case(EVENT_STREAM)
}

/// The item schema of an event stream: `holder`'s
/// `x-mockingbird-item-schema` if present, else `schema`'s `items` when it
/// is an array, else `schema` (`{}` when there is none). `holder`'s
/// `x-mockingbird-sse-event` is copied into it. `norm` normalizes a raw
/// source schema.
pub(crate) fn stream_item_schema(
    holder: &Value,
    schema: Option<&Value>,
    norm: impl Fn(&Value) -> Value,
) -> Value {
    let mut item = match holder.get(ITEM_SCHEMA_EXT) {
        Some(explicit) => norm(explicit),
        None => {
            let schema = schema.map(&norm).unwrap_or_else(|| json!({}));
            let is_array = match schema.get("type") {
                Some(Value::String(t)) => t == "array",
                Some(Value::Array(ts)) => ts.iter().any(|t| t == "array"),
                _ => false,
            };
            match schema.get("items") {
                Some(items) if is_array && items.is_object() => items.clone(),
                _ => schema,
            }
        }
    };
    if let (Some(name), Value::Object(obj)) =
        (holder.get(SSE_EVENT_EXT).and_then(Value::as_str), &mut item)
    {
        obj.insert(SSE_EVENT_EXT.into(), json!(name));
    }
    item
}

/// An operation whose success response is an event stream streams its
/// items to the client.
pub(crate) fn stream_kind(op: &Operation) -> Option<OperationKind> {
    op.success_output()
        .and_then(|o| o.content_type.as_deref())
        .filter(|ct| is_event_stream(ct))
        .map(|_| OperationKind::StreamServer)
}

/// Pick the JSON media entry (preferring `application/json`), if any.
fn json_media(content: &Map<String, Value>) -> Option<(&String, &Value)> {
    content
        .get_key_value("application/json")
        .or_else(|| content.iter().find(|(k, _)| is_json_media(k)))
}

fn schema_of(media: &Value) -> Value {
    media
        .get("schema")
        .map(|s| normalize_schema(s, REF_PREFIX, true))
        .unwrap_or_else(|| json!({}))
}

fn merged_parameters<'a>(
    doc: &'a Value,
    path_item: &'a Value,
    op: &'a Value,
    at: &str,
) -> Result<Vec<&'a Value>, ImportError> {
    let mut merged: Vec<(&str, &str, &Value)> = Vec::new();
    for (list, label) in [(path_item, "path item"), (op, "operation")] {
        let Some(params) = list.get("parameters").and_then(Value::as_array) else {
            continue;
        };
        for p in params {
            let p = deref(doc, p, &format!("{at}/parameters ({label})"))?;
            let name = p.get("name").and_then(Value::as_str).unwrap_or_default();
            let loc = p.get("in").and_then(Value::as_str).unwrap_or_default();
            // Operation-level wins over path-level on the same name+in.
            match merged.iter_mut().find(|(n, l, _)| *n == name && *l == loc) {
                Some(slot) => slot.2 = p,
                None => merged.push((name, loc, p)),
            }
        }
    }
    Ok(merged.into_iter().map(|(_, _, p)| p).collect())
}

fn params_schema(params: &[&Value], location: &str) -> Option<Value> {
    let mut props = Map::new();
    let mut required = Vec::new();
    for p in params
        .iter()
        .filter(|p| p.get("in").and_then(Value::as_str) == Some(location))
    {
        let Some(name) = p.get("name").and_then(Value::as_str) else {
            continue;
        };
        if location == "header"
            && matches!(
                name.to_ascii_lowercase().as_str(),
                "content-type" | "accept" | "authorization"
            )
        {
            continue;
        }
        let schema = match p.get("schema") {
            Some(s) => normalize_schema(s, REF_PREFIX, true),
            None => p
                .get("content")
                .and_then(Value::as_object)
                .and_then(json_media)
                .map(|(_, m)| schema_of(m))
                .unwrap_or_else(|| json!({})),
        };
        props.insert(name.to_owned(), schema);
        if p.get("required").and_then(Value::as_bool) == Some(true) || location == "path" {
            required.push(Value::String(name.to_owned()));
        }
    }
    if props.is_empty() {
        return None;
    }
    let mut obj = Map::new();
    obj.insert("type".into(), json!("object"));
    obj.insert("properties".into(), Value::Object(props));
    if !required.is_empty() {
        obj.insert("required".into(), Value::Array(required));
    }
    Some(Value::Object(obj))
}

fn compile_outputs(doc: &Value, op: &Value, at: &str) -> Result<Vec<OutputVariant>, ImportError> {
    let mut outputs = Vec::new();
    let Some(responses) = op.get("responses").and_then(Value::as_object) else {
        return Ok(outputs);
    };
    for (status, resp) in responses {
        let selector = match status.as_str() {
            "default" => StatusSelector::Default,
            s => match s.parse::<u16>() {
                Ok(code) => StatusSelector::Code(code),
                Err(_) => continue, // ranges like `2XX` are not representable
            },
        };
        let rat = format!("{at}/responses/{status}");
        let resp = deref(doc, resp, &rat)?;
        let content = resp.get("content").and_then(Value::as_object);
        let is_2xx = matches!(selector, StatusSelector::Code(c) if (200..300).contains(&c));
        let event_stream = content
            .filter(|_| is_2xx)
            .and_then(|c| c.iter().find(|(k, _)| is_event_stream(k)));
        let (content_type, schema) = match (event_stream, content) {
            (Some((_, media)), _) => (
                Some(EVENT_STREAM.to_owned()),
                Some(stream_item_schema(media, media.get("schema"), |s| {
                    normalize_schema(s, REF_PREFIX, true)
                })),
            ),
            (None, None) => (None, None),
            (None, Some(c)) if c.is_empty() => (None, None),
            (None, Some(c)) => match json_media(c) {
                Some((ct, media)) => (Some(ct.clone()), Some(schema_of(media))),
                None => (c.keys().next().cloned(), None),
            },
        };
        outputs.push(OutputVariant {
            status: selector,
            content_type,
            schema,
            headers: response_headers(doc, resp, &rat),
        });
    }
    Ok(outputs)
}

/// The headers a response object declares, each with its schema (a string
/// when it gives none). A header whose `$ref` does not resolve is left out.
fn response_headers(doc: &Value, resp: &Value, at: &str) -> Map<String, Value> {
    let Some(headers) = resp.get("headers").and_then(Value::as_object) else {
        return Map::new();
    };
    headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("content-type"))
        .filter_map(|(name, header)| {
            let header = deref(doc, header, &format!("{at}/headers/{name}")).ok()?;
            let schema = header
                .get("schema")
                .map(|s| normalize_schema(s, REF_PREFIX, true))
                .unwrap_or_else(|| json!({"type": "string"}));
            Some((name.clone(), schema))
        })
        .collect()
}

/// The request body schema and whether the body is required: the JSON
/// media entry's, else the form's (then the multipart one's), tagged with
/// its [`CONTENT_TYPE_EXT`].
fn compile_body(doc: &Value, op: &Value, at: &str) -> Result<(Option<Value>, bool), ImportError> {
    let Some(body) = op.get("requestBody") else {
        return Ok((None, false));
    };
    let body = deref(doc, body, &format!("{at}/requestBody"))?;
    let content = body.get("content").and_then(Value::as_object);
    let schema = match content.and_then(json_media) {
        Some((_, m)) => Some(schema_of(m)),
        None => content.and_then(form_body),
    };
    let required = body.get("required").and_then(Value::as_bool) == Some(true);
    Ok((schema, required))
}

/// The form (or else multipart) body schema of `content`, tagged with its
/// media type.
fn form_body(content: &Map<String, Value>) -> Option<Value> {
    let media = |wanted: &str| {
        content.iter().find(|(k, _)| {
            k.split(';')
                .next()
                .is_some_and(|base| base.trim().eq_ignore_ascii_case(wanted))
        })
    };
    let (ct, m) = [FORM_CONTENT_TYPE, MULTIPART_CONTENT_TYPE]
        .into_iter()
        .find_map(|ct| media(ct).map(|(_, m)| (ct, m)))?;
    let mut schema = schema_of(m);
    if let Value::Object(o) = &mut schema {
        o.insert(CONTENT_TYPE_EXT.into(), json!(ct));
    }
    Some(schema)
}

/// Rejects a `$ref` in `value` that names no definition. `at` names the
/// referrer and `base` is the JSON pointer of `value` in the source document
/// (empty when `value` was assembled from several places).
pub(crate) fn check_refs(
    value: &Value,
    defs: &Map<String, Value>,
    at: &str,
    base: &str,
) -> Result<(), ImportError> {
    let mut refs = Vec::new();
    collect_refs(value, &mut refs);
    for (pointer, r) in refs {
        if !defs_name(r).is_some_and(|n| defs.contains_key(n)) {
            let pointer = format!("{base}{pointer}");
            let location = if pointer.is_empty() {
                String::new()
            } else {
                format!(" at {pointer}")
            };
            return Err(invalid(
                r,
                format!("unresolvable $ref referenced from {at}{location}"),
            ));
        }
    }
    Ok(())
}

/// Checks the refs of every input and output schema of `op`.
pub(crate) fn check_operation_refs(
    op: &Operation,
    defs: &Map<String, Value>,
) -> Result<(), ImportError> {
    let input = &op.input;
    let parts = [
        ("path parameters", &input.path),
        ("query parameters", &input.query),
        ("header parameters", &input.headers),
        ("request body", &input.body),
    ];
    for (part, schema) in parts {
        if let Some(s) = schema {
            check_refs(s, defs, &format!("operation {} {part}", op.id), "")?;
        }
    }
    for o in &op.outputs {
        if let Some(s) = &o.schema {
            let status = match o.status {
                StatusSelector::Code(c) => c.to_string(),
                StatusSelector::Default => "default".into(),
            };
            let at = format!("operation {} response {status}", op.id);
            check_refs(s, defs, &at, "")?;
        }
    }
    Ok(())
}

pub(crate) fn compile(doc: &Value, bytes: &[u8]) -> Result<Bundle, ImportError> {
    let info = doc.get("info");
    let title = info
        .and_then(|i| i.get("title"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("/info/title", "missing required field"))?
        .to_owned();
    let version = info
        .and_then(|i| i.get("version"))
        .and_then(Value::as_str)
        .unwrap_or("0.0.0")
        .to_owned();

    let mut defs = Map::new();
    if let Some(schemas) = doc
        .pointer("/components/schemas")
        .and_then(Value::as_object)
    {
        for (name, s) in schemas {
            defs.insert(name.clone(), normalize_schema(s, REF_PREFIX, true));
        }
    }

    let mut ids = OperationIds::new(doc)?;
    let mut operations = Vec::new();
    let mut callbacks = Vec::new();
    let empty = Map::new();
    let paths = doc
        .get("paths")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for (path, item) in paths {
        let item = deref(doc, item, &format!("/paths/{}", escape(path)))?;
        for (key, method) in METHODS {
            let Some(op) = item.get(key) else { continue };
            let at = format!("/paths/{}/{key}", escape(path));
            let params = merged_parameters(doc, item, op, &at)?;
            let outputs = compile_outputs(doc, op, &at)?;
            let (body, body_required) = compile_body(doc, op, &at)?;
            let input = InputSchemas {
                path: params_schema(&params, "path"),
                query: params_schema(&params, "query"),
                headers: params_schema(&params, "header"),
                body,
                body_required,
            };
            let id = ids.assign(op, key, path);
            let kind = infer_kind(method, path);
            let mut op_out = Operation {
                id,
                protocol: Protocol::Http,
                binding: Binding::Http {
                    method,
                    path: path.clone(),
                },
                kind,
                summary: op.get("summary").and_then(Value::as_str).map(str::to_owned),
                input,
                outputs,
                resource_type: infer_resource_type(path),
                identity: None,
                semantic_hints: Vec::new(),
                security: compile_security(doc, op, "/components/securitySchemes", &at)?,
            };
            if let Some(kind) = stream_kind(&op_out) {
                op_out.kind = kind;
            }
            op_out.identity = infer_identity(
                &op_out.kind,
                path,
                op_out.success_output().and_then(|o| o.schema.as_ref()),
                &defs,
            );
            if let Some(d) = op.get("description").and_then(Value::as_str) {
                op_out.semantic_hints.push(d.to_owned());
            }
            if let Some(tags) = op.get("tags").and_then(Value::as_array) {
                op_out
                    .semantic_hints
                    .extend(tags.iter().filter_map(Value::as_str).map(str::to_owned));
            }

            check_operation_refs(&op_out, &defs)?;
            if let Some(cbs) = op.get("callbacks") {
                callbacks.push((op_out.id.clone(), cbs, format!("{at}/callbacks")));
            }
            operations.push(op_out);
        }
    }
    for (name, s) in &defs {
        check_refs(
            s,
            &defs,
            &format!("schema {name}"),
            &format!("/components/schemas/{}", escape(name)),
        )?;
    }

    let webhooks = compile_webhooks(doc, &defs, &callbacks)?;

    let digest = Sha256::digest(bytes);
    let source_digest = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(Bundle {
        id: slug(&title),
        title,
        version,
        source_digest,
        operations,
        defs,
        protocol_payload: None,
        channels: Vec::new(),
        webhooks,
    })
}

/// The contract's outbound events: each top-level `webhooks` entry (3.1),
/// then each callback of the operations in `callbacks` (operation id, its
/// `callbacks` object, and that object's pointer). A path item contributes
/// its first method with a JSON (or form) request body; a callback its
/// first expression. Callback URL expressions are not kept.
fn compile_webhooks(
    doc: &Value,
    defs: &Map<String, Value>,
    callbacks: &[(String, &Value, String)],
) -> Result<Vec<WebhookDef>, ImportError> {
    let mut out = Vec::new();
    if let Some(hooks) = doc.get("webhooks").and_then(Value::as_object) {
        for (name, item) in hooks {
            let at = format!("/webhooks/{}", escape(name));
            if let Some((payload_schema, summary)) = path_item_payload(doc, item, &at)? {
                check_refs(&payload_schema, defs, &format!("webhook {name}"), "")?;
                out.push(WebhookDef {
                    name: name.clone(),
                    operation: None,
                    payload_schema,
                    summary,
                });
            }
        }
    }
    for (op_id, cbs, at) in callbacks {
        let cbs = deref(doc, cbs, at)?;
        let Some(cbs) = cbs.as_object() else { continue };
        for (cb_name, cb) in cbs {
            let at = format!("{at}/{}", escape(cb_name));
            let cb = deref(doc, cb, &at)?;
            let Some((expr, item)) = cb.as_object().and_then(|o| o.iter().next()) else {
                continue;
            };
            let at = format!("{at}/{}", escape(expr));
            if let Some((payload_schema, summary)) = path_item_payload(doc, item, &at)? {
                let name = format!("{op_id}.{cb_name}");
                check_refs(&payload_schema, defs, &format!("callback {name}"), "")?;
                out.push(WebhookDef {
                    name,
                    operation: Some(op_id.clone()),
                    payload_schema,
                    summary,
                });
            }
        }
    }
    Ok(out)
}

/// The request body schema and summary of the first method of a path
/// item that has a body.
fn path_item_payload(
    doc: &Value,
    item: &Value,
    at: &str,
) -> Result<Option<(Value, Option<String>)>, ImportError> {
    let item = deref(doc, item, at)?;
    for (key, _) in METHODS {
        let Some(op) = item.get(key) else { continue };
        if let (Some(schema), _) = compile_body(doc, op, &format!("{at}/{key}"))? {
            let summary = op
                .get("summary")
                .or_else(|| item.get("summary"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            return Ok(Some((schema, summary)));
        }
    }
    Ok(None)
}
