//! Server-sent events: REST operations of kind `StreamServer` answer
//! `text/event-stream`.
//!
//! The engine's stream items come first; resources of the operation's type
//! upserted in the same session are interleaved as they arrive. The stream
//! stays open until the client disconnects or the process shuts down, and the call is reported once,
//! when the response body is dropped. A stream chaos drops ends with a
//! body error after its planned number of generated items, which aborts
//! the connection without a final event.

use std::sync::Arc;

use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use futures::stream::{self, BoxStream, PollNext};
use mb_engine::{OperationCall, Source, StreamItem};
use mb_ext::Tenancy;
use mb_mcm::{Binding, Operation};
use serde_json::{Map, Value, json};
use tokio::time::Instant;
use tracing::{Instrument, Span};

use super::handler;
use crate::MockRuntime;
use crate::call::{self, CallPlan, Report, recorded_headers};

/// Item schema extension naming the SSE `event:` field.
const SSE_EVENT_EXT: &str = "x-mockingbird-sse-event";
/// The event name when the contract names none.
const DEFAULT_EVENT: &str = "message";
const KEEPALIVE_TEXT: &str = "keepalive";

/// One thing to send to the client.
enum Piece {
    /// A generated stream item.
    Item(StreamItem),
    /// A resource upserted in the session.
    Changed(Value),
    /// The engine stream could not be opened.
    Failed(String),
    /// Chaos drops the stream here.
    Dropped,
}

/// A `200 text/event-stream` response streaming `plan.op`'s items, with a
/// keepalive comment every `rt.protocols.sse_keepalive`. Any injected
/// latency is slept before the first event. Must be called inside the
/// call's `mock.request` span.
pub(crate) fn sse_response(
    rt: Arc<MockRuntime>,
    plan: CallPlan<'_>,
    call: OperationCall,
    headers: HeaderMap,
) -> Response {
    let op = plan.op.clone();
    let event = event_name(&op);
    let keepalive = KeepAlive::new()
        .interval(rt.protocols.sse_keepalive)
        .text(KEEPALIVE_TEXT);
    // Subscribe now, so changes made while the stream opens are not missed.
    let feed = call::change_feed(&rt, plan.session.clone(), &op, None).map(Piece::Changed);
    let report = ReportOnDrop {
        rt: Arc::clone(&rt),
        span: Span::current(),
        started: Instant::now(),
        operation_id: op.id.clone(),
        session: plan.session.session.clone(),
        method: method_name(&op),
        path: concrete_path(&op, &call.path_params),
        tenancy: Some(plan.tenancy.clone()),
        request: if rt.recorder.is_some() {
            json!({
                "query": call.query,
                "headers": recorded_headers(&rt, &headers),
                "body": call.body,
            })
        } else {
            Value::Null
        },
        headers,
        items: 0,
        source: None,
    };
    let delay = plan.delay;
    let stopped = rt.shutdown.stopped();
    let generated = generated(rt, &plan, op, call);
    // A generated item whenever one is ready; changes in between. Open
    // until the client disconnects or the process shuts down.
    let merged = stream::select_with_strategy(generated, feed, |_: &mut ()| PollNext::Left)
        .chain(stream::pending())
        .take_until(stopped);
    let pieces = stream::once(async move {
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        merged
    })
    .flatten()
    .boxed();

    let body = Body {
        pieces,
        event,
        report,
        dropped: false,
    };
    let events = stream::unfold(body, |mut body| async move {
        if body.dropped {
            return None;
        }
        let span = body.report.span.clone();
        let piece = body.pieces.next().instrument(span).await?;
        if let Piece::Dropped = piece {
            // Let the server flush what was sent (at least the response
            // head) before the error aborts the connection.
            tokio::task::yield_now().await;
        }
        let event = body.render(piece);
        Some((event, body))
    });
    Sse::new(events).keep_alive(keepalive).into_response()
}

