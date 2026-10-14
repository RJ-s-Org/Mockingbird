use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::Response;
use mb_engine::{
    EngineError, GeneratedResponse, GenerationContext, OperationCall, SchemaValidator,
};
use mb_mcm::{Binding, Effect, HttpMethod, Operation, OperationKind, StatusSelector};
use mb_state::{SessionKey, StoredResource};
use serde_json::{Map, Number, Value};

use super::problem::problem;
use super::routing::{RouteError, RouteTable};
use crate::MockRuntime;

const SESSION_HEADER: &str = "x-mockingbird-session";
const STATUS_HEADER: &str = "x-mockingbird-status";
const LATENCY_HEADER: &str = "x-mockingbird-latency";
const SOURCE_HEADER: HeaderName = HeaderName::from_static("x-mockingbird-source");
const DEFAULT_SESSION: &str = "default";
const WORKSPACE: &str = "default";
const MAX_LATENCY: Duration = Duration::from_secs(30);
const LIST_CONTEXT_LIMIT: usize = 20;
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
const MULTIPART_CONTENT_TYPE: &str = "multipart/form-data";

/// Headers never forwarded to the engine (they may carry credentials).
const SENSITIVE_HEADERS: [&str; 6] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
];

pub(crate) struct RestState {
    rt: Arc<MockRuntime>,
    routes: RouteTable,
    inputs: HashMap<String, InputValidators>,
}

#[derive(Default)]
struct InputValidators {
    path: Option<SchemaValidator>,
    query: Option<SchemaValidator>,
    body: Option<SchemaValidator>,
}

impl RestState {
    pub(crate) fn new(rt: Arc<MockRuntime>) -> Result<Self, RouteError> {
        let routes = RouteTable::new(&rt.bundle)?;
        let compile = |op: &Operation, schema: &Option<Value>| {
            let schema = schema.as_ref()?;
            match SchemaValidator::new(&rt.bundle.with_reachable_defs(schema)) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(operation = %op.id, error = %e, "input schema skipped");
                    None
                }
            }
        };
        let inputs = rt
            .bundle
            .operations
            .iter()
            .map(|op| {
                let v = InputValidators {
                    path: compile(op, &op.input.path),
                    query: compile(op, &op.input.query),
                    body: compile(op, &op.input.body),
                };
                (op.id.clone(), v)
            })
            .collect();
        Ok(Self { rt, routes, inputs })
    }
}

