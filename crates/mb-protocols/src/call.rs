//! The per-call pipeline every adapter shares: admission, data-plane auth,
//! override headers and scenario lookup ([`plan`]), state reads and writes around generation
//! ([`generate_once`], [`open_stream`]), state change feeds
//! ([`change_feed`]) and usage/request-log reporting ([`report`]).

use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use mb_engine::{
    EngineError, GeneratedResponse, GenerationContext, OperationCall, Source, StreamContext,
    StreamItem,
};
use mb_ext::{CallInfo, Rejection, Tenancy, UsageEvent};
use mb_llm::Usage;
use mb_mcm::{Binding, Effect, Operation, OperationKind, Protocol};
use mb_state::{ChangeKind, SessionKey, StateError, StoredResource};
use serde_json::{Map, Value};
use tokio::sync::broadcast::error::RecvError;
use tracing::Instrument;

use crate::chaos::{self, CHAOS_HEADER, ChaosDecision};
pub(crate) use crate::dp_auth::Unauthorized;
use crate::dp_auth::{authenticate, verify_key};
use crate::{
    MockRuntime, RequestKind, RequestRecord, STREAM_ITEMS_HEADER, ScenarioRuntime, UnknownIds,
};

const SESSION_HEADER: &str = "x-mockingbird-session";
const STATUS_HEADER: &str = "x-mockingbird-status";
const LATENCY_HEADER: &str = "x-mockingbird-latency";
const SCENARIO_HEADER: &str = "x-mockingbird-scenario";
pub(crate) const DEFAULT_SESSION: &str = "default";
const MAX_LATENCY: Duration = Duration::from_secs(30);
const MAX_SESSION_LEN: usize = 128;
const MAX_STREAM_ITEMS: usize = 10_000;
const LIST_CONTEXT_LIMIT: usize = 20;
/// Usage-event source (the `source` label of `mb_requests_total`) for calls
/// answered without generating a body: refused (auth, quota, rate limit,
/// invalid request or override, unknown scenario) or answered natively (a
/// forced or injected error the contract declares no response for).
const NOT_GENERATED_SOURCE: &str = "not_generated";

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

/// The `X-Mockingbird-*` override headers of one call.
pub(crate) struct Overrides {
    pub session: String,
    pub status: Option<u16>,
    pub latency: Option<Duration>,
    pub scenario: Option<String>,
    pub stream_items: Option<usize>,
    /// `X-Mockingbird-Chaos: off`: no fault injection for this call.
    pub chaos_off: bool,
}

impl Overrides {
    /// `Err` carries the problem detail for an unusable override header.
    pub(crate) fn parse(headers: &HeaderMap, enabled: bool) -> Result<Self, String> {
        let mut out = Self {
            session: DEFAULT_SESSION.into(),
            status: None,
            latency: None,
            scenario: None,
            stream_items: None,
            chaos_off: false,
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
        if let Some(s) = header_text(headers, STREAM_ITEMS_HEADER)? {
            let items = s
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=MAX_STREAM_ITEMS).contains(n))
                .ok_or_else(|| {
                    format!(
                        "X-Mockingbird-Stream-Items must be an integer from 1 to \
                         {MAX_STREAM_ITEMS}, got {s:?}"
                    )
                })?;
            out.stream_items = Some(items);
        }
        if let Some(s) = header_text(headers, CHAOS_HEADER)? {
            if !s.eq_ignore_ascii_case("off") {
                return Err(r#"X-Mockingbird-Chaos must be "off""#.into());
            }
            out.chaos_off = true;
        }
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

/// How one admitted call is to be served.
pub(crate) struct CallPlan<'a> {
    pub op: &'a Operation,
    pub session: SessionKey,
    /// The selected scenario, when it applies to `op`.
    pub scenario: Option<&'a ScenarioRuntime>,
    /// Header status, else the scenario's status where `op` declares it
    /// (or any 5xx).
    pub forced_status: Option<u16>,
    /// Injected latency (header, else scenario) plus any chaos latency,
    /// capped at 30 s.
    pub delay: Option<Duration>,
    /// Header item count, else the scenario's.
    pub stream_items: Option<usize>,
    pub tenancy: Tenancy,
    /// The call's faults. An injected error comes with `forced_status` set
    /// to the declared error status it uses; with `forced_status` unset,
    /// the adapter answers its protocol-native error.
    pub chaos: ChaosDecision,
}

impl CallPlan<'_> {
    /// Whether the call answers an injected error that the operation
    /// declares no status for (the protocol-native answer applies).
    #[cfg(any(feature = "http-rest", feature = "soap"))]
    pub(crate) fn native_chaos_error(&self) -> bool {
        self.chaos.inject_error && self.forced_status.is_none()
    }
}

/// The credentials a call presents beyond its headers (for gRPC: its
/// metadata; for SOAP: its HTTP headers).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Presented<'a> {
    /// The raw query pairs, which may carry an `api_key` parameter or a
    /// query API key the contract declares. Empty for protocols without a
    /// query.
    pub query: &'a [(String, String)],
    /// The adapter already checked the data-plane key for the whole request
    /// (see [`check_key`]).
    pub key_verified: bool,
    /// The adapter already admitted the whole request (tenant, then quota)
    /// before checking its key; the call is not admitted again.
    pub admitted: Option<&'a Tenancy>,
}

