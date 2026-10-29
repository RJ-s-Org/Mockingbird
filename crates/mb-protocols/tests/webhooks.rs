#![cfg(test)]
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_protocols::webhooks::deliver::{
    DispatchSettings, MAX_RETRIES, OutboundEvent, ResolvedTarget, SecretBytes, WebhookDispatcher,
    retry_delay,
};
use mb_protocols::webhooks::log::{
    DeliveryLog, DeliveryRecord, DeliveryStatus, SharedDeliveryLog, redacted_url,
};
use mb_protocols::webhooks::sign::{Signing, stripe_signature};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::MemoryStore;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fast() -> DispatchSettings {
    DispatchSettings {
        backoff_base: Duration::from_millis(10),
        ..Default::default()
    }
}

fn target(
    url: &str,
    signing: Signing,
    secret: Result<Option<SecretBytes>, String>,
) -> ResolvedTarget {
    ResolvedTarget {
        url: url.into(),
        events: vec!["*".into()],
        signing,
        secret,
        delay: Duration::ZERO,
    }
}

fn dispatcher(
    url: &str,
    signing: Signing,
    secret: Option<&str>,
    settings: DispatchSettings,
) -> (Arc<WebhookDispatcher>, SharedDeliveryLog) {
    let secret = Ok(secret.map(|s| SecretBytes::new(s.into())));
    dispatcher_with(vec![target(url, signing, secret)], settings)
}

fn dispatcher_with_secret(
    url: &str,
    secret: Result<Option<SecretBytes>, String>,
    settings: DispatchSettings,
) -> (Arc<WebhookDispatcher>, SharedDeliveryLog) {
    dispatcher_with(vec![target(url, Signing::Stripe, secret)], settings)
}

fn dispatcher_with(
    targets: Vec<ResolvedTarget>,
    settings: DispatchSettings,
) -> (Arc<WebhookDispatcher>, SharedDeliveryLog) {
    let log: SharedDeliveryLog = Arc::default();
    let d = WebhookDispatcher::new(
        "default".into(),
        "m".into(),
        targets,
        reqwest::Client::new(),
        log.clone(),
        settings,
    );
    (Arc::new(d), log)
}

fn event(id: &str, body: &[u8]) -> OutboundEvent {
    OutboundEvent {
        event_id: id.into(),
        event_type: "order.created".into(),
        operation: "createOrder".into(),
        session: "default".into(),
        body: body.to_vec(),
    }
}

fn record(id: &str) -> DeliveryRecord {
    DeliveryRecord {
        id: id.into(),
        event_id: "evt_1".into(),
        event_type: "order.created".into(),
        operation: "createOrder".into(),
        session: "default".into(),
        url: "https://h.test/x".into(),
        status: DeliveryStatus::Pending,
        attempts: 0,
        response_status: None,
        error: None,
        created_at: SystemTime::now(),
        finished_at: None,
    }
}

async fn wait_for(log: &DeliveryLog, pred: impl Fn(&DeliveryRecord) -> bool) -> DeliveryRecord {
    for _ in 0..500 {
        if let Some(r) = log.list("default", "m", 200).into_iter().find(|r| pred(r)) {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no matching delivery: {:?}", log.list("default", "m", 200));
}

#[tokio::test]
async fn delivers_signed_body_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let (d, log) = dispatcher(
        &format!("{}/hook", server.uri()),
        Signing::Stripe,
        Some("whsec_test_secret"),
        fast(),
    );
    d.send(event("evt_1", br#"{"a":1}"#), &[0]);
    let r = wait_for(&log, |r| r.status == DeliveryStatus::Delivered).await;
    assert_eq!(r.attempts, 1);
    assert_eq!(r.response_status, Some(200));
    assert!(r.finished_at.is_some());
    let req = &server.received_requests().await.unwrap()[0];
    assert_eq!(req.body, br#"{"a":1}"#);
    assert_eq!(req.headers["content-type"], "application/json");
    assert_eq!(req.headers["x-mockingbird-event-id"], "evt_1");
    assert_eq!(req.headers["x-mockingbird-event-type"], "order.created");
    assert!(
        req.headers["user-agent"]
            .to_str()
            .unwrap()
            .starts_with("Mockingbird-Webhooks/")
    );
    let sig = req.headers["stripe-signature"].to_str().unwrap();
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
        stripe_signature(b"whsec_test_secret", t, br#"{"a":1}"#)
    );
}

#[tokio::test]
async fn retries_on_500_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let (d, log) = dispatcher(&server.uri(), Signing::None, None, fast());
    d.send(event("evt_2", b"{}"), &[0]);
    let r = wait_for(&log, |r| r.status == DeliveryStatus::Delivered).await;
    assert_eq!(r.attempts, 3);
}

#[tokio::test]
async fn gives_up_after_three_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .expect(4)
        .mount(&server)
        .await;
    let (d, log) = dispatcher(&server.uri(), Signing::None, None, fast());
    d.send(event("evt_3", b"{}"), &[0]);
    let r = wait_for(&log, |r| r.status == DeliveryStatus::Failed).await;
    assert_eq!((r.attempts, r.response_status), (4, Some(503)));
}

#[test]
fn default_settings_are_pinned() {
    let s = DispatchSettings::default();
    assert_eq!(
        (s.backoff_base, s.max_pending, s.attempt_timeout),
        (Duration::from_secs(1), 256, Duration::from_secs(10))
    );
    let delays: Vec<_> = (1..=MAX_RETRIES).map(|n| retry_delay(&s, n)).collect();
    assert_eq!(delays, [1, 2, 4].map(Duration::from_secs));
}

#[tokio::test]
async fn unresolved_secret_is_never_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let (d, log) = dispatcher_with_secret(
        &server.uri(),
        Err("secret reference env:MISSING_WEBHOOK_SECRET could not be resolved".into()),
        fast(),
    );
    d.send(event("evt_4", b"{}"), &[0]);
    let r = wait_for(&log, |r| r.status == DeliveryStatus::Failed).await;
    assert_eq!(r.attempts, 0);
    assert!(r.error.unwrap().contains("env:MISSING_WEBHOOK_SECRET"));
}

#[tokio::test]
async fn drops_beyond_max_pending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)))
        .mount(&server)
        .await;
    let (d, log) = dispatcher(
        &server.uri(),
        Signing::None,
        None,
        DispatchSettings {
            max_pending: 1,
            ..fast()
        },
    );
    d.send(event("evt_5", b"{}"), &[0]);
    d.send(event("evt_6", b"{}"), &[0]);
    let all = log.list("default", "m", 10);
    assert_eq!(all[0].event_id, "evt_6");
    assert_eq!(all[0].status, DeliveryStatus::Dropped);
    assert_eq!(all[1].status, DeliveryStatus::Pending);
}

