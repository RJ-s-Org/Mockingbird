#![allow(clippy::unwrap_used)]

//! Per-mock rate limits, request size caps, daily token budgets and the
//! engine guards, through an in-process app.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mb_engine::{SchemaValidator, TokenBudget};
use mb_mcm::ProtocolPayload;
use mb_protocols::grpc::DynamicCodec;
use mb_store::{NewMock, ResourceSource};
use mockingbird::budget::StoreTokenBudget;
use mockingbird::config::{App, ServeArgs, build_app};
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::{Value, json};
use tonic::codegen::http::uri::PathAndQuery;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel)
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// An app built from `config` (written next to any extra files), offline
/// unless the config names an LLM profile.
async fn app(dir: &Path, config: &str, offline: bool) -> App {
    let path = dir.join("mockingbird.yaml");
    std::fs::write(&path, config).unwrap();
    let args = ServeArgs {
        config: Some(path),
        offline,
        ..ServeArgs::default()
    };
    build_app(&args, None).await.unwrap()
}

fn completion(content: Value, total_tokens: u32) -> Value {
    json!({
        "choices": [{"message": {"content": content.to_string()}}],
        "usage": {
            "prompt_tokens": total_tokens / 6,
            "completion_tokens": total_tokens - total_tokens / 6,
            "total_tokens": total_tokens,
        }
    })
}

fn order(id: &str, customer: &str) -> Value {
    json!({"id": id, "customerId": customer, "status": "pending", "total": 10})
}