/// Admission for a whole request (`tenants.resolve`, then `quota.admit`,
/// then the mock's rate limit)
/// with operation id `operation_id`, for adapters that check the
/// data-plane key once per request ([`check_key`]) and must admit first.
/// `Err` carries the tenancy when it resolved.
#[cfg_attr(not(any(feature = "graphql", feature = "events")), allow(dead_code))]
pub(crate) async fn admit(
    rt: &MockRuntime,
    operation_id: &str,
    headers: &HeaderMap,
) -> Result<Tenancy, (Rejection, Option<Tenancy>)> {
    async {
        let call_info = CallInfo::new(&rt.name, operation_id, headers);
        let tenancy = rt
            .ext
            .tenants
            .resolve(&call_info)
            .await
            .map_err(|r| (r, None))?;
        if let Err(r) = rt.ext.quota.admit(&tenancy, &call_info).await {
            return Err((r, Some(tenancy)));
        }
        match rate_limit(rt) {
            Ok(()) => Ok(tenancy),
            Err(r) => Err((r, Some(tenancy))),
        }
    }
    .instrument(tracing::info_span!("mb.admission"))
    .await
}

/// The mock's rate limit (spec §7, step 2, after the quota): `Err` with
/// the `429` rejection (and its `Retry-After`) when the bucket is empty.
pub(crate) fn rate_limit(rt: &MockRuntime) -> Result<(), Rejection> {
    let Some(limiter) = &rt.limits.rate else {
        return Ok(());
    };
    limiter.try_acquire().map_err(|retry_after| {
        metrics::counter!(crate::limits::RATE_LIMITED_METRIC, "mock" => rt.name.clone())
            .increment(1);
        Rejection::new(
            429,
            "Too Many Requests",
            format!(
                "mock {} allows {} requests/s (burst {})",
                rt.name,
                limiter.requests_per_second(),
                limiter.burst()
            ),
        )
        .with_retry_after(retry_after)
    })
}

/// Admission of one more inbound message of a streaming call already
/// admitted with `tenancy` (gRPC client and bidi streams): `quota.admit`,
/// then the mock's rate limit, as for a new call.
#[cfg_attr(not(feature = "grpc"), allow(dead_code))]
pub(crate) async fn admit_item(
    rt: &MockRuntime,
    tenancy: &Tenancy,
    operation_id: &str,
    headers: &HeaderMap,
) -> Result<(), Rejection> {
    let call_info = CallInfo::new(&rt.name, operation_id, headers);
    rt.ext.quota.admit(tenancy, &call_info).await?;
    rate_limit(rt)
}

/// One usage event for a single engine call of a streaming call (a gRPC
/// bidi reply); the call's own [`report`] is then sent with
/// [`report_unmetered`], so nothing is counted twice.
#[cfg_attr(not(feature = "grpc"), allow(dead_code))]
pub(crate) fn meter(
    rt: &MockRuntime,
    tenancy: Tenancy,
    operation_id: &str,
    source: Source,
    latency: Duration,
    usage: Usage,
) {
    let mut event = UsageEvent::new(tenancy, rt.name.clone(), operation_id);
    event.status = 200;
    event.source = source.as_str().to_string();
    event.latency = latency;
    event.prompt_tokens = usage.prompt_tokens;
    event.completion_tokens = usage.completion_tokens;
    rt.ext.usage.record(&event);
}

/// The answer to a rate-limited call of `op` when the contract declares a
/// `429` response: the engine's fallback for that variant (never an LLM
/// call). `None` when the operation declares no `429`.
#[cfg_attr(not(any(feature = "http-rest", feature = "soap")), allow(dead_code))]
pub(crate) fn declared_rate_limited(rt: &MockRuntime, op: &Operation) -> Option<GeneratedResponse> {
    let variant = op
        .outputs
        .iter()
        .find(|o| o.status == mb_mcm::StatusSelector::Code(429))?;
    let ctx = GenerationContext {
        forced_status: Some(429),
        ..GenerationContext::default()
    };
    Some(rt.engine.fallback(
        &rt.bundle,
        op,
        variant,
        429,
        &OperationCall::default(),
        &ctx,
    ))
}

/// The data-plane key check of `api_key` mode for a whole request, for
/// adapters that answer it once before planning each call (GraphQL over
/// HTTP); `Ok` in every other mode.
#[cfg_attr(not(feature = "graphql"), allow(dead_code))]
pub(crate) async fn check_key(
    rt: &MockRuntime,
    headers: &HeaderMap,
    query: &[(String, String)],
) -> Result<(), Unauthorized> {
    verify_key(rt, headers, query).await
}

/// `pairs` without the mock's credential query parameters: they never
/// reach the engine or the record.
#[cfg_attr(not(feature = "http-rest"), allow(dead_code))]
pub(crate) fn public_query(rt: &MockRuntime, pairs: &[(String, String)]) -> Vec<(String, String)> {
    let secret = &rt.credential_names().query;
    pairs
        .iter()
        .filter(|(k, _)| !secret.contains(k))
        .cloned()
        .collect()
}

