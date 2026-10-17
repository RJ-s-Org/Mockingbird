//! SOAP adapter: serves a bundle's WSDL operations as SOAP 1.1 and 1.2
//! endpoints, one per distinct `address_path`.
//!
//! A `POST` is dispatched by its action (`SOAPAction` for 1.1, the
//! `action` content-type parameter for 1.2), else by the QName of the
//! Body's first child, decoded to JSON (see `xml_json`), validated and
//! run through the shared `call` pipeline. Responses use
//! the request's SOAP version; every failure is a SOAP Fault. `GET ?wsdl`
//! serves the stored WSDL with its addresses pointing at this mock.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use mb_engine::{EngineError, GeneratedResponse, OperationCall, SchemaValidator, Source};
use mb_ext::Rejection;
use mb_mcm::{Binding, Operation, ProtocolPayload, StatusSelector};
use serde_json::{Value, json};
use tokio::time::Instant;
use tracing::Instrument;
use tracing::field::Empty;

use crate::call::{
    self, CallPlan, DEFAULT_SESSION, PlanError, PlanFailure, Presented, Report, forwarded_headers,
    recorded_headers,
};
use crate::dp_auth::authenticate_contract;
use crate::{MockRuntime, MountPrefix};

mod envelope;
mod xml_json;

use envelope::{Element, Version};
pub use xml_json::envelope_body_to_json;
pub(crate) use xml_json::json_to_xml;

const SOURCE_HEADER: HeaderName = HeaderName::from_static("x-mockingbird-source");
const CHAOS_HEADER: HeaderName = HeaderName::from_static(crate::CHAOS_HEADER);
const SOAP_ACTION_HEADER: &str = "soapaction";
const PROBLEM_JSON: &str = "application/problem+json";
/// The request record's body when the envelope could not be decoded.
const UNDECODABLE: &str = "[undecodable]";

/// Why a bundle cannot be served over SOAP.
#[derive(Debug, thiserror::Error)]
pub enum SoapError {
    #[error("bundle has no WSDL")]
    MissingWsdl,
    #[error("contract has no SOAP operations")]
    NoOperations,
}

/// One SOAP operation, with its decoding schema and input validator.
struct Endpoint {
    /// Index into `bundle.operations`.
    op: usize,
    soap_action: Option<String>,
    /// The input body's `x-xml-root` as `(namespace, name)`.
    root: Option<(String, String)>,
    /// The input body schema with its reachable `$defs`.
    input: Option<Value>,
    validator: Option<SchemaValidator>,
}

struct SoapState {
    rt: Arc<MockRuntime>,
    wsdl: String,
    /// `address_path` → the operations served there.
    endpoints: HashMap<String, Vec<Endpoint>>,
    /// Service name → its `address_path`, for the `?wsdl` rewrite.
    services: HashMap<String, String>,
}

/// A router serving every SOAP operation of `rt.bundle` at its
/// `address_path` (`POST`), plus the WSDL at `GET <address_path>?wsdl`.
pub fn try_router(rt: Arc<MockRuntime>) -> Result<Router, SoapError> {
    let Some(ProtocolPayload::Soap { wsdl }) = &rt.bundle.protocol_payload else {
        return Err(SoapError::MissingWsdl);
    };
    let wsdl = wsdl.clone();
    let mut endpoints: HashMap<String, Vec<Endpoint>> = HashMap::new();
    let mut services: BTreeMap<String, String> = BTreeMap::new();
    for (i, op) in rt.bundle.operations.iter().enumerate() {
        let Binding::Soap {
            service,
            soap_action,
            address_path,
            ..
        } = &op.binding
        else {
            continue;
        };
        services
            .entry(service.clone())
            .or_insert_with(|| address_path.clone());
        let input = op
            .input
            .body
            .as_ref()
            .map(|s| rt.bundle.with_reachable_defs(s));
        let validator = input.as_ref().and_then(|s| match SchemaValidator::new(s) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(operation = %op.id, error = %e, "input schema skipped");
                None
            }
        });
        endpoints
            .entry(address_path.clone())
            .or_default()
            .push(Endpoint {
                op: i,
                soap_action: soap_action.clone(),
                root: op.input.body.as_ref().and_then(xml_root),
                input,
                validator,
            });
    }
    if endpoints.is_empty() {
        return Err(SoapError::NoOperations);
    }
    let state = Arc::new(SoapState {
        rt,
        wsdl,
        endpoints,
        services: services.into_iter().collect(),
    });
    Ok(Router::new().fallback(handle).with_state(state))
}

