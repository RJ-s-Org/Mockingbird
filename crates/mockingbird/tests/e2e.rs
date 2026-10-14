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
        host: "127.0.0.1".parse().unwrap(),
        port: 0,
        insecure_no_auth: false,
        offline: false,
        llm_timeout: "3s".parse().unwrap(),
        contracts,
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

    let app = build_app(&args(vec![fixture()]), Some(llm(mock.uri()))).unwrap();
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
    let app = build_app(&args(vec![fixture()]), Some(llm(mock.uri()))).unwrap();
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
    let base = serve(build_app(&args, None).unwrap()).await;

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
        let base = serve(build_app(&args, None).unwrap()).await;
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
