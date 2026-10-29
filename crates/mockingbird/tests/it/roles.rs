//! Node roles (`all`, `control`, `data`) and leader-gated work, in process:
//! nodes are built with `config::build_app_with_bus`/`build_app_with_leader`
//! and served with `server::serve_app` on ephemeral listeners. The leader
//! tests use role `all` on one SQLite file and a [`MemoryBusHub`] with a
//! [`LeaderBackend::Manual`] override; the split-role tests need Postgres
//! (`MB_TEST_POSTGRES_URL`, see `scripts/test-postgres.sh`).
#![cfg(test)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::support::{ephemeral, fixture, wait_until};
use mb_ext::Extensions;
use mb_state::{Bus, LeaderBackend, MemoryBusHub};
use mockingbird::config::{App, ServeArgs, build_app_with, build_app_with_leader};
use mockingbird::file_config::Role;
use mockingbird::server::{Listeners, ServeError, binds_for, run_with, serve_app};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// A node served by `serve_app`: its base URLs (the data URL only when
/// its role binds a data listener) and its reconcile counter.
struct Running {
    data: Option<String>,
    admin: String,
    reconciles: Arc<AtomicU64>,
    _stop: watch::Sender<bool>,
}

fn url(listener: &TcpListener) -> String {
    format!("http://{}", listener.local_addr().unwrap())
}

/// Binds what `app.role` binds (per `binds_for`) on ephemeral ports and
/// serves the app.
async fn run(app: App) -> Running {
    let (data, admin, grpc) = binds_for(app.role);
    assert!(admin, "every role binds the admin port");
    let data = match data {
        true => Some(ephemeral().await),
        false => None,
    };
    let grpc = match grpc {
        true => Some(ephemeral().await),
        false => None,
    };
    let admin = ephemeral().await;
    let running_urls = (data.as_ref().map(url), url(&admin));
    let reconciles = app.reconciles.clone();
    let (stop, stop_rx) = watch::channel(false);
    let listeners = Listeners {
        data,
        admin: Some(admin),
        grpc,
        proxy: None,
    };
    tokio::spawn(async move { serve_app(app, listeners, stop_rx).await.unwrap() });
    Running {
        data: running_urls.0,
        admin: running_urls.1,
        reconciles,
        _stop: stop,
    }
}

fn args(path: &Path) -> ServeArgs {
    ServeArgs {
        config: Some(path.to_path_buf()),
        offline: true,
        ..ServeArgs::default()
    }
}