#[test]
fn matching_by_operation_type_or_star() {
    let t = |events: &[&str]| ResolvedTarget {
        events: events.iter().map(|e| e.to_string()).collect(),
        ..target("http://h.test/x", Signing::None, Ok(None))
    };
    let (d, _) = dispatcher_with(
        vec![
            t(&["*"]),
            t(&["createOrder"]),
            t(&["order.created"]),
            t(&["other"]),
        ],
        fast(),
    );
    assert_eq!(d.matching("createOrder", "order.created"), [0, 1, 2]);
    assert_eq!(d.matching("deleteOrder", "order.deleted"), [0]);
}

#[test]
fn log_is_bounded_newest_first_and_redacted() {
    let log = DeliveryLog::default();
    for i in 0..205 {
        log.push("default", "m", record(&format!("whd_{i}")));
    }
    let all = log.list("default", "m", 500);
    assert_eq!(all.len(), 200);
    assert_eq!(all[0].id, "whd_204");
    assert!(log.list("default", "other", 10).is_empty());
    log.update("default", "m", "whd_204", |r| r.attempts = 7);
    assert_eq!(log.list("default", "m", 1)[0].attempts, 7);
    assert_eq!(
        redacted_url("https://user:pass@hooks.test:8443/a/b?token=x#f"),
        "https://hooks.test:8443/a/b"
    );
    assert_eq!(redacted_url("not a url"), "<invalid url>");
    assert_eq!(
        format!("{:?}", SecretBytes::new("whsec_test_secret".into())),
        "<redacted>"
    );
}

// ---- the post-write hook, end to end on a REST mock ----

