use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::Response;
use futures::StreamExt;
use mb_engine::error_body::{self, Violation};
use mb_engine::{
    EngineError, GeneratedResponse, GenerationContext, OperationCall, SchemaValidator, Source,
};
use mb_ext::{Rejection, Tenancy};
use mb_llm::Usage;
use mb_mcm::{BodyVariant, HttpMethod, Operation, OperationKind, OutputVariant, StatusSelector};
use serde_json::{Map, Value, json};
use tokio::time::Instant;
use tracing::Instrument;
use tracing::field::Empty;

use super::form;
use super::problem::{problem, problem_body, render};
use super::routing::RouteTable;
use crate::call::{
    self, DEFAULT_SESSION, PlanError, PlanFailure, Presented, Report, credential_headers,
    forwarded_headers, public_query, recorded_headers,
};
use crate::limits::MAX_REQUEST_PARAMS;
use crate::{MockRuntime, ScenarioRuntime};

const SOURCE_HEADER: HeaderName = HeaderName::from_static("x-mockingbird-source");
const FALLBACK_REASON_HEADER: HeaderName = HeaderName::from_static("x-mockingbird-fallback-reason");
/// Why a request failed validation.
pub(super) const VALIDATION_HEADER: HeaderName =
    HeaderName::from_static("x-mockingbird-validation");
/// The longest [`VALIDATION_HEADER`] value, in bytes.
const MAX_VALIDATION_HEADER: usize = 2048;
const CHAOS_HEADER: HeaderName = HeaderName::from_static(crate::CHAOS_HEADER);
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
const MULTIPART_CONTENT_TYPE: &str = "multipart/form-data";

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
    /// One per declared body media type, as `InputSchemas::bodies` lists
    /// them.
    bodies: Vec<Option<SchemaValidator>>,
}

impl RestState {
    pub(crate) fn new(rt: Arc<MockRuntime>) -> Self {
        let routes = RouteTable::with_base_paths(&rt.bundle, &rt.base_paths);
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
                    bodies: op
                        .input
                        .bodies
                        .iter()
                        .map(|b| compile(op, &Some(b.schema.clone())))
                        .collect(),
                };
                (op.id.clone(), v)
            })
            .collect();
        Self { rt, routes, inputs }
    }

    /// The operations left unrouted ([`RouteTable::skipped`]).
    pub(crate) fn skipped(&self) -> &[super::RouteError] {
        self.routes.skipped()
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
    source: Option<Source>,
    /// Why the faker answered, when an LLM call failed or none is configured.
    fallback_reason: Option<String>,
    usage: Usage,
    /// The response body, kept only when `record` is set.
    response: Option<Value>,
    /// The request body as `serve` decoded it (JSON, or a nested form),
    /// kept only when `record` is set, so the record need not parse it
    /// again.
    request_body: Option<Value>,
    /// Whether a recorder will receive this call's record.
    record: bool,
    /// Injected latency, slept after the call is recorded.
    delay: Option<Duration>,
    /// The body media type the matched operation declares (`x-content-type`).
    declared_body_type: Option<String>,
    /// Why the request failed validation, if it did.
    validation: Vec<String>,
    /// What became of the answer's effects, as the record's `request` keeps
    /// them; `null` when it listed none.
    effects: Value,
    /// What the record's `request` notes of the call beside its body: what the world
    /// description leaves open about the answer's operation (`world_gap`) and what the
    /// executor noted (`exec_fallback`, `exec_fallback_on`, `expr_failed`; see
    /// [`call::RECORD_NOTES`]).
    notes: serde_json::Map<String, Value>,
    /// The response is an event stream, which reports the call itself when
    /// its body is dropped and sleeps any latency before its first event.
    streaming: bool,
}

impl CallLog {
    fn new(record: bool) -> Self {
        Self {
            operation_id: None,
            session: DEFAULT_SESSION.into(),
            tenancy: None,
            admitted_or_rejected: false,
            source: None,
            fallback_reason: None,
            usage: Usage::default(),
            response: None,
            request_body: None,
            record,
            delay: None,
            declared_body_type: None,
            validation: Vec::new(),
            effects: Value::Null,
            notes: serde_json::Map::new(),
            streaming: false,
        }
    }

    /// A problem response, remembered for the request record.
    fn problem(&mut self, status: u16, title: &str, detail: impl Into<String>) -> Response {
        let (code, body) = problem_body(status, title, detail.into());
        let resp = render(code, &body);
        if self.record {
            self.response = Some(body);
        }
        resp
    }

    /// An answer of operation `op`, remembered for the request record with what the
    /// world description leaves open about `op` (see [`call::seed_world_gaps`]). Every
    /// answer of an operation goes through here, whether or not it reached the
    /// pipeline that seeds it too.
    fn answered(
        &mut self,
        rt: &MockRuntime,
        op: &Operation,
        mut generated: GeneratedResponse,
    ) -> Response {
        call::seed_world_gaps(rt, op, &mut generated);
        self.generated(generated)
    }