/// Why a call cannot be served.
#[derive(Debug)]
pub(crate) enum PlanError {
    /// Tenancy resolution or the quota refused the call.
    Rejected(Rejection),
    /// The mock's rate limit refused the call (`429`, with `Retry-After`).
    RateLimited(Rejection),
    /// Data-plane auth refused the call (`401`).
    Unauthorized(Unauthorized),
    /// An override header is unusable; carries the problem detail.
    BadOverride(String),
    /// `X-Mockingbird-Scenario` names no scenario of this mock; carries
    /// the name.
    UnknownScenario(String),
}

/// A failed [`plan`], with what was learned before it failed, so the call
/// can still be reported against the right tenant and session.
#[derive(Debug)]
pub(crate) struct PlanFailure {
    pub error: PlanError,
    /// Set once tenancy resolved (a quota rejection or a bad override).
    pub tenancy: Option<Tenancy>,
    /// Set once the override headers parsed (an unknown scenario).
    pub session: Option<String>,
}

impl PlanFailure {
    fn new(error: PlanError, tenancy: Option<Tenancy>) -> Self {
        Self {
            error,
            tenancy,
            session: None,
        }
    }
}

/// Admission (`tenants.resolve`, then `quota.admit`, then the mock's rate
/// limit, unless [`Presented::admitted`]), then data-plane auth
/// (spec §11.2), then override headers, then the scenario, then chaos.
/// Header status and latency beat the scenario's; a scenario status
/// applies only where `op` declares it, or when it is a 5xx. Chaos comes from the applying
/// scenario, else the mock, unless `X-Mockingbird-Chaos: off`; it injects
/// no error when a status is already forced. Records the session on the
/// current span. Admission and auth run in an `mb.admission` span.
pub(crate) async fn plan<'a>(
    rt: &'a MockRuntime,
    op: &'a Operation,
    headers: &HeaderMap,
    presented: Presented<'_>,
) -> Result<CallPlan<'a>, PlanFailure> {
    let tenancy = admit_and_authenticate(rt, op, headers, presented)
        .instrument(tracing::info_span!("mb.admission"))
        .await?;
    let overrides = match Overrides::parse(headers, rt.header_overrides) {
        Ok(o) => o,
        Err(detail) => {
            return Err(PlanFailure::new(
                PlanError::BadOverride(detail),
                Some(tenancy),
            ));
        }
    };
    tracing::Span::current().record("session", overrides.session.as_str());
    let scenario = match overrides.scenario.as_deref() {
        Some(name) => match rt.scenarios.get(name) {
            None => {
                return Err(PlanFailure {
                    error: PlanError::UnknownScenario(name.to_string()),
                    tenancy: Some(tenancy),
                    session: Some(overrides.session),
                });
            }
            Some(s) => Some(s),
        },
        None => rt.default_scenario.as_deref().and_then(|name| {
            let found = rt.scenarios.get(name);
            if found.is_none() {
                tracing::debug!(mock = %rt.name, scenario = name, "default scenario not found; ignored");
            }
            found
        }),
    }
    .filter(|s| s.applies_to(&op.id))
    .map(|s| s.for_operation(&op.id));
    // A scenario's status applies where the contract declares it, and as a
    // 5xx anywhere (answered natively, like an undeclared header 5xx); the
    // header override is used as given (each adapter answers an undeclared
    // one).
    let scenario_status = scenario
        .and_then(|s| s.status)
        .filter(|code| (500..600).contains(code) || op.output_for_status(*code).is_some());
    let mut forced_status = overrides.status.or(scenario_status);
    let mut delay = overrides
        .latency
        .or_else(|| scenario.and_then(|s| s.latency))
        .map(|d| d.min(MAX_LATENCY));
    let stream_items = overrides
        .stream_items
        .or_else(|| scenario.and_then(|s| s.stream_items));
    let chaos = decide_chaos(rt, op, scenario, &overrides, forced_status, stream_items);
    if chaos.inject_error {
        forced_status = chaos::error_status(op);
    }
    if !chaos.extra_latency.is_zero() {
        delay = Some((delay.unwrap_or_default() + chaos.extra_latency).min(MAX_LATENCY));
    }
    Ok(CallPlan {
        op,
        session: SessionKey {
            workspace: rt.workspace.clone(),
            mock: rt.name.clone(),
            session: overrides.session,
        },
        scenario,
        forced_status,
        delay,
        stream_items,
        tenancy,
        chaos,
    })
}

/// The admission and data-plane auth of [`plan`].
async fn admit_and_authenticate(
    rt: &MockRuntime,
    op: &Operation,
    headers: &HeaderMap,
    presented: Presented<'_>,
) -> Result<Tenancy, PlanFailure> {
    let call_info = CallInfo::new(&rt.name, &op.id, headers);
    let tenancy = match presented.admitted {
        Some(t) => t.clone(),
        None => {
            let tenancy = match rt.ext.tenants.resolve(&call_info).await {
                Ok(t) => t,
                Err(r) => return Err(PlanFailure::new(PlanError::Rejected(r), None)),
            };
            if let Err(r) = rt.ext.quota.admit(&tenancy, &call_info).await {
                return Err(PlanFailure::new(PlanError::Rejected(r), Some(tenancy)));
            }
            if let Err(r) = rate_limit(rt) {
                return Err(PlanFailure::new(PlanError::RateLimited(r), Some(tenancy)));
            }
            tenancy
        }
    };
    let auth = authenticate(
        rt,
        &op.id,
        &op.security,
        headers,
        presented.query,
        presented.key_verified,
    )
    .await;
    if let Err(unauthorized) = auth {
        return Err(PlanFailure::new(
            PlanError::Unauthorized(unauthorized),
            Some(tenancy),
        ));
    }
    Ok(tenancy)
}

