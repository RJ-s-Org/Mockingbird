//! The per-call pipeline every adapter shares: admission, data-plane auth,
//! override headers and scenario lookup ([`plan`]), state reads and writes around generation
//! ([`generate_once`], [`open_stream`]), state change feeds
//! ([`change_feed`]) and usage/request-log reporting ([`report`]).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::http::HeaderMap;
use mb_engine::consistency::{
    ENVELOPE_KEYS, item_identity, list_items, list_members, property_schema,
};
use mb_engine::{
    EngineError, FAKER_INVALID_REASON, GeneratedResponse, GenerationContext, OFFLINE_REASON,
    OperationCall, Source,
};
use mb_ext::{CallInfo, Rejection, Tenancy, UsageEvent};
use mb_llm::Usage;
use mb_mcm::{Binding, Effect, Operation, OperationKind, Protocol};
use mb_state::{CacheEpoch, CachedResponse, SessionKey, StateError, StoredResource};
use serde_json::{Map, Value};
use tracing::Instrument;

use crate::chaos::{self, CHAOS_HEADER, ChaosDecision};
pub(crate) use crate::dp_auth::Unauthorized;
use crate::dp_auth::authenticate;
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

/// List members of a read's answer whose stored items join its context.
const MAX_LIST_MEMBERS: usize = 3;
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
        // The engine reads the seed from the forwarded headers; only its
        // form is checked here.
        if let Some(s) = header_text(headers, mb_engine::SEED_HEADER)?
            && s.trim().parse::<u64>().is_err()
        {
            return Err(format!(
                "X-Mockingbird-Seed must be an integer from 0 to {}, got {s:?}",
                u64::MAX
            ));
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
    #[cfg(any(feature = "sse", feature = "graphql", feature = "grpc"))]
    pub stream_items: Option<usize>,
    pub tenancy: Tenancy,
    /// The engine the call generates with: the provider policy's choice for
    /// `tenancy` (see [`MockRuntime::engine_for`]).
    pub engine: std::sync::Arc<mb_engine::Engine>,
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
#[cfg(any(feature = "graphql", feature = "events"))]
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
#[cfg(feature = "grpc")]
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
#[cfg(feature = "grpc")]
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
    event.reasoning_tokens = usage.reasoning_tokens;
    rt.ext.usage.record(&event);
}

/// The answer to a rate-limited call of `op` when the contract declares a
/// `429` response: the engine's fallback for that variant (never an LLM
/// call). `None` when the operation declares no `429`.
#[cfg(any(feature = "http-rest", feature = "soap"))]
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
#[cfg(feature = "graphql")]
pub(crate) async fn check_key(
    rt: &MockRuntime,
    headers: &HeaderMap,
    query: &[(String, String)],
) -> Result<(), Unauthorized> {
    crate::dp_auth::verify_key(rt, headers, query).await
}

/// `pairs` without the mock's credential query parameters: they never
/// reach the engine or the record.
#[cfg(feature = "http-rest")]
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
    // The provider policy runs per call, with the call's tenancy; a
    // rejection is answered like a quota's.
    let engine = match rt.engine_for(&tenancy).await {
        Ok(engine) => engine,
        Err(r) => return Err(PlanFailure::new(PlanError::Rejected(r), Some(tenancy))),
    };
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
        #[cfg(any(feature = "sse", feature = "graphql", feature = "grpc"))]
        stream_items,
        tenancy,
        engine,
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
#[cfg(any(feature = "http-rest", feature = "grpc", feature = "soap"))]
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
    match prepare(rt, plan, call, response_schema).await {
        Prepared::Answered(answer) => Ok(answer),
        Prepared::Pending(pending) => {
            generate_pending(rt, plan, call, pending, response_schema).await
        }
    }
}

/// Generates the answer of a [`prepare`]d call, then [`complete`]s it.
pub(crate) async fn generate_pending(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    pending: Pending<'_>,
    response_schema: Option<&Value>,
) -> Result<GeneratedResponse, EngineError> {
    let ctx = GenerationContext {
        related: &pending.related,
        target: pending.target.as_ref(),
        forced_status: plan.forced_status,
        scenario: plan.scenario.map(|s| &s.spec),
        response_schema,
    };
    let generated = plan.engine.generate(&rt.bundle, plan.op, call, ctx).await?;
    Ok(complete(rt, plan, call, pending, generated).await)
}

/// What [`prepare`] found: an answer that needs no generation, or the
/// state a generation must take into account.
pub(crate) enum Prepared<'a> {
    Answered(GeneratedResponse),
    Pending(Pending<'a>),
}

/// A call to generate, then hand to [`complete`].
pub(crate) struct Pending<'a> {
    state: StateAccess<'a>,
    target_id: Option<String>,
    pub(crate) target: Option<StoredResource>,
    pub(crate) related: Vec<StoredResource>,
    /// The cache epoch before any state was read, and the answer's key,
    /// when the answer is cached.
    cache: Option<(CacheEpoch, String)>,
}

/// Reads the call's state and answers what needs no generation: an unknown
/// id (404), a stored resource, a cached answer.
pub(crate) async fn prepare<'a>(
    rt: &'a MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    response_schema: Option<&Value>,
) -> Prepared<'a> {
    let op = plan.op;
    let session = &plan.session;
    let mut state = StateAccess::new(rt);
    let resource_type = op.resource_type.as_deref();
    let mut target_id = target_id(op, call);
    // Before any state is read: an answer generated from older state must
    // not be cached after a write that lands meanwhile.
    let since = rt.state.response_cache().map(|c| c.epoch(session));
    let (target, related) = read_context(&mut state, session, op, target_id.as_deref(), call)
        .instrument(state_span())
        .await;
    // Found by another field (an issue's `number`): writes go to its id.
    if let Some(found) = &target {
        target_id = Some(found.id.clone());
    }

    if rt.unknown_ids == UnknownIds::NotFound
        && matches!(op.kind, OperationKind::Query)
        && plan.forced_status.is_none()
        && op.identity.as_ref().is_some_and(path_names_identity)
        && target.is_none()
        && state.available()
        && let (Some(t), Some(id)) = (resource_type, target_id.as_deref())
    {
        let detail = format!("{t} {id} does not exist in session {}", session.session);
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(rt.engine.not_found(&rt.bundle, op, call, &detail));
    }

    let target_missing = target.is_none();
    // A read naming a seed asks for that seed's answer, not for what a plain
    // read answered; a resource stored by a write or a fixture still stands.
    let target = target.filter(|stored| {
        !(matches!(op.kind, OperationKind::Query)
            && mb_engine::seed::request_seed(call).is_some()
            && rt.own_reads().holds(session, stored))
    });

    if matches!(op.kind, OperationKind::Query)
        && plain(plan)
        && let Some(stored) = &target
        && let Some(served) = rt
            .engine
            .serve_stored(&rt.bundle, op, &stored.data, response_schema)
    {
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(served);
    }

    let is_query = matches!(op.kind, OperationKind::Query);
    let cache = rt
        .state
        .response_cache()
        .filter(|_| is_query && rt.consistency && plain(plan));
    let key = cache.map(|_| {
        format!(
            "{}:{}",
            rt.bundle.source_digest,
            cache_key(op, call, response_schema)
        )
    });
    if let (Some(cache), Some(key)) = (cache, &key)
        && let Some(hit) = cache.get(session, key)
    {
        // A repeat of a fallback is answered at once, and still told apart.
        let (source, fallback_reason) = match hit.fallback_reason {
            Some(reason) => (
                Source::Fallback,
                Some(format!("repeat of an earlier fallback: {reason}")),
            ),
            None => (Source::Cache, None),
        };
        let served = GeneratedResponse {
            status: hit.status,
            content_type: hit.content_type,
            body: hit.body,
            source,
            usage: Usage::default(),
            fallback_reason,
        };
        // The answer's items were seeded when it was generated; writes and
        // resets that could remove them also clear the cache.
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(served);
    }

    // Last, so that a 404 or a cached answer costs no scan.
    let mut related = related;
    if is_query
        && target_missing
        && let Some(id) = target_id.as_deref()
    {
        let found = referrers(&mut state, session, op, id)
            .instrument(state_span())
            .await;
        for resource in found {
            if !holds(&related, &resource) {
                related.push(resource);
            }
        }
    }

    Prepared::Pending(Pending {
        state,
        target_id,
        target,
        related,
        cache: since.zip(key),
    })
}

/// Writes a generated answer's state (a 2xx command's resource, a list
/// read's seeded items), keeps the session alive, sends webhooks and caches
/// a read's answer.
pub(crate) async fn complete(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    pending: Pending<'_>,
    mut generated: GeneratedResponse,
) -> GeneratedResponse {
    let Pending {
        mut state,
        target_id,
        cache,
        ..
    } = pending;
    let op = plan.op;
    let session = &plan.session;
    let resource_type = op.resource_type.as_deref();
    let is_query = matches!(op.kind, OperationKind::Query);
    async {
        if (200..300).contains(&generated.status)
            && let Some(t) = resource_type
        {
            if matches!(op.kind, OperationKind::Command(Effect::Create)) {
                unique_create_id(rt, &mut state, session, op, t, call, &mut generated).await;
                next_numbers(rt, &mut state, session, t, &mut generated).await;
            }
            // A faker list standing in for a failed LLM call seeds nothing,
            // or its items would join the session's later LLM lists.
            let seeding = is_query
                && rt.consistency
                && target_id.is_none()
                && !transient_fallback(&generated);
            if !is_query || seeding {
                let origin = if is_query {
                    WriteOrigin::Seeded
                } else {
                    WriteOrigin::Call
                };
                write_state(
                    &mut state,
                    WriteRequest {
                        session,
                        op,
                        resource_type: t,
                        target_id: target_id.clone(),
                        generated: &generated,
                        request_body: call.body.as_ref(),
                        origin,
                    },
                )
                .await;
            }
        }
        if matches!(op.kind, OperationKind::Command(Effect::Create))
            && (200..300).contains(&generated.status)
        {
            store_wrapped(&mut state, session, resource_type, &generated).await;
        }
        // A request naming a seed asks for that seed's answer, and a query for
        // one view: the cache keeps those apart, so neither is the resource.
        if rt.consistency
            && plain(plan)
            && mb_engine::seed::request_seed(call).is_none()
            && call.query.is_empty()
            && !transient_fallback(&generated)
            && (200..300).contains(&generated.status)
            && !matches!(op.binding, Binding::Graphql { .. })
        {
            seed_own_read(
                &mut state,
                session,
                rt,
                op,
                target_id.as_deref(),
                &generated,
            )
            .await;
        }
        // An operation with no resource type is stateless, GraphQL stores the
        // objects of its answers under their own types, and a delete's answer
        // may name what it just removed.
        if (200..300).contains(&generated.status)
            && resource_type.is_some()
            && rt.consistency
            && !transient_fallback(&generated)
            && !matches!(op.binding, Binding::Graphql { .. })
            && !matches!(op.kind, OperationKind::Command(Effect::Delete))
            && let Some(body) = generated.body.as_ref()
        {
            let schema = op
                .output_for_status(generated.status)
                .and_then(|o| o.schema.as_ref());
            seed_entities(&mut state, session, rt, body, schema).await;
        }
        state.touch(session).await;
    }
    .instrument(state_span())
    .await;
    // Any successful command may change what a cached read would answer,
    // even one that stored nothing or stored another resource type (Slack's
    // `chat.postMessage` and `conversations.history`).
    if !is_query
        && (200..300).contains(&generated.status)
        && let Some(response_cache) = rt.state.response_cache()
    {
        response_cache.clear_session(session);
    }
    // Every caller of `complete` is one of these adapters.
    #[cfg(any(
        feature = "http-rest",
        feature = "graphql",
        feature = "grpc",
        feature = "soap"
    ))]
    crate::webhooks::emit::emit(
        rt,
        &plan.engine,
        op,
        call,
        &generated,
        target_id.as_deref(),
        session,
    );

    // Seeded first, then cached: a later list never clears a cached one.
    // A fallback is cached too: a schema the LLM cannot answer in time
    // would otherwise make every identical repeat wait as long again.
    if let (Some(response_cache), Some((since, key))) = (rt.state.response_cache(), cache)
        && (200..300).contains(&generated.status)
    {
        response_cache.put_if_current(
            session,
            key,
            resource_type,
            CachedResponse {
                status: generated.status,
                content_type: generated.content_type.clone(),
                body: generated.body.clone(),
                fallback_reason: transient_fallback(&generated)
                    .then(|| generated.fallback_reason.clone())
                    .flatten(),
            },
            since,
        );
    }
    generated
}