/// The single fallback handler serving every operation.
pub(crate) async fn handle(
    State(st): State<Arc<RestState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    raw_body: Bytes,
) -> Response {
    let rt = &st.rt;
    let path = uri.path();

    // 1. Route.
    let found = to_http_method(&method).and_then(|m| st.routes.find(m, path));
    let Some((op_id, raw_params)) = found else {
        return unrouted(&st.routes, &method, path);
    };
    let Some(op) = rt.bundle.operation(&op_id) else {
        return problem(
            500,
            "Internal Server Error",
            format!("unknown operation {op_id}"),
        );
    };
    let validators = st.inputs.get(&op_id);

    // 2. Decode and validate.
    let mut errors = Vec::new();
    let path_params = coerce_params(
        raw_params
            .into_iter()
            .map(|(k, v)| (k, vec![v.as_str().unwrap_or_default().to_string()])),
        op.input.path.as_ref(),
    );
    let query = match serde_urlencoded::from_str::<Vec<(String, String)>>(uri.query().unwrap_or(""))
    {
        Ok(pairs) => coerce_params(group(pairs), op.input.query.as_ref()),
        Err(e) => {
            errors.push(format!("query: {e}"));
            Map::new()
        }
    };
    let body = match decode_body(op, &raw_body) {
        Ok(body) => body,
        Err(e) => {
            errors.push(e);
            None
        }
    };
    if let Some(v) = validators {
        check(
            &mut errors,
            "path",
            v.path.as_ref(),
            Some(&Value::Object(path_params.clone())),
        );
        check(
            &mut errors,
            "query",
            v.query.as_ref(),
            Some(&Value::Object(query.clone())),
        );
        check(&mut errors, "body", v.body.as_ref(), body.as_ref());
    }
    if !errors.is_empty() {
        return invalid_input(rt, op, &errors);
    }

    // 3. Overrides.
    let overrides = match Overrides::parse(&headers, rt.header_overrides) {
        Ok(o) => o,
        Err(detail) => return problem(400, "Invalid override", detail),
    };

    // 4. State read.
    let session = SessionKey {
        workspace: WORKSPACE.into(),
        mock: rt.name.clone(),
        session: overrides.session,
    };
    let resource_type = op.resource_type.as_deref();
    let target_id = target_id(op, &path_params);
    let target = match (resource_type, &target_id) {
        (Some(t), Some(id)) => rt.state.get(&session, t, id).await.unwrap_or_else(|e| {
            tracing::warn!(mock = %rt.name, error = %e, "state read failed; continuing without state");
            None
        }),
        _ => None,
    };
    let related = related(rt, &session, op, target_id.is_some(), &query, body.as_ref()).await;

    // 5. Generate.
    let call = OperationCall {
        operation_id: op.id.clone(),
        path_params,
        query,
        headers: forwarded_headers(&headers),
        body,
    };
    let ctx = GenerationContext {
        related: &related,
        target: target.as_ref(),
        forced_status: overrides.status,
        scenario: None,
    };
    let generated = match rt.engine.generate(&rt.bundle, op, &call, ctx).await {
        Ok(g) => g,
        Err(EngineError::UndeclaredStatus(code)) => {
            return problem(
                400,
                "Undeclared status override",
                format!("operation {} declares no response for status {code}", op.id),
            );
        }
        Err(e) => return problem(500, "Generation failed", e.to_string()),
    };

    // 6. State write.
    if (200..300).contains(&generated.status)
        && let Some(t) = resource_type
    {
        write_state(rt, &session, op, t, target_id, &generated, call.body).await;
    }

    // 7. Respond.
    if let Some(latency) = overrides.latency {
        tokio::time::sleep(latency).await;
    }
    respond(generated)
}

/// 404 when no operation has this path, 405 when only the method differs.
fn unrouted(routes: &RouteTable, method: &Method, path: &str) -> Response {
    let allowed = routes.allowed(path);
    if allowed.is_empty() {
        return problem(
            404,
            "Not Found",
            format!("no operation matches {method} {path}"),
        );
    }
    let allow = allowed
        .iter()
        .map(|m| method_name(*m))
        .collect::<Vec<_>>()
        .join(", ");
    let mut resp = problem(
        405,
        "Method Not Allowed",
        format!("{method} is not allowed on {path}"),
    );
    if let Ok(v) = HeaderValue::from_str(&allow) {
        resp.headers_mut().insert(header::ALLOW, v);
    }
    resp
}

fn to_http_method(m: &Method) -> Option<HttpMethod> {
    Some(match *m {
        Method::GET => HttpMethod::Get,
        Method::POST => HttpMethod::Post,
        Method::PUT => HttpMethod::Put,
        Method::PATCH => HttpMethod::Patch,
        Method::DELETE => HttpMethod::Delete,
        Method::HEAD => HttpMethod::Head,
        Method::OPTIONS => HttpMethod::Options,
        _ => return None,
    })
}

fn method_name(m: HttpMethod) -> &'static str {
    match m {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Head => "HEAD",
        HttpMethod::Options => "OPTIONS",
    }
}

/// Group repeated query keys, keeping first-seen order.
fn group(pairs: Vec<(String, String)>) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for (k, v) in pairs {
        match out.iter_mut().find(|(key, _)| *key == k) {
            Some((_, values)) => values.push(v),
            None => out.push((k, vec![v])),
        }
    }
    out
}

/// Turn string parameters into JSON values, coercing to the type the
/// schema's property declares. Repeated values become an array only when
/// the property is an array; otherwise the last one wins.
fn coerce_params(
    params: impl IntoIterator<Item = (String, Vec<String>)>,
    schema: Option<&Value>,
) -> Map<String, Value> {
    params
        .into_iter()
        .filter_map(|(name, values)| {
            let prop = schema
                .and_then(|s| s.get("properties"))
                .and_then(|p| p.get(&name));
            let value = if prop.is_some_and(|p| types(p).contains(&"array")) {
                let items = prop.and_then(|p| p.get("items"));
                Value::Array(values.iter().map(|v| coerce(v, items)).collect())
            } else {
                coerce(values.last()?, prop)
            };
            Some((name, value))
        })
        .collect()
}

