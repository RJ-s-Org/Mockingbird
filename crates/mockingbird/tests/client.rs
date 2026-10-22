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
    run_with(admin, args, &[], None).await
}

/// [`run`] with extra environment variables and optional stdin.
async fn run_with(admin: &str, args: &[&str], env: &[(&str, &str)], stdin: Option<&str>) -> Output {
    let admin = admin.to_string();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let stdin = stdin.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::cargo_bin("mockingbird").unwrap();
        cmd.arg("--admin-url")
            .arg(&admin)
            .args(&args)
            .env_remove("MB_ADMIN_URL")
            .env_remove("MB_ADMIN_TOKEN")
            .env_remove("MB_WORKSPACE")
            .envs(env);
        if let Some(input) = stdin {
            cmd.write_stdin(input);
        }
        let out = cmd.output().unwrap();
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

#[test]
fn empty_or_invalid_workspace_is_a_usage_error() {
    // Nothing listens here: the arguments must be refused before any request.
    let admin = ["--admin-url", "http://127.0.0.1:9"];
    let cases: [(&[&str], Option<&str>); 3] = [
        (&["--workspace", "", "mocks", "list"], None),
        (&["--workspace", "Bad_Name", "mocks", "list"], None),
        (&["mocks", "list"], Some("")),
    ];
    for (args, env) in cases {
        let mut cmd = Command::cargo_bin("mockingbird").unwrap();
        cmd.env_remove("MB_WORKSPACE").args(admin).args(args);
        if let Some(value) = env {
            cmd.env("MB_WORKSPACE", value);
        }
        let out = cmd.output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?} {env:?}: {stderr}");
        assert!(stderr.contains("--workspace"), "{stderr}");
        assert!(stderr.contains("workspace name"), "{stderr}");
    }
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
        json!({ "version": 1, "status": "active", "operations": 3, "channels": 0, "warnings": [] })
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

fn smart_fixture(name: &str) -> PathBuf {
    repo_path(&format!("fixtures/smart/{name}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_har_waits_for_draft() {
    let admin = admin_url().await;
    let har = smart_fixture("petstore.har");
    let out = run(&admin, &["import", har.to_str().unwrap(), "--as", "pets"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let summary = out.json();
    assert_eq!(summary["version"], 1, "{summary}");
    assert_eq!(summary["status"], "draft", "{summary}");
    assert_eq!(summary["approved"], false, "{summary}");
    assert!(summary["job"].as_str().is_some_and(|j| !j.is_empty()));
    let ops: Vec<&str> = summary["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o.as_str().unwrap())
        .collect();
    assert!(ops.contains(&"GET /api/pets/{petId}"), "{ops:?}");
    assert!(summary["warnings"].is_array());

    let mock: Value = reqwest::get(format!("{admin}/api/v1/mocks/pets"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mock["source"], "api");
    assert_eq!(mock["active_version"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_postman_approve_activate() {
    let admin = admin_url().await;
    let collection = smart_fixture("legacy-v2.0.postman_collection.json");
    let out = run(
        &admin,
        &[
            "import",
            collection.to_str().unwrap(),
            "--as",
            "legacy",
            "--approve",
            "--activate",
        ],
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let summary = out.json();
    assert_eq!(summary["status"], "active", "{summary}");
    assert_eq!(summary["approved"], true, "{summary}");
    assert_eq!(summary["warnings"], json!([]), "{summary}");

    let mock: Value = reqwest::get(format!("{admin}/api/v1/mocks/legacy"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mock["active_version"], summary["version"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_with_warnings_not_approved() {
    let admin = admin_url().await;
    let har = smart_fixture("petstore.har");
    let out = run(
        &admin,
        &["import", har.to_str().unwrap(), "--as", "pets", "--approve"],
    )
    .await;
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    let summary = out.json();
    assert_eq!(summary["approved"], false, "{summary}");
    assert_eq!(summary["status"], "draft", "{summary}");
    let warnings = summary["warnings"].as_array().unwrap().len();
    assert!(warnings > 0, "{summary}");
    assert!(
        out.stderr.contains("not approved"),
        "stderr: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains(&format!("{warnings} warning")),
        "stderr: {}",
        out.stderr
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_markdown_offline_fails() {
    let admin = admin_url().await;
    let doc = smart_fixture("petstore.md");
    let out = run(&admin, &["import", doc.to_str().unwrap(), "--as", "pets"]).await;
    assert_eq!(out.code, 1, "stdout: {}", out.stdout);
    assert!(out.stderr.contains("LLM profile"), "stderr: {}", out.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_activate_without_approve_rejected() {
    let admin = admin_url().await;
    let har = smart_fixture("petstore.har");
    let out = run(
        &admin,
        &[
            "import",
            har.to_str().unwrap(),
            "--as",
            "pets",
            "--activate",
        ],
    )
    .await;
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("--activate needs --approve for document, HAR and Postman imports"),
        "stderr: {}",
        out.stderr
    );
    // Rejected before anything was created.
    let out = run(&admin, &["mocks", "list"]).await;
    assert_eq!(out.json(), json!([]));

    let out = run(
        &admin,
        &[
            "import",
            "https://docs.example.com/api",
            "--as",
            "pets",
            "--activate",
        ],
    )
    .await;
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
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
        json!({ "version": 1, "status": "active", "operations": 0, "channels": 4, "warnings": [] })
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

/// Creates workspace `name` over HTTP.
async fn create_workspace(admin: &str, name: &str) {
    let resp = reqwest::Client::new()
        .post(format!("{admin}/api/v1/workspaces"))
        .json(&json!({ "name": name }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());
}

/// The mock names `mocks list` prints.
fn names(out: &Output) -> Vec<String> {
    out.json()
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_flag_targets_workspace() {
    let admin = admin_url().await;
    create_workspace(&admin, "team-a").await;
    let out = run(&admin, &["--workspace", "team-a", "mocks", "create", "m1"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);

    let out = run(&admin, &["--workspace", "team-a", "mocks", "list"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(names(&out), ["m1"]);
    let out = run(&admin, &["mocks", "list", "--workspace", "default"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(names(&out).is_empty(), "{}", out.stdout);
    let out = run(&admin, &["mocks", "list"]).await;
    assert!(names(&out).is_empty(), "{}", out.stdout);

    let resp = reqwest::get(format!("{admin}/api/v1/workspaces/team-a/mocks/m1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mb_workspace_env_is_honoured() {
    let admin = admin_url().await;
    create_workspace(&admin, "team-b").await;
    let env = [("MB_WORKSPACE", "team-b")];
    let out = run_with(&admin, &["mocks", "create", "m2"], &env, None).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let out = run_with(&admin, &["mocks", "list"], &env, None).await;
    assert_eq!(names(&out), ["m2"]);
    let out = run(&admin, &["mocks", "list"]).await;
    assert!(names(&out).is_empty(), "{}", out.stdout);
    // The flag wins over the environment.
    let out = run_with(
        &admin,
        &["--workspace", "default", "mocks", "list"],
        &env,
        None,
    )
    .await;
    assert!(names(&out).is_empty(), "{}", out.stdout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenarios_put_list_delete() {
    let admin = admin_url().await;
    create_workspace(&admin, "team-a").await;
    let ws = ["--workspace", "team-a"];
    let out = run(&admin, &[&ws[..], &["mocks", "create", "orders"]].concat()).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("slow.json");
    std::fs::write(&file, r#"{"latency_ms": 250, "status": 503}"#).unwrap();
    let out = run(
        &admin,
        &[
            &ws[..],
            &[
                "scenarios",
                "put",
                "orders",
                "slow",
                "--file",
                file.to_str().unwrap(),
            ],
        ]
        .concat(),
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json()["latency_ms"], 250);

    // `--file -` reads stdin.
    let out = run_with(
        &admin,
        &[
            &ws[..],
            &["scenarios", "put", "orders", "chatty", "--file", "-"],
        ]
        .concat(),
        &[],
        Some(r#"{"instructions": "be chatty"}"#),
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);

    let out = run(
        &admin,
        &[&ws[..], &["scenarios", "list", "orders"]].concat(),
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(names(&out), ["chatty", "slow"]);

    let out = run(
        &admin,
        &[&ws[..], &["scenarios", "get", "orders", "slow"]].concat(),
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let slow = out.json();
    assert_eq!(slow["name"], "slow");
    assert_eq!(slow["status"], 503);

    let out = run(
        &admin,
        &[&ws[..], &["scenarios", "delete", "orders", "slow"]].concat(),
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let out = run(
        &admin,
        &[&ws[..], &["scenarios", "list", "orders"]].concat(),
    )
    .await;
    assert_eq!(names(&out), ["chatty"]);

    // The scenarios live in team-a, not in default.
    let out = run(&admin, &["scenarios", "list", "orders"]).await;
    assert_eq!(out.code, 1, "{}", out.stdout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenarios_get_unknown_is_usage_error() {
    let admin = admin_url().await;
    let out = run(&admin, &["mocks", "create", "orders"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let out = run(&admin, &["scenarios", "get", "orders", "nope"]).await;
    assert_eq!(out.code, 2, "stdout: {}", out.stdout);
    assert!(
        out.stderr
            .contains("scenario nope not found in mock orders"),
        "{}",
        out.stderr
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profiles_put_get_delete() {
    let admin = admin_url().await;
    create_workspace(&admin, "team-a").await;
    let ws = ["--workspace", "team-a"];
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("fast.json");
    std::fs::write(
        &file,
        r#"{"base_url": "https://llm.example.com/v1", "model": "small", "timeout_ms": 1500}"#,
    )
    .unwrap();
    let out = run(
        &admin,
        &[
            &ws[..],
            &["profiles", "put", "fast", "--file", file.to_str().unwrap()],
        ]
        .concat(),
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json()["model"], "small");

    let out = run(&admin, &[&ws[..], &["profiles", "get", "fast"]].concat()).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json()["timeout_ms"], 1500);

    let out = run(&admin, &[&ws[..], &["profiles", "list"]].concat()).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(names(&out).contains(&"fast".to_string()), "{}", out.stdout);
    let out = run(&admin, &["profiles", "list"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(!names(&out).contains(&"fast".to_string()), "{}", out.stdout);

    let out = run(&admin, &[&ws[..], &["profiles", "delete", "fast"]].concat()).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let out = run(&admin, &[&ws[..], &["profiles", "get", "fast"]].concat()).await;
    assert_eq!(out.code, 1, "{}", out.stdout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn brokers_list_stays_global() {
    let admin = admin_url_with_memory_broker().await;
    create_workspace(&admin, "team-a").await;
    let out = run(&admin, &["--workspace", "team-a", "brokers", "list"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json()[0]["name"], "mem", "{}", out.stdout);
}

#[test]
fn validate_prints_schema_warnings() {
    let out = Command::cargo_bin("mockingbird")
        .unwrap()
        .arg("validate")
        .arg(repo_path("fixtures/openapi/huge-response.json"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("warning: ") && l.contains("getBig")),
        "{stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_approve_activates_unless_no_activate() {
    let admin = admin_url().await;
    let c = smart_fixture("legacy-v2.0.postman_collection.json");
    let out = run(
        &admin,
        &["import", c.to_str().unwrap(), "--as", "a", "--approve"],
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json()["status"], "active");
    let out = run(
        &admin,
        &[
            "import",
            c.to_str().unwrap(),
            "--as",
            "b",
            "--approve",
            "--no-activate",
            "--mode",
            "replace",
        ],
    )
    .await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json()["status"], "approved");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_docs_folder_goes_to_imports() {
    let admin = admin_url().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), "# A\n\nGET /a returns a.").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/b.html"), "<h1>B</h1>").unwrap();
    let out = run(
        &admin,
        &["import", dir.path().to_str().unwrap(), "--as", "docs"],
    )
    .await;
    assert_eq!(out.code, 1, "stdout: {}", out.stdout);
    // Offline: documents need an LLM.
    assert!(out.stderr.contains("LLM profile"), "stderr: {}", out.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_sources_must_be_urls() {
    let admin = admin_url().await;
    let har = smart_fixture("petstore.har");
    let out = run(
        &admin,
        &[
            "import",
            har.to_str().unwrap(),
            "https://a.example/x",
            "--as",
            "m",
        ],
    )
    .await;
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("several sources must all be http(s) URLs"),
        "{}",
        out.stderr
    );
}
