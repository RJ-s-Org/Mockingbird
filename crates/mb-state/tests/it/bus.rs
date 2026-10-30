//! Cross-node bus. The Postgres and Redis tests are gated by
//! `MB_TEST_POSTGRES_URL` / `MB_TEST_REDIS_URL` (see `scripts/test-postgres.sh`
//! and `scripts/test-redis.sh`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use mb_state::bus::{
    Bus, BusEnvelope, BusEvent, BusTiming, LocalBus, MAX_BUS_PAYLOAD, MemoryBusHub, PgBus, RedisBus,
};
use sqlx::Connection;
use sqlx::postgres::PgConnection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::timeout;
use uuid::Uuid;

use crate::support::env_url;

const PG_VAR: &str = "MB_TEST_POSTGRES_URL";
const REDIS_VAR: &str = "MB_TEST_REDIS_URL";
const WAIT: Duration = Duration::from_secs(10);

/// The reconnect tests kill listener connections server-wide, so the gated
/// tests of this binary run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// Asserts nothing arrives for a short while. Its callers are in-process
/// and run on a paused clock, so the 300 ms are virtual.
async fn assert_silent(rx: &mut broadcast::Receiver<BusEvent>) {
    let got = timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(got.is_err(), "unexpected bus event: {got:?}");
}

#[tokio::test(start_paused = true)]
async fn memory_hub_delivers_to_others_not_self() {
    let hub = MemoryBusHub::new_shared();
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

#[tokio::test(start_paused = true)]
async fn local_bus_is_silent() {
    let bus = LocalBus::new_shared("solo".into());
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
    let hub = MemoryBusHub::new_shared();
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

// ---------------------------------------------------------- half-open links

/// A TCP proxy whose [`FreezableProxy::freeze`] makes every connection open
/// at that moment stop forwarding without closing, like a link that went
/// half-open (a partition, or conntrack or a load balancer dropping the
/// flow). Connections made later forward normally, unless the proxy is
/// partitioned: then new connections are accepted and frozen at once.
struct FreezableProxy {
    port: u16,
    epoch: Arc<AtomicU64>,
    partitioned: Arc<AtomicBool>,
}

impl FreezableProxy {
    async fn start(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let epoch = Arc::new(AtomicU64::new(0));
        let partitioned = Arc::new(AtomicBool::new(false));
        let (shared, cut) = (Arc::clone(&epoch), Arc::clone(&partitioned));
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = TcpStream::connect(&upstream).await else {
                    continue;
                };
                // An epoch that never comes: frozen from the start.
                let born = if cut.load(Ordering::SeqCst) {
                    u64::MAX
                } else {
                    shared.load(Ordering::SeqCst)
                };
                tokio::spawn(pipe(client, server, born, Arc::clone(&shared)));
            }
        });
        Self {
            port,
            epoch,
            partitioned,
        }
    }

    fn partition(&self, cut: bool) {
        self.partitioned.store(cut, Ordering::SeqCst);
    }

    fn freeze(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// `url` with its host and port pointing at this proxy.
    fn url(&self, url: &str) -> String {
        rewrite_authority(url, &format!("127.0.0.1:{}", self.port))
    }
}

/// Forwards both ways until the proxy freezes, then holds both sockets
/// open forever, forwarding nothing.
async fn pipe(client: TcpStream, server: TcpStream, born: u64, epoch: Arc<AtomicU64>) {
    let frozen = || epoch.load(Ordering::SeqCst) != born;
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();
    let (mut up, mut down) = ([0u8; 8192], [0u8; 8192]);
    loop {
        tokio::select! {
            r = cr.read(&mut up) => {
                let n = match r { Ok(0) | Err(_) => return, Ok(n) => n };
                if frozen() { break }
                if sw.write_all(&up[..n]).await.is_err() { return }
            }
            r = sr.read(&mut down) => {
                let n = match r { Ok(0) | Err(_) => return, Ok(n) => n };
                if frozen() { break }
                if cw.write_all(&down[..n]).await.is_err() { return }
            }
            () = tokio::time::sleep(Duration::from_millis(20)) => if frozen() { break },
        }
    }
    let _held = (cr, cw, sr, sw);
    std::future::pending::<()>().await;
}