    /// A generated response, remembered for the request record.
    fn generated(&mut self, generated: GeneratedResponse) -> Response {
        self.source = Some(generated.source);
        self.fallback_reason.clone_from(&generated.fallback_reason);
        self.usage = generated.usage;
        let resp = respond(&generated);
        if self.record {
            let mut recorded = Value::Null;
            call::record_effects(&mut recorded, &generated);
            self.effects = recorded["effects"].take();
            self.notes = call::RECORD_NOTES
                .iter()
                .filter_map(|name| {
                    let note = recorded.get_mut(*name)?.take();
                    (!note.is_null()).then(|| ((*name).to_string(), note))
                })
                .collect();
            self.response = generated.body;
        }
        resp
    }

    fn rejected(&mut self, r: Rejection) -> Response {
        let mut resp = self.problem(r.status, &r.title, r.detail);
        set_retry_after(&mut resp, r.retry_after);
        resp
    }
}

/// `Retry-After` in whole seconds, rounded up.
fn set_retry_after(resp: &mut Response, after: Option<Duration>) {
    if let Some(after) = after {
        let secs = after.as_secs() + u64::from(after.subsec_nanos() > 0);
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(secs));
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
    extensions: axum::http::Extensions,
    body: Body,
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
    let mut log = CallLog::new(rt.recorder.is_some());
    let (resp, raw_body) = match read_body(&headers, body, rt.limits.max_request_bytes).await {
        Ok(raw_body) => {
            let prefix = extensions
                .get::<crate::MountPrefix>()
                .map_or("", |p| p.0.as_str());
            let resp = serve(&st, (&method, &uri, prefix), &headers, &raw_body, &mut log)
                .instrument(span.clone())
                .await;
            (resp, raw_body)
        }
        Err(error) => {
            let found = to_http_method(&method).and_then(|m| st.routes.find(m, uri.path()));
            if let Some((op_id, _)) = found {
                span.record("operation", op_id.as_str());
                log.operation_id = Some(op_id);
            }
            let resp = match error {
                BodyError::TooLarge(limit) => log.problem(
                    413,
                    "Payload Too Large",
                    format!("request bodies are limited to {limit} bytes"),
                ),
                BodyError::Unreadable(detail) => {
                    log.problem(400, "Invalid request", format!("body: {detail}"))
                }
            };
            (resp, Bytes::new())
        }
    };
    let status = resp.status().as_u16();
    span.record("status", status);
    if log.streaming {
        return resp;
    }
    if let Some(source) = log.source {
        span.record("source", source.as_str());
    }
    let delay = log.delay;
    let latency = started.elapsed() + delay.unwrap_or_default();
    let request = RequestParts {
        method: &method,
        uri: &uri,
        headers: &headers,
        raw_body: &raw_body,
    };
    report(rt, request, log, status, latency)
        .instrument(span.clone())
        .await;
    if let Some(delay) = delay {
        tokio::time::sleep(delay).instrument(span).await;
    }
    resp
}

/// Why a request body was not read.
enum BodyError {
    /// Larger than the cap (in bytes).
    TooLarge(usize),
    Unreadable(String),
}