/// A faker answer standing in for a failed LLM call (an offline mock's
/// answers are its real ones, and a faker answer that only notes it found no
/// valid draw stands in for nothing).
pub(crate) fn transient_fallback(generated: &GeneratedResponse) -> bool {
    generated.source == Source::Fallback
        && generated
            .fallback_reason
            .as_deref()
            .is_some_and(|r| !r.starts_with(OFFLINE_REASON) && r != FAKER_INVALID_REASON)
}

/// Gives a create's answer a fresh identity when the session already holds
/// a resource under the generated one and the request did not ask for it.
async fn unique_create_id(
    rt: &MockRuntime,
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    resource_type: &str,
    call: &OperationCall,
    generated: &mut GeneratedResponse,
) {
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let Some(body) = generated.body.as_ref() else {
        return;
    };
    let Some(id) = body.get(field).and_then(id_string) else {
        return;
    };
    let requested = call
        .body
        .as_ref()
        .and_then(|b| b.get(field))
        .and_then(id_string);
    if requested.as_deref() == Some(id.as_str())
        || state.get(session, resource_type, &id).await.is_none()
    {
        return;
    }
    for _ in 0..3 {
        let Some(fresh) = rt.engine.fresh_identity(&rt.bundle, op, body) else {
            return;
        };
        let Some(fresh_id) = fresh.get(field).and_then(id_string) else {
            return;
        };
        if state.get(session, resource_type, &fresh_id).await.is_none() {
            generated.body = Some(fresh);
            return;
        }
    }
}

/// Stored resources scanned for a read by another field, or for the
/// numbers in use.
const MAX_SCAN: usize = 500;

/// What the pipeline looks up in a runtime's (fixed) bundle on every call,
/// computed once when the runtime is built.
#[derive(Debug, Default)]
pub(crate) struct BundleIndex {
    /// Each operation's position in `bundle.operations`, by id (the first
    /// one of a repeated id, as [`mb_mcm::Bundle::operation`] finds).
    by_id: HashMap<String, usize>,
    /// Each channel's position in `bundle.channels`, by id (the first one
    /// of a repeated id, as [`mb_mcm::Bundle::channel`] finds).
    #[cfg(any(test, feature = "events"))]
    channel_by_id: HashMap<String, usize>,
    /// Per resource type: the fields its operations take from the path
    /// (GitHub's issue `number`), sorted and deduplicated.
    path_fields: HashMap<String, Vec<String>>,
    /// The members every query operation's success schema lists.
    listed: HashSet<String>,
    /// Per operation, by position: the members its success schema lists.
    list_members: Vec<Vec<String>>,
    /// The resource type each `$defs` name is, from the query operations
    /// whose success schema or list items are that `$ref`: a read by path
    /// parameter first (`/repos/{owner}/{repo}`, not `/search/repositories`,
    /// names GitHub's `repository`), then a list, then any other read.
    types_by_def: HashMap<String, String>,
    /// The resource types of the query operations.
    read_types: HashSet<String>,
    /// Per resource type, the position of its first query by path parameter
    /// with a success schema: the read a stored resource must suit.
    detail_reads: HashMap<String, usize>,
    /// Per resource type with no such read, what a stored resource must
    /// suit instead, with the position of the read it comes from: a list's
    /// item schema, else a singleton read's success schema (`/user`).
    other_shapes: HashMap<String, (usize, Value)>,
    /// Per resource type, the types whose answers have a reference field of
    /// it (see [`reference_targets`]), sorted and deduplicated.
    referrers: HashMap<String, Vec<String>>,
}

impl BundleIndex {
    pub(crate) fn new(bundle: &mb_mcm::Bundle) -> Self {
        let mut by_id = HashMap::with_capacity(bundle.operations.len());
        let mut path_fields: HashMap<String, Vec<String>> = HashMap::new();
        let mut list_members_of = Vec::with_capacity(bundle.operations.len());
        let mut listed = HashSet::new();
        let mut read_types = HashSet::new();
        let mut detail_reads = HashMap::new();
        let mut ranked_defs: HashMap<String, (u8, String)> = HashMap::new();
        let mut shapes = TypeShapes::new();
        let mut referrers: HashMap<String, Vec<String>> = HashMap::new();
        for (i, op) in bundle.operations.iter().enumerate() {
            by_id.entry(op.id.clone()).or_insert(i);
            if let (Some(t), Some(field)) = (
                op.resource_type.as_deref(),
                op.identity.as_ref().and_then(|i| i.path_field.as_deref()),
            ) {
                path_fields
                    .entry(t.to_string())
                    .or_default()
                    .push(field.to_string());
            }
            let members = op
                .success_output()
                .and_then(|o| o.schema.as_ref())
                .map(|schema| list_members(bundle, schema))
                .unwrap_or_default();
            if let Some(t) = op.resource_type.as_deref() {
                record_referrers(bundle, op, t, &members, &mut referrers);
            }
            if matches!(op.kind, OperationKind::Query) {
                listed.extend(members.iter().cloned());
                if let Some(t) = op.resource_type.as_deref() {
                    read_types.insert(t.to_string());
                    let by_path = op.identity.as_ref().is_some_and(|i| i.path_param.is_some())
                        && !op.is_action();
                    if by_path && op.success_output().is_some_and(|o| o.schema.is_some()) {
                        detail_reads.entry(t.to_string()).or_insert(i);
                    }
                    let lists = list_item_schemas(bundle, op, &members);
                    rank_defs(op, t, by_path, &lists, &mut ranked_defs);
                    record_shapes(op, i, t, by_path, &members, &lists, &mut shapes);
                }
            }
            list_members_of.push(members);
        }
        for fields in path_fields.values_mut().chain(referrers.values_mut()) {
            fields.sort_unstable();
            fields.dedup();
        }
        #[cfg(any(test, feature = "events"))]
        let mut channel_by_id = HashMap::with_capacity(bundle.channels.len());
        #[cfg(any(test, feature = "events"))]
        for (i, channel) in bundle.channels.iter().enumerate() {
            channel_by_id.entry(channel.id.clone()).or_insert(i);
        }
        Self {
            by_id,
            #[cfg(any(test, feature = "events"))]
            channel_by_id,
            path_fields,
            listed,
            list_members: list_members_of,
            types_by_def: ranked_defs
                .into_iter()
                .map(|(def, (_, t))| (def, t))
                .collect(),
            read_types,
            detail_reads,
            other_shapes: shapes
                .into_iter()
                .map(|(t, (_, i, schema))| (t, (i, schema)))
                .collect(),
            referrers,
        }
    }

    /// The position of operation `id`.
    pub(crate) fn operation(&self, id: &str) -> Option<usize> {
        self.by_id.get(id).copied()
    }

    /// The position of channel `id`.
    #[cfg(any(test, feature = "events"))]
    pub(crate) fn channel(&self, id: &str) -> Option<usize> {
        self.channel_by_id.get(id).copied()
    }

    fn path_fields(&self, resource_type: &str) -> &[String] {
        self.path_fields
            .get(resource_type)
            .map_or(&[], Vec::as_slice)
    }

    fn list_members(&self, op: usize) -> &[String] {
        self.list_members.get(op).map_or(&[], Vec::as_slice)
    }

    fn referrers(&self, resource_type: &str) -> &[String] {
        self.referrers.get(resource_type).map_or(&[], Vec::as_slice)
    }
}

/// Records in `referrers` the types the fields of `op`'s answers (as type
/// `t`, its list items included) refer to.
fn record_referrers(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    t: &str,
    members: &[String],
    referrers: &mut HashMap<String, Vec<String>>,
) {
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let items = list_item_schemas(bundle, op, members);
    let shapes = std::iter::once(schema).chain(items.iter().map(|(_, s)| s));
    for shape in shapes {
        for part in schema_parts(bundle, Some(shape)).0 {
            let Some(properties) = part.get("properties").and_then(Value::as_object) else {
                continue;
            };
            for (field, property) in properties {
                for target in reference_targets(bundle, field, property) {
                    referrers.entry(target).or_default().push(t.to_string());
                }
            }
        }
    }
}

/// The types a field of an answer's schema can refer to: `<x>s` and `<x>`
/// for `<x>Id`/`<x>_id`, or for a member `<x>` that can hold a string.
fn reference_targets(bundle: &mb_mcm::Bundle, field: &str, property: &Value) -> Vec<String> {
    let prefix = reference_prefix(field)
        .or_else(|| (!field.is_empty() && holds_a_string(bundle, property)).then_some(field));
    prefix.map_or_else(Vec::new, |x| vec![format!("{x}s"), x.to_string()])
}

/// Whether `schema` admits a string, or a list of them.
fn holds_a_string(bundle: &mb_mcm::Bundle, schema: &Value) -> bool {
    let string = |s: &Value| {
        schema_parts(bundle, Some(s))
            .0
            .iter()
            .any(|p| match p.get("type") {
                Some(Value::String(t)) => t == "string",
                Some(Value::Array(types)) => types.iter().any(|t| t == "string"),
                _ => false,
            })
    };
    string(schema)
        || schema_parts(bundle, Some(schema))
            .0
            .iter()
            .filter_map(|p| p.get("items"))
            .any(string)
}

