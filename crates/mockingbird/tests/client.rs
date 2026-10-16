#![allow(clippy::unwrap_used)]

//! The admin client subcommands, run as a binary against a live Admin API.

use std::path::PathBuf;

use assert_cmd::Command;
use mockingbird::config::{ServeArgs, build_app};
use serde_json::{Value, json};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/openapi/orders-3.0.yaml")
}

/// Starts an offline server with no mocks and returns its admin base URL.
/// The `App` is leaked so its background tasks outlive this function.
async fn admin_url() -> String {
    let args = ServeArgs {
        host: Some("127.0.0.1".parse().unwrap()),
        port: Some(0),
        admin_port: Some(0),
        offline: true,
        ..ServeArgs::default()
    };
    let app = build_app(&args, None).await.unwrap();
    let router = app.admin.clone();
    Box::leak(Box::new(app));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Output {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {:?}", self.stdout))
    }
}

/// Runs the binary off the async runtime so the server keeps serving.
async fn run(admin: &str, args: &[&str]) -> Output {
    let admin = admin.to_string();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let out = Command::cargo_bin("mockingbird")
            .unwrap()
            .arg("--admin-url")
            .arg(&admin)
            .args(&args)
            .env_remove("MB_ADMIN_URL")
            .env_remove("MB_ADMIN_TOKEN")
            .output()
            .unwrap();
        Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_creates_and_activates() {
    let admin = admin_url().await;
    let contract = fixture();
    let out = run(
        &admin,
        &[
            "import",
            contract.to_str().unwrap(),
            "--as",
            "orders",
            "--activate",
        ],
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let result = out.json();
    assert_eq!(result["version"], 1);
    assert_eq!(result["status"], "active");

    let mock = reqwest::get(format!("{admin}/api/v1/mocks/orders"))
        .await
        .unwrap();
    assert_eq!(mock.status(), 200);
    let mock: Value = mock.json().await.unwrap();
    assert_eq!(mock["active_version"], 1);
    assert_eq!(mock["source"], "api");

    // Importing again into the existing mock adds version 2.
    let out = run(
        &admin,
        &[
            "import",
            contract.to_str().unwrap(),
            "--as",
            "orders",
            "--activate",
        ],
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json()["version"], 2);
    let out = run(&admin, &["mocks", "get", "orders"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json()["active_version"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mocks_list_json() {
    let admin = admin_url().await;
    let out = run(&admin, &["mocks", "list"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json(), json!([]));

    let out = run(
        &admin,
        &[
            "mocks",
            "create",
            "billing",
            "--prefix",
            "/bill",
            "--persona",
            "terse",
        ],
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json()["route_prefix"], "/bill");

    let out = run(&admin, &["mocks", "list"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let list = out.json();
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["name"], "billing");
    assert_eq!(list[0]["persona"], "terse");

    let out = run(&admin, &["mocks", "delete", "billing"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let out = run(&admin, &["mocks", "list"]).await;
    assert_eq!(out.json(), json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_seed_round_trip() {
    let admin = admin_url().await;
    let contract = fixture();
    let out = run(
        &admin,
        &["import", contract.to_str().unwrap(), "--as", "orders"],
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);

    let fixtures = json!([
        {"resource_type": "Order", "id": "o1", "data": {"id": "o1", "total": 10}},
        {"resource_type": "Order", "id": "o2", "data": {"id": "o2", "total": 20}},
    ]);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("fixtures.json");
    std::fs::write(&file, fixtures.to_string()).unwrap();

    let out = run(
        &admin,
        &[
            "seed-fixtures",
            "--mock",
            "orders",
            "--session",
            "alice",
            file.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);

    let out = run(
        &admin,
        &["export-fixtures", "--mock", "orders", "--session", "alice"],
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json(), fixtures);

    let out = run(&admin, &["sessions", "list", "orders"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let sessions = out.json();
    assert_eq!(sessions[0]["session"], "alice");
    assert_eq!(sessions[0]["resources"], 2);

    let out = run(&admin, &["sessions", "reset", "orders", "alice"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let out = run(
        &admin,
        &["export-fixtures", "--mock", "orders", "--session", "alice"],
    )
    .await;
    assert_eq!(out.json(), json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_error_exit_code() {
    let admin = admin_url().await;
    let out = run(&admin, &["mocks", "delete", "nope"]).await;
    assert_eq!(out.code, 1);
    assert!(out.stdout.is_empty(), "stdout: {}", out.stdout);
    assert!(out.stderr.contains("Not Found"), "stderr: {}", out.stderr);
}

#[test]
fn version_flag() {
    let out = Command::cargo_bin("mockingbird")
        .unwrap()
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        format!("mockingbird {}", env!("CARGO_PKG_VERSION"))
    );
}

fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_directory_uploads_archive() {
    let admin = admin_url().await;
    let dir = repo_path("fixtures/proto/multi");
    let out = run(&admin, &["import", dir.to_str().unwrap(), "--as", "orders"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(
        out.json(),
        json!({ "version": 1, "status": "active", "operations": 3, "channels": 0 })
    );
    let source = reqwest::get(format!("{admin}/api/v1/mocks/orders/contracts/1/source"))
        .await
        .unwrap();
    assert_eq!(source.headers()["content-type"], "application/octet-stream");
    assert_eq!(
        source.headers()["x-mockingbird-source-type"],
        "application/gzip"
    );
    assert_eq!(
        source.headers()["content-disposition"],
        r#"attachment; filename="orders-v1.tar.gz""#
    );
    let bytes = source.bytes().await.unwrap();
    assert_eq!(
        bytes.as_ref(),
        mb_import::pack_dir(&dir).unwrap().as_slice()
    );

    // A single .proto file uploads with its own media type.
    let file = repo_path("fixtures/proto/route_guide.proto");
    let out = run(&admin, &["import", file.to_str().unwrap(), "--as", "rg"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json()["operations"], 4);
}

#[test]
fn validate_handles_proto_and_wsdl() {
    let out = Command::cargo_bin("mockingbird")
        .unwrap()
        .arg("validate")
        .arg(repo_path("fixtures/proto/route_guide.proto"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains("/routeguide.RouteGuide/ListFeatures [stream-server]"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "{:<32} /routeguide.RouteGuide/GetFeature\n",
            "RouteGuide.GetFeature"
        )),
        "{stdout}"
    );

    let out = Command::cargo_bin("mockingbird")
        .unwrap()
        .arg("validate")
        .arg(repo_path("fixtures/wsdl/external"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("GetOrder"), "{stdout}");
}

#[test]
fn validate_prints_channels() {
    let out = Command::cargo_bin("mockingbird")
        .unwrap()
        .arg("validate")
        .arg(repo_path("fixtures/asyncapi/streetlights-kafka-3.0.yaml"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.starts_with("Streetlights Kafka API 1.0.0 (0 operations, 4 channels)\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "{:<32} publish kafka smartylighting.streetlights.1.0.action.{{streetlightId}}.turn.on\n",
            "turnOn"
        )),
        "{stdout}"
    );
    assert!(stdout.contains("receiveLightMeasurement"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_asyncapi_uses_asyncapi_media_type() {
    let admin = admin_url().await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("lights.asyncapi.yaml");
    std::fs::copy(
        repo_path("fixtures/asyncapi/streetlights-kafka-3.0.yaml"),
        &file,
    )
    .unwrap();
    let out = run(
        &admin,
        &["import", file.to_str().unwrap(), "--as", "lights"],
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(
        out.json(),
        json!({ "version": 1, "status": "active", "operations": 0, "channels": 4 })
    );
}

#[test]
fn parse_trigger_specs() {
    use mockingbird::client::parse_trigger_spec;
    assert_eq!(
        parse_trigger_spec("every:5s").unwrap(),
        json!({"schedule": {"every_ms": 5000}})
    );
    assert_eq!(
        parse_trigger_spec("every:1m 30s").unwrap(),
        json!({"schedule": {"every_ms": 90000}})
    );
    assert_eq!(
        parse_trigger_spec("cron:*/5 * * * *").unwrap(),
        json!({"cron": {"expr": "*/5 * * * *"}})
    );
    assert_eq!(
        parse_trigger_spec("state:orders").unwrap(),
        json!({"on_state_change": {"resource_type": "orders", "kind": "any"}})
    );
    assert_eq!(
        parse_trigger_spec("state:orders:deleted").unwrap(),
        json!({"on_state_change": {"resource_type": "orders", "kind": "deleted"}})
    );
    assert_eq!(
        parse_trigger_spec("state:orders:upserted").unwrap(),
        json!({"on_state_change": {"resource_type": "orders", "kind": "upserted"}})
    );
    assert_eq!(parse_trigger_spec("manual").unwrap(), json!("manual"));
    for bad in [
        "hourly",
        "every:soon",
        "state:",
        "state:orders:sometimes",
        "cron:",
        "manual:x",
    ] {
        assert_eq!(
            parse_trigger_spec(bad).unwrap_err(),
            format!(
                "unknown trigger spec \"{bad}\" (every:<duration>|cron:<expr>|state:<type>[:kind]|manual)"
            ),
            "{bad}"
        );
    }
}

/// An offline server whose config declares a `mem` memory broker and no
/// mocks; returns its admin base URL.
async fn admin_url_with_memory_broker() -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mockingbird.yaml");
    std::fs::write(&path, "brokers:\n  mem:\n    kind: memory\n").unwrap();
    let args = ServeArgs {
        config: Some(path),
        offline: true,
        ..ServeArgs::default()
    };
    let app = build_app(&args, None).await.unwrap();
    let router = app.admin.clone();
    Box::leak(Box::new((app, dir)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channels_cli_round_trip() {
    let admin = admin_url_with_memory_broker().await;
    let contract = repo_path("fixtures/asyncapi/orders-amqp-2.6.yaml");
    let out = run(
        &admin,
        &["import", contract.to_str().unwrap(), "--as", "orders"],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);

    let out = run(&admin, &["channels", "list", "orders"]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let ids: Vec<String> = out
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&"orderCreated".to_string()), "{ids:?}");

    let out = run(
        &admin,
        &[
            "channels",
            "set",
            "orders",
            "orderCreated",
            "--trigger",
            "every:5s",
            "--trigger",
            "manual",
        ],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);

    let out = run(&admin, &["channels", "get", "orders", "orderCreated"]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let view = out.json();
    assert_eq!(
        view["triggers"],
        json!([{"schedule": {"every_ms": 5000}}, "manual"]),
        "{view}"
    );
    assert_eq!(view["settings_source"], "api");
    assert_eq!(view["enabled"], true);

    // The channel runtime picks the settings up asynchronously.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let messages = loop {
        let out = run(
            &admin,
            &[
                "channels",
                "publish",
                "orders",
                "orderCreated",
                "--count",
                "2",
            ],
        )
        .await;
        if out.code == 0 {
            break out.json();
        }
        assert!(std::time::Instant::now() < deadline, "{}", out.stderr);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert_eq!(
        messages["messages"].as_array().unwrap().len(),
        2,
        "{messages}"
    );

    let out = run(&admin, &["channels", "broker", "orders", "mem"]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json(), json!({"broker": "mem", "source": "api"}));
    let out = run(&admin, &["channels", "broker", "orders"]).await;
    assert_eq!(out.json()["broker"], "mem");
    let out = run(&admin, &["channels", "broker", "orders", "-"]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json()["broker"], Value::Null);

    let out = run(
        &admin,
        &[
            "channels",
            "set",
            "orders",
            "orderCreated",
            "--disable",
            "--contract-triggers",
        ],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let view = out.json();
    assert_eq!(view["enabled"], false);
    assert_eq!(view["triggers"], json!(["manual"]), "{view}");

    let out = run(&admin, &["brokers", "list"]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json()[0]["name"], "mem", "{}", out.stdout);

    // Usage errors exit 2.
    let out = run(
        &admin,
        &[
            "channels",
            "set",
            "orders",
            "orderCreated",
            "--trigger",
            "hourly",
        ],
    )
    .await;
    assert_eq!(out.code, 2, "{}", out.stderr);
    assert!(
        out.stderr.contains("unknown trigger spec"),
        "{}",
        out.stderr
    );
    let out = run(
        &admin,
        &[
            "channels",
            "publish",
            "orders",
            "orderCreated",
            "--context",
            "{nope",
        ],
    )
    .await;
    assert_eq!(out.code, 2, "{}", out.stderr);
}
