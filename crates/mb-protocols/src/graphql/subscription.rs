//! Subscriptions over WebSocket (`graphql-transport-ws` and the legacy
//! `graphql-ws`).
//!
//! A subscription root field streams its operation's engine items, then
//! the resources of its type upserted in the same session, each projected
//! like a query result. It runs until the client completes it or
//! disconnects, or the process shuts down, and is reported once, when its
//! stream is dropped.

use std::sync::Arc;

use async_graphql::Data;
use async_graphql::dynamic::{
    FieldValue, ResolverContext, Schema, SubscriptionField, SubscriptionFieldFuture,
};
use async_graphql::http::{WebSocket, WebSocketProtocols, WsMessage};
use async_graphql::parser::types::{FieldDefinition, Type};
use axum::extract::WebSocketUpgrade;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
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

use super::MAX_BODY_BYTES;
use super::resolve::{Failure, Shapes, forced_message, project};
use super::schema::{input_value, type_ref};
use crate::MockRuntime;
use crate::call::{self, CallPlan, DEFAULT_SESSION, Report, forwarded_headers, recorded_headers};
use crate::ws::{ChannelConfig, ChannelIo, Outbound, serve_channel};

/// The subprotocols served, in order of preference.
const SUBPROTOCOLS: [&str; 2] = ["graphql-transport-ws", "graphql-ws"];

/// One WebSocket connection's override headers, shared by its
/// subscriptions through the connection data.
struct WsCall {
    headers: HeaderMap,
}

/// Upgrades a `GET` at the GraphQL path to a subscription channel. The
/// override headers are `headers` (the upgrade request's), overlaid by
/// the string-valued keys of the `connection_init` payload.
pub(crate) fn upgrade(
    schema: Schema,
    rt: &MockRuntime,
    upgrade: WebSocketUpgrade,
    headers: HeaderMap,
) -> Response {
    let cfg = ChannelConfig {
        subprotocols: SUBPROTOCOLS.to_vec(),
        keepalive: rt.protocols.ws_keepalive,
        max_message_bytes: MAX_BODY_BYTES,
    };
    serve_channel(upgrade, cfg, move |io| run(schema, io, headers))
}

async fn run(schema: Schema, io: ChannelIo, headers: HeaderMap) {
    // Without a negotiated subprotocol, speak the current one.
    let protocol = io
        .protocol
        .as_deref()
        .and_then(|p| p.parse().ok())
        .unwrap_or(WebSocketProtocols::GraphQLWS);
    let messages = WebSocket::new(schema, io.incoming, protocol).on_connection_init(
        move |payload| async move {
            let mut data = Data::default();
            data.insert(Arc::new(WsCall {
                headers: overlay(headers, &payload),
            }));
            Ok(data)
        },
    );
    futures::pin_mut!(messages);
    while let Some(message) = messages.next().await {
        let (out, last) = match message {
            WsMessage::Text(text) => (Outbound::Text(text), false),
            WsMessage::Close(code, reason) => (Outbound::Close { code, reason }, true),
        };
        if io.outgoing.send(out).await.is_err() || last {
            break;
        }
    }
}

/// `headers` with each string member of `payload` set as a header; names
/// or values that cannot be headers are skipped.
fn overlay(mut headers: HeaderMap, payload: &Value) -> HeaderMap {
    let Some(members) = payload.as_object() else {
        return headers;
    };
    for (name, value) in members {
        let Some(value) = value.as_str() else {
            continue;
        };
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        headers.insert(name, value);
    }
    headers
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
            let headers = ctx.data::<Arc<WsCall>>()?.headers.clone();
            let args = arguments(&ctx);
            let items = open(rt, op_id, ty, shapes, headers, args).await?;
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
    headers: HeaderMap,
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
    let Some(op) = rt.bundle.operation(&op_id).cloned() else {
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
            json!({"args": args, "headers": recorded_headers(&headers)})
        } else {
            Value::Null
        },
        headers,
        status: 200,
        error: None,
        items: 0,
        source: None,
    };
    let plan = match call::plan(&rt, &op, &report.headers)
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

    if let Some(status) = plan.forced_status.filter(|s| !(200..300).contains(s)) {
        let failure = Failure::new(status, forced_message(status));
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
    let feed = call::change_feed(&rt, plan.session.clone(), &op).map(Piece::Changed);
    let call = OperationCall {
        operation_id: op.id.clone(),
        headers: forwarded_headers(&report.headers),
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
    };
    Ok(stream::unfold(state, |mut st| async move {
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
}

/// The engine's items for this call, opened when first polled (after any
/// latency); generation runs only while the subscription polls it.
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
    stream::once(async move {
        let plan = CallPlan {
            op: &op,
            session,
            scenario: scenario.as_ref(),
            forced_status: None,
            delay: None,
            stream_items,
            tenancy,
        };
        match call::open_stream(&rt, &plan, call, Vec::new()).await {
            Ok(items) => items.map(Piece::Item).boxed(),
            Err(e) => {
                tracing::warn!(operation = %op.id, error = %e, "cannot open subscription stream");
                stream::iter([Piece::Failed(e.to_string())]).boxed()
            }
        }
    })
    .flatten()
    .boxed()
}

/// A running subscription: what is left to send and what to report.
struct Subscription {
    pieces: BoxStream<'static, Piece>,
    report: ReportOnDrop,
    /// The field's type, to project each item.
    ty: Type,
    shapes: Arc<Shapes>,
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
        };
        self.report.items += 1;
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
