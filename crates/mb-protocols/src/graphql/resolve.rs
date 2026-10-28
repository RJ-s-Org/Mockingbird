//! Field resolvers: root fields call the engine through the shared
//! [`call`](crate::call) pipeline; nested fields project from their
//! parent's generated JSON.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_graphql::dynamic::{FieldValue, ResolverContext};
use async_graphql::parser::types::{BaseType, Type};
use async_graphql::{Error, ErrorExtensions, Name};
use axum::http::{HeaderMap, StatusCode};
use mb_engine::{EngineError, OperationCall, Source};
use mb_ext::{Rejection, Tenancy};
use mb_mcm::OperationKind;
use serde_json::{Map, Value, json};
use tokio::time::Instant;
use tracing::Instrument;
use tracing::field::Empty;

use super::error_code;
use crate::MockRuntime;
use crate::call::{
    self, DEFAULT_SESSION, PlanError, PlanFailure, Presented, Report, forwarded_headers,
    recorded_headers,
};

/// How a named type's values are turned into field values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// Built-in or custom scalar: the JSON value as is.
    Scalar,
    /// An enum value, from a JSON string.
    Enum,
    /// An object: nested resolvers project from the JSON object.
    Object,
    /// An interface or union: an object tagged with its `__typename`.
    Abstract,
}

/// Every named type's [`Shape`]; built-in scalars are absent.
pub(crate) type Shapes = HashMap<String, Shape>;

/// One HTTP request being executed, shared by its root resolvers through
/// the request data; or one query or mutation root field sent over a
/// subscription WebSocket (see [`super::subscription::http_call`]).
pub(crate) struct HttpCall {
    /// The headers forwarded, recorded and reported.
    headers: HeaderMap,
    /// The headers the call is admitted and authenticated with, when they
    /// differ from `headers` (a WebSocket's `connection_init` credentials).
    auth_headers: Option<HeaderMap>,
    /// The raw query pairs (a WebSocket upgrade URL's), which may carry a
    /// key; empty over HTTP.
    query: Vec<(String, String)>,
    /// The data-plane key was already checked: once for the whole HTTP
    /// request, or once for the WebSocket connection in `api_key` mode.
    key_verified: bool,
    /// `POST` or `GET`.
    method: String,
    /// The request's variables, for the request record.
    variables: Value,
    /// The tenancy the whole request was admitted with, in `api_key` mode
    /// (admission runs before the key check); root fields are then not
    /// admitted again.
    admitted: Option<Tenancy>,
    /// Whether the root fields are recorded as one request (an HTTP
    /// request) rather than one each (WebSocket operations).
    one_record: bool,
    collected: Mutex<Collected>,
    /// The query's shared LLM call, set up by the first root resolver.
    pub(crate) batch: std::sync::OnceLock<Option<Arc<super::batch::Batch>>>,
}

/// What the root resolvers learned, read once execution is done.
#[derive(Default)]
pub(crate) struct Collected {
    /// Response key → `llm`, `repaired` or `fallback`.
    pub sources: Map<String, Value>,
    /// The worst source over the generated root fields.
    pub worst: Option<Source>,
    /// Response key → why the faker answered that root field.
    pub fallback_reasons: Map<String, Value>,
    /// The largest injected latency among the root fields.
    pub delay: Option<Duration>,
    /// Over HTTP, each root field's report (by response key), recorded as
    /// one request once execution is done.
    pub reports: Vec<(String, Report)>,
}

impl HttpCall {
    pub(crate) fn new(
        headers: HeaderMap,
        method: String,
        variables: Value,
        admitted: Option<Tenancy>,
    ) -> Self {
        Self {
            headers,
            auth_headers: None,
            query: Vec::new(),
            key_verified: true,
            method,
            variables,
            admitted,
            one_record: true,
            collected: Mutex::new(Collected::default()),
            batch: std::sync::OnceLock::new(),
        }
    }

    /// A call over a WebSocket: admitted per root field (no whole-request
    /// admission), authenticated with `auth_headers` and `query` unless
    /// the connection's key was `key_verified`.
    pub(crate) fn websocket(
        headers: HeaderMap,
        auth_headers: HeaderMap,
        query: Vec<(String, String)>,
        key_verified: bool,
        variables: Value,
    ) -> Self {
        Self {
            auth_headers: Some(auth_headers),
            query,
            key_verified,
            one_record: false,
            ..Self::new(headers, "GET".into(), variables, None)
        }
    }

    fn lock(&self) -> MutexGuard<'_, Collected> {
        self.collected.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Takes what the root resolvers collected.
    pub(crate) fn collected(&self) -> Collected {
        std::mem::take(&mut *self.lock())
    }