/// The call's faults, from the applying scenario's chaos, else the mock's;
/// none with `X-Mockingbird-Chaos: off`. No error is injected over a
/// status already forced. Counts the error and latency faults.
fn decide_chaos(
    rt: &MockRuntime,
    op: &Operation,
    scenario: Option<&ScenarioRuntime>,
    overrides: &Overrides,
    forced_status: Option<u16>,
    stream_items: Option<usize>,
) -> ChaosDecision {
    if overrides.chaos_off {
        return ChaosDecision::default();
    }
    let Some(source) = scenario
        .and_then(|s| s.chaos.as_ref())
        .or(rt.chaos.as_ref())
    else {
        return ChaosDecision::default();
    };
    let streaming = op.kind.streams_response();
    let limits = &rt.engine.config().stream;
    let planned_items = stream_items
        .unwrap_or(limits.default_items)
        .clamp(1, limits.max_items.max(1));
    let mut decision = source.decide(streaming, planned_items);
    decision.inject_error &= forced_status.is_none();
    if decision.inject_error {
        chaos::count(&rt.name, "error");
    }
    if !decision.extra_latency.is_zero() {
        chaos::count(&rt.name, "latency");
    }
    decision
}

/// The id of the resource this operation reads, updates or deletes: the
/// identity's path parameter, else its input field in the request body,
/// else, for HTTP updates and deletes whose identity names no path
/// parameter, the last path parameter.
pub(crate) fn target_id(op: &Operation, call: &OperationCall) -> Option<String> {
    let identity = op.identity.as_ref();
    let path_param = identity.and_then(|i| i.path_param.as_deref());
    if let Some(id) = path_param
        .and_then(|p| call.path_params.get(p))
        .and_then(id_string)
    {
        return Some(id);
    }
    if let Some(id) = identity
        .and_then(|i| i.input_field.as_deref())
        .and_then(|f| call.body.as_ref()?.get(f))
        .and_then(id_string)
    {
        return Some(id);
    }
    if path_param.is_some()
        || !matches!(
            op.kind,
            OperationKind::Command(Effect::Update | Effect::Delete)
        )
    {
        return None;
    }
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    call.path_params
        .get(&last_path_param(path)?)
        .and_then(id_string)
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

/// Reads the target and related resources, generates one response, writes
/// state for a 2xx and keeps the session alive.
#[cfg_attr(
    not(any(feature = "http-rest", feature = "grpc", feature = "soap")),
    allow(dead_code)
)]
pub(crate) async fn generate_once(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Result<GeneratedResponse, EngineError> {
    generate_narrowed(rt, plan, call, None).await
}

/// The stored target and related resources a read-only generation of `call`
/// takes into account (as [`generate_once`] reads them); the session is
/// marked used.
#[cfg(feature = "graphql")]
pub(crate) async fn read_state(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> (Option<StoredResource>, Vec<StoredResource>) {
    let mut state = StateAccess::new(rt);
    let target_id = target_id(plan.op, call);
    let read = read_context(
        &mut state,
        &plan.session,
        plan.op,
        target_id.as_deref(),
        call,
    )
    .instrument(state_span())
    .await;
    state.touch(&plan.session).instrument(state_span()).await;
    read
}

/// [`generate_once`] with a 2xx response schema narrowed to what the caller
/// reads (see [`GenerationContext::response_schema`]).
pub(crate) async fn generate_narrowed(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    response_schema: Option<&Value>,
) -> Result<GeneratedResponse, EngineError> {
    let op = plan.op;
    let session = &plan.session;
    let mut state = StateAccess::new(rt);
    let resource_type = op.resource_type.as_deref();
    let target_id = target_id(op, call);
    let (target, related) = read_context(&mut state, session, op, target_id.as_deref(), call)
        .instrument(state_span())
        .await;

    if rt.unknown_ids == UnknownIds::NotFound
        && matches!(op.kind, OperationKind::Query)
        && plan.forced_status.is_none()
        && op.identity.as_ref().is_some_and(|i| i.path_param.is_some())
        && target.is_none()
        && state.available()
        && let (Some(t), Some(id)) = (resource_type, target_id.as_deref())
    {
        let detail = format!("{t} {id} does not exist in session {}", session.session);
        state.touch(session).instrument(state_span()).await;
        return Ok(rt.engine.not_found(&rt.bundle, op, call, &detail));
    }

    if matches!(op.kind, OperationKind::Query)
        && plain(plan)
        && let Some(stored) = &target
        && let Some(served) = rt
            .engine
            .serve_stored(&rt.bundle, op, &stored.data, response_schema)
    {
        state.touch(session).instrument(state_span()).await;
        return Ok(served);
    }

    let ctx = GenerationContext {
        related: &related,
        target: target.as_ref(),
        forced_status: plan.forced_status,
        scenario: plan.scenario.map(|s| &s.spec),
        response_schema,
    };
    let generated = rt.engine.generate(&rt.bundle, op, call, ctx).await?;

    async {
        if (200..300).contains(&generated.status)
            && let Some(t) = resource_type
        {
            write_state(
                &mut state,
                session,
                op,
                t,
                target_id,
                &generated,
                call.body.as_ref(),
            )
            .await;
        }
        state.touch(session).await;
    }
    .instrument(state_span())
    .await;
    Ok(generated)
}

/// Whether no scenario applies to the call and no status is forced (an
/// injected chaos error forces one).
fn plain(plan: &CallPlan<'_>) -> bool {
    plan.scenario.is_none() && plan.forced_status.is_none()
}

/// The span of a call's state reads, or of its writes.
fn state_span() -> tracing::Span {
    tracing::info_span!("mb.state")
}

/// Reads the target and related resources like [`generate_once`], keeps
/// the session alive, then opens an engine stream. Generated items are
/// never written to state.
#[cfg_attr(not(feature = "sse"), allow(dead_code))] // also used by the GraphQL and gRPC adapters
pub(crate) async fn open_stream(
    rt: &Arc<MockRuntime>,
    plan: &CallPlan<'_>,
    call: OperationCall,
    request_stream: Vec<Value>,
) -> Result<BoxStream<'static, StreamItem>, EngineError> {
    let op = plan.op;
    let mut state = StateAccess::new(rt);
    let target_id = target_id(op, &call);
    let (target, related) = async {
        let context =
            read_context(&mut state, &plan.session, op, target_id.as_deref(), &call).await;
        state.touch(&plan.session).await;
        context
    }
    .instrument(state_span())
    .await;
    let ctx = StreamContext {
        related,
        target,
        scenario: plan.scenario.map(|s| s.spec.clone()),
        request_stream,
        item_interval: plan.scenario.and_then(|s| s.stream_item_interval),
    };
    rt.engine
        .generate_stream(Arc::clone(&rt.bundle), op, call, ctx, plan.stream_items)
}

/// The data of every resource of `op`'s resource type upserted in
/// `session` from now on, when it is a valid stream item of `op`. Events
/// missed because the receiver lagged are skipped. Empty when `op` has no
/// resource type.
#[cfg_attr(not(feature = "sse"), allow(dead_code))] // also used by the GraphQL adapter
pub(crate) fn change_feed(
    rt: &Arc<MockRuntime>,
    session: SessionKey,
    op: &Operation,
) -> BoxStream<'static, Value> {
    let Some(resource_type) = op.resource_type.clone() else {
        return stream::empty().boxed();
    };
    let feed = Feed {
        rt: Arc::clone(rt),
        op: op.clone(),
        session,
        resource_type,
        rx: rt.state.subscribe(),
    };
    stream::unfold(feed, |mut feed| async move {
        loop {
            let event = match feed.rx.recv().await {
                Ok(e) => e,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            };
            if event.kind != ChangeKind::Upserted
                || event.session != feed.session
                || event.resource_type != feed.resource_type
            {
                continue;
            }
            let found = feed
                .rt
                .state
                .get(&feed.session, &feed.resource_type, &event.id)
                .await;
            let Ok(Some(resource)) = found else { continue };
            if feed
                .rt
                .engine
                .conform_item(&feed.rt.bundle, &feed.op, &resource.data)
            {
                return Some((resource.data, feed));
            }
        }
    })
    .boxed()
}

