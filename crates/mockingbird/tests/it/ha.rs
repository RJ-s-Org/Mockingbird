//! HA change propagation, in process: two `build_app_with_bus` nodes on one
//! SQLite file (role `all`) joined by a [`MemoryBusHub`].
#![cfg(test)]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::support::{fixture, serve};
use async_trait::async_trait;
use mb_ext::Extensions;
use mb_state::{Bus, BusEvent, LocalBus, MemoryBusHub, SessionKey, StateError, StoredResource};
use mb_store::{ControlStore, DEFAULT_WORKSPACE, SqliteControlStore};
use mockingbird::config::{App, ServeArgs, build_app_with_bus};
use serde_json::{Value, json};
use tokio::sync::broadcast;

/// One running node: its app and its data and admin base URLs.
struct Node {
    app: App,
    data: String,
    admin: String,
}

/// Starts a node (offline) on `dir`'s shared SQLite file with a memory
/// broker `mem` and the `mocks` section `mocks` (YAML, may be empty).
async fn node(dir: &Path, bus: Arc<dyn Bus>, mocks: &str) -> Node {
    let config = format!(
        "storage:\n  url: {}\nbrokers:\n  mem:\n    kind: memory\n{mocks}",
        storage_url(dir)
    );
    let path = dir.join("mockingbird.yaml");
    std::fs::write(&path, config).unwrap();
    let args = ServeArgs {
        config: Some(path),
        offline: true,
        ..ServeArgs::default()
    };
    let app = build_app_with_bus(&args, None, Extensions::default(), bus)
        .await
        .unwrap();
    let data = serve(app.data.clone()).await;
    let admin = serve(app.admin.clone()).await;
    Node { app, data, admin }
}

fn storage_url(dir: &Path) -> String {
    format!("sqlite://{}", dir.join("mb.db").display())
}

/// Nodes `a` and `b` on one SQLite file and one bus hub.
struct Pair {
    hub: Arc<MemoryBusHub>,
    a: Node,
    b: Node,
    dir: tempfile::TempDir,
}

async fn pair(mocks: &str) -> Pair {
    let dir = tempfile::tempdir().unwrap();
    let hub = MemoryBusHub::new();
    let a = node(dir.path(), hub.join("node-a".into()), mocks).await;
    let b = node(dir.path(), hub.join("node-b".into()), mocks).await;
    Pair { hub, a, b, dir }
}

fn mocks_section(mocks: &[(&str, &str)]) -> String {
    let mut yaml = String::from("mocks:\n");
    for (name, file) in mocks {
        yaml.push_str(&format!(
            "  {name}:\n    contract: {}\n",
            fixture(file).display()
        ));
    }
    yaml
}