/// The body, unless its `Content-Length` or its collected length exceeds
/// `limit` bytes, or it cannot be read.
async fn read_body(headers: &HeaderMap, body: Body, limit: usize) -> Result<Bytes, BodyError> {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > limit) {
        return Err(BodyError::TooLarge(limit));
    }
    let mut stream = body.into_data_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| BodyError::Unreadable(e.to_string()))?;
        if buf.len() + chunk.len() > limit {
            return Err(BodyError::TooLarge(limit));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

async fn serve(
    st: &RestState,
    (method, uri, prefix): (&Method, &Uri, &str),
    headers: &HeaderMap,
    raw_body: &[u8],
    log: &mut CallLog,
) -> Response {
    let rt = &st.rt;
    let path = uri.path();

    // 1. Route, then 2. decode and validate, in the `mb.decode` span.
    let decode = tracing::info_span!("mb.decode").entered();
    let found = to_http_method(method).and_then(|m| st.routes.find(m, path));
    let Some((op_id, raw_params)) = found else {
        return unrouted(&st.routes, method, path, log);
    };
    tracing::Span::current().record("operation", op_id.as_str());
    log.operation_id = Some(op_id.clone());
    let Some(op) = rt.operation(&op_id) else {
        return log.problem(
            500,
            "Internal Server Error",
            format!("unknown operation {op_id}"),
        );
    };
    let validators = st.inputs.get(&op_id);
    // The body the request's media type selects is the one it is decoded,
    // validated and answered for.
    let variant = body_variant(
        &op.input.bodies,
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
    );
    let op = with_body(op, variant.map(|(_, b)| &b.schema));
    let op: &Operation = &op;
    let body_validator = validators.and_then(|v| match variant {
        Some((i, _)) => v.bodies.get(i).and_then(Option::as_ref),
        None => v.body.as_ref(),
    });
    log.declared_body_type = op
        .input
        .body
        .as_ref()
        .and_then(|s| s.get("x-content-type"))
        .and_then(Value::as_str)
        .map(str::to_string);

    // 2. Decode and validate.
    let mut errors: Vec<Violation> = Vec::new();
    let path_params = coerce_params(
        raw_params
            .into_iter()
            .map(|(k, v)| (k, vec![v.as_str().unwrap_or_default().to_string()])),
        op.input.path.as_ref(),
    );
    // Credential parameters (`api_key`, declared query API keys) are
    // checked by data-plane auth but never reach the engine.
    let raw_query =
        match serde_urlencoded::from_str::<Vec<(String, String)>>(uri.query().unwrap_or("")) {
            Ok(pairs) => pairs,
            Err(e) => {
                errors.push(Violation {
                    location: "query".into(),
                    pointer: String::new(),
                    reason: format!("query: {e}"),
                });
                Vec::new()
            }
        };
    if raw_query.len() > MAX_REQUEST_PARAMS {
        return too_many_params(log, "query pairs", raw_query.len());
    }
    let query = form::decode(
        public_query(rt, &raw_query),
        op.input.query.as_ref(),
        &rt.bundle.defs,
    );
    // The entered span is not held across the await.
    let decode_span = decode.exit();
    let decoded = decode_body(op, &rt.bundle.defs, headers, raw_body)
        .instrument(decode_span.clone())
        .await;
    let decode = decode_span.entered();
    let body = match decoded {
        Ok(body) => body,
        Err(BodyDecodeError::Invalid(e)) => {
            errors.push(Violation {
                location: "body".into(),
                pointer: String::new(),
                reason: e,
            });
            None
        }
        Err(BodyDecodeError::TooManyFields(n)) => {
            return too_many_params(log, "body fields", n);
        }
    };
    // The record shows the body as decoded, as the engine sees it.
    if log.record {
        log.request_body.clone_from(&body);
    }
    if let Some(v) = validators {
        check(
            &mut errors,
            ("path", false),
            v.path.as_ref(),
            Some(&Value::Object(path_params.clone())),
        );
        // A contract the smart import inferred guesses at its request schemas: its
        // body and query are not refused for what such a guess gets wrong.
        let lenient = rt.bundle.inferred && !rt.strict_requests;
        check(
            &mut errors,
            ("query", lenient),
            v.query.as_ref(),
            Some(&Value::Object(query.clone())),
        );
        // A body may send `null` for a member it need not send at all.
        if let (Some(v), Some(body)) = (body_validator, body.as_ref())
            && let Err(found) = v.validate_request_located(body, lenient)
        {
            errors.extend(found.into_iter().map(|(pointer, e)| Violation {
                location: "body".to_string(),
                pointer,
                reason: format!("body{e}"),
            }));
        }
    }
    if !errors.is_empty() {
        // The entered span is not held across the await.
        drop(decode);
        let presented = Presented {
            query: &raw_query,
            key_verified: false,
            admitted: None,
        };
        return invalid_input(rt, op, (headers, presented), &errors, log).await;
    }
    drop(decode);

    // 3. Admission, data-plane auth, overrides and scenario.
    log.admitted_or_rejected = true;
    let presented = Presented {
        query: &raw_query,
        key_verified: false,
        admitted: None,
    };
    // Built before the plan: a refused call's declared headers are drawn
    // from it as an answered one's are.
    let call = OperationCall {
        operation_id: op.id.clone(),
        path_params,
        query,
        headers: forwarded_headers(rt, headers),
        credentials: credential_headers(rt, headers),
        body,
    };

    let plan = match call::plan(rt, op, headers, presented).await {
        Ok(mut plan) => {
            plan.page_url = Some(format!(
                "{}{path}",
                crate::base_url::of(rt, headers, prefix)
            ));
            plan
        }
        Err(PlanFailure {
            error,
            tenancy,
            session,
        }) => {
            log.tenancy = tenancy;
            if let Some(session) = session {
                log.session = session;
            }
            return match error {
                PlanError::Rejected(r) => log.rejected(r),
                PlanError::RateLimited(r) => match call::declared_rate_limited(rt, op) {
                    Some(generated) => {
                        let mut resp = log.answered(rt, op, as_xml(rt, op, generated));
                        set_retry_after(&mut resp, r.retry_after);
                        add_headers(&mut resp, rt, op, None, &call);
                        resp
                    }
                    None => log.rejected(r),
                },
                PlanError::Unauthorized(u) => {
                    let mut resp = log.problem(401, "Unauthorized", u.detail);
                    if let Ok(v) = HeaderValue::from_str(&u.challenge) {
                        resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
                    }
                    resp
                }
                PlanError::BadOverride(detail) => log.problem(400, "Invalid override", detail),
                PlanError::UnknownScenario(name) => log.problem(
                    400,
                    "Unknown scenario",
                    format!("mock {} has no scenario {name:?}", rt.name),
                ),
            };
        }
    };
    log.tenancy = Some(plan.tenancy.clone());
    log.session = plan.session.session.clone();

    // An injected error the contract declares no status for: 503.
    if plan.native_chaos_error() {
        log.delay = plan.delay;
        let mut resp = log.problem(503, "Service Unavailable", crate::chaos::CHAOS_ERROR_DETAIL);
        mark_chaos(&mut resp);
        return resp;
    }

    // An injected error answered in the shape of the `default` response
    // (the only error the contract declares): faker-filled, never the LLM.
    if plan.chaos.inject_error
        && let Some(status) = plan.forced_status
        && !op
            .outputs
            .iter()
            .any(|o| o.status == StatusSelector::Code(status))
        && let Some(variant) = op
            .outputs
            .iter()
            .find(|o| o.status == StatusSelector::Default && o.schema.is_some())
    {
        log.delay = plan.delay;
        let reason = StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or("Error");
        let message = format!("{reason} ({})", crate::chaos::CHAOS_ERROR_DETAIL);
        let mut resp = log.answered(rt, op, filled_error(rt, op, variant, status, &message));
        mark_chaos(&mut resp);
        return resp;
    }

    // A stream, unless a non-2xx status is forced: that is answered once.
    if op.kind == OperationKind::StreamServer
        && plan
            .forced_status
            .is_none_or(|code| (200..300).contains(&code))
    {
        return stream(rt, plan, call, headers, log);
    }

    // 4. State read, generate, state write.
    let generated = match call::generate_once(rt, &plan, &call).await {
        Ok(g) => g,
        // An undeclared server error is answered natively, as every other
        // protocol does: any API can fail with a 5xx whatever its contract.
        Err(EngineError::UndeclaredStatus(code)) if (500..600).contains(&code) => {
            log.delay = plan.delay;
            let title = axum::http::StatusCode::from_u16(code)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("Server Error");
            let mut resp = log.problem(
                code,
                title,
                format!(
                    "operation {} declares no response for forced status {code}",
                    op.id
                ),
            );
            add_headers(&mut resp, rt, op, plan.scenario, &call);
            return resp;
        }
        Err(EngineError::UndeclaredStatus(code)) => {
            return log.problem(
                400,
                "Undeclared status override",
                format!("operation {} declares no response for status {code}", op.id),
            );
        }
        Err(e) => return log.problem(500, "Generation failed", e.to_string()),
    };

    // 5. Respond (the handler sleeps `delay` after recording).
    log.delay = plan.delay;
    let entity_tag = call::entity_tag(rt, &plan, &call, &generated).await;
    let next_range = call::next_range(rt, &plan, &call, &generated);
    let settled = generated.headers.clone();
    let mut resp = tracing::info_span!("mb.encode")
        .in_scope(|| log.answered(rt, op, as_xml(rt, op, generated)));
    // Headers the call settled (a created record's id) stand over the declared ones'
    // faker values; a scenario's still replace them.
    for (name, value) in settled {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            resp.headers_mut().insert(name, value);
        }
    }
    add_headers(&mut resp, rt, op, plan.scenario, &call);
    // The record's concurrency token, and a ranged page's next range, over the faker's
    // values (a scenario's stand).
    for (name, value) in [entity_tag, next_range].into_iter().flatten() {
        if plan.scenario.is_none()
            && let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            )
        {
            resp.headers_mut().insert(name, value);
        }
    }
    if plan.chaos.inject_error {
        mark_chaos(&mut resp);
    }
    resp
}