/// The item schemas of `op`'s success schema: each of its list `members`'
/// (with the member), and the root's when it is an array itself.
fn list_item_schemas(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    members: &[String],
) -> Vec<(Option<String>, Value)> {
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return Vec::new();
    };
    let items_of = |list: &Value| {
        let (parts, _) = schema_parts(bundle, Some(list));
        parts.iter().find_map(|p| p.get("items")).cloned()
    };
    let mut out: Vec<(Option<String>, Value)> = members
        .iter()
        .filter_map(|m| {
            let items = items_of(&property_schema(bundle, schema, m)?)?;
            Some((Some(m.clone()), items))
        })
        .collect();
    out.extend(items_of(schema).map(|items| (None, items)));
    out
}

/// Records in `ranked` the `$defs` names `op` reads as resource type `t`,
/// keeping the best-ranked type of each (see [`BundleIndex::types_by_def`]).
fn rank_defs(
    op: &Operation,
    t: &str,
    by_path: bool,
    lists: &[(Option<String>, Value)],
    ranked: &mut HashMap<String, (u8, String)>,
) {
    const DETAIL: u8 = 0;
    const LIST: u8 = 1;
    const OTHER: u8 = 2;
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let mut found: Vec<(u8, String)> = Vec::new();
    if let Some(def) = def_name(schema) {
        found.push((if by_path { DETAIL } else { OTHER }, def.to_string()));
    }
    let items = lists.iter().filter_map(|(_, items)| def_name(items));
    found.extend(items.map(|d| (LIST, d.to_string())));
    for (rank, def) in found {
        let held = ranked.entry(def).or_insert_with(|| (rank, t.to_string()));
        if rank < held.0 {
            *held = (rank, t.to_string());
        }
    }
}

/// Per resource type: the best rank seen, the read's position and the schema.
type TypeShapes = HashMap<String, (u8, usize, Value)>;

/// Records in `shapes` what query `op` (at position `i`, of type `t`) says a
/// resource looks like (see [`BundleIndex::other_shapes`]): a list's items
/// describe the type the member names and, for the root, an envelope or an
/// only member, `t`; with no list, its success schema describes `t`.
fn record_shapes(
    op: &Operation,
    i: usize,
    t: &str,
    by_path: bool,
    members: &[String],
    lists: &[(Option<String>, Value)],
    shapes: &mut TypeShapes,
) {
    const ITEMS: u8 = 0;
    const SINGLETON: u8 = 1;
    let mut record = |t: &str, rank: u8, schema: &Value| {
        let held = shapes
            .entry(t.to_string())
            .or_insert_with(|| (rank, i, schema.clone()));
        if rank < held.0 {
            *held = (rank, i, schema.clone());
        }
    };
    for (member, items) in lists {
        match member.as_deref() {
            None => record(t, ITEMS, items),
            Some(m) if ENVELOPE_KEYS.contains(&m) || members.len() == 1 => {
                record(t, ITEMS, items);
                if !ENVELOPE_KEYS.contains(&m) {
                    record(m, ITEMS, items);
                }
            }
            Some(m) => record(m, ITEMS, items),
        }
    }
    if lists.is_empty()
        && !by_path
        && let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref())
    {
        record(t, SINGLETON, schema);
    }
}

/// The `$defs` name `schema` is a `$ref` to.
fn def_name(schema: &Value) -> Option<&str> {
    schema.get("$ref")?.as_str()?.strip_prefix("#/$defs/")
}

/// Schemas [`schema_parts`] returns, at most.
const MAX_SCHEMA_PARTS: usize = 32;

/// What `schema` is made of, its `$ref`s followed and its
/// `anyOf`/`oneOf`/`allOf` branches flattened, with the `$defs` names met.
fn schema_parts<'a>(
    bundle: &'a mb_mcm::Bundle,
    schema: Option<&'a Value>,
) -> (Vec<&'a Value>, Vec<&'a str>) {
    fn visit<'a>(
        bundle: &'a mb_mcm::Bundle,
        schema: &'a Value,
        depth: usize,
        parts: &mut Vec<&'a Value>,
        defs: &mut Vec<&'a str>,
    ) {
        if depth > 8 || parts.len() >= MAX_SCHEMA_PARTS {
            return;
        }
        if let Some(def) = def_name(schema) {
            defs.push(def);
            if let Some(target) = bundle.defs.get(def) {
                visit(bundle, target, depth + 1, parts, defs);
            }
            return;
        }
        parts.push(schema);
        for key in ["anyOf", "oneOf", "allOf"] {
            for branch in schema
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                visit(bundle, branch, depth + 1, parts, defs);
            }
        }
    }
    let (mut parts, mut defs) = (Vec::new(), Vec::new());
    if let Some(schema) = schema {
        visit(bundle, schema, 0, &mut parts, &mut defs);
    }
    (parts, defs)
}

/// The resource type of an object found under `member`, shaped by `$defs`
/// entry `def`: the def's type, else the listed type named `member` or
/// `<member>s`, one a read can name by path first (GitHub's `user` member is
/// a `/users/{username}`, not the caller's `/user`).
pub(crate) fn entity_type(index: &BundleIndex, def: Option<&str>, member: &str) -> Option<String> {
    if let Some(t) = def.and_then(|d| index.types_by_def.get(d)) {
        return Some(t.clone());
    }
    if member.is_empty() {
        return None;
    }
    let names = [member.to_string(), format!("{member}s")];
    // An envelope's member (`items`) names no type of its own.
    let is_type = |name: &String| {
        index.read_types.contains(name)
            || (index.listed.contains(name) && !ENVELOPE_KEYS.contains(&name.as_str()))
    };
    names
        .iter()
        .find(|name| index.detail_reads.contains_key(*name))
        .or_else(|| names.iter().find(|name| is_type(name)))
        .cloned()
}

/// Aliases [`PathAliases`] holds at most; it is emptied when full.
const MAX_PATH_ALIASES: usize = 4096;

/// Which stored resource a path field value names (GitHub's issue `number`
/// 412 → the issue stored under its `id`), remembered from this runtime's
/// writes so that a read by number gets the resource instead of scanning
/// the session's resources of the type. Only a hint: a hit is used only
/// when the resource it names still holds that value, and a read that
/// misses (written elsewhere: the Admin API, another node, before a
/// restart) still scans, then remembers what it found.
#[derive(Debug, Default)]
pub(crate) struct PathAliases(std::sync::Mutex<HashMap<PathAlias, String>>);

#[derive(Debug, PartialEq, Eq, Hash)]
struct PathAlias {
    session: SessionKey,
    resource_type: String,
    field: String,
    value: String,
}

impl PathAliases {
    /// Remembers that resource `id` of type `t` holds each of `values`
    /// (field, value).
    fn remember(&self, session: &SessionKey, t: &str, id: &str, values: Vec<(&str, String)>) {
        if values.is_empty() {
            return;
        }
        let Ok(mut aliases) = self.0.lock() else {
            return;
        };
        if aliases.len() + values.len() > MAX_PATH_ALIASES {
            aliases.clear();
        }
        for (field, value) in values {
            let key = PathAlias {
                session: session.clone(),
                resource_type: t.to_string(),
                field: field.to_string(),
                value,
            };
            aliases.insert(key, id.to_string());
        }
    }

    /// The id last remembered for `field` = `value`.
    fn id_of(&self, session: &SessionKey, t: &str, field: &str, value: &str) -> Option<String> {
        let key = PathAlias {
            session: session.clone(),
            resource_type: t.to_string(),
            field: field.to_string(),
            value: value.to_string(),
        };
        self.0.lock().ok()?.get(&key).cloned()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().map(|a| a.len()).unwrap_or_default()
    }
}

/// Answers [`OwnReads`] holds at most; it is emptied when full.
const MAX_OWN_READS: usize = 4096;

/// Which stored resources are a plain read's own answer, not a write's or a
/// fixture's: a read naming a seed is not answered from them. Remembered
/// from this runtime's reads with a hash of the answer, so any later write
/// that changes the resource ends it with no bookkeeping. Only a hint: on
/// another node, after a restart or once emptied, the resource counts as
/// written and answers such a read as any stored one does.
#[derive(Debug, Default)]
pub(crate) struct OwnReads(std::sync::Mutex<HashMap<OwnRead, u64>>);

#[derive(Debug, PartialEq, Eq, Hash)]
struct OwnRead {
    session: SessionKey,
    resource_type: String,
    id: String,
}

impl OwnReads {
    fn key(session: &SessionKey, r: &StoredResource) -> OwnRead {
        OwnRead {
            session: session.clone(),
            resource_type: r.resource_type.clone(),
            id: r.id.clone(),
        }
    }

    fn digest(data: &Value) -> u64 {
        use std::hash::{DefaultHasher, Hasher};
        let mut hasher = DefaultHasher::new();
        hash_canonical(data, &mut hasher);
        hasher.finish()
    }

    fn remember(&self, session: &SessionKey, r: &StoredResource) {
        let Ok(mut reads) = self.0.lock() else {
            return;
        };
        if reads.len() >= MAX_OWN_READS {
            reads.clear();
        }
        reads.insert(Self::key(session, r), Self::digest(&r.data));
    }

    /// Whether `r` is, as it stands, a plain read's own answer.
    fn holds(&self, session: &SessionKey, r: &StoredResource) -> bool {
        self.0
            .lock()
            .is_ok_and(|reads| reads.get(&Self::key(session, r)) == Some(&Self::digest(&r.data)))
    }
}

impl MockRuntime {
    /// The operation `id` of [`MockRuntime::bundle`] (the first of a
    /// repeated id), without scanning the operations. (SOAP keeps its own
    /// operation positions.)
    #[cfg(any(test, feature = "http-rest", feature = "graphql", feature = "grpc"))]
    pub(crate) fn operation(&self, id: &str) -> Option<&Operation> {
        self.bundle.operations.get(self.index.operation(id)?)
    }

    /// The channel `id` of [`MockRuntime::bundle`] (the first of a repeated
    /// id), without scanning the channels.
    #[cfg(any(test, feature = "events"))]
    pub(crate) fn channel(&self, id: &str) -> Option<&mb_mcm::Channel> {
        self.bundle.channels.get(self.index.channel(id)?)
    }
}