    fn add_delay(&self, delay: Option<Duration>) {
        if let Some(d) = delay {
            let mut c = self.lock();
            c.delay = Some(c.delay.map_or(d, |old| old.max(d)));
        }
    }

    fn add_source(&self, key: String, source: Source, fallback_reason: Option<&str>) {
        let mut c = self.lock();
        if let Some(reason) = fallback_reason {
            c.fallback_reasons.insert(key.clone(), json!(reason));
        }
        c.sources.insert(key, json!(source.as_str()));
        c.worst = Some(c.worst.map_or(source, |w| w.worst(source)));
    }
}

/// How a root field that will generate gets its answer.
enum Route<'a, 'b> {
    /// With the query's other root fields, in one LLM call.
    Join(&'b super::batch::Batch, call::Pending<'a>),
    /// Without generating: an unknown id, a stored resource, a cached answer.
    Answered(mb_engine::GeneratedResponse),
    /// On its own.
    Alone,
}

/// A root field failure: the HTTP status it stands for and the field error.
pub(crate) struct Failure {
    pub(crate) status: u16,
    pub(crate) error: Error,
}

impl Failure {
    pub(crate) fn new(status: u16, message: impl Into<String>) -> Self {
        let error = Error::new(message).extend_with(|_, e| {
            e.set("code", error_code(status));
            e.set("status", i32::from(status));
        });
        Self { status, error }
    }

    /// The field error for a failed [`call::plan`] of mock `mock`.
    pub(crate) fn plan(error: PlanError, mock: &str) -> Self {
        match error {
            PlanError::Rejected(r) | PlanError::RateLimited(r) => Self::rejected(r),
            PlanError::Unauthorized(u) => Self::new(401, u.detail),
            PlanError::BadOverride(detail) => Self::new(400, detail),
            PlanError::UnknownScenario(name) => {
                Self::new(400, format!("mock {mock} has no scenario {name:?}"))
            }
        }
    }

    /// The field error of an error injected by chaos: the declared
    /// `status` when the operation has one, else `UNAVAILABLE` (503).
    /// Carries `extensions.chaos = "error"`.
    pub(crate) fn chaos(status: Option<u16>) -> Self {
        let detail = crate::chaos::CHAOS_ERROR_DETAIL;
        let mut failure = match status {
            Some(status) => {
                let reason = canonical_reason(status).unwrap_or("Error");
                Self::new(status, format!("{reason} ({detail})"))
            }
            None => Self::unavailable(format!("Service Unavailable ({detail})")),
        };
        failure.error = failure.error.extend_with(|_, e| e.set("chaos", "error"));
        failure
    }

    /// An `UNAVAILABLE` field error (HTTP 503).
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        let error = Error::new(message).extend_with(|_, e| {
            e.set("code", "UNAVAILABLE");
            e.set("status", 503);
        });
        Self { status: 503, error }
    }

    fn rejected(r: Rejection) -> Self {
        let mut failure = Self::new(r.status, r.detail);
        if let Some(after) = r.retry_after {
            let secs = after.as_secs() + u64::from(after.subsec_nanos() > 0);
            failure.error = failure.error.extend_with(|_, e| e.set("retryAfter", secs));
        }
        failure
    }
}