/// The engine's items for this call, opened lazily when first polled
/// (after any latency), followed by [`Piece::Dropped`] after
/// `plan.chaos.drop_after` items. The stream reads state and generates
/// only while the response body polls it.
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
            chaos: Default::default(),
        };
        match call::open_stream(&rt, &plan, call, Vec::new()).await {
            Ok(items) => items.map(Piece::Item).boxed(),
            Err(e) => {
                tracing::warn!(operation = %op.id, error = %e, "cannot open SSE stream");
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

/// The response body's state: what is left to send and what to report.
struct Body {
    pieces: BoxStream<'static, Piece>,
    /// The `event:` name of every item.
    event: String,
    report: ReportOnDrop,
    /// Chaos dropped the stream: nothing more is sent.
    dropped: bool,
}

impl Body {
    /// An SSE event for `piece`: `event`, the 1-based `id` and the item as
    /// compact JSON `data`; an error ending the body for a chaos drop.
    fn render(&mut self, piece: Piece) -> Result<Event, std::io::Error> {
        let data = match piece {
            Piece::Item(item) => {
                self.report.source = Some(match self.report.source {
                    Some(s) => s.worst(item.source),
                    None => item.source,
                });
                item.value
            }
            Piece::Changed(value) => value,
            Piece::Failed(detail) => {
                let problem = json!({"title": "Generation failed", "detail": detail});
                return Ok(Event::default().event("error").data(problem.to_string()));
            }
            Piece::Dropped => {
                self.dropped = true;
                crate::chaos::count(&self.report.rt.name, "drop");
                return Err(std::io::Error::other(crate::chaos::STREAM_DROPPED));
            }
        };
        self.report.items += 1;
        Ok(Event::default()
            .event(&self.event)
            .id(self.report.items.to_string())
            .data(data.to_string()))
    }
}

/// Reports the call once, when the response body is dropped (the client
/// disconnected or the server shut down).
struct ReportOnDrop {
    rt: Arc<MockRuntime>,
    span: Span,
    started: Instant,
    operation_id: String,
    session: String,
    method: String,
    path: String,
    tenancy: Option<Tenancy>,
    /// `{query, headers, body}`; `Null` without a recorder.
    request: Value,
    headers: HeaderMap,
    /// Events sent so far.
    items: usize,
    /// The worst source over the generated items sent.
    source: Option<Source>,
}

impl Drop for ReportOnDrop {
    fn drop(&mut self) {
        if let Some(source) = self.source {
            self.span.record("source", source.as_str());
        }
        let report = Report {
            operation_id: Some(std::mem::take(&mut self.operation_id)),
            session: std::mem::take(&mut self.session),
            method: std::mem::take(&mut self.method),
            path: std::mem::take(&mut self.path),
            status: 200,
            source: self.source,
            fallback_reason: None,
            usage: Default::default(),
            latency: self.started.elapsed(),
            tenancy: self.tenancy.take(),
            admission_ran: true,
            request: self.request.take(),
            request_body_bytes: None,
            response: Some(json!({"items": self.items})),
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
            Err(_) => tracing::warn!(mock = %rt.name, "SSE call not reported: no runtime"),
        }
    }
}

/// The item schema's `x-mockingbird-sse-event`, else `message`. A name
/// that cannot be an SSE field value (it has a line break) is ignored.
fn event_name(op: &Operation) -> String {
    op.stream_item_schema()
        .and_then(|s| s.get(SSE_EVENT_EXT))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty() && !name.contains(['\r', '\n']))
        .unwrap_or(DEFAULT_EVENT)
        .to_string()
}

fn method_name(op: &Operation) -> String {
    match &op.binding {
        Binding::Http { method, .. } => handler::method_name(*method).to_string(),
        _ => String::new(),
    }
}

/// The request path: the operation's path template with its parameters
/// filled in.
fn concrete_path(op: &Operation, params: &Map<String, Value>) -> String {
    let Binding::Http { path, .. } = &op.binding else {
        return String::new();
    };
    params.iter().fold(path.clone(), |path, (name, value)| {
        let value = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        path.replace(&format!("{{{name}}}"), &value)
    })
}