/// Numbers a create's answer must take above those the session holds: each
/// integer field a read of the resource type takes from its path (GitHub's
/// issue `number`). A clashing number moves past the highest stored one,
/// and URLs naming it (`…/issues/412`) follow.
async fn next_numbers(
    rt: &MockRuntime,
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    resource_type: &str,
    generated: &mut GeneratedResponse,
) {
    let fields = rt.index().path_fields(resource_type);
    if fields.is_empty() {
        return;
    }
    let Some(body) = generated.body.as_mut() else {
        return;
    };
    let stored = state.list(session, resource_type, MAX_SCAN).await;
    for field in fields.iter().map(String::as_str) {
        let Some(number) = body.get(field).and_then(Value::as_i64) else {
            continue;
        };
        let highest = stored
            .iter()
            .filter_map(|r| r.data.get(field).and_then(Value::as_i64))
            .max();
        let Some(highest) = highest.filter(|h| number <= *h) else {
            continue;
        };
        let next = highest + 1;
        body[field] = Value::from(next);
        let (old, new) = (
            format!("/{resource_type}/{number}"),
            format!("/{resource_type}/{next}"),
        );
        renumber_urls(body, &old, &new);
    }
}

/// Replaces path `old` with `new` in every string of `value` where it ends
/// a path segment (`…/issues/412`, `…/issues/412/comments`).
fn renumber_urls(value: &mut Value, old: &str, new: &str) {
    match value {
        Value::String(text) => {
            let mut out = String::with_capacity(text.len());
            let mut rest = text.as_str();
            while let Some(at) = rest.find(old) {
                let after = &rest[at + old.len()..];
                let ends = after.is_empty() || after.starts_with(['/', '?', '#']);
                out.push_str(&rest[..at]);
                out.push_str(if ends { new } else { old });
                rest = after;
            }
            out.push_str(rest);
            *text = out;
        }
        Value::Array(items) => items.iter_mut().for_each(|v| renumber_urls(v, old, new)),
        Value::Object(obj) => obj.values_mut().for_each(|v| renumber_urls(v, old, new)),
        _ => {}
    }
}

/// Fields that identify a wrapped created object, in order: Slack's
/// message is keyed by `ts`, Twilio's resources by `sid`.
const WRAPPED_IDS: [&str; 4] = ["id", "ts", "sid", "uuid"];

/// Stores each object a create's answer wraps under a member `<x>` (Slack's
/// `chat.postMessage` → `{ok, ts, message: {...}}`) as a resource of type
/// `<x>s` when a read lists `<x>s` (`conversations.history`'s `messages`),
/// so that read is generated knowing it. The create's own member, one of the
/// operation's own type (`resource_type`), replaces what the session holds.
/// Any other is a mention, stored under what [`seed_entities`] requires of
/// one: it replaces what is held only when that cannot key it, having no
/// `id` or `number` (Slack's `ts`), and otherwise is stored only when new.
async fn store_wrapped(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    resource_type: Option<&str>,
    generated: &GeneratedResponse,
) {
    let Some(Value::Object(body)) = generated.body.as_ref() else {
        return;
    };
    let rt = state.rt;
    let listed = &rt.index().listed;
    for (member, value) in body {
        let plural = format!("{member}s");
        if !value.is_object() || !listed.contains(&plural) {
            continue;
        }
        let Some(id) = WRAPPED_IDS
            .iter()
            .find_map(|f| value.get(*f).and_then(id_string))
        else {
            continue;
        };
        let own = resource_type == Some(member.as_str()) || resource_type == Some(plural.as_str());
        let unkeyed = value.get("id").is_none() && value.get("number").is_none();
        let resource = StoredResource {
            resource_type: plural,
            id,
            data: value.clone(),
        };
        if own {
            state.upsert(session, resource).await;
        } else if rt.consistency
            && !transient_fallback(generated)
            && entity_fits(rt, &resource.resource_type, &resource.data)
        {
            if unkeyed {
                state.upsert(session, resource).await;
            } else {
                state.insert_seeded(session, resource).await;
            }
        }
    }
}

/// Stores a generated read by id as the resource it read, when it answers
/// with its type's by-path schema: otherwise a later answer's mention would
/// be stored and change what repeating this read returns. It goes under the
/// target id when the path names the identity. A path parameter naming no
/// field (`/repos/{o}/{r}`) is no key, or one owner's answer would answer for
/// another's, so that answer goes under its own id. A route read by another
/// field (`/issues/{number}`) stores nothing: its answer's `id` comes from
/// the path and is no resource's, so a list would store the issue again.
async fn seed_own_read(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    op: &Operation,
    target_id: Option<&str>,
    generated: &GeneratedResponse,
) {
    let (true, Some(t), Some(target_id), Some(identity), Some(body)) = (
        matches!(op.kind, OperationKind::Query),
        op.resource_type.as_deref(),
        target_id,
        op.identity.as_ref(),
        generated.body.as_ref(),
    ) else {
        return;
    };
    let index = rt.index();
    let detail_schema = index.detail_reads.get(t).and_then(|i| {
        rt.bundle()
            .operations
            .get(*i)?
            .success_output()?
            .schema
            .as_ref()
    });
    let own_schema = op
        .output_for_status(generated.status)
        .and_then(|o| o.schema.as_ref());
    if detail_schema.is_none() || detail_schema != own_schema {
        return;
    }
    if identity.path_field.is_some() {
        return;
    }
    let id = if path_names_identity(identity) {
        Some(target_id.to_string())
    } else {
        body.get(&identity.field).and_then(id_string)
    };
    let Some(id) = id else {
        return;
    };
    let resource = StoredResource {
        resource_type: t.to_string(),
        id,
        data: body.clone(),
    };
    if state.insert_seeded(session, resource.clone()).await {
        rt.own_reads().remember(session, &resource);
    }
}

/// How deep an answer is walked for entities: containers below its root.
const MAX_ENTITY_DEPTH: usize = 6;
/// Entities one answer may seed.
const MAX_ENTITIES: usize = 100;

/// Stores what a 2xx answer mentions so that a later read returns it: each
/// object below the answer's root with an `id` or `number`, a known type (see
/// [`entity_type`]) and, where its type has a read by path parameter, a value
/// that read would serve. What the session already holds is left as it is.
async fn seed_entities(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    body: &Value,
    schema: Option<&Value>,
) {
    let mut walk = EntityWalk {
        rt,
        considered: 0,
        found: Vec::new(),
    };
    walk.visit(body, schema, "", 0);
    for resource in walk.found {
        state.insert_seeded(session, resource).await;
    }
}

/// One walk of an answer for [`seed_entities`].
struct EntityWalk<'a> {
    rt: &'a MockRuntime,
    /// Objects taken for entities, stored or refused.
    considered: usize,
    found: Vec<StoredResource>,
}

impl<'a> EntityWalk<'a> {
    fn visit(&mut self, value: &Value, schema: Option<&'a Value>, member: &str, depth: usize) {
        if depth > MAX_ENTITY_DEPTH || self.considered >= MAX_ENTITIES {
            return;
        }
        let bundle: &'a mb_mcm::Bundle = self.rt.bundle();
        let (parts, defs) = schema_parts(bundle, schema);
        match value {
            Value::Array(items) => {
                let item = parts.iter().find_map(|p| p.get("items"));
                for value in items {
                    self.visit(value, item, member, depth + 1);
                }
            }
            Value::Object(fields) => {
                if depth > 0 {
                    self.consider(value, &defs, member);
                }
                for (field, value) in fields {
                    if !value.is_object() && !value.is_array() {
                        continue;
                    }
                    let inner = parts
                        .iter()
                        .find_map(|p| p.get("properties")?.get(field.as_str()));
                    // `data` in Stripe's `charges: {data: [...]}` holds `charges`.
                    let named = if ENVELOPE_KEYS.contains(&field.as_str()) {
                        member
                    } else {
                        field
                    };
                    self.visit(value, inner, named, depth + 1);
                }
            }
            _ => {}
        }
    }

    fn consider(&mut self, value: &Value, defs: &[&str], member: &str) {
        let Some(id) = ["id", "number"]
            .iter()
            .find_map(|f| value.get(*f).and_then(id_string))
        else {
            return;
        };
        let index = self.rt.index();
        let def = defs
            .iter()
            .copied()
            .find(|d| index.types_by_def.contains_key(*d));
        let Some(resource_type) = entity_type(index, def, member) else {
            return;
        };
        if self.considered >= MAX_ENTITIES {
            return;
        }
        self.considered += 1;
        if entity_fits(self.rt, &resource_type, value) {
            self.found.push(StoredResource {
                resource_type,
                id,
                data: value.clone(),
            });
        }
    }
}

/// Whether `value` suits resource type `t`: its by-path read would serve it
/// (stored unservable, a later read would fall to the faker), else its list
/// items or singleton read accept it. A type with neither has nothing to
/// check against.
fn entity_fits(rt: &MockRuntime, t: &str, value: &Value) -> bool {
    let index = rt.index();
    let bundle = rt.bundle();
    let (read, shape) = match index.detail_reads.get(t) {
        Some(i) => (Some(*i), None),
        None => match index.other_shapes.get(t) {
            Some((i, schema)) => (Some(*i), Some(schema)),
            None => (None, None),
        },
    };
    let Some(op) = read.and_then(|i| bundle.operations.get(i)) else {
        return true;
    };
    rt.engine.serve_stored(bundle, op, value, shape).is_some()
}

/// The response cache key of a read: its operation, path parameters,
/// query (sorted), a query operation's body, the narrowed selection and the
/// call's faker seed (`X-Mockingbird-Seed`).
fn cache_key(op: &Operation, call: &OperationCall, response_schema: Option<&Value>) -> String {
    use std::hash::{DefaultHasher, Hasher};
    let body = match op.kind {
        OperationKind::Query => call.body.as_ref().map(canonical),
        _ => None,
    };
    let selection = response_schema.map(|schema| {
        let mut hasher = DefaultHasher::new();
        hash_canonical(schema, &mut hasher);
        format!("{:016x}", hasher.finish())
    });
    serde_json::json!([
        op.id,
        canonical(&Value::Object(call.path_params.clone())),
        canonical(&Value::Object(call.query.clone())),
        body,
        selection,
        mb_engine::seed::request_seed(call),
    ])
    .to_string()
}

