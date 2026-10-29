//! A whole HA cluster in process: `control` and `data` nodes built from
//! their configuration files (Postgres control store, Redis state store and
//! Redis bus, so leader election uses the Redis lease) and served with
//! `server::serve_app` on ephemeral ports. Gated on both
//! `MB_TEST_POSTGRES_URL` and `MB_TEST_REDIS_URL`; run with
//! `MB_TEST_RUNNER=scripts/test-postgres.sh scripts/test-redis.sh -p mockingbird-cli --test it -- ha_cluster::`.
#![cfg(test)]

use std::path::Path;
use std::time::{Duration, Instant};

use crate::support::{ephemeral, fixture, wait_until};
use mb_ext::Extensions;
use mb_store::DEFAULT_WORKSPACE;
use mockingbird::config::{ServeArgs, build_app_with};
use mockingbird::server::{Listeners, binds_for, serve_app};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The tests share the Redis bus channel, the leader key and (in the
/// outage test) every pub/sub connection on the server, so they run one at
/// a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The test's Postgres schema, dropped (with everything in it) when the
/// test ends, passed or not.
struct Schema {
    /// `MB_TEST_POSTGRES_URL` as given.
    server: String,
    name: String,
}

impl Drop for Schema {
    fn drop(&mut self) {
        let (server, name) = (self.server.clone(), self.name.clone());
        // Drop can neither await nor block the test's runtime: drop the
        // schema from a thread with its own.
        let dropped = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async {
                use sqlx::Connection;
                let mut conn = sqlx::postgres::PgConnection::connect(&server).await?;
                // A node still running (after a failed assertion) may hold
                // a lock: give up rather than hang.
                let sql = format!("SET lock_timeout = '5s'; DROP SCHEMA IF EXISTS {name} CASCADE");
                sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                    .execute(&mut conn)
                    .await?;
                conn.close().await?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            })
        })
        .join();
        if !matches!(dropped, Ok(Ok(()))) {
            eprintln!("could not drop schema {}: {dropped:?}", self.name);
        }
    }
}

/// `MB_TEST_POSTGRES_URL` (with a fresh schema as its search path, dropped
/// with the returned [`Schema`]) and `MB_TEST_REDIS_URL`, or `None`
/// (skipped) when either is unset.
async fn backends() -> Option<(String, String, Schema)> {
    use sqlx::Connection;
    let (Ok(pg), Ok(redis)) = (
        std::env::var("MB_TEST_POSTGRES_URL"),
        std::env::var("MB_TEST_REDIS_URL"),
    ) else {
        eprintln!("skipped: MB_TEST_POSTGRES_URL and MB_TEST_REDIS_URL must both be set");
        return None;
    };
    let schema = format!("mb_cluster_{}", uuid::Uuid::new_v4().simple());
    let mut conn = sqlx::postgres::PgConnection::connect(&pg).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    let sep = if pg.contains('?') { '&' } else { '?' };
    Some((
        format!("{pg}{sep}options=-c%20search_path%3D{schema}"),
        redis,
        Schema {
            server: pg,
            name: schema,
        },
    ))
}

/// A served node: its base URLs (data only on a `data` node), whether it
/// leads, and the handle to stop it.
struct Node {
    data: Option<String>,
    admin: String,
    leader: watch::Receiver<bool>,
    stop: watch::Sender<bool>,
    served: JoinHandle<()>,
}

impl Node {
    fn is_leader(&self) -> bool {
        *self.leader.borrow()
    }

    fn data(&self) -> &str {
        self.data.as_deref().unwrap()
    }

    /// Stops serving and waits until the app has shut down (which drops
    /// its leadership).
    async fn kill(self) {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(15), self.served)
            .await
            .unwrap()
            .unwrap();
    }
}

fn url(listener: &TcpListener) -> String {
    format!("http://{}", listener.local_addr().unwrap())
}