struct Feed {
    rt: Arc<MockRuntime>,
    op: Operation,
    session: SessionKey,
    resource_type: String,
    rx: tokio::sync::broadcast::Receiver<mb_state::StateChanged>,
}

/// One served call, as reported to the usage sink and the request
/// recorder.
pub(crate) struct Report {
    /// `None` when no operation matched.
    pub operation_id: Option<String>,
    pub session: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    /// The (worst) source of a generated response; `None` for problems.
    pub source: Option<Source>,
    /// Why the faker answered, when an LLM call failed or none is configured.
    pub fallback_reason: Option<String>,
    pub usage: Usage,
    /// Time to build the response, plus any injected latency.
    pub latency: Duration,
    /// Tenancy resolved during admission.
    pub tenancy: Option<Tenancy>,
    /// Whether admission ran (a call rejected before tenancy resolved has
    /// no tenancy to bill).
    pub admission_ran: bool,
    /// `{query, headers, body}` for the record; only read when a recorder
    /// is set.
    pub request: Value,
    pub request_body_bytes: Option<usize>,
    pub response: Option<Value>,
}

/// Report a served call to the usage sink and the request recorder. Calls
/// that never reached admission are still attributed to a tenant.
pub(crate) async fn report(rt: &MockRuntime, headers: &HeaderMap, r: Report) {
    report_with(rt, headers, r, true).await;
}

/// [`report`] to the request recorder only: the call's usage was already
/// recorded item by item (see [`meter`]).
#[cfg_attr(not(feature = "grpc"), allow(dead_code))]
pub(crate) async fn report_unmetered(rt: &MockRuntime, headers: &HeaderMap, r: Report) {
    report_with(rt, headers, r, false).await;
}