/// `value` with every object's keys sorted, whatever map order serde_json
/// keeps.
pub(crate) fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(obj) => {
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            // Build an array of pairs: ordered with or without `preserve_order`.
            Value::Array(
                keys.into_iter()
                    .map(|k| Value::Array(vec![Value::String(k.clone()), canonical(&obj[k])]))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// Feeds `value` to `hasher` with every object's keys sorted, like hashing
/// [`canonical`]'s output without building it: equal values (whatever their
/// map order) hash equally.
pub(crate) fn hash_canonical<H: std::hash::Hasher>(value: &Value, hasher: &mut H) {
    use std::hash::Hash;
    std::mem::discriminant(value).hash(hasher);
    match value {
        Value::Null => {}
        Value::Bool(b) => b.hash(hasher),
        Value::Number(n) => n.hash(hasher),
        Value::String(s) => s.hash(hasher),
        Value::Array(items) => {
            items.len().hash(hasher);
            for item in items {
                hash_canonical(item, hasher);
            }
        }
        Value::Object(obj) => hash_canonical_members(obj, hasher),
    }
}

/// [`hash_canonical`] of an object's members.
pub(crate) fn hash_canonical_members<H: std::hash::Hasher>(
    obj: &Map<String, Value>,
    hasher: &mut H,
) {
    use std::hash::Hash;
    let mut members: Vec<(&String, &Value)> = obj.iter().collect();
    members.sort_unstable_by(|a, b| a.0.cmp(b.0));
    members.len().hash(hasher);
    for (key, member) in members {
        key.hash(hasher);
        hash_canonical(member, hasher);
    }
}

/// Whether the path parameter carries the value state is keyed by: it is
/// the identity field itself, or id-like (`id`, `orderId`, `movie_id`). A
/// `/users/{username}` read keyed by `id` cannot tell a missing user.
fn path_names_identity(identity: &mb_mcm::Identity) -> bool {
    let Some(param) = identity.path_param.as_deref() else {
        return false;
    };
    param == identity.field
        || identity.path_field.is_some()
        || param.eq_ignore_ascii_case("id")
        || param.ends_with("Id")
        || param.ends_with("_id")
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
#[cfg(any(feature = "sse", feature = "graphql", feature = "grpc"))]
pub(crate) async fn open_stream(
    rt: &std::sync::Arc<MockRuntime>,
    plan: &CallPlan<'_>,
    call: OperationCall,
    request_stream: Vec<Value>,
) -> Result<futures::stream::BoxStream<'static, mb_engine::StreamItem>, EngineError> {
    use std::sync::Arc;

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
    let ctx = mb_engine::StreamContext {
        related,
        target,
        scenario: plan.scenario.map(|s| s.spec.clone()),
        request_stream,
        item_interval: plan.scenario.and_then(|s| s.stream_item_interval),
    };
    plan.engine
        .generate_stream(Arc::clone(&rt.bundle), op, call, ctx, plan.stream_items)
}

/// Whether a stored resource can be sent as a stream item, when the
/// caller reads less than the whole item (a GraphQL selection).
#[cfg(any(feature = "sse", feature = "graphql"))]
pub(crate) type ItemCheck = std::sync::Arc<dyn Fn(&Value) -> bool + Send + Sync>;

/// The data of every resource of `op`'s resource type upserted in
/// `session` from now on, when it is a valid stream item of `op` (or
/// passes `check`). Events missed because the receiver lagged are skipped.
/// Empty when `op` has no resource type.
#[cfg(any(feature = "sse", feature = "graphql"))]
pub(crate) fn change_feed(
    rt: &std::sync::Arc<MockRuntime>,
    session: SessionKey,
    op: &Operation,
    check: Option<ItemCheck>,
) -> futures::stream::BoxStream<'static, Value> {
    use futures::StreamExt;
    use futures::stream;
    use mb_state::ChangeKind;
    use std::sync::Arc;
    use tokio::sync::broadcast::error::RecvError;

    let Some(resource_type) = op.resource_type.clone() else {
        return stream::empty().boxed();
    };
    let feed = Feed {
        rt: Arc::clone(rt),
        op: op.clone(),
        session,
        resource_type,
        check,
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
            let sendable = match &feed.check {
                Some(check) => check(&resource.data),
                None => feed
                    .rt
                    .engine
                    .conform_item(&feed.rt.bundle, &feed.op, &resource.data),
            };
            if sendable {
                return Some((resource.data, feed));
            }
        }
    })
    .boxed()
}

#[cfg(any(feature = "sse", feature = "graphql"))]
struct Feed {
    rt: std::sync::Arc<MockRuntime>,
    op: Operation,
    session: SessionKey,
    resource_type: String,
    check: Option<ItemCheck>,
    rx: tokio::sync::broadcast::Receiver<mb_state::StateChanged>,
}

/// One served call, as reported to the usage sink and the request
/// recorder.
#[derive(Clone)]
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
    report_with(rt, headers, r, true, true).await;
}

/// [`report`] to the usage sink only: the call is recorded as part of a
/// larger request (see [`record_request`]).
#[cfg(feature = "graphql")]
pub(crate) async fn report_usage(rt: &MockRuntime, headers: &HeaderMap, r: Report) {
    report_with(rt, headers, r, true, false).await;
}

/// Hands one request, whose calls were metered on their own, to the
/// request recorder.
#[cfg(feature = "graphql")]
pub(crate) fn record_request(rt: &MockRuntime, r: Report) {
    record(rt, r);
}

/// [`report`] to the request recorder only: the call's usage was already
/// recorded item by item (see [`meter`]).
#[cfg(feature = "grpc")]
pub(crate) async fn report_unmetered(rt: &MockRuntime, headers: &HeaderMap, r: Report) {
    report_with(rt, headers, r, false, true).await;
}

async fn report_with(
    rt: &MockRuntime,
    headers: &HeaderMap,
    mut r: Report,
    metered: bool,
    recorded: bool,
) {
    if !metered {
        if recorded {
            record(rt, r);
        }
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
        event.reasoning_tokens = r.usage.reasoning_tokens;
        rt.ext.usage.record(&event);
    }
    if recorded {
        record(rt, r);
    }
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
#[cfg(feature = "events")]
pub(crate) fn is_recordable_header(rt: &MockRuntime, name: &str) -> bool {
    !is_sensitive(name)
        && !rt
            .credential_names()
            .headers
            .contains(&name.to_ascii_lowercase())
}

/// Headers forwarded to the engine (and the prompt), with
/// `X-Mockingbird-Seed` when the mock honours override headers.
pub(crate) fn forwarded_headers(rt: &MockRuntime, headers: &HeaderMap) -> Map<String, Value> {
    let mut out = safe_headers(rt, headers, false);
    if rt.header_overrides
        && let Some(seed) = headers
            .get(mb_engine::SEED_HEADER)
            .and_then(|v| v.to_str().ok())
    {
        out.insert(mb_engine::SEED_HEADER.into(), Value::String(seed.into()));
    }
    out
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
        (Some(t), Some(id)) => match state.get(session, t, id).await {
            Some(hit) => Some(hit),
            None => match op.identity.as_ref() {
                Some(identity) => match identity.path_field.as_deref() {
                    Some(field) => by_path_fields(state, session, t, &[field], id, None).await,
                    None if state.rt.consistency
                        && identity.path_param.is_some()
                        && !path_names_identity(identity) =>
                    {
                        let owned = owned_name(op, call);
                        by_path_fields(state, session, t, &NAME_FIELDS, id, owned.as_deref()).await
                    }
                    None => None,
                },
                None => None,
            },
        },
        _ => None,
    };
    let related = related(state, session, op, target_id, target.as_ref(), call).await;
    (target, related)
}

/// The value `data` holds for each of `fields`, as (field, value).
fn path_values<'f>(fields: &'f [String], data: &Value) -> Vec<(&'f str, String)> {
    fields
        .iter()
        .filter_map(|f| Some((f.as_str(), data.get(f).and_then(id_string)?)))
        .collect()
}

/// Fields a path parameter that names no field may mean: `/repos/{o}/{r}`
/// finds a repository seeded under its numeric id by its `name`.
const NAME_FIELDS: [&str; 4] = ["name", "login", "slug", "key"];

/// `{o}/{r}` of a route ending in those two parameters, which a stored
/// `full_name` must end with. Any other route gives none: a person's
/// `full_name` is no path.
fn owned_name(op: &Operation, call: &OperationCall) -> Option<String> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let mut segments = path.rsplit('/').filter(|s| !s.is_empty());
    let mut value = || {
        let name = segments.next()?.strip_prefix('{')?.strip_suffix('}')?;
        call.path_params.get(name).and_then(id_string)
    };
    let name = value()?;
    let owner = value()?;
    Some(format!("{owner}/{name}"))
}

/// Whether stored `r`'s `full_name`, when it has one, ends with path
/// segments `owned`.
fn full_name_fits(r: &StoredResource, owned: Option<&str>) -> bool {
    let full_name = r.data.get("full_name").and_then(Value::as_str);
    match (owned, full_name) {
        (Some(owned), Some(full_name)) => full_name
            .strip_suffix(owned)
            .is_some_and(|rest| rest.is_empty() || rest.ends_with('/')),
        _ => true,
    }
}

/// The resource of type `t` whose field is `value`, trying `fields` in order
/// (a read by GitHub's issue number, or a repository's name) and, with
/// `owned`, only one whose `full_name` fits: the one [`PathAliases`] names
/// when it still holds the value, else the first of one scan.
async fn by_path_fields(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    fields: &[&str],
    value: &str,
    owned: Option<&str>,
) -> Option<StoredResource> {
    let holds = |r: &StoredResource, field: &str| {
        r.data.get(field).and_then(id_string).as_deref() == Some(value) && full_name_fits(r, owned)
    };
    for field in fields {
        if let Some(id) = state.rt.path_aliases().id_of(session, t, field, value)
            && let Some(hit) = state.get(session, t, &id).await
            && holds(&hit, field)
        {
            return Some(hit);
        }
    }
    let stored = state.list(session, t, MAX_SCAN).await;
    let (field, found) = fields.iter().find_map(|field| {
        let found = stored.iter().find(|r| holds(r, field))?;
        Some((*field, found))
    })?;
    let values = vec![(field, value.to_string())];
    state
        .rt
        .path_aliases()
        .remember(session, t, &found.id, values);
    Some(found.clone())
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

    /// Whether the store took the write.
    async fn upsert(&mut self, s: &SessionKey, r: StoredResource) -> bool {
        if !self.open() {
            return false;
        }
        let rt = self.rt;
        let values = path_values(rt.index().path_fields(&r.resource_type), &r.data);
        let named = (!values.is_empty()).then(|| (r.resource_type.clone(), r.id.clone()));
        let r = rt.state.upsert(s, r).await;
        let ok = self.settle(r).is_some();
        self.wrote |= ok;
        if ok && let Some((t, id)) = named {
            rt.path_aliases().remember(s, &t, &id, values);
        }
        ok
    }

    /// Whether the store took the delete.
    async fn delete(&mut self, s: &SessionKey, t: &str, id: &str) -> bool {
        if !self.open() {
            return false;
        }
        let r = self.rt.state.delete(s, t, id).await;
        let ok = self.settle(r).is_some();
        self.wrote |= ok;
        ok
    }

    /// Stores a seeded resource unless one is stored under its id; `true`
    /// when stored.
    pub(crate) async fn insert_seeded(&mut self, s: &SessionKey, r: StoredResource) -> bool {
        if !self.open() {
            return false;
        }
        let rt = self.rt;
        let values = path_values(rt.index().path_fields(&r.resource_type), &r.data);
        let named = (!values.is_empty()).then(|| (r.resource_type.clone(), r.id.clone()));
        let r = rt.state.insert_seeded(s, r).await;
        let stored = self.settle(r).unwrap_or(false);
        self.wrote |= stored;
        if stored && let Some((t, id)) = named {
            rt.path_aliases().remember(s, &t, &id, values);
        }
        stored
    }
}