fn types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn coerce(raw: &str, schema: Option<&Value>) -> Value {
    let types = schema.map(types).unwrap_or_default();
    if types.contains(&"integer")
        && let Ok(n) = raw.parse::<i64>()
    {
        return Value::Number(n.into());
    }
    if types.contains(&"number")
        && let Some(n) = raw.parse::<f64>().ok().and_then(Number::from_f64)
    {
        return Value::Number(n);
    }
    if types.contains(&"boolean") {
        match raw {
            "true" => return Value::Bool(true),
            "false" => return Value::Bool(false),
            _ => {}
        }
    }
    Value::String(raw.to_string())
}

/// The request body, decoded per the operation's body schema: JSON by
/// default, `application/x-www-form-urlencoded` (Swagger 2 `formData`)
/// coerced through the schema, and `multipart/form-data` passed through
/// unvalidated as no body. A missing body is an error only when the
/// contract marks it required.
fn decode_body(op: &Operation, raw: &[u8]) -> Result<Option<Value>, String> {
    let schema = op.input.body.as_ref();
    let content_type = schema
        .and_then(|s| s.get("x-content-type"))
        .and_then(Value::as_str);
    if content_type == Some(MULTIPART_CONTENT_TYPE) {
        return Ok(None);
    }
    if raw.iter().all(u8::is_ascii_whitespace) {
        return if op.input.body_required {
            Err("body: a request body is required".into())
        } else {
            Ok(None)
        };
    }
    if content_type == Some(FORM_CONTENT_TYPE) {
        return serde_urlencoded::from_bytes::<Vec<(String, String)>>(raw)
            .map(|pairs| Some(Value::Object(coerce_params(group(pairs), schema))))
            .map_err(|e| format!("body: not valid form data: {e}"));
    }
    match serde_json::from_slice(raw) {
        Ok(v) => Ok(Some(v)),
        Err(e) if schema.is_some() => Err(format!("body: not valid JSON: {e}")),
        Err(_) => Ok(None),
    }
}

fn check(
    errors: &mut Vec<String>,
    location: &str,
    validator: Option<&SchemaValidator>,
    value: Option<&Value>,
) {
    if let (Some(v), Some(value)) = (validator, value)
        && let Err(found) = v.validate(value)
    {
        errors.extend(found.into_iter().map(|e| format!("{location}{e}")));
    }
}

/// The contract's 400, 422 or `default` error shape if it declares one, else
/// problem+json 400. Never calls the LLM.
fn invalid_input(rt: &MockRuntime, op: &Operation, errors: &[String]) -> Response {
    let declared = [
        StatusSelector::Code(400),
        StatusSelector::Code(422),
        StatusSelector::Default,
    ]
    .into_iter()
    .find_map(|selector| {
        op.outputs
            .iter()
            .find(|o| o.status == selector && o.schema.is_some())
    });
    match declared {
        Some(variant) => {
            let status = match variant.status {
                StatusSelector::Code(c) => c,
                StatusSelector::Default => 400,
            };
            respond(rt.engine.fallback(
                &rt.bundle,
                op,
                variant,
                status,
                &OperationCall::default(),
                &GenerationContext::default(),
            ))
        }
        None => problem(400, "Invalid request", errors.join("; ")),
    }
}

struct Overrides {
    session: String,
    status: Option<u16>,
    latency: Option<Duration>,
}