/// `scheme://[user@]host:port/rest` with `host:port` replaced.
fn rewrite_authority(url: &str, authority: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("url scheme");
    let (auth, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let user = auth.rsplit_once('@').map(|(u, _)| format!("{u}@"));
    format!("{scheme}://{}{authority}{path}", user.unwrap_or_default())
}

/// The `host:port` of `url`.
fn authority(url: &str) -> String {
    let rest = url.split_once("://").expect("url scheme").1;
    let auth = rest.split('/').next().unwrap_or(rest);
    auth.rsplit_once('@').map_or(auth, |(_, a)| a).to_owned()
}

fn fast_timing() -> BusTiming {
    BusTiming {
        initial_backoff: Duration::from_millis(100),
        heartbeat: Duration::from_millis(300),
        safety_resync: Duration::from_secs(3600),
    }
}

/// Waits for `want`, skipping unrelated traffic from tests sharing the server.
async fn wait_for(rx: &mut broadcast::Receiver<BusEvent>, want: &BusEvent, within: Duration) {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let got = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {want:?}"))
            .expect("bus channel closed");
        if &got == want {
            return;
        }
    }
}

/// After the link freezes, the bus notices within a few heartbeats,
/// reconnects, resyncs and hears new events again.
async fn frozen_link_resyncs(proxied: Arc<dyn Bus>, other: Arc<dyn Bus>, proxy: &FreezableProxy) {
    let mut rx = proxied.subscribe();
    let before = mock_event(&format!("m-{}", Uuid::new_v4().simple()));
    other.publish(before.clone()).await.expect("publish");
    wait_for(&mut rx, &before, WAIT).await;

    proxy.freeze();
    let missed = mock_event(&format!("m-{}", Uuid::new_v4().simple()));
    other.publish(missed).await.expect("publish");
    // One heartbeat period plus the 5 s heartbeat timeout, plus slack.
    wait_for(&mut rx, &BusEvent::Resync, Duration::from_secs(9)).await;
    assert!(proxied.health().is_ok(), "{:?}", proxied.health());

    let after = mock_event(&format!("m-{}", Uuid::new_v4().simple()));
    other.publish(after.clone()).await.expect("publish");
    wait_for(&mut rx, &after, WAIT).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_bus_frozen_link_reconnects_and_resyncs() {
    let Some(url) = env_url(PG_VAR) else { return };
    let _serial = SERIAL.lock().await;
    let proxy = FreezableProxy::start(authority(&url)).await;
    let proxied = PgBus::connect_with_timing(&proxy.url(&url), node(), fast_timing())
        .await
        .expect("connect proxied");
    let other = PgBus::connect(&url, node()).await.expect("connect other");
    frozen_link_resyncs(proxied, other, &proxy).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn redis_bus_frozen_link_reconnects_and_resyncs() {
    let Some(url) = env_url(REDIS_VAR) else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let proxy = FreezableProxy::start(authority(&url)).await;
    let proxied = RedisBus::connect_with_timing(&proxy.url(&url), node(), fast_timing())
        .await
        .expect("connect proxied");
    let other = RedisBus::connect(&url, node())
        .await
        .expect("connect other");
    frozen_link_resyncs(proxied, other, &proxy).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn redis_bus_periodic_safety_resync() {
    let Some(url) = env_url(REDIS_VAR) else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let timing = BusTiming {
        safety_resync: Duration::from_millis(400),
        ..fast_timing()
    };
    let bus = RedisBus::connect_with_timing(&url, node(), timing)
        .await
        .expect("connect");
    let mut rx = bus.subscribe();
    wait_for(&mut rx, &BusEvent::Resync, Duration::from_secs(3)).await;
    wait_for(&mut rx, &BusEvent::Resync, Duration::from_secs(3)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_bus_periodic_safety_resync() {
    let Some(url) = env_url(PG_VAR) else { return };
    let _serial = SERIAL.lock().await;
    let timing = BusTiming {
        safety_resync: Duration::from_millis(400),
        ..fast_timing()
    };
    let bus = PgBus::connect_with_timing(&url, node(), timing)
        .await
        .expect("connect");
    let mut rx = bus.subscribe();
    wait_for(&mut rx, &BusEvent::Resync, Duration::from_secs(3)).await;
    wait_for(&mut rx, &BusEvent::Resync, Duration::from_secs(3)).await;
}

/// A publish that fails while the link is frozen is followed, once the bus
/// works again, by a `Resync` announced to the other nodes.
#[tokio::test(flavor = "multi_thread")]
async fn redis_failed_publish_announces_resync_after_recovery() {
    let Some(url) = env_url(REDIS_VAR) else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let proxy = FreezableProxy::start(authority(&url)).await;
    let publisher = RedisBus::connect_with_timing(&proxy.url(&url), node(), fast_timing())
        .await
        .expect("connect publisher");
    let listener = RedisBus::connect(&url, node())
        .await
        .expect("connect listener");
    let mut rx = listener.subscribe();
    proxy.freeze();
    let lost = mock_event(&format!("m-{}", Uuid::new_v4().simple()));
    let failed = timeout(Duration::from_secs(30), publisher.publish(lost))
        .await
        .expect("publish did not give up");
    assert!(failed.is_err(), "publish through a frozen link succeeded");
    wait_for(&mut rx, &BusEvent::Resync, Duration::from_secs(20)).await;
    let after = mock_event(&format!("m-{}", Uuid::new_v4().simple()));
    timeout(Duration::from_secs(30), publisher.publish(after.clone()))
        .await
        .expect("publish did not give up")
        .expect("publish after recovery");
    wait_for(&mut rx, &after, WAIT).await;
}

/// As [`redis_failed_publish_announces_resync_after_recovery`], over
/// Postgres. The pool would open a fresh connection for the publish, so
/// the link is partitioned (new connections freeze too) until the publish
/// has failed.
#[tokio::test(flavor = "multi_thread")]
async fn pg_failed_publish_announces_resync_after_recovery() {
    let Some(url) = env_url(PG_VAR) else { return };
    let _serial = SERIAL.lock().await;
    let proxy = FreezableProxy::start(authority(&url)).await;
    let publisher = PgBus::connect_with_timing(&proxy.url(&url), node(), fast_timing())
        .await
        .expect("connect publisher");
    let listener = PgBus::connect(&url, node())
        .await
        .expect("connect listener");
    let mut rx = listener.subscribe();
    // Open the publish pool's connection before the link freezes.
    let warm = mock_event(&format!("m-{}", Uuid::new_v4().simple()));
    publisher.publish(warm.clone()).await.expect("publish");
    wait_for(&mut rx, &warm, WAIT).await;
    proxy.partition(true);
    proxy.freeze();
    let lost = mock_event(&format!("m-{}", Uuid::new_v4().simple()));
    let failed = timeout(Duration::from_secs(30), publisher.publish(lost))
        .await
        .expect("publish did not give up");
    assert!(failed.is_err(), "publish through a frozen link succeeded");
    proxy.partition(false);
    wait_for(&mut rx, &BusEvent::Resync, Duration::from_secs(20)).await;
    let after = mock_event(&format!("m-{}", Uuid::new_v4().simple()));
    timeout(Duration::from_secs(30), publisher.publish(after.clone()))
        .await
        .expect("publish did not give up")
        .expect("publish after recovery");
    wait_for(&mut rx, &after, WAIT).await;
}
