//! Swagger 2.0 -> MCM compiler.

use crate::ImportError;
use crate::heuristics::{infer_identity, infer_kind, infer_resource_type};
use crate::openapi3::{METHODS, check_refs, deref, escape, invalid, is_json_media, slug};
use crate::schema::normalize_schema;
use mb_mcm::{Binding, Bundle, InputSchemas, Operation, OutputVariant, Protocol, StatusSelector};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const REF_PREFIX: &str = "#/definitions/";
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

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
fn inline_param_schema(p: &Value) -> Value {
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

/// The `in: body` schema, else an object schema built from `formData` params.
fn compile_body(params: &[&Value]) -> Option<Value> {
    let body = params
        .iter()
        .find(|p| p.get("in").and_then(Value::as_str) == Some("body"));
    if let Some(schema) = body.and_then(|p| p.get("schema")) {
        return Some(norm(schema));
    }
    let mut form = params_schema(params, "formData")?;
    if let Value::Object(o) = &mut form {
        o.insert("x-content-type".into(), json!(FORM_CONTENT_TYPE));
    }
    Some(form)
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
    for (status, resp) in responses {
        let selector = match status.as_str() {
            "default" => StatusSelector::Default,
            s => match s.parse::<u16>() {
                Ok(code) => StatusSelector::Code(code),
                Err(_) => continue,
            },
        };
        let resp = deref(doc, resp, &format!("{at}/responses/{status}"))?;
        let (content_type, schema) = match resp.get("schema") {
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
            let input = InputSchemas {
                path: params_schema(&params, "path"),
                query: params_schema(&params, "query"),
                headers: params_schema(&params, "header"),
                body: compile_body(&params),
            };
            let id = op
                .get("operationId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{key}_{}", slug(path).replace('-', "_")));
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