async fn report_with(rt: &MockRuntime, headers: &HeaderMap, mut r: Report, metered: bool) {
    if !metered {
        record(rt, r);
        return;
    }
    let operation_id = r.operation_id.clone().unwrap_or_default();
    let tenancy = match (r.tenancy.take(), r.admission_ran) {
        (Some(t), _) => Some(t),
        (None, true) => None,
        (None, false) => {
            let call_info = CallInfo::new(&rt.name, &operation_id, headers);
            rt.ext.tenants.resolve(&call_info).await.ok()
        }
    };
    if let Some(tenancy) = tenancy {
        let mut event = UsageEvent::new(tenancy, rt.name.clone(), operation_id);
        event.status = r.status;
        event.source = r
            .source
            .map_or(NOT_GENERATED_SOURCE, Source::as_str)
            .to_string();
        event.latency = r.latency;
        event.prompt_tokens = r.usage.prompt_tokens;
        event.completion_tokens = r.usage.completion_tokens;
        rt.ext.usage.record(&event);
    }
    record(rt, r);
}

/// Hands one call to the request recorder, when one is set.
fn record(rt: &MockRuntime, r: Report) {
    if let Some(recorder) = &rt.recorder {
        recorder.record(RequestRecord {
            workspace: rt.workspace.clone(),
            mock: rt.name.clone(),
            operation_id: r.operation_id,
            session: r.session,
            method: r.method,
            path: r.path,
            status: r.status,
            source: r.source.map(|s| s.as_str().to_string()),
            fallback_reason: r.fallback_reason,
            latency: r.latency,
            prompt_tokens: r.usage.prompt_tokens,
            completion_tokens: r.usage.completion_tokens,
            request: r.request,
            request_body_bytes: r.request_body_bytes,
            response: r.response,
            kind: request_kind(rt.bundle.protocol()),
        });
    }
}

/// The request-log kind of a call to a mock serving `protocol`.
fn request_kind(protocol: Protocol) -> RequestKind {
    match protocol {
        Protocol::Http => RequestKind::Http,
        Protocol::Graphql => RequestKind::Graphql,
        Protocol::Grpc => RequestKind::Grpc,
        Protocol::Soap => RequestKind::Soap,
        _ => RequestKind::Http,
    }
}

/// Substrings that mark a header name as carrying credentials, matched
/// case-insensitively (header names from a GraphQL `connection_init`
/// payload may use any spelling, e.g. `apiKey`).
const SENSITIVE_PATTERNS: [&str; 8] = [
    "token",
    "secret",
    "password",
    "api-key",
    "api_key",
    "apikey",
    "authorization",
    "cookie",
];

pub(crate) fn is_sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SENSITIVE_HEADERS.contains(&name.as_str())
        || SENSITIVE_PATTERNS.iter().any(|s| name.contains(s))
}

/// Non-sensitive, UTF-8 request headers, optionally keeping
/// `X-Mockingbird-*` override headers. The mock's credential headers
/// ([`MockRuntime::credential_names`]) are sensitive too.
fn safe_headers(rt: &MockRuntime, headers: &HeaderMap, keep_overrides: bool) -> Map<String, Value> {
    let secret = &rt.credential_names().headers;
    headers
        .iter()
        .filter(|(name, _)| {
            let n = name.as_str();
            !is_sensitive(n)
                && !secret.contains(n)
                && (keep_overrides || !n.starts_with("x-mockingbird-"))
        })
        .filter_map(|(name, v)| {
            Some((
                name.as_str().to_string(),
                Value::String(v.to_str().ok()?.into()),
            ))
        })
        .collect()
}

/// Whether a header named `name` (any case) may be kept in a request
/// record: not credential-like (see [`is_sensitive`]) and not one of the
/// mock's credential headers. Used for headers outside a `HeaderMap`
/// (consumed broker messages).
#[cfg_attr(not(feature = "events"), allow(dead_code))]
pub(crate) fn is_recordable_header(rt: &MockRuntime, name: &str) -> bool {
    !is_sensitive(name)
        && !rt
            .credential_names()
            .headers
            .contains(&name.to_ascii_lowercase())
}

/// Headers forwarded to the prompt.
pub(crate) fn forwarded_headers(rt: &MockRuntime, headers: &HeaderMap) -> Map<String, Value> {
    safe_headers(rt, headers, false)
}

/// Headers kept in the request record (overrides included).
pub(crate) fn recorded_headers(rt: &MockRuntime, headers: &HeaderMap) -> Map<String, Value> {
    safe_headers(rt, headers, true)
}

/// The target (when `target_id` is set) and the related resources.
async fn read_context(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    target_id: Option<&str>,
    call: &OperationCall,
) -> (Option<StoredResource>, Vec<StoredResource>) {
    let target = match (op.resource_type.as_deref(), target_id) {
        (Some(t), Some(id)) => state.get(session, t, id).await,
        _ => None,
    };
    let related = related(
        state,
        session,
        op,
        target_id.is_some(),
        &call.query,
        call.body.as_ref(),
    )
    .await;
    (target, related)
}

/// State-store access for one call. The first failing call marks the mock
/// degraded and makes the rest of the call stateless; a successful call
/// clears the mark. While degraded, [`crate::StateHealth`] lets one call per
/// retry interval try the store; the rest skip it entirely. The breaker is
/// consulted lazily, on the first actual store call, so a call that never
/// touches the store does not use up the retry.
pub(crate) struct StateAccess<'a> {
    rt: &'a MockRuntime,
    gate: Gate,
    /// A read succeeded, so the session needs a [`StateAccess::touch`].
    read: bool,
    /// A write succeeded; writes refresh the session themselves.
    wrote: bool,
}

