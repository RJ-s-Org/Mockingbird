//! Inference of operation kind, resource type, and identity from HTTP shape.

use crate::schema::defs_name;
use mb_mcm::{Effect, HttpMethod, Identity, OperationKind};
use serde_json::{Map, Value};

fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

fn is_param(seg: &str) -> bool {
    seg.starts_with('{') && seg.ends_with('}')
}

pub(crate) fn infer_kind(method: HttpMethod, path: &str) -> OperationKind {
    match method {
        HttpMethod::Get | HttpMethod::Head | HttpMethod::Options => OperationKind::Query,
        HttpMethod::Post => {
            let ends_static = segments(path).last().is_none_or(|s| !is_param(s));
            OperationKind::Command(if ends_static {
                Effect::Create
            } else {
                Effect::Other
            })
        }
        HttpMethod::Put | HttpMethod::Patch => OperationKind::Command(Effect::Update),
        HttpMethod::Delete => OperationKind::Command(Effect::Delete),
    }
}

/// The last static segment before any trailing `{param}`.
pub(crate) fn infer_resource_type(path: &str) -> Option<String> {
    segments(path)
        .into_iter()
        .rev()
        .find(|s| !is_param(s))
        .map(str::to_owned)
}

fn trailing_param(path: &str) -> Option<&str> {
    segments(path)
        .last()
        .filter(|s| is_param(s))
        .map(|s| &s[1..s.len() - 1])
}

/// Follow `$ref` chains through `defs` (bounded, so ref cycles terminate).
fn resolve<'a>(schema: &'a Value, defs: &'a Map<String, Value>) -> Option<&'a Value> {
    let mut cur = schema;
    for _ in 0..16 {
        match cur.get("$ref").and_then(Value::as_str) {
            Some(r) => cur = defs.get(defs_name(r)?)?,
            None => return Some(cur),
        }
    }
    None
}

pub(crate) fn infer_identity(
    _kind: &OperationKind,
    path: &str,
    success_schema: Option<&Value>,
    defs: &Map<String, Value>,
) -> Option<Identity> {
    let path_param = trailing_param(path);
    let props = success_schema
        .and_then(|s| resolve(s, defs))
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object)?;
    let field = if props.contains_key("id") {
        "id".to_owned()
    } else {
        path_param
            .filter(|p| props.contains_key(*p))
            .map(str::to_owned)?
    };
    Some(Identity {
        path_param: path_param.map(str::to_owned),
        field,
        input_field: None,
    })
}
