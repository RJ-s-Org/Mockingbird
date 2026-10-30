//! Subscriptions over WebSocket (`graphql-transport-ws` and the legacy
//! `graphql-ws`).
//!
//! A subscription root field streams its operation's engine items, then
//! the resources of its type upserted in the same session, each projected
//! like a query result. It runs until the client completes it or
//! disconnects, or the process shuts down, and is reported once, when its
//! stream is dropped. A subscription chaos drops ends after its planned
//! number of generated items with an `error` message (code `UNAVAILABLE`)
//! for its id.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_graphql::dynamic::{
    FieldValue, ResolverContext, SubscriptionField, SubscriptionFieldFuture,
};
use async_graphql::http::{WebSocket, WebSocketProtocols, WsMessage};
use async_graphql::parser::types::{FieldDefinition, Type};
use async_graphql::{Data, ErrorExtensions};
use axum::extract::WebSocketUpgrade;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, header};
use axum::response::Response;
use futures::StreamExt;
use futures::stream::{self, BoxStream, PollNext};
use mb_engine::{OperationCall, Source, StreamItem};
use mb_ext::Tenancy;
use mb_mcm::Operation;
use serde_json::{Map, Value, json};
use tokio::time::Instant;
use tracing::field::Empty;
use tracing::{Instrument, Span};

use super::resolve::{Failure, HttpCall, Shapes, forced_message, project};
use super::schema::{input_value, type_ref};
use crate::call::{
    self, CallPlan, DEFAULT_SESSION, Presented, Report, forwarded_headers, recorded_headers,
};
use crate::ws::{ChannelConfig, ChannelIo, Outbound, serve_channel};
use crate::{DataPlaneAuthMode, MockRuntime};

/// The subprotocols served, in order of preference.
const SUBPROTOCOLS: [&str; 2] = ["graphql-transport-ws", "graphql-ws"];

/// One WebSocket connection's credentials and override headers, shared by
/// its subscriptions through the connection data.
struct WsCall {
    /// The upgrade request's headers overlaid by the `connection_init`
    /// payload, credentials included: used only to admit and authenticate
    /// each subscription, never forwarded or recorded.
    auth_headers: HeaderMap,
    /// [`WsCall::auth_headers`] without the payload's credential members:
    /// what the prompt and the request record may see (after the usual
    /// redaction).
    headers: HeaderMap,
    /// The upgrade URL's query pairs (passthrough query API keys).
    query: Vec<(String, String)>,
    /// The connection presented a valid data-plane key (`api_key` mode).
    key_verified: bool,
}

/// The call of a query or mutation root field sent over this connection,
/// with the connection's credentials and override headers; an error when
/// the field is not run over a subscription WebSocket.
pub(crate) fn http_call(ctx: &ResolverContext<'_>) -> async_graphql::Result<HttpCall> {
    let conn = ctx.data::<Arc<WsCall>>()?;
    let variables = serde_json::to_value(&ctx.ctx.query_env.variables).unwrap_or(Value::Null);
    Ok(HttpCall::websocket(
        conn.headers.clone(),
        conn.auth_headers.clone(),
        conn.query.clone(),
        conn.key_verified,
        variables,
    ))
}

/// `connection_init` payload members that carry a data-plane key, besides
/// the mock's key header.
const PAYLOAD_KEY_MEMBERS: [&str; 2] = ["api_key", "apiKey"];
/// Close code for a connection refused by data-plane auth.
const CLOSE_UNAUTHORIZED: u16 = 4401;
/// Close code for a connection that sent no `connection_init` in time.
const CLOSE_INIT_TIMEOUT: u16 = 4408;

