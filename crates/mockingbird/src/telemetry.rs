//! Process-wide logging, tracing export and the Prometheus recorder.

use std::sync::Mutex;

use anyhow::{Context, anyhow};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Turns on OTLP span export when set (the standard OpenTelemetry variable).
pub const OTLP_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

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
/// `RUST_LOG` (default `info`) and, when [`OTLP_ENDPOINT_ENV`] is set, an
/// OpenTelemetry layer exporting spans over OTLP HTTP/protobuf. Fails when
/// a global subscriber is already installed.
pub fn init(service_name: &str) -> anyhow::Result<TelemetryGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let endpoint = std::env::var(OTLP_ENDPOINT_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());
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

static PROMETHEUS: Mutex<Option<PrometheusHandle>> = Mutex::new(None);

/// Installs the global Prometheus `metrics` recorder and returns a handle
/// that renders the text exposition. Later calls return the same handle.
/// Fails when another global `metrics` recorder is installed.
pub fn install_prometheus() -> anyhow::Result<PrometheusHandle> {
    let mut installed = PROMETHEUS
        .lock()
        .map_err(|_| anyhow!("Prometheus recorder lock poisoned"))?;
    if let Some(handle) = installed.as_ref() {
        return Ok(handle.clone());
    }
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder)
        .map_err(|e| anyhow!("installing the Prometheus recorder: {e}"))?;
    *installed = Some(handle.clone());
    Ok(handle)
}
