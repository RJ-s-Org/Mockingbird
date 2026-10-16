//! Cross-node bus. The Postgres and Redis tests are gated by
//! `MB_TEST_POSTGRES_URL` / `MB_TEST_REDIS_URL` (see `scripts/test-postgres.sh`
//! and `scripts/test-redis.sh`).

use std::time::Duration;

use mb_state::bus::{
    Bus, BusEnvelope, BusEvent, LocalBus, MAX_BUS_PAYLOAD, MemoryBusHub, PgBus, RedisBus,
};
use sqlx::Connection;
use sqlx::postgres::PgConnection;
use tokio::sync::broadcast;
use tokio::time::timeout;
use uuid::Uuid;

const PG_VAR: &str = "MB_TEST_POSTGRES_URL";
const REDIS_VAR: &str = "MB_TEST_REDIS_URL";
const WAIT: Duration = Duration::from_secs(10);

/// The reconnect tests kill listener connections server-wide, so the gated
/// tests of this binary run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn env_url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipped: {var} not set");
            None
        }
    }
}

fn node() -> String {
    format!("node-{}", Uuid::new_v4().simple())
}

fn mock_event(name: &str) -> BusEvent {
    BusEvent::Mock {
        workspace: "default".into(),
        name: name.into(),
    }
}

/// Next event within `within`, panicking on timeout or a closed channel.
async fn next(rx: &mut broadcast::Receiver<BusEvent>, within: Duration) -> BusEvent {
    timeout(within, rx.recv())
        .await
        .expect("timed out waiting for a bus event")
        .expect("bus channel closed")
}

/// Asserts nothing arrives for a short while.
async fn assert_silent(rx: &mut broadcast::Receiver<BusEvent>) {
    let got = timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(got.is_err(), "unexpected bus event: {got:?}");
}

#[tokio::test]
async fn memory_hub_delivers_to_others_not_self() {
    let hub = MemoryBusHub::new();
    let a = hub.join("a".into());
    let b = hub.join("b".into());
    let c = hub.join("c".into());
    assert!(!a.is_local());
    assert_eq!(a.node_id(), "a");
    assert!(a.health().is_ok());
    let mut ra = a.subscribe();
    let mut rb = b.subscribe();
    let mut rc = c.subscribe();

    a.publish(mock_event("orders")).await.expect("publish");
    assert_eq!(next(&mut rb, WAIT).await, mock_event("orders"));
    assert_eq!(next(&mut rc, WAIT).await, mock_event("orders"));
    assert_silent(&mut ra).await;
}

#[tokio::test]
async fn local_bus_is_silent() {
    let bus = LocalBus::new("solo".into());
    assert!(bus.is_local());
    assert_eq!(bus.node_id(), "solo");
    assert!(bus.health().is_ok());
    let mut rx = bus.subscribe();
    bus.publish(mock_event("orders")).await.expect("publish");
    assert_silent(&mut rx).await;
}

#[test]
fn envelope_round_trip_json() {
    let events = [
        mock_event("orders"),
        BusEvent::Profile {
            workspace: "w".into(),
            name: "p".into(),
        },
        BusEvent::Workspace { name: "w".into() },
        BusEvent::State {
            workspace: "w".into(),
            mock: "m".into(),
            session: "s".into(),
            resource_type: "order".into(),
            id: "1".into(),
            deleted: true,
        },
        BusEvent::RequestLogged { id: 42 },
        BusEvent::Resync,
    ];
    for event in events {
        let env = BusEnvelope {
            origin: "n1".into(),
            event,
        };
        let json = serde_json::to_string(&env).expect("encode");
        let back: BusEnvelope = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, env);
    }
    let json = serde_json::to_value(BusEnvelope {
        origin: "n1".into(),
        event: BusEvent::RequestLogged { id: 7 },
    })
    .expect("encode");
    assert_eq!(
        json,
        serde_json::json!({"origin": "n1", "event": {"type": "request_logged", "id": 7}})
    );
}