/// Upgrades a `GET` at the GraphQL path to a subscription channel. The
/// override headers are `headers` (the upgrade request's), overlaid by
/// the string-valued keys of the `connection_init` payload.
///
/// In `api_key` mode the connection is authenticated before any
/// subscription runs: by a key in the key header or the `api_key` query
/// parameter of the upgrade request (an invalid one is a `401` problem,
/// with no upgrade), else by the `connection_init` payload (the key header
/// name, `api_key` or `apiKey`); without a valid key the connection is
/// refused with a `connection_error` (`graphql-ws`) or close code 4401
/// (`graphql-transport-ws`). Refusals are reported as `401` calls.
///
/// Once `graphql_max_connections_per_mock` connections are open
/// (`connections` counts them), the upgrade is refused with a `503`
/// problem and `Retry-After: 1`, reported as a call. A connection that
/// sends no `connection_init` within `graphql_connection_init_timeout` is
/// closed with code 4408, freeing its slot.
pub(crate) async fn upgrade(
    schema: super::directives::Served,
    rt: &Arc<MockRuntime>,
    connections: Arc<AtomicUsize>,
    upgrade: WebSocketUpgrade,
    headers: HeaderMap,
    query: Vec<(String, String)>,
) -> Response {
    let Some(slot) =
        ConnectionSlot::acquire(connections, rt.protocols.graphql_max_connections_per_mock)
    else {
        let mut resp = super::problem(
            503,
            "Service Unavailable",
            "too many GraphQL WebSocket connections",
        );
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return super::refused(rt, &Method::GET, &headers, resp, None).await;
    };
    let mut key_verified = false;
    if rt.data_plane_auth.mode == DataPlaneAuthMode::ApiKey
        && crate::dp_auth::presented_key(rt, &headers, &query).is_some()
    {
        match call::check_key(rt, &headers, &query).await {
            Ok(()) => key_verified = true,
            Err(u) => {
                report_refusal(rt, &headers, &u.detail).await;
                let mut resp = super::problem(401, "Unauthorized", u.detail);
                if let Ok(v) = HeaderValue::from_str(&u.challenge) {
                    resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
                }
                return resp;
            }
        }
    }
    let cfg = ChannelConfig::for_mock(rt, SUBPROTOCOLS.to_vec());
    let rt = Arc::clone(rt);
    serve_channel(upgrade, cfg, move |io| async move {
        // Held for as long as the connection is open.
        let _slot = slot;
        run(schema, io, rt, headers, query, key_verified).await;
    })
}

/// One open connection of a mock, counted until dropped.
struct ConnectionSlot(Arc<AtomicUsize>);

impl ConnectionSlot {
    /// A slot, unless `max` connections are already open.
    fn acquire(open: Arc<AtomicUsize>, max: usize) -> Option<Self> {
        open.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < max).then_some(n + 1)
        })
        .ok()?;
        Some(Self(open))
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Reports a connection refused by data-plane auth as a `401` call.
async fn report_refusal(rt: &MockRuntime, headers: &HeaderMap, detail: &str) {
    let report = Report {
        operation_id: None,
        session: DEFAULT_SESSION.into(),
        method: "GET".into(),
        path: rt.protocols.graphql_path.clone(),
        status: 401,
        source: None,
        fallback_reason: None,
        usage: Default::default(),
        latency: Default::default(),
        tenancy: None,
        admission_ran: false,
        request: if rt.recorder.is_some() {
            json!({"headers": recorded_headers(rt, headers)})
        } else {
            Value::Null
        },
        request_body_bytes: None,
        response: rt
            .recorder
            .is_some()
            .then(|| json!({"message": detail, "extensions": {"code": "UNAUTHENTICATED"}})),
    };
    call::report(rt, headers, report).await;
}

