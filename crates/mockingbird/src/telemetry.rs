//! Process-wide logging, tracing export and the Prometheus recorder.

use std::sync::Mutex;

use anyhow::{Context, anyhow};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Turns on OTLP span export when set (the standard OpenTelemetry variable).
pub const OTLP_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// The traces-only endpoint variable, which also turns on OTLP span
/// export (and wins over [`OTLP_ENDPOINT_ENV`] for spans).
pub const OTLP_TRACES_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT";

/// The protocol variables: the exporter speaks HTTP/protobuf only, so a
/// `grpc` value is warned about.
const OTLP_PROTOCOL_ENVS: [&str; 2] = [
    "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
    "OTEL_EXPORTER_OTLP_PROTOCOL",
];

/// Keeps span export running; dropping it flushes and shuts the exporter down.
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
}

impl TelemetryGuard {
    /// Whether spans are exported over OTLP.
    pub fn otlp_enabled(&self) -> bool {
        self.provider.is_some()
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take()
            && let Err(error) = provider.shutdown()
        {
            eprintln!("cannot flush OTLP spans: {error}");
        }
    }
}

/// Installs the global `tracing` subscriber: a stderr fmt layer filtered by
/// `RUST_LOG` (default `info`) and, when [`OTLP_TRACES_ENDPOINT_ENV`] or
/// [`OTLP_ENDPOINT_ENV`] is set, an OpenTelemetry layer exporting spans
/// over OTLP HTTP/protobuf (a `grpc` protocol variable is warned about and
/// not followed). Fails when a global subscriber is already installed.
pub fn init(service_name: &str) -> anyhow::Result<TelemetryGuard> {
    let env = |name: &str| std::env::var(name).ok();
    let guard = init_with(service_name, otlp_endpoint(env))?;
    if guard.otlp_enabled()
        && let Some(name) = grpc_protocol(env)
    {
        tracing::warn!(
            "{name}=grpc is not supported: spans are exported over OTLP HTTP/protobuf (point the endpoint at the collector's HTTP port, usually 4318)"
        );
    }
    Ok(guard)
}

/// The endpoint that turns on span export: the traces-only variable, else
/// the general one (blank values count as unset).
fn otlp_endpoint(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    [OTLP_TRACES_ENDPOINT_ENV, OTLP_ENDPOINT_ENV]
        .into_iter()
        .filter_map(env)
        .find(|v| !v.trim().is_empty())
}

/// The protocol variable that asks for gRPC, if one does: the traces-only
/// one when set, else the general one.
fn grpc_protocol(env: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    let (name, value) = OTLP_PROTOCOL_ENVS.into_iter().find_map(|name| {
        env(name)
            .filter(|v| !v.trim().is_empty())
            .map(|v| (name, v))
    })?;
    value.trim().eq_ignore_ascii_case("grpc").then_some(name)
}

/// [`init`] with the value of [`OTLP_ENDPOINT_ENV`] given rather than read
/// (`None` or blank: no OTLP export). The exporter itself still reads the
/// standard `OTEL_EXPORTER_OTLP_*` variables.
pub fn init_with(
    service_name: &str,
    otlp_endpoint: Option<String>,
) -> anyhow::Result<TelemetryGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Colours only on a terminal: a background server's stderr is a log
    // file that agents read as text.
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()));

    let endpoint = otlp_endpoint.filter(|v| !v.trim().is_empty());
    let provider = match endpoint {
        Some(_) => Some(otlp_provider(service_name)?),
        None => None,
    };
    let otel = provider.as_ref().map(|p| {
        tracing_opentelemetry::layer()
            .with_tracer(p.tracer(service_name.to_string()))
            .boxed()
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt)
        .with(otel)
        .try_init()
        .context("installing the tracing subscriber")?;
    if let Some(p) = &provider {
        opentelemetry::global::set_tracer_provider(p.clone());
    }
    Ok(TelemetryGuard { provider })
}

/// A tracer provider batching spans to the OTLP endpoint named by the
/// standard `OTEL_EXPORTER_OTLP_*` variables.
fn otlp_provider(service_name: &str) -> anyhow::Result<SdkTracerProvider> {
    // The exporter's blocking HTTP client must not be built on an async
    // runtime thread, so it is built on a plain thread.
    let exporter = std::thread::spawn(|| {
        SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
    })
    .join()
    .map_err(|_| anyhow!("building the OTLP exporter panicked"))?
    .context("building the OTLP span exporter")?;
    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_service_name(service_name.to_string())
                .build(),
        )
        .build())
}

/// The request latency metric, exported as a Prometheus histogram.
pub const REQUEST_DURATION_METRIC: &str = "mb_request_duration_seconds";

/// Bucket bounds (seconds) of [`REQUEST_DURATION_METRIC`].
pub const REQUEST_DURATION_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

static PROMETHEUS: Mutex<Option<PrometheusHandle>> = Mutex::new(None);

/// Installs the global Prometheus `metrics` recorder and returns a handle
/// that renders the text exposition. [`REQUEST_DURATION_METRIC`] is a
/// histogram with [`REQUEST_DURATION_BUCKETS`] (other histograms are
/// summaries). Later calls return the same handle.
/// Fails when another global `metrics` recorder is installed.
pub fn install_prometheus() -> anyhow::Result<PrometheusHandle> {
    let mut installed = PROMETHEUS
        .lock()
        .map_err(|_| anyhow!("Prometheus recorder lock poisoned"))?;
    if let Some(handle) = installed.as_ref() {
        return Ok(handle.clone());
    }
    let recorder = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full(REQUEST_DURATION_METRIC.to_string()),
            &REQUEST_DURATION_BUCKETS,
        )
        .map_err(|e| anyhow!("configuring Prometheus histogram buckets: {e}"))?
        .build_recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder)
        .map_err(|e| anyhow!("installing the Prometheus recorder: {e}"))?;
    *installed = Some(handle.clone());
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    /// MOCKINGBIRD-STUB-10: the traces-only endpoint turns export on too.
    #[test]
    fn either_endpoint_variable_enables_export() {
        assert_eq!(otlp_endpoint(env(&[])), None);
        assert_eq!(otlp_endpoint(env(&[(OTLP_ENDPOINT_ENV, " ")])), None);
        assert_eq!(
            otlp_endpoint(env(&[(
                OTLP_TRACES_ENDPOINT_ENV,
                "http://c:4318/v1/traces"
            )])),
            Some("http://c:4318/v1/traces".into())
        );
        assert_eq!(
            otlp_endpoint(env(&[(OTLP_ENDPOINT_ENV, "http://c:4318")])),
            Some("http://c:4318".into())
        );
    }

    /// MOCKINGBIRD-STUB-10: a grpc protocol is detected (to be warned
    /// about), the traces-only variable taking precedence.
    #[test]
    fn a_grpc_protocol_is_detected() {
        let general = "OTEL_EXPORTER_OTLP_PROTOCOL";
        let traces = "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL";
        assert_eq!(grpc_protocol(env(&[])), None);
        assert_eq!(grpc_protocol(env(&[(general, "http/protobuf")])), None);
        assert_eq!(grpc_protocol(env(&[(general, "gRPC")])), Some(general));
        assert_eq!(
            grpc_protocol(env(&[(general, "grpc"), (traces, "http/protobuf")])),
            None
        );
        assert_eq!(
            grpc_protocol(env(&[(general, "http/protobuf"), (traces, "grpc")])),
            Some(traces)
        );
    }
}