/// The headers the contract declares for the response's status (each
/// unless already set, as a rate limiter's `Retry-After` is; drawn from the
/// engine's faker seed and `call` when there is a seed), then the
/// scenario's, which replace them.
fn add_headers(
    resp: &mut Response,
    rt: &MockRuntime,
    op: &Operation,
    scenario: Option<&ScenarioRuntime>,
    call: &OperationCall,
) {
    let status = resp.status().as_u16();
    let headers = resp.headers_mut();
    for (name, value) in rt.engine.declared_headers(&rt.bundle, op, status, call) {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) && !headers.contains_key(&name)
        {
            headers.insert(name, value);
        }
    }
    for (name, value) in scenario.map(|s| s.headers.as_slice()).unwrap_or_default() {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
}

/// Marks a response as an error injected by chaos.
fn mark_chaos(resp: &mut Response) {
    resp.headers_mut()
        .insert(CHAOS_HEADER, HeaderValue::from_static("error"));
}

/// The event stream answering a `StreamServer` call.
#[cfg(feature = "sse")]
fn stream(
    rt: &Arc<MockRuntime>,
    plan: call::CallPlan<'_>,
    call: OperationCall,
    headers: &HeaderMap,
    log: &mut CallLog,
) -> Response {
    log.streaming = true;
    super::sse::sse_response(Arc::clone(rt), plan, call, headers.clone())
}

/// Without the `sse` feature, streams are not served.
#[cfg(not(feature = "sse"))]
fn stream(
    _rt: &Arc<MockRuntime>,
    plan: call::CallPlan<'_>,
    _call: OperationCall,
    _headers: &HeaderMap,
    log: &mut CallLog,
) -> Response {
    log.problem(
        501,
        "Streaming not enabled",
        format!(
            "operation {} streams its response, and this build has no sse feature",
            plan.op.id
        ),
    )
}

/// The parts of the HTTP request that [`report`] records.
struct RequestParts<'a> {
    method: &'a Method,
    uri: &'a Uri,
    headers: &'a HeaderMap,
    raw_body: &'a [u8],
}

