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
use mb_ext::{CallInfo, Rejection, Tenancy, UsageEvent};
use mb_mcm::{Binding, Effect, HttpMethod, Operation, OperationKind, StatusSelector};
use mb_state::{SessionKey, StateError, StoredResource};
use serde_json::{Map, Number, Value, json};
use tokio::time::Instant;
use tracing::Instrument;
use tracing::field::Empty;

use super::problem::{problem, problem_body, render};
use super::routing::{RouteError, RouteTable};
use crate::{MockRuntime, RequestRecord, ScenarioRuntime};

const SESSION_HEADER: &str = "x-mockingbird-session";
const STATUS_HEADER: &str = "x-mockingbird-status";
const LATENCY_HEADER: &str = "x-mockingbird-latency";
const SCENARIO_HEADER: &str = "x-mockingbird-scenario";
const SOURCE_HEADER: HeaderName = HeaderName::from_static("x-mockingbird-source");
const DEFAULT_SESSION: &str = "default";
const WORKSPACE: &str = "default";
const MAX_LATENCY: Duration = Duration::from_secs(30);
const MAX_SESSION_LEN: usize = 128;
const LIST_CONTEXT_LIMIT: usize = 20;
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
const MULTIPART_CONTENT_TYPE: &str = "multipart/form-data";
/// Usage-event source for responses that were not generated (problems).
const NO_SOURCE: &str = "none";

/// Headers never forwarded to the engine or recorded (they may carry
/// credentials).
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

/// What serving one call produced, beyond the response itself; feeds the
/// usage event and the request record.
struct CallLog {
    operation_id: Option<String>,
    session: String,
    /// Tenancy resolved during admission.
    tenancy: Option<Tenancy>,
    /// Whether admission ran (a rejected call has no tenancy to bill).
    admitted_or_rejected: bool,
    source: Option<&'static str>,
    prompt_tokens: u32,
    completion_tokens: u32,
    response: Option<Value>,
    /// Injected latency, slept after the call is recorded.
    delay: Option<Duration>,
    /// The body media type the matched operation declares (`x-content-type`).
    declared_body_type: Option<String>,
}

impl CallLog {
    fn new() -> Self {
        Self {
            operation_id: None,
            session: DEFAULT_SESSION.into(),
            tenancy: None,
            admitted_or_rejected: false,
            source: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            response: None,
            delay: None,
            declared_body_type: None,
        }
    }

    /// A problem response, remembered for the request record.
    fn problem(&mut self, status: u16, title: &str, detail: impl Into<String>) -> Response {
        let (code, body) = problem_body(status, title, detail.into());
        let resp = render(code, &body);
        self.response = Some(body);
        resp
    }

    /// A generated response, remembered for the request record.
    fn generated(&mut self, generated: GeneratedResponse) -> Response {
        self.source = Some(generated.source.as_str());
        self.prompt_tokens = generated.usage.prompt_tokens;
        self.completion_tokens = generated.usage.completion_tokens;
        self.response = generated.body.clone();
        respond(generated)
    }

