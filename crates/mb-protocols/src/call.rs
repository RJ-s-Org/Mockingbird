//! The per-call pipeline every adapter shares: admission, data-plane auth,
//! override headers and scenario lookup ([`plan`]), state reads and writes around generation
//! ([`generate_once`], [`open_stream`]), state change feeds
//! ([`change_feed`]) and usage/request-log reporting ([`report`]).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::http::HeaderMap;
use mb_engine::consistency::{
    ENVELOPE_KEYS, item_identity, item_schema, list_items, list_members, property_schema,
};
use mb_engine::{
    EffectOutcome, EngineError, FAKER_INVALID_REASON, GeneratedResponse, GenerationContext,
    OFFLINE_REASON, OperationCall, ResourceEffect, Source,
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

mod cursors;
mod deleted;
mod envelopes;
mod lists;
mod minted;
mod pages;
mod runs;
mod value_index;
mod world;

pub(crate) use value_index::{ValueFeed, ValueIndex};
mod write_memory;

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
    /// A hash of the token an `Authorization: Bearer` header sent, never the token (see
    /// [`minted::bearer_hash`]).
    pub bearer: Option<String>,
    /// The URL the call reached the mock at, with no query: the base URL a client
    /// reached it at ([`crate::base_url::of`]) and the request's path as sent. What a
    /// next page's URL is written with ([`pages`]); `None` off HTTP routes.
    pub page_url: Option<String>,
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
        bearer: minted::bearer_hash(headers),
        page_url: None,
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
/// takes into account (as [`generate_once`] reads them, but for the hops,
/// which its callers do not read); the session is marked used.
#[cfg(feature = "graphql")]
pub(crate) async fn read_state(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> (Option<StoredResource>, Vec<StoredResource>) {
    let mut state = StateAccess::new(rt);
    let target_id = target_id(plan.op, call);
    let (target, related, _) = read_context(
        &mut state,
        &plan.session,
        plan.op,
        target_id.as_deref(),
        call,
    )
    .instrument(state_span())
    .await;
    state.touch(&plan.session).instrument(state_span()).await;
    (target, related)
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
            generate_pending(rt, plan, call, *pending, response_schema).await
        }
    }
}

/// Generates the answer of a [`prepare`]d call, then [`complete`]s it.
pub(crate) async fn generate_pending(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    mut pending: Pending<'_>,
    response_schema: Option<&Value>,
) -> Result<GeneratedResponse, EngineError> {
    let guard = pending
        .target
        .as_ref()
        .filter(|_| response_schema.is_none())
        .map(|held| write_memory::SettleGuard {
            rt,
            plan,
            call,
            held,
        });
    let ctx = GenerationContext {
        related: &pending.related,
        target: pending.target.as_ref(),
        forced_status: plan.forced_status,
        scenario: plan.scenario.map(|s| &s.spec),
        response_schema,
        earlier_requests: &pending.earlier.told,
        earlier_own_body: pending.earlier.own.as_ref(),
        earlier_answers: &pending.earlier_answers,
        earlier_writes: &pending.earlier_writes,
        earlier_origins: &pending.earlier_origins,
        served_cursors: &pending.served_cursors,
        created_since: &pending.created_since,
        alongside: &pending.alongside,
        same_values: &pending.same_values,
        changes_by_effects: write_memory::by_effects(&pending.earlier, pending.target.as_ref()),
        update_guard: guard.as_ref().map(|g| g as &dyn mb_engine::UpdateGuard),
    };
    let generated = plan.engine.generate(&rt.bundle, plan.op, call, ctx).await?;
    if generated.source == Source::Fallback {
        write_memory::failed_attempt(&mut pending.state, plan, &mut pending.earlier).await;
    }
    // The LLM failed a read that only generated to catch up: what is stored
    // still answers, as the faker must not invent a change. Nothing was stored
    // or marked, so the next read tries again, up to the cap.
    if generated.source == Source::Fallback
        && let Some(stored) = pending.earlier.stored_answer.take()
    {
        tracing::warn!(
            mock = %rt.name,
            operation = %plan.op.id,
            reason = generated.fallback_reason.as_deref().unwrap_or_default(),
            "read could not catch up with earlier requests; answered from state"
        );
        pending
            .state
            .touch(&plan.session)
            .instrument(state_span())
            .await;
        let mut answer = GeneratedResponse {
            usage: generated.usage,
            ..*stored
        };
        write_memory::note_not_caught_up(
            &mut answer,
            plan,
            pending.target.as_ref(),
            &pending.earlier,
        );
        return Ok(answer);
    }
    Ok(complete(rt, plan, call, pending, generated).await)
}

/// What [`prepare`] found: an answer that needs no generation, or the
/// state a generation must take into account.
pub(crate) enum Prepared<'a> {
    Answered(GeneratedResponse),
    Pending(Box<Pending<'a>>),
}

/// A call to generate, then hand to [`complete`].
pub(crate) struct Pending<'a> {
    state: StateAccess<'a>,
    target_id: Option<String>,
    pub(crate) target: Option<StoredResource>,
    pub(crate) related: Vec<StoredResource>,
    /// What the writes the target does not show yet created (see
    /// [`write_memory::created_since`]).
    created_since: Vec<StoredResource>,
    /// The target's key holds another owner's resource, not the target.
    foreign_key: bool,
    /// The cache epoch before any state was read, and the answer's key,
    /// when the answer is cached.
    cache: Option<(CacheEpoch, String)>,
    /// The writes the target does not show yet.
    earlier: write_memory::Earlier,
    /// What routes with no parameters answered earlier in the session.
    earlier_answers: Vec<Value>,
    /// Whether only a read has written to the session (see [`world::list_session`]).
    unlisted: bool,
    /// The answers that minted values this call carries, with their requests.
    earlier_origins: Vec<Value>,
    /// The hidden cursor parameters this call carries with a value the session served
    /// (see [`cursors`]).
    served_cursors: Vec<String>,
    /// What was written to this list read's own path earlier in the session.
    earlier_writes: Vec<Value>,
    /// Those of `related` a create's own collection holds (see [`created_alongside`]).
    alongside: Vec<(String, String)>,
    /// Those of `related` of a create's type that hold a value of its request, with the
    /// member (see [`shown_same_values`]).
    same_values: Vec<(String, String, String)>,
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
    state.tracks_deleted = deleted::applies(rt, plan);
    if state.tracks_deleted {
        state.call_params = Some(Box::new(OperationCall {
            path_params: call.path_params.clone(),
            ..OperationCall::default()
        }));
    }
    let resource_type = op.resource_type.as_deref();
    let mut target_id = target_id(op, call);
    // Before any state is read: an answer generated from older state must
    // not be cached after a write that lands meanwhile.
    let since = rt.state.response_cache().map(|c| c.epoch(session));
    let (target, mut related, mut followed) =
        read_context(&mut state, session, op, target_id.as_deref(), call)
            .instrument(state_span())
            .await;
    let target = match target {
        Some(hit) => Some(hit),
        None => {
            write_memory::silent_write_target(&mut state, plan, call, target_id.as_deref())
                .instrument(state_span())
                .await
        }
    };
    // What the path named, before a record found by another member takes its place.
    let named_id = target_id.clone();
    // Found by another field (an issue's `number`): writes go to its id.
    if let Some(found) = &target {
        target_id = Some(found.id.clone());
    }
    // What the target's key holds is another owner's: a write must leave it.
    let foreign_key = !matches!(op.kind, OperationKind::Query)
        && target.is_none()
        && guarded(rt, op, call).is_some()
        && match (resource_type, target_id.as_deref()) {
            (Some(t), Some(id)) => state.get(session, t, id).await.is_some(),
            _ => false,
        };

    if rt.unknown_ids == UnknownIds::NotFound
        && matches!(op.kind, OperationKind::Query)
        && plan.forced_status.is_none()
        && op.identity.as_ref().is_some_and(path_names_identity)
        && target.is_none()
        && state.available()
        && let (Some(t), Some(id)) = (resource_type, target_id.as_deref())
        && !(guarded(rt, op, call).is_some()
            && any_owner_holds(
                &mut state,
                session,
                t,
                id,
                target_param(op).as_deref().unwrap_or_default(),
            )
            .await)
    {
        let detail = format!("{t} {id} does not exist in session {}", session.session);
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(nothing_held(rt, op, call, response_schema, &detail));
    }
    // Whatever `unknown_ids` says: this one is known to be gone. A record found by
    // another member than its key (`/issues/{issue_iid}`) is not what the path named
    // when that was deleted: another issue can share the `iid`.
    if matches!(op.kind, OperationKind::Query)
        && plan.forced_status.is_none()
        && target
            .as_ref()
            .is_none_or(|found| Some(found.id.as_str()) != named_id.as_deref())
        && state.tracks_deleted
        && let (Some(t), Some(id)) = (resource_type, named_id.as_deref())
        && deleted::is_deleted(
            &mut state,
            session,
            t,
            deleted::scope(op, call).as_deref(),
            id,
        )
        .await
    {
        let detail = format!("{t} {id} was deleted in session {}", session.session);
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(nothing_held(rt, op, call, response_schema, &detail));
    }

    let target_missing = target.is_none();
    // A read naming a seed asks for that seed's answer, not for what a plain
    // read answered; a resource stored by a write or a fixture still stands.
    let target = target.filter(|stored| {
        !(matches!(op.kind, OperationKind::Query)
            && mb_engine::seed::request_seed(call).is_some()
            && rt.own_reads().holds(session, stored))
    });

    // Whether a stored answer stands depends on what is remembered; otherwise
    // what is remembered waits until the cache has had its say.
    let mut recalled = None;
    if matches!(op.kind, OperationKind::Query)
        && plain(plan)
        && let Some(stored) = &target
        // What the schema alone tells of the envelope costs no read; what the session's
        // answers said of it ([`envelopes`]) is read only for an answer that stands.
        && let (bare, enveloped) = (
            serve_held(rt, op, stored, None, response_schema),
            envelopes::applies(rt, op),
        )
        && (bare.is_some() || enveloped)
    {
        let mut earlier = write_memory::recall(
            &mut state,
            plan,
            call,
            target_id.as_deref().filter(|_| !foreign_key),
            true,
        )
        .instrument(state_span())
        .await;
        if earlier.forces(rt, plan, call) {
            earlier.stored_answer = bare.map(Box::new);
        } else {
            let kept = match enveloped {
                true => {
                    envelopes::recall(&mut state, plan)
                        .instrument(state_span())
                        .await
                }
                false => None,
            };
            let served = kept
                .and_then(|kept| serve_held(rt, op, stored, Some(&kept), response_schema))
                .or(bare);
            if let Some(mut served) = served {
                write_memory::note_not_caught_up(&mut served, plan, Some(stored), &earlier);
                state.touch(session).instrument(state_span()).await;
                return Prepared::Answered(served);
            }
        }
        recalled = Some(earlier);
    }
    if target.is_none()
        && let Some(served) = member_read(&mut state, plan, call, response_schema)
            .instrument(state_span())
            .await
    {
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(served);
    }

    // The next page of a list this session cut, by the token it minted.
    if target.is_none()
        && let Some(served) = pages::serve(&mut state, plan, call)
            .instrument(state_span())
            .await
    {
        cursors::after_answer(&mut state, plan, &served)
            .instrument(state_span())
            .await;
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
            cache_key(&rt.bundle, op, call, response_schema)
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
            effects: Vec::new(),
            effect_outcomes: Vec::new(),
            dropped_effects: 0,
            new_record: false,
            patched: None,
        };
        // The answer's items were seeded when it was generated; writes and
        // resets that could remove them also clear the cache.
        state.touch(session).instrument(state_span()).await;
        return Prepared::Answered(served);
    }

    let earlier = match recalled {
        Some(earlier) => earlier,
        None => {
            write_memory::recall(
                &mut state,
                plan,
                call,
                target_id.as_deref().filter(|_| !foreign_key),
                target.is_some(),
            )
            .instrument(state_span())
            .await
        }
    };

    // Last, so that a 404, a stored or a cached answer costs none of this.
    let ((earlier_answers, unlisted), earlier_writes, (earlier_origins, served_cursors), alongside) =
        async {
            parents_by_name(
                &mut state,
                session,
                op,
                call,
                target.as_ref(),
                &mut followed,
                &mut related,
            )
            .await;
            world::named_by_value(
                &mut state,
                plan,
                call,
                target.as_ref(),
                &followed,
                &mut related,
            )
            .await;
            follow_hops(&mut state, session, target.as_ref(), followed, &mut related).await;
            if is_query
                && target_missing
                && let Some(id) = target_id.as_deref()
            {
                for resource in referrers(&mut state, session, op, id).await {
                    if !holds(&related, &resource) {
                        related.push(resource);
                    }
                }
            }
            let alongside =
                created_alongside(&mut state, plan, call, target.as_ref(), &mut related).await;
            let same_values =
                shown_same_values(&mut state, plan, call, target.as_ref(), &mut related).await;
            (
                world::recall_answers(&mut state, plan).await,
                write_memory::recall_route(&mut state, plan, call).await,
                (
                    minted::recall(&mut state, plan, call).await,
                    cursors::recall(&mut state, plan, call).await,
                ),
                (alongside, same_values),
            )
        }
        .instrument(state_span())
        .await;
    let (alongside, same_values) = alongside;
    let created_since =
        write_memory::created_since(&mut state, plan, &earlier, target.as_ref(), &mut related)
            .instrument(state_span())
            .await;

    Prepared::Pending(Box::new(Pending {
        state,
        target_id,
        target,
        related,
        created_since,
        foreign_key,
        cache: since.zip(key),
        earlier,
        earlier_answers,
        unlisted,
        earlier_writes,
        earlier_origins,
        served_cursors,
        alongside,
        same_values,
    }))
}

/// Stored `stored` served as `op`'s answer, in the envelope its read holds it in (see
/// [`mb_engine::Engine::held_answer`]), with what `kept` says the session's answers of
/// that envelope said besides the record ([`envelopes`]). `None` when the envelope
/// cannot be told, or the answer does not suit the schema.
fn serve_held(
    rt: &MockRuntime,
    op: &Operation,
    stored: &StoredResource,
    kept: Option<&Map<String, Value>>,
    response_schema: Option<&Value>,
) -> Option<GeneratedResponse> {
    let answer = rt
        .engine
        .held_answer(&rt.bundle, op, &stored.data, kept, response_schema)?;
    rt.engine
        .serve_stored(&rt.bundle, op, &answer, response_schema)
}

/// The answer to a read of a record the session does not hold, or deleted: a read
/// answering the records an id has as an array has none (`[]`), when its schema allows
/// that; any other answers its `404`.
fn nothing_held(
    rt: &MockRuntime,
    op: &Operation,
    call: &OperationCall,
    response_schema: Option<&Value>,
    detail: &str,
) -> GeneratedResponse {
    let lists = op
        .identity
        .as_ref()
        .is_some_and(|i| i.record == Some(mb_mcm::RecordAt::Item));
    lists
        .then(|| {
            rt.engine
                .serve_stored(&rt.bundle, op, &Value::Array(Vec::new()), response_schema)
        })
        .flatten()
        .unwrap_or_else(|| rt.engine.not_found(&rt.bundle, op, call, detail))
}