/// Waits until mock `mock`'s request log has an entry with `status`.
async fn wait_for_logged_status(admin: &str, mock: &str, status: u16) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let rows: Value = reqwest::get(format!("{admin}/api/v1/mocks/{mock}/requests"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if rows
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["status"] == status)
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no {status} row for {mock}: {rows}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn rate_limited_call_is_429_with_retry_after() {
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "mocks:\n  orders:\n    contract: {}\n    limits:\n      rate_limit: {{requests_per_second: 1, burst: 2}}\n",
        fixture("openapi/orders-3.0.yaml").display()
    );
    let app = app(dir.path(), &config, true).await;
    let data = serve(app.data.clone()).await;
    let admin = serve(app.admin.clone()).await;

    for _ in 0..2 {
        let r = reqwest::get(format!("{data}/orders/orders/o1"))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    let r = reqwest::get(format!("{data}/orders/orders/o1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "1");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["title"], "Too Many Requests");
    assert_eq!(
        body["detail"], "mock orders allows 1 requests/s (burst 2)",
        "{body}"
    );

    let metrics = reqwest::get(format!("{admin}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("mb_rate_limited_total{mock=\"orders\"}"),
        "{metrics}"
    );
    wait_for_logged_status(&admin, "orders", 429).await;
}

const THINGS: &str = r##"openapi: 3.0.3
info: {title: Things, version: 1.0.0}
paths:
  /things/{id}:
    get:
      operationId: getThing
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {type: object, required: [id], properties: {id: {type: string}}}
        "429":
          description: Slow down
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Throttled"}
components:
  schemas:
    Throttled:
      type: object
      required: [code, retryAfterSeconds]
      additionalProperties: false
      properties:
        code: {type: string, enum: [THROTTLED]}
        retryAfterSeconds: {type: integer, minimum: 1, maximum: 60}
"##;

#[tokio::test]
async fn rate_limit_uses_declared_429_shape() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("things.yaml"), THINGS).unwrap();
    let config = "mocks:\n  things:\n    contract: things.yaml\n    limits:\n      rate_limit: {requests_per_second: 0.5, burst: 1}\n";
    let app = app(dir.path(), config, true).await;
    let data = serve(app.data.clone()).await;

    let r = reqwest::get(format!("{data}/things/things/t1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = reqwest::get(format!("{data}/things/things/t1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "2");
    assert_eq!(r.headers()["content-type"], "application/json");
    assert_eq!(r.headers()["x-mockingbird-source"], "fallback");
    let body: Value = r.json().await.unwrap();

    let bundle = mb_import::compile(THINGS.as_bytes()).unwrap();
    let op = bundle.operation("getThing").unwrap();
    let schema = op.output_for_status(429).unwrap().schema.as_ref().unwrap();
    let validator = SchemaValidator::new(&bundle.with_reachable_defs(schema)).unwrap();
    validator.validate(&body).unwrap();
    assert_eq!(body["code"], "THROTTLED", "{body}");
}

#[tokio::test]
async fn oversized_request_is_413_and_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        r#"
limits: {{max_request_bytes: 2048}}
mocks:
  orders:
    contract: {orders}
  github:
    contract: {github}
  calculator:
    contract: {calculator}
    limits: {{max_request_bytes: 1024}}
"#,
        orders = fixture("openapi/orders-3.0.yaml").display(),
        github = fixture("graphql/github.graphql").display(),
        calculator = fixture("wsdl/calculator.wsdl").display(),
    );
    let app = app(dir.path(), &config, true).await;
    let data = serve(app.data.clone()).await;
    let admin = serve(app.admin.clone()).await;
    let http = reqwest::Client::new();
    let filler = "x".repeat(4096);

    // REST: Content-Length over the cap.
    let r = http
        .post(format!("{data}/orders/orders"))
        .json(&json!({"customerId": filler, "total": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["title"], "Payload Too Large", "{body}");
    assert!(body["detail"].as_str().unwrap().contains("2048"), "{body}");
    // A body within the cap is served.
    let r = http
        .post(format!("{data}/orders/orders"))
        .json(&json!({"customerId": "c1", "total": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    // REST: no Content-Length (chunked), collected length over the cap.
    let chunks =
        futures::stream::iter((0..4).map(|_| Ok::<_, std::io::Error>(bytes_of(&"y".repeat(1000)))));
    let r = http
        .post(format!("{data}/orders/orders"))
        .header("content-type", "application/json")
        .body(reqwest::Body::wrap_stream(chunks))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);

    // GraphQL.
    let r = http
        .post(format!("{data}/github/graphql"))
        .json(&json!({"query": format!("{{ viewer {{ login }} }} # {filler}")}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);

    // SOAP: a Fault with HTTP 413, against the mock's own 1 KiB cap.
    let envelope = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"><soap:Body><Add xmlns="http://tempuri.org/"><intA>2</intA><intB>3</intB></Add></soap:Body></soap:Envelope><!-- {} -->"#,
        "z".repeat(1500)
    );
    let r = http
        .post(format!("{data}/calculator/calculator.asmx"))
        .header("content-type", "text/xml; charset=utf-8")
        .header("SOAPAction", "\"http://tempuri.org/Add\"")
        .body(envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    let text = r.text().await.unwrap();
    assert!(text.contains("Fault"), "{text}");
    assert!(text.contains("1024"), "{text}");

    for mock in ["orders", "github", "calculator"] {
        wait_for_logged_status(&admin, mock, 413).await;
    }
}

fn bytes_of(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

#[tokio::test]
async fn grpc_oversized_message_resource_exhausted() {
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "mocks:\n  hello:\n    contract: {}\n    limits: {{max_request_bytes: 1024}}\n",
        fixture("proto/helloworld.proto").display()
    );
    let app = app(dir.path(), &config, true).await;
    let admin = serve(app.admin.clone()).await;
    let grpc_addr = serve(app.grpc.clone()).await;

    let bundle =
        mb_import::compile(&std::fs::read(fixture("proto/helloworld.proto")).unwrap()).unwrap();
    let Some(ProtocolPayload::Grpc {
        file_descriptor_set,
    }) = &bundle.protocol_payload
    else {
        panic!("no descriptors")
    };
    let pool = DescriptorPool::decode(file_descriptor_set.as_slice()).unwrap();
    let request_desc = pool.get_message_by_name("helloworld.HelloRequest").unwrap();
    let reply_desc = pool.get_message_by_name("helloworld.HelloReply").unwrap();
    let channel = tonic::transport::Endpoint::from_shared(grpc_addr)
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut grpc = tonic::client::Grpc::new(channel);

    let call = |name: String| {
        let request =
            DynamicMessage::deserialize(request_desc.clone(), json!({"name": name})).unwrap();
        (
            tonic::Request::new(request),
            DynamicCodec::new(request_desc.clone(), reply_desc.clone()),
        )
    };
    let (request, codec) = call("small".into());
    grpc.ready().await.unwrap();
    let ok = grpc
        .unary(
            request,
            PathAndQuery::from_static("/helloworld.Greeter/SayHello"),
            codec,
        )
        .await;
    assert!(ok.is_ok(), "{ok:?}");

    let (request, codec) = call("n".repeat(4096));
    grpc.ready().await.unwrap();
    let status = grpc
        .unary(
            request,
            PathAndQuery::from_static("/helloworld.Greeter/SayHello"),
            codec,
        )
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::ResourceExhausted, "{status:?}");
    wait_for_logged_status(&admin, "hello", 413).await;
}

fn llm_config(base: &str, timeout: &str, extra: &str, mock_extra: &str) -> String {
    format!(
        r#"
llm:
  default_profile: main
  profiles:
    main: {{base_url: "{base}", model: m, timeout: {timeout}}}
{extra}
mocks:
  orders:
    contract: {orders}
{mock_extra}
"#,
        orders = fixture("openapi/orders-3.0.yaml").display(),
    )
}

#[tokio::test]
async fn budget_exhausted_serves_fallback_and_degrades_readyz() {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion(order("o1", "c1"), 600)))
        .mount(&llm)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = llm_config(
        &llm.uri(),
        "3s",
        "",
        "    limits: {daily_token_budget: 1000}",
    );
    let app = app(dir.path(), &config, false).await;
    let data = serve(app.data.clone()).await;
    let admin = serve(app.admin.clone()).await;

    let source = |r: &reqwest::Response| r.headers()["x-mockingbird-source"].clone();
    let ready: Value = reqwest::get(format!("{admin}/readyz"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ready["status"], "ready", "{ready}");

    // 600 tokens, then 1200: the second call still starts under the budget.
    for _ in 0..2 {
        let r = reqwest::get(format!("{data}/orders/orders/o1"))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(source(&r), "llm");
    }
    let r = reqwest::get(format!("{data}/orders/orders/o1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(source(&r), "fallback");
    assert_eq!(llm.received_requests().await.unwrap().len(), 2);

    let ready: Value = reqwest::get(format!("{admin}/readyz"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ready["status"], "degraded", "{ready}");
    assert!(
        ready["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r == "mock orders: daily token budget exhausted"),
        "{ready}"
    );
    let metrics = reqwest::get(format!("{admin}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("mb_token_budget_exhausted_total{mock=\"orders\"}"),
        "{metrics}"
    );
}

#[tokio::test]
async fn budget_shared_through_store() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("mb.db").display());
    let one = mb_store::connect(&url).await.unwrap();
    let two = mb_store::connect(&url).await.unwrap();
    let mock = one
        .create_mock(
            mb_store::DEFAULT_WORKSPACE,
            NewMock {
                name: "orders".into(),
                route_prefix: None,
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::Api,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: Default::default(),
                hosts: vec![],
            },
        )
        .await
        .unwrap();

    let a = StoreTokenBudget::spawn(Arc::clone(&one), &mock, 1000);
    let b = StoreTokenBudget::spawn(Arc::clone(&two), &mock, 1000);
    assert_eq!(a.label(), "orders");
    a.spend(600);
    assert_eq!(a.used(), 600);
    assert_eq!(b.used(), 0);
    a.flush().await;
    b.flush().await;
    assert_eq!(b.used(), 600);
    assert!(!b.exhausted());

    b.spend(500);
    // Local spend counts at once; the other replica learns it on a flush.
    assert!(b.is_exhausted());
    assert!(!a.is_exhausted());
    b.flush().await;
    a.flush().await;
    assert_eq!(a.used(), 1100);
    assert!(a.exhausted());
    let today = chrono::Utc::now().date_naive();
    assert_eq!(two.token_usage(mock.id, today).await.unwrap(), 1100);
}

#[tokio::test]
async fn concurrency_limit_from_config() {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(completion(order("o1", "c1"), 10))
                .set_delay(Duration::from_millis(800)),
        )
        .mount(&llm)
        .await;
    let dir = tempfile::tempdir().unwrap();
    // One slot: the first call holds it for 800 ms; the others cannot get
    // it and finish within the 1.2 s call timeout.
    let config = llm_config(
        &llm.uri(),
        "1200ms",
        "limits: {max_concurrent_generations: 1}",
        "",
    );
    let app = app(dir.path(), &config, false).await;
    let data = serve(app.data.clone()).await;

    let calls = (0..3).map(|i| {
        let url = format!("{data}/orders/orders/o{i}");
        async move {
            let r = reqwest::get(url).await.unwrap();
            assert_eq!(r.status(), 200);
            r.headers()["x-mockingbird-source"]
                .to_str()
                .unwrap()
                .to_string()
        }
    });
    let sources = futures::future::join_all(calls).await;
    assert!(sources.iter().any(|s| s == "llm"), "{sources:?}");
    assert!(sources.iter().any(|s| s == "fallback"), "{sources:?}");
}

#[tokio::test]
async fn pii_enabled_scrubs_responses() {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(completion(order("o1", "jane@corp.com"), 10)),
        )
        .mount(&llm)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = llm_config(&llm.uri(), "3s", "pii: {enabled: true}", "");
    let app = app(dir.path(), &config, false).await;
    let data = serve(app.data.clone()).await;
    let admin = serve(app.admin.clone()).await;

    let r = reqwest::get(format!("{data}/orders/orders/o1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-mockingbird-source"], "llm");
    let body: Value = r.json().await.unwrap();
    let customer = body["customerId"].as_str().unwrap();
    assert!(customer.ends_with("@example.com"), "{body}");
    assert!(!body.to_string().contains("corp.com"), "{body}");

    let metrics = reqwest::get(format!("{admin}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("mb_pii_replacements_total{pattern=\"email\"}"),
        "{metrics}"
    );
}