async fn run(
    schema: super::directives::Served,
    io: ChannelIo,
    rt: Arc<MockRuntime>,
    headers: HeaderMap,
    query: Vec<(String, String)>,
    key_verified: bool,
) {
    // Without a negotiated subprotocol, speak the current one.
    let init_timeout = rt.protocols.graphql_connection_init_timeout;
    let protocol = io
        .protocol
        .as_deref()
        .and_then(|p| p.parse().ok())
        .unwrap_or(WebSocketProtocols::GraphQLWS);
    let refused = Arc::new(AtomicBool::new(false));
    let refused_init = Arc::clone(&refused);
    let initialised = Arc::new(AtomicBool::new(false));
    let initialised_init = Arc::clone(&initialised);
    let active = Arc::new(Mutex::new(HashSet::new()));
    let incoming = capped(
        io.incoming,
        Arc::clone(&rt),
        headers.clone(),
        protocol,
        Arc::clone(&active),
        io.outgoing.clone(),
    );
    let messages =
        WebSocket::new(schema, incoming, protocol).on_connection_init(move |payload| async move {
            initialised_init.store(true, Ordering::Relaxed);
            let (auth_headers, headers) = overlay(&rt, headers, &payload);
            let api_key = rt.data_plane_auth.mode == DataPlaneAuthMode::ApiKey;
            if api_key
                && !key_verified
                && let Err(u) = call::check_key(&rt, &auth_headers, &[]).await
            {
                refused_init.store(true, Ordering::Relaxed);
                report_refusal(&rt, &headers, &u.detail).await;
                return Err(async_graphql::Error::new(u.detail));
            }
            let mut data = Data::default();
            data.insert(Arc::new(WsCall {
                auth_headers,
                headers,
                query,
                key_verified: api_key,
            }));
            Ok(data)
        });
    futures::pin_mut!(messages);
    // A connection that never sends `connection_init` would hold its slot
    // (counted against `graphql_max_connections_per_mock`) for as long as
    // the client likes: close it once the init timeout passes.
    let init_deadline = tokio::time::sleep(init_timeout);
    futures::pin_mut!(init_deadline);
    // Operations chaos dropped: their `complete` is not sent.
    let mut dropped = HashSet::new();
    loop {
        let message = tokio::select! {
            message = messages.next() => message,
            () = &mut init_deadline, if !initialised.load(Ordering::Relaxed) => {
                let close = Outbound::Close {
                    code: CLOSE_INIT_TIMEOUT,
                    reason: "Connection initialisation timeout".into(),
                };
                let _ = io.outgoing.send(close).await;
                break;
            }
        };
        let Some(message) = message else { break };
        let (out, last) = match message {
            WsMessage::Text(text) => {
                let text = match dropped_error(&text, protocol) {
                    Some((id, error)) => {
                        dropped.insert(id);
                        error
                    }
                    None => text,
                };
                let op = operation_message(&text);
                if let Some((ty, id)) = &op
                    && ty == "complete"
                    && dropped.remove(id)
                {
                    continue;
                }
                // A finished operation frees its slot.
                if let Some((ty, id)) = op
                    && matches!(ty.as_str(), "complete" | "error")
                {
                    lock(&active).remove(&id);
                }
                (Outbound::Text(text), false)
            }
            // `graphql-transport-ws` answers a refused `connection_init`
            // with a close frame: make it say why.
            WsMessage::Close(_, _) if refused.load(Ordering::Relaxed) => (
                Outbound::Close {
                    code: CLOSE_UNAUTHORIZED,
                    reason: "Unauthorized".into(),
                },
                true,
            ),
            WsMessage::Close(code, reason) => (Outbound::Close { code, reason }, true),
        };
        if io.outgoing.send(out).await.is_err() || last {
            break;
        }
    }
}

/// A `next` (`data`) message carrying the error of a subscription chaos
/// dropped, rewritten as an `error` message for its id; with the id.
fn dropped_error(text: &str, protocol: WebSocketProtocols) -> Option<(String, String)> {
    // Most messages are not: skip parsing them.
    if !text.contains(r#""chaos":"drop""#) {
        return None;
    }
    let msg: Value = serde_json::from_str(text).ok()?;
    if !matches!(msg.get("type")?.as_str()?, "next" | "data") {
        return None;
    }
    let id = msg.get("id")?.as_str()?.to_string();
    let errors = msg.get("payload")?.get("errors")?.as_array()?;
    let error = errors
        .iter()
        .find(|e| e["extensions"]["chaos"] == "drop")?
        .clone();
    let payload = match protocol {
        WebSocketProtocols::GraphQLWS => json!([error]),
        _ => error,
    };
    let frame = json!({"type": "error", "id": id, "payload": payload});
    Some((id, frame.to_string()))
}

/// The open operations of one connection, by id.
type Active = Arc<Mutex<HashSet<String>>>;

fn lock(active: &Active) -> std::sync::MutexGuard<'_, HashSet<String>> {
    active.lock().unwrap_or_else(|p| p.into_inner())
}