    fn rejected(&mut self, r: Rejection) -> Response {
        let mut resp = self.problem(r.status, &r.title, r.detail);
        if let Some(after) = r.retry_after {
            let secs = after.as_secs() + u64::from(after.subsec_nanos() > 0);
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        resp
    }
}

/// The single fallback handler serving every operation.
///
/// Each call runs in a `mock.request` span. Once the response is built it
/// is reported to the usage sink and the request recorder, and only then is
/// any injected latency slept.
pub(crate) async fn handle(
    State(st): State<Arc<RestState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    raw_body: Bytes,
) -> Response {
    let rt = &st.rt;
    let span = tracing::info_span!(
        "mock.request",
        mock = %rt.name,
        operation = Empty,
        session = Empty,
        status = Empty,
        source = Empty,
    );
    let started = Instant::now();
    let mut log = CallLog::new();
    let resp = serve(&st, &method, &uri, &headers, &raw_body, &mut log)
        .instrument(span.clone())
        .await;
    let status = resp.status().as_u16();
    span.record("status", status);
    if let Some(source) = log.source {
        span.record("source", source);
    }
    let latency = started.elapsed() + log.delay.unwrap_or_default();
    report(
        rt, &method, &uri, &headers, &raw_body, &log, status, latency,
    )
    .instrument(span.clone())
    .await;
    if let Some(delay) = log.delay {
        tokio::time::sleep(delay).instrument(span).await;
    }
    resp
}

async fn serve(
    st: &RestState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    raw_body: &[u8],
    log: &mut CallLog,
) -> Response {
    let rt = &st.rt;
    let path = uri.path();

    // 1. Route.
    let found = to_http_method(method).and_then(|m| st.routes.find(m, path));
    let Some((op_id, raw_params)) = found else {
        return unrouted(&st.routes, method, path, log);
    };
    tracing::Span::current().record("operation", op_id.as_str());
    log.operation_id = Some(op_id.clone());
    let Some(op) = rt.bundle.operation(&op_id) else {
        return log.problem(
            500,
            "Internal Server Error",
            format!("unknown operation {op_id}"),
        );
    };
    let validators = st.inputs.get(&op_id);
    log.declared_body_type = op
        .input
        .body
        .as_ref()
        .and_then(|s| s.get("x-content-type"))
        .and_then(Value::as_str)
        .map(str::to_string);

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
    let body = match decode_body(op, raw_body) {
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
        return invalid_input(rt, op, &errors, log);
    }

    // 3. Admission.
    let call_info = CallInfo {
        mock: &rt.name,
        operation_id: &op.id,
        headers,
    };
    log.admitted_or_rejected = true;
    let tenancy = match rt.ext.tenants.resolve(&call_info).await {
        Ok(t) => t,
        Err(r) => return log.rejected(r),
    };
    let admitted = rt.ext.quota.admit(&tenancy, &call_info).await;
    log.tenancy = Some(tenancy);
    if let Err(r) = admitted {
        return log.rejected(r);
    }

    // 4. Overrides and scenario.
    let overrides = match Overrides::parse(headers, rt.header_overrides) {
        Ok(o) => o,
        Err(detail) => return log.problem(400, "Invalid override", detail),
    };
    tracing::Span::current().record("session", overrides.session.as_str());
    log.session = overrides.session.clone();
    let scenario = match overrides.scenario.as_deref() {
        None => None,
        Some(name) => match rt.scenarios.get(name) {
            None => {
                return log.problem(
                    400,
                    "Unknown scenario",
                    format!("mock {} has no scenario {name:?}", rt.name),
                );
            }
            Some(s) => s.applies_to(&op.id).then_some(s),
        },
    };
    // A scenario's status applies only where the contract declares it; the
    // header override is used as given (an undeclared one is a 400).
    let scenario_status = scenario
        .and_then(|s| s.status)
        .filter(|code| op.output_for_status(*code).is_some());
    let forced_status = overrides.status.or(scenario_status);
    let delay = overrides
        .latency
        .or_else(|| scenario.and_then(|s| s.latency))
        .map(|d| d.min(MAX_LATENCY));

    // 5. State read.
    let session = SessionKey {
        workspace: WORKSPACE.into(),
        mock: rt.name.clone(),
        session: overrides.session,
    };
    let mut state = StateAccess::new(rt);
    let resource_type = op.resource_type.as_deref();
    let target_id = target_id(op, &path_params);
    let target = match (resource_type, &target_id) {
        (Some(t), Some(id)) => state.get(&session, t, id).await,
        _ => None,
    };
    let related = related(
        &mut state,
        &session,
        op,
        target_id.is_some(),
        &query,
        body.as_ref(),
    )
    .await;

    // 6. Generate.
    let call = OperationCall {
        operation_id: op.id.clone(),
        path_params,
        query,
        headers: forwarded_headers(headers),
        body,
    };
    let ctx = GenerationContext {
        related: &related,
        target: target.as_ref(),
        forced_status,
        scenario: scenario.map(|s: &ScenarioRuntime| &s.spec),
    };
    let generated = match rt.engine.generate(&rt.bundle, op, &call, ctx).await {
        Ok(g) => g,
        Err(EngineError::UndeclaredStatus(code)) => {
            return log.problem(
                400,
                "Undeclared status override",
                format!("operation {} declares no response for status {code}", op.id),
            );
        }
        Err(e) => return log.problem(500, "Generation failed", e.to_string()),
    };

    // 7. State write.
    if (200..300).contains(&generated.status)
        && let Some(t) = resource_type
    {
        write_state(
            &mut state, &session, op, t, target_id, &generated, call.body,
        )
        .await;
    }

    // 8. Respond (the handler sleeps `delay` after recording).
    log.delay = delay;
    log.generated(generated)
}

/// Report a served call to the usage sink and the request recorder.
#[allow(clippy::too_many_arguments)]
async fn report(
    rt: &MockRuntime,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    raw_body: &[u8],
    log: &CallLog,
    status: u16,
    latency: Duration,
) {
    let operation_id = log.operation_id.clone().unwrap_or_default();
    // Calls that never reached admission are still attributed to a tenant.
    let tenancy = match (&log.tenancy, log.admitted_or_rejected) {
        (Some(t), _) => Some(t.clone()),
        (None, true) => None,
        (None, false) => {
            let call_info = CallInfo {
                mock: &rt.name,
                operation_id: &operation_id,
                headers,
            };
            rt.ext.tenants.resolve(&call_info).await.ok()
        }
    };
    if let Some(tenancy) = tenancy {
        rt.ext.usage.record(&UsageEvent {
            tenancy,
            mock: rt.name.clone(),
            operation_id: operation_id.clone(),
            status,
            source: log.source.unwrap_or(NO_SOURCE).to_string(),
            latency,
            prompt_tokens: log.prompt_tokens,
            completion_tokens: log.completion_tokens,
        });
    }
    if let Some(recorder) = &rt.recorder {
        recorder.record(RequestRecord {
            mock: rt.name.clone(),
            operation_id: log.operation_id.clone(),
            session: log.session.clone(),
            method: method.as_str().to_string(),
            path: uri.path().to_string(),
            status,
            source: log.source.map(str::to_string),
            latency,
            prompt_tokens: log.prompt_tokens,
            completion_tokens: log.completion_tokens,
            request: json!({
                "query": recorded_query(uri),
                "headers": recorded_headers(headers),
                "body": recorded_body(
                    raw_body,
                    media_type(headers).or(log.declared_body_type.clone()).as_deref(),
                ),
            }),
            response: log.response.clone(),
        });
    }
}

/// 404 when no operation has this path, 405 when only the method differs.
fn unrouted(routes: &RouteTable, method: &Method, path: &str, log: &mut CallLog) -> Response {
    let allowed = routes.allowed(path);
    if allowed.is_empty() {
        return log.problem(
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
    let mut resp = log.problem(
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
        return form_pairs(raw)
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
fn invalid_input(
    rt: &MockRuntime,
    op: &Operation,
    errors: &[String],
    log: &mut CallLog,
) -> Response {
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
            log.generated(rt.engine.fallback(
                &rt.bundle,
                op,
                variant,
                status,
                &OperationCall::default(),
                &GenerationContext::default(),
            ))
        }
        None => log.problem(400, "Invalid request", errors.join("; ")),
    }
}

struct Overrides {
    session: String,
    status: Option<u16>,
    latency: Option<Duration>,
    scenario: Option<String>,
}

impl Overrides {
    /// `Err` carries the problem detail for an unusable override header.
    fn parse(headers: &HeaderMap, enabled: bool) -> Result<Self, String> {
        let mut out = Self {
            session: DEFAULT_SESSION.into(),
            status: None,
            latency: None,
            scenario: None,
        };
        if !enabled {
            return Ok(out);
        }
        // An empty value means the default session.
        if let Some(s) = header_text(headers, SESSION_HEADER)?.filter(|s| !s.is_empty()) {
            if !is_session_id(s) {
                return Err(format!(
                    "X-Mockingbird-Session must be 1-{MAX_SESSION_LEN} characters of \
                     [A-Za-z0-9._-], got {s:?}"
                ));
            }
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
        out.scenario = header_text(headers, SCENARIO_HEADER)?.map(str::to_string);
        Ok(out)
    }
}

/// 1-128 characters of `[A-Za-z0-9._-]`. (An empty header is the default
/// session and never reaches this check.)
fn is_session_id(s: &str) -> bool {
    (1..=MAX_SESSION_LEN).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
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

/// State-store access for one call. The first failing call marks the mock
/// degraded and makes the rest of the call stateless; a successful call
/// clears the mark. While degraded, [`crate::StateHealth`] lets one call per
/// retry interval try the store; the rest skip it entirely.
struct StateAccess<'a> {
    rt: &'a MockRuntime,
    failed: bool,
}

impl<'a> StateAccess<'a> {
    /// A call skips the store while the breaker holds it off.
    fn new(rt: &'a MockRuntime) -> Self {
        let failed = !rt.state_health.try_acquire();
        Self { rt, failed }
    }

    /// `None` when the store is unavailable for this call.
    fn settle<T>(&mut self, result: Result<T, StateError>) -> Option<T> {
        match result {
            Ok(v) => {
                self.rt.state_health.record_success();
                Some(v)
            }
            Err(e) => {
                tracing::warn!(mock = %self.rt.name, error = %e, "state store failed; serving statelessly");
                self.rt.state_health.record_failure();
                metrics::counter!("mb_state_errors_total", "mock" => self.rt.name.clone())
                    .increment(1);
                self.failed = true;
                None
            }
        }
    }

    async fn get(&mut self, s: &SessionKey, t: &str, id: &str) -> Option<StoredResource> {
        if self.failed {
            return None;
        }
        let r = self.rt.state.get(s, t, id).await;
        self.settle(r).flatten()
    }

    async fn list(&mut self, s: &SessionKey, t: &str, limit: usize) -> Vec<StoredResource> {
        if self.failed {
            return Vec::new();
        }
        let r = self.rt.state.list(s, t, limit).await;
        self.settle(r).unwrap_or_default()
    }

    async fn upsert(&mut self, s: &SessionKey, r: StoredResource) {
        if !self.failed {
            let r = self.rt.state.upsert(s, r).await;
            self.settle(r);
        }
    }

    async fn delete(&mut self, s: &SessionKey, t: &str, id: &str) {
        if !self.failed {
            let r = self.rt.state.delete(s, t, id).await;
            self.settle(r);
        }
    }
}

/// Resources the response should stay consistent with: recent items for a
/// list query, plus anything referenced by `<x>Id` fields.
async fn related(
    state: &mut StateAccess<'_>,
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
        out = state.list(session, t, LIST_CONTEXT_LIMIT).await;
    }
    let body_fields = body.and_then(Value::as_object).into_iter().flatten();
    for (field, value) in query.iter().chain(body_fields) {
        let (Some(prefix), Some(id)) = (reference_prefix(field), value.as_str()) else {
            continue;
        };
        for t in [format!("{prefix}s"), prefix.to_string()] {
            if let Some(hit) = state.get(session, &t, id).await {
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
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    resource_type: &str,
    target_id: Option<String>,
    generated: &GeneratedResponse,
    request_body: Option<Value>,
) {
    let data = generated.body.clone().or(request_body);
    match op.kind {
        OperationKind::Command(Effect::Create) => {
            let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
            let Some(data) = data else { return };
            let Some(id) = data.get(field).and_then(id_string) else {
                return;
            };
            let resource = StoredResource {
                resource_type: resource_type.into(),
                id,
                data,
            };
            state.upsert(session, resource).await;
        }
        OperationKind::Command(Effect::Update) => {
            let (Some(id), Some(data)) = (target_id, data) else {
                return;
            };
            let resource = StoredResource {
                resource_type: resource_type.into(),
                id,
                data,
            };
            state.upsert(session, resource).await;
        }
        OperationKind::Command(Effect::Delete) => {
            let Some(id) = target_id else { return };
            state.delete(session, resource_type, &id).await;
        }
        _ => {}
    }
}

fn is_sensitive(name: &str) -> bool {
    SENSITIVE_HEADERS.contains(&name)
        || ["token", "secret", "password", "api-key"]
            .iter()
            .any(|s| name.contains(s))
}

/// Non-sensitive, UTF-8 request headers, optionally keeping
/// `X-Mockingbird-*` override headers.
fn safe_headers(headers: &HeaderMap, keep_overrides: bool) -> Map<String, Value> {
    headers
        .iter()
        .filter(|(name, _)| {
            let n = name.as_str();
            !is_sensitive(n) && (keep_overrides || !n.starts_with("x-mockingbird-"))
        })
        .filter_map(|(name, v)| {
            Some((
                name.as_str().to_string(),
                Value::String(v.to_str().ok()?.into()),
            ))
        })
        .collect()
}

/// Headers forwarded to the prompt.
fn forwarded_headers(headers: &HeaderMap) -> Map<String, Value> {
    safe_headers(headers, false)
}

/// Headers kept in the request record (overrides included).
fn recorded_headers(headers: &HeaderMap) -> Map<String, Value> {
    safe_headers(headers, true)
}

/// The raw query as recorded: repeated keys become arrays of strings.
fn recorded_query(uri: &Uri) -> Value {
    let pairs = serde_urlencoded::from_str::<Vec<(String, String)>>(uri.query().unwrap_or(""))
        .unwrap_or_default();
    Value::Object(string_params(pairs))
}

/// `pairs` as an object of strings; repeated keys become arrays of strings.
fn string_params(pairs: Vec<(String, String)>) -> Map<String, Value> {
    group(pairs)
        .into_iter()
        .map(|(k, mut values)| {
            let v = if values.len() == 1 {
                Value::String(values.remove(0))
            } else {
                Value::Array(values.into_iter().map(Value::String).collect())
            };
            (k, v)
        })
        .collect()
}

/// Decodes an `application/x-www-form-urlencoded` body.
fn form_pairs(raw: &[u8]) -> Result<Vec<(String, String)>, serde_urlencoded::de::Error> {
    serde_urlencoded::from_bytes::<Vec<(String, String)>>(raw)
}

/// The lowercased media type of the request's `Content-Type`, without
/// parameters.
fn media_type(headers: &HeaderMap) -> Option<String> {
    let ct = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    let media = ct.split(';').next().unwrap_or_default().trim();
    (!media.is_empty()).then(|| media.to_ascii_lowercase())
}

/// Recorded in place of a multipart body.
const MULTIPART_OMITTED: &str = "[multipart omitted]";
/// Recorded in place of a form body that does not decode.
const FORM_UNDECODABLE: &str = "[undecodable form body omitted]";

/// The request body as recorded, given its media type: a form body as an
/// object of its fields (so field redaction applies), a multipart body as
/// [`MULTIPART_OMITTED`], otherwise JSON when it parses, else text, else
/// null.
fn recorded_body(raw: &[u8], media_type: Option<&str>) -> Value {
    if media_type == Some(MULTIPART_CONTENT_TYPE) {
        return Value::String(MULTIPART_OMITTED.into());
    }
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Value::Null;
    }
    if media_type == Some(FORM_CONTENT_TYPE) {
        return match form_pairs(raw) {
            Ok(pairs) => Value::Object(string_params(pairs)),
            Err(_) => Value::String(FORM_UNDECODABLE.into()),
        };
    }
    serde_json::from_slice(raw)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(raw).into_owned()))
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
