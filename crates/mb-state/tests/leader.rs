//! Leader election. The Postgres and Redis tests are gated by
//! `MB_TEST_POSTGRES_URL` / `MB_TEST_REDIS_URL`; each uses its own random lock
//! name so tests share one server safely.

use std::time::Duration;

use mb_state::leader::{
    LEADER_LEASE_TTL, LEADER_RENEW_INTERVAL, LeaderBackend, LeaderTimings, Leadership,
    pg_advisory_key,
};
use sqlx::Connection;
use sqlx::postgres::PgConnection;
use tokio::sync::watch;
use tokio::time::{Instant, sleep, timeout};
use uuid::Uuid;

const PG_VAR: &str = "MB_TEST_POSTGRES_URL";
const REDIS_VAR: &str = "MB_TEST_REDIS_URL";

const RENEW: Duration = Duration::from_millis(200);

fn timings() -> LeaderTimings {
    LeaderTimings {
        renew_interval: RENEW,
        lease_ttl: Duration::from_millis(1000),
    }
}

fn env_url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipped: {var} not set");
            None
        }
    }
}

fn lock_name() -> String {
    format!("test-{}", Uuid::new_v4().simple())
}

/// Waits until `rx` reads `want`, or panics after `within`.
async fn wait_for(rx: &mut watch::Receiver<bool>, want: bool, within: Duration) {
    timeout(within, rx.wait_for(|v| *v == want))
        .await
        .unwrap_or_else(|_| panic!("leadership did not become {want} within {within:?}"))
        .expect("leadership task ended");
}

#[test]
fn defaults_match_constraints() {
    assert_eq!(LEADER_LEASE_TTL, Duration::from_secs(15));
    assert_eq!(LEADER_RENEW_INTERVAL, Duration::from_secs(5));
    let t = LeaderTimings::default();
    assert_eq!(t.renew_interval, LEADER_RENEW_INTERVAL);
    assert_eq!(t.lease_ttl, LEADER_LEASE_TTL);
}

#[test]
fn advisory_key_is_sha256_prefix() {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(b"mockingbird:control");
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    assert_eq!(pg_advisory_key("control"), i64::from_be_bytes(bytes));
}

#[tokio::test]
async fn always_is_leader() {
    let l = Leadership::start(LeaderBackend::Always, "control", "n1".into())
        .await
        .expect("start");
    assert!(l.is_leader());
    assert!(*l.watch().borrow());
}