/// `type` and `id` of a protocol message, when it has both (as strings).
/// The payload is skipped, not built into a [`Value`]; a repeated member
/// counts by its last value, as when the frame is parsed into a `Value`.
fn operation_message(text: &str) -> Option<(String, String)> {
    let head: Head = serde_json::from_str(text).ok()?;
    Some((head.ty?, head.id?))
}

/// The `type` and `id` members of a protocol message; `None` for one that
/// is missing or not a string.
#[derive(Default)]
struct Head {
    ty: Option<String>,
    id: Option<String>,
}

impl<'de> serde::Deserialize<'de> for Head {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct HeadVisitor;
        impl<'de> serde::de::Visitor<'de> for HeadVisitor {
            type Value = Head;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a protocol message object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Head, A::Error> {
                let mut head = Head::default();
                while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                    match key.as_ref() {
                        "type" => head.ty = map.next_value::<MaybeString>()?.0,
                        "id" => head.id = map.next_value::<MaybeString>()?.0,
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(head)
            }
        }
        deserializer.deserialize_map(HeadVisitor)
    }
}

/// A string, or `None` for any other JSON value (which is skipped).
struct MaybeString(Option<String>);

impl<'de> serde::Deserialize<'de> for MaybeString {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(deserializer)? {
            Value::String(s) => MaybeString(Some(s)),
            _ => MaybeString(None),
        })
    }
}

/// The client's messages, with operations past the connection's
/// `graphql_max_subscriptions_per_connection` open ones refused: such a
/// `subscribe` (`start`) is answered with an `error` message for its id,
/// reported as a `429` call and not handed to the executor; the
/// connection stays open. `complete` (`stop`) frees a slot, as does the
/// server completing or failing an operation.
fn capped(
    incoming: BoxStream<'static, String>,
    rt: Arc<MockRuntime>,
    headers: HeaderMap,
    protocol: WebSocketProtocols,
    active: Active,
    outgoing: tokio::sync::mpsc::Sender<Outbound>,
) -> BoxStream<'static, String> {
    let max = rt.protocols.graphql_max_subscriptions_per_connection;
    incoming
        .filter_map(move |text| {
            let (rt, headers, active, outgoing) = (
                Arc::clone(&rt),
                headers.clone(),
                Arc::clone(&active),
                outgoing.clone(),
            );
            async move {
                let Some((ty, id)) = operation_message(&text) else {
                    return Some(text);
                };
                match ty.as_str() {
                    "subscribe" | "start" => {
                        // Bounded before async-graphql validates it, as over HTTP.
                        if let Some(oversize) = subscribe_oversize(&text, &rt) {
                            let (message, code) = oversize.describe();
                            let error = json!({"message": message, "extensions": {"code": code}});
                            let _ = outgoing
                                .send(Outbound::Text(error_frame(protocol, &id, error)))
                                .await;
                            report_refused(&rt, &headers, 400, &message, code).await;
                            return None;
                        }
                        let admitted = {
                            let mut open = lock(&active);
                            let admitted = open.contains(&id) || open.len() < max;
                            if admitted {
                                open.insert(id.clone());
                            }
                            admitted
                        };
                        if admitted {
                            return Some(text);
                        }
                        let message =
                            format!("too many subscriptions on this connection (maximum {max})");
                        let error = json!({"message": message});
                        let _ = outgoing
                            .send(Outbound::Text(error_frame(protocol, &id, error)))
                            .await;
                        report_refused(&rt, &headers, 429, &message, "RATE_LIMITED").await;
                        None
                    }
                    "complete" | "stop" => {
                        lock(&active).remove(&id);
                        Some(text)
                    }
                    _ => Some(text),
                }
            }
        })
        .boxed()
}

/// The `error` message for operation `id` carrying `error`.
fn error_frame(protocol: WebSocketProtocols, id: &str, error: Value) -> String {
    let payload = match protocol {
        WebSocketProtocols::GraphQLWS => json!([error]),
        _ => error,
    };
    json!({"type": "error", "id": id, "payload": payload}).to_string()
}