/// Stores `resources` in the session: merged over what it holds
/// (`merge`), else only those it does not hold yet. Keeps the session
/// alive.
#[cfg(feature = "graphql")]
pub(crate) async fn store_resources(
    rt: &MockRuntime,
    session: &SessionKey,
    resources: Vec<StoredResource>,
    merge: bool,
) {
    let mut state = StateAccess::new(rt);
    async {
        for mut resource in resources {
            if !merge {
                state.insert_seeded(session, resource).await;
                continue;
            }
            if let Some(held) = state
                .get(session, &resource.resource_type, &resource.id)
                .await
                && let (Value::Object(held), Value::Object(new)) = (held.data, &resource.data)
            {
                let mut merged = held.clone();
                merged.extend(new.clone());
                // Already stored as it is (a create's own write): no
                // second change event.
                if merged == held {
                    continue;
                }
                resource.data = Value::Object(merged);
            }
            state.upsert(session, resource).await;
        }
        state.touch(session).await;
    }
    .instrument(state_span())
    .await;
}

/// Resources the response should stay consistent with, nearest first: what
/// the request names, recent items for a list query, then what the target
/// and the resources the request names refer to. With consistency off, only
/// the `<x>Id` members and global ids of the query and body are followed.
async fn related(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    target_id: Option<&str>,
    target: Option<&StoredResource>,
    call: &OperationCall,
) -> Vec<StoredResource> {
    let rt = state.rt;
    let index = rt.consistency.then(|| rt.index());
    let mut references = Vec::new();
    if index.is_some() {
        for (field, value) in &call.path_params {
            if let Some(id) = id_string(value) {
                references_in(field, &Value::String(id), 0, index, &mut references);
            }
        }
    }
    for (field, value) in &call.query {
        references_in(field, value, 0, index, &mut references);
    }
    if let Some(Value::Object(fields)) = &call.body {
        for (field, value) in fields {
            references_in(field, value, 0, index, &mut references);
        }
    }
    let mut out: Vec<StoredResource> = Vec::new();
    let mut seen = Vec::new();
    follow(state, session, references, &mut seen, target, &mut out).await;
    let named = out.len();
    if matches!(op.kind, OperationKind::Query)
        && target_id.is_none()
        && let Some(t) = op.resource_type.as_deref()
    {
        let mut listed = state.list(session, t, LIST_CONTEXT_LIMIT).await;
        // Lists the answer holds that other reads stored: Plaid's
        // `/auth/get` answers the `accounts` that `/accounts/get` seeded.
        let members = rt
            .index()
            .operation(&op.id)
            .map(|i| rt.index().list_members(i))
            .unwrap_or_default();
        let others = members
            .iter()
            .filter(|m| m.as_str() != t && !ENVELOPE_KEYS.contains(&m.as_str()));
        for member in others.take(MAX_LIST_MEMBERS) {
            if listed.len() >= LIST_CONTEXT_LIMIT {
                break;
            }
            let room = LIST_CONTEXT_LIMIT - listed.len();
            listed.extend(state.list(session, member, room).await);
        }
        for item in listed {
            if !holds(&out, &item) {
                out.push(item);
            }
        }
    }
    let hops = if index.is_some() { MAX_RELATED_HOPS } else { 0 };
    let (mut own, mut start, mut end) = (target, 0, named);
    for _ in 0..hops {
        let mut references = Vec::new();
        for resource in own.take().into_iter().chain(&out[start..end]) {
            references_in("", &resource.data, 0, index, &mut references);
        }
        start = out.len();
        follow(state, session, references, &mut seen, target, &mut out).await;
        end = out.len();
    }
    out
}

/// A reference: the types to look in, and the id.
type Reference = (Vec<String>, String);

/// Adds to `out` the resources `references` name, other than `target`.
/// `seen` holds the references looked up so far in this call: at most
/// [`MAX_REFERENCES`], so that what is followed first is what is nearest.
async fn follow(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    references: Vec<Reference>,
    seen: &mut Vec<Reference>,
    target: Option<&StoredResource>,
    out: &mut Vec<StoredResource>,
) {
    for reference in references {
        if seen.len() >= MAX_REFERENCES {
            return;
        }
        if seen.contains(&reference) {
            continue;
        }
        seen.push(reference.clone());
        let (types, id) = reference;
        for t in types {
            if target.is_some_and(|r| r.resource_type == t && r.id == id) {
                break;
            }
            if let Some(hit) = state.get(session, &t, &id).await {
                let is_target =
                    target.is_some_and(|r| r.resource_type == hit.resource_type && r.id == hit.id);
                if !is_target && !holds(out, &hit) {
                    out.push(hit);
                }
                break;
            }
        }
    }
}

/// Whether `resources` already holds `resource`.
fn holds(resources: &[StoredResource], resource: &StoredResource) -> bool {
    resources
        .iter()
        .any(|r| r.resource_type == resource.resource_type && r.id == resource.id)
}

/// The stored resources that refer to `id` of `op`'s resource type, for a
/// read of an id the session does not hold: at most [`MAX_SCAN`] resources
/// of the types the contract says can refer to it are read, newest first,
/// and at most what a prompt shows ([`mb_engine::prompt::MAX_RELATED`]) kept.
async fn referrers(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    id: &str,
) -> Vec<StoredResource> {
    let rt = state.rt;
    let Some(wanted) = op.resource_type.as_deref().filter(|_| rt.consistency) else {
        return Vec::new();
    };
    let index = rt.index();
    let types = index.referrers(wanted);
    let each = MAX_SCAN / types.len().clamp(1, MAX_REFERENCES);
    let mut out = Vec::new();
    for t in types.iter().take(MAX_REFERENCES) {
        for resource in state.list(session, t, each).await {
            let mut references = Vec::new();
            references_in("", &resource.data, 0, Some(index), &mut references);
            let refers = references
                .iter()
                .any(|(types, to)| to == id && types.iter().any(|t| t == wanted));
            if refers {
                out.push(resource);
                if out.len() >= mb_engine::prompt::MAX_RELATED {
                    return out;
                }
            }
        }
    }
    out
}

/// The stored resources `args` refers to by `<x>Id` members and global
/// ids (see [`references_in`]), for a GraphQL answer to agree with them.
#[cfg(feature = "graphql")]
pub(crate) async fn referenced(
    rt: &MockRuntime,
    session: &SessionKey,
    args: &Map<String, Value>,
) -> Vec<StoredResource> {
    let mut references = Vec::new();
    for (field, value) in args {
        references_in(field, value, 0, None, &mut references);
    }
    let mut state = StateAccess::new(rt);
    let mut out: Vec<StoredResource> = Vec::new();
    async {
        for (types, id) in references.into_iter().take(MAX_REFERENCES) {
            for t in types {
                if let Some(hit) = state.get(session, &t, &id).await {
                    if !out.iter().any(|r| r.id == hit.id) {
                        out.push(hit);
                    }
                    break;
                }
            }
        }
        state.touch(session).await;
    }
    .instrument(state_span())
    .await;
    out
}

/// Reference lookups per call, at most.
const MAX_REFERENCES: usize = 16;

/// How far references are followed out of what a request names and of its
/// target: to what those refer to, and no further.
const MAX_RELATED_HOPS: usize = 1;

/// The resources `value` (under member `field`) refers to, as the types to
/// look in and the id, here or in nested objects and lists (GraphQL input
/// objects, `lines: [{merchandiseId}]`): a `<x>Id`/`<x>_id` member (types
/// `<x>s`, `<x>`), or a global id `gid://<app>/<Type>/…` (type `<type>`).
/// With `index`, which is `None` when consistency is off, also a `<x>Id`
/// holding a number, and a string member named after a resource type
/// (`customer`, `payment_intent`; see [`entity_type`]).
fn references_in(
    field: &str,
    value: &Value,
    depth: usize,
    index: Option<&BundleIndex>,
    out: &mut Vec<Reference>,
) {
    if depth > 4 || out.len() >= MAX_REFERENCES {
        return;
    }
    match value {
        Value::String(id) => {
            if let Some(t) = global_id_type(id) {
                out.push((vec![t], id.clone()));
            } else if let Some(prefix) = reference_prefix(field) {
                out.push((vec![format!("{prefix}s"), prefix.to_string()], id.clone()));
            } else if let Some(t) = index.and_then(|i| entity_type(i, None, field)) {
                out.push((vec![t], id.clone()));
            }
        }
        Value::Number(id) if index.is_some() => {
            if let Some(prefix) = reference_prefix(field) {
                out.push((
                    vec![format!("{prefix}s"), prefix.to_string()],
                    id.to_string(),
                ));
            }
        }
        Value::Array(items) => {
            for item in items {
                references_in(field, item, depth + 1, index, out);
            }
        }
        Value::Object(obj) => {
            for (k, v) in obj {
                references_in(k, v, depth + 1, index, out);
            }
        }
        _ => {}
    }
}

/// The type of a global id (`gid://shopify/ProductVariant/42` →
/// `productvariant`), as GraphQL answers are stored.
fn global_id_type(id: &str) -> Option<String> {
    let rest = id.strip_prefix("gid://")?;
    let mut parts = rest.split('/');
    let _app = parts.next()?;
    let t = parts.next().filter(|t| !t.is_empty())?;
    parts.next()?;
    Some(t.to_lowercase())
}

/// Who made a state write: the client's call, or list seeding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteOrigin {
    Call,
    Seeded,
}

/// A 2xx answer to write to state, and where it came from.
struct WriteRequest<'a> {
    session: &'a SessionKey,
    op: &'a Operation,
    resource_type: &'a str,
    target_id: Option<String>,
    generated: &'a GeneratedResponse,
    request_body: Option<&'a Value>,
    origin: WriteOrigin,
}