/// Polls `url` until it answers `status`, for at most `within`.
async fn wait_status(url: &str, status: u16, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        let got = reqwest::get(url).await.unwrap().status().as_u16();
        if got == status {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{url} answered {got}, not {status}, within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Creates mock `name` serving the orders contract through `admin`.
async fn create_orders_mock(admin: &str, name: &str) {
    let http = reqwest::Client::new();
    let r = http
        .post(format!("{admin}/api/v1/mocks"))
        .json(&json!({"name": name}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let r = http
        .post(format!("{admin}/api/v1/mocks/{name}/contracts"))
        .header("content-type", "application/yaml")
        .body(std::fs::read(fixture("openapi/orders-3.0.yaml")).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
}

/// Reads SSE `data:` lines from `resp` until one contains `needle`, for at
/// most `within`; returns everything read.
async fn read_until(resp: &mut reqwest::Response, needle: &str, within: Duration) -> String {
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + within;
    while !text.contains(needle) {
        match tokio::time::timeout_at(deadline, resp.chunk()).await {
            Ok(Ok(Some(chunk))) => text.push_str(&String::from_utf8_lossy(&chunk)),
            _ => break,
        }
    }
    text
}

/// Reads and discards what `resp` sends for `period`.
async fn drain(resp: &mut reqwest::Response, period: Duration) {
    let deadline = tokio::time::Instant::now() + period;
    while let Ok(Ok(Some(_))) = tokio::time::timeout_at(deadline, resp.chunk()).await {}
}

#[tokio::test]
async fn mock_created_on_a_is_served_by_b() {
    let p = pair("").await;
    create_orders_mock(&p.a.admin, "c").await;
    wait_status(
        &format!("{}/c/orders/o1", p.b.data),
        200,
        Duration::from_secs(2),
    )
    .await;
    wait_status(
        &format!("{}/c/orders/o1", p.a.data),
        200,
        Duration::from_secs(2),
    )
    .await;
}

#[tokio::test]
async fn mock_deleted_on_a_stops_on_b() {
    let p = pair("").await;
    create_orders_mock(&p.a.admin, "c").await;
    let on_b = format!("{}/c/orders/o1", p.b.data);
    wait_status(&on_b, 200, Duration::from_secs(2)).await;

    let r = reqwest::Client::new()
        .delete(format!("{}/api/v1/mocks/c", p.a.admin))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    wait_status(&on_b, 404, Duration::from_secs(2)).await;
}

#[tokio::test]
async fn state_written_on_b_reaches_change_feeds_on_a() {
    let p = pair(&mocks_section(&[("events", "openapi/events-3.1.yaml")])).await;
    let mut feed = reqwest::get(format!("{}/events/ticks", p.a.data))
        .await
        .unwrap();
    assert_eq!(feed.status(), 200);
    assert_eq!(feed.headers()["content-type"], "text/event-stream");
    // Let the feed subscribe and the generated items drain.
    drain(&mut feed, Duration::from_millis(300)).await;

    let r = reqwest::Client::new()
        .post(format!("{}/events/ticks", p.b.data))
        .json(&json!({"price": 42.5}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let created: Value = r.json().await.unwrap();
    let id = created["id"].as_str().unwrap().to_owned();

    let text = read_until(&mut feed, &id, Duration::from_secs(2)).await;
    assert!(
        text.contains(&id),
        "A's change feed did not see B's upsert of {id}: {text}"
    );
}

#[tokio::test]
async fn remote_state_change_does_not_fire_broker_trigger() {
    // `orderCreated` fires on every upserted order.
    let mocks = format!(
        "{}    broker: mem\n    channels:\n      orderCreated:\n        triggers:\n          - on_state_change: {{resource_type: orders, kind: upserted}}\n",
        mocks_section(&[("orders", "asyncapi/orders-amqp-2.6.yaml")])
    );
    let p = pair(&mocks).await;
    let broker_a = p.a.app.brokers.memory("mem").unwrap();
    let broker_b = p.b.app.brokers.memory("mem").unwrap();
    // Let both nodes' trigger listeners subscribe.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let key = SessionKey {
        workspace: DEFAULT_WORKSPACE.into(),
        mock: "orders".into(),
        session: "s1".into(),
    };
    p.b.app
        .state
        .upsert(
            &key,
            StoredResource {
                resource_type: "orders".into(),
                id: "41".into(),
                data: json!({"id": "41", "status": "placed", "total": 10}),
            },
        )
        .await
        .unwrap();

    // B, where the write happened, publishes once.
    broker_b.wait_published(1, Duration::from_secs(2)).await;
    // Give A time to (wrongly) publish too.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let total = broker_a.published().len() + broker_b.published().len();
    assert_eq!(
        total,
        1,
        "a: {:?}, b: {:?}",
        broker_a.published(),
        broker_b.published()
    );
    assert!(broker_a.published().is_empty());
}

#[tokio::test]
async fn request_logged_on_b_appears_in_live_feed_on_a() {
    let p = pair(&mocks_section(&[("events", "openapi/events-3.1.yaml")])).await;
    let mut tail = reqwest::get(format!("{}/api/v1/mocks/events/requests/stream", p.a.admin))
        .await
        .unwrap();
    assert_eq!(tail.status(), 200);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let r = reqwest::Client::new()
        .post(format!("{}/events/ticks?marker=from-b", p.b.data))
        .json(&json!({"price": 1.5}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let text = read_until(&mut tail, "from-b", Duration::from_secs(2)).await;
    assert!(text.contains("from-b"), "{text}");
    assert!(text.contains("POST"), "{text}");
}

#[tokio::test]
async fn resync_reloads_all_mocks() {
    let p = pair("").await;
    create_orders_mock(&p.a.admin, "c").await;
    let on_a = format!("{}/c/orders/o1", p.a.data);
    wait_status(&on_a, 200, Duration::from_secs(2)).await;

    // Deleted behind every node's back: nothing is announced.
    let raw = SqliteControlStore::connect(&storage_url(p.dir.path()))
        .await
        .unwrap();
    assert!(raw.delete_mock(DEFAULT_WORKSPACE, "c").await.unwrap());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(reqwest::get(&on_a).await.unwrap().status(), 200);

    let injector = p.hub.join("injector".into());
    injector.publish(BusEvent::Resync).await.unwrap();
    wait_status(&on_a, 404, Duration::from_secs(2)).await;
    wait_status(
        &format!("{}/c/orders/o1", p.b.data),
        404,
        Duration::from_secs(2),
    )
    .await;
}

/// A bus whose health a test sets.
struct ToggleBus {
    inner: Arc<dyn Bus>,
    health: Mutex<Result<(), String>>,
}

#[async_trait]
impl Bus for ToggleBus {
    async fn publish(&self, event: BusEvent) -> Result<(), StateError> {
        self.inner.publish(event).await
    }
    fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.inner.subscribe()
    }
    fn health(&self) -> Result<(), String> {
        self.health.lock().unwrap().clone()
    }
    fn node_id(&self) -> &str {
        self.inner.node_id()
    }
    fn is_local(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn bus_down_degrades_readyz() {
    let dir = tempfile::tempdir().unwrap();
    let bus = Arc::new(ToggleBus {
        inner: LocalBus::new("toggle".into()),
        health: Mutex::new(Ok(())),
    });
    let n = node(dir.path(), bus.clone(), "").await;
    let readyz = || async {
        reqwest::get(format!("{}/readyz", n.admin))
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    assert_eq!(readyz().await["status"], "ready");

    *bus.health.lock().unwrap() = Err("redis: pub/sub connection lost".into());
    let body = readyz().await;
    assert_eq!(body["status"], "degraded", "{body}");
    assert!(
        body["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r == "bus: redis: pub/sub connection lost"),
        "{body}"
    );

    *bus.health.lock().unwrap() = Ok(());
    assert_eq!(readyz().await["status"], "ready");
}