/// Why the operation of a `subscribe` (`start`) message is too large to
/// hand to the executor (see [`super::oversize`]).
fn subscribe_oversize(text: &str, rt: &MockRuntime) -> Option<super::Oversize> {
    let msg: Value = serde_json::from_str(text).ok()?;
    let payload = msg.get("payload")?;
    let query = payload.get("query")?.as_str()?;
    let operation = payload.get("operationName").and_then(Value::as_str);
    super::oversize(query, operation, rt.protocols.graphql_max_root_fields)
}

/// Reports an operation refused before it ran (by the per-connection cap,
/// `429`, or for its size, `400`) as a call.
async fn report_refused(
    rt: &MockRuntime,
    headers: &HeaderMap,
    status: u16,
    message: &str,
    code: &str,
) {
    let report = Report {
        operation_id: None,
        session: DEFAULT_SESSION.into(),
        method: "GET".into(),
        path: rt.protocols.graphql_path.clone(),
        status,
        source: None,
        fallback_reason: None,
        usage: Default::default(),
        latency: Default::default(),
        tenancy: None,
        admission_ran: false,
        request: if rt.recorder.is_some() {
            json!({"headers": recorded_headers(rt, headers)})
        } else {
            Value::Null
        },
        request_body_bytes: None,
        response: rt
            .recorder
            .is_some()
            .then(|| json!({"message": message, "extensions": {"code": code}})),
    };
    call::report(rt, headers, report).await;
}

/// `headers` with each string member of `payload` set as a header (names
/// or values that cannot be headers are skipped), twice: with every
/// member, for authentication, and without the credential members (the
/// mock's key header, `api_key`, `apiKey`, `authorization` and any other
/// credential-like name), for the prompt and the record. In the first,
/// an `api_key`/`apiKey` member also fills the key header when the
/// payload does not set it.
fn overlay(rt: &MockRuntime, headers: HeaderMap, payload: &Value) -> (HeaderMap, HeaderMap) {
    let mut auth = headers.clone();
    let mut public = headers;
    let Some(members) = payload.as_object() else {
        return (auth, public);
    };
    let names = rt.credential_names();
    for (name, value) in members {
        let Some(value) = value.as_str() else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        let (Ok(header), Ok(value)) = (
            HeaderName::from_bytes(lower.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        let credential = PAYLOAD_KEY_MEMBERS.contains(&name.as_str())
            || names.headers.contains(&lower)
            || call::is_sensitive(&lower);
        if !credential {
            public.insert(header.clone(), value.clone());
        }
        auth.insert(header, value);
    }
    if let Some(key_header) = names.key_header.as_deref()
        && !auth.contains_key(key_header)
        && let Some(key) = PAYLOAD_KEY_MEMBERS
            .iter()
            .find_map(|m| members.get(*m).and_then(Value::as_str))
        && let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(key_header.as_bytes()),
            HeaderValue::from_str(key),
        )
    {
        auth.insert(name, value);
    }
    (auth, public)
}

/// A subscription root field serving operation `op_id`, or answering an
/// error when the field has no operation.
pub(crate) fn field(
    rt: &Arc<MockRuntime>,
    f: &FieldDefinition,
    op_id: Option<&str>,
    shapes: &Arc<Shapes>,
) -> SubscriptionField {
    let name = f.name.node.to_string();
    let ty = f.ty.node.clone();
    let rt = Arc::clone(rt);
    let shapes = Arc::clone(shapes);
    let op_id = op_id.map(str::to_string);
    let field_name = name.clone();
    let mut out = SubscriptionField::new(name, type_ref(&ty), move |ctx| {
        let rt = Arc::clone(&rt);
        let shapes = Arc::clone(&shapes);
        let ty = ty.clone();
        let op_id = op_id.clone();
        let field_name = field_name.clone();
        SubscriptionFieldFuture::new(async move {
            let Some(op_id) = op_id else {
                return Err(Failure::new(
                    501,
                    format!("subscription {field_name} has no operation"),
                )
                .error);
            };
            let conn = Arc::clone(ctx.data::<Arc<WsCall>>()?);
            let args = arguments(&ctx);
            let selection = super::select::Selection::of(&ctx.ctx.field());
            let items = open(rt, op_id, ty, shapes, selection, conn, args).await?;
            Ok(items.map(|r| r.map(widen)))
        })
    });
    if let Some(d) = &f.description {
        out = out.description(d.node.clone());
    }
    for a in &f.arguments {
        out = out.argument(input_value(&a.node));
    }
    out
}

/// A `'static` field value as one borrowing for the resolver's lifetime.
fn widen<'a>(v: FieldValue<'static>) -> FieldValue<'a> {
    v
}