#[tokio::test]
async fn oversized_event_becomes_resync() {
    let hub = MemoryBusHub::new();
    let a = hub.join("a".into());
    let b = hub.join("b".into());
    let mut rb = b.subscribe();
    a.publish(mock_event(&"x".repeat(MAX_BUS_PAYLOAD)))
        .await
        .expect("publish");
    assert_eq!(next(&mut rb, WAIT).await, BusEvent::Resync);
    // Just under the limit still goes through as itself.
    let small = mock_event(&"x".repeat(MAX_BUS_PAYLOAD - 200));
    a.publish(small.clone()).await.expect("publish");
    assert_eq!(next(&mut rb, WAIT).await, small);
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_bus_delivers_between_nodes() {
    let Some(url) = env_url(PG_VAR) else { return };
    let _serial = SERIAL.lock().await;
    let a = PgBus::connect(&url, node()).await.expect("connect a");
    let b = PgBus::connect(&url, node()).await.expect("connect b");
    assert!(!a.is_local());
    let mut ra = a.subscribe();
    let mut rb = b.subscribe();
    let name = format!("m-{}", Uuid::new_v4().simple());
    a.publish(mock_event(&name)).await.expect("publish");
    // Other tests share the server: skip unrelated traffic.
    loop {
        if next(&mut rb, WAIT).await == mock_event(&name) {
            break;
        }
    }
    // The publisher never hears itself.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, ra.recv()).await {
        assert_ne!(ev, mock_event(&name), "publisher received its own event");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_bus_reconnect_emits_resync() {
    let Some(url) = env_url(PG_VAR) else { return };
    let _serial = SERIAL.lock().await;
    let me = node();
    let bus = PgBus::connect_with_backoff(&url, me.clone(), Duration::from_millis(100))
        .await
        .expect("connect");
    let mut rx = bus.subscribe();
    let mut admin = PgConnection::connect(&url).await.expect("admin");
    // This node's listener backend (its connections carry the node id).
    let killed: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE application_name = $1 AND query ILIKE '%LISTEN%mockingbird_bus%'",
    )
    .bind(format!("mockingbird-bus:{me}"))
    .fetch_all(&mut admin)
    .await
    .expect("terminate");
    assert!(!killed.is_empty(), "no listener backend found");
    loop {
        if next(&mut rx, Duration::from_secs(35)).await == BusEvent::Resync {
            break;
        }
    }
    assert!(bus.health().is_ok());
    // Still delivering after the reconnect.
    let other = PgBus::connect(&url, node()).await.expect("connect other");
    let name = format!("m-{}", Uuid::new_v4().simple());
    other.publish(mock_event(&name)).await.expect("publish");
    loop {
        if next(&mut rx, WAIT).await == mock_event(&name) {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn redis_bus_delivers_between_nodes() {
    let Some(url) = env_url(REDIS_VAR) else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let a = RedisBus::connect(&url, node()).await.expect("connect a");
    let b = RedisBus::connect(&url, node()).await.expect("connect b");
    assert!(!a.is_local());
    let mut ra = a.subscribe();
    let mut rb = b.subscribe();
    let name = format!("m-{}", Uuid::new_v4().simple());
    a.publish(mock_event(&name)).await.expect("publish");
    loop {
        if next(&mut rb, WAIT).await == mock_event(&name) {
            break;
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, ra.recv()).await {
        assert_ne!(ev, mock_event(&name), "publisher received its own event");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn redis_bus_reconnect_emits_resync() {
    let Some(url) = env_url(REDIS_VAR) else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let bus = RedisBus::connect_with_backoff(&url, node(), Duration::from_millis(100))
        .await
        .expect("connect");
    let mut rx = bus.subscribe();
    let client = redis::Client::open(url.as_str()).expect("client");
    let mut con = client
        .get_multiplexed_async_connection()
        .await
        .expect("admin connect");
    let killed: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("TYPE")
        .arg("pubsub")
        .query_async(&mut con)
        .await
        .expect("CLIENT KILL");
    assert!(killed >= 1, "no pubsub client killed");
    loop {
        if next(&mut rx, Duration::from_secs(35)).await == BusEvent::Resync {
            break;
        }
    }
    assert!(bus.health().is_ok());
    let other = RedisBus::connect(&url, node())
        .await
        .expect("connect other");
    let name = format!("m-{}", Uuid::new_v4().simple());
    other.publish(mock_event(&name)).await.expect("publish");
    loop {
        if next(&mut rx, WAIT).await == mock_event(&name) {
            break;
        }
    }
}