/// A schema's `x-xml-root` as `(namespace, name)`.
fn xml_root(schema: &Value) -> Option<(String, String)> {
    let root = schema.get("x-xml-root")?;
    let name = root.get("name")?.as_str()?.to_string();
    let ns = root
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some((ns, name))
}

async fn handle(State(st): State<Arc<SoapState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let Some(endpoints) = st.endpoints.get(&path) else {
        return problem(404, "Not Found", format!("no SOAP endpoint at {path}"));
    };
    match parts.method {
        Method::POST => {
            let span = tracing::info_span!(
                "mock.request",
                mock = %st.rt.name,
                operation = Empty,
                session = Empty,
                status = Empty,
                source = Empty,
            );
            serve_call(&st, endpoints, path, parts.headers, body)
                .instrument(span)
                .await
        }
        Method::GET if is_wsdl_query(parts.uri.query()) => {
            // The WSDL describes every operation: it needs the same
            // credentials as calling them (HTTP headers only).
            if let Err(u) = authenticate_contract(&st.rt, &parts.headers, &[]).await {
                let mut resp = problem(401, "Unauthorized", u.detail);
                if let Ok(v) = HeaderValue::from_str(&u.challenge) {
                    resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
                }
                return resp;
            }
            let prefix = parts
                .extensions
                .get::<MountPrefix>()
                .map_or("", |p| p.0.as_str());
            serve_wsdl(&st, &wsdl_base(&st.rt, &parts.headers, prefix))
        }
        _ => {
            let mut resp = problem(
                405,
                "Method Not Allowed",
                format!(
                    "{} is not supported; POST a SOAP envelope or GET ?wsdl",
                    parts.method
                ),
            );
            resp.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, POST"));
            resp
        }
    }
}

/// The base URL the `?wsdl` addresses point at: scheme, host and mount
/// prefix.
///
/// The scheme is `https` when Mockingbird terminates TLS; else, when
/// forwarded headers are trusted, the first `X-Forwarded-Proto` value or
/// the first RFC 7239 `Forwarded: proto=`; else `http`. The host is, when
/// forwarded headers are trusted, the first `X-Forwarded-Host` value or
/// the first `Forwarded: host=`; else `Host`. Unusable values (a scheme
/// other than `http`/`https`, a host with `/`, whitespace or other
/// characters a host cannot carry) are ignored.
fn wsdl_base(rt: &MockRuntime, headers: &HeaderMap, prefix: &str) -> String {
    let trusted = rt.protocols.trust_forwarded_headers;
    let scheme = if rt.protocols.tls_terminated {
        "https".to_string()
    } else {
        trusted
            .then(|| {
                forwarded_value(headers, "x-forwarded-proto")
                    .or_else(|| first_forwarded(headers, "proto"))
            })
            .flatten()
            .map(|p| p.to_ascii_lowercase())
            .filter(|p| p == "http" || p == "https")
            .unwrap_or_else(|| "http".into())
    };
    let forwarded_host = trusted
        .then(|| {
            forwarded_value(headers, "x-forwarded-host")
                .or_else(|| first_forwarded(headers, "host"))
        })
        .flatten()
        .filter(|h| is_host(h));
    let host = forwarded_host
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .filter(|h| is_host(h))
        })
        .unwrap_or_else(|| "localhost".into());
    format!("{scheme}://{host}{prefix}")
}

/// The first comma-separated value of header `name` (`X-Forwarded-*`),
/// trimmed; `None` when absent or empty.
fn forwarded_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?;
    let first = value.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_string())
}

/// Parameter `param` (`proto` or `host`) of the first element of the RFC
/// 7239 `Forwarded` header that sets it, unquoted.
fn first_forwarded(headers: &HeaderMap, param: &str) -> Option<String> {
    let value = headers.get(header::FORWARDED)?.to_str().ok()?;
    value
        .split(',')
        .flat_map(|element| element.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            if !name.trim().eq_ignore_ascii_case(param) {
                return None;
            }
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            (!value.is_empty()).then(|| value.to_string())
        })
}