/// A read of a member of its parent (see [`BundleIndex::member_reads`]), answered by
/// that member of the stored parent: Spotify's `/playlists/{id}/items` by the stored
/// playlist's `tracks`. Not with a query, and not while a write the parent's record
/// does not show yet is remembered for the parent or for this route, as the member is
/// then behind it.
async fn member_read(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    response_schema: Option<&Value>,
) -> Option<GeneratedResponse> {
    let rt = state.rt;
    let op = plan.op;
    // A query asks for another view of the member (a page, a filter): it generates.
    if !(rt.consistency
        && matches!(op.kind, OperationKind::Query)
        && plain(plan)
        && call.query.is_empty()
        && mb_engine::seed::request_seed(call).is_none())
    {
        return None;
    }
    let index = rt.index();
    let read = index.member_reads.get(&index.operation(&op.id)?)?;
    let parent_type = rt
        .bundle()
        .operations
        .get(read.parent)?
        .resource_type
        .as_deref()?;
    let id = call.path_params.get(&read.param).and_then(id_string)?;
    let parent = state.get(&plan.session, parent_type, &id).await?;
    let member = parent.data.get(&read.member)?;
    if write_memory::is_behind(state, plan, call, &parent).await {
        return None;
    }
    rt.engine
        .serve_stored(&rt.bundle, op, member, response_schema)
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
        mut target_id,
        target,
        related,
        created_since,
        foreign_key,
        cache,
        mut earlier,
        unlisted,
        same_values,
        ..
    } = pending;
    let acted_on = target_id.clone();
    let op = plan.op;
    let session = &plan.session;
    let resource_type = op.resource_type.as_deref();
    let is_query = matches!(op.kind, OperationKind::Query);
    let collection = collection_of(op, call).filter(|_| rt.consistency);
    // A new record whose id no draw could free: the answer is served, and
    // nothing else comes of it.
    let mut unstored = false;
    // What state keeps of an answer holding its record in an envelope or a one-item
    // array is the record, and of a request the record it holds the same way.
    let held_in = op.identity.as_ref().filter(|i| i.record.is_some());
    let mut answered = held_in
        .filter(|_| (200..300).contains(&generated.status))
        .and_then(|identity| {
            let answer = generated.body.take()?;
            generated.body = identity.record_in(&answer).cloned();
            Some(answer)
        });
    let record_call = match (held_in, call.body.as_ref()) {
        (Some(identity), Some(request)) if answered.is_some() => Some(OperationCall {
            body: identity
                .record_in(request)
                .cloned()
                .or(Some(request.clone())),
            ..call.clone()
        }),
        _ => None,
    };
    let state_call = record_call.as_ref().unwrap_or(call);
    async {
        // What the list's query asks for, before its items are seeded or cached. An
        // answer holding one record (`/users/me` answering `{user}`) is no list.
        let fallback = transient_fallback(&generated);
        if is_query
            && target_id.is_none()
            && answered.is_none()
            && (200..300).contains(&generated.status)
            && deleted::applies(rt, plan)
            && let Some(body) = generated.body.as_mut()
        {
            // The store first: a filter sees what the session wrote since.
            lists::overlay_stored(&mut state, (plan, call), resource_type, body).await;
            let cut = lists::narrow(rt, plan, call, body);
            // A full page whose rest the model left out, but the store holds.
            let cut = match fallback {
                false => {
                    lists::stored_rest(&mut state, (plan, call), resource_type, body, cut).await
                }
                true => None,
            };
            let kept = match cut {
                Some(cut) => {
                    pages::mint(&mut state, (plan, call), resource_type, cut, &mut generated).await
                }
                None => false,
            };
            // A first page kept nothing: no run of an earlier one serves its next pages.
            if !kept {
                runs::forget(&mut state, (plan, call)).await;
            }
        }
        if (200..300).contains(&generated.status)
            && let Some(t) = resource_type
        {
            if is_query && target_id.is_none() && answered.is_none() && state.tracks_deleted {
                let scope = deleted::scope(op, call);
                deleted::drop_from_list(
                    &mut state,
                    session,
                    (plan, call),
                    t,
                    scope.as_deref(),
                    &mut generated,
                )
                .await;
            }
            if matches!(op.kind, OperationKind::Command(Effect::Create)) {
                unique_create_id(
                    rt,
                    &mut state,
                    (session, op, t, state_call),
                    &mut generated,
                    answered.as_ref(),
                )
                .await;
                next_numbers(rt, &mut state, session, t, &mut generated).await;
                // A create answered for the id of a record held already does not replace
                // it: nothing comes of it, and the answer agrees with the store. It is the
                // refusal the prompt offered on that record, when it did; otherwise (a
                // forced status, a scenario, no refusal declared or offered) the held
                // record, under the status answered, as an idempotent create answers.
                let repeated =
                    repeated_record(&mut state, (session, op, t, state_call), &generated).await;
                // Held in another collection (another parent's label of the same name):
                // the answer is served, but stored over that record it is not.
                if let Some(Repeated::Elsewhere(id)) = &repeated {
                    unstored = true;
                    tracing::info!(
                        mock = %rt.name,
                        operation = %op.id,
                        id = %log_capped(id),
                        "create not stored: its id is held in another collection"
                    );
                }
                if let Some(Repeated::Here(held)) = repeated {
                    unstored = true;
                    drop_all_effects(rt, op, &held.id, &mut generated);
                    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
                    let offered = same_values
                        .iter()
                        .any(|(st, id, member)| st == t && *id == held.id && member == field);
                    let refusal = offered
                        .then(|| plan.engine.refuse_repeat(rt.bundle(), op, call, &generated))
                        .flatten();
                    match refusal {
                        Some(mut refusal) => {
                            refusal.effect_outcomes =
                                std::mem::take(&mut generated.effect_outcomes);
                            refusal.dropped_effects = generated.dropped_effects;
                            answered = None;
                            generated = refusal;
                        }
                        None => generated.body = Some(held.data),
                    }
                }
            }
            // An action's new record goes under its own id, not over another's.
            if generated.new_record {
                unstored = !place_new_record(
                    rt,
                    &mut state,
                    (session, op, t, state_call),
                    &mut generated,
                    answered.as_ref(),
                )
                .await;
                if !unstored {
                    next_numbers(rt, &mut state, session, t, &mut generated).await;
                }
                let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
                let own = generated.body.as_ref().and_then(|b| b.get(field));
                if let Some(id) = own.and_then(id_string) {
                    target_id = Some(id);
                }
            }
            // A faker list standing in for a failed LLM call seeds nothing,
            // or its items would join the session's later LLM lists.
            let seeding = is_query
                && rt.consistency
                && target_id.is_none()
                && !transient_fallback(&generated);
            // A single answer with an identity of its own is one record, not a list of
            // what it holds (a weather reading's `weather` conditions). It is stored
            // under its identity only on a route with no path parameter, which then
            // reads it: one with a parameter that is not the identity (TMDB's
            // `/movie/{movie_id}/credits`) would make a type of what no read finds, and
            // one with no parameters at all (`/me`) is an earlier answer (see
            // [`world`]) instead.
            let (in_path, single_read) = match &op.binding {
                Binding::Http { path, .. } => (
                    path.contains('{'),
                    rt.index().single_reads.contains_key(path),
                ),
                _ => (false, false),
            };
            let own_record = op
                .identity
                .as_ref()
                .filter(|i| seeding && i.path_param.is_none())
                .and_then(|i| generated.body.as_ref()?.get(&i.field).and_then(id_string));
            if let Some(id) = own_record {
                if let Some(body) = generated.body.clone().filter(|_| !in_path && !single_read) {
                    let resource = StoredResource {
                        resource_type: t.to_string(),
                        id,
                        data: body,
                    };
                    state.insert_seeded(session, resource).await;
                }
            } else if (!is_query || seeding)
                && !unstored
                && !write_memory::keeps_stored(rt, plan, &generated, target.is_some())
            {
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
                        // What the key holds is another's: this write has no
                        // key of its own to go under.
                        target_id: target_id.clone().filter(|_| !foreign_key),
                        generated: &generated,
                        request_body: state_call.body.as_ref(),
                        collection: collection.clone(),
                        origin,
                        held: target.is_some() && !generated.new_record,
                    },
                )
                .await;
                if op.kind == OperationKind::Command(Effect::Delete)
                    && state.tracks_deleted
                    && !foreign_key
                    && let Some(id) = target_id.as_deref()
                {
                    deleted::mark(&mut state, session, (op, call), t, id, target.as_ref()).await;
                }
            }
        }
        // What else the envelope said, for a stored record to be answered the same.
        if let Some(answer) = answered.as_ref().filter(|_| !unstored) {
            envelopes::remember(&mut state, plan, answer, &generated).await;
        }
        if matches!(op.kind, OperationKind::Command(Effect::Create))
            && (200..300).contains(&generated.status)
            && !unstored
        {
            // The record an envelope holds is stored already, under its identity.
            let wrapped = answered.as_ref().map(|answer| GeneratedResponse {
                body: Some(answer.clone()),
                ..generated.clone()
            });
            let own = held_in.and_then(|i| match &i.record {
                Some(mb_mcm::RecordAt::Member(member)) => Some(member.as_str()),
                _ => None,
            });
            store_wrapped(
                &mut state,
                session,
                (resource_type, own),
                wrapped.as_ref().unwrap_or(&generated),
                op.output_for_status(generated.status)
                    .and_then(|o| o.schema.as_ref()),
                collection.as_ref(),
            )
            .await;
        }
        // A request naming a seed asks for that seed's answer, and a query for
        // one view: the cache keeps those apart, so neither is the resource.
        if rt.consistency
            && plain(plan)
            && mb_engine::seed::request_seed(call).is_none()
            // A catch-up's answer has the whole resource's schema, whatever the
            // query asked: it settles as a plain read does.
            && (call.query.is_empty()
                || (is_query && !earlier.shown.is_empty() && target.is_some()))
            && !transient_fallback(&generated)
            && (200..300).contains(&generated.status)
            && !matches!(op.binding, Binding::Graphql { .. })
        {
            if is_query && !earlier.shown.is_empty() {
                write_memory::settle_read(
                    &mut state,
                    plan,
                    call,
                    (target_id.as_deref(), target.as_ref()),
                    &mut earlier,
                    &mut generated,
                )
                .await;
            } else {
                seed_own_read(
                    &mut state,
                    session,
                    rt,
                    op,
                    call,
                    target_id.as_deref(),
                    &generated,
                )
                .await;
            }
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
            && !unstored
            && let Some(body) = generated.body.as_ref()
        {
            let schema = op
                .output_for_status(generated.status)
                .and_then(|o| o.schema.as_ref());
            // A create's answer may wrap its own resource.
            let created = resource_type
                .filter(|_| matches!(op.kind, OperationKind::Command(Effect::Create)))
                .zip(collection.as_ref());
            let body = answered.as_ref().unwrap_or(body);
            seed_entities(&mut state, session, rt, body, schema, created).await;
        }
        // Last: an effect on what this call just stored sees it.
        if rt.consistency && (200..300).contains(&generated.status) {
            // An effect on an update's own target goes on what was just stored, under
            // the protections the answer has.
            let own = resource_type
                .zip(acted_on.as_deref())
                .filter(|_| op.kind == OperationKind::Command(Effect::Update) && !op.is_action());
            let shown = shown_types(rt, target.as_ref(), &related, &created_since);
            resolve_effect_types(&mut state, session, rt, &shown, &mut generated.effects).await;
            write_memory::effects_on_target(&earlier, target.as_ref(), &mut generated.effects);
            if let Some(t) =
                resource_type.filter(|_| op.kind == OperationKind::Command(Effect::Create))
            {
                let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
                let created = generated
                    .body
                    .as_ref()
                    .and_then(|b| b.get(field))
                    .and_then(id_string);
                let effects = std::mem::take(&mut generated.effects);
                let (kept, dropped) =
                    sibling_effects(t, created.as_deref(), state_call.body.as_ref(), effects);
                generated.effects = kept;
                generated.dropped_effects += dropped.len();
                generated
                    .effect_outcomes
                    .extend(dropped.into_iter().map(|effect| {
                        EffectOutcome {
                            effect,
                            rejected: Some(
                                "a create copies its request onto no other record of its type"
                                    .to_string(),
                            ),
                            merged: None,
                            applied: Vec::new(),
                        }
                    }));
            }
            let outcomes = apply_effects(
                &mut state,
                session,
                rt,
                &generated.effects,
                own,
                (plan, call),
            )
            .await;
            generated.effect_outcomes.extend(outcomes);
            write_memory::serve_own_effects(rt, op, own, &mut generated);
            if generated.dropped_effects > 0 {
                metrics::counter!(
                    EFFECTS_METRIC,
                    "mock" => rt.name.clone(),
                    "outcome" => "rejected"
                )
                .increment(generated.dropped_effects as u64);
            }
            if is_query {
                write_memory::settle_by_effects(
                    &mut state,
                    plan,
                    &earlier,
                    target.as_ref(),
                    &mut generated,
                )
                .await;
            }
        }
        // After the effects: one on the resource means it shows the write.
        write_memory::after_write(
            &mut state,
            rt,
            plan,
            &write_memory::Written {
                call: state_call,
                generated: &generated,
                acted_on: acted_on.as_deref(),
                held: target.as_ref(),
                foreign_key,
                unstored,
                earlier: &earlier,
            },
        )
        .await;
        write_memory::after_route_write(
            &mut state,
            plan,
            call,
            &generated,
            foreign_key || unstored,
        )
        .await;
        // Served as it was answered, holding the record as state now has it.
        if let (Some(identity), Some(mut answer)) = (held_in, answered.take()) {
            if let (Some(record), Some(slot)) =
                (generated.body.take(), identity.record_in_mut(&mut answer))
            {
                *slot = record;
            }
            generated.body = Some(answer);
        }
        world::list_session(&mut state, plan, unlisted, &generated).await;
        minted::after_answer(&mut state, plan, call, &generated).await;
        cursors::after_answer(&mut state, plan, &generated).await;
        world::after_answer(&mut state, plan, call, &generated).await;
        world::after_delete(&mut state, plan, acted_on.as_deref(), &generated).await;
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
    if !unstored {
        crate::webhooks::emit::emit(
            rt,
            &plan.engine,
            op,
            call,
            &generated,
            target_id.as_deref(),
            session,
        );
    }
    #[cfg(any(
        feature = "http-rest",
        feature = "graphql",
        feature = "grpc",
        feature = "soap"
    ))]
    crate::webhooks::emit::emit_effects(rt, op, &generated.effect_outcomes, session);

    // Seeded first, then cached: a later list never clears a cached one.
    // A fallback is cached too: a schema the LLM cannot answer in time
    // would otherwise make every identical repeat wait as long again. Not one
    // of a read still owing earlier requests, which is to try again.
    if let (Some(response_cache), Some((since, key))) = (rt.state.response_cache(), cache)
        && (200..300).contains(&generated.status)
        && earlier.cacheable(&generated)
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

/// Counter of the effects an answer listed, by `mock` and `outcome`
/// (`applied` or `rejected`).
const EFFECTS_METRIC: &str = "mb_effects_total";

/// Characters of an answer's own text that a log line carries, in one field.
const MAX_LOGGED_CHARS: usize = 64;

/// `text`, which an LLM wrote, cut to fit a log line.
fn log_capped(text: &str) -> String {
    let mut chars = text.chars();
    let mut capped: String = chars.by_ref().take(MAX_LOGGED_CHARS).collect();
    if chars.next().is_some() {
        capped.push('…');
    }
    capped
}

/// Applies what an answer says it changed in other resources, in this session
/// only, and counts each effect. A rejected effect costs itself: the answer is
/// served and the next effect still applies. One on `own`, the (type, id) an update
/// just stored, keeps only what [`write_memory::own_effect_members`] allows.
async fn apply_effects(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    effects: &[ResourceEffect],
    own: Option<(&str, &str)>,
    source: (&CallPlan<'_>, &OperationCall),
) -> Vec<EffectOutcome> {
    let mut outcomes = Vec::with_capacity(effects.len());
    for effect in effects {
        let on_own = own == Some((effect.resource_type.as_str(), effect.id.as_str()));
        let result = apply_effect(state, session, rt, effect, on_own.then_some(source)).await;
        tracing::info!(
            mock = %rt.name,
            resource_type = %log_capped(&effect.resource_type),
            id = %log_capped(&effect.id),
            rejected = result.as_ref().err().copied(),
            "effect of an answer"
        );
        let outcome = if result.is_ok() {
            "applied"
        } else {
            "rejected"
        };
        metrics::counter!(EFFECTS_METRIC, "mock" => rt.name.clone(), "outcome" => outcome)
            .increment(1);
        // An applied effect is told by what it set, as it now stands: its dotted
        // keys as the members they went into.
        let (effect, merged, rejected) = match result {
            Ok((merged, set)) => (
                ResourceEffect {
                    set,
                    ..effect.clone()
                },
                Some(merged),
                None,
            ),
            Err(reason) => (effect.clone(), None, Some(reason.to_string())),
        };
        let applied = match rejected {
            None => effect.set.keys().cloned().collect(),
            Some(_) => Vec::new(),
        };
        outcomes.push(EffectOutcome {
            effect,
            rejected,
            merged,
            applied,
        });
    }
    outcomes
}

/// Adds what became of `generated`'s effects to a request record's `request`
/// (nothing when it listed none), under `effects`.
pub(crate) fn record_effects(request: &mut Value, generated: &GeneratedResponse) {
    if generated.effect_outcomes.is_empty() {
        return;
    }
    let recorded = generated.effect_outcomes.iter().map(|o| {
        let mut record = serde_json::json!({
            "type": o.effect.resource_type,
            "id": o.effect.id,
            "set": o.effect.set,
            "outcome": if o.rejected.is_some() { "rejected" } else { "applied" },
        });
        if let Some(reason) = &o.rejected {
            record["reason"] = Value::from(reason.as_str());
        }
        record
    });
    if request.is_null() {
        *request = serde_json::json!({});
    }
    if let Value::Object(request) = request {
        match request.get_mut("effects") {
            Some(Value::Array(list)) => list.extend(recorded),
            _ => {
                request.insert("effects".to_string(), Value::Array(recorded.collect()));
            }
        }
    }
}

/// Whether resource type `t` has a schema an effect on it is checked against.
fn has_schema(index: &BundleIndex, t: &str) -> bool {
    index.detail_reads.contains_key(t) || index.other_shapes.contains_key(t)
}

/// Whether `t` is a resource type of the contract's: one an operation acts on, or a
/// definition is of. Such a name is what it says, schema or not.
fn known_type(rt: &MockRuntime, t: &str) -> bool {
    rt.bundle()
        .operations
        .iter()
        .any(|op| op.resource_type.as_deref() == Some(t))
        || rt.index().types_by_def.values().any(|v| v == t)
}

/// The resource types a prompt showed, in the order it showed them, each once, of
/// those an effect can be checked against (see [`mb_engine::prompt`]'s effects note).
fn shown_types(
    rt: &MockRuntime,
    target: Option<&StoredResource>,
    related: &[StoredResource],
    created_since: &[StoredResource],
) -> Vec<String> {
    let max = mb_engine::prompt::MAX_RELATED;
    let index = rt.index();
    let mut types: Vec<String> = Vec::new();
    for r in target
        .into_iter()
        .chain(related.iter().take(max))
        .chain(created_since.iter().take(max))
    {
        if has_schema(index, &r.resource_type) && !types.contains(&r.resource_type) {
            types.push(r.resource_type.clone());
        }
    }
    types
}

/// `name` as [`mb_mcm::stem`] has it, its words run together: `meetingRegistrant`,
/// `meeting_registrants` and `Meeting-Registrant` are one.
fn type_stem(name: &str) -> String {
    let joined: String = name.chars().filter(|c| c.is_alphanumeric()).collect();
    mb_mcm::stem(&joined)
}

/// The type of `shown` that `name` means, `holding` being those of `shown` that hold a
/// record under the effect's id. The stem decides (see [`type_stem`]): the one shown
/// type of that stem, unless the id is held elsewhere and not by it; of several of
/// that stem, the one holding the id. An id alone names no type.
fn stem_type<'a>(shown: &'a [String], name: &str, holding: &[&str]) -> Option<&'a str> {
    let stem = type_stem(name);
    let matching: Vec<&'a str> = shown
        .iter()
        .filter(|t| type_stem(t) == stem)
        .map(String::as_str)
        .collect();
    match matching.as_slice() {
        [] => None,
        [only] => (holding.is_empty() || holding.contains(only)).then_some(*only),
        several => {
            let held: Vec<&'a str> = several
                .iter()
                .copied()
                .filter(|t| holding.contains(t))
                .collect();
            match held.as_slice() {
                [one] => Some(*one),
                _ => None,
            }
        }
    }
}

/// Gives each effect whose type the contract does not know (models write `playlist`
/// for the `playlists` they were shown) the `shown` type it means (see
/// [`stem_type`]). With none, the effect is left as it came, and is rejected.
async fn resolve_effect_types(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    shown: &[String],
    effects: &mut [ResourceEffect],
) {
    let index = rt.index();
    for effect in effects.iter_mut() {
        if has_schema(index, &effect.resource_type) || known_type(rt, &effect.resource_type) {
            continue;
        }
        if !shown
            .iter()
            .any(|t| type_stem(t) == type_stem(&effect.resource_type))
        {
            continue;
        }
        let mut holding = Vec::new();
        for t in shown {
            if state.get(session, t, &effect.id).await.is_some() {
                holding.push(t.as_str());
            }
        }
        if let Some(t) = stem_type(shown, &effect.resource_type, &holding) {
            tracing::debug!(
                mock = %rt.name,
                written = %log_capped(&effect.resource_type),
                resource_type = %t,
                "effect type resolved to a shown type"
            );
            effect.resource_type = t.to_string();
        }
    }
}

/// What one place in a record's schema says, by its `$ref`s and compositions (see
/// [`schema_parts`]).
struct Place<'a> {
    parts: Vec<&'a Value>,
    defs: Vec<&'a str>,
}

impl<'a> Place<'a> {
    fn of(rt: &'a MockRuntime, schema: Option<&'a Value>) -> Self {
        let (parts, defs) = schema_parts(rt.bundle(), schema);
        Self { parts, defs }
    }

    /// The schema the place's `properties` give member `key`.
    fn property(&self, key: &str) -> Option<&'a Value> {
        self.parts
            .iter()
            .find_map(|p| p.get("properties")?.get(key))
    }

    /// Whether a part of the place lists member `key` as `required`.
    fn requires(&self, key: &str) -> bool {
        self.parts.iter().any(|p| {
            p.get("required")
                .and_then(Value::as_array)
                .is_some_and(|r| r.iter().any(|k| k.as_str() == Some(key)))
        })
    }

    /// [`Place::of`], where a `oneOf` or `anyOf` counts only the branches `held`, what
    /// the place holds now, validates against (all of them when it validates against
    /// none, or nothing is held): what one variant requires or lets be null says
    /// nothing of another.
    fn of_held(rt: &'a MockRuntime, schema: Option<&'a Value>, held: Option<&Value>) -> Self {
        fn visit<'a>(
            bundle: &'a mb_mcm::Bundle,
            schema: &'a Value,
            held: Option<&Value>,
            depth: usize,
            place: &mut Place<'a>,
        ) {
            if depth > 8 || place.parts.len() >= MAX_SCHEMA_PARTS {
                return;
            }
            if let Some(def) = def_name(schema) {
                place.defs.push(def);
                if let Some(target) = bundle.defs.get(def) {
                    visit(bundle, target, held, depth + 1, place);
                }
                return;
            }
            place.parts.push(schema);
            for key in ["anyOf", "oneOf", "allOf"] {
                let branches: Vec<&Value> = schema
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .collect();
                let matching: Vec<&Value> = match held.filter(|_| key != "allOf") {
                    Some(held) => branches
                        .iter()
                        .copied()
                        .filter(|b| {
                            mb_engine::SchemaValidator::new(&bundle.with_reachable_defs(b))
                                .is_ok_and(|v| v.is_valid(held))
                        })
                        .collect(),
                    None => Vec::new(),
                };
                let chosen = if matching.is_empty() {
                    branches
                } else {
                    matching
                };
                for branch in chosen {
                    visit(bundle, branch, held, depth + 1, place);
                }
            }
        }
        let mut place = Self {
            parts: Vec::new(),
            defs: Vec::new(),
        };
        if let Some(schema) = schema {
            visit(rt.bundle(), schema, held, 0, &mut place);
        }
        place
    }

    /// What a `null` for member `key` is at this place: kept where the place does
    /// not declare the member or its schema admits null (see [`admits_null`]),
    /// otherwise the member's removal when it is optional and refused when it is
    /// required. For a union, build the place with [`Place::of_held`].
    fn null_member(&self, rt: &MockRuntime, key: &str) -> NullMember {
        match self.property(key) {
            Some(schema) if !admits_null(rt, schema) => {
                if self.requires(key) {
                    NullMember::Refused
                } else {
                    NullMember::Removed
                }
            }
            _ => NullMember::Kept,
        }
    }

    /// Whether the place declares any `properties`.
    fn declares(&self) -> bool {
        self.parts
            .iter()
            .any(|p| p.get("properties").and_then(Value::as_object).is_some())
    }

    /// Whether the place is a map: `additionalProperties` and nothing declared.
    fn is_map(&self) -> bool {
        !self.declares()
            && self.parts.iter().any(|p| {
                matches!(
                    p.get("additionalProperties"),
                    Some(Value::Object(_) | Value::Bool(true))
                )
            })
    }

    /// Whether the place holds an object.
    fn is_object(&self) -> bool {
        self.declares()
            || self.is_map()
            || self
                .parts
                .iter()
                .any(|p| p.get("type").and_then(Value::as_str) == Some("object"))
    }

    /// Whether `new` is of the variant `held` is, at a place that is a union of two or
    /// more object branches (`oneOf`, `anyOf`): no two values of its discriminator,
    /// `discriminator.propertyName` or else a member a part of every object branch pins
    /// (`const`, a one-value `enum`), tell them apart. Anywhere else there is one variant.
    fn same_variant(
        &self,
        rt: &MockRuntime,
        held: &Map<String, Value>,
        new: &Map<String, Value>,
    ) -> bool {
        let Some((part, branches)) = self.parts.iter().find_map(|p| {
            let branches = p
                .get("oneOf")
                .or_else(|| p.get("anyOf"))
                .and_then(Value::as_array)?;
            let objects: Vec<Place<'_>> = branches
                .iter()
                .map(|b| Place::of(rt, Some(b)))
                .filter(Place::is_object)
                .collect();
            (objects.len() >= 2).then_some((*p, objects))
        }) else {
            return true;
        };
        // Any part of a branch may pin it (`allOf: [$ref Base, {...}]`), whatever
        // the others declare.
        let pins = |key: &str| {
            branches.iter().all(|b| {
                b.parts.iter().any(|p| {
                    p.get("properties")
                        .and_then(|members| members.get(key))
                        .is_some_and(|s| {
                            s.get("const").is_some()
                                || s.get("enum")
                                    .and_then(Value::as_array)
                                    .is_some_and(|e| e.len() == 1)
                        })
                })
            })
        };
        let key = part
            .get("discriminator")
            .and_then(|d| d.get("propertyName"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                let first = branches.first()?;
                first
                    .parts
                    .iter()
                    .filter_map(|p| p.get("properties").and_then(Value::as_object))
                    .flat_map(|members| members.keys())
                    .find(|k| pins(k))
                    .cloned()
            });
        // Another variant only when both say which, and differ: a value that leaves
        // it out says nothing of its variant. With no discriminator to tell, the
        // variants cannot be told apart, and the value is taken for another one.
        key.is_some_and(|k| !matches!((held.get(&k), new.get(&k)), (Some(a), Some(b)) if a != b))
    }

    /// The resource type the place's `$ref` is a definition of.
    fn record_type(&self, index: &BundleIndex) -> Option<String> {
        self.defs
            .iter()
            .find_map(|d| index.types_by_def.get(*d))
            .cloned()
    }
}

