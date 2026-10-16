//! GraphQL adapter: serves a bundle's queries and mutations from a dynamic
//! schema built from the SDL the importer stored in the bundle.
//!
//! Every query or mutation root field is one engine call (see
//! [`resolve`]); nested fields project from the generated JSON.
//! Subscriptions are served over WebSocket at the same path (see
//! [`subscription`]). GraphQL parse and validation errors are
//! answered `200 {errors}`; only transport-level failures (unknown path,
//! wrong method, oversized or unparseable body) are problem documents.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use async_graphql::Request;
use async_graphql::parser::types::{
    DocumentOperations, ExecutableDocument, OperationType, Selection, SelectionSet,
};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{FromRequestParts, Query, Request as HttpRequest, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::{Value, json};

use mb_ext::Tenancy;

use crate::{DataPlaneAuthMode, MockRuntime, call};

mod resolve;
mod schema;
mod subscription;

pub(crate) use schema::build_schema;

const SOURCE_HEADER: HeaderName = HeaderName::from_static("x-mockingbird-source");
const PROBLEM_JSON: &str = "application/problem+json";
const GRAPHQL_MEDIA_TYPE: &str = "application/graphql";

/// Why a bundle cannot be served over GraphQL.
#[derive(Debug, thiserror::Error)]
pub enum GraphqlError {
    #[error("bundle has no GraphQL schema")]
    MissingSdl,
    #[error("invalid GraphQL schema: {0}")]
    Schema(String),
}

struct GraphqlState {
    rt: Arc<MockRuntime>,
    schema: async_graphql::dynamic::Schema,
    /// Open subscription WebSocket connections to this mock.
    connections: Arc<AtomicUsize>,
}

/// A router serving `rt.bundle`'s GraphQL schema at
/// `rt.protocols.graphql_path`, over `POST` and `GET`, and subscriptions
/// over a WebSocket upgrade of a `GET`.
pub fn try_router(rt: Arc<MockRuntime>) -> Result<Router, GraphqlError> {
    let schema = build_schema(&rt)?;
    let state = Arc::new(GraphqlState {
        rt,
        schema,
        connections: Arc::new(AtomicUsize::new(0)),
    });
    Ok(Router::new().fallback(handle).with_state(state))
}

/// The `extensions.code` of a field error standing for HTTP `status`.
pub(crate) fn error_code(status: u16) -> &'static str {
    match status {
        400 => "BAD_USER_INPUT",
        401 => "UNAUTHENTICATED",
        403 => "FORBIDDEN",
        404 => "NOT_FOUND",
        409 => "CONFLICT",
        429 => "RATE_LIMITED",
        402..=499 => "BAD_REQUEST",
        _ => "INTERNAL_SERVER_ERROR",
    }
}