/// Whether `host` can stand as a URL authority: a hostname or IP literal
/// with an optional port, and nothing else (no `/`, whitespace, `@`, …).
fn is_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']')
        })
}

fn is_wsdl_query(query: Option<&str>) -> bool {
    query.is_some_and(|q| q.split('&').any(|p| p == "wsdl" || p == "WSDL"))
}

fn serve_wsdl(st: &SoapState, base: &str) -> Response {
    match envelope::rewrite_wsdl(&st.wsdl, base, &st.services) {
        Ok(text) => {
            let mut resp = (StatusCode::OK, text).into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(Version::V11.content_type()),
            );
            resp
        }
        Err(e) => problem(500, "Internal Server Error", format!("stored WSDL: {e}")),
    }
}

/// How a call ends: a response envelope (or fault) with what to report.
struct Answer {
    /// HTTP status of the response.
    http: u16,
    /// Status reported to usage and the request log (the fault's
    /// semantic status; the HTTP status for a success).
    status: u16,
    version: Version,
    xml: String,
    source: Option<Source>,
    /// Recorded response.
    record: Value,
    retry_after: Option<Duration>,
    /// The `WWW-Authenticate` challenge of a data-plane auth failure.
    www_authenticate: Option<String>,
    /// The answer is an error injected by chaos.
    chaos: bool,
}

impl Answer {
    /// A fault for semantic `status` (4xx: Client/Sender, else
    /// Server/Receiver). 1.1 faults are HTTP 500; 1.2 Sender faults 400.
    fn fault(version: Version, status: u16, message: impl Into<String>) -> Self {
        let message = message.into();
        let client = (400..500).contains(&status);
        let http = match version {
            Version::V11 => 500,
            Version::V12 if client => 400,
            Version::V12 => 500,
        };
        Answer {
            http,
            status,
            version,
            xml: envelope::fault(version, client, &message, None),
            source: None,
            record: json!({ "fault": { "client": client, "message": message } }),
            retry_after: None,
            www_authenticate: None,
            chaos: false,
        }
    }

    fn with_http(mut self, http: u16) -> Self {
        self.http = http;
        self
    }
}

async fn serve_call(
    st: &SoapState,
    endpoints: &[Endpoint],
    path: String,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let rt = &st.rt;
    let started = Instant::now();
    let mut report = Report {
        operation_id: None,
        session: DEFAULT_SESSION.into(),
        method: "POST".into(),
        path,
        status: 200,
        source: None,
        usage: Default::default(),
        latency: Default::default(),
        tenancy: None,
        admission_ran: false,
        request: Value::Null,
        request_body_bytes: None,
        response: None,
    };
    let mut decoded: Option<Value> = None;
    let mut delay = None;
    let answer = answer(
        st,
        endpoints,
        &headers,
        body,
        &mut report,
        &mut decoded,
        &mut delay,
    )
    .await;

    if let Some(d) = delay {
        tokio::time::sleep(d).await;
    }
    let span = tracing::Span::current();
    span.record("status", answer.status);
    if let Some(source) = answer.source {
        span.record("source", source.as_str());
    }
    report.status = answer.status;
    report.source = answer.source;
    report.latency = started.elapsed();
    if rt.recorder.is_some() {
        report.request = json!({
            "headers": recorded_headers(rt, &headers),
            "body": decoded.unwrap_or_else(|| Value::String(UNDECODABLE.into())),
        });
        report.response = Some(answer.record);
    }
    call::report(rt, &headers, report).await;

    let code = StatusCode::from_u16(answer.http).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut resp = (code, answer.xml).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(answer.version.content_type()),
    );
    if let Some(source) = answer.source {
        h.insert(SOURCE_HEADER, HeaderValue::from_static(source.as_str()));
    }
    if answer.chaos {
        h.insert(CHAOS_HEADER, HeaderValue::from_static("error"));
    }
    if let Some(after) = answer.retry_after {
        let secs = after.as_secs() + u64::from(after.subsec_nanos() > 0);
        h.insert(header::RETRY_AFTER, HeaderValue::from(secs));
    }
    if let Some(v) = answer
        .www_authenticate
        .as_deref()
        .and_then(|c| HeaderValue::from_str(c).ok())
    {
        h.insert(header::WWW_AUTHENTICATE, v);
    }
    resp
}