fn arguments(ctx: &ResolverContext<'_>) -> Map<String, Value> {
    ctx.args
        .as_index_map()
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone().into_json().unwrap_or(Value::Null)))
        .collect()
}

type Items = BoxStream<'static, async_graphql::Result<FieldValue<'static>>>;

/// Plans the call and opens its stream: one error for a failed plan or a
/// forced non-2xx status, else the generated items followed by the
/// session's changes, after any injected latency.
async fn open(
    rt: Arc<MockRuntime>,
    op_id: String,
    ty: Type,
    shapes: Arc<Shapes>,
    selection: super::select::Selection,
    conn: Arc<WsCall>,
    args: Map<String, Value>,
) -> async_graphql::Result<Items> {
    let span = tracing::info_span!(
        "mock.request",
        mock = %rt.name,
        operation = %op_id,
        session = Empty,
        status = Empty,
        source = Empty,
    );
    let Some(op) = rt.operation(&op_id).cloned() else {
        return Err(Failure::new(500, format!("unknown operation {op_id}")).error);
    };
    let mut report = ReportOnDrop {
        rt: Arc::clone(&rt),
        span: span.clone(),
        started: Instant::now(),
        operation_id: op_id,
        session: DEFAULT_SESSION.into(),
        tenancy: None,
        request: if rt.recorder.is_some() {
            json!({"args": args, "headers": recorded_headers(&rt, &conn.headers)})
        } else {
            Value::Null
        },
        headers: conn.headers.clone(),
        status: 200,
        error: None,
        items: 0,
        source: None,
    };
    // Each subscription is admitted and checked against the upgrade
    // request's headers overlaid by the `connection_init` payload; in
    // `api_key` mode the connection's key was verified once already.
    let presented = Presented {
        query: &conn.query,
        key_verified: conn.key_verified,
        admitted: None,
    };
    let plan = match call::plan(&rt, &op, &conn.auth_headers, presented)
        .instrument(span.clone())
        .await
    {
        Ok(plan) => plan,
        Err(failure) => {
            report.tenancy = failure.tenancy;
            if let Some(s) = failure.session {
                report.session = s;
            }
            let failure = Failure::plan(failure.error, &rt.name);
            report.fail(&failure);
            return Err(failure.error);
        }
    };
    report.tenancy = Some(plan.tenancy.clone());
    report.session = plan.session.session.clone();
    let delay = plan.delay;
    let sleep = async move {
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
    };

    let refused = if plan.chaos.inject_error {
        Some(Failure::chaos(plan.forced_status))
    } else {
        plan.forced_status
            .filter(|s| !(200..300).contains(s))
            .map(|status| Failure::new(status, forced_message(status)))
    };
    if let Some(failure) = refused {
        report.fail(&failure);
        let error = failure.error;
        return Ok(stream::once(async move {
            sleep.await;
            drop(report);
            Err(error)
        })
        .boxed());
    }

    // Subscribe now, so changes made while the stream opens are not missed.
    // A stored resource is sent when it has what the selection reads:
    // mutations store only what they were asked for.
    let check = op
        .stream_item_schema()
        .map(|schema| super::select::narrow(schema, &rt.bundle.defs, &selection))
        .and_then(|narrowed| mb_engine::SchemaValidator::new(&narrowed).ok())
        .map(|validator| {
            let selection = selection.clone();
            Arc::new(move |data: &Value| {
                validator
                    .validate(&super::select::rekey(data.clone(), &selection))
                    .is_ok()
            }) as call::ItemCheck
        });
    let feed = call::change_feed(&rt, plan.session.clone(), &op, check).map(Piece::Changed);
    let call = OperationCall {
        operation_id: op.id.clone(),
        headers: forwarded_headers(&rt, &conn.headers),
        body: Some(Value::Object(args)),
        ..Default::default()
    };
    let generated = generated(Arc::clone(&rt), &plan, op.clone(), call);
    // Open until the client stops it or the process shuts down.
    let merged = stream::select_with_strategy(generated, feed, |_: &mut ()| PollNext::Left)
        .chain(stream::pending())
        .take_until(rt.shutdown.stopped());
    let pieces = stream::once(async move {
        sleep.await;
        merged
    })
    .flatten()
    .boxed();
    let state = Subscription {
        pieces,
        report,
        ty,
        shapes,
        selection,
        dropped: false,
    };
    Ok(stream::unfold(state, |mut st| async move {
        if st.dropped {
            return None;
        }
        let span = st.report.span.clone();
        let piece = st.pieces.next().instrument(span).await?;
        let out = st.render(piece);
        Some((out, st))
    })
    .boxed())
}