/// What a `null` set on a declared member becomes (see [`Place::null_member`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NullMember {
    /// Stored as null: the member's schema admits it, or the place does not declare it.
    Kept,
    /// The member is removed: it is optional, and its schema does not admit null.
    Removed,
    /// Not applied: the member is required, and its schema does not admit null.
    Refused,
}

/// Whether `schema` admits `null`: a part of it (by its `$ref`s and compositions)
/// has type `null` (or `nullable: true`), a `const` null or an `enum` holding null,
/// or none of its parts says what type it is.
fn admits_null(rt: &MockRuntime, schema: &Value) -> bool {
    let parts = Place::of(rt, Some(schema)).parts;
    let null = Value::Null;
    let says_null = |p: &&Value| {
        p.get("nullable").and_then(Value::as_bool) == Some(true)
            || match p.get("type") {
                Some(Value::String(t)) => t == "null",
                Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("null")),
                _ => false,
            }
            || p.get("const") == Some(&null)
            || p.get("enum")
                .and_then(Value::as_array)
                .is_some_and(|e| e.contains(&null))
    };
    let constrains = |p: &&Value| {
        [
            "type",
            "const",
            "enum",
            "properties",
            "items",
            "additionalProperties",
        ]
        .iter()
        .any(|k| p.get(*k).is_some())
    };
    parts.iter().any(says_null) || !parts.iter().any(constrains)
}

/// Whether `value` is what a prompt shows in place of a redacted one.
fn is_redacted(value: &Value) -> bool {
    value.as_str() == Some(mb_engine::REDACTED)
}

/// `new` where nothing is kept: what was redacted in it is left out (see
/// [`mb_engine::without_redacted`]); none when nothing is left.
fn unredacted(new: &Value) -> Option<Value> {
    mb_engine::without_redacted(new)
}

/// `new` replacing `held` at the same place, what was redacted in it as `held` has it
/// there (left out where `held` has nothing). An array holding anything redacted
/// stays as held: it is replaced whole, and a prompt showed it redacted.
fn restored(held: Option<&Value>, new: &Value) -> Option<Value> {
    if is_redacted(new) {
        return held.cloned();
    }
    match new {
        Value::Object(members) => {
            let held = held.and_then(Value::as_object);
            Some(Value::Object(
                members
                    .iter()
                    .filter_map(|(k, v)| {
                        Some((k.clone(), restored(held.and_then(|h| h.get(k)), v)?))
                    })
                    .collect(),
            ))
        }
        Value::Array(_) if unredacted(new).as_ref() != Some(new) => held.cloned(),
        other => Some(other.clone()),
    }
}

/// `new` put where `held` is (none: nothing is there), at a place of schema `schema`;
/// none when nothing is to be there. Objects merge member by member where the place
/// declares members (a value object), as the members' own places say. These are
/// replaced whole: anything that is not an object; a map (see [`Place::is_map`]) or
/// an object the schema says nothing of, what was redacted in it kept as it was at
/// its own key (see [`restored`]); and an object of another variant of a union (see
/// [`Place::same_variant`]) or naming another identity (see [`reassigns`]), which
/// keeps nothing of `held`, redacted or not. In a value object, a `null` its
/// optional member's schema does not admit removes the member (see
/// [`Place::null_member`]); on a required member it stays, for the record's own
/// check to refuse.
fn merge_at(
    rt: &MockRuntime,
    held: Option<&Value>,
    new: &Value,
    schema: Option<&Value>,
) -> Option<Value> {
    let Some(held) = held else {
        // A new value object's nulls go as its members' places say.
        if new
            .as_object()
            .is_some_and(|m| m.values().any(Value::is_null))
            && Place::of(rt, schema).declares()
        {
            return merge_at(rt, Some(&Value::Object(Map::new())), new, schema);
        }
        return unredacted(new);
    };
    let (Value::Object(held_members), Value::Object(new_members)) = (held, new) else {
        return restored(Some(held), new);
    };
    let place = Place::of(rt, schema);
    if !place.same_variant(rt, held_members, new_members)
        || reassigns(rt, &place, held_members, new_members)
    {
        return unredacted(new);
    }
    if !place.declares() {
        return restored(Some(held), new);
    }
    // Nulls go by the variant of a union the object is now.
    let nulls = new_members
        .values()
        .any(Value::is_null)
        .then(|| Place::of_held(rt, schema, Some(held)));
    let mut out = held_members.clone();
    for (key, value) in new_members {
        // A null the member's schema does not admit removes an optional member (see
        // [`Place::null_member`]). On a required one it stays, and the merged record
        // breaks its schema: what sets it is refused whole.
        if value.is_null()
            && nulls.as_ref().map(|p| p.null_member(rt, key)) == Some(NullMember::Removed)
        {
            out.remove(key);
            continue;
        }
        if let Some(merged) = merge_at(rt, out.get(key), value, place.property(key)) {
            out.insert(key.clone(), merged);
        }
    }
    Some(Value::Object(out))
}

/// Whether `new` names another object than `held` at `place`. For a place of a
/// resource type, an identity member of that type (see [`identity_members`]) the place
/// declares holds another value. For an inline object, a scalar member that is a
/// generic id (`id`, `_id`, see [`mb_mcm::is_generic_id`]) does, of any type, or a
/// string member the place declares `readOnly`, by its `$ref`s and `allOf`s too.
fn reassigns(
    rt: &MockRuntime,
    place: &Place<'_>,
    held: &Map<String, Value>,
    new: &Map<String, Value>,
) -> bool {
    let changed = |k: &str| matches!((held.get(k), new.get(k)), (Some(a), Some(b)) if a != b);
    match place.record_type(rt.index()) {
        Some(t) => identity_members(rt, &t)
            .iter()
            .filter(|k| place.property(k).is_some())
            .any(|k| changed(k)),
        None => new.iter().any(|(k, v)| {
            let scalar = |v: &Value| !v.is_object() && !v.is_array();
            let read_only = || {
                Place::of(rt, place.property(k))
                    .parts
                    .iter()
                    .any(|p| p.get("readOnly").and_then(Value::as_bool) == Some(true))
            };
            changed(k)
                && if mb_mcm::is_generic_id(k) {
                    scalar(v) && held.get(k).is_some_and(scalar)
                } else {
                    v.is_string() && held.get(k).is_some_and(Value::is_string) && read_only()
                }
        }),
    }
}

/// Puts `set` on `data`, the record of type `t`, each member as its place says (see
/// [`merge_at`]).
fn merge_members(
    rt: &MockRuntime,
    t: &str,
    data: &mut Map<String, Value>,
    set: Map<String, Value>,
) {
    let place = Place::of(rt, write_memory::read_schema(rt, t));
    for (key, value) in set {
        if let Some(merged) = merge_at(rt, data.get(&key), &value, place.property(&key)) {
            data.insert(key, merged);
        }
    }
}

/// The keys a dotted key of a record of type `t` is a path of, and the schema of the
/// place it ends at; none when it is no path. It is one when the schema does not
/// declare the dotted name and declares its first segment as an object. It goes
/// on through declared `properties` only: past a map, or a segment not declared,
/// the rest is one key (`annotations.example.com/owner`).
fn dotted_path<'a>(
    rt: &'a MockRuntime,
    record: &Place<'a>,
    key: &str,
) -> Option<(Vec<String>, Option<&'a Value>)> {
    let segments: Vec<&str> = key.split('.').collect();
    let (first, rest) = segments.split_first()?;
    if rest.is_empty() || segments.iter().any(|s| s.is_empty()) || record.property(key).is_some() {
        return None;
    }
    let mut schema = record.property(first)?;
    if !Place::of(rt, Some(schema)).is_object() {
        return None;
    }
    let mut path = vec![(*first).to_string()];
    for (i, segment) in rest.iter().enumerate() {
        let place = Place::of(rt, Some(schema));
        match place.property(segment).filter(|_| !place.is_map()) {
            Some(inner) => {
                path.push((*segment).to_string());
                schema = inner;
            }
            None => {
                path.push(rest[i..].join("."));
                return Some((path, None));
            }
        }
    }
    Some((path, Some(schema)))
}

/// `value` put at `path` below `base`, the last key as its place says (see
/// [`merge_at`]); what is not an object on the way becomes one.
fn put_at(
    rt: &MockRuntime,
    base: &mut Value,
    path: &[String],
    value: &Value,
    schema: Option<&Value>,
) {
    let Some((key, rest)) = path.split_first() else {
        return;
    };
    if !base.is_object() {
        *base = Value::Object(Map::new());
    }
    let Value::Object(members) = base else {
        return;
    };
    if rest.is_empty() {
        if let Some(merged) = merge_at(rt, members.get(key), value, schema) {
            members.insert(key.clone(), merged);
        }
        return;
    }
    let entry = members.entry(key.clone()).or_insert(Value::Null);
    put_at(rt, entry, rest, value, schema);
}

/// `set` with each dotted key that is a path (see [`dotted_path`]) put into the member
/// its first segment names, as `held` (the stored record of type `t`) has it: a whole
/// new value for that member, with the rest of it as stored. A dotted name the schema
/// declares, or one into no declared object, stays as it is.
fn dotted_as_paths(
    rt: &MockRuntime,
    t: &str,
    held: &Map<String, Value>,
    set: &Map<String, Value>,
) -> Map<String, Value> {
    if !set.keys().any(|k| k.contains('.')) {
        return set.clone();
    }
    let record = Place::of(rt, write_memory::read_schema(rt, t));
    let mut out = Map::new();
    for (key, value) in set {
        match dotted_path(rt, &record, key) {
            Some((path, schema)) => {
                let Some((first, rest)) = path.split_first() else {
                    continue;
                };
                let mut member = out
                    .get(first)
                    .or_else(|| held.get(first))
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new()));
                put_at(rt, &mut member, rest, value, schema);
                out.insert(first.clone(), member);
            }
            None => {
                // After a path into it, a member is merged as its place says.
                let merged = match out.get(key) {
                    Some(was) => merge_at(rt, Some(was), value, record.property(key)),
                    None => Some(value.clone()),
                };
                if let Some(merged) = merged {
                    out.insert(key.clone(), merged);
                }
            }
        }
    }
    out
}

/// The members a record of type `t` is identified by: its read's key, `id`,
/// `number` and the fields its operations identify it by.
fn identity_members<'a>(rt: &'a MockRuntime, t: &str) -> Vec<&'a str> {
    let index = rt.index();
    let own = index
        .detail_reads
        .get(t)
        .and_then(|i| rt.bundle().operations.get(*i)?.identity.as_ref())
        .map(|identity| identity.field.as_str());
    own.into_iter()
        .chain(["id", "number"])
        .chain(index.identity_fields(t).iter().map(String::as_str))
        .collect()
}

/// Whether `object` names `stored` and says nothing more: each of its members is one
/// of `identity`, or a string `stored` holds as well (a `uri`, an `href`), and every
/// one is as `stored` has it. It holds less than `stored`, or there is nothing to fill.
fn is_stub(object: &Map<String, Value>, stored: &Value, identity: &[&str]) -> bool {
    let Value::Object(held) = stored else {
        return false;
    };
    object.len() < held.len()
        && object.iter().any(|(k, _)| identity.contains(&k.as_str()))
        && object.iter().all(|(k, v)| {
            (identity.contains(&k.as_str()) || v.is_string()) && held.get(k) == Some(v)
        })
}

/// `stored` with only the members `place` declares (all of it when it declares none).
fn as_declared(stored: &Value, place: &Place<'_>) -> Value {
    match stored {
        Value::Object(members) if place.declares() => Value::Object(
            members
                .iter()
                .filter(|(k, _)| place.property(k).is_some())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Records one effect's stub fill reads from the store at most, all its members
/// together.
const MAX_STUB_READS: usize = 20;

/// Fills, in the members `keys` of record `value` of type `t`, each object that only
/// names a record (see [`is_stub`]) at a place whose `$ref` is a definition of that
/// record's type, with that record as the session stores it and as the place
/// declares it (see [`as_declared`]).
async fn fill_stubs(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    t: &str,
    value: &mut Value,
    keys: &[String],
) {
    let record = Place::of(rt, write_memory::read_schema(rt, t));
    // First what the stubs name, then the records under it.
    let mut named: Vec<(String, String)> = Vec::new();
    if let Value::Object(members) = &*value {
        let mut walk = StubWalk {
            rt,
            fill: &mut |rtype: &str, id: &str, _: &Map<String, Value>, _: &Place<'_>| {
                let pair = (rtype.to_string(), id.to_string());
                if named.len() < MAX_STUB_READS && !named.contains(&pair) {
                    named.push(pair);
                }
                None
            },
        };
        for key in keys {
            if let Some(member) = members.get(key) {
                let mut probe = member.clone();
                walk.visit(&mut probe, record.property(key), 1);
            }
        }
    }
    let mut held: HashMap<(String, String), Value> = HashMap::new();
    for (rtype, id) in named {
        if let Some(found) = state.get(session, &rtype, &id).await {
            held.insert((rtype, id), found.data);
        }
    }
    if held.is_empty() {
        return;
    }
    if let Value::Object(members) = value {
        let mut walk = StubWalk {
            rt,
            fill: &mut |rtype: &str, id: &str, object: &Map<String, Value>, place: &Place<'_>| {
                let stored = as_declared(held.get(&(rtype.to_string(), id.to_string()))?, place);
                let identity = identity_members(rt, rtype);
                is_stub(object, &stored, &identity).then_some(stored)
            },
        };
        for key in keys {
            if let Some(member) = members.get_mut(key) {
                walk.visit(member, record.property(key), 1);
            }
        }
    }
}

/// What [`StubWalk`] asks of each object at a place holding a resource type: given
/// the type, the id the object names, the object and its place, the value to put
/// in its place, if any.
type StubFill<'f> = dyn FnMut(&str, &str, &Map<String, Value>, &Place<'_>) -> Option<Value> + 'f;

/// The objects an effect's member holds at places whose `$ref` is a definition of a
/// resource type, with the id each names; `fill` may put another value in its place.
struct StubWalk<'a, 'f> {
    rt: &'a MockRuntime,
    fill: &'f mut StubFill<'f>,
}