/// Answers one request. Every response sent before execution (an unknown
/// path, a refused upgrade, an invalid or oversized request, a wrong
/// method, too many root fields, a refused admission or key) is reported
/// as a call with its status.
async fn handle(State(st): State<Arc<GraphqlState>>, req: HttpRequest) -> Response {
    let (mut parts, body) = req.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let headers = parts.headers.clone();
    let rt = &st.rt;
    if uri.path() != rt.protocols.graphql_path {
        let resp = problem(
            404,
            "Not Found",
            format!("no GraphQL endpoint at {}", uri.path()),
        );
        return refused(rt, &method, &headers, resp, None).await;
    }
    let query = Query::<Vec<(String, String)>>::try_from_uri(&uri)
        .map(|q| q.0)
        .unwrap_or_default();
    if method == Method::GET && is_websocket_upgrade(&headers) {
        return match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(upgrade) => {
                let connections = Arc::clone(&st.connections);
                subscription::upgrade(st.schema.clone(), rt, connections, upgrade, headers, query)
                    .await
            }
            Err(rejection) => {
                let resp = rejection.into_response();
                refused(rt, &method, &headers, resp, None).await
            }
        };
    }
    let request = match method {
        Method::GET => {
            let request = match async_graphql::http::parse_query_string(uri.query().unwrap_or("")) {
                Ok(r) => r,
                Err(e) => {
                    let resp = problem(400, "Invalid GraphQL request", e.to_string());
                    return refused(rt, &method, &headers, resp, None).await;
                }
            };
            if is_mutation(&request) {
                let mut resp = problem(
                    405,
                    "Method Not Allowed",
                    "mutations must be sent with POST",
                );
                resp.headers_mut()
                    .insert(header::ALLOW, HeaderValue::from_static("POST"));
                return refused(rt, &method, &headers, resp, None).await;
            }
            request
        }
        Method::POST => {
            let parsed = match read_body(&headers, body, rt.limits.max_request_bytes).await {
                Ok(bytes) => parse_post(&headers, &bytes)
                    .map_err(|detail| problem(400, "Invalid GraphQL request", detail)),
                Err(resp) => Err(resp),
            };
            match parsed {
                Ok(r) => r,
                Err(resp) => return refused(rt, &method, &headers, resp, None).await,
            }
        }
        _ => {
            let mut resp = problem(
                405,
                "Method Not Allowed",
                format!("{method} is not supported; use GET or POST"),
            );
            resp.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, POST"));
            return refused(rt, &method, &headers, resp, None).await;
        }
    };
    // Each root field is one engine call: cap them before anything runs.
    let max = rt.protocols.graphql_max_root_fields;
    if let Some(count) = root_field_count(&request).filter(|n| *n > max) {
        let body = json!({"errors": [{
            "message": format!("too many root fields: {count} (maximum {max})"),
            "extensions": {"code": "TOO_MANY_ROOT_FIELDS"},
        }]});
        let mut resp = (StatusCode::BAD_REQUEST, body.to_string()).into_response();
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        return refused(rt, &method, &headers, resp, Some(body)).await;
    }
    // Data-plane auth: in `api_key` mode the whole request is admitted
    // (tenant, then quota) and then its key checked once, before anything
    // runs (a 429/401 problem, as for REST, reported like any call); root
    // fields are then not admitted again. Each root field still runs the
    // per-operation checks of the shared pipeline.
    let admitted = if rt.data_plane_auth.mode == DataPlaneAuthMode::ApiKey {
        match admit_request(rt, &request, &method, &headers, &query).await {
            Ok(tenancy) => Some(tenancy),
            Err(resp) => return resp,
        }
    } else {
        None
    };
    execute(&st, request, &method, headers, admitted).await
}

/// Admits a whole request and checks its data-plane key; a refusal is
/// answered with a problem and reported (without the key).
async fn admit_request(
    rt: &MockRuntime,
    request: &Request,
    method: &Method,
    headers: &HeaderMap,
    query: &[(String, String)],
) -> Result<Tenancy, Response> {
    let started = tokio::time::Instant::now();
    let operation_id = request.operation_name.clone().unwrap_or_default();
    let (resp, tenancy) = match call::admit(rt, &operation_id, headers).await {
        Err((r, tenancy)) => {
            let mut resp = problem(r.status, &r.title, r.detail);
            if let Some(after) = r.retry_after {
                let secs = after.as_secs() + u64::from(after.subsec_nanos() > 0);
                resp.headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(secs));
            }
            (resp, tenancy)
        }
        Ok(tenancy) => match call::check_key(rt, headers, query).await {
            Ok(()) => return Ok(tenancy),
            Err(u) => {
                let mut resp = problem(401, "Unauthorized", u.detail);
                if let Ok(v) = HeaderValue::from_str(&u.challenge) {
                    resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
                }
                (resp, Some(tenancy))
            }
        },
    };
    let status = resp.status().as_u16();
    let report = call::Report {
        operation_id: None,
        session: call::DEFAULT_SESSION.into(),
        method: method.as_str().to_string(),
        path: rt.protocols.graphql_path.clone(),
        status,
        source: None,
        usage: Default::default(),
        latency: started.elapsed(),
        tenancy,
        admission_ran: true,
        request: if rt.recorder.is_some() {
            json!({
                "variables": request.variables,
                "headers": call::recorded_headers(rt, headers),
            })
        } else {
            Value::Null
        },
        request_body_bytes: None,
        response: None,
    };
    call::report(rt, headers, report).await;
    Err(resp)
}

