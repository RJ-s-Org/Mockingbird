#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use mb_protocols::webhooks::deliver::{
    DispatchSettings, MAX_RETRIES, OutboundEvent, ResolvedTarget, SecretBytes, WebhookDispatcher,
    retry_delay,
};
use mb_protocols::webhooks::log::{
    DeliveryLog, DeliveryRecord, DeliveryStatus, SharedDeliveryLog, redacted_url,
};
use mb_protocols::webhooks::sign::{Signing, stripe_signature};
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