/// Builds node `name` with `role` on `pg` (control store) and `redis`
/// (state store and bus) and serves it on ephemeral ports.
async fn start(dir: &Path, name: &str, role: &str, pg: &str, redis: &str) -> Node {
    let path = dir.join(format!("{name}.yaml"));
    std::fs::write(
        &path,
        format!(
            "server:\n  role: {role}\nstorage:\n  url: \"{pg}\"\n  state_url: \"{redis}\"\nbus:\n  url: \"{redis}\"\n"
        ),
    )
    .unwrap();
    let args = ServeArgs {
        config: Some(path),
        offline: true,
        ..ServeArgs::default()
    };
    let app = build_app_with(&args, None, Extensions::default())
        .await
        .unwrap();
    let (data, _, grpc) = binds_for(app.role);
    let data = match data {
        true => Some(ephemeral().await),
        false => None,
    };
    let grpc = match grpc {
        true => Some(ephemeral().await),
        false => None,
    };
    let admin = ephemeral().await;
    let (data_url, admin_url) = (data.as_ref().map(url), url(&admin));
    let leader = app.leadership.watch();
    let (stop, stop_rx) = watch::channel(false);
    let listeners = Listeners {
        data,
        admin: Some(admin),
        grpc,
        proxy: None,
    };
    let served = tokio::spawn(async move { serve_app(app, listeners, stop_rx).await.unwrap() });
    Node {
        data: data_url,
        admin: admin_url,
        leader,
        stop,
        served,
    }
}

async fn status(url: &str) -> u16 {
    reqwest::get(url).await.unwrap().status().as_u16()
}