/// Report a served call to the usage sink and the request recorder.
async fn report(
    rt: &MockRuntime,
    request: RequestParts<'_>,
    log: CallLog,
    status: u16,
    latency: Duration,
) {
    let RequestParts {
        method,
        uri,
        headers,
        raw_body,
    } = request;
    // The record's request is built only when a recorder will receive it.
    let request = if rt.recorder.is_some() {
        let media = media_type(headers).or(log.declared_body_type);
        let mut request = json!({
            "query": recorded_query(rt, uri),
            "headers": recorded_headers(rt, headers),
            "body": request_body_record(raw_body, media.as_deref(), log.request_body),
        });
        if !log.validation.is_empty() {
            request["validation"] = json!(log.validation);
        }
        if !log.effects.is_null() {
            request["effects"] = log.effects;
        }
        for (name, note) in log.notes {
            request[name] = note;
        }
        request
    } else {
        Value::Null
    };
    let r = Report {
        operation_id: log.operation_id,
        session: log.session,
        method: method.as_str().to_string(),
        path: uri.path().to_string(),
        status,
        source: log.source,
        fallback_reason: log.fallback_reason,
        usage: log.usage,
        latency,
        tenancy: log.tenancy,
        admission_ran: log.admitted_or_rejected,
        request,
        request_body_bytes: Some(raw_body.len()),
        response: log.response,
    };
    call::report(rt, headers, r).await;
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

pub(super) fn method_name(m: HttpMethod) -> &'static str {
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

/// Group repeated query keys, keeping first-seen order (linear: keys are
/// found through an index, not by scanning the groups so far).
fn group(pairs: impl IntoIterator<Item = (String, String)>) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (k, v) in pairs {
        match index.get(&k).and_then(|&i| out.get_mut(i)) {
            Some((_, values)) => values.push(v),
            None => {
                index.insert(k.clone(), out.len());
                out.push((k, vec![v]));
            }
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
    form::coerce(raw, &schema.map(types).unwrap_or_default())
}

/// How a request body is decoded.
#[derive(Clone, Copy, PartialEq)]
enum BodyKind {
    Json,
    Form,
    Multipart,
}

impl BodyKind {
    fn of(media_type: &str) -> Option<Self> {
        match media_type {
            FORM_CONTENT_TYPE => Some(Self::Form),
            MULTIPART_CONTENT_TYPE => Some(Self::Multipart),
            _ => None,
        }
    }
}

/// The request body as JSON, by the request's form or multipart
/// `Content-Type`, else the media type the contract declares
/// (`x-content-type`), else as JSON. Form and multipart fields are nested
/// by bracket notation and coerced through the body schema. A body that is
/// not JSON, sent to an operation declaring no body, is passed on as text:
/// a non-empty body never reaches the engine as null. A missing body is an
/// error only when the contract marks it required.
async fn decode_body(
    op: &Operation,
    defs: &Map<String, Value>,
    headers: &HeaderMap,
    raw: &[u8],
) -> Result<Option<Value>, BodyDecodeError> {
    let schema = op.input.body.as_ref();
    let declared = schema
        .and_then(|s| s.get("x-content-type"))
        .and_then(Value::as_str)
        .and_then(BodyKind::of);
    let kind = media_type(headers)
        .as_deref()
        .and_then(BodyKind::of)
        .or(declared)
        .unwrap_or(BodyKind::Json);
    if raw.iter().all(u8::is_ascii_whitespace) {
        return if op.input.body_required {
            Err(BodyDecodeError::Invalid(
                "body: a request body is required".into(),
            ))
        } else {
            Ok(None)
        };
    }
    let fields = match kind {
        BodyKind::Form => form_pairs(raw).map_err(|e| format!("body: not valid form data: {e}")),
        BodyKind::Multipart => multipart_fields(headers, raw)
            .await
            .map_err(|e| format!("body: not valid multipart data: {e}")),
        BodyKind::Json => {
            return match serde_json::from_slice(raw) {
                Ok(v) => Ok(Some(v)),
                Err(e) if schema.is_some() => Err(BodyDecodeError::Invalid(format!(
                    "body: not valid JSON: {e}"
                ))),
                Err(_) => Ok(Some(Value::String(
                    String::from_utf8_lossy(raw).into_owned(),
                ))),
            };
        }
    }
    .map_err(BodyDecodeError::Invalid)?;
    if fields.len() > MAX_REQUEST_PARAMS {
        return Err(BodyDecodeError::TooManyFields(fields.len()));
    }
    Ok(Some(Value::Object(form::decode(fields, schema, defs))))
}

/// Why a request body did not decode.
enum BodyDecodeError {
    /// Not valid for its media type, or missing though required.
    Invalid(String),
    /// A form or multipart body with more than [`MAX_REQUEST_PARAMS`]
    /// fields (counted up to one past the cap).
    TooManyFields(usize),
}

/// The 400 problem for a request carrying more than
/// [`MAX_REQUEST_PARAMS`] query pairs or body fields.
fn too_many_params(log: &mut CallLog, what: &str, n: usize) -> Response {
    log.problem(
        400,
        "Too many parameters",
        format!("the request has {n} {what}; at most {MAX_REQUEST_PARAMS} are accepted"),
    )
}

/// The fields of a `multipart/form-data` body: a file part as its file
/// name, any other part as its text.
async fn multipart_fields(
    headers: &HeaderMap,
    raw: &[u8],
) -> Result<Vec<(String, String)>, multer::Error> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let boundary = multer::parse_boundary(content_type)?;
    let data = Bytes::copy_from_slice(raw);
    let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(data) });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let mut fields = Vec::new();
    while let Some(field) = multipart.next_field().await? {
        let Some(name) = field.name().map(str::to_string) else {
            continue;
        };
        let value = match field.file_name() {
            Some(file) => file.to_string(),
            None => field.text().await?,
        };
        fields.push((name, value));
        // One past the cap is enough to refuse the body.
        if fields.len() > MAX_REQUEST_PARAMS {
            break;
        }
    }
    Ok(fields)
}