/// Whether this call may use the store.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// No store call yet; the breaker has not been asked.
    Unasked,
    Open,
    /// Held off by the breaker, or a store call failed.
    Closed,
}

impl<'a> StateAccess<'a> {
    fn new(rt: &'a MockRuntime) -> Self {
        Self {
            rt,
            gate: Gate::Unasked,
            read: false,
            wrote: false,
        }
    }

    /// Keeps the session alive once per call: reads do not refresh it, so a
    /// call that only read touches it here. A no-op when nothing was read,
    /// when a write already refreshed it, or when the store is unavailable.
    async fn touch(&mut self, s: &SessionKey) {
        if self.read && !self.wrote && self.gate == Gate::Open {
            let r = self.rt.state.touch(s).await;
            self.settle(r);
        }
    }

    /// Whether the store answered this call's reads: a missing resource is
    /// then known to be missing, not merely unread.
    fn available(&self) -> bool {
        self.gate == Gate::Open
    }

    /// Whether the next store call may run, asking the breaker on first use.
    fn open(&mut self) -> bool {
        if self.gate == Gate::Unasked {
            self.gate = if self.rt.state_health.try_acquire() {
                Gate::Open
            } else {
                Gate::Closed
            };
        }
        self.gate == Gate::Open
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
                self.gate = Gate::Closed;
                None
            }
        }
    }

    async fn get(&mut self, s: &SessionKey, t: &str, id: &str) -> Option<StoredResource> {
        if !self.open() {
            return None;
        }
        let r = self.rt.state.get(s, t, id).await;
        let found = self.settle(r);
        self.read |= found.is_some();
        found.flatten()
    }

    async fn list(&mut self, s: &SessionKey, t: &str, limit: usize) -> Vec<StoredResource> {
        if !self.open() {
            return Vec::new();
        }
        let r = self.rt.state.list(s, t, limit).await;
        let found = self.settle(r);
        self.read |= found.is_some();
        found.unwrap_or_default()
    }

    async fn upsert(&mut self, s: &SessionKey, r: StoredResource) {
        if self.open() {
            let r = self.rt.state.upsert(s, r).await;
            self.wrote |= self.settle(r).is_some();
        }
    }

    async fn delete(&mut self, s: &SessionKey, t: &str, id: &str) {
        if self.open() {
            let r = self.rt.state.delete(s, t, id).await;
            self.wrote |= self.settle(r).is_some();
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
    request_body: Option<&Value>,
) {
    let data = generated.body.clone().or_else(|| request_body.cloned());
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

/// `reason` as a header value: printable ASCII only (anything else becomes
/// `?`), cut to 300 bytes.
#[cfg(any(feature = "http-rest", feature = "graphql"))]
pub(crate) fn reason_header(reason: &str) -> axum::http::HeaderValue {
    let clean: String = reason
        .chars()
        .map(|c| {
            if c == ' ' || c.is_ascii_graphic() {
                c
            } else {
                '?'
            }
        })
        .take(300)
        .collect();
    axum::http::HeaderValue::from_str(&clean)
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("unprintable"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use mb_engine::{Engine, EngineConfig, ScenarioSpec};
    use mb_import::compile;
    use mb_mcm::{Bundle, Identity, Protocol};
    use mb_state::{MemoryStore, StateStore};
    use serde_json::json;

    #[test]
    fn request_kind_follows_protocol() {
        assert_eq!(request_kind(Protocol::Http), RequestKind::Http);
        assert_eq!(request_kind(Protocol::Graphql), RequestKind::Graphql);
        assert_eq!(request_kind(Protocol::Grpc), RequestKind::Grpc);
        assert_eq!(request_kind(Protocol::Soap), RequestKind::Soap);
    }

    #[test]
    fn credential_like_headers_are_sensitive_in_any_case() {
        for name in [
            "api_key",
            "apikey",
            "apiKey",
            "x-apikey",
            "x-api-key",
            "X-API-KEY",
            "x-api_key",
            "authorization",
            "Authorization",
            "proxy-authorization",
            "cookie",
            "Cookie",
            "x-cookie-jar",
            "x-auth-token",
            "my-secret",
            "x-password",
        ] {
            assert!(is_sensitive(name), "{name}");
        }
        for name in ["x-trace", "x-mockingbird-session", "accept", "user-agent"] {
            assert!(!is_sensitive(name), "{name}");
        }
    }

    #[test]
    fn credential_names_follow_data_plane_auth_changes() {
        use crate::{DataPlaneAuth, DataPlaneAuthMode};
        let bundle = Arc::new(compile(STREAM_CONTRACT.as_bytes()).unwrap());
        let mut rt = MockRuntime::new(
            "t",
            bundle,
            Arc::new(Engine::new(None, EngineConfig::default())),
            Arc::new(MemoryStore::new(Duration::from_secs(60))),
        );
        // Reading the names before auth is configured must not pin them.
        assert!(rt.credential_names().headers.is_empty());
        rt.set_data_plane_auth(DataPlaneAuth {
            mode: DataPlaneAuthMode::ApiKey,
            header: "x-tenant-pass".into(),
        });
        let mut headers = HeaderMap::new();
        headers.insert("x-tenant-pass", "k".parse().unwrap());
        headers.insert("x-trace", "t".parse().unwrap());
        let forwarded = forwarded_headers(&rt, &headers);
        assert!(!forwarded.contains_key("x-tenant-pass"), "{forwarded:?}");
        assert!(forwarded.contains_key("x-trace"), "{forwarded:?}");
        assert!(public_query(&rt, &[("api_key".into(), "k".into())]).is_empty());

        rt.set_data_plane_auth(DataPlaneAuth::default());
        assert!(rt.credential_names().headers.is_empty());
        assert!(forwarded_headers(&rt, &headers).contains_key("x-tenant-pass"));
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

    const STREAM_CONTRACT: &str = r#"
openapi: 3.1.0
info: {title: Ticks, version: "1"}
paths:
  /ticks:
    get:
      operationId: ticks
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema:
                type: object
                required: [id, "n"]
                properties: {id: {type: string}, "n": {type: integer}}
"#;

    /// A `StreamServer` operation over `ticks` whose item is
    /// `{id: string, n: integer}`.
    fn stream_runtime() -> Arc<MockRuntime> {
        let mut bundle: Bundle = compile(STREAM_CONTRACT.as_bytes()).unwrap();
        bundle.operations[0].kind = OperationKind::StreamServer;
        bundle.operations[0].resource_type = Some("ticks".into());
        let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
        Arc::new(MockRuntime::new(
            "ticks",
            Arc::new(bundle),
            Arc::new(Engine::new(None, EngineConfig::default())),
            state,
        ))
    }

    fn session(name: &str) -> SessionKey {
        SessionKey {
            workspace: "default".into(),
            mock: "ticks".into(),
            session: name.into(),
        }
    }

    #[test]
    fn target_id_from_input_field() {
        let rt = stream_runtime();
        let mut op = rt.bundle.operations[0].clone();
        op.protocol = Protocol::Grpc;
        op.kind = OperationKind::Command(Effect::Update);
        op.identity = Some(Identity {
            path_param: None,
            field: "id".into(),
            input_field: Some("tickId".into()),
        });
        let call = OperationCall {
            operation_id: op.id.clone(),
            body: Some(json!({"tickId": 7, "n": 1})),
            ..OperationCall::default()
        };
        assert_eq!(target_id(&op, &call), Some("7".into()));
        let missing = OperationCall {
            body: Some(json!({"n": 1})),
            ..call
        };
        assert_eq!(target_id(&op, &missing), None);
    }

    #[tokio::test]
    async fn change_feed_emits_upserts_for_resource_type() {
        let rt = stream_runtime();
        let op = rt.bundle.operations[0].clone();
        let mut feed = change_feed(&rt, session("a"), &op);
        let tick = |id: &str| StoredResource {
            resource_type: "ticks".into(),
            id: id.into(),
            data: json!({"id": id, "n": 1}),
        };
        // Another session, another type, and an invalid item: all ignored.
        rt.state.upsert(&session("b"), tick("t0")).await.unwrap();
        let other = StoredResource {
            resource_type: "tocks".into(),
            ..tick("t1")
        };
        rt.state.upsert(&session("a"), other).await.unwrap();
        let invalid = StoredResource {
            data: json!({"id": "t2"}),
            ..tick("t2")
        };
        rt.state.upsert(&session("a"), invalid).await.unwrap();
        rt.state.upsert(&session("a"), tick("t3")).await.unwrap();

        let got = tokio::time::timeout(Duration::from_secs(1), feed.next())
            .await
            .unwrap();
        assert_eq!(got, Some(json!({"id": "t3", "n": 1})));
        let none = tokio::time::timeout(Duration::from_millis(100), feed.next()).await;
        assert!(none.is_err(), "nothing else is emitted");
    }

    #[tokio::test]
    async fn change_feed_is_empty_without_resource_type() {
        let rt = stream_runtime();
        let mut op = rt.bundle.operations[0].clone();
        op.resource_type = None;
        let mut feed = change_feed(&rt, session("a"), &op);
        assert_eq!(feed.next().await, None);
    }

    #[tokio::test]
    async fn open_stream_applies_scenario_stream_settings() {
        let mut rt = stream_runtime();
        let scenario = ScenarioRuntime {
            spec: ScenarioSpec {
                name: "two".into(),
                instructions: None,
                set: Some(json!({"n": 42})),
                status: None,
            },
            stream_items: Some(2),
            stream_item_interval: Some(Duration::from_millis(1)),
            ..ScenarioRuntime::default()
        };
        Arc::get_mut(&mut rt)
            .unwrap()
            .scenarios
            .insert("two".into(), scenario);
        let op = &rt.bundle.operations[0];
        let mut headers = HeaderMap::new();
        headers.insert(SCENARIO_HEADER, "two".parse().unwrap());
        let plan = plan(&rt, op, &headers, Presented::default()).await.unwrap();
        assert_eq!(plan.stream_items, Some(2));
        let call = OperationCall {
            operation_id: op.id.clone(),
            ..OperationCall::default()
        };
        let items: Vec<StreamItem> = open_stream(&rt, &plan, call, Vec::new())
            .await
            .unwrap()
            .collect()
            .await;
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.value["n"] == 42));

        // The header beats the scenario.
        headers.insert(STREAM_ITEMS_HEADER, "3".parse().unwrap());
        let plan = super::plan(&rt, op, &headers, Presented::default())
            .await
            .unwrap();
        assert_eq!(plan.stream_items, Some(3));
    }
}
