//! Usage and request-log reporting, and which headers are recorded.

use super::*;

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

pub(super) async fn report_with(
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
pub(super) fn record(rt: &MockRuntime, r: Report) {
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
pub(super) fn request_kind(protocol: Protocol) -> RequestKind {
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
pub(super) fn safe_headers(
    rt: &MockRuntime,
    headers: &HeaderMap,
    keep_overrides: bool,
) -> Map<String, Value> {
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