/// Resolves a root field of operation `op_id`: plans the call, generates
/// one response (unless a non-2xx status is forced or planning fails),
/// reports it and projects the result.
pub(crate) async fn root(
    rt: &MockRuntime,
    op_id: &str,
    ty: &Type,
    shapes: &Shapes,
    ctx: &ResolverContext<'_>,
) -> async_graphql::Result<Option<FieldValue<'static>>> {
    // Over a subscription WebSocket there is no HTTP request: the field
    // is its own call, which sleeps its own injected latency.
    let (http, websocket) = match ctx.data_opt::<Arc<HttpCall>>() {
        Some(http) => (Arc::clone(http), false),
        None => (Arc::new(super::subscription::http_call(ctx)?), true),
    };
    let key = ctx.ctx.item.node.response_key().node.to_string();
    let span = tracing::info_span!(
        "mock.request",
        mock = %rt.name,
        operation = %op_id,
        session = Empty,
        status = Empty,
        source = Empty,
    );
    let args: Map<String, Value> = span.in_scope(|| {
        tracing::info_span!("mb.decode").in_scope(|| {
            ctx.args
                .as_index_map()
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone().into_json().unwrap_or(Value::Null)))
                .collect()
        })
    });
    let selection = super::select::Selection::of(&ctx.ctx.field());
    // Over HTTP, a query's root fields share one LLM call.
    let batch = if websocket {
        None
    } else {
        http.batch
            .get_or_init(|| super::batch::Batch::for_query(ctx.ctx))
            .clone()
    };
    let result = generate(rt, op_id, &http, key, args, &selection, batch.as_deref())
        .instrument(span.clone())
        .await;
    if websocket && let Some(delay) = http.collected().delay {
        tokio::time::sleep(delay).await;
    }
    match result {
        Ok(body) => Ok(span
            .in_scope(|| tracing::info_span!("mb.encode").in_scope(|| project(body, ty, shapes)))),
        // A nullable field answers null and reports the error at its path;
        // the dynamic executor would otherwise drop the field from `data`
        // and the error's path.
        Err(error) if ty.nullable => {
            let error = error.into_server_error(ctx.ctx.item.pos);
            ctx.ctx.add_error(ctx.ctx.set_error_path(error));
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

async fn generate(
    rt: &MockRuntime,
    op_id: &str,
    http: &HttpCall,
    key: String,
    args: Map<String, Value>,
    selection: &super::select::Selection,
    batch: Option<&super::batch::Batch>,
) -> Result<Value, Error> {
    let mut batch = batch;
    let result = generate_in(rt, op_id, http, key, args, selection, &mut batch).await;
    // A root field that did not join the batch must not hold it up.
    if let Some(batch) = batch {
        batch.withdraw();
    }
    result
}

/// [`generate`]; takes `batch` (leaving `None`) when it joins it.
async fn generate_in(
    rt: &MockRuntime,
    op_id: &str,
    http: &HttpCall,
    key: String,
    args: Map<String, Value>,
    selection: &super::select::Selection,
    batch: &mut Option<&super::batch::Batch>,
) -> Result<Value, Error> {
    let started = Instant::now();
    let Some(op) = rt.bundle.operation(op_id) else {
        return Err(Failure::new(500, format!("unknown operation {op_id}")).error);
    };
    let mut report = Report {
        operation_id: Some(op_id.to_string()),
        session: DEFAULT_SESSION.into(),
        method: http.method.clone(),
        path: rt.protocols.graphql_path.clone(),
        status: 200,
        source: None,
        fallback_reason: None,
        usage: Default::default(),
        latency: Default::default(),
        tenancy: None,
        admission_ran: true,
        request: if rt.recorder.is_some() {
            json!({
                "variables": http.variables,
                "args": args,
                "headers": recorded_headers(rt, &http.headers),
            })
        } else {
            Value::Null
        },
        request_body_bytes: None,
        response: None,
    };
    let mut delay = None;

    // Over HTTP the data-plane key was checked once for the whole request
    // (after admitting it, in `api_key` mode).
    let presented = Presented {
        query: &http.query,
        key_verified: http.key_verified,
        admitted: http.admitted.as_ref(),
    };
    let auth_headers = http.auth_headers.as_ref().unwrap_or(&http.headers);
    let outcome = match call::plan(rt, op, auth_headers, presented).await {
        Err(PlanFailure {
            error,
            tenancy,
            session,
        }) => {
            report.tenancy = tenancy;
            if let Some(s) = session {
                report.session = s;
            }
            Err(Failure::plan(error, &rt.name))
        }
        Ok(plan) => {
            report.tenancy = Some(plan.tenancy.clone());
            report.session = plan.session.session.clone();
            delay = plan.delay;
            match plan.forced_status.filter(|s| !(200..300).contains(s)) {
                _ if plan.chaos.inject_error => Err(Failure::chaos(plan.forced_status)),
                Some(status) => Err(Failure::new(status, forced_message(status))),
                None => {
                    let call = OperationCall {
                        operation_id: op.id.clone(),
                        headers: forwarded_headers(rt, &http.headers),
                        body: Some(Value::Object(args)),
                        ..Default::default()
                    };
                    // Only the selected fields are generated, mutations
                    // included: a whole return type (GitHub's `Issue`) is
                    // tens of thousands of prompt tokens and an answer no
                    // model finishes. What the answer holds is stored below.
                    let narrowed = op
                        .success_output()
                        .and_then(|o| o.schema.as_ref())
                        .map(|schema| super::select::narrow(schema, &rt.bundle.defs, selection));
                    let was_narrowed = narrowed.is_some();
                    // The engine overlays a read's stored target by field
                    // name, which an aliased selection must re-key (unless
                    // a scenario's `set`, applied after it, wins).
                    let rekeys_target = was_narrowed
                        && op.kind == OperationKind::Query
                        && plan.scenario.is_none()
                        && selection.aliased();
                    let mut stored = None;
                    let engine_failure = |e: EngineError| match e {
                        EngineError::UndeclaredStatus(code) => Failure::new(
                            400,
                            format!("operation {} declares no response for status {code}", op.id),
                        ),
                        e => Failure::new(500, e.to_string()),
                    };
                    // A root field of a batch reads its state, cache and
                    // stored resource like any call; only what is left to
                    // generate joins the shared LLM call, and its answer
                    // is seeded and cached per field.
                    let route = match batch.take() {
                        Some(b) => match call::prepare(rt, &plan, &call, narrowed.as_ref()).await {
                            call::Prepared::Answered(answer) => {
                                // Not generating: give up the batch place.
                                *batch = Some(b);
                                Route::Answered(answer)
                            }
                            call::Prepared::Pending(pending) => Route::Join(b, pending),
                        },
                        None => Route::Alone,
                    };
                    if rekeys_target {
                        stored = match &route {
                            Route::Join(_, pending) => {
                                pending.target.as_ref().map(|t| t.data.clone())
                            }
                            _ => call::read_state(rt, &plan, &call).await.0.map(|t| t.data),
                        };
                    }
                    let generated = match route {
                        Route::Join(b, pending) => {
                            let part = super::batch::Part {
                                key: key.clone(),
                                op_id: op.id.clone(),
                                call: call.clone(),
                                schema: narrowed.clone(),
                                target: pending.target.clone(),
                                related: pending.related.clone(),
                                scenario: plan.scenario.map(|s| s.spec.clone()),
                            };
                            match b.join(rt, part).await {
                                Some(Ok(joined)) => {
                                    Ok(call::complete(rt, &plan, &call, pending, joined).await)
                                }
                                Some(Err(reason)) => Err(Failure::new(500, reason)),
                                None => call::generate_pending(
                                    rt,
                                    &plan,
                                    &call,
                                    pending,
                                    narrowed.as_ref(),
                                )
                                .await
                                .map_err(engine_failure),
                            }
                        }
                        Route::Answered(answer) => Ok(answer),
                        Route::Alone => {
                            call::generate_narrowed(rt, &plan, &call, narrowed.as_ref())
                                .await
                                .map_err(engine_failure)
                        }
                    };
                    let generated = match generated {
                        Ok(generated) => {
                            store_answer(rt, &plan, op, selection, &call, &generated).await;
                            Ok(generated)
                        }
                        Err(e) => Err(e),
                    };
                    generated.map(|generated| {
                        report.usage = generated.usage;
                        report.source = Some(generated.source);
                        report.fallback_reason = generated.fallback_reason;
                        let body = generated.body.unwrap_or(Value::Null);
                        // A narrowed answer is keyed by response key; a full
                        // one by field name.
                        if was_narrowed {
                            let mut body = body;
                            let identity = op.identity.as_ref().map(|i| i.field.as_str());
                            super::select::settle(&mut body, selection, stored.as_ref(), identity);
                            body
                        } else {
                            super::select::rekey(body, selection)
                        }
                    })
                }
            }
        }
    };

    let span = tracing::Span::current();
    let record_key = key.clone();
    match &outcome {
        Ok(body) => {
            if let Some(source) = report.source {
                span.record("source", source.as_str());
                http.add_source(key, source, report.fallback_reason.as_deref());
            }
            if rt.recorder.is_some() {
                report.response = Some(body.clone());
            }
        }
        Err(failure) => {
            report.status = failure.status;
            if rt.recorder.is_some() {
                report.response = Some(json!({
                    "message": failure.error.message,
                    "extensions": failure.error.extensions,
                }));
            }
        }
    }
    span.record("status", report.status);
    http.add_delay(delay);
    report.latency = started.elapsed() + delay.unwrap_or_default();
    if http.one_record {
        call::report_usage(rt, &http.headers, report.clone()).await;
        http.lock().reports.push((record_key, report));
    } else {
        call::report(rt, &http.headers, report).await;
    }
    outcome.map_err(|f| f.error)
}

/// Stores the objects a generated 2xx answer holds (see [`super::state`]):
/// a mutation's always, a query's under consistency unless the faker stood
/// in for a failed LLM call. Answers from state or the cache are already
/// stored.
async fn store_answer(
    rt: &MockRuntime,
    plan: &call::CallPlan<'_>,
    op: &mb_mcm::Operation,
    selection: &super::select::Selection,
    call: &OperationCall,
    generated: &mb_engine::GeneratedResponse,
) {
    let Some(mode) = super::state::mode(op) else {
        return;
    };
    let seeding = mode == super::state::Store::Seed;
    if !(200..300).contains(&generated.status)
        || matches!(generated.source, Source::State | Source::Cache)
        || (seeding && (!rt.consistency || call::transient_fallback(generated)))
    {
        return;
    }
    let (Some(body), Some(Value::Object(args))) = (&generated.body, &call.body) else {
        return;
    };
    let objects = super::state::objects(&rt.bundle, op, selection, body, args);
    if !objects.is_empty() {
        call::store_resources(rt, &plan.session, objects, !seeding).await;
    }
}

/// The one request-log record of an HTTP request's root fields: a single
/// field's own; for several, their operations (comma-separated, by
/// response key), the highest status, the worst source, the summed tokens,
/// the longest latency, `request.args` keyed by response key and the whole
/// GraphQL `response`.
pub(crate) fn merged_record(
    mut reports: Vec<(String, Report)>,
    response: impl FnOnce() -> Option<Value>,
) -> Option<Report> {
    if reports.len() <= 1 {
        return reports.pop().map(|(_, r)| r);
    }
    reports.sort_by(|a, b| a.0.cmp(&b.0));
    let mut args = Map::new();
    let mut operations: Vec<String> = Vec::new();
    let (_, first) = reports.first()?;
    let mut merged = first.clone();
    merged.source = None;
    merged.fallback_reason = None;
    merged.usage = Default::default();
    for (key, r) in &reports {
        if let Some(op) = &r.operation_id
            && !operations.contains(op)
        {
            operations.push(op.clone());
        }
        args.insert(
            key.clone(),
            r.request.get("args").cloned().unwrap_or(Value::Null),
        );
        merged.status = merged.status.max(r.status);
        merged.source = match (merged.source, r.source) {
            (Some(a), Some(b)) => Some(a.worst(b)),
            (a, b) => a.or(b),
        };
        if merged.fallback_reason.is_none() {
            merged.fallback_reason = r.fallback_reason.clone();
        }
        merged.usage = merged.usage + r.usage;
        merged.latency = merged.latency.max(r.latency);
    }
    merged.operation_id = Some(operations.join(","));
    if let Value::Object(request) = &mut merged.request {
        request.insert("args".into(), Value::Object(args));
    }
    if merged.response.is_some() {
        merged.response = response();
    }
    Some(merged)
}

pub(crate) fn forced_message(status: u16) -> String {
    match canonical_reason(status) {
        Some(reason) => format!("{reason} (forced status {status})"),
        None => format!("forced status {status}"),
    }
}

fn canonical_reason(status: u16) -> Option<&'static str> {
    StatusCode::from_u16(status)
        .ok()
        .and_then(|c| c.canonical_reason())
}

/// Resolves a nested field: the parent object's member under this field's
/// response key (alias or name), missing members being null. Arguments are
/// ignored. Every parent is keyed by response key (see
/// [`super::select::rekey`] and [`super::select::settle`]).
pub(crate) fn nested(
    ty: &Type,
    shapes: &Shapes,
    ctx: &ResolverContext<'_>,
) -> async_graphql::Result<Option<FieldValue<'static>>> {
    let parent = ctx.parent_value.try_downcast_ref::<Value>()?;
    let key = ctx.ctx.item.node.response_key().node.as_str();
    Ok(parent.get(key).and_then(|v| project(v.clone(), ty, shapes)))
}