async fn receiver(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

fn to(server: &MockServer, events: &[&str]) -> ResolvedTarget {
    ResolvedTarget {
        events: events.iter().map(|e| e.to_string()).collect(),
        ..target(&format!("{}/hook", server.uri()), Signing::None, Ok(None))
    }
}

fn rest(fixture: &str, targets: Vec<ResolvedTarget>) -> (Router, SharedDeliveryLog) {
    let path = format!(
        "{}/../../fixtures/openapi/{fixture}",
        env!("CARGO_MANIFEST_DIR")
    );
    let bundle = Arc::new(compile(&std::fs::read(path).unwrap()).unwrap());
    let mut rt = MockRuntime::new(
        "m",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let (d, log) = dispatcher_with(targets, fast());
    rt.webhooks = Some(d);
    (http_rest::router(Arc::new(rt)), log)
}

fn post_json_with(uri: &str, body: Value, headers: &[(&str, &str)]) -> Request<Body> {
    let mut req = Request::post(uri).header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.body(Body::from(body.to_string())).unwrap()
}

fn post_json(uri: &str, body: Value) -> Request<Body> {
    post_json_with(uri, body, &[])
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

fn delete(uri: &str) -> Request<Body> {
    Request::delete(uri).body(Body::empty()).unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn sent(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn create_posts_envelope() {
    let server = receiver(200).await;
    let (router, log) = rest("orders-3.0.yaml", vec![to(&server, &["*"])]);
    let resp = router
        .oneshot(post_json(
            "/orders",
            json!({"customerId": "c1", "total": 5}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let r = wait_for(&log, |r| r.status == DeliveryStatus::Delivered).await;
    assert_eq!(
        (r.operation.as_str(), r.event_type.as_str()),
        ("createOrder", "order.created")
    );
    let sent = sent(&server).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["type"], "order.created");
    assert_eq!(sent[0]["object"], "event");
    assert_eq!(sent[0]["data"]["object"], created);
    assert_eq!(sent[0]["id"], json!(r.event_id));
    assert!(sent[0]["id"].as_str().unwrap().starts_with("evt_"));
    assert!(sent[0]["created"].as_i64().unwrap() > 1_700_000_000);
}

#[tokio::test]
async fn reads_errors_and_forced_failures_fire_nothing() {
    let server = receiver(200).await;
    let (router, log) = rest("orders-3.0.yaml", vec![to(&server, &["*"])]);
    // A list seeds its items into state: still no event.
    let list = router.clone().oneshot(get("/orders")).await.unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    router.clone().oneshot(get("/orders/o1")).await.unwrap();
    let invalid = router
        .clone()
        .oneshot(post_json("/orders", json!({"bad": true})))
        .await
        .unwrap();
    assert!(invalid.status().is_client_error(), "{}", invalid.status());
    let forced = router
        .clone()
        .oneshot(post_json_with(
            "/orders",
            json!({"customerId": "c1", "total": 5}),
            &[("x-mockingbird-status", "500")],
        ))
        .await
        .unwrap();
    assert_eq!(forced.status(), StatusCode::INTERNAL_SERVER_ERROR);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(log.list("default", "m", 10).is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn delete_without_body_carries_id() {
    let server = receiver(200).await;
    let (router, log) = rest("orders-3.0.yaml", vec![to(&server, &["deleteOrder"])]);
    let resp = router.oneshot(delete("/orders/o42")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    wait_for(&log, |r| r.status == DeliveryStatus::Delivered).await;
    let sent = sent(&server).await;
    assert_eq!(sent[0]["type"], "order.deleted");
    assert_eq!(
        sent[0]["data"]["object"],
        json!({"id": "o42", "deleted": true})
    );
}

#[tokio::test]
async fn events_filter_by_operation_or_type() {
    let server = receiver(200).await;
    let (router, log) = rest(
        "orders-3.0.yaml",
        vec![
            to(&server, &["order.updated"]),
            to(&server, &["deleteOrder"]),
        ],
    );
    router
        .clone()
        .oneshot(post_json(
            "/orders",
            json!({"customerId": "c1", "total": 5}),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(log.list("default", "m", 10).is_empty());
    let req = Request::put("/orders/o1")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"customerId": "c1", "total": 6}).to_string(),
        ))
        .unwrap();
    assert_eq!(router.oneshot(req).await.unwrap().status(), StatusCode::OK);
    let r = wait_for(&log, |r| r.status == DeliveryStatus::Delivered).await;
    assert_eq!(r.event_type, "order.updated");
    assert_eq!(log.list("default", "m", 10).len(), 1);
}

#[tokio::test]
async fn openapi_webhook_payload_used_when_present() {
    let server = receiver(200).await;
    let (router, log) = rest("webhooks-3.1.yaml", vec![to(&server, &["*"])]);
    let resp = router
        .oneshot(post_json("/pets", json!({"name": "Rex"})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    wait_for(&log, |r| r.status == DeliveryStatus::Delivered).await;
    let sent = sent(&server).await;
    // createPet declares a callback, which wins over the `pet.created` webhook.
    assert_eq!(sent[0]["adopted"], json!(true));
    assert!(sent[0].get("data").is_none());
}

#[tokio::test]
async fn slow_receiver_does_not_delay_the_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .mount(&server)
        .await;
    let (router, _log) = rest("orders-3.0.yaml", vec![to(&server, &["*"])]);
    let started = std::time::Instant::now();
    let resp = router
        .oneshot(post_json(
            "/orders",
            json!({"customerId": "c1", "total": 5}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(started.elapsed() < Duration::from_secs(1));
}

/// An RPC-style read (Plaid's `POST /transactions/sync`) fires for targets
/// that name its operation, as Plaid sends `SYNC_UPDATES_AVAILABLE`, but
/// not for `*`.
#[tokio::test]
async fn a_read_fires_only_for_targets_naming_it() {
    let contract = r#"
openapi: 3.0.3
info: {title: sync, version: "1"}
paths:
  /transactions/sync:
    post:
      operationId: transactionsSync
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  next_cursor: {type: string}
"#;
    let named = receiver(200).await;
    let star = receiver(200).await;
    let mut rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let (d, log) = dispatcher_with(
        vec![to(&named, &["transactionsSync"]), to(&star, &["*"])],
        fast(),
    );
    rt.webhooks = Some(d);
    let router = http_rest::router(Arc::new(rt));
    let resp = router
        .oneshot(post_json("/transactions/sync", json!({})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let r = wait_for(&log, |r| r.status == DeliveryStatus::Delivered).await;
    assert_eq!(
        (r.operation.as_str(), r.event_type.as_str()),
        ("transactionsSync", "transaction.updated")
    );
    assert_eq!(sent(&named).await.len(), 1);
    assert!(sent(&star).await.is_empty());
}