/// Everything between reading the request and writing the response.
async fn answer(
    st: &SoapState,
    endpoints: &[Endpoint],
    headers: &HeaderMap,
    body: Body,
    report: &mut Report,
    decoded: &mut Option<Value>,
    delay: &mut Option<Duration>,
) -> Answer {
    let rt = &st.rt;
    let Some((version, action)) = version_and_action(headers) else {
        return Answer::fault(
            Version::V11,
            415,
            "unsupported Content-Type; use text/xml (SOAP 1.1) or application/soap+xml (SOAP 1.2)",
        )
        .with_http(415);
    };
    let bytes = match read_body(headers, body, rt.limits.max_request_bytes).await {
        Ok(b) => b,
        Err(error) => return error.fault(version),
    };
    report.request_body_bytes = Some(bytes.len());
    // Parse, dispatch and validate in the `mb.decode` span.
    let decode = tracing::info_span!("mb.decode").entered();
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Answer::fault(version, 400, "the request body is not UTF-8");
    };
    let child = match envelope::body_child(text) {
        Ok(c) => c,
        Err(message) => return Answer::fault(version, 400, message),
    };
    let Some(endpoint) = dispatch(endpoints, action.as_deref(), &child) else {
        return Answer::fault(version, 400, "unknown operation");
    };
    let Some(op) = rt.bundle.operations.get(endpoint.op) else {
        return Answer::fault(version, 500, "unknown operation");
    };
    report.operation_id = Some(op.id.clone());
    tracing::Span::current().record("operation", op.id.as_str());

    let input = endpoint
        .input
        .as_ref()
        .map(|schema| xml_json::element_to_json(&child, schema));
    *decoded = input.clone();
    if let (Some(validator), Some(value)) = (&endpoint.validator, &input)
        && let Err(errors) = validator.validate(value)
    {
        return Answer::fault(
            version,
            400,
            format!("invalid request: {}", errors.join("; ")),
        );
    }
    drop(decode);

    report.admission_ran = true;
    // SOAP credentials travel in HTTP headers only.
    let plan = match call::plan(rt, op, headers, Presented::default()).await {
        Ok(plan) => plan,
        Err(PlanFailure {
            error,
            tenancy,
            session,
        }) => {
            report.tenancy = tenancy;
            if let Some(s) = session {
                report.session = s;
            }
            if let PlanError::RateLimited(r) = &error
                && let Some(generated) = call::declared_rate_limited(rt, op)
            {
                return rate_limited_fault(rt, op, version, r, generated);
            }
            return plan_fault(version, error, &rt.name);
        }
    };
    report.tenancy = Some(plan.tenancy.clone());
    report.session = plan.session.session.clone();
    *delay = plan.delay;

    let call = OperationCall {
        operation_id: op.id.clone(),
        headers: forwarded_headers(rt, headers),
        body: input,
        ..Default::default()
    };
    if plan.native_chaos_error() {
        let message = format!("Service Unavailable ({})", crate::chaos::CHAOS_ERROR_DETAIL);
        let mut answer = Answer::fault(version, 503, message).with_http(503);
        answer.chaos = true;
        return answer;
    }
    let chaos = plan.chaos.inject_error;
    match plan.forced_status.filter(|s| !(200..300).contains(s)) {
        Some(status) => {
            let mut answer = forced_fault(st, op, plan, &call, version, status, report).await;
            answer.chaos = chaos;
            answer
        }
        None => match call::generate_once(rt, &plan, &call).await {
            Ok(generated) => {
                report.usage = generated.usage;
                let body = generated.body.unwrap_or(Value::Null);
                let encode = tracing::info_span!("mb.encode").entered();
                let schema = op
                    .output_for_status(generated.status)
                    .and_then(|o| o.schema.as_ref())
                    .map(|s| rt.bundle.with_reachable_defs(s));
                let xml = match schema {
                    Some(schema) => json_to_xml(&body, &schema),
                    None => Err(format!("operation {} declares no output schema", op.id)),
                };
                let xml = xml.map(|xml| envelope::envelope(version, &xml));
                drop(encode);
                match xml {
                    Ok(xml) => Answer {
                        http: generated.status,
                        status: generated.status,
                        version,
                        xml,
                        source: Some(generated.source),
                        record: body,
                        retry_after: None,
                        www_authenticate: None,
                        chaos: false,
                    },
                    Err(e) => Answer::fault(version, 500, e),
                }
            }
            Err(e) => engine_fault(version, op, e),
        },
    }
}