/// One thing to send to the client.
enum Piece {
    /// A generated stream item.
    Item(StreamItem),
    /// A resource upserted in the session.
    Changed(Value),
    /// The engine stream could not be opened.
    Failed(String),
    /// Chaos drops the subscription here.
    Dropped,
}

/// The engine's items for this call, opened when first polled (after any
/// latency), followed by [`Piece::Dropped`] after `plan.chaos.drop_after`
/// items; generation runs only while the subscription polls it.
fn generated(
    rt: Arc<MockRuntime>,
    plan: &CallPlan<'_>,
    op: Operation,
    call: OperationCall,
) -> BoxStream<'static, Piece> {
    let session = plan.session.clone();
    let scenario = plan.scenario.cloned();
    let stream_items = plan.stream_items;
    let tenancy = plan.tenancy.clone();
    let engine = plan.engine.clone();
    let drop_after = plan.chaos.drop_after;
    let items = stream::once(async move {
        let plan = CallPlan {
            op: &op,
            session,
            scenario: scenario.as_ref(),
            forced_status: None,
            delay: None,
            stream_items,
            tenancy,
            engine,
            chaos: Default::default(),
        };
        match call::open_stream(&rt, &plan, call, Vec::new()).await {
            Ok(items) => items.map(Piece::Item).boxed(),
            Err(e) => {
                tracing::warn!(operation = %op.id, error = %e, "cannot open subscription stream");
                stream::iter([Piece::Failed(e.to_string())]).boxed()
            }
        }
    })
    .flatten();
    match drop_after {
        Some(n) => items.take(n).chain(stream::iter([Piece::Dropped])).boxed(),
        None => items.boxed(),
    }
}

/// A running subscription: what is left to send and what to report.
struct Subscription {
    pieces: BoxStream<'static, Piece>,
    report: ReportOnDrop,
    /// The field's type, to project each item.
    ty: Type,
    shapes: Arc<Shapes>,
    /// The field's selection, to re-key each item (keyed by field name).
    selection: super::select::Selection,
    /// Chaos dropped the subscription: nothing more is sent.
    dropped: bool,
}

impl Subscription {
    fn render(&mut self, piece: Piece) -> async_graphql::Result<FieldValue<'static>> {
        let value = match piece {
            Piece::Item(item) => {
                self.report.source = Some(match self.report.source {
                    Some(s) => s.worst(item.source),
                    None => item.source,
                });
                item.value
            }
            Piece::Changed(value) => value,
            Piece::Failed(detail) => {
                let failure = Failure::new(500, detail);
                self.report.fail(&failure);
                return Err(failure.error);
            }
            Piece::Dropped => {
                self.dropped = true;
                crate::chaos::count(&self.report.rt.name, "drop");
                let mut failure = Failure::unavailable(crate::chaos::STREAM_DROPPED);
                failure.error = failure.error.extend_with(|_, e| e.set("chaos", "drop"));
                self.report.fail(&failure);
                return Err(failure.error);
            }
        };
        self.report.items += 1;
        let value = super::select::rekey(value, &self.selection);
        Ok(project(value, &self.ty, &self.shapes).unwrap_or(FieldValue::NULL))
    }
}