impl Overrides {
    /// `Err` carries the problem detail for an unusable override header.
    fn parse(headers: &HeaderMap, enabled: bool) -> Result<Self, String> {
        let mut out = Self {
            session: DEFAULT_SESSION.into(),
            status: None,
            latency: None,
        };
        if !enabled {
            return Ok(out);
        }
        if let Some(s) = header_text(headers, SESSION_HEADER)?
            && !s.is_empty()
        {
            out.session = s.to_string();
        }
        if let Some(s) = header_text(headers, STATUS_HEADER)? {
            let code = s
                .parse::<u16>()
                .ok()
                .filter(|c| (100..=599).contains(c))
                .ok_or_else(|| {
                    format!("X-Mockingbird-Status must be an HTTP status code, got {s:?}")
                })?;
            out.status = Some(code);
        }
        if let Some(s) = header_text(headers, LATENCY_HEADER)? {
            let latency = humantime::parse_duration(s)
                .map_err(|e| format!("X-Mockingbird-Latency must be a duration like 150ms: {e}"))?;
            out.latency = Some(latency.min(MAX_LATENCY));
        }
        Ok(out)
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, String> {
    match headers.get(name) {
        None => Ok(None),
        Some(v) => v
            .to_str()
            .map(|s| Some(s.trim()))
            .map_err(|_| format!("{name} must be visible ASCII")),
    }
}

/// The id of the resource this operation reads, updates or deletes.
/// Falls back to the last path parameter for updates and deletes whose
/// identity hint names none.
fn target_id(op: &Operation, path_params: &Map<String, Value>) -> Option<String> {
    let param = match op.identity.as_ref().and_then(|i| i.path_param.clone()) {
        Some(p) => p,
        None if matches!(
            op.kind,
            OperationKind::Command(Effect::Update | Effect::Delete)
        ) =>
        {
            let Binding::Http { path, .. } = &op.binding else {
                return None;
            };
            last_path_param(path)?
        }
        None => return None,
    };
    path_params.get(&param).and_then(id_string)
}

fn last_path_param(template: &str) -> Option<String> {
    let start = template.rfind('{')?;
    let end = start + template[start..].find('}')?;
    Some(template[start + 1..end].to_string())
}

fn id_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `customerId` / `customer_id` → `customer`.
fn reference_prefix(field: &str) -> Option<&str> {
    field
        .strip_suffix("Id")
        .or_else(|| field.strip_suffix("_id"))
        .filter(|p| !p.is_empty())
}

/// Resources the response should stay consistent with: recent items for a
/// list query, plus anything referenced by `<x>Id` fields.
async fn related(
    rt: &MockRuntime,
    session: &SessionKey,
    op: &Operation,
    has_target: bool,
    query: &Map<String, Value>,
    body: Option<&Value>,
) -> Vec<StoredResource> {
    let mut out: Vec<StoredResource> = Vec::new();
    if matches!(op.kind, OperationKind::Query)
        && !has_target
        && let Some(t) = op.resource_type.as_deref()
    {
        out = rt
            .state
            .list(session, t, LIST_CONTEXT_LIMIT)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(mock = %rt.name, error = %e, "state read failed; continuing without state");
                Vec::new()
            });
    }
    let body_fields = body.and_then(Value::as_object).into_iter().flatten();
    for (field, value) in query.iter().chain(body_fields) {
        let (Some(prefix), Some(id)) = (reference_prefix(field), value.as_str()) else {
            continue;
        };
        for t in [format!("{prefix}s"), prefix.to_string()] {
            let hit = rt.state.get(session, &t, id).await.unwrap_or_else(|e| {
                tracing::warn!(mock = %rt.name, error = %e, "state read failed; continuing without state");
                None
            });
            if let Some(hit) = hit {
                if !out
                    .iter()
                    .any(|r| r.resource_type == hit.resource_type && r.id == hit.id)
                {
                    out.push(hit);
                }
                break;
            }
        }
    }
    out
}

