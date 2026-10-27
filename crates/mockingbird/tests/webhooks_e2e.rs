//! A served mock with `webhooks:` delivers signed events to a local
//! receiver; an unresolvable secret keeps the mock serving.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mb_engine::StreamConfig;
use mb_ext::Extensions;
use mb_protocols::events::EventDeps;
use mb_protocols::webhooks::log::{DeliveryRecord, DeliveryStatus, SharedDeliveryLog};
use mb_protocols::webhooks::sign::stripe_signature;
use mb_protocols::{ProtocolSettings, http_rest};
use mb_state::{MemoryStore, StateStore};
use mb_store::{
    ContractStatus, ControlStore, DEFAULT_WORKSPACE, NewContractVersion, NewMock, ResourceSource,
    SqliteControlStore, WebhookSigning, WebhookTarget,
};
use mockingbird::runtime::{RuntimeDeps, ServeMode, build_runtime};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Set for this test process only; a throwaway value.
const SECRET_VAR: &str = "WS6_TEST_WEBHOOK_SECRET";
const UNSET_VAR: &str = "WS6_UNSET_WEBHOOK_SECRET";

fn deps(store: Arc<dyn ControlStore>, log: SharedDeliveryLog) -> RuntimeDeps {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    RuntimeDeps {
        store,
        state,
        ext: Extensions::default(),
        recorder: None,
        offline: true,
        default_profile: None.into(),
        data_plane_keys: None,
        protocols: ProtocolSettings::default(),
        stream: StreamConfig::default(),
        events: EventDeps::default(),
        limiter: mb_engine::GenerationLimiter::new(64),
        scrubber: None,
        limits: Default::default(),
        http: Default::default(),
        mode: ServeMode::Full,
        webhook_log: log,
    }
}

/// Serves an orders mock whose one webhook target is `server` with
/// `secret`, and returns its base URL and the delivery log.
async fn serve(server: &MockServer, secret: &str) -> (String, SharedDeliveryLog) {
    let store: Arc<dyn ControlStore> = Arc::new(
        SqliteControlStore::connect("sqlite::memory:")
            .await
            .unwrap(),
    );
    let mock = store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: "orders".into(),
                route_prefix: None,
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::File,
                data_plane_auth: Default::default(),
                chaos: None,
                limits: Default::default(),
                behavior: Default::default(),
                hosts: vec![],
                webhooks: vec![WebhookTarget {
                    url: format!("{}/webhook", server.uri()),
                    events: WebhookTarget::all_events(),
                    secret: Some(secret.into()),
                    signing: Some(WebhookSigning::Stripe),
                    delay_ms: 0,
                }],
            },
        )
        .await
        .unwrap();
    let source = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/openapi/orders-3.0.yaml"),
    )
    .unwrap();
    let v = store
        .add_contract_version(
            mock.id,
            NewContractVersion {
                bundle: mb_import::compile(&source).unwrap(),
                source,
                status: ContractStatus::Approved,
                created_by: None,
            },
        )
        .await
        .unwrap();
    store
        .activate_contract_version(mock.id, v.version)
        .await
        .unwrap();
    let log = SharedDeliveryLog::default();
    let deps = deps(store, log.clone());
    let rt = build_runtime(&deps, &mock).await.unwrap().unwrap();
    let app = http_rest::router(Arc::new(rt));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), log)
}

async fn wait_for(log: &SharedDeliveryLog, status: DeliveryStatus) -> DeliveryRecord {
    for _ in 0..500 {
        if let Some(r) = log
            .list(DEFAULT_WORKSPACE, "orders", 10)
            .into_iter()
            .find(|r| r.status == status)
        {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no {status:?} delivery");
}

async fn create_order(base: &str) -> u16 {
    reqwest::Client::new()
        .post(format!("{base}/orders"))
        .json(&json!({"customerId": "c1", "total": 5}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn served_mock_delivers_stripe_signed_event() {
    // SAFETY: only this test reads the variable; set before any thread reads it.
    unsafe { std::env::set_var(SECRET_VAR, "whsec_test_secret") };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let (base, log) = serve(&server, &format!("env:{SECRET_VAR}")).await;
    assert_eq!(create_order(&base).await, 201);
    let r = wait_for(&log, DeliveryStatus::Delivered).await;
    assert_eq!(r.event_type, "order.created");
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let sig = reqs[0].headers["stripe-signature"].to_str().unwrap();
    let t: i64 = sig
        .split(',')
        .next()
        .unwrap()
        .strip_prefix("t=")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        sig,
        stripe_signature(b"whsec_test_secret", t, &reqs[0].body)
    );
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["type"], "order.created");
}

#[tokio::test]
async fn unset_secret_keeps_serving_and_records_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let (base, log) = serve(&server, &format!("env:{UNSET_VAR}")).await;
    assert_eq!(create_order(&base).await, 201);
    let r = wait_for(&log, DeliveryStatus::Failed).await;
    assert_eq!(r.attempts, 0);
    assert!(
        r.error
            .as_deref()
            .unwrap()
            .contains(&format!("env:{UNSET_VAR}"))
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn redirects_are_not_followed() {
    unsafe { std::env::set_var(SECRET_VAR, "whsec_test_secret") };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/ok"))
        .mount(&server)
        .await;
    Mock::given(path("/ok"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let (base, log) = serve(&server, &format!("env:{SECRET_VAR}")).await;
    assert_eq!(create_order(&base).await, 201);
    for _ in 0..500 {
        if let Some(r) = log.list(DEFAULT_WORKSPACE, "orders", 1).first()
            && r.attempts >= 1
        {
            assert_ne!(r.status, DeliveryStatus::Delivered, "{r:?}");
            assert_eq!(r.response_status, Some(302), "{r:?}");
            let followed = server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|req| req.url.path() == "/ok");
            assert!(!followed, "the redirect was followed");
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no attempt recorded");
}
