//! Swagger 2.0 -> MCM compiler.

use crate::ImportError;
use crate::heuristics::{infer_identity, infer_kind, infer_resource_type};
use crate::openapi3::{
    CONTENT_TYPE_EXT, EVENT_STREAM, FORM_CONTENT_TYPE, METHODS, MULTIPART_CONTENT_TYPE,
    OperationIds, check_operation_refs, check_refs, deref, escape, invalid, is_event_stream,
    is_json_media, slug, stream_item_schema, stream_kind,
};
use crate::schema::normalize_schema;
use crate::security::compile_security;
use mb_mcm::{Binding, Bundle, InputSchemas, Operation, OutputVariant, Protocol, StatusSelector};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const REF_PREFIX: &str = "#/definitions/";

/// Parameter keys that describe the parameter itself, not its value schema.
const PARAM_META: [&str; 6] = [
    "name",
    "in",
    "required",
    "description",
    "allowEmptyValue",
    "collectionFormat",
];

fn norm(schema: &Value) -> Value {
    normalize_schema(schema, REF_PREFIX, true)
}

fn merged_parameters<'a>(
    doc: &'a Value,
    path_item: &'a Value,
    op: &'a Value,
    at: &str,
) -> Result<Vec<&'a Value>, ImportError> {
    let mut merged: Vec<(&str, &str, &Value)> = Vec::new();
    for list in [path_item, op] {
        let Some(params) = list.get("parameters").and_then(Value::as_array) else {
            continue;
        };
        for p in params {
            let p = deref(doc, p, &format!("{at}/parameters"))?;
            let name = p.get("name").and_then(Value::as_str).unwrap_or_default();
            let loc = p.get("in").and_then(Value::as_str).unwrap_or_default();
            match merged.iter_mut().find(|(n, l, _)| *n == name && *l == loc) {
                Some(slot) => slot.2 = p,
                None => merged.push((name, loc, p)),
            }
        }
    }
    Ok(merged.into_iter().map(|(_, _, p)| p).collect())
}

/// Swagger 2 non-body parameters carry their schema inline (`type`, `enum`, ...).
/// A `type: file` form parameter is a binary string.
fn inline_param_schema(p: &Value) -> Value {
    if p.get("type").and_then(Value::as_str) == Some("file") {
        return json!({"type": "string", "format": "binary"});
    }
    let obj: Map<String, Value> = p
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| !PARAM_META.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    norm(&Value::Object(obj))
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
        props.insert(name.to_owned(), inline_param_schema(p));
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

/// The `in: body` schema, else an object schema built from `formData`
/// params (tagged with its `x-content-type`), and whether it is required.
fn compile_body(params: &[&Value], consumes: &[String]) -> (Option<Value>, bool) {
    let is_required = |p: &Value| p.get("required").and_then(Value::as_bool) == Some(true);
    let body = params
        .iter()
        .find(|p| p.get("in").and_then(Value::as_str) == Some("body"));
    if let Some(p) = body
        && let Some(schema) = p.get("schema")
    {
        return (Some(norm(schema)), is_required(p));
    }
    let Some(mut form) = params_schema(params, "formData") else {
        return (None, false);
    };
    let has = |ct: &str| consumes.iter().any(|c| c.eq_ignore_ascii_case(ct));
    let content_type = if !has(FORM_CONTENT_TYPE) && has(MULTIPART_CONTENT_TYPE) {
        MULTIPART_CONTENT_TYPE
    } else {
        FORM_CONTENT_TYPE
    };
    if let Value::Object(o) = &mut form {
        o.insert(CONTENT_TYPE_EXT.into(), json!(content_type));
    }
    let required = params
        .iter()
        .any(|p| p.get("in").and_then(Value::as_str) == Some("formData") && is_required(p));
    (Some(form), required)
}

fn media_types(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn compile_outputs(doc: &Value, op: &Value, at: &str) -> Result<Vec<OutputVariant>, ImportError> {
    let mut outputs = Vec::new();
    let Some(responses) = op.get("responses").and_then(Value::as_object) else {
        return Ok(outputs);
    };
    // Operation-level `produces` replaces the global list.
    let produces = match op.get("produces") {
        Some(p) => media_types(Some(p)),
        None => media_types(doc.get("produces")),
    };
    let json_ct = produces.iter().find(|m| is_json_media(m)).cloned();
    let event_stream = produces.iter().any(|m| is_event_stream(m));
    for (status, resp) in responses {
        let selector = match status.as_str() {
            "default" => StatusSelector::Default,
            s => match s.parse::<u16>() {
                Ok(code) => StatusSelector::Code(code),
                Err(_) => continue,
            },
        };
        let resp = deref(doc, resp, &format!("{at}/responses/{status}"))?;
        let is_2xx = matches!(selector, StatusSelector::Code(c) if (200..300).contains(&c));
        // A 2xx response of an operation producing an event stream streams
        // its items; the stream extensions sit on the response object.
        let (content_type, schema) = match resp.get("schema") {
            s if is_2xx && event_stream => (
                Some(EVENT_STREAM.to_owned()),
                Some(stream_item_schema(resp, s, norm)),
            ),
            None => (None, None),
            Some(s) if produces.is_empty() => (Some("application/json".to_owned()), Some(norm(s))),
            Some(s) => match &json_ct {
                Some(ct) => (Some(ct.clone()), Some(norm(s))),
                None => (produces.first().cloned(), None),
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
    if let Some(schemas) = doc.get("definitions").and_then(Value::as_object) {
        for (name, s) in schemas {
            defs.insert(name.clone(), norm(s));
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
            // Operation-level `consumes` replaces the global list.
            let consumes = match op.get("consumes") {
                Some(c) => media_types(Some(c)),
                None => media_types(doc.get("consumes")),
            };
            let (body, body_required) = compile_body(&params, &consumes);
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
                security: compile_security(doc, op, "/securityDefinitions", &at)?,
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
            operations.push(op_out);
        }
    }
    for (name, s) in &defs {
        check_refs(
            s,
            &defs,
            &format!("schema {name}"),
            &format!("/definitions/{}", escape(name)),
        )?;
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
        protocol_payload: None,
        channels: Vec::new(),
        webhooks: Vec::new(),
    })
}