impl<'a> StubWalk<'a, '_> {
    fn visit(&mut self, value: &mut Value, schema: Option<&'a Value>, depth: usize) {
        if depth > MAX_ENTITY_DEPTH {
            return;
        }
        let place = Place::of(self.rt, schema);
        match value {
            Value::Array(items) => {
                let item = place.parts.iter().find_map(|p| p.get("items"));
                for value in items {
                    self.visit(value, item, depth + 1);
                }
            }
            Value::Object(fields) => {
                if let Some(rtype) = place.record_type(self.rt.index())
                    && let Some(id) = identity_members(self.rt, &rtype)
                        .into_iter()
                        .find_map(|f| fields.get(f).and_then(id_string))
                    && let Some(record) = (self.fill)(&rtype, &id, fields, &place)
                {
                    *value = record;
                    return;
                }
                for (field, value) in fields.iter_mut() {
                    if value.is_object() || value.is_array() {
                        self.visit(value, place.property(field), depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Merges `effect` into the stored resource it names and returns the result and the
/// members it set, as they now are; `Err` says why not. The resource must exist (an
/// effect never creates one, and never deletes one), keep its identity and owner and,
/// merged, still suit its type, which must have a schema to check against (see
/// [`entity_fits`]). Each member goes as its place in the schema says (see
/// [`merge_at`]), a dotted key may be a path (see [`dotted_as_paths`]), and objects
/// that only name stored records are filled with them (see [`fill_stubs`]). Member
/// names written with JSON around them are the declared ones first (see
/// [`mended_set`]). A `null` a member's schema does not admit removes the member when
/// it is optional and rejects the effect when it is required (see
/// [`Place::null_member`]). It applies whole or not at all.
async fn apply_effect(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    effect: &ResourceEffect,
    own: Option<(&CallPlan<'_>, &OperationCall)>,
) -> Result<(Value, Map<String, Value>), &'static str> {
    let t = effect.resource_type.as_str();
    let index = rt.index();
    let set = mended_set(rt, t, &effect.set);
    let identity = index.identity_fields(t);
    if set
        .keys()
        .any(|k| matches!(k.as_str(), "id" | "number") || identity.contains(k))
    {
        return Err("it sets an identity field");
    }
    if !has_schema(index, t) {
        return Err("no schema to check against");
    }
    let Some(mut stored) = state.get(session, t, &effect.id).await else {
        return Err("no such resource");
    };
    let held = stored.clone();
    let Value::Object(data) = &mut stored.data else {
        return Err("the stored resource is not an object");
    };
    let requested = dotted_as_paths(rt, t, data, &set);
    // What was redacted is restored, or left out, where it merges (see [`merge_at`]).
    if !set.is_empty() && set.values().all(|v| unredacted(v).is_none()) {
        return Err("it sets only what was redacted");
    }
    let requested = if let Some((plan, call)) = own {
        let set = write_memory::own_effect_members(rt, plan, call, &held, &requested);
        if set.is_empty() {
            return Err("it sets no member an effect on this update's target may");
        }
        set
    } else {
        requested
    };
    // A null the member's schema does not admit (in the variant of a union the record
    // is) removes an optional member; on a required one, the effect is refused.
    let record = Place::of_held(rt, write_memory::read_schema(rt, t), Some(&held.data));
    let mut requested = requested;
    let mut removed = Vec::new();
    let mut refused = false;
    requested.retain(|key, value| {
        if !value.is_null() {
            return true;
        }
        match record.null_member(rt, key) {
            NullMember::Kept => true,
            NullMember::Removed => {
                removed.push(key.clone());
                false
            }
            NullMember::Refused => {
                refused = true;
                true
            }
        }
    });
    if refused {
        return Err("it sets a required member to null");
    }
    for key in &removed {
        data.remove(key);
    }
    let keys: Vec<String> = requested.keys().cloned().collect();
    merge_members(rt, t, data, requested);
    // Objects the effect names only by their identity are the records stored under it.
    fill_stubs(state, session, rt, t, &mut stored.data, &keys).await;
    if !entity_fits(rt, t, &stored.data) {
        return Err("the merged resource breaks its schema");
    }
    if let Some((plan, call)) = own
        && !write_memory::keeps_owner(rt, plan, call, &held, &stored.data)
    {
        return Err("the merged resource is no longer its owner's");
    }
    let applied: Map<String, Value> = keys
        .iter()
        .filter_map(|k| Some((k.clone(), stored.data.get(k)?.clone())))
        .collect();
    let merged = stored.data.clone();
    let carried = stored.clone();
    if !state.upsert(session, stored).await {
        return Err("the store took no write");
    }
    rt.collections().carry(session, &held, &carried);
    Ok((merged, applied))
}

/// `set`, an effect's members for a record of type `t`, with each name written with
/// JSON around it (`"status:"`) the member the type's schema declares (see
/// [`mb_engine::mend_member_names`]); as it is when the type has no schema.
fn mended_set(rt: &MockRuntime, t: &str, set: &Map<String, Value>) -> Map<String, Value> {
    let Some(schema) = write_memory::read_schema(rt, t) else {
        return set.clone();
    };
    let bundle = rt.bundle();
    let resolve = |r: &str| r.strip_prefix("#/$defs/").and_then(|n| bundle.defs.get(n));
    let mut value = Value::Object(set.clone());
    if mb_engine::mend_member_names(schema, &resolve, &mut value) > 0 {
        tracing::debug!(mock = %rt.name, resource_type = %t, "mended member names in an effect");
    }
    match value {
        Value::Object(mended) => mended,
        _ => set.clone(),
    }
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

/// The id of `generated`'s answer, when the request did not ask for it and the
/// session already holds a resource of this type under it, or the request carries it
/// (see [`request_values`]) as the id of a record of another type the session holds
/// (see [`world::held_elsewhere`]): a new message is not the channel it names. Another
/// type's id the request does not carry says nothing, and neither does a value the
/// request carries that is no held record's (`{"name": "my-bucket"}`).
async fn taken_id(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    resource_type: &str,
    call: &OperationCall,
    generated: &GeneratedResponse,
) -> Option<String> {
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let id = generated.body.as_ref()?.get(field).and_then(id_string)?;
    // A create asks for an id only under a member its request declares: one it does
    // not, the API mints over (see [`mb_engine::guards::asked_identity`]).
    let requested = match op.kind {
        OperationKind::Command(Effect::Create) => {
            mb_engine::guards::asked_identity(state.rt.bundle(), op, call)
        }
        _ => call
            .body
            .as_ref()
            .and_then(|b| b.get(field))
            .and_then(id_string),
    };
    if requested.as_deref() == Some(id.as_str()) {
        return None;
    }
    if state.get(session, resource_type, &id).await.is_some()
        || (request_values(op, call).contains(&id)
            && world::held_elsewhere(state, session, resource_type, &id).await)
    {
        return Some(id);
    }
    None
}

/// A different identity for `record`, the record a create's answer holds (in
/// `answer`, when it holds it in an envelope or an array), drawn for `attempt`.
fn fresh_record(
    rt: &MockRuntime,
    op: &Operation,
    status: u16,
    (record, answer): (&Value, Option<&Value>),
    attempt: u32,
) -> Option<Value> {
    let Some((identity, answer)) = op
        .identity
        .as_ref()
        .filter(|i| i.record.is_some())
        .zip(answer)
    else {
        return rt
            .engine
            .fresh_identity_at(&rt.bundle, op, status, record, attempt);
    };
    let mut held = answer.clone();
    *identity.record_in_mut(&mut held)? = record.clone();
    let fresh = rt
        .engine
        .fresh_identity_at(&rt.bundle, op, status, &held, attempt)?;
    identity.record_in(&fresh).cloned()
}

/// The values a request carries in its path and, as strings, anywhere in its body. A
/// create's path value naming the record itself, and a body member named as the
/// identity at any depth (`{"book": {"id": ..}}`, or `@id` for an XML attribute), are
/// its request for an id, and left out.
fn request_values(op: &Operation, call: &OperationCall) -> HashSet<String> {
    fn strings(value: &Value, field: &str, depth: usize, out: &mut HashSet<String>) {
        match value {
            Value::String(s) if !s.is_empty() => {
                out.insert(s.clone());
            }
            Value::Array(items) if depth < 8 => {
                for item in items {
                    strings(item, field, depth + 1, out);
                }
            }
            Value::Object(members) if depth < 8 => {
                // `@id` is how an XML attribute `id` reads (see the SOAP mapper).
                let own = |name: &str| name.strip_prefix('@').unwrap_or(name) == field;
                for (_, member) in members.iter().filter(|(name, _)| !own(name)) {
                    strings(member, field, depth + 1, out);
                }
            }
            _ => {}
        }
    }
    let identity = op.identity.as_ref();
    let own_param = identity
        .and_then(|i| i.path_param.as_deref())
        .filter(|_| matches!(op.kind, OperationKind::Command(Effect::Create)));
    let mut out: HashSet<String> = call
        .path_params
        .iter()
        .filter(|(name, _)| Some(name.as_str()) != own_param)
        .filter_map(|(_, v)| id_string(v))
        .collect();
    let field = identity.map_or("id", |i| i.field.as_str());
    if let Some(body) = &call.body {
        strings(body, field, 0, &mut out);
    }
    out
}

/// What the session holds under the id of a create's answer when that id is the one the
/// request asked for by its body's identity field ([`mb_engine::guards::asked_identity`],
/// a member the request declares): the record, when it is in the collection the create
/// runs in ([`in_create_collection`]), so that the create repeats it; else that a record
/// elsewhere holds the id. Storing the answer would replace that record either way, which
/// no create does. A create
/// sent with a declared idempotency key may be a retry the API answers as the first, and
/// an HTTP `PUT` is an upsert by its method: neither is one. (A create naming its id in
/// its path gets a fresh one drawn first, see [`unique_create_id`].)
async fn repeated_record(
    state: &mut StateAccess<'_>,
    (session, op, resource_type, call): (&SessionKey, &Operation, &str, &OperationCall),
    generated: &GeneratedResponse,
) -> Option<Repeated> {
    let rt = state.rt;
    if !matches!(op.kind, OperationKind::Command(Effect::Create))
        || matches!(
            op.binding,
            Binding::Http {
                method: mb_mcm::HttpMethod::Put,
                ..
            }
        )
        || mb_engine::guards::carries_an_idempotency_key(rt.bundle(), op, call)
    {
        return None;
    }
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let id = generated.body.as_ref()?.get(field).and_then(id_string)?;
    if mb_engine::guards::asked_identity(rt.bundle(), op, call).as_deref() != Some(id.as_str()) {
        return None;
    }
    let held = state.get(session, resource_type, &id).await?;
    Some(if in_create_collection(rt, session, op, call, &held) {
        Repeated::Here(held)
    } else {
        Repeated::Elsewhere(id)
    })
}

/// What [`repeated_record`] found under the id a create asked for.
enum Repeated {
    /// The record, in the collection the create runs in: the create repeats it.
    Here(StoredResource),
    /// A record under that id the create does not repeat, in another collection (or
    /// placed nowhere known, of a type also created under a parent): the create is new
    /// there, but storing it would replace that record, so it is served unstored.
    Elsewhere(String),
}

/// Drops every effect of `generated`, a create's answer that is not stored as it
/// repeats the record held under `id` (see [`repeated_record`]), and says so in its
/// outcomes: a duplicate changes nothing.
fn drop_all_effects(rt: &MockRuntime, op: &Operation, id: &str, generated: &mut GeneratedResponse) {
    const REASON: &str = "the create repeats a stored record's id: nothing is stored";
    let dropped = std::mem::take(&mut generated.effects);
    generated.dropped_effects += dropped.len();
    generated
        .effect_outcomes
        .extend(dropped.into_iter().map(|effect| EffectOutcome {
            effect,
            rejected: Some(REASON.to_string()),
            merged: None,
            applied: Vec::new(),
        }));
    tracing::info!(
        mock = %rt.name,
        operation = %op.id,
        id = %log_capped(id),
        "create not stored: its id is a stored record's"
    );
}

/// Whether `set`, what an effect sets a member to, repeats `asked`, what the request
/// gives that member: equal (numbers by value), or, for objects, holding at any depth a
/// value the request holds at the same place (`{"parent": {"id": "tr_1"}, "note": ..}`).
fn copies_value(set: &Value, asked: &Value, depth: usize) -> bool {
    match (set, asked) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Object(set), Value::Object(asked)) if depth < 8 => set
            .iter()
            .any(|(k, v)| asked.get(k).is_some_and(|a| copies_value(v, a, depth + 1))),
        _ => set == asked,
    }
}

/// Splits `effects`, those of a create of `resource_type` whose answer's record is
/// `created`, into what may apply and what is rejected: on another record of its own
/// type, every member whose value copies what the request gives that member (see
/// [`copies_value`]) is taken out, as the request copied onto a sibling (Mollie's second
/// payment setting the first one's amount, a request's `parent_id` making its parent its
/// own parent), whatever the value: the sibling's own id is copied as much as any. The
/// rest of the effect applies (a reply's count on its thread, clearing `is_default` on
/// the old default); an effect with nothing left is rejected whole. Effects on the created record and on other
/// types are left as they are. Each rejected part is returned as an effect of the
/// members taken out.
fn sibling_effects(
    resource_type: &str,
    created: Option<&str>,
    body: Option<&Value>,
    effects: Vec<ResourceEffect>,
) -> (Vec<ResourceEffect>, Vec<ResourceEffect>) {
    let empty = Map::new();
    let body = match body {
        Some(Value::Object(members)) => members,
        _ => &empty,
    };
    let mut kept = Vec::with_capacity(effects.len());
    let mut dropped = Vec::new();
    for mut e in effects {
        if e.resource_type != resource_type || created == Some(e.id.as_str()) {
            kept.push(e);
            continue;
        }
        let (copied, rest): (Map<String, Value>, Map<String, Value>) = std::mem::take(&mut e.set)
            .into_iter()
            .partition(|(k, v)| body.get(k).is_some_and(|asked| copies_value(v, asked, 0)));
        if !copied.is_empty() {
            dropped.push(ResourceEffect {
                set: copied,
                ..e.clone()
            });
        }
        if !rest.is_empty() {
            kept.push(ResourceEffect { set: rest, ..e });
        }
    }
    (kept, dropped)
}

/// Gives a create's answer a fresh identity when its generated one is taken (see
/// [`taken_id`]).
async fn unique_create_id(
    rt: &MockRuntime,
    state: &mut StateAccess<'_>,
    (session, op, resource_type, call): (&SessionKey, &Operation, &str, &OperationCall),
    generated: &mut GeneratedResponse,
    answer: Option<&Value>,
) {
    if taken_id(state, session, op, resource_type, call, generated)
        .await
        .is_none()
    {
        return;
    }
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let Some(body) = generated.body.as_ref() else {
        return;
    };
    let carried = request_values(op, call);
    for _ in 0..3 {
        let Some(fresh) = fresh_record(rt, op, generated.status, (body, answer), 0) else {
            return;
        };
        let Some(fresh_id) = fresh.get(field).and_then(id_string) else {
            return;
        };
        if !carried.contains(&fresh_id)
            && state.get(session, resource_type, &fresh_id).await.is_none()
        {
            generated.body = Some(fresh);
            return;
        }
    }
}

/// Keeps an action's new record off a stored one: an answer whose id is taken
/// takes a fresh one, each retry drawing anew. With none to be had it is not
/// stored, which the request record says, and its effects are dropped. So is an
/// effect naming the id it moved from, which meant the answer's own record.
/// Returns whether the answer can be stored.
async fn place_new_record(
    rt: &MockRuntime,
    state: &mut StateAccess<'_>,
    (session, op, resource_type, call): (&SessionKey, &Operation, &str, &OperationCall),
    generated: &mut GeneratedResponse,
    answer: Option<&Value>,
) -> bool {
    let Some(id) = taken_id(state, session, op, resource_type, call, generated).await else {
        return true;
    };
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let mut fresh = None;
    let carried = request_values(op, call);
    if let Some(body) = generated.body.as_ref() {
        for attempt in 0..3 {
            let draw = fresh_record(rt, op, generated.status, (body, answer), attempt);
            let drawn = draw.as_ref().and_then(|d| d.get(field)).and_then(id_string);
            let Some(drawn) = drawn else {
                break;
            };
            if !carried.contains(&drawn)
                && state.get(session, resource_type, &drawn).await.is_none()
            {
                fresh = draw;
                break;
            }
        }
    }
    let stored = fresh.is_some();
    if let Some(fresh) = fresh {
        generated.body = Some(fresh);
    }
    let (dropped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut generated.effects)
        .into_iter()
        .partition(|e| !stored || (e.resource_type == resource_type && e.id == id));
    generated.effects = kept;
    generated.dropped_effects += dropped.len();
    let rejected = |effect, reason: &str| EffectOutcome {
        effect,
        rejected: Some(reason.to_string()),
        merged: None,
        applied: Vec::new(),
    };
    let reason = if stored {
        "the answer moved off this id"
    } else {
        "the answer was not stored"
    };
    generated
        .effect_outcomes
        .extend(dropped.into_iter().map(|effect| rejected(effect, reason)));
    if !stored {
        tracing::warn!(
            mock = %rt.name,
            operation = %op.id,
            id = %log_capped(&id),
            "new record not stored: its id is taken and no other could be drawn"
        );
        let note = ResourceEffect {
            resource_type: resource_type.to_string(),
            id,
            set: serde_json::Map::new(),
        };
        generated.effect_outcomes.push(rejected(
            note,
            "new record not stored: its id is taken and no other could be drawn",
        ));
    }
    stored
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
    /// Per resource type: the fields its operations identify it by, those
    /// [`BundleIndex::path_fields`] holds included, sorted and deduplicated.
    identity_fields: HashMap<String, Vec<String>>,
    /// The members every query operation's success schema lists.
    listed: HashSet<String>,
    /// Per operation, by position: the members its success schema lists.
    list_members: Vec<Vec<String>>,
    /// The resource type each `$defs` name is, from the query operations
    /// whose success schema or list items are that `$ref`: a read by path
    /// parameter first (`/repos/{owner}/{repo}`, not `/search/repositories`,
    /// names GitHub's `repository`), then a list, then any other read. A
    /// list under a member naming a type read by path is that type.
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
    /// The types an effect can name, those with a read to check against, sorted.
    effect_types: Vec<String>,
    /// Per path with no parameters whose `GET` answers a single object (see
    /// [`answers_a_list`]), that operation's position.
    single_reads: HashMap<String, usize>,
    /// Per path a command and a list read by no path parameter share, that list.
    written_lists: HashMap<String, WrittenList>,
    /// Per type of items, how many list reads of it the contract has whose path has no
    /// parameter (`/tasks`, `/me/tasks`, `/tasks.list`), by any method: the type their
    /// `$defs` entry is, else the operation's, as [`BundleIndex::item_type`] finds it.
    unscoped_lists: HashMap<String, usize>,
    /// Per path of such a list read, the type of its items, so counted.
    unscoped_paths: HashMap<String, String>,
    /// Whether a create or an action may mint a value for a client to send back (see
    /// [`minted::may_mint`]).
    mints: bool,
    /// Per read below a record's read (`GET /playlists/{id}/items` below
    /// `/playlists/{id}`), by position, when it answers exactly the schema of one of
    /// that record's members (`tracks`): where to find it (see [`member_read`]).
    member_reads: HashMap<usize, MemberRead>,
    /// Per resource type whose read by path answers its record in an envelope or an
    /// array, the record's own schema: what a stored record of the type must suit.
    record_schemas: HashMap<String, Value>,
    /// Per resource type, the string members of its items that a lookup keys on: a
    /// list read whose required query parameter names the member (see
    /// [`lists::filtered_member`]), sorted. Auth0's `email`, which
    /// `/users-by-email?email=` requires; never an optional filter such as `state`.
    lookup_members: HashMap<String, Vec<String>>,
}

/// The schema of the record an answer of `schema` holds where `identity` says: an
/// envelope's member, or an array's items. `None` for an answer that is its record.
fn record_schema_of(
    bundle: &mb_mcm::Bundle,
    identity: &mb_mcm::Identity,
    schema: &Value,
) -> Option<Value> {
    match identity.record.as_ref()? {
        mb_mcm::RecordAt::Member(member) => property_schema(bundle, schema, member),
        mb_mcm::RecordAt::Item => {
            let (parts, _) = schema_parts(bundle, Some(schema));
            parts.iter().find_map(|p| p.get("items")).cloned()
        }
    }
}

/// The [`BundleIndex::record_schemas`] of `bundle`, from the reads by path chosen.
fn record_schemas_of(
    bundle: &mb_mcm::Bundle,
    detail_reads: &HashMap<String, usize>,
) -> HashMap<String, Value> {
    detail_reads
        .iter()
        .filter_map(|(t, i)| {
            let op = bundle.operations.get(*i)?;
            let schema = op.success_output()?.schema.as_ref()?;
            let record = record_schema_of(bundle, op.identity.as_ref()?, schema)?;
            Some((t.clone(), record))
        })
        .collect()
}

/// Where a read of a member of its parent finds it.
#[derive(Debug, Clone)]
struct MemberRead {
    /// The position of the parent's read.
    parent: usize,
    /// The parent's member it answers.
    member: String,
    /// The path parameter of this read that holds the parent's id.
    param: String,
}

/// The [`BundleIndex::member_reads`] of `bundle`.
fn member_reads_of(bundle: &mb_mcm::Bundle) -> HashMap<usize, MemberRead> {
    fn get(op: &Operation) -> Option<Vec<&str>> {
        match &op.binding {
            Binding::Http { method, path }
                if *method == mb_mcm::HttpMethod::Get
                    && matches!(op.kind, OperationKind::Query) =>
            {
                Some(path_segments(path))
            }
            _ => None,
        }
    }
    // A route, whatever it names its parameters.
    let route = |segments: &[&str]| {
        let shape: Vec<&str> = segments
            .iter()
            .map(|s| if segment_param(s).is_some() { "{}" } else { s })
            .collect();
        shape.join("/")
    };
    let mut records: HashMap<String, usize> = HashMap::new();
    for (i, op) in bundle.operations.iter().enumerate() {
        let by_path = op.identity.as_ref().is_some_and(|i| {
            i.path_param.is_some() && i.path_field.is_none() && i.record.is_none()
        });
        if let (Some(segments), true) = (get(op), by_path)
            && segments.last().is_some_and(|s| segment_param(s).is_some())
        {
            records.entry(route(&segments)).or_insert(i);
        }
    }
    let mut reads = HashMap::new();
    if records.is_empty() {
        return reads;
    }
    for (i, op) in bundle.operations.iter().enumerate() {
        let (Some(segments), None, Some(schema)) = (
            get(op),
            op.identity.as_ref(),
            op.success_output().and_then(|o| o.schema.as_ref()),
        ) else {
            continue;
        };
        let [.., param, last] = segments.as_slice() else {
            continue;
        };
        let (Some(param), None) = (segment_param(param), segment_param(last)) else {
            continue;
        };
        let parent_route = route(&segments[..segments.len() - 1]);
        let Some((parent, read)) = records
            .get(&parent_route)
            .and_then(|&p| Some((p, bundle.operations.get(p)?)))
        else {
            continue;
        };
        let parent_schema = read.success_output().and_then(|o| o.schema.as_ref());
        let (parts, _) = schema_parts(bundle, parent_schema);
        let mut members = parts
            .iter()
            .filter_map(|part| part.get("properties").and_then(Value::as_object))
            .flatten()
            .filter(|(_, member)| *member == schema)
            .map(|(name, _)| name.clone());
        let (Some(member), None) = (members.next(), members.next()) else {
            continue;
        };
        let read = MemberRead {
            parent,
            member,
            param: param.to_string(),
        };
        reads.insert(i, read);
    }
    reads
}

/// A list read whose path also takes writes.
#[derive(Debug, Clone)]
struct WrittenList {
    resource_type: String,
    /// The schema of its items: all of a write's body that it is told.
    items: Value,
}

impl BundleIndex {
    pub(crate) fn new(bundle: &mb_mcm::Bundle) -> Self {
        let mut by_id = HashMap::with_capacity(bundle.operations.len());
        let mut path_fields: HashMap<String, Vec<String>> = HashMap::new();
        let mut identity_fields: HashMap<String, Vec<String>> = HashMap::new();
        let mut list_members_of = Vec::with_capacity(bundle.operations.len());
        let mut listed = HashSet::new();
        let mut read_types = HashSet::new();
        let mut detail_reads = HashMap::new();
        let mut ranked_defs = RankedDefs::new();
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
            if let (Some(t), Some(identity)) = (op.resource_type.as_deref(), op.identity.as_ref()) {
                let fields = identity_fields.entry(t.to_string()).or_default();
                fields.push(identity.field.clone());
                fields.extend(identity.path_field.iter().cloned());
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
                    // An answer with an identity of its own is one record: the lists it
                    // holds are not of its type (a weather reading's conditions).
                    let own = op.identity.as_ref().is_some_and(|i| i.record.is_none());
                    rank_defs(op, (t, own), by_path, &lists, &mut ranked_defs);
                    record_shapes(op, i, (t, own), by_path, &members, &lists, &mut shapes);
                }
            }
            list_members_of.push(members);
        }
        for fields in path_fields
            .values_mut()
            .chain(identity_fields.values_mut())
            .chain(referrers.values_mut())
        {
            fields.sort_unstable();
            fields.dedup();
        }
        // Lists under a member of a type read by path hold that type: the
        // `offers` of a flight search are offers.
        let by_path_types: HashSet<String> = detail_reads.keys().cloned().collect();
        let in_detail_reads = |name: &String| by_path_types.contains(name);
        let (single_reads, written_lists, unscoped_lists) =
            routes_of(bundle, &read_types, &list_members_of);
        let record_schemas = record_schemas_of(bundle, &detail_reads);
        let mut effect_types: Vec<String> =
            detail_reads.keys().chain(shapes.keys()).cloned().collect();
        effect_types.sort_unstable();
        effect_types.dedup();
        #[cfg(any(test, feature = "events"))]
        let mut channel_by_id = HashMap::with_capacity(bundle.channels.len());
        #[cfg(any(test, feature = "events"))]
        for (i, channel) in bundle.channels.iter().enumerate() {
            channel_by_id.entry(channel.id.clone()).or_insert(i);
        }
        let types_by_def: HashMap<String, String> = ranked_defs
            .into_iter()
            .map(|(def, (_, t, member))| {
                let t = member
                    .and_then(|m| [format!("{m}s"), m].into_iter().find(in_detail_reads))
                    .unwrap_or(t);
                (def, t)
            })
            .collect();
        let lookup_members = lookup_members(bundle, &types_by_def, &list_members_of);
        // Each list counts under its items' type, as [`BundleIndex::item_type`] finds it.
        let mut unscoped_counts: HashMap<String, usize> = HashMap::new();
        let mut unscoped_paths: HashMap<String, String> = HashMap::new();
        for list in unscoped_lists {
            let t = list
                .def
                .as_deref()
                .and_then(|d| types_by_def.get(d).cloned())
                .unwrap_or(list.resource_type);
            *unscoped_counts.entry(t.clone()).or_default() += 1;
            unscoped_paths.insert(list.path, t);
        }
        Self {
            by_id,
            #[cfg(any(test, feature = "events"))]
            channel_by_id,
            path_fields,
            identity_fields,
            listed,
            list_members: list_members_of,
            types_by_def,
            lookup_members,
            read_types,
            detail_reads,
            other_shapes: shapes
                .into_iter()
                .map(|(t, (_, i, schema))| (t, (i, schema)))
                .collect(),
            referrers,
            effect_types,
            single_reads,
            written_lists,
            unscoped_lists: unscoped_counts,
            unscoped_paths,
            mints: bundle
                .operations
                .iter()
                .any(|op| minted::may_mint(bundle, op)),
            member_reads: member_reads_of(bundle),
            record_schemas,
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

    fn identity_fields(&self, resource_type: &str) -> &[String] {
        self.identity_fields
            .get(resource_type)
            .map_or(&[], Vec::as_slice)
    }

    fn list_members(&self, op: usize) -> &[String] {
        self.list_members.get(op).map_or(&[], Vec::as_slice)
    }

    /// Whether `path`, with no parameter, is the one list read of `resource_type` with
    /// no path parameter the contract has, and a command of the type shares its path:
    /// the type's records are all that list's (`POST /tasks` beside `GET /tasks`).
    fn sole_written_list(&self, path: &str, resource_type: &str) -> bool {
        !path.contains('{')
            && self.unscoped_lists.get(resource_type) == Some(&1)
            && self.unscoped_paths.get(path).map(String::as_str) == Some(resource_type)
            && self.written_lists.contains_key(path)
    }

    fn referrers(&self, resource_type: &str) -> &[String] {
        self.referrers.get(resource_type).map_or(&[], Vec::as_slice)
    }

    fn lookup_members(&self, resource_type: &str) -> &[String] {
        self.lookup_members
            .get(resource_type)
            .map_or(&[], Vec::as_slice)
    }

    /// The resource type of the items of `op`'s list answer `body`: the one their
    /// `$defs` entry is, else `op`'s.
    fn item_type(&self, bundle: &mb_mcm::Bundle, op: &Operation, body: &Value) -> Option<String> {
        let schema = op.success_output().and_then(|o| o.schema.as_ref());
        let items = schema.and_then(|schema| item_schema(bundle, schema, body));
        items
            .as_ref()
            .and_then(def_name)
            .and_then(|d| self.types_by_def.get(d).cloned())
            .or_else(|| op.resource_type.clone())
    }
}

/// Per resource type, the string members of its items a list read's required query
/// parameter keys on (see [`BundleIndex::lookup_members`]).
fn lookup_members(
    bundle: &mb_mcm::Bundle,
    types_by_def: &HashMap<String, String>,
    members_of: &[Vec<String>],
) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (i, op) in bundle.operations.iter().enumerate() {
        let query = op.input.query.as_ref();
        let params = query
            .and_then(|q| q.get("properties"))
            .and_then(Value::as_object);
        let (true, Some(params)) = (matches!(op.kind, OperationKind::Query), params) else {
            continue;
        };
        let members = members_of.get(i).map_or(&[][..], Vec::as_slice);
        for (_, items) in list_item_schemas(bundle, op, members) {
            let t = def_name(&items)
                .and_then(|d| types_by_def.get(d).cloned())
                .or_else(|| op.resource_type.clone());
            let Some(t) = t else { continue };
            let (item_parts, _) = schema_parts(bundle, Some(&items));
            // Only a parameter the route requires keys a lookup; an optional one
            // (`state`, `status`) narrows a list of many.
            let required = query
                .and_then(|q| q.get("required"))
                .and_then(Value::as_array);
            let keys = params.keys().filter(|param| {
                required.is_some_and(|names| names.iter().any(|n| n.as_str() == Some(param)))
            });
            for param in keys {
                if let Some(member) = lists::filtered_member(bundle, query, &item_parts, param)
                    && member.is_text()
                {
                    out.entry(t.clone())
                        .or_default()
                        .push(member.name.to_string());
                }
            }
        }
    }
    for fields in out.values_mut() {
        fields.sort_unstable();
        fields.dedup();
    }
    out
}

/// A list read whose path has no parameter, by any method: its path, the `$defs` name
/// of its items, when they are one, and its operation's resource type.
struct UnscopedList {
    path: String,
    def: Option<String>,
    resource_type: String,
}

/// What routes the contract's operations share: see [`BundleIndex::single_reads`],
/// [`BundleIndex::written_lists`] and [`BundleIndex::unscoped_lists`].
fn routes_of(
    bundle: &mb_mcm::Bundle,
    read_types: &HashSet<String>,
    members_of: &[Vec<String>],
) -> (
    HashMap<String, usize>,
    HashMap<String, WrittenList>,
    Vec<UnscopedList>,
) {
    let (mut single_reads, mut written_lists) = (HashMap::new(), HashMap::new());
    let mut unscoped_lists: Vec<UnscopedList> = Vec::new();
    let mut lists: HashMap<&str, WrittenList> = HashMap::new();
    for (i, op) in bundle.operations.iter().enumerate() {
        let Binding::Http { method, path } = &op.binding else {
            continue;
        };
        if !matches!(op.kind, OperationKind::Query) {
            continue;
        }
        let members = members_of.get(i).map_or(&[][..], Vec::as_slice);
        let items = listed_items(bundle, op, read_types, members);
        if let (Some(items), Some(t)) = (&items, op.resource_type.as_deref())
            && !path.contains('{')
        {
            unscoped_lists.push(UnscopedList {
                path: path.clone(),
                def: def_name(items).map(str::to_string),
                resource_type: t.to_string(),
            });
        }
        if *method != mb_mcm::HttpMethod::Get {
            continue;
        }
        if let (Some(items), Some(t), true) = (
            &items,
            op.resource_type.as_deref(),
            op.identity.as_ref().is_none_or(|i| i.path_param.is_none()),
        ) {
            let list = WrittenList {
                resource_type: t.to_string(),
                items: items.clone(),
            };
            lists.insert(path.as_str(), list);
        }
        let parameters = path.contains('{') || op.input.path.is_some() || op.input.query.is_some();
        if items.is_none() && !parameters && op.success_output().is_some_and(|o| o.schema.is_some())
        {
            single_reads.insert(path.clone(), i);
        }
    }
    for op in &bundle.operations {
        if let (OperationKind::Command(_), Binding::Http { path, .. }) = (&op.kind, &op.binding)
            && let Some(list) = lists.get(path.as_str())
        {
            written_lists.insert(path.clone(), list.clone());
        }
    }
    (single_reads, written_lists, unscoped_lists)
}

/// The item schema of `op`'s success answer when it is a list: an array, or an object
/// holding one in an envelope's member or under a member that is itself a type read. An
/// object that only has a list among its members (a profile's `images`) is a single object.
fn listed_items(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    read_types: &HashSet<String>,
    members: &[String],
) -> Option<Value> {
    list_item_schemas(bundle, op, members)
        .into_iter()
        .find(|(member, _)| {
            member.as_deref().is_none_or(|m| {
                ENVELOPE_KEYS.contains(&m)
                    || op.resource_type.as_deref() == Some(m)
                    || read_types.contains(m)
                    || read_types.contains(&format!("{m}s"))
            })
        })
        .map(|(_, items)| items)
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

/// Per `$defs` name: the best rank seen, the type of the read it was seen in
/// and, for a list, the non-envelope member holding it.
type RankedDefs = HashMap<String, (u8, String, Option<String>)>;

/// Records in `ranked` the `$defs` names `op` reads as resource type `t`,
/// keeping the best-ranked type of each (see [`BundleIndex::types_by_def`]).
fn rank_defs(
    op: &Operation,
    (t, own): (&str, bool),
    by_path: bool,
    lists: &[(Option<String>, Value)],
    ranked: &mut RankedDefs,
) {
    const DETAIL: u8 = 0;
    const LIST: u8 = 1;
    const OTHER: u8 = 2;
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let mut found: Vec<(u8, String, Option<String>)> = Vec::new();
    if let Some(def) = def_name(schema) {
        found.push((if by_path { DETAIL } else { OTHER }, def.to_string(), None));
    }
    let items = lists.iter().filter_map(|(member, items)| {
        let member = member
            .as_ref()
            .filter(|m| !ENVELOPE_KEYS.contains(&m.as_str()));
        Some((LIST, def_name(items)?.to_string(), member.cloned()))
    });
    found.extend(items);
    for (rank, def, member) in found {
        // A list held by one record says nothing of what its items are: they are of
        // a type only where another read serves exactly their schema.
        if own && rank == LIST {
            continue;
        }
        let held = ranked
            .entry(def)
            .or_insert_with(|| (rank, t.to_string(), member.clone()));
        if rank < held.0 {
            *held = (rank, t.to_string(), member);
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
    (t, own): (&str, bool),
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
            // What a record holds is no shape of a type, its own or one named for
            // the member.
            Some(_) if own => {}
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
    if (lists.is_empty() || own)
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

/// Sessions [`WrittenTypes`] holds at most; it is emptied when full.
const MAX_WRITTEN_SESSIONS: usize = 1024;
/// Types [`WrittenTypes`] holds per session at most, the latest.
const MAX_WRITTEN_TYPES: usize = 32;

/// Which types each session last wrote through this runtime, latest first. Only an
/// order of looking (see [`world::named_by_value`]): a contract can have more types than
/// a call may read, and what the session wrote is what it refers to. A session written
/// to elsewhere (the Admin API, another node) is looked up in the contract's order.
#[derive(Debug, Default)]
pub(crate) struct WrittenTypes(std::sync::Mutex<HashMap<SessionKey, Vec<String>>>);

impl WrittenTypes {
    fn remember(&self, session: &SessionKey, t: &str) {
        let Ok(mut written) = self.0.lock() else {
            return;
        };
        if written.len() >= MAX_WRITTEN_SESSIONS && !written.contains_key(session) {
            written.clear();
        }
        let types = written.entry(session.clone()).or_default();
        if types.first().is_some_and(|first| first == t) {
            return;
        }
        types.retain(|held| held != t);
        types.insert(0, t.to_string());
        types.truncate(MAX_WRITTEN_TYPES);
    }

    fn of(&self, session: &SessionKey) -> Vec<String> {
        self.0
            .lock()
            .ok()
            .and_then(|written| written.get(session).cloned())
            .unwrap_or_default()
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
/// The member `stored`, the record the operation's identity is of, was stored
/// with the create.
async fn store_wrapped(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (resource_type, stored): (Option<&str>, Option<&str>),
    generated: &GeneratedResponse,
    schema: Option<&Value>,
    collection: Option<&Collection>,
) {
    let Some(Value::Object(body)) = generated.body.as_ref() else {
        return;
    };
    let rt = state.rt;
    let answer = Place::of(rt, schema);
    let listed = &rt.index().listed;
    for (member, value) in body {
        let plural = format!("{member}s");
        if !value.is_object() || !listed.contains(&plural) || stored == Some(member.as_str()) {
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
            let stored = resource.clone();
            let took = state.upsert(session, resource).await;
            if took && state.tracks_deleted {
                let scope = collection.map(|c| c.path.as_str());
                deleted::clear(state, session, &stored.resource_type, scope, &stored).await;
            }
            if took && let Some(collection) = collection {
                rt.collections()
                    .record(session, &stored, collection.clone(), true);
            }
        } else if rt.consistency
            && !transient_fallback(generated)
            && entity_fits_from(
                rt,
                &resource.resource_type,
                &resource.data,
                // The member's own inline schema says what it is too.
                answer
                    .property(member)
                    .filter(|s| Place::of(rt, Some(s)).defs.is_empty()),
            )
        {
            if unkeyed {
                let gone = state.tracks_deleted
                    && deleted::mention_is_deleted(
                        state,
                        session,
                        &resource.resource_type,
                        &resource.id,
                    )
                    .await;
                if !gone {
                    state.upsert(session, resource).await;
                }
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
/// field goes under its own id, or under the parameter's value when that is
/// what its `id` holds. After an earlier parameter (`/repos/{o}/{r}`,
/// `/orgs/{o}/teams/{t}`) that value is no key, or one owner's answer would
/// answer for another's: such an answer stores nothing, the response cache
/// keeping its repeat. So does one naming its target after an owner that it
/// does not show (see [`shows_owner`]).
/// A route read by another member (`/issues/{number}`, `/teams/{team_slug}`) goes
/// under its answer's own `id` when that member holds the path's value, which
/// the engine sets there, and the answer shows the path's owner, if it has one.
async fn seed_own_read(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    op: &Operation,
    call: &OperationCall,
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
    let named = identity.path_member();
    let id = if named != identity.field {
        // Read by another member than its key (`/issues/{issue_iid}` by `iid`): the path's
        // value is in that member, and the record goes under its own key.
        let value = |member: &str| body.get(member).and_then(id_string);
        value(&identity.field).filter(|_| value(named).as_deref() == Some(target_id))
    } else if identity.path_field.is_some() {
        // A path that may hold the key or another member (Jira's `issueIdOrKey`) tells
        // neither apart, so a generated read stores nothing.
        return;
    } else if path_names_identity(identity) {
        Some(target_id.to_string())
    } else {
        body.get(&identity.field)
            .and_then(id_string)
            .filter(|id| id != target_id || !follows_a_param(op, identity))
    };
    let Some(id) = id else {
        return;
    };
    let resource = StoredResource {
        resource_type: t.to_string(),
        id,
        data: body.clone(),
    };
    if (named != identity.field || !path_names_identity(identity))
        && let Some(owner) = owner_of(op, call)
        && !shows_owner(rt, &resource, &owner)
    {
        return;
    }
    if state.insert_seeded(session, resource.clone()).await {
        rt.own_reads().remember(session, &resource);
        if let Some(collection) = collection_of(op, call) {
            rt.collections().remember(session, &resource, collection);
        }
        return;
    }
    // What is held is shorter than this read (see [`is_summary`]): this answer
    // was generated knowing it, and a later read can be served.
    if let Some(held) = state.get(session, t, &resource.id).await
        && !entity_fits(rt, t, &held.data)
        && entity_fits(rt, t, &resource.data)
        && state.upsert(session, resource.clone()).await
    {
        rt.own_reads().remember(session, &resource);
        if let Some(collection) = collection_of(op, call) {
            rt.collections().remember(session, &resource, collection);
        }
    }
}

/// How deep an answer is walked for entities: containers below its root.
const MAX_ENTITY_DEPTH: usize = 6;
/// Entities one answer may seed.
const MAX_ENTITIES: usize = 100;

/// Stores what a 2xx answer mentions so that a later read returns it: each
/// object below the answer's root with a known type (see
/// [`entity_type`]), keyed by the field its type's read by path goes by, else an
/// `id`, a `number` or another field that type is identified by,
/// and a value its type's read would serve or a shorter form
/// of one (see [`entity_fits`], [`is_summary`]). What the session already
/// holds is left as it is.
async fn seed_entities(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    body: &Value,
    schema: Option<&Value>,
    created: Option<(&str, &Collection)>,
) {
    let mut walk = EntityWalk {
        rt,
        considered: 0,
        found: Vec::new(),
    };
    walk.visit(body, schema, "", 0);
    for resource in walk.found {
        let stored = resource.clone();
        if state.insert_seeded(session, resource).await
            && let Some((_, collection)) = created.filter(|(t, _)| *t == stored.resource_type)
        {
            rt.collections()
                .remember(session, &stored, collection.clone());
        }
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
                    self.consider(value, schema, &defs, member);
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

    fn consider(&mut self, value: &Value, schema: Option<&Value>, defs: &[&str], member: &str) {
        let index = self.rt.index();
        let def = defs
            .iter()
            .copied()
            .find(|d| index.types_by_def.contains_key(*d));
        let Some(resource_type) = entity_type(index, def, member) else {
            return;
        };
        // As the type's routes key it, so a record is stored once, else as a list
        // with no identity of its type keys one.
        let own = index
            .detail_reads
            .get(&resource_type)
            .and_then(|i| self.rt.bundle().operations.get(*i)?.identity.as_ref())
            .map(|identity| identity.field.as_str());
        let identity = index.identity_fields(&resource_type).iter();
        let Some(id) = own
            .into_iter()
            .chain(["id", "number"])
            .chain(identity.map(String::as_str))
            .find_map(|f| value.get(f).and_then(id_string))
        else {
            return;
        };
        if self.considered >= MAX_ENTITIES {
            return;
        }
        self.considered += 1;
        // An inline object's own schema says what it is too; a `$ref` is the
        // definition's, which may be another type's.
        let came_from = schema.filter(|_| defs.is_empty());
        if entity_fits_from(self.rt, &resource_type, value, came_from)
            || is_summary(self.rt, &resource_type, defs, value)
        {
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
/// check against, so nothing suits it. An object must also show it is one: a
/// schema requiring nothing would take any object (an acknowledgement for the
/// settings it updated), so one of its members must be a member the type's
/// schema declares (see [`shows_its_type`]).
fn entity_fits(rt: &MockRuntime, t: &str, value: &Value) -> bool {
    entity_fits_from(rt, t, value, None)
}

/// [`entity_fits`] of `value` found at a place of schema `came_from` in an answer:
/// a member that place declares shows the type as well (Slack's `message`, an
/// inline object, of a `messages` type whose read is another API's).
fn entity_fits_from(rt: &MockRuntime, t: &str, value: &Value, came_from: Option<&Value>) -> bool {
    let index = rt.index();
    let bundle = rt.bundle();
    let (read, shape) = match index.detail_reads.get(t) {
        // A record its read answers in an envelope or an array suits its own schema.
        Some(i) => (Some(*i), index.record_schemas.get(t)),
        None => match index.other_shapes.get(t) {
            Some((i, schema)) => (Some(*i), Some(schema)),
            None => (None, None),
        },
    };
    let Some(op) = read.and_then(|i| bundle.operations.get(i)) else {
        return false;
    };
    let schema = shape.or_else(|| op.success_output()?.schema.as_ref());
    shows_its_type(rt, schema, came_from, value)
        && rt.engine.serve_stored(bundle, op, value, shape).is_some()
}

/// Whether `value` holds a member the type's `schema` declares, or one `came_from`
/// (the place it was found at) declares. True for anything but an object, and for a
/// type whose schema declares no member (a map, a free-form object, `properties: {}`):
/// there is nothing to tell by.
fn shows_its_type(
    rt: &MockRuntime,
    schema: Option<&Value>,
    came_from: Option<&Value>,
    value: &Value,
) -> bool {
    let Value::Object(members) = value else {
        return true;
    };
    let declared = |schema: Option<&Value>| -> Vec<String> {
        Place::of(rt, schema)
            .parts
            .iter()
            .filter_map(|p| p.get("properties").and_then(Value::as_object))
            .flat_map(|p| p.keys().cloned())
            .collect()
    };
    let own = declared(schema);
    if own.is_empty() {
        return true;
    }
    let found = came_from.map(|s| declared(Some(s))).unwrap_or_default();
    members.keys().any(|k| own.contains(k) || found.contains(k))
}

/// Whether `value`, shaped by `defs`, is a shorter form of resource type `t`:
/// valid as one of those definitions, which its by-path read's schema is
/// composed of (a search's `FlightOffer`, of `Offer`). It is stored though
/// that read would not serve it: the read is generated knowing it and keeps
/// what it holds, and the answer takes its place (see [`seed_own_read`]).
fn is_summary(rt: &MockRuntime, t: &str, defs: &[&str], value: &Value) -> bool {
    let bundle = rt.bundle();
    let Some(op) = rt
        .index()
        .detail_reads
        .get(t)
        .and_then(|i| bundle.operations.get(*i))
    else {
        return false;
    };
    let read = op.success_output().and_then(|o| o.schema.as_ref());
    let (_, read_defs) = schema_parts(bundle, read);
    let Some(def) = defs.iter().find(|d| read_defs.contains(d)) else {
        return false;
    };
    let shape = serde_json::json!({"$ref": format!("#/$defs/{def}")});
    rt.engine
        .serve_stored(bundle, op, value, Some(&shape))
        .is_some()
}

/// The response cache key of a read: its operation, path parameters,
/// query (sorted), a query operation's body, the narrowed selection, the
/// call's faker seed (`X-Mockingbird-Seed`) and, when it carries any, the
/// declared headers its prompt shows ([`mb_engine::prompt::shown_headers`]), so a
/// read in another language or at another version is not answered from this one.
fn cache_key(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    call: &OperationCall,
    response_schema: Option<&Value>,
) -> String {
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
    let mut key = serde_json::json!([
        op.id,
        canonical(&Value::Object(call.path_params.clone())),
        canonical(&Value::Object(call.query.clone())),
        body,
        selection,
        mb_engine::seed::request_seed(call),
    ]);
    // Only a read carrying such a header keys by it: other keys stay as they were.
    let headers = mb_engine::prompt::shown_headers(bundle, op, call);
    if !headers.is_empty()
        && let Some(parts) = key.as_array_mut()
    {
        parts.push(canonical(&Value::Object(headers)));
    }
    key.to_string()
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
    param == identity.field || identity.path_field.is_some() || id_like(param)
}

/// Whether a path parameter's name says it holds an id.
fn id_like(param: &str) -> bool {
    param.eq_ignore_ascii_case("id") || param.ends_with("Id") || param.ends_with("_id")
}

/// Whether another path parameter comes before the one naming `op`'s target:
/// that value then only means something under the earlier one.
fn follows_a_param(op: &Operation, identity: &mb_mcm::Identity) -> bool {
    let (Binding::Http { path, .. }, Some(param)) = (&op.binding, identity.path_param.as_deref())
    else {
        return false;
    };
    let own = format!("{{{param}}}");
    path.split('/')
        .take_while(|s| !s.contains(&own))
        .any(|s| s.contains('{'))
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
        let (target, mut related, followed) =
            read_context(&mut state, &plan.session, op, target_id.as_deref(), &call).await;
        follow_hops(
            &mut state,
            &plan.session,
            target.as_ref(),
            followed,
            &mut related,
        )
        .await;
        state.touch(&plan.session).await;
        (target, related)
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

/// Whether a header or member named `name` (any case) may carry credentials:
/// never one of Mockingbird's own overrides, whatever its name says
/// (`X-Mockingbird-Session`).
pub(crate) fn is_sensitive(name: &str) -> bool {
    !name.to_ascii_lowercase().starts_with("x-mockingbird-")
        && mb_mcm::credentials::is_credential_name(name)
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
        && !rt.bundle.declares_api_key(name)
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

/// Headers kept in the request record (overrides included): never one any API-key
/// scheme of the contract names ([`mb_mcm::Bundle::declares_api_key`]), whatever the
/// data-plane auth mode, as the record is stored and exported.
pub(crate) fn recorded_headers(rt: &MockRuntime, headers: &HeaderMap) -> Map<String, Value> {
    let mut out = safe_headers(rt, headers, true);
    out.retain(|name, _| !rt.bundle.declares_api_key(name));
    out
}

/// [`public_query`] as the request record keeps it: also without a parameter any
/// API-key scheme of the contract names, whatever the data-plane auth mode. The call
/// itself keeps those, for its validation.
#[cfg(feature = "http-rest")]
pub(crate) fn recorded_public_query(
    rt: &MockRuntime,
    pairs: &[(String, String)],
) -> Vec<(String, String)> {
    public_query(rt, pairs)
        .into_iter()
        .filter(|(k, _)| !rt.bundle.declares_api_key(k))
        .collect()
}

/// The target (when `target_id` is set), the related resources before any
/// hop, and what [`follow_hops`] needs to add those.
async fn read_context(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    target_id: Option<&str>,
    call: &OperationCall,
) -> (Option<StoredResource>, Vec<StoredResource>, Followed) {
    let mut owner = guarded(state.rt, op, call);
    if let Some(owner) = &mut owner {
        owner.writing = !matches!(op.kind, OperationKind::Query);
    }
    let owner = owner.as_ref();
    let rt = state.rt;
    let target = match (op.resource_type.as_deref(), target_id) {
        (Some(t), Some(id)) => match state
            .get(session, t, id)
            .await
            .filter(|hit| owner.is_none_or(|o| key_fits(rt, session, hit, o)))
            // Where the path names another member than the key (`/issues/{number}`),
            // a record keyed by the path's value is another one unless that member
            // says so too.
            .filter(|hit| {
                op.identity.as_ref().is_none_or(|identity| {
                    let named = identity.path_member();
                    named == identity.field
                        || hit
                            .data
                            .get(named)
                            .and_then(id_string)
                            .is_none_or(|value| value == id)
                })
            }) {
            Some(hit) => Some(hit),
            None => {
                let delete = matches!(op.kind, OperationKind::Command(Effect::Delete));
                let identity = op.identity.as_ref();
                let param = target_param(op);
                let param_is_id = param.as_deref().is_some_and(id_like)
                    || identity.is_some_and(path_names_identity);
                // A delete may not declare what its route's read goes by.
                let read_fields: Vec<&str> = rt
                    .index()
                    .path_fields(t)
                    .iter()
                    .filter(|_| delete && rt.consistency && !param_is_id)
                    .map(String::as_str)
                    .collect();
                match by_path_fields(state, session, t, &read_fields, id, owner).await {
                    Some(hit) => Some(hit),
                    None => match identity.and_then(|i| i.path_field.as_deref()) {
                        Some(field) => by_path_fields(state, session, t, &[field], id, owner).await,
                        None if rt.consistency
                            && param.is_some()
                            && (delete || identity.is_some())
                            && identity.is_none_or(|i| !path_names_identity(i)) =>
                        {
                            let names = name_fields(param.as_deref().unwrap_or_default());
                            by_path_fields(state, session, t, &names, id, owner).await
                        }
                        None => None,
                    },
                }
            }
        },
        _ => None,
    };
    let (related, followed) = related(state, session, op, target_id, target.as_ref(), call).await;
    (target, related, followed)
}

/// The value `data` holds for each of `fields`, as (field, value).
fn path_values<'f>(fields: &'f [String], data: &Value) -> Vec<(&'f str, String)> {
    fields
        .iter()
        .filter_map(|f| Some((f.as_str(), data.get(f).and_then(id_string)?)))
        .collect()
}

/// Fields a path parameter that names no field may mean: `/users/{username}`
/// finds a user seeded under its numeric id by its `login`. A last resort, when
/// no member is named for the parameter (see [`name_fields`]).
const NAME_FIELDS: [&str; 4] = ["name", "login", "slug", "key"];

/// Whose a path parameter's value is, as the path says: `/orgs/{org}/teams/`
/// `{slug}` has `{org}` before `{slug}`, and the path up to it.
struct Owner {
    /// The path of the collection the parameter names a member of.
    collection: Collection,
    /// The name of the parameter nearest before it, and that parameter's value.
    param: String,
    value: String,
    /// The value of the parameter itself.
    name: String,
    /// The path from `value` to `name`: `octo/teams/core`.
    tail: String,
    /// For a write, which acts unless the resource is known to be another's.
    writing: bool,
}

/// A collection a path names: `/orgs/octo/teams` of `/orgs/{}/teams`.
#[derive(Debug, Clone, PartialEq)]
struct Collection {
    template: String,
    path: String,
}

/// `path` by segment, a trailing slash aside.
fn path_segments(path: &str) -> Vec<&str> {
    path.trim_end_matches('/').split('/').collect()
}

/// The parameter a path segment wholly is.
fn segment_param(segment: &str) -> Option<&str> {
    segment.strip_prefix('{')?.strip_suffix('}')
}

/// `segments` as a [`Collection`], when `call` gives each parameter a value.
fn fill_path(call: &OperationCall, segments: &[&str]) -> Option<Collection> {
    let mut filled = Vec::with_capacity(segments.len());
    let mut template = Vec::with_capacity(segments.len());
    for segment in segments {
        match segment_param(segment) {
            Some(param) => {
                filled.push(call.path_params.get(param).and_then(id_string)?);
                template.push("{}");
            }
            None => {
                filled.push((*segment).to_string());
                template.push(segment);
            }
        }
    }
    Some(Collection {
        template: template.join("/"),
        path: filled.join("/"),
    })
}

/// The [`Owner`] of path parameter `name`, when a parameter comes before it.
fn owner_of_param(op: &Operation, call: &OperationCall, name: &str) -> Option<Owner> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let param = format!("{{{name}}}");
    let segments = path_segments(path);
    let at = segments.iter().position(|s| *s == param)?;
    let before = segments[..at]
        .iter()
        .rposition(|s| segment_param(s).is_some())?;
    let owner = segment_param(segments[before])?;
    let value = |p: &str| call.path_params.get(p).and_then(id_string);
    // A bare `{id}` is named by the collection before it: a project's.
    let owner_name = match before.checked_sub(1).map(|i| segments[i]) {
        Some(collection) if mb_mcm::is_generic_id(owner) && segment_param(collection).is_none() => {
            collection.strip_suffix('s').unwrap_or(collection)
        }
        _ => owner,
    };
    Some(Owner {
        collection: fill_path(call, &segments[..at])?,
        param: owner_name.to_string(),
        value: value(owner)?,
        name: value(name)?,
        tail: fill_path(call, &segments[before..=at])?.path,
        writing: false,
    })
}

/// The path parameter naming what `op` acts on (see [`target_id`]).
fn target_param(op: &Operation) -> Option<String> {
    let identity = op.identity.as_ref();
    if let Some(param) = identity.and_then(|i| i.path_param.clone()) {
        return Some(param);
    }
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    matches!(
        op.kind,
        OperationKind::Command(Effect::Update | Effect::Delete)
    )
    .then(|| last_path_param(path))
    .flatten()
}

/// The [`Owner`] of what `op` targets, unless its parameter says it is an id:
/// an id stands on its own, whatever the route it is read by.
fn owner_of(op: &Operation, call: &OperationCall) -> Option<Owner> {
    let param = target_param(op)?;
    if id_like(&param) {
        return None;
    }
    owner_of_param(op, call, &param)
}

/// [`owner_of`], when consistency is on.
fn guarded(rt: &MockRuntime, op: &Operation, call: &OperationCall) -> Option<Owner> {
    owner_of(op, call).filter(|_| rt.consistency)
}

/// The collection a write or a list answer puts its resources in, when a path
/// parameter makes it one of many: `/orgs/octo/teams` for a list there, a create
/// there and an update of `/orgs/octo/teams/{slug}` alike. A target parameter in
/// no segment of its own (`{slug}.json`) gives none.
fn collection_of(op: &Operation, call: &OperationCall) -> Option<Collection> {
    fill_path(call, &collection_segments(op)?)
}

/// The segments of `op`'s path naming the collection it writes or lists in, when a
/// path parameter makes it one of many (see [`collection_of`]).
fn collection_segments(op: &Operation) -> Option<Vec<&str>> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let mut segments = path_segments(path);
    let end = match target_param(op) {
        Some(target) => {
            let param = format!("{{{target}}}");
            segments.iter().position(|s| *s == param)?
        }
        None => segments.len(),
    };
    segments.truncate(end);
    segments
        .iter()
        .any(|s| segment_param(s).is_some())
        .then_some(segments)
}

/// Whether the contract creates records of type `t` in a collection under a parent
/// path (`POST /groups/{group}/contacts`): where such a record is then matters.
fn created_in_collections(bundle: &mb_mcm::Bundle, t: &str) -> bool {
    bundle.operations.iter().any(|op| {
        op.kind == OperationKind::Command(Effect::Create)
            && op.resource_type.as_deref() == Some(t)
            && collection_segments(op).is_some()
    })
}

/// Whether `hit`, a stored record of the type `call`, a create of `op`, makes, is in the
/// collection the create runs in, as far as this runtime's writes tell: placed there, or,
/// placed nowhere known, when the create is top-level and the contract never creates the
/// type under a parent path (see [`created_in_collections`]). A record of a parent path
/// counts only under that parent: a label `bug` of one repository is no other's.
fn in_create_collection(
    rt: &MockRuntime,
    session: &SessionKey,
    op: &Operation,
    call: &OperationCall,
    hit: &StoredResource,
) -> bool {
    let collection = collection_of(op, call);
    match rt.collections().placement(session, hit) {
        Some(at) => at == collection,
        None => collection.is_none() && !created_in_collections(rt.bundle(), &hit.resource_type),
    }
}

/// What [`Collections`] holds at most; it is emptied when full.
const MAX_COLLECTIONS: usize = 4096;

/// Which collection a stored resource was listed or written under, from this
/// runtime's calls, with a hash of the resource: a change this runtime did not
/// record (a reset, a fixture) ends it with no bookkeeping. It decides only for
/// a read with the same template, which the contract says is the same
/// collection: `/orgs/octo/teams` for `/orgs/{org}/teams/{slug}`, whatever the
/// resource's members, but not `/orgs/octo/repos` for `/repos/{owner}/{repo}`.
/// Only a hint: otherwise, as on another node or after a restart, the resource
/// must show its owner (see [`shows_owner`]).
#[derive(Debug, Default)]
pub(crate) struct Collections(std::sync::Mutex<CollectionRecords>);

/// What [`Collections`] keeps.
#[derive(Debug, Default)]
struct CollectionRecords {
    records: HashMap<OwnRead, CollectionRecord>,
    /// The resources a top-level create stored, in no collection, until a write puts
    /// them in one.
    top_level: std::collections::HashSet<OwnRead>,
    /// The order of the next record.
    next: u64,
}

/// Where a resource was recorded, its hash then, when, and whether a create put it
/// there.
#[derive(Debug)]
struct CollectionRecord {
    collection: Collection,
    digest: u64,
    order: u64,
    created: bool,
}

impl Collections {
    fn remember(&self, session: &SessionKey, r: &StoredResource, collection: Collection) {
        self.record(session, r, collection, false);
    }

    fn record(
        &self,
        session: &SessionKey,
        r: &StoredResource,
        collection: Collection,
        created: bool,
    ) {
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        if held.records.len() + held.top_level.len() >= MAX_COLLECTIONS {
            held.records.clear();
            held.top_level.clear();
        }
        let order = held.next;
        held.next += 1;
        let key = OwnReads::key(session, r);
        held.top_level.remove(&key);
        // A later write or list there leaves what a create put there.
        let created = created
            || held
                .records
                .get(&key)
                .is_some_and(|was| was.created && was.collection == collection);
        let record = CollectionRecord {
            collection,
            digest: OwnReads::digest(&r.data),
            order,
            created,
        };
        held.records.insert(key, record);
    }

    fn forget(&self, session: &SessionKey, r: &StoredResource) {
        if let Ok(mut held) = self.0.lock() {
            let key = OwnReads::key(session, r);
            held.records.remove(&key);
            held.top_level.remove(&key);
        }
    }

    /// Records that a top-level create stored `r`, in no collection.
    fn place_top_level(&self, session: &SessionKey, r: &StoredResource) {
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        if held.records.len() + held.top_level.len() >= MAX_COLLECTIONS {
            held.records.clear();
            held.top_level.clear();
        }
        let key = OwnReads::key(session, r);
        held.records.remove(&key);
        held.top_level.insert(key);
    }

    /// Where `r` is, as far as this runtime's writes tell: `Some(Some(_))` in a
    /// collection (as [`Collections::get`] says), `Some(None)` stored by a top-level
    /// create in none, and `None` unknown (another node's, a seed's, or forgotten).
    fn placement(&self, session: &SessionKey, r: &StoredResource) -> Option<Option<Collection>> {
        if let Some(collection) = self.get(session, r) {
            return Some(Some(collection));
        }
        let held = self.0.lock().ok()?;
        held.top_level
            .contains(&OwnReads::key(session, r))
            .then_some(None)
    }

    /// The collection `r` was recorded under, as it stands.
    fn get(&self, session: &SessionKey, r: &StoredResource) -> Option<Collection> {
        let held = self.0.lock().ok()?;
        let record = held.records.get(&OwnReads::key(session, r))?;
        (record.digest == OwnReads::digest(&r.data)).then(|| record.collection.clone())
    }

    /// Whether a record for `r`'s key exists, as `r` may not be what it recorded.
    fn has(&self, session: &SessionKey, r: &StoredResource) -> bool {
        self.0
            .lock()
            .is_ok_and(|held| held.records.contains_key(&OwnReads::key(session, r)))
    }

    /// `old`'s record goes on to what replaced it, `new`, a create's mark with it.
    fn carry(&self, session: &SessionKey, old: &StoredResource, new: &StoredResource) {
        let created = {
            let Ok(held) = self.0.lock() else {
                return;
            };
            held.records
                .get(&OwnReads::key(session, old))
                .is_some_and(|record| record.created)
        };
        if let Some(collection) = self.get(session, old) {
            self.record(session, new, collection, created);
        }
    }

    /// The resources a create put in a collection `fits` in `session`, as (type, id),
    /// the newest first, at most `max`. They may since have changed or gone: the caller
    /// reads them and checks with [`Collections::get`].
    fn created_in(
        &self,
        session: &SessionKey,
        fits: impl Fn(&Collection) -> bool,
        max: usize,
    ) -> Vec<(String, String)> {
        let Ok(held) = self.0.lock() else {
            return Vec::new();
        };
        let mut found: Vec<(&OwnRead, u64)> = held
            .records
            .iter()
            .filter(|(key, record)| {
                record.created && key.session == *session && fits(&record.collection)
            })
            .map(|(key, record)| (key, record.order))
            .collect();
        found.sort_by_key(|(_, order)| std::cmp::Reverse(*order));
        found
            .into_iter()
            .take(max)
            .map(|(key, _)| (key.resource_type.clone(), key.id.clone()))
            .collect()
    }

    /// Whether `r` is `owner`'s, when a record can say.
    fn fits(&self, session: &SessionKey, r: &StoredResource, owner: &Owner) -> Option<bool> {
        let collection = self.get(session, r)?;
        (collection.template == owner.collection.template)
            .then(|| collection.path == owner.collection.path)
    }
}

/// Whether `text` is a path ending in `owner`'s value and name: `octo/hello`
/// (a `full_name`, a `path_with_namespace`) or `…/octo/teams/core` (a `url`).
fn path_shows(text: &str, owner: &Owner) -> bool {
    let text = text.trim_end_matches('/');
    let short = format!("{}/{}", owner.value, owner.name);
    [short.as_str(), owner.tail.as_str()].iter().any(|path| {
        text.strip_suffix(path)
            .is_some_and(|rest| rest.is_empty() || rest.ends_with('/'))
    })
}

/// Whether `text` is a path relative to nothing, ending in `name`: `octo/hello`.
fn relative_path_ending(text: &str, name: &str) -> bool {
    !text.contains("://")
        && !text.starts_with('/')
        && text
            .trim_end_matches('/')
            .rsplit_once('/')
            .is_some_and(|(_, last)| last == name)
}

/// How a member relates to parameter `param`: named for it, or for its id.
/// `org` also means `organization`, but `org_ref` and `owner_previous` are
/// other things.
fn tied_to_param(member: &str, param: &str) -> bool {
    let (member, param) = (member.to_lowercase(), param.to_lowercase());
    member == param
        || member == format!("{param}_id")
        || member == format!("{param}id")
        || (param.len() >= 3
            && member.starts_with(&param)
            && member.chars().all(char::is_alphanumeric))
}

/// What `r` itself says of `owner`, `None` when it says nothing: a relative path
/// ending in the name decides, all of them having to show the owner (a fork
/// names its source, and its `full_name` says whose it is); else a member tied
/// to the owner's parameter, as a value or an object holding it, not one the
/// contract types as `r`'s own type; else a link showing the owner.
fn owner_evidence(rt: &MockRuntime, r: &StoredResource, owner: &Owner) -> Option<bool> {
    let members = r.data.as_object()?;
    let strings = || members.values().filter_map(Value::as_str);
    let mut paths = strings()
        .filter(|text| relative_path_ending(text, &owner.name))
        .peekable();
    if paths.peek().is_some() {
        return Some(paths.all(|text| path_shows(text, owner)));
    }
    let own_type = |member: &str| typed_as_own(rt, &r.resource_type, member);
    let holds = |value: &Value| match value {
        Value::Object(inner) => inner
            .values()
            .any(|v| v.as_str() == Some(owner.value.as_str())),
        value => id_string(value).as_deref() == Some(owner.value.as_str()),
    };
    let is_link = |value: &Value| value.as_str().is_some_and(|text| text.contains("://"));
    let numeric_owner = owner.value.parse::<f64>().is_ok();
    let mut tied = members
        .iter()
        .filter(|(member, value)| {
            !mb_mcm::is_generic_id(member)
                && tied_to_param(member, &owner.param)
                && !is_link(value)
                && !own_type(member)
        })
        // A number says nothing of an owner the route names by a string.
        .filter(|(_, value)| {
            value.is_string() || value.is_object() || (value.is_number() && numeric_owner)
        })
        .peekable();
    if tied.peek().is_some() {
        return Some(tied.any(|(_, value)| holds(value)));
    }
    strings()
        .any(|text| text.contains("://") && path_shows(text, owner))
        .then_some(true)
}

/// Whether the contract types `member` of a `resource_type` record as that same type
/// (a task's `parent`, a user's `manager`), by the type's record schema: such a member
/// names another of its kind, not an owner.
fn typed_as_own(rt: &MockRuntime, resource_type: &str, member: &str) -> bool {
    let index = rt.index();
    let schema = index.record_schemas.get(resource_type).or_else(|| {
        let i = index.detail_reads.get(resource_type)?;
        rt.bundle()
            .operations
            .get(*i)?
            .success_output()?
            .schema
            .as_ref()
    });
    let (parts, _) = schema_parts(rt.bundle(), schema);
    let inner = parts.iter().find_map(|p| p.get("properties")?.get(member));
    let (_, defs) = schema_parts(rt.bundle(), inner);
    defs.iter()
        .any(|d| index.types_by_def.get(*d).map(String::as_str) == Some(resource_type))
}

/// Whether `r` shows it is `owner`'s (see [`owner_evidence`]).
fn shows_owner(rt: &MockRuntime, r: &StoredResource, owner: &Owner) -> bool {
    owner_evidence(rt, r, owner) == Some(true)
}

/// Whether a string of `r` ending in the name does not show `owner`: another
/// owner's link, or none at all. Weak: a `homepage` may end so.
fn links_elsewhere(r: &StoredResource, owner: &Owner) -> bool {
    r.data.as_object().is_some_and(|members| {
        members.values().filter_map(Value::as_str).any(|text| {
            text.trim_end_matches('/')
                .rsplit_once('/')
                .is_some_and(|(_, last)| last == owner.name)
                && !path_shows(text, owner)
        })
    })
}

/// Whether `r` is known to be another's: by a record, by what it says, or, when
/// it says nothing, by a link elsewhere.
fn known_foreign(
    rt: &MockRuntime,
    session: &SessionKey,
    r: &StoredResource,
    owner: &Owner,
) -> bool {
    rt.collections().fits(session, r, owner).map_or_else(
        || match owner_evidence(rt, r, owner) {
            Some(shown) => !shown,
            None => links_elsewhere(r, owner),
        },
        |fits| !fits,
    )
}

/// Whether stored `r` may be taken for `owner`'s under the value in `field`. A
/// write acts on what it cannot tell is another's, or it would leave it behind.
/// A read needs a record or what `r` shows, as an answer can be generated. A
/// number cannot show its owner, so it passes unless `r` links elsewhere.
fn owner_fits(
    rt: &MockRuntime,
    session: &SessionKey,
    r: &StoredResource,
    owner: &Owner,
    field: &str,
) -> bool {
    if owner.writing {
        return !known_foreign(rt, session, r, owner);
    }
    rt.collections()
        .fits(session, r, owner)
        .unwrap_or_else(|| match owner_evidence(rt, r, owner) {
            Some(shown) => shown,
            None => r.data.get(field).is_some_and(Value::is_number) && !links_elsewhere(r, owner),
        })
}

/// Whether `r`, stored under the value a path gave, is `owner`'s to answer with.
/// A write takes it unless it is known to be another's. For a read a record
/// decides; else a key that is no name of `r` is `r`'s id, and stands on its
/// own unless `r` says otherwise. A name (in one of [`NAME_FIELDS`], or the end of a relative path) must
/// show its owner, or differ from `r`'s own `id`: only a write through the route
/// keys it so, as a list or a mention keys by the `id`.
fn key_fits(rt: &MockRuntime, session: &SessionKey, r: &StoredResource, owner: &Owner) -> bool {
    if owner.writing {
        return !known_foreign(rt, session, r, owner);
    }
    if let Some(fits) = rt.collections().fits(session, r, owner) {
        return fits;
    }
    let Value::Object(members) = &r.data else {
        return true;
    };
    let is_name = NAME_FIELDS
        .iter()
        .any(|field| members.get(*field).and_then(id_string).as_ref() == Some(&r.id))
        || members
            .values()
            .filter_map(Value::as_str)
            .any(|text| relative_path_ending(text, &r.id));
    let written = members
        .get("id")
        .and_then(id_string)
        .is_some_and(|id| id != r.id);
    let evidence = owner_evidence(rt, r, owner);
    if is_name {
        written || evidence == Some(true)
    } else {
        evidence != Some(false)
    }
}

/// The members that may hold a name path parameter `param` gives: one named for
/// the parameter, which the contract ties to it, and [`NAME_FIELDS`]. Any other
/// string could hold anything: a `description`, a `default_branch`.
fn name_fields(param: &str) -> Vec<&str> {
    let mut fields = NAME_FIELDS.to_vec();
    if !fields.contains(&param) {
        fields.push(param);
    }
    fields
}

/// Whether any owner's resource of type `t` goes by `value`: a route that cannot
/// tell whose it is cannot call it missing.
async fn any_owner_holds(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    value: &str,
    param: &str,
) -> bool {
    if state.get(session, t, value).await.is_some() {
        return true;
    }
    let rt = state.rt;
    let mut fields = name_fields(param);
    fields.extend(rt.index().path_fields(t).iter().map(String::as_str));
    by_path_fields(state, session, t, &fields, value, None)
        .await
        .is_some()
}

/// The resource of type `t` whose field is `value`, trying `fields` in order
/// (a read by GitHub's issue number, or a repository's name) and, with
/// `owner`, only one that is theirs: the one [`PathAliases`] names when it still
/// holds the value, else the first of one scan.
async fn by_path_fields(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    fields: &[&str],
    value: &str,
    owner: Option<&Owner>,
) -> Option<StoredResource> {
    if fields.is_empty() {
        return None;
    }
    let rt = state.rt;
    let holds = |r: &StoredResource, field: &str| {
        r.data.get(field).and_then(id_string).as_deref() == Some(value)
            && owner.is_none_or(|o| owner_fits(rt, session, r, o, field))
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
    let found = fields.iter().find_map(|field| {
        let found = stored.iter().find(|r| holds(r, field))?;
        Some((*field, found))
    });
    let Some((field, found)) = found else {
        // No member holds the name alone, but a path may hold it with its owner.
        let owner = owner?;
        return stored
            .iter()
            .find(|r| {
                r.data.as_object().is_some_and(|m| {
                    m.values()
                        .filter_map(Value::as_str)
                        .any(|t| path_shows(t, owner))
                }) && owner_fits(rt, session, r, owner, "")
            })
            .cloned();
    };
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
    /// This call keeps and honours deleted markers (see [`deleted`]).
    tracks_deleted: bool,
    /// The path parameters of the call, for the parent of what it mentions.
    call_params: Option<Box<OperationCall>>,
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
            tracks_deleted: false,
            call_params: None,
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

    /// Whether the session is live, refreshing it if so. `false` when it is not, or the
    /// store is unavailable.
    async fn live(&mut self, s: &SessionKey) -> bool {
        if !self.open() {
            return false;
        }
        let r = self.rt.state.touch_live(s).await;
        self.settle(r).unwrap_or(false)
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
        let t = r.resource_type.clone();
        let indexed = value_index::Indexed::of(rt, &r);
        let r = rt.state.upsert(s, r).await;
        let ok = self.settle(r).is_some();
        self.wrote |= ok;
        if ok && t != mb_state::INTERNAL_TYPE {
            rt.written_types().remember(s, &t);
        }
        if ok {
            rt.value_index().remember_indexed(s, indexed);
        }
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
        if ok {
            self.rt.value_index().forget(s, t, id);
        }
        ok
    }

    /// Stores a seeded resource unless one is stored under its id or it was
    /// deleted; `true` when stored.
    pub(crate) async fn insert_seeded(&mut self, s: &SessionKey, r: StoredResource) -> bool {
        if !self.open() {
            return false;
        }
        if self.tracks_deleted
            && deleted::mention_is_deleted(self, s, &r.resource_type, &r.id).await
        {
            return false;
        }
        let rt = self.rt;
        let values = path_values(rt.index().path_fields(&r.resource_type), &r.data);
        let named = (!values.is_empty()).then(|| (r.resource_type.clone(), r.id.clone()));
        let t = r.resource_type.clone();
        let indexed = value_index::Indexed::of(rt, &r);
        let r = rt.state.insert_seeded(s, r).await;
        let stored = self.settle(r).unwrap_or(false);
        self.wrote |= stored;
        if stored {
            rt.written_types().remember(s, &t);
            rt.value_index().remember_indexed(s, indexed);
        }
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

/// The resource of type `t` the session holds under `id`.
#[cfg(feature = "graphql")]
pub(crate) async fn stored(
    rt: &MockRuntime,
    session: &SessionKey,
    t: &str,
    id: &str,
) -> Option<StoredResource> {
    let mut state = StateAccess::new(rt);
    async { state.get(session, t, id).await }
        .instrument(state_span())
        .await
}

/// Removes `gone`, a resource the session holds, from it. Keeps the
/// session alive.
#[cfg(feature = "graphql")]
pub(crate) async fn delete_resource(rt: &MockRuntime, session: &SessionKey, gone: StoredResource) {
    let mut state = StateAccess::new(rt);
    async {
        if state.delete(session, &gone.resource_type, &gone.id).await {
            rt.collections().forget(session, &gone);
        }
        state.touch(session).await;
    }
    .instrument(state_span())
    .await;
}

/// Resources the response should stay consistent with, nearest first: what
/// the request names, then recent items for a list query. [`follow_hops`]
/// adds what those refer to. With consistency off, only the `<x>Id` members
/// and global ids of the query and body are followed.
async fn related(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    target_id: Option<&str>,
    target: Option<&StoredResource>,
    call: &OperationCall,
) -> (Vec<StoredResource>, Followed) {
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
    // A name only means something under its owner: another's is not named.
    if index.is_some() {
        for (field, value) in &call.path_params {
            let (Some(owner), Some(name)) = (
                Some(field.as_str())
                    .filter(|f| !id_like(f))
                    .and_then(|f| owner_of_param(op, call, f)),
                id_string(value),
            ) else {
                continue;
            };
            out.retain(|r| r.id != name || key_fits(rt, session, r, &owner));
        }
    }
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
    (out, Followed { named, seen })
}

/// What [`related`] followed, for [`follow_hops`] to go on from.
struct Followed {
    /// How many of the related resources the request itself names; they
    /// come first.
    named: usize,
    /// The references looked up so far (see [`follow`]).
    seen: Vec<Reference>,
}

/// Adds to `out`, after what the request names, the parents its path names by
/// name where a lookup by id found nothing: `{repo}` in
/// `/repos/{owner}/{repo}/issues` is a repository stored under its numeric id
/// (see [`NAME_FIELDS`]). Only a path parameter [`related`] looked up as a
/// reference, other than the one naming the target (already searched for), so
/// this stays within [`MAX_REFERENCES`]. Separate from [`related`], for what
/// [`follow_hops`] is. Nothing with consistency off.
async fn parents_by_name(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    call: &OperationCall,
    target: Option<&StoredResource>,
    followed: &mut Followed,
    out: &mut Vec<StoredResource>,
) {
    let rt = state.rt;
    if !rt.consistency {
        return;
    }
    let own = op.identity.as_ref().and_then(|i| i.path_param.as_deref());
    for (field, value) in &call.path_params {
        let (Some(id), false) = (id_string(value), own == Some(field.as_str())) else {
            continue;
        };
        let Some(t) = entity_type(rt.index(), None, field) else {
            continue;
        };
        let reference = (vec![t.clone()], id.clone());
        let found = |r: &StoredResource| r.resource_type == t && r.id == id;
        if !followed.seen.contains(&reference)
            || target.is_some_and(found)
            || out[..followed.named].iter().any(found)
        {
            continue;
        }
        let owner = owner_of_param(op, call, field);
        let names = name_fields(field);
        let Some(parent) = by_path_fields(state, session, &t, &names, &id, owner.as_ref()).await
        else {
            continue;
        };
        let is_target =
            target.is_some_and(|r| r.resource_type == parent.resource_type && r.id == parent.id);
        if is_target || holds(&out[..followed.named], &parent) {
            continue;
        }
        // Listed already, it moves up among what the request names.
        out.retain(|r| r.resource_type != parent.resource_type || r.id != parent.id);
        out.insert(followed.named, parent);
        followed.named += 1;
    }
}

/// Adds to `out`, as gathered by [`related`], what the target and the
/// resources the request names refer to. Separate from [`related`] because
/// only a generation needs it: an answer served from the store or the cache
/// must not pay for these lookups. Nothing with consistency off.
async fn follow_hops(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    target: Option<&StoredResource>,
    followed: Followed,
    out: &mut Vec<StoredResource>,
) {
    let rt = state.rt;
    if !rt.consistency {
        return;
    }
    let index = Some(rt.index());
    let Followed { named, mut seen } = followed;
    let (mut own, mut start, mut end) = (target, 0, named);
    for _ in 0..MAX_RELATED_HOPS {
        let mut references = Vec::new();
        for resource in own.take().into_iter().chain(&out[start..end]) {
            references_in("", &resource.data, 0, index, &mut references);
        }
        start = out.len();
        follow(state, session, references, &mut seen, target, out).await;
        end = out.len();
    }
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
        for t in types.iter().filter(|t| *t != mb_state::INTERNAL_TYPE) {
            let t = t.as_str();
            if target.is_some_and(|r| r.resource_type == t && r.id == id) {
                break;
            }
            if let Some(hit) = state.get(session, t, &id).await {
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

/// Records [`created_alongside`] reads, at most.
const MAX_CREATED_ALONGSIDE: usize = 4;

/// The path of the record a read by path parameter reads: `/captures/C1` of
/// `/captures/{capture_id}`.
fn record_path(op: &Operation, call: &OperationCall) -> Option<String> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let param = format!("{{{}}}", target_param(op)?);
    let segments = path_segments(path);
    let at = segments.iter().position(|s| *s == param)?;
    Some(fill_path(call, &segments[..=at])?.path)
}

/// Adds to `related` what creates put in the collection under a parent path that a
/// create or a read runs in (`/captures/C1/refund`), newest first: a second refund is
/// shown the first. A read of one record by its path (`/captures/C1`) is shown, the
/// same way, what creates put under that path, so a capture's read sees its refunds.
/// The record must still be what was created there, or [`Collections`] no longer
/// places it; it is scrubbed as a record found by value is
/// ([`write_memory::matched_for_context`]). Context only, and only where
/// [`write_memory::applies`] holds. For a create, the type and id of every record of
/// `related` that creates put in its own collection, already shown or added here: what
/// it can conflict with by being of their type (see
/// [`mb_engine::GenerationContext::alongside`]).
async fn created_alongside(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target: Option<&StoredResource>,
    related: &mut Vec<StoredResource>,
) -> Vec<(String, String)> {
    let rt = state.rt;
    let op = plan.op;
    let mut marked = Vec::new();
    if !write_memory::applies(rt, plan)
        || !matches!(
            op.kind,
            OperationKind::Query | OperationKind::Command(Effect::Create)
        )
    {
        return marked;
    }
    let create = matches!(op.kind, OperationKind::Command(Effect::Create));
    let alongside = collection_of(op, call);
    let beneath = matches!(op.kind, OperationKind::Query)
        .then(|| record_path(op, call))
        .flatten()
        .map(|path| format!("{path}/"));
    if alongside.is_none() && beneath.is_none() {
        return marked;
    }
    let fits = |c: &Collection| {
        alongside.as_ref() == Some(c) || beneath.as_ref().is_some_and(|p| c.path.starts_with(p))
    };
    let session = &plan.session;
    let created = rt
        .collections()
        .created_in(session, fits, MAX_CREATED_ALONGSIDE);
    for (t, id) in created {
        let shown = |r: &StoredResource| r.resource_type == t && r.id == id;
        if target.is_some_and(shown) {
            continue;
        }
        if related.iter().any(shown) {
            // Already shown, by value, as a scrubbed copy: whether this collection
            // still holds it is the stored copy's to say.
            if create
                && let Some(held) = state.get(session, &t, &id).await
                && rt
                    .collections()
                    .get(session, &held)
                    .is_some_and(|c| fits(&c))
            {
                marked.push((t.clone(), id.clone()));
            }
            continue;
        }
        let Some(held) = state.get(session, &t, &id).await else {
            continue;
        };
        if !rt
            .collections()
            .get(session, &held)
            .is_some_and(|c| fits(&c))
        {
            continue;
        }
        if let Some(data) = write_memory::matched_for_context(rt, plan, &t, &held.data) {
            if create {
                marked.push((t.clone(), id.clone()));
            }
            related.push(StoredResource { data, ..held });
        }
    }
    marked
}

/// Puts first in `related` the records of the type a create makes that already hold a
/// value of its request under the same member ([`value_index::same_values`]), so a
/// prompt shows them whatever else it shows, and returns each one's type, id and member
/// (see [`mb_engine::GenerationContext::same_values`]). A record `related` holds already
/// keeps the copy it has; another is scrubbed as a record found by value is
/// ([`write_memory::matched_for_context`]), and left out when nothing of it may be shown.
/// Nothing, and `related` as it was, when there is none.
async fn shown_same_values(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target: Option<&StoredResource>,
    related: &mut Vec<StoredResource>,
) -> Vec<(String, String, String)> {
    let rt = state.rt;
    let known = |t: &str, id: &str| target.is_some_and(|r| r.resource_type == t && r.id == id);
    let found = value_index::same_values(state, plan, call, known).await;
    let mut first = Vec::with_capacity(found.len());
    let mut marked = Vec::with_capacity(found.len());
    for (hit, member) in found {
        let shown = match related
            .iter()
            .position(|r| r.resource_type == hit.resource_type && r.id == hit.id)
        {
            Some(at) => related.remove(at),
            None => {
                let Some(data) =
                    write_memory::matched_for_context(rt, plan, &hit.resource_type, &hit.data)
                else {
                    continue;
                };
                StoredResource { data, ..hit }
            }
        };
        marked.push((shown.resource_type.clone(), shown.id.clone(), member));
        first.push(shown);
    }
    related.splice(0..0, first);
    marked
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
                out.push((reference_types(index, prefix), id.clone()));
            } else if let Some(t) = index.and_then(|i| entity_type(i, None, field)) {
                out.push((vec![t], id.clone()));
            }
        }
        Value::Number(id) if index.is_some() => {
            if let Some(prefix) = reference_prefix(field) {
                out.push((reference_types(index, prefix), id.to_string()));
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

/// The types a `<prefix>Id` member refers to: `<prefix>s` and `<prefix>`, then, with
/// `index`, each type of the contract of the same stem ([`type_stem`]), so that
/// `eventTypeId` and `event_type_id` find `event-types`. Sorted after the first two.
fn reference_types(index: Option<&BundleIndex>, prefix: &str) -> Vec<String> {
    let mut types = vec![format!("{prefix}s"), prefix.to_string()];
    let Some(index) = index else {
        return types;
    };
    let stem = type_stem(prefix);
    let mut known: Vec<&String> = index
        .read_types
        .iter()
        .chain(index.detail_reads.keys())
        .chain(&index.effect_types)
        .chain(
            index
                .listed
                .iter()
                .filter(|t| !ENVELOPE_KEYS.contains(&t.as_str())),
        )
        .filter(|t| !types.contains(t) && type_stem(t) == stem)
        .collect();
    known.sort();
    known.dedup();
    types.extend(known.into_iter().cloned());
    types
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
    /// Where the path puts what this writes (see [`collection_of`]).
    collection: Option<Collection>,
    origin: WriteOrigin,
    /// The session held what this writes to, so it cannot be marked deleted.
    held: bool,
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
        collection,
        origin,
        held: was_held,
    } = w;
    let rt = state.rt;
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
            // Marked under this list's parent; a mention is seen to on insert.
            if state.tracks_deleted {
                let parent = state
                    .call_params
                    .as_ref()
                    .and_then(|call| deleted::scope(op, call));
                if deleted::is_deleted(state, session, resource_type, parent.as_deref(), &id).await
                {
                    continue;
                }
            }
            let resource = StoredResource {
                resource_type: resource_type.into(),
                id: id.clone(),
                data: item.clone(),
            };
            let stored = resource.clone();
            let Some(collection) = collection.clone() else {
                state.insert_seeded(session, resource).await;
                continue;
            };
            if state.insert_seeded(session, resource).await {
                rt.collections().remember(session, &stored, collection);
            } else if let Some(held) = state.get(session, resource_type, &id).await
                && rt.collections().get(session, &held).is_none()
                && held.data == stored.data
            {
                // A mention stored it first: this list says where it belongs.
                rt.collections().remember(session, &held, collection);
            }
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
            // An answer of another schema adds nothing of its own to the held record.
            let data = match &generated.body {
                Some(_) if was_held && !generated.new_record => {
                    let held = state.get(session, resource_type, &id).await;
                    let on = (resource_type, held.as_ref().map(|h| &h.data));
                    write_memory::onto_record(rt, op, on, data, generated.status)
                }
                // With nothing held, an answer that does not suit the type (a task
                // acknowledging the update) is no record of it: nothing is stored.
                Some(_) if !entity_fits(rt, resource_type, &data) => return,
                _ => data,
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
            let stored = resource.clone();
            // A route naming no collection keeps the one it had.
            let held = match &collection {
                None if rt.collections().has(session, &stored) => {
                    state.get(session, resource_type, &stored.id).await
                }
                _ => None,
            };
            if state.upsert(session, resource).await {
                if state.tracks_deleted && !was_held {
                    let scope = collection.as_ref().map(|c| c.path.as_str());
                    deleted::clear(state, session, resource_type, scope, &stored).await;
                }
                let created = op.kind == OperationKind::Command(Effect::Create);
                match (collection, held) {
                    (Some(collection), _) => {
                        rt.collections()
                            .record(session, &stored, collection, created);
                    }
                    (None, Some(held)) => rt.collections().carry(session, &held, &stored),
                    (None, None) if created => rt.collections().place_top_level(session, &stored),
                    (None, None) => {}
                }
            }
        }
        None => {
            let gone = state.get(session, resource_type, &id).await;
            state.delete(session, resource_type, &id).await;
            if let Some(gone) = gone {
                rt.collections().forget(session, &gone);
            }
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
    fn an_effect_type_matches_the_one_shown_type_of_its_stem() {
        let shown: Vec<String> = ["playlists", "message_reactions", "tasks", "Issues"]
            .into_iter()
            .map(String::from)
            .collect();
        for (written, t) in [
            ("playlist", "playlists"),
            ("Playlists", "playlists"),
            ("messageReaction", "message_reactions"),
            ("message-reaction", "message_reactions"),
            ("task", "tasks"),
            ("issue", "Issues"),
        ] {
            assert_eq!(stem_type(&shown, written, &[]), Some(t), "{written}");
        }
        assert_eq!(stem_type(&shown, "order", &[]), None);
        // Two shown types of one stem: no guess.
        let both: Vec<String> = vec!["entries".into(), "entry".into()];
        assert_eq!(stem_type(&both, "Entry", &[]), None);
    }

    #[test]
    fn a_create_s_effects_copy_its_request_onto_no_sibling() {
        let effect = |t: &str, id: &str, set: Value| ResourceEffect {
            resource_type: t.into(),
            id: id.into(),
            set: set.as_object().cloned().unwrap_or_default(),
        };
        let sets = |v: &[ResourceEffect]| {
            v.iter()
                .map(|e| format!("{}:{}", e.id, Value::Object(e.set.clone())))
                .collect::<Vec<_>>()
        };
        let split = |body: Value, effects: Vec<ResourceEffect>| {
            let (kept, dropped) = sibling_effects("payments", Some("new"), Some(&body), effects);
            (sets(&kept), sets(&dropped))
        };
        // The amount alone, though `1` is the sibling's id.
        assert_eq!(
            split(
                json!({"amount": 1, "description": "Order X-2"}),
                vec![effect("payments", "1", json!({"amount": 1}))],
            ),
            (vec![], vec!["1:{\"amount\":1}".to_string()])
        );
        // An amount the request does not give, and a default it does not copy, apply.
        assert_eq!(
            split(
                json!({"amount": 9.99, "is_default": true}),
                vec![effect(
                    "payments",
                    "1",
                    json!({"amount": 9.990, "is_default": false, "replies": 1})
                )],
            ),
            (
                vec!["1:{\"is_default\":false,\"replies\":1}".to_string()],
                vec!["1:{\"amount\":9.99}".to_string()]
            )
        );
        // A nested object holding a copied value goes whole, its other members too.
        assert_eq!(
            split(
                json!({"fields": {"parent": {"id": "tr_1"}, "note": "copied"}}),
                vec![effect(
                    "payments",
                    "tr_1",
                    json!({"fields": {"parent": {"id": "tr_1"}, "note": "copied"}, "replies": 2})
                )],
            ),
            (
                vec!["tr_1:{\"replies\":2}".to_string()],
                vec![
                    "tr_1:{\"fields\":{\"note\":\"copied\",\"parent\":{\"id\":\"tr_1\"}}}"
                        .to_string()
                ]
            )
        );
        // So does one holding a nested value of the request beside values of its own.
        assert_eq!(
            split(
                json!({"fields": {"parent": {"id": "tr_1"}, "note": "copied"}}),
                vec![effect(
                    "payments",
                    "tr_1",
                    json!({"fields": {"parent": {"id": "tr_1"}, "note": "its own"}})
                )],
            )
            .0,
            Vec::<String>::new()
        );
        // The sibling's own id is copied as much as any value: a reply's `in_reply_to`
        // on its parent, a `parent_id` making a record its own parent, a `quantity` that
        // happens to equal an id. What the request does not give still applies.
        assert_eq!(
            split(
                json!({"amount": 3, "in_reply_to": 1_234_567}),
                vec![effect(
                    "payments",
                    "1234567",
                    json!({"in_reply_to": 1_234_567, "replies": 1})
                )],
            ),
            (
                vec!["1234567:{\"replies\":1}".to_string()],
                vec!["1234567:{\"in_reply_to\":1234567}".to_string()]
            )
        );
        assert_eq!(
            split(
                json!({"parent_id": "tr_1"}),
                vec![effect("payments", "tr_1", json!({"parent_id": "tr_1"}))],
            ),
            (vec![], vec!["tr_1:{\"parent_id\":\"tr_1\"}".to_string()])
        );
        assert_eq!(
            split(
                json!({"amount": 2, "quantity": 1}),
                vec![effect("payments", "1", json!({"quantity": 1}))],
            ),
            (vec![], vec!["1:{\"quantity\":1}".to_string()])
        );
        // The created record and other types are left as they are.
        assert_eq!(
            split(
                json!({"amount": 1}),
                vec![
                    effect("payments", "new", json!({"amount": 1})),
                    effect("customers", "1", json!({"amount": 1})),
                ],
            ),
            (
                vec![
                    "new:{\"amount\":1}".to_string(),
                    "1:{\"amount\":1}".to_string()
                ],
                vec![]
            )
        );
    }

    #[test]
    fn the_id_only_chooses_among_types_of_the_stem() {
        let shown: Vec<String> = ["entries", "entry", "playlists", "tracks"]
            .into_iter()
            .map(String::from)
            .collect();
        // Several of the stem: the one holding the id.
        assert_eq!(stem_type(&shown, "Entry", &["entry"]), Some("entry"));
        assert_eq!(stem_type(&shown, "Entry", &["entries", "entry"]), None);
        assert_eq!(stem_type(&shown, "Entry", &["tracks"]), None);
        // One of the stem, the id held elsewhere: they disagree.
        assert_eq!(stem_type(&shown, "playlist", &["tracks"]), None);
        assert_eq!(
            stem_type(&shown, "playlist", &["playlists"]),
            Some("playlists")
        );
        // Held nowhere: the stem's type, which then holds no such resource.
        assert_eq!(stem_type(&shown, "playlist", &[]), Some("playlists"));
        // No stem match: the id names no type.
        assert_eq!(stem_type(&shown, "song", &["tracks"]), None);
    }

    #[test]
    fn a_stub_names_its_record_and_says_nothing_else() {
        let stored = json!({"id": "t1", "uri": "u:t1", "name": "Song", "plays": 3});
        let stub = |v: Value| v.as_object().unwrap().clone();
        let id = ["id", "number"];
        assert!(is_stub(&stub(json!({"id": "t1"})), &stored, &id));
        assert!(is_stub(
            &stub(json!({"id": "t1", "uri": "u:t1"})),
            &stored,
            &id
        ));
        // A link that is not the stored one, a member that is not a link, no identity.
        assert!(!is_stub(
            &stub(json!({"id": "t1", "uri": "u:x"})),
            &stored,
            &id
        ));
        assert!(!is_stub(
            &stub(json!({"id": "t1", "plays": 3})),
            &stored,
            &id
        ));
        assert!(!is_stub(&stub(json!({"uri": "u:t1"})), &stored, &id));
        // As much as the record: nothing to fill.
        assert!(!is_stub(&stub(stored.clone()), &stored, &id));
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
        assert_eq!(
            cache_key(&rt.bundle, op, &a, None),
            cache_key(&rt.bundle, op, &b, None)
        );
        assert_ne!(
            cache_key(&rt.bundle, op, &a, None),
            cache_key(&rt.bundle, op, &call(json!({"a": 2, "b": 2})), None)
        );
        let narrow = json!({"type": "object", "properties": {"id": {"type": "string"}}});
        assert_ne!(
            cache_key(&rt.bundle, op, &a, None),
            cache_key(&rt.bundle, op, &a, Some(&narrow))
        );
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
            record: None,
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

    /// A list under a member naming a type read by path is that type, and one a
    /// record holds, of a schema no read serves, is of no type: not the record's,
    /// and not one named for its member.
    #[test]
    fn a_listed_def_takes_its_members_type() {
        let path = format!(
            "{}/../../docs/benchmarks/contracts/travel.openapi.yaml",
            env!("CARGO_MANIFEST_DIR")
        );
        let rt = runtime_of(compile(&std::fs::read(path).unwrap()).unwrap());
        let index = rt.index();
        assert_eq!(index.types_by_def["Offer"], "offers");
        assert_eq!(index.types_by_def["FlightOffer"], "offers");
        assert_eq!(index.types_by_def.get("PassengerRecord"), None);
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

    /// A `<x>Id` finds a type of the contract whatever case either is named in, and
    /// only one of the same stem.
    #[test]
    fn references_find_types_of_the_same_stem() {
        let contract = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /event-types/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getEventType
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}}
  /events/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getEvent
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}}
"##;
        let rt = runtime_of(mb_import::compile(contract.as_bytes()).unwrap());
        let index = Some(rt.index());
        for field in ["eventTypeId", "event_type_id", "EventTypeId"] {
            let mut out = Vec::new();
            references_in(field, &json!("et_1"), 0, index, &mut out);
            let [(types, id)] = out.as_slice() else {
                panic!("{field}: {out:?}");
            };
            assert_eq!(id, "et_1");
            assert!(
                types.iter().any(|t| t == "event-types"),
                "{field}: {types:?}"
            );
            assert!(!types.iter().any(|t| t == "events"), "{field}: {types:?}");
        }
        assert_eq!(
            reference_types(None, "eventType"),
            ["eventTypes", "eventType"]
        );
    }

    /// What an LLM wrote into a log field is bounded, in characters.
    #[test]
    fn log_fields_are_capped() {
        assert_eq!(log_capped("ch_1"), "ch_1");
        let exact = "é".repeat(MAX_LOGGED_CHARS);
        assert_eq!(log_capped(&exact), exact);
        let long = log_capped(&"é".repeat(MAX_LOGGED_CHARS * 100));
        assert_eq!(long.chars().count(), MAX_LOGGED_CHARS + 1);
        assert!(long.ends_with('…'));
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

    /// Counts `get` and `list` calls on a memory store.
    struct ListSpy {
        inner: MemoryStore,
        gets: std::sync::atomic::AtomicUsize,
        lists: std::sync::atomic::AtomicUsize,
    }

    impl ListSpy {
        fn gets(&self) -> usize {
            self.gets.load(std::sync::atomic::Ordering::SeqCst)
        }

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
            self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
            gets: Default::default(),
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

    /// A read answered from the store never reaches a prompt, so it looks up
    /// its target and nothing the target refers to.
    /// The search for a parent by name is a generation's: a read the store
    /// answers makes no scan, and a generation one, then none.
    #[cfg(feature = "http-rest")]
    #[tokio::test]
    async fn only_a_generation_scans_for_a_parent_by_name() {
        use tower::ServiceExt;
        const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Nested, version: "1"}
paths:
  /repos/{repo}:
    get:
      operationId: getRepo
      parameters: [{name: repo, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Repo"}}}}
  /repos/{repo}/issues/{id}:
    get:
      operationId: getIssue
      parameters:
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
components:
  schemas:
    Repo:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    Issue:
      type: object
      required: [id]
      properties: {id: {type: string}}
"##;
        let spy = Arc::new(ListSpy {
            inner: MemoryStore::new(Duration::from_secs(3600)),
            gets: Default::default(),
            lists: Default::default(),
        });
        let rt = MockRuntime::new(
            "m",
            Arc::new(compile(CONTRACT.as_bytes()).unwrap()),
            Arc::new(Engine::new(None, EngineConfig::default())),
            spy.clone(),
        );
        let s = SessionKey {
            workspace: "default".into(),
            mock: "m".into(),
            session: DEFAULT_SESSION.into(),
        };
        for (t, id, data) in [
            ("repos", "7", json!({"id": "7", "name": "widget"})),
            ("issues", "i1", json!({"id": "i1"})),
        ] {
            let stored = StoredResource {
                resource_type: t.into(),
                id: id.into(),
                data,
            };
            spy.inner.upsert(&s, stored).await.unwrap();
        }
        let router = crate::http_rest::router(Arc::new(rt));
        let get = |uri: &'static str| {
            let router = router.clone();
            async move {
                let request = axum::http::Request::get(uri)
                    .body(axum::body::Body::empty())
                    .unwrap();
                router.oneshot(request).await.unwrap()
            }
        };
        let served = get("/repos/widget/issues/i1").await;
        assert_eq!(served.headers()["x-mockingbird-source"], "state");
        assert_eq!(spy.lists(), 0);
        get("/repos/widget/issues/new1").await;
        assert_eq!(spy.lists(), 1);
        get("/repos/widget/issues/new2").await;
        assert_eq!(spy.lists(), 1);
    }

    #[cfg(feature = "http-rest")]
    #[tokio::test]
    async fn a_served_read_makes_one_get() {
        use tower::ServiceExt;
        const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Served, version: "1"}
paths:
  /customers/{customer}:
    get:
      operationId: getCustomer
      parameters: [{name: customer, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [id]
                properties: {id: {type: string}, offerId: {type: string}, customer: {type: string}}
"##;
        let spy = Arc::new(ListSpy {
            inner: MemoryStore::new(Duration::from_secs(3600)),
            gets: Default::default(),
            lists: Default::default(),
        });
        let rt = MockRuntime::new(
            "m",
            Arc::new(compile(CONTRACT.as_bytes()).unwrap()),
            Arc::new(Engine::new(None, EngineConfig::default())),
            spy.clone(),
        );
        let s = SessionKey {
            workspace: "default".into(),
            mock: "m".into(),
            session: DEFAULT_SESSION.into(),
        };
        // `customer` is the member the path names, so it holds the path's value.
        let data = json!({"id": "c1", "offerId": "of1", "customer": "c1"});
        let stored = StoredResource {
            resource_type: "customers".into(),
            id: "c1".into(),
            data,
        };
        spy.inner.upsert(&s, stored).await.unwrap();
        let request = axum::http::Request::get("/customers/c1")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = crate::http_rest::router(Arc::new(rt))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.headers()["x-mockingbird-source"], "state");
        assert_eq!(spy.gets(), 1);
        assert_eq!(spy.lists(), 0);
    }

    async fn read_by(rt: &MockRuntime, op: &Operation, number: &str) -> Option<String> {
        let mut state = StateAccess::new(rt);
        let (target, _, _) = read_context(
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
