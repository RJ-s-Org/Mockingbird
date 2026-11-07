//! Helpers shared by the integration-test modules.

use std::future::Future;
#[cfg(any(
    feature = "grpc",
    feature = "graphql",
    feature = "soap",
    feature = "sse",
    feature = "http-rest"
))]
use std::sync::Mutex;
use std::time::Duration;

#[cfg(any(
    feature = "grpc",
    feature = "graphql",
    feature = "soap",
    feature = "sse",
    feature = "http-rest"
))]
use mb_protocols::{RequestRecord, RequestRecorder};

/// The header a create sends to take the generated path (`X-Mockingbird-Seed`): a
/// seeded call is not the state-first executor's, so the record the scripted model
/// answered is stored under the id it chose. The tests written before creates were
/// executed send it on their creates.
pub const SEED: (&str, &str) = ("x-mockingbird-seed", "1");

/// Polls `check` every `poll` until it returns `Ok`, and returns that value.
///
/// Panics once `timeout` has passed, with the message of the last `Err`, so a
/// condition that never holds fails the test with the state it last saw
/// instead of letting the test carry on with partial results.
pub async fn wait_until<T, F, Fut>(timeout: Duration, poll: Duration, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let last = match check().await {
            Ok(value) => return value,
            Err(last) => last,
        };
        if tokio::time::Instant::now() >= deadline {
            panic!("condition not met within {timeout:?}: {last}");
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(any(
    feature = "grpc",
    feature = "graphql",
    feature = "soap",
    feature = "sse",
    feature = "http-rest"
))]
/// A [`RequestRecorder`] that keeps every record in memory.
#[derive(Default)]
pub struct Records(pub Mutex<Vec<RequestRecord>>);

#[cfg(any(
    feature = "grpc",
    feature = "graphql",
    feature = "soap",
    feature = "sse",
    feature = "http-rest"
))]
impl RequestRecorder for Records {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

#[cfg(feature = "graphql")]
impl Records {
    /// Waits up to 2 s for at least `n` records and returns them all.
    pub async fn wait(&self, n: usize) -> Vec<RequestRecord> {
        wait_until(
            Duration::from_secs(2),
            Duration::from_millis(20),
            || async {
                let got = self.0.lock().unwrap().clone();
                if got.len() >= n {
                    Ok(got)
                } else {
                    Err(format!(
                        "wanted {n} request records, got {}: {got:?}",
                        got.len()
                    ))
                }
            },
        )
        .await
    }
}

/// Waits up to 5 s until `broker` has at least `n` subscriptions.
///
/// [`MemoryBroker::inject`] reaches only the subscribers that exist when it
/// runs, so a test injects only once the consumers it expects are in place.
///
/// [`MemoryBroker::inject`]: mb_protocols::events::MemoryBroker::inject
#[cfg(feature = "events")]
pub async fn subscribed(broker: &mb_protocols::events::MemoryBroker, n: usize) {
    wait_until(Duration::from_secs(5), Duration::from_millis(2), || async {
        let groups = broker.subscribed_groups();
        if groups.len() >= n {
            Ok(())
        } else {
            Err(format!("wanted {n} subscriptions, got {groups:?}"))
        }
    })
    .await;
}

/// Waits up to 5 s until two readings of `count` taken `poll` apart agree,
/// and returns that count.
///
/// For a producer that should stop: once the count holds still, a test can
/// bound it and then check that it stays put.
#[cfg(any(feature = "graphql", feature = "sse"))]
pub async fn stable_count(poll: Duration, count: impl Fn() -> usize) -> usize {
    let mut last = count();
    // `wait_until` checks at once: the first reading to compare is `poll` on.
    tokio::time::sleep(poll).await;
    wait_until(Duration::from_secs(5), poll, || {
        let now = count();
        let previous = std::mem::replace(&mut last, now);
        async move {
            if now == previous {
                Ok(now)
            } else {
                Err(format!("still moving: {previous} -> {now}"))
            }
        }
    })
    .await
}

/// A runtime built from a contract and a world description document the way a served
/// mock builds it, with a scripted LLM, a request recorder and its REST router.
#[cfg(feature = "http-rest")]
pub struct DescribedHarness {
    pub rt: std::sync::Arc<mb_protocols::MockRuntime>,
    pub provider: std::sync::Arc<mb_llm::ScriptedProvider>,
    pub recorded: std::sync::Arc<Records>,
    router: axum::Router,
}

/// How a [`DescribedHarness`] is built.
#[cfg(feature = "http-rest")]
#[derive(Default)]
pub struct HarnessOptions {
    /// Wrap the state store in a response cache, as a served mock does.
    pub cached: bool,
    /// Answer a read of an id the session does not hold with its `404`.
    pub unknown_ids_not_found: bool,
}

/// [`described`]'s runtime, with `configure` applied to the bare runtime before it is
/// served: the one constructor every other builds on.
#[cfg(feature = "http-rest")]
pub fn harness(
    contract: &str,
    script: Vec<serde_json::Value>,
    options: &HarnessOptions,
    configure: impl FnOnce(mb_protocols::MockRuntime) -> mb_protocols::MockRuntime,
) -> DescribedHarness {
    let bundle = mb_import::compile(contract.as_bytes()).unwrap();
    harness_of(bundle, script, options, configure)
}

/// [`harness`] over a compiled contract.
#[cfg(feature = "http-rest")]
pub fn harness_of(
    bundle: mb_mcm::Bundle,
    script: Vec<serde_json::Value>,
    options: &HarnessOptions,
    configure: impl FnOnce(mb_protocols::MockRuntime) -> mb_protocols::MockRuntime,
) -> DescribedHarness {
    use std::sync::Arc;

    use mb_engine::{Engine, EngineConfig};
    use mb_llm::{Provider, ScriptedProvider};
    use mb_state::{CachingStateStore, MemoryStore, StateStore};

    let provider = Arc::new(ScriptedProvider::new(
        script.into_iter().map(|v| Ok(v.to_string())).collect(),
    ));
    let mut store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    if options.cached {
        store = CachingStateStore::wrap(
            store,
            Duration::from_secs(3600),
            mb_state::DEFAULT_CACHE_ENTRIES,
        );
    }
    let mut rt = mb_protocols::MockRuntime::new(
        "described",
        Arc::new(bundle),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        store,
    );
    if options.unknown_ids_not_found {
        rt.unknown_ids = mb_protocols::UnknownIds::NotFound;
    }
    let recorded = Arc::new(Records::default());
    rt.recorder = Some(Arc::clone(&recorded) as Arc<dyn RequestRecorder>);
    let rt = Arc::new(configure(rt));
    DescribedHarness {
        router: mb_protocols::http_rest::router(Arc::clone(&rt)),
        rt,
        provider,
        recorded,
    }
}

/// The effective world of `world`, a revision document, over `bundle`'s tier 0, as the
/// runtime computes it: `(base, effective, refusals)`.
#[cfg(feature = "http-rest")]
pub fn effective_world(
    bundle: &mb_mcm::Bundle,
    world: serde_json::Value,
) -> (
    mb_mcm::world::World,
    mb_mcm::world::World,
    Vec<mb_mcm::world::Refusal>,
) {
    let doc: mb_mcm::world::World = serde_json::from_value(world).unwrap();
    let base = mb_mcm::world::tier0(bundle);
    let (effective, refused) = mb_mcm::world::effective(bundle, &base, &doc);
    (base, effective, refused)
}

/// A runtime of `contract` serving `world` (a revision document, as an author writes
/// it) at revision 1, through its effective world over tier 0. Panics when the
/// effective world refuses any entry, so a test whose world has a typo fails instead
/// of testing tier 0; [`described_allowing_refusals`] does not.
#[cfg(feature = "http-rest")]
pub fn described(
    contract: &str,
    world: serde_json::Value,
    script: Vec<serde_json::Value>,
) -> DescribedHarness {
    described_with(contract, world, script, &HarnessOptions::default(), false)
}

/// [`described`], without the refusal check.
#[cfg(feature = "http-rest")]
pub fn described_allowing_refusals(
    contract: &str,
    world: serde_json::Value,
    script: Vec<serde_json::Value>,
) -> DescribedHarness {
    described_with(contract, world, script, &HarnessOptions::default(), true)
}

/// [`described`], then each operation in `gaps` given those static gaps.
#[cfg(feature = "http-rest")]
pub fn described_with_gaps(
    contract: &str,
    world: serde_json::Value,
    script: Vec<serde_json::Value>,
    gaps: std::collections::HashMap<String, Vec<mb_protocols::world_gaps::GapCode>>,
) -> DescribedHarness {
    described_with_gaps_and(contract, world, script, gaps, &HarnessOptions::default())
}

/// [`described_with_gaps`], built with `options`.
#[cfg(feature = "http-rest")]
pub fn described_with_gaps_and(
    contract: &str,
    world: serde_json::Value,
    script: Vec<serde_json::Value>,
    gaps: std::collections::HashMap<String, Vec<mb_protocols::world_gaps::GapCode>>,
    options: &HarnessOptions,
) -> DescribedHarness {
    harness(contract, script, options, |rt| {
        with_effective(rt, world, false).with_test_gaps(gaps)
    })
}

/// [`described`], built with `options`.
#[cfg(feature = "http-rest")]
pub fn described_with(
    contract: &str,
    world: serde_json::Value,
    script: Vec<serde_json::Value>,
    options: &HarnessOptions,
    allow_refusals: bool,
) -> DescribedHarness {
    harness(contract, script, options, |rt| {
        with_effective(rt, world, allow_refusals)
    })
}

/// `rt` serving `world` (a revision document) at revision 1 through its effective
/// world over tier 0, as [`described`] builds it; panics on a refused entry unless
/// `allow_refusals`.
#[cfg(feature = "http-rest")]
pub fn with_effective(
    rt: mb_protocols::MockRuntime,
    world: serde_json::Value,
    allow_refusals: bool,
) -> mb_protocols::MockRuntime {
    let (base, effective, refused) = effective_world(rt.bundle(), world);
    assert!(
        allow_refusals || refused.is_empty(),
        "the test world has refused entries: {refused:?}"
    );
    rt.with_world(&base, &effective, 1)
}

#[cfg(feature = "http-rest")]
impl DescribedHarness {
    /// Sends one REST request; answers its status, headers and JSON body (`null` when
    /// empty, a string when not JSON).
    pub async fn send(
        &self,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> (
        axum::http::StatusCode,
        axum::http::HeaderMap,
        serde_json::Value,
    ) {
        let mut req = axum::http::Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let body = body.map_or_else(axum::body::Body::empty, |b| {
            axum::body::Body::from(b.to_string())
        });
        send_to(&self.router, req.body(body).unwrap()).await
    }

    /// [`DescribedHarness::send`] of a create, seeded ([`SEED`]): it takes the
    /// generated path, which stores the record the model answered under the id it chose.
    pub async fn seeded(
        &self,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> (
        axum::http::StatusCode,
        axum::http::HeaderMap,
        serde_json::Value,
    ) {
        let mut headers = headers.to_vec();
        headers.push(SEED);
        self.send(method, uri, body, &headers).await
    }

    /// Every request record so far.
    pub fn records(&self) -> Vec<RequestRecord> {
        self.recorded.0.lock().unwrap().clone()
    }

    /// Stores `records` of type `t` in the default session, in order, each under its
    /// `id`: what earlier writes left there.
    pub async fn hold(&self, t: &str, records: &[serde_json::Value]) {
        self.hold_by(t, "id", records).await;
    }

    /// [`DescribedHarness::hold`], each record under its member `key` (text or a number).
    pub async fn hold_by(&self, t: &str, key: &str, records: &[serde_json::Value]) {
        let session = mb_state::SessionKey {
            workspace: self.rt.workspace.clone(),
            mock: self.rt.name.clone(),
            session: "default".into(),
        };
        for record in records {
            let resource = mb_state::StoredResource {
                resource_type: t.to_string(),
                id: match &record[key] {
                    serde_json::Value::String(id) => id.clone(),
                    other => other.to_string(),
                },
                data: record.clone(),
            };
            self.rt.state.upsert(&session, resource).await.unwrap();
        }
    }

    /// [`Self::send`] with a JSON body sent as `content_type`.
    #[allow(dead_code, reason = "used by the exec test binaries, not by `it`")]
    pub async fn send_as(
        &self,
        method: &str,
        uri: &str,
        body: serde_json::Value,
        content_type: &str,
    ) -> (
        axum::http::StatusCode,
        axum::http::HeaderMap,
        serde_json::Value,
    ) {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", content_type)
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        send_to(&self.router, req).await
    }

    /// The record of type `t` stored under `id` in the default session.
    pub async fn stored(&self, t: &str, id: &str) -> Option<serde_json::Value> {
        let session = mb_state::SessionKey {
            workspace: self.rt.workspace.clone(),
            mock: self.rt.name.clone(),
            session: "default".into(),
        };
        self.rt
            .state
            .get(&session, t, id)
            .await
            .unwrap()
            .map(|r| r.data)
    }
}

/// Sends `req` to `router`; answers its status, headers and body (JSON, else a string,
/// `null` when empty).
#[cfg(feature = "http-rest")]
pub async fn send_to(
    router: &axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (
    axum::http::StatusCode,
    axum::http::HeaderMap,
    serde_json::Value,
) {
    use tower::ServiceExt;
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, headers, body)
}
