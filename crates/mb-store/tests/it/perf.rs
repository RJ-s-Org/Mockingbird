//! Timing harness for the store's hot paths (refinement P8). Every test is
//! `#[ignore]`d: run them with
//! `cargo test -p mockingbird-store --release --test it perf:: -- --ignored --nocapture --test-threads 1`
//! and compare the printed timings at N and 10N.
#![cfg(test)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use mb_mcm::Bundle;
use mb_store::{
    ContractStatus, ControlStore, DEFAULT_WORKSPACE, Mock, NewContractVersion, NewMock,
    NewRequestLogEntry, RequestKind, ResourceSource,
};
use serde_json::json;
use uuid::Uuid;

fn new_mock(name: &str) -> NewMock {
    NewMock {
        name: name.to_string(),
        route_prefix: None,
        persona: None,
        llm_profile: None,
        header_overrides: false,
        source: ResourceSource::Api,
        chaos: None,
        limits: Default::default(),
        data_plane_auth: Default::default(),
        behavior: Default::default(),
        hosts: vec![],
        webhooks: Vec::new(),
    }
}

async fn mock(store: &Arc<dyn ControlStore>, name: &str) -> Mock {
    store
        .create_mock(DEFAULT_WORKSPACE, new_mock(name))
        .await
        .expect("create mock")
}

/// A bundle with `ops` operations of gRPC service `pkg.<title>`, each with
/// a small body schema (about 0.4 KiB per operation).
fn bundle(title: &str, ops: usize) -> Bundle {
    let operations: Vec<_> = (0..ops)
        .map(|i| {
            json!({
                "id": format!("Get{i}"),
                "protocol": "grpc",
                "binding": { "grpc": { "package": "pkg", "service": title, "method": format!("Get{i}") } },
                "kind": "query",
                "summary": format!("Reads thing {i}"),
                "input": { "path": null, "query": null, "headers": null, "body": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "format": "uuid" },
                        "name": { "type": "string", "maxLength": 64 },
                        "count": { "type": "integer", "minimum": 0 },
                    },
                    "required": ["id"],
                } },
                "outputs": [],
                "resource_type": null,
                "identity": null,
                "semantic_hints": [],
            })
        })
        .collect();
    serde_json::from_value(json!({
        "id": title,
        "title": title,
        "version": "1.0.0",
        "source_digest": "abc",
        "operations": operations,
        "defs": {},
    }))
    .expect("bundle")
}

fn version(title: &str, ops: usize, status: ContractStatus) -> NewContractVersion {
    let bundle = bundle(title, ops);
    NewContractVersion {
        source: serde_json::to_vec(&bundle).expect("source"),
        bundle,
        status,
        created_by: None,
    }
}

async fn time<F, Fut>(runs: u32, mut f: F) -> Duration
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    f().await;
    let start = Instant::now();
    for _ in 0..runs {
        f().await;
    }
    start.elapsed() / runs
}

/// PERF-1: listing a mock's versions for the console, full rows against
/// the info query (no source or bundle).
#[tokio::test]
#[ignore = "timing harness"]
async fn perf_version_list() {
    for n in [10, 100] {
        let store = mb_store::connect("sqlite::memory:").await.expect("connect");
        let m = mock(&store, "versions").await;
        for i in 0..n {
            store
                .add_contract_version(
                    m.id,
                    version(&format!("v{i}"), 200, ContractStatus::Approved),
                )
                .await
                .expect("add");
        }
        let full = time(20, || async {
            store.list_contract_versions(m.id).await.expect("list");
        })
        .await;
        let infos = time(20, || async {
            store
                .list_contract_version_infos(m.id)
                .await
                .expect("infos");
        })
        .await;
        println!("PERF-1 {n} versions x 200 operations: full {full:?}, infos {infos:?}");
    }
}

/// PERF-2: every mock's active bundle (the gRPC conflict check and config
/// reconcile), one query per mock against the bulk query.
#[tokio::test]
#[ignore = "timing harness"]
async fn perf_active_bundles() {
    for n in [50, 500] {
        let store = mb_store::connect("sqlite::memory:").await.expect("connect");
        for i in 0..n {
            let m = mock(&store, &format!("m{i}")).await;
            store
                .add_contract_version(m.id, version(&format!("S{i}"), 10, ContractStatus::Active))
                .await
                .expect("add");
        }
        let per_mock = time(10, || async {
            let mut services = 0;
            for m in store.list_all_mocks().await.expect("mocks") {
                if let Some(b) = store.active_bundle(m.id).await.expect("bundle") {
                    services += b.bundle.grpc_services().len();
                }
            }
            assert_eq!(services, n);
        })
        .await;
        let bulk = time(10, || async {
            let ids: Vec<Uuid> = store
                .list_all_mocks()
                .await
                .expect("mocks")
                .iter()
                .map(|m| m.id)
                .collect();
            let bundles = store.active_bundles(&ids).await.expect("bundles");
            let services: usize = bundles
                .values()
                .map(|b| b.bundle.grpc_services().len())
                .sum();
            assert_eq!(services, n);
        })
        .await;
        println!("PERF-2 {n} mocks x 10 operations: per mock {per_mock:?}, bulk {bulk:?}");
    }
}

fn log_entry(mock_id: Uuid) -> NewRequestLogEntry {
    NewRequestLogEntry {
        mock_id,
        operation_id: Some("op".into()),
        session: "s".into(),
        method: "GET".into(),
        path: "/orders".into(),
        status: 200,
        source: Some("faker".into()),
        fallback_reason: None,
        latency_ms: 1,
        prompt_tokens: 0,
        completion_tokens: 0,
        request: json!({"q": 1}),
        response: Some(json!({"ok": true})),
        at: Utc::now(),
        kind: RequestKind::Http,
    }
}

/// PERF-7: request-log appends at a steady state of `retention` rows (one
/// prune every `RETENTION_PRUNE_INTERVAL` appends), with another mock's
/// rows in the table too.
#[tokio::test]
#[ignore = "timing harness"]
async fn perf_request_log_prune() {
    for retention in [10_000u32, 100_000] {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}", dir.path().join("control.db").display());
        let store = mb_store::connect(&url).await.expect("connect");
        let m = mock(&store, "logged").await;
        let other = mock(&store, "other").await;
        let pool = sqlx::SqlitePool::connect(&url).await.expect("pool");
        for (id, rows) in [(other.id, retention), (m.id, retention)] {
            sqlx::query(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?2) \
                 INSERT INTO request_log (mock_id, operation_id, session, method, path, status, source, \
                 latency_ms, prompt_tokens, completion_tokens, request, response, at) \
                 SELECT ?1, 'op', 's', 'GET', '/orders', 200, 'faker', 1, 0, 0, '{}', '{}', \
                 '2026-01-01T00:00:00Z' FROM n",
            )
            .bind(id.to_string())
            .bind(i64::from(rows))
            .execute(&pool)
            .await
            .expect("fill");
        }
        pool.close().await;
        let runs = 5u32;
        let start = Instant::now();
        for _ in 0..runs * 100 {
            store
                .append_request_log(log_entry(m.id), retention)
                .await
                .expect("append");
        }
        let per_100 = start.elapsed() / runs;
        println!("PERF-7 retention {retention}: 100 appends (one prune) {per_100:?}");
    }
}