#[tokio::test]
async fn manual_follows_its_sender() {
    let (tx, rx) = watch::channel(false);
    let l = Leadership::start(LeaderBackend::Manual(rx), "control", "n1".into())
        .await
        .expect("start");
    assert!(!l.is_leader());
    let mut w = l.watch();
    tx.send(true).expect("send");
    wait_for(&mut w, true, Duration::from_secs(1)).await;
    assert!(l.is_leader());
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_leader_single_holder() {
    let Some(url) = env_url(PG_VAR) else { return };
    let name = lock_name();
    let a = Leadership::start_with(
        LeaderBackend::Postgres(url.clone()),
        &name,
        "a".into(),
        timings(),
    )
    .await
    .expect("start a");
    assert!(a.is_leader(), "a lone node leads on return");
    let b = Leadership::start_with(
        LeaderBackend::Postgres(url.clone()),
        &name,
        "b".into(),
        timings(),
    )
    .await
    .expect("start b");
    assert!(!b.is_leader());
    sleep(RENEW * 3).await;
    assert!(a.is_leader() && !b.is_leader(), "exactly one leader");

    let mut wb = b.watch();
    let dropped = Instant::now();
    drop(a);
    wait_for(&mut wb, true, RENEW * 2 + Duration::from_millis(300)).await;
    assert!(dropped.elapsed() <= RENEW * 2 + Duration::from_millis(300));
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_leader_lost_on_connection_drop() {
    let Some(url) = env_url(PG_VAR) else { return };
    let name = lock_name();
    let l = Leadership::start_with(
        LeaderBackend::Postgres(url.clone()),
        &name,
        "a".into(),
        timings(),
    )
    .await
    .expect("start");
    assert!(l.is_leader());
    let mut w = l.watch();

    let key = pg_advisory_key(&name) as u64;
    let (hi, lo) = ((key >> 32) as i64, (key & 0xffff_ffff) as i64);
    let mut admin = PgConnection::connect(&url).await.expect("admin");
    let killed: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_locks \
         WHERE locktype = 'advisory' AND granted \
           AND classid::bigint = $1 AND objid::bigint = $2 AND objsubid = 1",
    )
    .bind(hi)
    .bind(lo)
    .fetch_all(&mut admin)
    .await
    .expect("terminate");
    assert_eq!(killed, vec![true], "the holder's backend was terminated");

    // Noticed by the next renewal.
    wait_for(&mut w, false, RENEW * 2 + Duration::from_millis(300)).await;
    // With nobody else contending, it reconnects and leads again.
    wait_for(&mut w, true, Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn redis_leader_single_holder() {
    let Some(url) = env_url(REDIS_VAR) else {
        return;
    };
    let name = lock_name();
    let a = Leadership::start_with(
        LeaderBackend::Redis(url.clone()),
        &name,
        "a".into(),
        timings(),
    )
    .await
    .expect("start a");
    assert!(a.is_leader(), "a lone node leads on return");
    let b = Leadership::start_with(
        LeaderBackend::Redis(url.clone()),
        &name,
        "b".into(),
        timings(),
    )
    .await
    .expect("start b");
    assert!(!b.is_leader());
    // Renewals keep the lease alive well past its TTL.
    sleep(Duration::from_millis(1500)).await;
    assert!(a.is_leader() && !b.is_leader(), "exactly one leader");
    assert_eq!(holder(&url, &name).await.as_deref(), Some("a"));

    let mut wb = b.watch();
    drop(a);
    // Released on drop, so b need not wait for the TTL.
    wait_for(&mut wb, true, RENEW * 2 + Duration::from_millis(300)).await;
    assert_eq!(holder(&url, &name).await.as_deref(), Some("b"));
}

#[tokio::test(flavor = "multi_thread")]
async fn redis_leader_expires_and_fails_over() {
    let Some(url) = env_url(REDIS_VAR) else {
        return;
    };
    let name = lock_name();
    let a = Leadership::start_with(
        LeaderBackend::Redis(url.clone()),
        &name,
        "a".into(),
        timings(),
    )
    .await
    .expect("start a");
    let b = Leadership::start_with(
        LeaderBackend::Redis(url.clone()),
        &name,
        "b".into(),
        timings(),
    )
    .await
    .expect("start b");
    assert!(a.is_leader() && !b.is_leader());
    let (mut wa, mut wb) = (a.watch(), b.watch());

    let client = redis::Client::open(url.as_str()).expect("client");
    let mut con = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");
    let _: i64 = redis::cmd("DEL")
        .arg(format!("mockingbird:leader:{name}"))
        .query_async(&mut con)
        .await
        .expect("DEL");

    wait_for(&mut wa, false, RENEW * 2 + Duration::from_millis(300)).await;
    wait_for(&mut wb, true, RENEW * 2 + Duration::from_millis(300)).await;
    sleep(RENEW * 3).await;
    assert!(!a.is_leader() && b.is_leader(), "b took over, a stays out");
    assert_eq!(holder(&url, &name).await.as_deref(), Some("b"));
}

async fn holder(url: &str, name: &str) -> Option<String> {
    let client = redis::Client::open(url).expect("client");
    let mut con = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");
    redis::cmd("GET")
        .arg(format!("mockingbird:leader:{name}"))
        .query_async(&mut con)
        .await
        .expect("GET")
}