fn check(
    errors: &mut Vec<Violation>,
    (location, lenient): (&str, bool),
    validator: Option<&SchemaValidator>,
    value: Option<&Value>,
) {
    if let (Some(v), Some(value)) = (validator, value)
        && let Err(found) = if lenient {
            v.validate_lenient_located(value)
        } else {
            v.validate_located(value)
        }
    {
        errors.extend(found.into_iter().map(|(pointer, e)| Violation {
            location: location.to_string(),
            pointer,
            reason: format!("{location}{e}"),
        }));
    }
}

/// The contract's 400, 422 or `default` error shape if it declares one,
/// else problem+json 400. The reasons go into the error shape's message-like
/// members and the violated fields into its path-like ones (see
/// [`mb_engine::error_body`]), and the reasons into the [`VALIDATION_HEADER`] and the
/// request record. A code-like member of the shape is written by the LLM the provider
/// policy picks for the caller, once per operation and status, when the caller's
/// credentials hold; else the faker's.
async fn invalid_input(
    rt: &MockRuntime,
    op: &Operation,
    (headers, presented): (&HeaderMap, Presented<'_>),
    violations: &[Violation],
    log: &mut CallLog,
) -> Response {
    let errors: Vec<String> = violations.iter().map(|v| v.reason.clone()).collect();
    log.validation.clone_from(&errors);
    let reasons = errors.join("; ");
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
    let mut resp = match declared {
        Some(variant) => {
            let status = match variant.status {
                StatusSelector::Code(c) => c,
                StatusSelector::Default => 400,
            };
            let writes = variant
                .schema
                .as_ref()
                .is_some_and(|s| error_body::writes_code(s, &rt.bundle.defs));
            let engine = match writes {
                true => call::refusing_engine(rt, op, headers, presented, status).await,
                false => None,
            };
            let base = match &engine {
                Some(engine) => {
                    engine
                        .error_answer(
                            &rt.bundle,
                            op,
                            &OperationCall {
                                operation_id: op.id.clone(),
                                ..OperationCall::default()
                            },
                            status,
                            error_body::ErrorCause::Invalid,
                        )
                        .await
                }
                None => None,
            };
            let base = base.unwrap_or_else(|| {
                rt.engine.fallback(
                    &rt.bundle,
                    op,
                    variant,
                    status,
                    &OperationCall::default(),
                    &GenerationContext::default(),
                )
            });
            let mut rejected = with_facts(rt, op, base, &reasons, &errors, violations);
            rejected.source = Source::Rejected;
            log.answered(rt, op, rejected)
        }
        None => log.problem(400, "Invalid request", reasons.clone()),
    };
    // A client error, not a generation that fell back.
    log.source = Some(Source::Rejected);
    if let Ok(v) = HeaderValue::from_str(&header_text(&reasons, MAX_VALIDATION_HEADER)) {
        resp.headers_mut().insert(VALIDATION_HEADER, v);
    }
    resp
}

/// The contract's error shape `variant`, faker-filled, its message-like
/// field set to `message`. Never calls the LLM.
fn filled_error(
    rt: &MockRuntime,
    op: &Operation,
    variant: &OutputVariant,
    status: u16,
    message: &str,
) -> GeneratedResponse {
    let generated = rt.engine.fallback(
        &rt.bundle,
        op,
        variant,
        status,
        &OperationCall::default(),
        &GenerationContext::default(),
    );
    with_facts(rt, op, generated, message, &[message.to_string()], &[])
}

/// `generated`, an error body the mock made itself, holding the call's facts: its
/// message-like member says `message`, arrays of messages hold `reasons`, and its
/// path-like members name the fields of `violations` (see [`mb_engine::error_body`]),
/// and a member that holds the HTTP status says `generated`'s (see
/// [`mb_engine::Engine::with_own_status`]), whatever a model wrote there. Rendered as
/// XML when declared so.
fn with_facts(
    rt: &MockRuntime,
    op: &Operation,
    mut generated: GeneratedResponse,
    message: &str,
    reasons: &[String],
    violations: &[Violation],
) -> GeneratedResponse {
    let schema = op
        .output_for_status(generated.status)
        .and_then(|v| v.schema.as_ref());
    if let (Some(body), Some(schema)) = (generated.body.take(), schema) {
        let mut set = body.clone();
        error_body::set_message(&mut set, schema, &rt.bundle.defs, message);
        let body = match rt.engine.error_validator(&rt.bundle, op, generated.status) {
            Some(validator) => {
                let base = if validator.is_valid(&set) || !validator.is_valid(&body) {
                    set
                } else {
                    body
                };
                error_body::with_reasons(&validator, base, reasons, violations)
            }
            None => set,
        };
        generated.body = Some(body);
    }
    let generated = rt.engine.with_own_status(&rt.bundle, op, generated);
    as_xml(rt, op, generated)
}

/// A body generated as JSON for a response declared as XML
/// (`application/xml`, `text/xml`, `+xml`), rendered as that XML by the
/// SOAP mapper. The root element is the `x-xml-root` of the variant's
/// schema, else its `xml.name`, else the name of the definition it
/// references, else `response`. A string body (a declared example) is
/// served as written; a body the mapper cannot render stays JSON.
#[cfg(feature = "soap")]
fn as_xml(rt: &MockRuntime, op: &Operation, mut generated: GeneratedResponse) -> GeneratedResponse {
    let xml = generated.content_type.as_deref().is_some_and(is_xml_media);
    let (Some(body), Some(schema)) = (
        generated.body.as_ref().filter(|b| xml && !b.is_string()),
        op.output_for_status(generated.status)
            .and_then(|v| v.schema.as_ref()),
    ) else {
        return generated;
    };
    let mut with_defs = rt.bundle.with_reachable_defs(schema);
    if let Some(obj) = with_defs.as_object_mut()
        && !obj.contains_key("x-xml-root")
    {
        let mut root = Map::new();
        root.insert("name".into(), json!(xml_root_name(schema)));
        if let Some(ns) = schema.pointer("/xml/namespace") {
            root.insert("namespace".into(), ns.clone());
        }
        obj.insert("x-xml-root".into(), Value::Object(root));
    }
    match crate::soap::json_to_xml(body, &with_defs) {
        Ok(rendered) => generated.body = Some(Value::String(rendered)),
        Err(e) => tracing::warn!(operation = %op.id, error = %e, "XML response not rendered"),
    }
    generated
}

/// Without the SOAP XML mapper an XML response is served as JSON.
#[cfg(not(feature = "soap"))]
fn as_xml(_: &MockRuntime, _: &Operation, generated: GeneratedResponse) -> GeneratedResponse {
    generated
}

/// The root element name of an XML response with body schema `schema`.
#[cfg(feature = "soap")]
fn xml_root_name(schema: &Value) -> &str {
    schema
        .pointer("/xml/name")
        .and_then(Value::as_str)
        .or_else(|| {
            schema
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.rsplit('/').next())
        })
        .filter(|name| {
            name.chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
        })
        .unwrap_or("response")
}