/// Runs `request`, then sleeps the largest injected latency of its root
/// fields (they have all been reported by then) and answers `200` JSON.
async fn execute(
    st: &GraphqlState,
    request: Request,
    method: &Method,
    headers: HeaderMap,
    admitted: Option<Tenancy>,
) -> Response {
    let variables = serde_json::to_value(&request.variables).unwrap_or(Value::Null);
    let call = Arc::new(resolve::HttpCall::new(
        headers,
        method.as_str().to_string(),
        variables,
        admitted,
    ));
    let mut response = st.schema.execute(request.data(Arc::clone(&call))).await;
    let collected = call.collected();
    if !collected.sources.is_empty() {
        let ext = json!({ "sources": collected.sources });
        if let Ok(v) = async_graphql::Value::from_json(ext) {
            response.extensions.insert("mockingbird".into(), v);
        }
    }
    if let Some(delay) = collected.delay {
        tokio::time::sleep(delay).await;
    }
    let body = match serde_json::to_vec(&response) {
        Ok(b) => b,
        Err(e) => return problem(500, "Internal Server Error", e.to_string()),
    };
    let mut resp = (StatusCode::OK, body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Some(source) = collected.worst {
        resp.headers_mut()
            .insert(SOURCE_HEADER, HeaderValue::from_static(source.as_str()));
    }
    resp
}

/// Whether the request asks for `Upgrade: websocket`.
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// The body, unless it exceeds `limit` bytes (413) or cannot be read
/// (400).
async fn read_body(headers: &HeaderMap, body: Body, limit: usize) -> Result<Bytes, Response> {
    let too_large = || {
        problem(
            413,
            "Payload Too Large",
            format!("GraphQL request bodies are limited to {limit} bytes"),
        )
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > limit) {
        return Err(too_large());
    }
    let mut stream = body.into_data_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| problem(400, "Invalid GraphQL request", format!("body: {e}")))?;
        if buf.len() + chunk.len() > limit {
            return Err(too_large());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

/// Reports `resp`, a response sent before admission ran (so attributed
/// like an unrouted call), as a call with its status and the given
/// recorded `response`; returns it.
pub(crate) async fn refused(
    rt: &MockRuntime,
    method: &Method,
    headers: &HeaderMap,
    resp: Response,
    response: Option<Value>,
) -> Response {
    let report = call::Report {
        operation_id: None,
        session: call::DEFAULT_SESSION.into(),
        method: method.as_str().to_string(),
        path: rt.protocols.graphql_path.clone(),
        status: resp.status().as_u16(),
        source: None,
        usage: Default::default(),
        latency: Default::default(),
        tenancy: None,
        admission_ran: false,
        request: if rt.recorder.is_some() {
            json!({"headers": call::recorded_headers(rt, headers)})
        } else {
            Value::Null
        },
        request_body_bytes: None,
        response: response.filter(|_| rt.recorder.is_some()),
    };
    call::report(rt, headers, report).await;
    resp
}

/// A `POST` body: raw query text for `application/graphql`, else the JSON
/// `{query, variables, operationName}` envelope.
fn parse_post(headers: &HeaderMap, body: &[u8]) -> Result<Request, String> {
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase());
    if media.as_deref() == Some(GRAPHQL_MEDIA_TYPE) {
        let text = std::str::from_utf8(body).map_err(|e| format!("body is not UTF-8: {e}"))?;
        return Ok(Request::new(text));
    }
    serde_json::from_slice::<Request>(body).map_err(|e| format!("body: {e}"))
}

/// Whether the operation `request` selects is a mutation. Anything that
/// does not parse or select is left to the executor to report.
fn is_mutation(request: &Request) -> bool {
    let Ok(doc) = async_graphql::parser::parse_query(&request.query) else {
        return false;
    };
    let selected = match (&doc.operations, request.operation_name.as_deref()) {
        (DocumentOperations::Single(op), _) => Some(op),
        (DocumentOperations::Multiple(ops), Some(name)) => ops.get(name),
        (DocumentOperations::Multiple(ops), None) if ops.len() == 1 => ops.values().next(),
        (DocumentOperations::Multiple(_), None) => None,
    };
    selected.is_some_and(|op| op.node.ty == OperationType::Mutation)
}

/// How many root fields the operation `request` selects, with fragment
/// spreads and inline fragments on the root type expanded. `None` when the
/// query does not parse or selects no operation (the executor reports
/// that).
fn root_field_count(request: &Request) -> Option<usize> {
    let doc = async_graphql::parser::parse_query(&request.query).ok()?;
    let selected = match (&doc.operations, request.operation_name.as_deref()) {
        (DocumentOperations::Single(op), _) => op,
        (DocumentOperations::Multiple(ops), Some(name)) => ops.get(name)?,
        (DocumentOperations::Multiple(ops), None) if ops.len() == 1 => ops.values().next()?,
        (DocumentOperations::Multiple(_), None) => return None,
    };
    let mut visiting = Vec::new();
    Some(count_fields(
        &doc,
        &selected.node.selection_set.node,
        &mut visiting,
    ))
}

/// The fields of `set`, expanding fragments; a fragment already being
/// expanded (a cycle, which validation rejects) counts nothing.
fn count_fields<'d>(
    doc: &'d ExecutableDocument,
    set: &'d SelectionSet,
    visiting: &mut Vec<&'d str>,
) -> usize {
    set.items
        .iter()
        .map(|item| match &item.node {
            Selection::Field(_) => 1,
            Selection::InlineFragment(f) => count_fields(doc, &f.node.selection_set.node, visiting),
            Selection::FragmentSpread(spread) => {
                let name = spread.node.fragment_name.node.as_str();
                if visiting.contains(&name) {
                    return 0;
                }
                let Some(fragment) = doc.fragments.get(&spread.node.fragment_name.node) else {
                    return 0;
                };
                visiting.push(name);
                let n = count_fields(doc, &fragment.node.selection_set.node, visiting);
                visiting.pop();
                n
            }
        })
        .sum()
}