/// Reports the subscription once, when it is dropped (completed by the
/// client, the connection closed, or an error ended it).
struct ReportOnDrop {
    rt: Arc<MockRuntime>,
    span: Span,
    started: Instant,
    operation_id: String,
    session: String,
    tenancy: Option<Tenancy>,
    /// `{args, headers}`; `Null` without a recorder.
    request: Value,
    headers: HeaderMap,
    status: u16,
    /// The error sent, as `{message, extensions}`.
    error: Option<Value>,
    /// Items sent so far.
    items: usize,
    /// The worst source over the generated items sent.
    source: Option<Source>,
}

impl ReportOnDrop {
    fn fail(&mut self, failure: &Failure) {
        self.status = failure.status;
        self.error = Some(json!({
            "message": failure.error.message,
            "extensions": failure.error.extensions,
        }));
    }
}

impl Drop for ReportOnDrop {
    fn drop(&mut self) {
        self.span.record("status", self.status);
        if let Some(source) = self.source {
            self.span.record("source", source.as_str());
        }
        let response = match self.error.take() {
            Some(error) if self.items == 0 => error,
            _ => json!({"items": self.items}),
        };
        let report = Report {
            operation_id: Some(std::mem::take(&mut self.operation_id)),
            session: std::mem::take(&mut self.session),
            method: "GET".into(),
            path: self.rt.protocols.graphql_path.clone(),
            status: self.status,
            source: self.source,
            fallback_reason: None,
            usage: Default::default(),
            latency: self.started.elapsed(),
            tenancy: self.tenancy.take(),
            admission_ran: true,
            request: self.request.take(),
            request_body_bytes: None,
            response: self.rt.recorder.is_some().then_some(response),
        };
        let rt = Arc::clone(&self.rt);
        let headers = std::mem::take(&mut self.headers);
        let span = self.span.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(
                    async move { call::report(&rt, &headers, report).instrument(span).await },
                );
            }
            Err(_) => tracing::warn!(mock = %rt.name, "subscription not reported: no runtime"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `Value`-based reading `operation_message` replaces.
    fn by_value(text: &str) -> Option<(String, String)> {
        let msg: Value = serde_json::from_str(text).ok()?;
        let ty = msg.get("type")?.as_str()?.to_string();
        let id = msg.get("id")?.as_str()?.to_string();
        Some((ty, id))
    }

    #[test]
    fn operation_message_reads_type_and_id_like_a_value() {
        let frames = [
            r#"{"type":"next","id":"1","payload":{"data":{"a":[1,2,{"b":null}]}}}"#,
            r#"{"id":"op\"2","type":"complete"}"#,
            r#"{"type":"connection_ack"}"#,
            r#"{"type":"next","id":7}"#,
            r#"{"type":null,"id":"1"}"#,
            r#"{"type":"next","id":"1","type":"error"}"#,
            r#"{"type":"next","id":"1","id":2}"#,
            r#"{"type":"next","id":"1"} trailing"#,
            r#"["type","id"]"#,
            r#""type""#,
            "not json",
            "",
        ];
        for frame in frames {
            assert_eq!(operation_message(frame), by_value(frame), "{frame}");
        }
        assert_eq!(
            operation_message(frames[1]),
            Some(("complete".into(), "op\"2".into()))
        );
    }

    /// Timing harness: `cargo test -p mockingbird-protocols --lib --
    /// --ignored --nocapture operation_message_timing`.
    #[test]
    #[ignore = "timing harness"]
    fn operation_message_timing() {
        for items in [100usize, 1000] {
            let rows: Vec<Value> = (0..items)
                .map(|i| json!({"id": i, "title": "an issue title", "labels": ["a", "b"]}))
                .collect();
            let frame =
                json!({"type": "next", "id": "1", "payload": {"data": {"rows": rows}}}).to_string();
            let n = 2000u32;
            let started = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(by_value(std::hint::black_box(&frame)));
            }
            let before = started.elapsed() / n;
            let started = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(operation_message(std::hint::black_box(&frame)));
            }
            let after = started.elapsed() / n;
            println!(
                "frame of {} bytes: Value {before:?}, head only {after:?}",
                frame.len()
            );
        }
    }
}