fn sqlite_config(dir: &Path, mocks: &str) -> String {
    format!(
        "storage:\n  url: sqlite://{}\n{mocks}",
        dir.join("mb.db").display()
    )
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

/// A role-`all` node on `dir`'s SQLite file and `bus`, whose leadership
/// follows `leader`.
async fn leader_node(
    dir: &Path,
    file: &str,
    mocks: &str,
    bus: Arc<dyn Bus>,
    leader: watch::Receiver<bool>,
    watch: bool,
) -> App {
    let path = dir.join(file);
    std::fs::write(&path, sqlite_config(dir, mocks)).unwrap();
    let args = ServeArgs {
        watch,
        ..args(&path)
    };
    build_app_with_leader(
        &args,
        None,
        Extensions::default(),
        bus,
        LeaderBackend::Manual(leader),
    )
    .await
    .unwrap()
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
        tokio::time::sleep(Duration::from_millis(50)).await;
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

#[test]
fn control_role_binds_no_data_listener() {
    assert_eq!(binds_for(Role::Control), (false, true, false));
    assert_eq!(binds_for(Role::Data), (true, true, true));
    assert_eq!(binds_for(Role::All), (true, true, true));
}

#[tokio::test]
async fn data_role_rejects_watch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mockingbird.yaml");
    // Valid split-role storage; nothing is connected before the check.
    std::fs::write(
        &path,
        "server:\n  role: data\nstorage:\n  url: postgres://127.0.0.1:1/mb\n",
    )
    .unwrap();
    let args = ServeArgs {
        watch: true,
        ..args(&path)
    };
    match run_with(args, None, Extensions::default()).await {
        Err(ServeError::Usage(msg)) => assert!(msg.contains("--watch"), "{msg}"),
        other => panic!("expected a usage error, got {other:?}"),
    }
}

#[tokio::test]
async fn only_leader_reconciles() {
    let dir = tempfile::tempdir().unwrap();
    let hub = MemoryBusHub::new();
    let mocks = mocks_section(&[("orders", "openapi/orders-3.0.yaml")]);
    let (_lead_tx, lead_rx) = watch::channel(true);
    let (_follow_tx, follow_rx) = watch::channel(false);
    let a = leader_node(
        dir.path(),
        "a.yaml",
        &mocks,
        hub.join("node-a".into()),
        lead_rx,
        false,
    )
    .await;
    let b = leader_node(
        dir.path(),
        "b.yaml",
        &mocks,
        hub.join("node-b".into()),
        follow_rx,
        false,
    )
    .await;
    let (a, b) = (run(a).await, run(b).await);
    // b serves what a reconciled into the shared store.
    wait_status(
        &format!("{}/orders/orders/o1", b.data.as_deref().unwrap()),
        200,
        Duration::from_secs(2),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(a.reconciles.load(Ordering::SeqCst), 1);
    assert_eq!(b.reconciles.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn leader_gain_starts_watch() {
    let dir = tempfile::tempdir().unwrap();
    let hub = MemoryBusHub::new();
    let (leader_tx, leader_rx) = watch::channel(false);
    let app = leader_node(
        dir.path(),
        "mockingbird.yaml",
        "",
        hub.join("node".into()),
        leader_rx,
        true,
    )
    .await;
    let node = run(app).await;
    let data = node.data.clone().unwrap();
    assert_eq!(node.reconciles.load(Ordering::SeqCst), 0);

    leader_tx.send(true).unwrap();
    wait_until("the new leader reconciles", Duration::from_secs(3), || {
        node.reconciles.load(Ordering::SeqCst) == 1
    })
    .await;
    // The watcher starts after the reconcile; an edit made before it
    // watches is missed, so the edit is repeated until it is applied.
    let mocks = mocks_section(&[("orders", "openapi/orders-3.0.yaml")]);
    let url = format!("{data}/orders/orders/o1");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut rewrite = Instant::now();
    loop {
        if Instant::now() >= rewrite {
            std::fs::write(
                dir.path().join("mockingbird.yaml"),
                sqlite_config(dir.path(), &mocks),
            )
            .unwrap();
            rewrite = Instant::now() + mockingbird::watch::DEBOUNCE * 2;
        }
        if status(&url).await == 200 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the leader's watch never applied the edit"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Losing leadership stops the watch: a later change is not applied.
    leader_tx.send(false).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    std::fs::write(
        dir.path().join("mockingbird.yaml"),
        sqlite_config(dir.path(), ""),
    )
    .unwrap();
    // Nothing to wait for: twice the debounce, and the edit stays unapplied.
    tokio::time::sleep(mockingbird::watch::DEBOUNCE * 2).await;
    assert_eq!(status(&url).await, 200);
}

#[tokio::test]
async fn readyz_reports_role_and_leader() {
    let dir = tempfile::tempdir().unwrap();
    let hub = MemoryBusHub::new();
    let (leader_tx, leader_rx) = watch::channel(false);
    let app = leader_node(
        dir.path(),
        "mockingbird.yaml",
        "",
        hub.join("node".into()),
        leader_rx,
        false,
    )
    .await;
    let node = run(app).await;
    let body = readyz(&node.admin).await;
    assert_eq!(body["status"], "ready", "{body}");
    assert_eq!(body["role"], "all", "{body}");
    assert_eq!(body["leader"], false, "{body}");

    leader_tx.send(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let body = readyz(&node.admin).await;
        if body["leader"] == true {
            assert_eq!(body["role"], "all", "{body}");
            break;
        }
        assert!(Instant::now() < deadline, "leader never reported: {body}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// --- Split roles over Postgres -------------------------------------------

/// `MB_TEST_POSTGRES_URL` with a fresh schema as its search path.
async fn pg_url() -> Option<String> {
    use sqlx::Connection;
    let Ok(base) = std::env::var("MB_TEST_POSTGRES_URL") else {
        eprintln!("skipped: MB_TEST_POSTGRES_URL not set");
        return None;
    };
    let schema = format!("mb_roles_{}", uuid::Uuid::new_v4().simple());
    let mut conn = sqlx::postgres::PgConnection::connect(&base).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    let sep = if base.contains('?') { '&' } else { '?' };
    Some(format!("{base}{sep}options=-c%20search_path%3D{schema}"))
}

/// A node with `role` on Postgres `url` (state and bus too).
async fn pg_node(dir: &Path, role: &str, url: &str, extra: &str) -> App {
    let path = dir.join(format!("{role}.yaml"));
    std::fs::write(
        &path,
        format!("server:\n  role: {role}\nstorage:\n  url: \"{url}\"\n{extra}"),
    )
    .unwrap();
    build_app_with(&args(&path), None, Extensions::default())
        .await
        .unwrap()
}

/// Serialises the Postgres tests: they contend for the one `control` lock.
static PG_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test(flavor = "multi_thread")]
async fn data_role_serves_ops_only_on_admin_port() {
    let Some(url) = pg_url().await else { return };
    let _serial = PG_SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let node = run(pg_node(dir.path(), "data", &url, "").await).await;
    assert_eq!(status(&format!("{}/api/v1/mocks", node.admin)).await, 404);
    assert_eq!(status(&format!("{}/healthz", node.admin)).await, 200);
    assert_eq!(status(&format!("{}/readyz", node.admin)).await, 200);
    assert_eq!(status(&format!("{}/metrics", node.admin)).await, 200);
    let body = readyz(&node.admin).await;
    assert_eq!(body["role"], "data", "{body}");
    assert_eq!(body["leader"], false, "{body}");
    assert_eq!(node.reconciles.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn control_publish_only_websocket_channel_409() {
    let Some(url) = pg_url().await else { return };
    let _serial = PG_SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let mocks = mocks_section(&[
        ("chat", "asyncapi/chat-ws-2.6.yaml"),
        ("orders", "asyncapi/orders-amqp-2.6.yaml"),
    ]);
    let extra = format!("brokers:\n  mem:\n    kind: memory\n{mocks}");
    let node = run(pg_node(dir.path(), "control", &url, &extra).await).await;
    assert!(node.data.is_none());
    let http = reqwest::Client::new();

    let r = http
        .post(format!(
            "{}/api/v1/mocks/chat/channels/chatMessage/publish",
            node.admin
        ))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let problem: Value = r.json().await.unwrap();
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("WebSocket channels are served by data nodes"),
        "{problem}"
    );

    // A broker channel publishes from a control node.
    let r = http
        .post(format!(
            "{}/api/v1/mocks/orders/channels/orderCancelled/publish",
            node.admin
        ))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn split_roles_over_postgres() {
    let Some(url) = pg_url().await else { return };
    let _serial = PG_SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let control = run(pg_node(dir.path(), "control", &url, "").await).await;
    let data = run(pg_node(dir.path(), "data", &url, "").await).await;
    assert_eq!(control.reconciles.load(Ordering::SeqCst), 1);
    assert_eq!(readyz(&control.admin).await["leader"], true);

    let http = reqwest::Client::new();
    let r = http
        .post(format!("{}/api/v1/mocks", control.admin))
        .json(&json!({"name": "split"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let r = http
        .post(format!("{}/api/v1/mocks/split/contracts", control.admin))
        .header("content-type", "application/yaml")
        .body(std::fs::read(fixture("openapi/orders-3.0.yaml")).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());

    wait_status(
        &format!("{}/split/orders/o1", data.data.as_deref().unwrap()),
        200,
        Duration::from_secs(3),
    )
    .await;
}