/// An RFC 9457 `application/problem+json` response.
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

    #[test]
    fn error_codes() {
        let cases = [
            (400, "BAD_USER_INPUT"),
            (401, "UNAUTHENTICATED"),
            (403, "FORBIDDEN"),
            (404, "NOT_FOUND"),
            (409, "CONFLICT"),
            (429, "RATE_LIMITED"),
            (402, "BAD_REQUEST"),
            (418, "BAD_REQUEST"),
            (499, "BAD_REQUEST"),
            (500, "INTERNAL_SERVER_ERROR"),
            (503, "INTERNAL_SERVER_ERROR"),
            (302, "INTERNAL_SERVER_ERROR"),
        ];
        for (status, code) in cases {
            assert_eq!(error_code(status), code, "status {status}");
        }
    }

    #[test]
    fn mutation_detection() {
        let req = |q: &str, name: Option<&str>| {
            let mut r = Request::new(q);
            r.operation_name = name.map(str::to_string);
            r
        };
        assert!(is_mutation(&req("mutation { a }", None)));
        assert!(!is_mutation(&req("{ a }", None)));
        assert!(!is_mutation(&req("query Q { a }", None)));
        let both = "query Q { a } mutation M { b }";
        assert!(is_mutation(&req(both, Some("M"))));
        assert!(!is_mutation(&req(both, Some("Q"))));
        assert!(!is_mutation(&req(both, None)));
        assert!(!is_mutation(&req("mutation {", None)));
    }
}