/// Polls `url` until it answers `want`, for at most `within`.
async fn wait_status(url: &str, want: u16, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        let got = status(url).await;
        if got == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{url} answered {got}, not {want}, within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn readyz(admin: &str) -> Value {
    reqwest::get(format!("{admin}/readyz"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
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

/// Reads `resp` (an SSE stream) until everything read contains every one
/// of `needles`, for at most `within`; returns everything read.
async fn read_until(resp: &mut reqwest::Response, needles: &[&str], within: Duration) -> String {
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + within;
    while !needles.iter().all(|n| text.contains(n)) {
        match tokio::time::timeout_at(deadline, resp.chunk()).await {
            Ok(Ok(Some(chunk))) => text.push_str(&String::from_utf8_lossy(&chunk)),
            _ => break,
        }
    }
    text
}

/// Waits until no node holds the Redis leader lease, so the next test's
/// control nodes need not wait for it to expire: a node releases it on
/// shutdown from a spawned task.
async fn wait_lease_released(redis: &str) {
    let client = redis::Client::open(redis).unwrap();
    let mut con = client.get_multiplexed_async_connection().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let held: bool = redis::cmd("EXISTS")
            .arg("mockingbird:leader:control")
            .query_async(&mut con)
            .await
            .unwrap();
        if !held {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the leader lease was not released"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A unique mock name, so runs against one Redis server never share state.
fn mock_name(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn cluster_end_to_end() {
    let Some((pg, redis, _schema)) = backends().await else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let control1 = start(dir.path(), "control1", "control", &pg, &redis).await;
    let control2 = start(dir.path(), "control2", "control", &pg, &redis).await;
    let data1 = start(dir.path(), "data1", "data", &pg, &redis).await;
    let data2 = start(dir.path(), "data2", "data", &pg, &redis).await;

    // Exactly one control node leads (a lease a crashed earlier run left
    // behind expires within its 15 s TTL plus one 5 s retry).
    wait_until("a control node leads", Duration::from_secs(25), || {
        control1.is_leader() || control2.is_leader()
    })
    .await;
    for _ in 0..10 {
        assert!(
            !(control1.is_leader() && control2.is_leader()),
            "both control nodes lead"
        );
        assert!(!data1.is_leader() && !data2.is_leader());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let leaders: Vec<bool> = [&control1, &control2]
        .iter()
        .map(|n| n.is_leader())
        .collect();
    assert_eq!(leaders.iter().filter(|l| **l).count(), 1, "{leaders:?}");
    for (node, leads) in [(&control1, leaders[0]), (&control2, leaders[1])] {
        let body = readyz(&node.admin).await;
        assert_eq!(body["role"], "control", "{body}");
        assert_eq!(body["leader"], leads, "{body}");
    }

    // A mock created on control 1 is served by both data nodes.
    let mock = mock_name("cluster");
    create_orders_mock(&control1.admin, &mock).await;
    for data in [&data1, &data2] {
        wait_status(
            &format!("{}/{mock}/orders/o1", data.data()),
            200,
            Duration::from_secs(5),
        )
        .await;
    }

    // The live feed on control 2 sees calls served by either data node.
    let mut feed = reqwest::get(format!(
        "{}/api/v1/mocks/{mock}/requests/stream",
        control2.admin
    ))
    .await
    .unwrap();
    assert_eq!(feed.status(), 200);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A resource created through data 1 is visible through data 2 in the
    // same session.
    let http = reqwest::Client::new();
    let r = http
        .post(format!(
            "{}/{mock}/orders?marker=post-via-data1",
            data1.data()
        ))
        .header("x-mockingbird-session", "cluster-s1")
        .json(&json!({"customerId": "c1", "total": 10}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let created: Value = r.json().await.unwrap();
    let id = created["id"].as_str().unwrap().to_owned();
    let r = http
        .get(format!(
            "{}/{mock}/orders/{id}?marker=get-via-data2",
            data2.data()
        ))
        .header("x-mockingbird-session", "cluster-s1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let fetched: Value = r.json().await.unwrap();
    assert_eq!(fetched, created);

    let text = read_until(
        &mut feed,
        &["post-via-data1", "get-via-data2"],
        Duration::from_secs(5),
    )
    .await;
    assert!(text.contains("post-via-data1"), "{text}");
    assert!(text.contains("get-via-data2"), "{text}");
    drop(feed);

    // Dropping the leader hands leadership to the other control node.
    let (leader, survivor) = match leaders[0] {
        true => (control1, control2),
        false => (control2, control1),
    };
    leader.kill().await;
    wait_until(
        "the surviving control node leads",
        Duration::from_secs(20),
        || survivor.is_leader(),
    )
    .await;
    assert_eq!(readyz(&survivor.admin).await["leader"], true);

    // Deleting the mock stops it on both data nodes.
    let r = http
        .delete(format!("{}/api/v1/mocks/{mock}", survivor.admin))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    for data in [&data1, &data2] {
        wait_status(
            &format!("{}/{mock}/orders/o1", data.data()),
            404,
            Duration::from_secs(3),
        )
        .await;
    }

    for node in [survivor, data1, data2] {
        node.kill().await;
    }
    wait_lease_released(&redis).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cluster_redis_bus_outage_degrades_then_recovers() {
    let Some((pg, redis, _schema)) = backends().await else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let control = start(dir.path(), "control", "control", &pg, &redis).await;
    let data = start(dir.path(), "data", "data", &pg, &redis).await;

    let mock = mock_name("outage");
    create_orders_mock(&control.admin, &mock).await;
    let on_data = format!("{}/{mock}/orders/o1", data.data());
    wait_status(&on_data, 200, Duration::from_secs(5)).await;
    let body = readyz(&data.admin).await;
    assert_eq!(body["status"], "ready", "{body}");

    // Deleted behind every node's back: nothing is announced, so only a
    // resync can notice.
    let raw = mb_store::connect(&pg).await.unwrap();
    assert!(raw.delete_mock(DEFAULT_WORKSPACE, &mock).await.unwrap());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(status(&on_data).await, 200);

    // Cut every pub/sub connection on the server.
    let client = redis::Client::open(redis.as_str()).unwrap();
    let mut con = client.get_multiplexed_async_connection().await.unwrap();
    let killed: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("TYPE")
        .arg("pubsub")
        .query_async(&mut con)
        .await
        .unwrap();
    assert!(killed >= 1, "no pub/sub client was killed");

    // Degraded while the bus reconnects (after a 1 s backoff)...
    let deadline = Instant::now() + Duration::from_secs(5);
    let degraded = loop {
        let body = readyz(&data.admin).await;
        if body["status"] == "degraded" {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "data node never reported the bus outage: {body}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(
        degraded["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().starts_with("bus: ")),
        "{degraded}"
    );

    // ...then ready again, and the resync on reconnect reloaded the mocks.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let body = readyz(&data.admin).await;
        if body["status"] == "ready" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "data node did not recover: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait_status(&on_data, 404, Duration::from_secs(3)).await;

    control.kill().await;
    data.kill().await;
    wait_lease_released(&redis).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cluster_schema_is_dropped_on_teardown() {
    use sqlx::Connection;
    let Some((pg, _redis, schema)) = backends().await else {
        return;
    };
    let (server, name) = (schema.server.clone(), schema.name.clone());
    // The schema is in use, with tables in it, when it is dropped.
    mb_store::connect(&pg).await.unwrap();
    let mut conn = sqlx::postgres::PgConnection::connect(&server)
        .await
        .unwrap();
    let count = "SELECT count(*) FROM pg_namespace WHERE nspname = $1";
    let before: i64 = sqlx::query_scalar(count)
        .bind(&name)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(before, 1);
    drop(schema);
    let after: i64 = sqlx::query_scalar(count)
        .bind(&name)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(after, 0, "schema {name} left behind");
}