/// `text` as a header value: visible ASCII only, at most `max` bytes.
fn header_text(text: &str, max: usize) -> String {
    text.chars()
        .map(|c| {
            if c == ' ' || c.is_ascii_graphic() {
                c
            } else {
                '?'
            }
        })
        .take(max)
        .collect()
}

/// The raw query as recorded (without credential parameters, the contract's API keys
/// included): repeated keys become arrays of strings.
fn recorded_query(rt: &MockRuntime, uri: &Uri) -> Value {
    let pairs = serde_urlencoded::from_str::<Vec<(String, String)>>(uri.query().unwrap_or(""))
        .unwrap_or_default();
    Value::Object(string_params(call::recorded_public_query(rt, &pairs)))
}

/// `pairs` as an object of strings; repeated keys become arrays of strings.
/// Only the first [`MAX_REQUEST_PARAMS`] pairs are kept.
fn string_params(pairs: Vec<(String, String)>) -> Map<String, Value> {
    group(pairs.into_iter().take(MAX_REQUEST_PARAMS))
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

/// `op` with `schema` as its request body, when it is not that already.
fn with_body<'a>(op: &'a Operation, schema: Option<&Value>) -> Cow<'a, Operation> {
    match schema {
        Some(schema) if op.input.body.as_ref() != Some(schema) => {
            let mut op = op.clone();
            op.input.body = Some(schema.clone());
            Cow::Owned(op)
        }
        _ => Cow::Borrowed(op),
    }
}

/// The declared body (and its index) a request sent as `content_type`
/// selects: among the entries of the same base type, one whose every
/// declared parameter the request carries with an equal value, preferring
/// the most parameters (so a bare entry is the fallback). Type and parameter
/// names match whatever their case; `charset` is ignored on both sides.
/// `None` when no entry matches: the operation's default body applies.
fn body_variant<'a>(
    variants: &'a [BodyVariant],
    content_type: Option<&str>,
) -> Option<(usize, &'a BodyVariant)> {
    let (base, params) = media_parts(content_type?);
    let mut best: Option<(usize, usize, &BodyVariant)> = None;
    for (i, v) in variants.iter().enumerate() {
        let (declared_base, declared) = media_parts(&v.media_type);
        let carried = declared.iter().all(|(name, value)| {
            params
                .iter()
                .any(|(n, val)| n.eq_ignore_ascii_case(name) && val == value)
        });
        if !declared_base.eq_ignore_ascii_case(base) || !carried {
            continue;
        }
        if best.is_none_or(|(score, _, _)| declared.len() > score) {
            best = Some((declared.len(), i, v));
        }
    }
    best.map(|(_, i, v)| (i, v))
}