async fn write_state(
    rt: &MockRuntime,
    session: &SessionKey,
    op: &Operation,
    resource_type: &str,
    target_id: Option<String>,
    generated: &GeneratedResponse,
    request_body: Option<Value>,
) {
    let data = generated.body.clone().or(request_body);
    let result = match op.kind {
        OperationKind::Command(Effect::Create) => {
            let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
            let Some(data) = data else { return };
            let Some(id) = data.get(field).and_then(id_string) else {
                return;
            };
            rt.state
                .upsert(
                    session,
                    StoredResource {
                        resource_type: resource_type.into(),
                        id,
                        data,
                    },
                )
                .await
        }
        OperationKind::Command(Effect::Update) => {
            let (Some(id), Some(data)) = (target_id, data) else {
                return;
            };
            rt.state
                .upsert(
                    session,
                    StoredResource {
                        resource_type: resource_type.into(),
                        id,
                        data,
                    },
                )
                .await
        }
        OperationKind::Command(Effect::Delete) => {
            let Some(id) = target_id else { return };
            rt.state
                .delete(session, resource_type, &id)
                .await
                .map(|_| ())
        }
        _ => return,
    };
    if let Err(e) = result {
        tracing::warn!(mock = %rt.name, error = %e, "state write failed; response served without persisting");
    }
}

/// Forward non-sensitive, UTF-8 request headers to the prompt.
fn forwarded_headers(headers: &HeaderMap) -> Map<String, Value> {
    headers
        .iter()
        .filter(|(name, _)| {
            let n = name.as_str();
            !SENSITIVE_HEADERS.contains(&n)
                && !n.starts_with("x-mockingbird-")
                && !["token", "secret", "password", "api-key"]
                    .iter()
                    .any(|s| n.contains(s))
        })
        .filter_map(|(name, v)| {
            Some((
                name.as_str().to_string(),
                Value::String(v.to_str().ok()?.into()),
            ))
        })
        .collect()
}

fn respond(generated: GeneratedResponse) -> Response {
    let Ok(status) = StatusCode::from_u16(generated.status) else {
        return problem(
            500,
            "Internal Server Error",
            format!("invalid status {}", generated.status),
        );
    };
    let body = match &generated.body {
        Some(v) => match serde_json::to_vec(v) {
            Ok(bytes) => Body::from(bytes),
            Err(e) => return problem(500, "Internal Server Error", e.to_string()),
        },
        None => Body::empty(),
    };
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let headers = resp.headers_mut();
    if generated.body.is_some() {
        let declared = generated
            .content_type
            .as_deref()
            .filter(|ct| is_json_media(ct))
            .and_then(|ct| HeaderValue::from_str(ct).ok());
        headers.insert(
            header::CONTENT_TYPE,
            declared.unwrap_or_else(|| HeaderValue::from_static("application/json")),
        );
    }
    headers.insert(
        SOURCE_HEADER,
        HeaderValue::from_static(generated.source.as_str()),
    );
    resp
}

/// `application/json` or any `+json` media type (parameters allowed).
fn is_json_media(ct: &str) -> bool {
    let base = ct
        .split(';')
        .next()
        .unwrap_or(ct)
        .trim()
        .to_ascii_lowercase();
    base == "application/json" || base.ends_with("+json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn coerces_by_schema_type() {
        let schema = json!({"type": "object", "properties": {
            "n": {"type": "integer"}, "x": {"type": ["number", "null"]},
            "b": {"type": "boolean"}, "s": {"type": "string"},
            "tags": {"type": "array", "items": {"type": "integer"}}
        }});
        let params = group(vec![
            ("n".into(), "3".into()),
            ("x".into(), "1.5".into()),
            ("b".into(), "true".into()),
            ("s".into(), "7".into()),
            ("tags".into(), "1".into()),
            ("tags".into(), "2".into()),
            ("free".into(), "a".into()),
            ("free".into(), "b".into()),
        ]);
        let out = Value::Object(coerce_params(params, Some(&schema)));
        assert_eq!(
            out,
            json!({"n": 3, "x": 1.5, "b": true, "s": "7", "tags": [1, 2], "free": "b"})
        );
    }

    #[test]
    fn reference_fields() {
        assert_eq!(reference_prefix("customerId"), Some("customer"));
        assert_eq!(reference_prefix("customer_id"), Some("customer"));
        assert_eq!(reference_prefix("Id"), None);
        assert_eq!(reference_prefix("id"), None);
        assert_eq!(last_path_param("/a/{x}/b/{y}"), Some("y".into()));
        assert_eq!(last_path_param("/a"), None);
    }
}