/// A forced non-2xx status: when the operation declares a `500` fault,
/// its detail is generated (forced 500); otherwise a Fault without detail
/// and without an engine call.
async fn forced_fault(
    st: &SoapState,
    op: &Operation,
    mut plan: CallPlan<'_>,
    call: &OperationCall,
    version: Version,
    status: u16,
    report: &mut Report,
) -> Answer {
    let rt = &st.rt;
    let message = forced_message(status);
    let declared = op
        .outputs
        .iter()
        .find(|o| o.status == StatusSelector::Code(500))
        .and_then(|o| o.schema.as_ref());
    let Some(schema) = declared else {
        return Answer::fault(version, status, message);
    };
    plan.forced_status = Some(500);
    let generated = match call::generate_once(rt, &plan, call).await {
        Ok(g) => g,
        Err(e) => return engine_fault(version, op, e),
    };
    report.usage = generated.usage;
    let body = generated.body.unwrap_or(Value::Null);
    let detail = match json_to_xml(&body, &rt.bundle.with_reachable_defs(schema)) {
        Ok(d) => d,
        Err(e) => return Answer::fault(version, 500, e),
    };
    let mut answer = Answer::fault(version, status, message.as_str());
    answer.xml = envelope::fault(
        version,
        (400..500).contains(&status),
        &message,
        Some(&detail),
    );
    answer.source = Some(generated.source);
    if let Some(fault) = answer.record.get_mut("fault") {
        fault["detail"] = body;
    }
    answer
}

fn engine_fault(version: Version, op: &Operation, e: EngineError) -> Answer {
    match e {
        EngineError::UndeclaredStatus(code) => Answer::fault(
            version,
            400,
            format!("operation {} declares no response for status {code}", op.id),
        ),
        e => Answer::fault(version, 500, e.to_string()),
    }
}

fn plan_fault(version: Version, error: PlanError, mock: &str) -> Answer {
    match error {
        PlanError::Rejected(r) | PlanError::RateLimited(r) => {
            let mut answer = Answer::fault(version, r.status, r.detail);
            answer.retry_after = r.retry_after;
            answer
        }
        PlanError::Unauthorized(u) => {
            let mut answer = Answer::fault(version, 401, u.detail).with_http(401);
            answer.www_authenticate = Some(u.challenge);
            answer
        }
        PlanError::BadOverride(detail) => Answer::fault(version, 400, detail),
        PlanError::UnknownScenario(name) => Answer::fault(
            version,
            400,
            format!("mock {mock} has no scenario {name:?}"),
        ),
    }
}

/// A rate-limited call of an operation declaring a `429` response: a
/// Fault (HTTP 429, with `Retry-After`) whose detail is the engine's
/// fallback for that response.
fn rate_limited_fault(
    rt: &MockRuntime,
    op: &Operation,
    version: Version,
    rejection: &Rejection,
    generated: GeneratedResponse,
) -> Answer {
    let body = generated.body.unwrap_or(Value::Null);
    let detail = op
        .output_for_status(429)
        .and_then(|o| o.schema.as_ref())
        .and_then(|schema| json_to_xml(&body, &rt.bundle.with_reachable_defs(schema)).ok());
    let mut answer = Answer::fault(version, 429, rejection.detail.as_str()).with_http(429);
    answer.xml = envelope::fault(version, true, &rejection.detail, detail.as_deref());
    answer.source = Some(generated.source);
    answer.retry_after = rejection.retry_after;
    if let Some(fault) = answer.record.get_mut("fault") {
        fault["detail"] = body;
    }
    answer
}

fn forced_message(status: u16) -> String {
    match StatusCode::from_u16(status)
        .ok()
        .and_then(|c| c.canonical_reason())
    {
        Some(reason) => format!("{reason} (forced status {status})"),
        None => format!("forced status {status}"),
    }
}

/// The endpoint `action` names, else the one whose input root is the
/// Body child's QName.
fn dispatch<'e>(
    endpoints: &'e [Endpoint],
    action: Option<&str>,
    child: &Element,
) -> Option<&'e Endpoint> {
    action
        .and_then(|a| {
            endpoints
                .iter()
                .find(|e| e.soap_action.as_deref() == Some(a))
        })
        .or_else(|| {
            endpoints.iter().find(|e| {
                e.root
                    .as_ref()
                    .is_some_and(|(ns, name)| *ns == child.name.ns && *name == child.name.local)
            })
        })
}

