#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use assert_cmd::Command;
use mockingbird::config::{LlmEnv, ServeArgs, build_app};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/openapi/orders-3.0.yaml")
}

fn args(contracts: Vec<PathBuf>) -> ServeArgs {
    ServeArgs {
        host: Some("127.0.0.1".parse().unwrap()),
        port: Some(0),
        insecure_no_auth: false,
        offline: false,
        llm_timeout: Some("3s".parse().unwrap()),
        contracts,
        ..ServeArgs::default()
    }
}

fn llm(base: String) -> LlmEnv {
    LlmEnv {
        base_url: base,
        model: "m".into(),
        api_key: Some("k".into()),
        json_schema: false,
    }
}

fn completion(order: Value) -> Value {
    json!({"choices":[{"message":{"content": order.to_string()}}],
           "usage":{"prompt_tokens":1,"completion_tokens":1}})
}

fn order(id: &str) -> Value {
    json!({"id": id, "customerId": "c1", "status": "pending", "total": 10})
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn e2e_create_get_delete_with_wiremock_llm() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion(order("o1"))))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion(order("o2"))))
        .with_priority(2)
        .mount(&mock)
        .await;

    let app = build_app(&args(vec![fixture()]), Some(llm(mock.uri())))
        .await
        .unwrap()
        .data;
    let base = serve(app).await;
    let http = reqwest::Client::new();

    let r = http
        .post(format!("{base}/orders"))
        .json(&json!({"customerId": "c1", "total": 10}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    assert_eq!(r.headers()["x-mockingbird-source"], "llm");

    let r = http.get(format!("{base}/orders/o1")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["id"], "o1");

    let r = http
        .delete(format!("{base}/orders/o1"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let r = http
        .get(format!("{base}/orders/o1"))
        .header("X-Mockingbird-Status", "404")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn e2e_llm_down_falls_back() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock)
        .await;
    let app = build_app(&args(vec![fixture()]), Some(llm(mock.uri())))
        .await
        .unwrap()
        .data;
    let base = serve(app).await;

    let r = reqwest::get(format!("{base}/orders/x")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-mockingbird-source"], "fallback");
    let body: Value = r.json().await.unwrap();
    let obj = body.as_object().unwrap();
    for key in ["id", "customerId", "status", "total"] {
        assert!(obj.contains_key(key), "missing {key}: {body}");
    }
    assert!(body["total"].is_number());
    assert!(["pending", "shipped"].contains(&body["status"].as_str().unwrap()));
}

#[tokio::test]
async fn multiple_contracts_are_prefixed() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.yaml");
    let b = dir.path().join("b.yaml");
    std::fs::copy(fixture(), &a).unwrap();
    std::fs::copy(fixture(), &b).unwrap();
    let mut args = args(vec![a, b]);
    args.offline = true;
    let base = serve(build_app(&args, None).await.unwrap().data).await;

    for p in ["/a/orders", "/b/orders"] {
        let r = reqwest::get(format!("{base}{p}")).await.unwrap();
        assert_eq!(r.status(), 200, "{p}");
    }
    let r = reqwest::get(format!("{base}/orders")).await.unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn health_endpoint_is_reserved_and_outside_mock_prefixes() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.yaml");
    let b = dir.path().join("b.yaml");
    std::fs::copy(fixture(), &a).unwrap();
    std::fs::copy(fixture(), &b).unwrap();
    for contracts in [vec![fixture()], vec![a, b]] {
        let mut args = args(contracts);
        args.offline = true;
        let base = serve(build_app(&args, None).await.unwrap().data).await;
        let r = reqwest::get(format!("{base}/__mockingbird/healthz"))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body, json!({"status": "ok"}));
    }
}

fn bin() -> Command {
    let mut cmd = Command::cargo_bin("mockingbird").unwrap();
    cmd.env_remove("MB_LLM_API_KEY");
    cmd
}

#[test]
fn refuses_public_bind_without_flag() {
    bin()
        .args(["serve", "--host", "0.0.0.0", "--offline"])
        .arg(fixture())
        .assert()
        .code(2)
        .stderr(predicates::str::contains("--insecure-no-auth"));
}

#[test]
fn requires_api_key_unless_offline() {
    bin()
        .args(["serve"])
        .arg(fixture())
        .assert()
        .code(2)
        .stderr(predicates::str::contains("MB_LLM_API_KEY"));
}

#[test]
fn validate_prints_operations() {
    bin()
        .args(["validate"])
        .arg(fixture())
        .assert()
        .success()
        .stdout(predicates::str::contains("createOrder"))
        .stdout(predicates::str::contains("getOrder"));
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Kills the server process when dropped.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn serve_binds_data_and_admin_ports() {
    let (port, admin_port) = (free_port(), free_port());
    let _child = Child(
        std::process::Command::new(assert_cmd::cargo::cargo_bin("mockingbird"))
            .env_remove("MB_LLM_API_KEY")
            .args(["serve", "--offline", "--port"])
            .arg(port.to_string())
            .arg("--admin-port")
            .arg(admin_port.to_string())
            .arg(fixture())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let data = format!("http://127.0.0.1:{port}/orders");
    while reqwest::get(&data).await.is_err() {
        assert!(std::time::Instant::now() < deadline, "server did not start");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(reqwest::get(&data).await.unwrap().status(), 200);
    for p in ["/healthz", "/api/v1/mocks"] {
        let r = reqwest::get(format!("http://127.0.0.1:{admin_port}{p}"))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{p}");
    }
}

/// Waits until `GET url` answers JSON satisfying `ok`, or fails after 5s.
async fn wait_for_json(url: &str, ok: impl Fn(&Value) -> bool) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let body: Value = reqwest::get(url).await.unwrap().json().await.unwrap();
        if ok(&body) {
            return body;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "GET {url}: still {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Breaking a profile a serving mock uses keeps the mock serving its
/// previous version; the API warns about it and `/readyz` names the mock.
#[tokio::test]
async fn broken_profile_keeps_mock_serving() {
    let app = build_app(
        &args(vec![fixture()]),
        Some(llm("http://127.0.0.1:9".into())),
    )
    .await
    .unwrap();
    let data = serve(app.data.clone()).await;
    let admin = serve(app.admin.clone()).await;
    let http = reqwest::Client::new();

    let profile = json!({"base_url": "http://127.0.0.1:9", "model": "m", "timeout_ms": 500});
    let r = http
        .put(format!("{admin}/api/v1/llm-profiles/q"))
        .json(&profile)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let r = http
        .post(format!("{admin}/api/v1/mocks"))
        .json(&json!({"name": "c", "llm_profile": "q"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let r = http
        .post(format!("{admin}/api/v1/mocks/c/contracts"))
        .header("content-type", "application/yaml")
        .body(std::fs::read(fixture()).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while reqwest::get(format!("{data}/c/orders"))
        .await
        .unwrap()
        .status()
        != 200
    {
        assert!(std::time::Instant::now() < deadline, "c never mounted");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let mut broken = profile.clone();
    broken["secret_ref"] = json!("env:MB_LLM_E2E_NOT_SET_41C7");
    let r = http
        .put(format!("{admin}/api/v1/llm-profiles/q"))
        .json(&broken)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let warnings = body["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("\"c\"")),
        "{body}"
    );

    let ready = wait_for_json(&format!("{admin}/readyz"), |b| b["status"] == "degraded").await;
    assert!(
        ready["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().starts_with("mock c: ")),
        "{ready}"
    );
    assert_eq!(
        reqwest::get(format!("{data}/c/orders"))
            .await
            .unwrap()
            .status(),
        200
    );

    // Deleting the profile still succeeds, with a warning.
    let r = http
        .delete(format!("{admin}/api/v1/llm-profiles/q"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["warnings"][0].as_str().unwrap().contains("\"c\""),
        "{body}"
    );
    assert_eq!(
        reqwest::get(format!("{data}/c/orders"))
            .await
            .unwrap()
            .status(),
        200
    );
}

async fn admin_status(app: &axum::Router, host: Option<&str>) -> (u16, String) {
    use tower::ServiceExt;
    let mut req = axum::http::Request::get("/api/v1/mocks");
    if let Some(host) = host {
        req = req.header("host", host);
    }
    let resp = app
        .clone()
        .oneshot(req.body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    let ct = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    (resp.status().as_u16(), ct)
}

/// On a loopback bind the admin port answers only to loopback host names,
/// which blocks DNS rebinding.
#[tokio::test]
async fn admin_rejects_foreign_host_on_loopback() {
    let mut a = args(vec![fixture()]);
    a.offline = true;
    let app = build_app(&a, None).await.unwrap();
    for host in [
        "evil.example",
        "evil.example:9090",
        "127.0.0.1.evil.example",
        "localhost.evil.example:9090",
        "10.0.0.1:9090",
    ] {
        let (status, ct) = admin_status(&app.admin, Some(host)).await;
        assert_eq!(status, 421, "{host}");
        assert_eq!(ct, "application/problem+json", "{host}");
    }
    for host in [
        "localhost",
        "LocalHost:9090",
        "127.0.0.1",
        "127.0.0.1:9090",
        "[::1]",
        "[::1]:9090",
    ] {
        assert_eq!(admin_status(&app.admin, Some(host)).await.0, 200, "{host}");
    }
    assert_eq!(admin_status(&app.admin, None).await.0, 200);

    // A non-loopback bind (with --insecure-no-auth) does not check.
    let mut a = args(vec![fixture()]);
    a.offline = true;
    a.host = Some("0.0.0.0".parse().unwrap());
    a.insecure_no_auth = true;
    let app = build_app(&a, None).await.unwrap();
    assert_eq!(admin_status(&app.admin, Some("evil.example")).await.0, 200);
}