/// `value` as a field value of type `ty`: enums from strings, objects as
/// owned JSON for nested resolvers, interfaces and unions tagged with
/// their `__typename`. `None` (null) for a null or mismatched value.
pub(crate) fn project(value: Value, ty: &Type, shapes: &Shapes) -> Option<FieldValue<'static>> {
    if value.is_null() {
        return None;
    }
    match &ty.base {
        BaseType::List(inner) => match value {
            Value::Array(items) => {
                Some(FieldValue::list(items.into_iter().map(|item| {
                    project(item, inner, shapes).unwrap_or(FieldValue::NULL)
                })))
            }
            _ => None,
        },
        BaseType::Named(name) => match shapes.get(name.as_str()).copied() {
            None | Some(Shape::Scalar) => async_graphql::Value::from_json(value)
                .ok()
                .map(FieldValue::value),
            Some(Shape::Enum) => match value {
                Value::String(s) => {
                    Some(FieldValue::value(async_graphql::Value::Enum(Name::new(s))))
                }
                _ => None,
            },
            Some(Shape::Object) => Some(FieldValue::owned_any(value)),
            Some(Shape::Abstract) => {
                let typename = value.get("__typename")?.as_str()?.to_string();
                Some(FieldValue::owned_any(value).with_type(typename))
            }
        },
    }
}
