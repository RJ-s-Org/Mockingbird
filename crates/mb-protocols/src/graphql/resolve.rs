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
use mb_ext::Rejection;
use serde_json::{Map, Value, json};
use tokio::time::Instant;
use tracing::Instrument;
use tracing::field::Empty;

use super::error_code;
use crate::MockRuntime;
use crate::call::{
    self, DEFAULT_SESSION, PlanError, PlanFailure, Report, forwarded_headers, recorded_headers,
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
/// the request data.
pub(crate) struct HttpCall {
    headers: HeaderMap,
    /// `POST` or `GET`.
    method: String,
    /// The request's variables, for the request record.
    variables: Value,
    collected: Mutex<Collected>,
}

/// What the root resolvers learned, read once execution is done.
#[derive(Default)]
pub(crate) struct Collected {
    /// Response key → `llm`, `repaired` or `fallback`.
    pub sources: Map<String, Value>,
    /// The worst source over the generated root fields.
    pub worst: Option<Source>,
    /// The largest injected latency among the root fields.
    pub delay: Option<Duration>,
}

impl HttpCall {
    pub(crate) fn new(headers: HeaderMap, method: String, variables: Value) -> Self {
        Self {
            headers,
            method,
            variables,
            collected: Mutex::new(Collected::default()),
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

    fn add_source(&self, key: String, source: Source) {
        let mut c = self.lock();
        c.sources.insert(key, json!(source.as_str()));
        c.worst = Some(c.worst.map_or(source, |w| w.worst(source)));
    }
}

/// A root field failure: the HTTP status it stands for and the field error.
struct Failure {
    status: u16,
    error: Error,
}

impl Failure {
    fn new(status: u16, message: impl Into<String>) -> Self {
        let error = Error::new(message).extend_with(|_, e| {
            e.set("code", error_code(status));
            e.set("status", i32::from(status));
        });
        Self { status, error }
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
    let http = ctx.data::<Arc<HttpCall>>()?;
    let key = ctx.ctx.item.node.response_key().node.to_string();
    let args: Map<String, Value> = ctx
        .args
        .as_index_map()
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone().into_json().unwrap_or(Value::Null)))
        .collect();
    let span = tracing::info_span!(
        "mock.request",
        mock = %rt.name,
        operation = %op_id,
        session = Empty,
        status = Empty,
        source = Empty,
    );
    let result = generate(rt, op_id, http, key, args).instrument(span).await;
    match result {
        Ok(body) => Ok(project(body, ty, shapes)),
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
        usage: Default::default(),
        latency: Default::default(),
        tenancy: None,
        admission_ran: true,
        request: if rt.recorder.is_some() {
            json!({
                "variables": http.variables,
                "args": args,
                "headers": recorded_headers(&http.headers),
            })
        } else {
            Value::Null
        },
        request_body_bytes: None,
        response: None,
    };
    let mut delay = None;

    let outcome = match call::plan(rt, op, &http.headers).await {
        Err(PlanFailure {
            error,
            tenancy,
            session,
        }) => {
            report.tenancy = tenancy;
            if let Some(s) = session {
                report.session = s;
            }
            Err(match error {
                PlanError::Rejected(r) => Failure::rejected(r),
                PlanError::BadOverride(detail) => Failure::new(400, detail),
                PlanError::UnknownScenario(name) => {
                    Failure::new(400, format!("mock {} has no scenario {name:?}", rt.name))
                }
            })
        }
        Ok(plan) => {
            report.tenancy = Some(plan.tenancy.clone());
            report.session = plan.session.session.clone();
            delay = plan.delay;
            match plan.forced_status.filter(|s| !(200..300).contains(s)) {
                Some(status) => Err(Failure::new(status, forced_message(status))),
                None => {
                    let call = OperationCall {
                        operation_id: op.id.clone(),
                        headers: forwarded_headers(&http.headers),
                        body: Some(Value::Object(args)),
                        ..Default::default()
                    };
                    match call::generate_once(rt, &plan, &call).await {
                        Ok(generated) => {
                            report.usage = generated.usage;
                            report.source = Some(generated.source);
                            Ok(generated.body.unwrap_or(Value::Null))
                        }
                        Err(EngineError::UndeclaredStatus(code)) => Err(Failure::new(
                            400,
                            format!("operation {} declares no response for status {code}", op.id),
                        )),
                        Err(e) => Err(Failure::new(500, e.to_string())),
                    }
                }
            }
        }
    };

    let span = tracing::Span::current();
    match &outcome {
        Ok(body) => {
            if let Some(source) = report.source {
                span.record("source", source.as_str());
                http.add_source(key, source);
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
    call::report(rt, &http.headers, report).await;
    outcome.map_err(|f| f.error)
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

/// Resolves a nested field: the parent object's member `name`, missing
/// members being null. Arguments are ignored.
pub(crate) fn nested(
    name: &str,
    ty: &Type,
    shapes: &Shapes,
    ctx: &ResolverContext<'_>,
) -> async_graphql::Result<Option<FieldValue<'static>>> {
    let parent = ctx.parent_value.try_downcast_ref::<Value>()?;
    Ok(parent
        .get(name)
        .and_then(|v| project(v.clone(), ty, shapes)))
}

/// `value` as a field value of type `ty`: enums from strings, objects as
/// owned JSON for nested resolvers, interfaces and unions tagged with
/// their `__typename`. `None` (null) for a null or mismatched value.
fn project(value: Value, ty: &Type, shapes: &Shapes) -> Option<FieldValue<'static>> {
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
