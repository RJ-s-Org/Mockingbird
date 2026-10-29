//! Timing harness for the Admin API's hot paths (refinement P6). Every test
//! is `#[ignore]`d: run them with
//! `cargo test -p mockingbird-admin --release --test it perf:: -- --ignored --nocapture`
//! and compare the printed timings at N and 10N.
#![cfg(test)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use mb_admin::{AdminState, WebhookSink, router};
use mb_protocols::RequestRecord;
use mb_state::{MemoryStore, StateStore};
use mb_store::{
    AuditEvent, ContractStatus, ControlStore, DEFAULT_WORKSPACE, Mock, NewContractVersion, NewMock,
    NewRequestLogEntry, RequestKind, ResourceSource,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{body_partial_json, method};
use wiremock::{Mock as WireMock, MockServer, ResponseTemplate};

struct Harness {
    app: Router,
    store: Arc<dyn ControlStore>,
}

/// An in-memory SQLite store, or with `MB_PERF_POSTGRES_URL` set a Postgres
/// store in a fresh schema of that database (left behind for inspection).
async fn harness() -> Harness {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let url = match std::env::var("MB_PERF_POSTGRES_URL") {
        Ok(base) => {
            use sqlx::Connection;
            let schema = format!("perf_{}", uuid::Uuid::new_v4().simple());
            let mut conn = sqlx::PgConnection::connect(&base).await.unwrap();
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
                .execute(&mut conn)
                .await
                .unwrap();
            format!("{base}?options=-c%20search_path%3D{schema}")
        }
        Err(_) => "sqlite::memory:".to_string(),
    };
    let store = mb_store::connect(&url).await.unwrap();
    let app = router(AdminState::new(store.clone(), state));
    Harness { app, store }
}

impl Harness {
    async fn send(&self, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let resp = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }

    /// The mean time of `runs` calls of `method uri`, after one warm-up.
    async fn time(&self, method: Method, uri: &str, body: Option<Value>, runs: u32) -> Duration {
        let (status, value) = self.send(method.clone(), uri, body.clone()).await;
        assert!(status.is_success(), "{status} {value}");
        let start = Instant::now();
        for _ in 0..runs {
            self.send(method.clone(), uri, body.clone()).await;
        }
        start.elapsed() / runs
    }

    async fn mock(&self, name: &str) -> Mock {
        self.store
            .create_mock(
                DEFAULT_WORKSPACE,
                NewMock {
                    name: name.into(),
                    route_prefix: None,
                    persona: None,
                    llm_profile: None,
                    header_overrides: false,
                    source: ResourceSource::Api,
                    data_plane_auth: Default::default(),
                    chaos: None,
                    limits: Default::default(),
                    behavior: Default::default(),
                    hosts: vec![],
                    webhooks: vec![],
                },
            )
            .await
            .unwrap()
    }
}

/// An OpenAPI 3.1 JSON document with `ops` GET operations `op0…`, padded
/// with a description of `pad` bytes.
fn openapi(ops: usize, pad: usize) -> Vec<u8> {
    let mut paths = serde_json::Map::new();
    for i in 0..ops {
        paths.insert(
            format!("/r{i}"),
            json!({ "get": { "operationId": format!("op{i}"), "responses": {
                "200": { "description": "ok", "content": { "application/json": {
                    "schema": { "type": "object", "properties": { "id": { "type": "string" } } }
                } } }
            } } }),
        );
    }
    serde_json::to_vec(&json!({
        "openapi": "3.1.0",
        "info": { "title": "perf", "version": "1", "description": "x".repeat(pad) },
        "paths": paths,
    }))
    .unwrap()
}

fn version(source: &[u8], status: ContractStatus) -> NewContractVersion {
    NewContractVersion {
        source: source.to_vec(),
        bundle: mb_import::compile(source).unwrap(),
        status,
        created_by: None,
    }
}

fn report(what: &str, small: (usize, Duration), large: (usize, Duration)) {
    println!(
        "{what}: N={} {:?}, N={} {:?} (x{:.1})",
        small.0,
        small.1,
        large.0,
        large.1,
        large.1.as_secs_f64() / small.1.as_secs_f64()
    );
}

/// MB-ADMIN-PERF-2: listing mocks, each with a 256 KiB active contract.
#[tokio::test]
#[ignore = "timing harness"]
async fn perf_list_mocks() {
    let mut results = vec![];
    for n in [10, 100] {
        let h = harness().await;
        let source = openapi(20, 256 * 1024);
        for i in 0..n {
            let m = h.mock(&format!("m{i}")).await;
            h.store
                .add_contract_version(m.id, version(&source, ContractStatus::Active))
                .await
                .unwrap();
        }
        let t = h.time(Method::GET, "/api/v1/mocks", None, 10).await;
        results.push((n, t));
    }
    report("PERF-2 GET /mocks", results[0], results[1]);
}

/// MB-ADMIN-PERF-4: listing the versions of a mock with 256 KiB sources.
#[tokio::test]
#[ignore = "timing harness"]
async fn perf_list_contracts() {
    let mut results = vec![];
    for n in [5, 50] {
        let h = harness().await;
        let m = h.mock("m").await;
        for i in 0..n {
            let source = openapi(50, 256 * 1024 + i);
            h.store
                .add_contract_version(m.id, version(&source, ContractStatus::Approved))
                .await
                .unwrap();
        }
        let t = h
            .time(Method::GET, "/api/v1/mocks/m/contracts", None, 10)
            .await;
        results.push((n, t));
    }
    report("PERF-4 GET /contracts", results[0], results[1]);
}

/// MB-ADMIN-PERF-6: a page of `fields=summary` over 32 KiB bodies.
#[tokio::test]
#[ignore = "timing harness"]
async fn perf_request_summaries() {
    let mut results = vec![];
    for n in [50, 500] {
        let h = harness().await;
        let m = h.mock("m").await;
        let body = json!({ "items": (0..1000).map(|i| json!({ "n": i, "s": "abcdefghijklmnop" })).collect::<Vec<_>>() });
        for _ in 0..n {
            h.store
                .append_request_log(
                    NewRequestLogEntry {
                        mock_id: m.id,
                        operation_id: Some("op".into()),
                        session: "s".into(),
                        method: "GET".into(),
                        path: "/r".into(),
                        status: 200,
                        source: Some("llm".into()),
                        fallback_reason: None,
                        latency_ms: 1,
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        request: body.clone(),
                        response: Some(body.clone()),
                        at: chrono::Utc::now(),
                        kind: RequestKind::Http,
                    },
                    10_000,
                )
                .await
                .unwrap();
        }
        let t = h
            .time(
                Method::GET,
                "/api/v1/mocks/m/requests?fields=summary&limit=500",
                None,
                5,
            )
            .await;
        results.push((n, t));
    }
    report(
        "PERF-6 GET /requests?fields=summary",
        results[0],
        results[1],
    );
}

/// MB-ADMIN-PERF-7 and 8: endpoints that need only the active bundle,
/// against an active contract with a 1 MiB and a 10 MiB source.
#[tokio::test]
#[ignore = "timing harness"]
async fn perf_active_bundle_readers() {
    let mut draft = vec![];
    let mut channels = vec![];
    let mut scenario = vec![];
    for mib in [1, 10] {
        let h = harness().await;
        let m = h.mock("m").await;
        let active = openapi(20, mib * 1024 * 1024);
        h.store
            .add_contract_version(m.id, version(&active, ContractStatus::Active))
            .await
            .unwrap();
        let source = openapi(25, 0);
        h.store
            .add_draft_contract_version(
                m.id,
                version(&source, ContractStatus::Draft),
                json!({ "source_kind": "text", "chunk_count": 1, "chunks_processed": 1, "edited": false }),
            )
            .await
            .unwrap();
        draft.push((
            mib,
            h.time(Method::GET, "/api/v1/mocks/m/contracts/2/draft", None, 10)
                .await,
        ));
        channels.push((
            mib,
            h.time(Method::GET, "/api/v1/mocks/m/channels", None, 10)
                .await,
        ));
        scenario.push((
            mib,
            h.time(
                Method::PUT,
                "/api/v1/mocks/m/scenarios/s",
                Some(json!({ "operations": ["op1"] })),
                10,
            )
            .await,
        ));
    }
    report(
        "PERF-7 GET draft (MiB of active source)",
        draft[0],
        draft[1],
    );
    report("PERF-8 GET channels (MiB)", channels[0], channels[1]);
    report("PERF-8 PUT scenario (MiB)", scenario[0], scenario[1]);
}

/// MB-ADMIN-PERF-9: a scenario naming every operation of a contract with
/// N operations, with an instruction for each.
#[tokio::test]
#[ignore = "timing harness"]
async fn perf_scenario_validation() {
    let mut results = vec![];
    for n in [500, 5000] {
        let h = harness().await;
        let m = h.mock("m").await;
        h.store
            .add_contract_version(m.id, version(&openapi(n, 0), ContractStatus::Active))
            .await
            .unwrap();
        let ops: Vec<String> = (0..n).map(|i| format!("op{i}")).collect();
        let instructions: serde_json::Map<String, Value> =
            ops.iter().map(|o| (o.clone(), json!("be slow"))).collect();
        let body = json!({ "operations": ops, "operation_instructions": instructions });
        let t = h
            .time(Method::PUT, "/api/v1/mocks/m/scenarios/s", Some(body), 3)
            .await;
        results.push((n, t));
    }
    report("PERF-9 PUT scenario (operations)", results[0], results[1]);
}

/// MB-ADMIN-PERF-10: how long an event waits behind one whose endpoint
/// keeps failing (backoff base 100 ms, so 0.7 s of backoff in all).
#[tokio::test]
#[ignore = "timing harness"]
async fn perf_webhook_head_of_line() {
    let server = MockServer::start().await;
    WireMock::given(method("POST"))
        .and(body_partial_json(json!({ "id": 1 })))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    WireMock::given(method("POST"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let sink = WebhookSink::spawn_with(
        server.uri(),
        None,
        reqwest::Client::new(),
        Duration::from_millis(100),
    );
    let start = Instant::now();
    sink.send(event(1));
    sink.send(event(2));
    loop {
        let got = server.received_requests().await.unwrap_or_default();
        if got
            .iter()
            .any(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["id"] == 2)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    println!(
        "PERF-10 event 2 delivered after {:?} behind a failing event",
        start.elapsed()
    );
}

/// MB-ADMIN-PERF-5 (deferred to 0.2): what each open live tail pays per
/// served call, since `broadcast::recv` clones the whole record (bodies
/// included) for every subscriber before the workspace and mock filter.
#[tokio::test]
#[ignore = "timing harness"]
async fn perf_live_feed_fan_out() {
    let body = json!({ "items": (0..1000).map(|i| json!({ "n": i, "s": "abcdefghijklmnop" })).collect::<Vec<_>>() });
    let record = RequestRecord {
        workspace: "default".into(),
        mock: "m".into(),
        operation_id: Some("op".into()),
        session: "s".into(),
        method: "GET".into(),
        path: "/r".into(),
        status: 200,
        source: Some("llm".into()),
        fallback_reason: None,
        latency: Duration::from_millis(1),
        prompt_tokens: 1,
        completion_tokens: 1,
        request: body.clone(),
        request_body_bytes: None,
        response: Some(body),
        kind: mb_protocols::RequestKind::Http,
    };
    let mut results = vec![];
    for subscribers in [1, 10] {
        let (tx, _) = tokio::sync::broadcast::channel::<RequestRecord>(1024);
        let mut rxs: Vec<_> = (0..subscribers).map(|_| tx.subscribe()).collect();
        let runs = 200;
        let start = Instant::now();
        for _ in 0..runs {
            tx.send(record.clone()).unwrap();
            for rx in &mut rxs {
                rx.recv().await.unwrap();
            }
        }
        results.push((subscribers, start.elapsed() / runs));
    }
    report("PERF-5 one record to N subscribers", results[0], results[1]);
}

fn event(id: i64) -> AuditEvent {
    AuditEvent {
        id,
        workspace: Some("default".into()),
        actor_subject: "local".into(),
        actor_kind: "local".into(),
        action: "mock.create".into(),
        target: format!("mock:m{id}"),
        before: None,
        after: None,
        request_id: None,
        at: chrono::Utc::now(),
    }
}
