//! The offline throughput bench: one zero-config mock served by the real
//! server stack, driven over loopback HTTP.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use mb_mcm::{Binding, HttpMethod};
use mockingbird::config::{ServeArgs, build_app};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::report::percentile;

/// What one bench run measured.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct BenchReport {
    pub requests: u64,
    /// Transport failures and 5xx responses.
    pub errors: u64,
    pub rps: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub concurrency: usize,
    pub duration_s: f64,
}

/// Value substituted for every `{param}` of a request path.
const PATH_PARAM_VALUE: &str = "1";

/// The per-request timeout of the bench client.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One request the bench sends repeatedly.
#[derive(Clone, Debug)]
struct Plan {
    post: bool,
    path: String,
    body: Option<Value>,
}

/// The non-streaming HTTP GET and POST operations of `bundle`, with every
/// path parameter set to [`PATH_PARAM_VALUE`] and a faker-built body.
fn plans(bundle: &mb_mcm::Bundle) -> Vec<Plan> {
    let faker = mb_engine::Engine::new(None, mb_engine::EngineConfig::default());
    bundle
        .operations
        .iter()
        .filter(|op| !op.kind.streams_request() && !op.kind.streams_response())
        .filter_map(|op| {
            let Binding::Http { method, path } = &op.binding else {
                return None;
            };
            let post = match method {
                HttpMethod::Get => false,
                HttpMethod::Post => true,
                _ => return None,
            };
            let body = if post {
                op.input.body.as_ref().map(|s| faker.fake(bundle, s))
            } else {
                None
            };
            Some(Plan {
                post,
                path: fill_path(path),
                body,
            })
        })
        .collect()
}

/// `path` with every `{param}` replaced by [`PATH_PARAM_VALUE`].
fn fill_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        match rest[open..].find('}') {
            Some(close) => {
                out.push_str(PATH_PARAM_VALUE);
                rest = &rest[open + close + 1..];
            }
            None => {
                rest = &rest[open..];
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Serves `contract` offline (faker only, SQLite in memory) on
/// `127.0.0.1:0` and drives its GET and POST operations round-robin from
/// `concurrency` workers for `duration`. A transport failure or a 5xx
/// response counts as an error; latencies cover every request.
pub async fn bench(
    contract: &Path,
    duration: Duration,
    concurrency: usize,
) -> anyhow::Result<BenchReport> {
    let concurrency = concurrency.max(1);
    let source = mb_import::read_source(contract)
        .with_context(|| format!("cannot read {}", contract.display()))?;
    let bundle = mb_import::compile(&source)
        .with_context(|| format!("cannot compile {}", contract.display()))?;
    let plans = Arc::new(plans(&bundle));
    if plans.is_empty() {
        return Err(anyhow!(
            "{} has no HTTP GET or POST operation to bench",
            contract.display()
        ));
    }

    let args = ServeArgs {
        contracts: vec![contract.to_path_buf()],
        offline: true,
        storage_url: Some("sqlite::memory:".into()),
        ..ServeArgs::default()
    };
    let app = build_app(&args, None)
        .await
        .context("starting the offline server")?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding 127.0.0.1:0")?;
    let base = format!("http://{}", listener.local_addr()?);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(
        axum::serve(listener, app.data.clone())
            .with_graceful_shutdown(async {
                let _ = stop_rx.await;
            })
            .into_future(),
    );

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building the HTTP client")?;
    let started = Instant::now();
    let deadline = started + duration;
    let mut workers = Vec::with_capacity(concurrency);
    for worker in 0..concurrency {
        let client = client.clone();
        let plans = Arc::clone(&plans);
        let base = base.clone();
        workers.push(tokio::spawn(async move {
            let mut latencies = Vec::new();
            let mut errors = 0u64;
            let mut next = worker;
            while Instant::now() < deadline {
                let plan = &plans[next % plans.len()];
                next += 1;
                let url = format!("{base}{}", plan.path);
                let request = if plan.post {
                    let req = client.post(&url);
                    match &plan.body {
                        Some(body) => req.json(body),
                        None => req,
                    }
                } else {
                    client.get(&url)
                };
                let sent = Instant::now();
                let ok = match request.send().await {
                    Ok(response) => {
                        let server_error = response.status().is_server_error();
                        response.bytes().await.is_ok() && !server_error
                    }
                    Err(_) => false,
                };
                latencies.push(sent.elapsed().as_secs_f64() * 1000.0);
                if !ok {
                    errors += 1;
                }
            }
            (latencies, errors)
        }));
    }

    let mut latencies = Vec::new();
    let mut errors = 0;
    for worker in workers {
        let (l, e) = worker.await.context("bench worker panicked")?;
        latencies.extend(l);
        errors += e;
    }
    let elapsed = started.elapsed().as_secs_f64();

    let _ = stop_tx.send(());
    match server.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "bench server failed"),
        Err(e) => tracing::warn!(error = %e, "bench server task failed"),
    }
    app.shutdown().await;

    latencies.sort_by(f64::total_cmp);
    let requests = latencies.len() as u64;
    Ok(BenchReport {
        requests,
        errors,
        rps: if elapsed > 0.0 {
            requests as f64 / elapsed
        } else {
            0.0
        },
        p50_ms: percentile(&latencies, 0.50),
        p95_ms: percentile(&latencies, 0.95),
        p99_ms: percentile(&latencies, 0.99),
        concurrency,
        duration_s: elapsed,
    })
}

#[cfg(test)]
mod tests {
    use super::fill_path;

    #[test]
    fn path_params_are_filled() {
        assert_eq!(fill_path("/orders/{orderId}"), "/orders/1");
        assert_eq!(fill_path("/a/{x}/b/{y}"), "/a/1/b/1");
        assert_eq!(fill_path("/plain"), "/plain");
        assert_eq!(fill_path("/broken/{x"), "/broken/{x");
    }
}