/// A media type's base type and its parameters other than `charset`, each
/// trimmed, a quoted value unquoted.
fn media_parts(ct: &str) -> (&str, Vec<(&str, &str)>) {
    let mut parts = ct.split(';');
    let base = parts.next().unwrap_or_default().trim();
    let params = parts
        .filter_map(|p| {
            let (name, value) = p.split_once('=')?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            Some((name.trim(), value))
        })
        .filter(|(name, _)| !name.eq_ignore_ascii_case("charset"))
        .collect();
    (base, params)
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

/// The request body for the record: the body `serve` already decoded
/// (`decoded`) unless it is multipart, else [`recorded_body`] of the raw
/// bytes.
fn request_body_record(raw: &[u8], media_type: Option<&str>, decoded: Option<Value>) -> Value {
    match decoded {
        Some(body) if media_type != Some(MULTIPART_CONTENT_TYPE) => body,
        _ => recorded_body(raw, media_type),
    }
}

fn respond(generated: &GeneratedResponse) -> Response {
    let Ok(status) = StatusCode::from_u16(generated.status) else {
        return problem(
            500,
            "Internal Server Error",
            format!("invalid status {}", generated.status),
        );
    };
    // A string body of a non-JSON media type (a declared example, faked
    // text, rendered XML) is served as written, labelled as declared.
    let raw = generated
        .content_type
        .as_deref()
        .filter(|ct| !is_json_media(ct))
        .zip(generated.body.as_ref().and_then(Value::as_str));
    let body = match (&generated.body, raw) {
        (_, Some((_, text))) => Body::from(text.to_owned()),
        (Some(v), None) => match serde_json::to_vec(v) {
            Ok(bytes) => Body::from(bytes),
            Err(e) => return problem(500, "Internal Server Error", e.to_string()),
        },
        (None, None) => Body::empty(),
    };
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let headers = resp.headers_mut();
    if generated.body.is_some() {
        let declared = generated
            .content_type
            .as_deref()
            .filter(|ct| is_json_media(ct) || raw.is_some())
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
    if let Some(reason) = generated.fallback_reason.as_deref() {
        headers.insert(FALLBACK_REASON_HEADER, call::reason_header(reason));
    }
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

/// `application/xml`, `text/xml` or any `+xml` media type (parameters
/// allowed).
#[cfg(feature = "soap")]
fn is_xml_media(ct: &str) -> bool {
    let base = ct
        .split(';')
        .next()
        .unwrap_or(ct)
        .trim()
        .to_ascii_lowercase();
    base == "application/xml" || base == "text/xml" || base.ends_with("+xml")
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

    /// Grouping 50k pairs (distinct keys, then one repeated key) is
    /// linear, not O(k^2), and keeps first-seen order.
    #[test]
    fn group_is_linear_and_keeps_first_seen_order() {
        const N: usize = 50_000;
        let distinct: Vec<_> = (0..N).map(|i| (format!("k{i}"), "v".to_string())).collect();
        let started = std::time::Instant::now();
        let out = group(distinct);
        let took = started.elapsed();
        assert_eq!(out.len(), N);
        assert_eq!(out[N - 1].0, format!("k{}", N - 1));
        assert!(took < std::time::Duration::from_secs(1), "took {took:?}");

        let out = group(vec![
            ("b".into(), "1".into()),
            ("a".into(), "2".into()),
            ("b".into(), "3".into()),
        ]);
        assert_eq!(
            out,
            vec![
                ("b".to_string(), vec!["1".to_string(), "3".to_string()]),
                ("a".to_string(), vec!["2".to_string()]),
            ]
        );
    }

    fn generated() -> GeneratedResponse {
        GeneratedResponse {
            status: 200,
            content_type: None,
            body: Some(json!({"id": "o1"})),
            source: mb_engine::Source::Fallback,
            usage: Default::default(),
            fallback_reason: None,
            effects: Vec::new(),
            effect_outcomes: Vec::new(),
            dropped_effects: 0,
            new_record: false,
            patched: None,
            headers: Vec::new(),
            world_gaps: Vec::new(),
            exec_notes: serde_json::Map::new(),
        }
    }

    /// Without a recorder the response body is not kept for the record.
    #[test]
    fn call_log_keeps_bodies_only_for_a_recorder() {
        let mut log = CallLog::new(false);
        let resp = log.generated(generated());
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(log.response.is_none());
        log.problem(404, "Not Found", "nope");
        assert!(log.response.is_none());

        let mut log = CallLog::new(true);
        log.generated(generated());
        assert_eq!(log.response, Some(json!({"id": "o1"})));
    }

    /// The record reuses the body `serve` decoded, but not a multipart
    /// one, and decodes anything not decoded from the raw bytes.
    #[test]
    fn recorded_body_reuses_decoded_json() {
        let decoded = Some(json!({"reused": true}));
        assert_eq!(
            request_body_record(br#"{"raw":1}"#, Some("application/json"), decoded.clone()),
            json!({"reused": true})
        );
        assert_eq!(
            request_body_record(b"a[b]=1", Some(FORM_CONTENT_TYPE), decoded.clone()),
            json!({"reused": true})
        );
        assert_eq!(
            request_body_record(b"a=1", Some(FORM_CONTENT_TYPE), None),
            json!({"a": "1"})
        );
        assert_eq!(
            request_body_record(b"x", Some(MULTIPART_CONTENT_TYPE), decoded),
            json!(MULTIPART_OMITTED)
        );
        assert_eq!(
            request_body_record(br#"{"raw":1}"#, None, None),
            json!({"raw": 1})
        );
    }
}