/// Writes a 2xx answer to state: a call's create, update or delete, or
/// (`Seeded`) every identified item of a list answer that the session does
/// not hold yet.
async fn write_state(state: &mut StateAccess<'_>, w: WriteRequest<'_>) {
    let WriteRequest {
        session,
        op,
        resource_type,
        target_id,
        generated,
        request_body,
        origin,
    } = w;
    if origin == WriteOrigin::Seeded {
        let Some(items) = generated.body.as_ref().and_then(list_items) else {
            return;
        };
        let field = item_identity(
            state.rt.bundle(),
            op,
            generated.body.as_ref().unwrap_or(&Value::Null),
        );
        for item in items {
            let Some(id) = item.get(&field).and_then(id_string) else {
                continue;
            };
            let resource = StoredResource {
                resource_type: resource_type.into(),
                id: id.clone(),
                data: item.clone(),
            };
            state.insert_seeded(session, resource).await;
        }
        return;
    }
    let data = generated.body.clone().or_else(|| request_body.cloned());
    let (id, data) = match op.kind {
        OperationKind::Command(Effect::Create) => {
            let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
            let Some(data) = data else { return };
            let Some(id) = data.get(field).and_then(id_string) else {
                return;
            };
            (id, Some(data))
        }
        OperationKind::Command(Effect::Update) => {
            let (Some(id), Some(data)) = (target_id, data) else {
                return;
            };
            (id, Some(data))
        }
        OperationKind::Command(Effect::Delete) => {
            let Some(id) = target_id else { return };
            (id, None)
        }
        _ => return,
    };
    match data {
        Some(data) => {
            let resource = StoredResource {
                resource_type: resource_type.into(),
                id,
                data,
            };
            state.upsert(session, resource).await;
        }
        None => {
            state.delete(session, resource_type, &id).await;
        }
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

    use super::*;
    use futures::StreamExt;
    use mb_engine::StreamItem;
    use mb_engine::{Engine, EngineConfig, ScenarioSpec};
    use mb_import::compile;
    use mb_mcm::{Bundle, Identity, Protocol};
    use mb_state::{MemoryStore, StateStore};
    use serde_json::json;
    use std::sync::Arc;

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
    fn cache_key_ignores_query_order_and_tracks_the_selection() {
        let rt = stream_runtime();
        let op = &rt.bundle.operations[0];
        let call = |q: Value| OperationCall {
            query: q.as_object().unwrap().clone(),
            ..OperationCall::default()
        };
        let a = call(json!({"a": 1, "b": 2}));
        let b = call(json!({"b": 2, "a": 1}));
        assert_eq!(cache_key(op, &a, None), cache_key(op, &b, None));
        assert_ne!(
            cache_key(op, &a, None),
            cache_key(op, &call(json!({"a": 2, "b": 2})), None)
        );
        let narrow = json!({"type": "object", "properties": {"id": {"type": "string"}}});
        assert_ne!(cache_key(op, &a, None), cache_key(op, &a, Some(&narrow)));
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
            path_field: None,
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
        let mut feed = change_feed(&rt, session("a"), &op, None);
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
        let mut feed = change_feed(&rt, session("a"), &op, None);
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

    /// An example contract, compiled.
    fn example(path: &str) -> Bundle {
        let path = format!("{}/../../examples/apis/{path}", env!("CARGO_MANIFEST_DIR"));
        compile(&std::fs::read(path).unwrap()).unwrap()
    }

    fn runtime_of(bundle: Bundle) -> MockRuntime {
        let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
        MockRuntime::new(
            "m",
            Arc::new(bundle),
            Arc::new(Engine::new(None, EngineConfig::default())),
            state,
        )
    }

    /// What `next_numbers` used to compute per create.
    fn scanned_path_fields<'a>(bundle: &'a Bundle, resource_type: &str) -> Vec<&'a str> {
        let mut fields: Vec<&str> = bundle
            .operations
            .iter()
            .filter(|o| o.resource_type.as_deref() == Some(resource_type))
            .filter_map(|o| o.identity.as_ref()?.path_field.as_deref())
            .collect();
        fields.sort_unstable();
        fields.dedup();
        fields
    }

    /// What `store_wrapped` used to compute per create.
    fn scanned_listed(bundle: &Bundle) -> Vec<String> {
        bundle
            .operations
            .iter()
            .filter(|o| matches!(o.kind, OperationKind::Query))
            .filter_map(|o| o.success_output()?.schema.as_ref())
            .flat_map(|schema| list_members(bundle, schema))
            .collect()
    }

    /// GitHub's `repository` is the detail route's type, not a list's.
    #[test]
    fn entity_types_of_the_github_contract() {
        let rt = runtime_of(example("github-rest/github-rest.openapi.yaml"));
        let index = rt.index();
        assert_eq!(index.types_by_def["repository"], "repos");
        assert_eq!(
            entity_type(index, Some("repository"), "ignored").as_deref(),
            Some("repos")
        );
        assert_eq!(entity_type(index, None, "repo").as_deref(), Some("repos"));
        assert_eq!(entity_type(index, None, "items"), None);
        assert_eq!(entity_type(index, None, "nothing"), None);
        assert_eq!(entity_type(index, None, ""), None);
    }

    /// The runtime's index answers what scanning the bundle answers.
    #[test]
    fn bundle_index_matches_a_scan() {
        let mut bundles = vec![
            example("github-rest/github-rest.openapi.yaml"),
            example("slack/slack.openapi.yaml"),
        ];
        // A repeated id resolves to its first operation, as a scan does.
        let mut repeated = bundles[0].clone();
        let mut twin = repeated.operations[1].clone();
        twin.id = repeated.operations[0].id.clone();
        repeated.operations.push(twin);
        bundles.push(repeated);
        for bundle in bundles {
            let rt = runtime_of(bundle);
            let bundle = rt.bundle();
            let mut types = HashSet::new();
            for op in &bundle.operations {
                assert!(std::ptr::eq(
                    rt.operation(&op.id).unwrap(),
                    bundle.operation(&op.id).unwrap()
                ));
                let members = op
                    .success_output()
                    .and_then(|o| o.schema.as_ref())
                    .map(|schema| list_members(bundle, schema))
                    .unwrap_or_default();
                let i = rt.index().operation(&op.id).unwrap();
                if std::ptr::eq(&bundle.operations[i], op) {
                    assert_eq!(rt.index().list_members(i), members.as_slice());
                }
                types.extend(op.resource_type.clone());
            }
            assert!(rt.operation("noSuchOperation").is_none());
            for t in types.iter().map(String::as_str).chain(["noSuchType"]) {
                let indexed: Vec<&str> = rt
                    .index()
                    .path_fields(t)
                    .iter()
                    .map(String::as_str)
                    .collect();
                assert_eq!(indexed, scanned_path_fields(bundle, t), "{t}");
            }
            let scanned: HashSet<String> = scanned_listed(bundle).into_iter().collect();
            assert_eq!(rt.index().listed, scanned);
        }
    }

    /// The runtime's channel index finds what scanning the channels finds,
    /// the first of a repeated id included.
    #[test]
    fn channel_index_matches_a_scan() {
        let mut bundle = example("kafka-events/commerce-events.asyncapi.yaml");
        assert!(bundle.channels.len() > 1);
        let mut twin = bundle.channels[1].clone();
        twin.id = bundle.channels[0].id.clone();
        bundle.channels.push(twin);
        let rt = runtime_of(bundle);
        let bundle = rt.bundle();
        for channel in &bundle.channels {
            assert!(std::ptr::eq(
                rt.channel(&channel.id).unwrap(),
                bundle.channel(&channel.id).unwrap()
            ));
        }
        assert!(rt.channel("noSuchChannel").is_none());
    }

    /// Timing harness: `cargo test -p mockingbird-protocols --lib --
    /// --ignored --nocapture channel_index_timing`.
    #[test]
    #[ignore = "timing harness"]
    fn channel_index_timing() {
        let events = example("kafka-events/commerce-events.asyncapi.yaml");
        for copies in [10usize, 100] {
            let mut bundle = events.clone();
            bundle.channels = (0..copies)
                .flat_map(|c| {
                    events.channels.iter().map(move |ch| {
                        let mut ch = ch.clone();
                        ch.id = format!("{}{c}", ch.id);
                        ch
                    })
                })
                .collect();
            let channels = bundle.channels.len();
            let last = bundle.channels[channels - 1].id.clone();
            let rt = runtime_of(bundle);
            let bundle = rt.bundle();
            let n = 20_000u32;
            let started = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(bundle.channel(std::hint::black_box(&last)));
            }
            let scan = started.elapsed() / n;
            let started = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(rt.channel(std::hint::black_box(&last)));
            }
            let indexed = started.elapsed() / n;
            println!("{channels} channels: lookup scan {scan:?}, index {indexed:?}");
        }
    }

    /// Timing harness: `cargo test -p mockingbird-protocols --lib --
    /// --ignored --nocapture index_timing`.
    #[test]
    #[ignore = "timing harness"]
    fn index_timing() {
        let github = example("github-rest/github-rest.openapi.yaml");
        for copies in [10usize, 100] {
            let mut bundle = github.clone();
            bundle.operations = (0..copies)
                .flat_map(|c| {
                    github.operations.iter().map(move |op| {
                        let mut op = op.clone();
                        op.id = format!("{}{c}", op.id);
                        op
                    })
                })
                .collect();
            let ops = bundle.operations.len();
            let last = bundle.operations[ops - 1].id.clone();
            let issue_type = bundle
                .operations
                .iter()
                .find_map(|o| {
                    o.identity
                        .as_ref()?
                        .path_field
                        .as_ref()
                        .and(o.resource_type.clone())
                })
                .unwrap_or_default();
            let rt = runtime_of(bundle);
            let bundle = rt.bundle();
            let n = 2000u32;
            let time = |f: &dyn Fn()| {
                let started = std::time::Instant::now();
                for _ in 0..n {
                    f();
                }
                started.elapsed() / n
            };
            let scan = time(&|| {
                std::hint::black_box(bundle.operation(std::hint::black_box(&last)));
            });
            let indexed = time(&|| {
                std::hint::black_box(rt.operation(std::hint::black_box(&last)));
            });
            println!("{ops} operations: lookup scan {scan:?}, index {indexed:?}");
            let scan = time(&|| {
                std::hint::black_box(scanned_path_fields(bundle, &issue_type));
                std::hint::black_box(scanned_listed(bundle));
            });
            let indexed = time(&|| {
                std::hint::black_box(rt.index().path_fields(&issue_type));
                std::hint::black_box(&rt.index().listed);
            });
            println!("{ops} operations: per-create walk {scan:?}, index {indexed:?}");
        }
    }

    /// Timing harness for what a gRPC call's plan used to copy (its
    /// operation and scenario) against finding them again:
    /// `cargo test -p mockingbird-protocols --lib -- --ignored --nocapture
    /// owned_plan_timing`.
    #[test]
    #[ignore = "timing harness"]
    fn owned_plan_timing() {
        let mut rt = runtime_of(example("github-rest/github-rest.openapi.yaml"));
        let ids: Vec<String> = rt
            .bundle()
            .operations
            .iter()
            .map(|o| o.id.clone())
            .collect();
        for variants in [2usize, 20] {
            let instructions = ids
                .iter()
                .take(variants)
                .map(|id| (id.clone(), "be terse".to_string()))
                .collect();
            let scenario = ScenarioRuntime {
                spec: ScenarioSpec {
                    name: "s".into(),
                    instructions: Some("x".repeat(500)),
                    ..Default::default()
                },
                ..Default::default()
            }
            .with_operation_instructions(instructions);
            rt.scenarios.insert("s".into(), scenario);
            let op_id = &ids[0];
            let n = 2000u32;
            let started = std::time::Instant::now();
            for _ in 0..n {
                let op = rt.operation(op_id).unwrap().clone();
                let s = rt.scenarios["s"].for_operation(op_id).clone();
                std::hint::black_box((op, s));
            }
            let copied = started.elapsed() / n;
            let started = std::time::Instant::now();
            for _ in 0..n {
                let op = rt.operation(op_id).unwrap();
                let s = rt.scenarios.get("s").unwrap().for_operation(op_id);
                std::hint::black_box((op, s));
            }
            let found = started.elapsed() / n;
            println!("{variants} scenario variants: copy {copied:?}, find again {found:?}");
        }
    }

    /// Timing harness for the copies an offline bidi message (20 earlier
    /// bodies and its own) and a consumed event without a recorder (its
    /// payload and headers) no longer make: `cargo test -p
    /// mockingbird-protocols --lib -- --ignored --nocapture
    /// skipped_copies_timing`.
    #[test]
    #[ignore = "timing harness"]
    fn skipped_copies_timing() {
        let body = |fields: usize| -> Value {
            (0..fields)
                .map(|i| (format!("field{i}"), json!({"text": "some words", "n": i})))
                .collect::<Map<String, Value>>()
                .into()
        };
        let n = 1000u32;
        for fields in [20usize, 200] {
            let earlier: Vec<Value> = (0..20).map(|_| body(fields)).collect();
            let current = body(fields);
            let started = std::time::Instant::now();
            for _ in 0..n {
                let copies: Vec<Value> = earlier.to_vec();
                std::hint::black_box((copies, current.clone()));
            }
            let size = current.to_string().len();
            println!(
                "bidi message of {size} bytes: {:?} of copies",
                started.elapsed() / n
            );
        }
        let headers: Vec<(String, String)> = (0..10)
            .map(|i| (format!("x-header-{i}"), "value".repeat(4)))
            .collect();
        for fields in [200usize, 2000] {
            let payload = body(fields);
            let started = std::time::Instant::now();
            for _ in 0..n {
                let map: Map<String, Value> = headers
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect();
                std::hint::black_box((map, payload.clone()));
            }
            let size = payload.to_string().len();
            println!(
                "consumed message of {size} bytes: {:?} of copies",
                started.elapsed() / n
            );
        }
    }

    #[test]
    fn canonical_hash_ignores_member_order_and_tells_values_apart() {
        use std::hash::{DefaultHasher, Hasher};
        let hash = |v: &Value| {
            let mut hasher = DefaultHasher::new();
            hash_canonical(v, &mut hasher);
            hasher.finish()
        };
        let mut forward = Map::new();
        forward.insert("a".into(), json!(1));
        forward.insert("b".into(), json!({"y": [1, 2], "x": null}));
        let mut backward = Map::new();
        backward.insert("b".into(), json!({"x": null, "y": [1, 2]}));
        backward.insert("a".into(), json!(1));
        assert_eq!(
            hash(&Value::Object(forward)),
            hash(&Value::Object(backward))
        );
        let distinct = [
            json!({"a": [1, 2]}),
            json!({"a": [2, 1]}),
            json!({"a": "1"}),
            json!({"a": 1}),
            json!({"a": null}),
            json!({}),
            json!([["a", 1]]),
            json!({"a": {"b": 1}}),
            json!({"a": [{"b": 1}]}),
        ];
        let hashes: HashSet<u64> = distinct.iter().map(hash).collect();
        assert_eq!(hashes.len(), distinct.len());
    }

    /// Counts `list` calls on a memory store.
    struct ListSpy {
        inner: MemoryStore,
        lists: std::sync::atomic::AtomicUsize,
    }

    impl ListSpy {
        fn lists(&self) -> usize {
            self.lists.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl StateStore for ListSpy {
        async fn get(
            &self,
            s: &SessionKey,
            t: &str,
            id: &str,
        ) -> Result<Option<StoredResource>, StateError> {
            self.inner.get(s, t, id).await
        }
        async fn list(
            &self,
            s: &SessionKey,
            t: &str,
            limit: usize,
        ) -> Result<Vec<StoredResource>, StateError> {
            self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.list(s, t, limit).await
        }
        async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
            self.inner.upsert(s, r).await
        }
        async fn delete(&self, s: &SessionKey, t: &str, id: &str) -> Result<bool, StateError> {
            self.inner.delete(s, t, id).await
        }
        async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
            self.inner.reset(s).await
        }
        async fn reset_mock(&self, w: &str, m: &str) -> Result<(), StateError> {
            self.inner.reset_mock(w, m).await
        }
        async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
            self.inner.touch(s).await
        }
        async fn sessions(
            &self,
            w: &str,
            m: &str,
        ) -> Result<Vec<mb_state::SessionInfo>, StateError> {
            self.inner.sessions(w, m).await
        }
        async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
            self.inner.export(s).await
        }
        async fn seed(&self, s: &SessionKey, r: Vec<StoredResource>) -> Result<(), StateError> {
            self.inner.seed(s, r).await
        }
        async fn ping(&self) -> Result<(), StateError> {
            Ok(())
        }
        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<mb_state::StateChanged> {
            self.inner.subscribe()
        }
    }

    /// GitHub's runtime over a [`ListSpy`], with its read of an issue by
    /// number.
    fn issue_reads() -> (Arc<ListSpy>, MockRuntime, Operation) {
        let spy = Arc::new(ListSpy {
            inner: MemoryStore::new(Duration::from_secs(3600)),
            lists: Default::default(),
        });
        let rt = MockRuntime::new(
            "m",
            Arc::new(example("github-rest/github-rest.openapi.yaml")),
            Arc::new(Engine::new(None, EngineConfig::default())),
            spy.clone(),
        );
        let op = rt
            .bundle()
            .operations
            .iter()
            .find(|o| o.identity.as_ref().is_some_and(|i| i.path_field.is_some()))
            .cloned()
            .unwrap();
        (spy, rt, op)
    }

    fn issue(op: &Operation, id: &str, number: u64) -> StoredResource {
        StoredResource {
            resource_type: op.resource_type.clone().unwrap(),
            id: id.into(),
            data: json!({"id": id, "number": number, "title": format!("issue {number}")}),
        }
    }

    async fn read_by(rt: &MockRuntime, op: &Operation, number: &str) -> Option<String> {
        let mut state = StateAccess::new(rt);
        let (target, _) = read_context(
            &mut state,
            &session("s"),
            op,
            Some(number),
            &OperationCall::default(),
        )
        .await;
        target.map(|t| t.id)
    }

    #[tokio::test]
    async fn a_read_by_path_field_finds_this_runtimes_writes_without_a_scan() {
        let (spy, rt, op) = issue_reads();
        let field = op.identity.as_ref().unwrap().path_field.as_deref();
        assert_eq!(field, Some("number"));
        let t = op.resource_type.clone().unwrap();
        let s = session("s");
        let mut state = StateAccess::new(&rt);
        assert!(state.upsert(&s, issue(&op, "I_1", 7)).await);
        assert!(state.insert_seeded(&s, issue(&op, "I_2", 8)).await);
        assert_eq!(read_by(&rt, &op, "7").await.as_deref(), Some("I_1"));
        assert_eq!(read_by(&rt, &op, "8").await.as_deref(), Some("I_2"));
        assert_eq!(spy.lists(), 0);

        // Renumbered behind the runtime's back: the alias no longer holds,
        // so the read scans like before, and remembers what it found.
        rt.state.upsert(&s, issue(&op, "I_1", 9)).await.unwrap();
        assert_eq!(read_by(&rt, &op, "7").await, None);
        assert_eq!(spy.lists(), 1);
        assert_eq!(read_by(&rt, &op, "9").await.as_deref(), Some("I_1"));
        assert_eq!(spy.lists(), 2);
        assert_eq!(read_by(&rt, &op, "9").await.as_deref(), Some("I_1"));
        assert_eq!(spy.lists(), 2);

        // Deleted or reset: the alias names nothing.
        rt.state.delete(&s, &t, "I_1").await.unwrap();
        assert_eq!(read_by(&rt, &op, "9").await, None);
        rt.state.reset(&s).await.unwrap();
        assert_eq!(read_by(&rt, &op, "8").await, None);
        // Another session does not see this one's aliases.
        let mut state = StateAccess::new(&rt);
        assert!(state.upsert(&session("other"), issue(&op, "I_3", 3)).await);
        assert_eq!(read_by(&rt, &op, "3").await, None);
    }

    #[test]
    fn path_aliases_are_bounded() {
        let aliases = PathAliases::default();
        let s = session("s");
        for i in 0..(MAX_PATH_ALIASES + 10) {
            let values = vec![("number", i.to_string())];
            aliases.remember(&s, "issues", &format!("I_{i}"), values);
            assert!(aliases.len() <= MAX_PATH_ALIASES);
        }
        let last = MAX_PATH_ALIASES + 9;
        assert_eq!(
            aliases.id_of(&s, "issues", "number", &last.to_string()),
            Some(format!("I_{last}"))
        );
    }

    /// Timing harness for a read by path field, scanning (as before) and
    /// through [`PathAliases`]: `cargo test -p mockingbird-protocols --lib
    /// --release -- --ignored --nocapture path_read_timing`.
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "timing harness"]
    async fn path_read_timing() {
        let (_, rt, op) = issue_reads();
        let s = session("s");
        let t = op.resource_type.clone().unwrap();
        let mut stored = 0;
        for n in [50usize, 500] {
            let mut state = StateAccess::new(&rt);
            for i in stored..n {
                let mut r = issue(&op, &format!("I_{i}"), i as u64);
                r.data["body"] = json!("y".repeat(1500));
                r.data["user"] = json!({"login": "octocat", "id": 1});
                state.upsert(&s, r).await;
            }
            stored = n;
            let iters = 500u32;
            let started = std::time::Instant::now();
            for _ in 0..iters {
                let _ = rt.state.get(&s, &t, "3").await;
                let found = rt
                    .state
                    .list(&s, &t, MAX_SCAN)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|r| r.data.get("number").and_then(id_string).as_deref() == Some("3"));
                std::hint::black_box(found);
            }
            let scan = started.elapsed() / iters;
            let started = std::time::Instant::now();
            for _ in 0..iters {
                std::hint::black_box(read_by(&rt, &op, "3").await);
            }
            let aliased = started.elapsed() / iters;
            println!("{n} issues stored: read by number, scan {scan:?}, alias {aliased:?}");
        }
    }
}