/// The SOAP version from the `Content-Type` (`text/xml` → 1.1,
/// `application/soap+xml` → 1.2) and the action: `SOAPAction` for 1.1,
/// the `action` parameter for 1.2 (quotes stripped; empty → none).
fn version_and_action(headers: &HeaderMap) -> Option<(Version, Option<String>)> {
    let ct = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    let mut parts = ct.split(';');
    let media = parts.next()?.trim().to_ascii_lowercase();
    let unquote = |v: &str| {
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(v);
        (!v.is_empty()).then(|| v.to_string())
    };
    match media.as_str() {
        "text/xml" => {
            let action = headers
                .get(SOAP_ACTION_HEADER)
                .and_then(|v| v.to_str().ok())
                .and_then(unquote);
            Some((Version::V11, action))
        }
        "application/soap+xml" => {
            let action = parts
                .filter_map(|p| p.split_once('='))
                .find(|(k, _)| k.trim().eq_ignore_ascii_case("action"))
                .and_then(|(_, v)| unquote(v));
            Some((Version::V12, action))
        }
        _ => None,
    }
}

/// Why a request body was not read.
enum BodyError {
    /// Larger than the cap (in bytes).
    TooLarge(usize),
    Unreadable,
}

impl BodyError {
    /// The fault answering it, in the request's version: a `413` Fault with
    /// HTTP 413, or a `400` Fault.
    fn fault(self, version: Version) -> Answer {
        match self {
            Self::TooLarge(limit) => Answer::fault(
                version,
                413,
                format!("SOAP request bodies are limited to {limit} bytes"),
            )
            .with_http(413),
            Self::Unreadable => Answer::fault(version, 400, "the request body could not be read"),
        }
    }
}

/// The body, unless it exceeds `limit` bytes or cannot be read.
async fn read_body(headers: &HeaderMap, body: Body, limit: usize) -> Result<Vec<u8>, BodyError> {
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
        let chunk = chunk.map_err(|_| BodyError::Unreadable)?;
        if buf.len() + chunk.len() > limit {
            return Err(BodyError::TooLarge(limit));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// An RFC 9457 `application/problem+json` response, for transport-level
/// errors (unknown path, unsupported method).
fn problem(status: u16, title: &str, detail: impl Into<String>) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = json!({
        "type": "about:blank",
        "title": title,
        "status": code.as_u16(),
        "detail": detail.into(),
    });
    let mut resp = (code, body.to_string()).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn versions_and_actions() {
        let v = |pairs| version_and_action(&headers(pairs));
        assert_eq!(
            v(&[
                ("content-type", "text/xml; charset=utf-8"),
                ("soapaction", "\"urn:a\"")
            ]),
            Some((Version::V11, Some("urn:a".into())))
        );
        assert_eq!(
            v(&[("content-type", "Text/XML"), ("soapaction", "\"\"")]),
            Some((Version::V11, None))
        );
        assert_eq!(
            v(&[(
                "content-type",
                "application/soap+xml; charset=utf-8; Action=\"urn:b\""
            )]),
            Some((Version::V12, Some("urn:b".into())))
        );
        assert_eq!(
            v(&[
                ("content-type", "application/soap+xml"),
                ("soapaction", "urn:c")
            ]),
            Some((Version::V12, None))
        );
        assert_eq!(v(&[("content-type", "application/json")]), None);
        assert_eq!(v(&[]), None);
    }

    #[test]
    fn wsdl_queries() {
        assert!(is_wsdl_query(Some("wsdl")));
        assert!(is_wsdl_query(Some("WSDL")));
        assert!(is_wsdl_query(Some("x=1&wsdl")));
        assert!(!is_wsdl_query(Some("wsdl=1")));
        assert!(!is_wsdl_query(None));
    }

    #[test]
    fn fault_http_statuses() {
        assert_eq!(Answer::fault(Version::V11, 400, "x").http, 500);
        assert_eq!(Answer::fault(Version::V11, 503, "x").http, 500);
        assert_eq!(Answer::fault(Version::V12, 404, "x").http, 400);
        assert_eq!(Answer::fault(Version::V12, 500, "x").http, 500);
    }
}
