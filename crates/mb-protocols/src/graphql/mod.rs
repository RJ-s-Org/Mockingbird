//! GraphQL adapter: serves a bundle's queries and mutations from a dynamic
//! schema built from the SDL the importer stored in the bundle.
//!
//! Every query or mutation root field is one engine call (see
//! `resolve`); nested fields project from the generated JSON.
//! Subscriptions are served over WebSocket at the same path (see
//! `subscription`). GraphQL parse and validation errors are
//! answered `200 {errors}`; only transport-level failures (unknown path,
//! wrong method, oversized or unparseable body) are problem documents.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use async_graphql::Name;
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
    // Each root field is one engine call, and validation walks the document
    // with every fragment spread expanded: bound both before anything runs.
    if let Some(oversize) = oversize(
        &request.query,
        request.operation_name.as_deref(),
        rt.protocols.graphql_max_root_fields,
    ) {
        let (message, code) = oversize.describe();
        let body = json!({"errors": [{
            "message": message,
            "extensions": {"code": code},
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

/// Most selections (fields, inline fragments and fragment spreads) a
/// document may expand to once every fragment spread is replaced by its
/// fragment. Validation and execution walk the expanded document, so a
/// kilobyte of fragments that each spread the next one twice would cost
/// minutes of CPU. Ten times the default complexity limit, which refuses
/// any query this large anyway.
pub(crate) const MAX_EXPANDED_SELECTIONS: usize = 10_000;

/// Why a document is refused before it is validated or run.
#[derive(Debug, PartialEq)]
pub(crate) enum Oversize {
    /// The selected operation has more root fields than allowed (`count`,
    /// `max`).
    RootFields { count: usize, max: usize },
    /// The document expands to more than [`MAX_EXPANDED_SELECTIONS`].
    Selections,
}

impl Oversize {
    /// The GraphQL error message and `extensions.code`.
    pub(crate) fn describe(&self) -> (String, &'static str) {
        match self {
            Oversize::RootFields { count, max } => (
                format!("too many root fields: {count} (maximum {max})"),
                "TOO_MANY_ROOT_FIELDS",
            ),
            Oversize::Selections => (
                format!(
                    "query too large: more than {MAX_EXPANDED_SELECTIONS} selections once fragments are expanded"
                ),
                "QUERY_TOO_LARGE",
            ),
        }
    }
}

/// Whether `query` is too large to validate or run: the whole document
/// (every operation and fragment, as validation visits them) expanding to
/// more than [`MAX_EXPANDED_SELECTIONS`], or the operation it selects
/// having more than `max_root_fields` root fields (fragment spreads and
/// inline fragments on the root type expanded). Linear in the document:
/// each fragment's expansion is counted once and memoized, counts
/// saturate, and counting stops once over the bound. `None` when it is
/// within bounds, or does not parse or select an operation (the executor
/// reports those).
pub(crate) fn oversize(
    query: &str,
    operation_name: Option<&str>,
    max_root_fields: usize,
) -> Option<Oversize> {
    let doc = async_graphql::parser::parse_query(query).ok()?;
    let mut expander = Expander {
        doc: &doc,
        memo: HashMap::new(),
        visiting: Vec::new(),
    };
    let mut total = 0usize;
    for (_, op) in doc.operations.iter() {
        let size = expander.set(&op.node.selection_set.node);
        total = total.saturating_add(size.selections);
        if total > MAX_EXPANDED_SELECTIONS {
            return Some(Oversize::Selections);
        }
    }
    for name in doc.fragments.keys() {
        total = total.saturating_add(expander.fragment(name).selections);
        if total > MAX_EXPANDED_SELECTIONS {
            return Some(Oversize::Selections);
        }
    }
    let selected = match (&doc.operations, operation_name) {
        (DocumentOperations::Single(op), _) => op,
        (DocumentOperations::Multiple(ops), Some(name)) => ops.get(name)?,
        (DocumentOperations::Multiple(ops), None) if ops.len() == 1 => ops.values().next()?,
        (DocumentOperations::Multiple(_), None) => return None,
    };
    let count = expander.set(&selected.node.selection_set.node).fields;
    (count > max_root_fields).then_some(Oversize::RootFields {
        count,
        max: max_root_fields,
    })
}

/// The expanded size of a selection set.
#[derive(Clone, Copy, Default)]
struct Size {
    /// Fields directly in the set, through inline fragments and spreads.
    fields: usize,
    /// Every selection at any depth.
    selections: usize,
}

/// Expands selection sets, counting each fragment once.
struct Expander<'d> {
    doc: &'d ExecutableDocument,
    memo: HashMap<&'d str, Size>,
    /// Fragments being expanded: a spread of one of them is a cycle (which
    /// validation rejects) and counts nothing.
    visiting: Vec<&'d str>,
}

impl<'d> Expander<'d> {
    /// The size of `set`; stops early (still over the bound) once it has
    /// more than [`MAX_EXPANDED_SELECTIONS`] selections.
    fn set(&mut self, set: &'d SelectionSet) -> Size {
        let mut size = Size::default();
        for item in &set.items {
            let inner = match &item.node {
                Selection::Field(field) => Size {
                    fields: 1,
                    selections: self.set(&field.node.selection_set.node).selections,
                },
                Selection::InlineFragment(f) => self.set(&f.node.selection_set.node),
                Selection::FragmentSpread(spread) => self.fragment(&spread.node.fragment_name.node),
            };
            size.fields = size.fields.saturating_add(inner.fields);
            size.selections = size
                .selections
                .saturating_add(inner.selections)
                .saturating_add(1);
            if size.selections > MAX_EXPANDED_SELECTIONS {
                break;
            }
        }
        size
    }

    /// The size of fragment `name`'s selection set, memoized.
    fn fragment(&mut self, name: &'d Name) -> Size {
        let key = name.as_str();
        if let Some(size) = self.memo.get(key) {
            return *size;
        }
        if self.visiting.contains(&key) {
            return Size::default();
        }
        let Some(fragment) = self.doc.fragments.get(name) else {
            return Size::default();
        };
        self.visiting.push(key);
        let size = self.set(&fragment.node.selection_set.node);
        self.visiting.pop();
        self.memo.insert(key, size);
        size
    }
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
