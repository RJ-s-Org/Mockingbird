//! OpenAPI 3.0 / 3.1 -> MCM compiler.

use crate::ImportError;
use crate::heuristics::{infer_identity, infer_kind, infer_resource_type};
use crate::schema::{collect_refs, defs_name, normalize_schema};
use crate::security::compile_security;
use mb_mcm::{
    Binding, Bundle, HttpMethod, InputSchemas, Operation, OutputVariant, Protocol, StatusSelector,
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
        let (content_type, schema) = match content {
            None => (None, None),
            Some(c) if c.is_empty() => (None, None),
            Some(c) => match json_media(c) {
                Some((ct, media)) => (Some(ct.clone()), Some(schema_of(media))),
                None => (c.keys().next().cloned(), None),
            },
        };
        outputs.push(OutputVariant {
            status: selector,
            content_type,
            schema,
        });
    }
    Ok(outputs)
}

/// The JSON request body schema and whether the body is required.
fn compile_body(doc: &Value, op: &Value, at: &str) -> Result<(Option<Value>, bool), ImportError> {
    let Some(body) = op.get("requestBody") else {
        return Ok((None, false));
    };
    let body = deref(doc, body, &format!("{at}/requestBody"))?;
    let schema = body
        .get("content")
        .and_then(Value::as_object)
        .and_then(json_media)
        .map(|(_, m)| schema_of(m));
    let required = body.get("required").and_then(Value::as_bool) == Some(true);
    Ok((schema, required))
}

pub(crate) fn check_refs(
    value: &Value,
    defs: &Map<String, Value>,
    at: &str,
) -> Result<(), ImportError> {
    let mut refs = Vec::new();
    collect_refs(value, &mut refs);
    for r in refs {
        if !defs_name(r).is_some_and(|n| defs.contains_key(n)) {
            return Err(invalid(
                r,
                format!("unresolvable $ref referenced from {at}"),
            ));
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

            let op_at = format!("operation {}", op_out.id);
            for s in [
                &op_out.input.path,
                &op_out.input.query,
                &op_out.input.headers,
                &op_out.input.body,
            ]
            .into_iter()
            .flatten()
            .chain(op_out.outputs.iter().filter_map(|o| o.schema.as_ref()))
            {
                check_refs(s, &defs, &op_at)?;
            }
            operations.push(op_out);
        }
    }
    for (name, s) in &defs {
        check_refs(s, &defs, &format!("schema {name}"))?;
    }

    let digest = Sha256::digest(bytes);
    let source_digest = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(Bundle {
        id: slug(&title),
        title,
        version,
        source_digest,
        operations,
        defs,
    })
}
