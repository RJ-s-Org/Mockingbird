#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_mcm::Bundle;
use mb_protocols::http_rest;
use mb_protocols::{
    DataPlaneAuth, DataPlaneAuthMode, DataPlaneKeys, MockRuntime, RequestRecord, RequestRecorder,
};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

fn bundle(rel: &str) -> Arc<Bundle> {
    let path = format!("{}/../../fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
    Arc::new(compile(&std::fs::read(path).unwrap()).unwrap())
}

fn memory() -> Arc<dyn StateStore> {
    Arc::new(MemoryStore::new(Duration::from_secs(3600)))
}

fn runtime(
    name: &str,
    bundle: Arc<Bundle>,
    provider: Option<Arc<dyn Provider>>,
    state: Arc<dyn StateStore>,
) -> MockRuntime {
    MockRuntime::new(
        name,
        bundle,
        Arc::new(Engine::new(provider, EngineConfig::default())),
        state,
    )
}

/// Accepts exactly the listed `(workspace, key)` pairs.
struct StubKeys(Vec<(&'static str, &'static str)>);

#[async_trait]
impl DataPlaneKeys for StubKeys {
    async fn verify(&self, workspace: &str, key: &str) -> bool {
        self.0.iter().any(|(w, k)| *w == workspace && *k == key)
    }
}

fn stub() -> Arc<dyn DataPlaneKeys> {
    Arc::new(StubKeys(vec![("default", "good")]))
}

fn api_key_mode(header: &str) -> DataPlaneAuth {
    DataPlaneAuth {
        mode: DataPlaneAuthMode::ApiKey,
        header: header.into(),
    }
}

/// The orders mock in `api_key` mode with the stub keys.
fn orders_api_key() -> MockRuntime {
    let mut rt = runtime("orders", bundle("openapi/orders-3.0.yaml"), None, memory());
    rt.data_plane_auth = api_key_mode("x-api-key");
    rt.data_plane_keys = Some(stub());
    rt
}

/// The secured widgets mock in `passthrough_validate` mode.
fn widgets_passthrough() -> Router {
    let mut rt = runtime(
        "widgets",
        bundle("openapi/secured-3.0.yaml"),
        None,
        memory(),
    );
    rt.data_plane_auth = DataPlaneAuth {
        mode: DataPlaneAuthMode::PassthroughValidate,
        ..DataPlaneAuth::default()
    };
    http_rest::router(Arc::new(rt))
}

async fn send(router: &Router, req: Request<Body>) -> Response {
    router.clone().oneshot(req).await.unwrap()
}

fn req(method: &str, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).unwrap()
}

fn get(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    req("GET", uri, headers)
}

async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn www_authenticate(resp: &Response) -> &str {
    resp.headers()["www-authenticate"].to_str().unwrap()
}

async fn assert_unauthorized(resp: Response, challenge: &str, case: &str) {
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{case}");
    assert_eq!(www_authenticate(&resp), challenge, "{case}");
    assert_eq!(
        resp.headers()["content-type"],
        "application/problem+json",
        "{case}"
    );
    let body = body_json(resp).await;
    assert_eq!(body["title"], "Unauthorized", "{case}");
    assert_eq!(body["status"], 401, "{case}");
}

#[derive(Default)]
struct Collect {
    records: Mutex<Vec<RequestRecord>>,
}

impl RequestRecorder for Collect {
    fn record(&self, r: RequestRecord) {
        self.records.lock().unwrap().push(r);
    }
}

const API_KEY_CHALLENGE: &str = r#"ApiKey realm="mockingbird", header="x-api-key""#;

#[tokio::test]
async fn api_key_header_accepted() {
    let router = http_rest::router(Arc::new(orders_api_key()));
    let resp = send(&router, get("/orders/o1", &[("x-api-key", "good")])).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn api_key_query_accepted() {
    let router = http_rest::router(Arc::new(orders_api_key()));
    let resp = send(&router, get("/orders/o1?api_key=good", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn missing_or_bad_key_401_with_www_authenticate() {
    let router = http_rest::router(Arc::new(orders_api_key()));
    let resp = send(&router, get("/orders/o1", &[])).await;
    assert_unauthorized(resp, API_KEY_CHALLENGE, "missing").await;
    let resp = send(&router, get("/orders/o1", &[("x-api-key", "bad")])).await;
    assert_unauthorized(resp, API_KEY_CHALLENGE, "bad header").await;
    let resp = send(&router, get("/orders/o1?api_key=bad", &[])).await;
    assert_unauthorized(resp, API_KEY_CHALLENGE, "bad query").await;

    // Without a key verifier no key is accepted.
    let mut rt = orders_api_key();
    rt.data_plane_keys = None;
    let router = http_rest::router(Arc::new(rt));
    let resp = send(&router, get("/orders/o1", &[("x-api-key", "good")])).await;
    assert_unauthorized(resp, API_KEY_CHALLENGE, "no verifier").await;

    // The challenge names the configured header.
    let mut rt = orders_api_key();
    rt.data_plane_auth = api_key_mode("x-key");
    let router = http_rest::router(Arc::new(rt));
    let resp = send(&router, get("/orders/o1", &[("x-api-key", "good")])).await;
    assert_unauthorized(
        resp,
        r#"ApiKey realm="mockingbird", header="x-key""#,
        "custom header",
    )
    .await;
}

#[tokio::test]
async fn key_from_other_workspace_401() {
    let mut rt = orders_api_key();
    rt.workspace = "team-a".into();
    rt.data_plane_keys = Some(Arc::new(StubKeys(vec![
        ("default", "good"),
        ("team-a", "team-key"),
    ])));
    let router = http_rest::router(Arc::new(rt));
    let resp = send(&router, get("/orders/o1", &[("x-api-key", "good")])).await;
    assert_unauthorized(resp, API_KEY_CHALLENGE, "default key on team-a").await;
    let resp = send(&router, get("/orders/o1", &[("x-api-key", "team-key")])).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn data_plane_key_never_reaches_prompt_or_record() {
    let order = json!({"id": "o1", "customerId": "c1", "status": "pending", "total": 10});
    let provider = Arc::new(ScriptedProvider::new(vec![
        Ok(json!([order]).to_string()),
        Ok(json!([order]).to_string()),
    ]));
    let collect = Arc::new(Collect::default());
    let mut rt = runtime(
        "orders",
        bundle("openapi/orders-3.0.yaml"),
        Some(provider.clone() as Arc<dyn Provider>),
        memory(),
    );
    rt.data_plane_auth = api_key_mode("x-key");
    rt.data_plane_keys = Some(stub());
    rt.recorder = Some(collect.clone());
    let router = http_rest::router(Arc::new(rt));

    let resp = send(&router, get("/orders?api_key=good&x=1", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = send(&router, get("/orders?x=2", &[("x-key", "good")])).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    for (i, request) in requests.iter().enumerate() {
        let prompt: String = request
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert!(prompt.contains("\"x\""), "prompt {i} lacks x: {prompt}");
        assert!(!prompt.contains("good"), "prompt {i} leaks the key");
        assert!(!prompt.contains("api_key"), "prompt {i} names api_key");
        assert!(!prompt.contains("x-key"), "prompt {i} names x-key");
    }

    let records = collect.records.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].request["query"], json!({"x": "1"}));
    assert_eq!(records[1].request["query"], json!({"x": "2"}));
    for r in &records {
        let headers = r.request["headers"].as_object().unwrap();
        assert!(
            !headers.contains_key("x-key"),
            "recorded headers {headers:?}"
        );
        assert!(!r.request.to_string().contains("good"));
    }
}

#[tokio::test]
async fn passthrough_declared_api_keys_never_reach_prompt_or_record() {
    let widget = json!({"id": "w1", "name": "n"});
    let provider = Arc::new(ScriptedProvider::new(vec![
        Ok(json!([widget]).to_string()),
        Ok(widget.to_string()),
    ]));
    let collect = Arc::new(Collect::default());
    let mut rt = runtime(
        "widgets",
        bundle("openapi/secured-3.0.yaml"),
        Some(provider.clone() as Arc<dyn Provider>),
        memory(),
    );
    rt.data_plane_auth = DataPlaneAuth {
        mode: DataPlaneAuthMode::PassthroughValidate,
        ..DataPlaneAuth::default()
    };
    rt.recorder = Some(collect.clone());
    let router = http_rest::router(Arc::new(rt));

    let resp = send(&router, get("/widgets/search?apikey=qsecret&x=1", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = send(
        &router,
        req(
            "PUT",
            "/widgets/w1",
            &[
                ("cookie", "session_id=csecret"),
                ("X-Partner-Key", "hsecret"),
                ("x-trace", "t1"),
            ],
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let leaks = ["qsecret", "csecret", "hsecret"];
    for (i, request) in requests.iter().enumerate() {
        let prompt: String = request
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        for secret in leaks {
            assert!(
                !prompt.contains(secret),
                "prompt {i} leaks {secret}: {prompt}"
            );
        }
    }
    let prompt: String = requests[0]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        prompt.contains("\"x\""),
        "the other query parameter is kept"
    );

    let records = collect.records.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].request["query"], json!({"x": "1"}));
    assert_eq!(records[1].request["headers"]["x-trace"], "t1");
    for r in &records {
        for secret in leaks {
            assert!(!r.request.to_string().contains(secret), "{}", r.request);
        }
    }
}

type Headers = Vec<(&'static str, &'static str)>;

#[tokio::test]
async fn passthrough_bearer_basic_apikey() {
    let router = widgets_passthrough();
    // `user:pass` and `nocolon`, standard base64.
    let basic_ok = "Basic dXNlcjpwYXNz";
    let basic_no_colon = "Basic bm9jb2xvbg==";

    let ok: Vec<(&str, &str, Headers)> = vec![
        ("GET", "/widgets", vec![("authorization", "Bearer abc")]),
        ("GET", "/widgets", vec![("authorization", "bearer abc")]),
        ("POST", "/widgets", vec![("authorization", basic_ok)]),
        ("GET", "/widgets/search?apikey=k", vec![]),
        (
            "PUT",
            "/widgets/w1",
            vec![
                ("cookie", "theme=dark; session_id=s1"),
                ("X-Partner-Key", "p"),
            ],
        ),
        ("DELETE", "/widgets/w1", vec![("authorization", "Bearer t")]),
        ("GET", "/me", vec![("authorization", "Bearer t")]),
        ("GET", "/admin", vec![("authorization", basic_ok)]),
    ];
    for (method, uri, headers) in ok {
        let resp = send(&router, req(method, uri, &headers)).await;
        assert!(
            resp.status().is_success(),
            "{method} {uri} {headers:?}: {}",
            resp.status()
        );
    }

    let bearer = "Bearer";
    let basic = r#"Basic realm="mockingbird""#;
    let denied: Vec<(&str, &str, Headers, &str)> = vec![
        ("GET", "/widgets", vec![], bearer),
        (
            "GET",
            "/widgets",
            vec![("authorization", "Bearer ")],
            bearer,
        ),
        ("GET", "/widgets", vec![("authorization", basic_ok)], bearer),
        ("POST", "/widgets", vec![], basic),
        (
            "POST",
            "/widgets",
            vec![("authorization", "Basic !!!")],
            basic,
        ),
        (
            "POST",
            "/widgets",
            vec![("authorization", basic_no_colon)],
            basic,
        ),
        (
            "POST",
            "/widgets",
            vec![("authorization", "Bearer abc")],
            basic,
        ),
        (
            "GET",
            "/widgets/search",
            vec![],
            r#"ApiKey realm="mockingbird", query="apikey""#,
        ),
        (
            "GET",
            "/widgets/search?apikey=",
            vec![],
            r#"ApiKey realm="mockingbird", query="apikey""#,
        ),
        (
            "PUT",
            "/widgets/w1",
            vec![("cookie", "session_id=s1")],
            r#"ApiKey realm="mockingbird", cookie="session_id""#,
        ),
        (
            "PUT",
            "/widgets/w1",
            vec![("x-partner-key", "p")],
            r#"ApiKey realm="mockingbird", cookie="session_id""#,
        ),
        ("DELETE", "/widgets/w1", vec![], bearer),
        ("GET", "/me", vec![], bearer),
        ("GET", "/admin", vec![], basic),
    ];
    for (method, uri, headers, challenge) in denied {
        let resp = send(&router, req(method, uri, &headers)).await;
        assert_unauthorized(resp, challenge, &format!("{method} {uri} {headers:?}")).await;
    }
}

#[tokio::test]
async fn passthrough_optional_and_unsecured_ops_pass() {
    let router = widgets_passthrough();
    for uri in ["/widgets/w1", "/health"] {
        let resp = send(&router, get(uri, &[])).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    }
    let resp = send(
        &router,
        get("/widgets/w1", &[("authorization", "Bearer t")]),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn record_carries_workspace() {
    let collect = Arc::new(Collect::default());
    let state = memory();
    let mut rt = runtime(
        "orders",
        bundle("openapi/orders-3.0.yaml"),
        None,
        state.clone(),
    );
    assert_eq!(rt.workspace, "default");
    rt.workspace = "team-a".into();
    rt.recorder = Some(collect.clone());
    let router = http_rest::router(Arc::new(rt));
    let create = Request::post("/orders")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"customerId":"c1","total":10}"#))
        .unwrap();
    assert_eq!(send(&router, create).await.status(), StatusCode::CREATED);
    let resp = send(&router, get("/nope", &[])).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let records = collect.records.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|r| r.workspace == "team-a"));
    // State is kept under the runtime's workspace.
    assert_eq!(state.sessions("team-a", "orders").await.unwrap().len(), 1);
    assert!(
        state
            .sessions("default", "orders")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn none_mode_unchanged() {
    let collect = Arc::new(Collect::default());
    let mut rt = runtime("orders", bundle("openapi/orders-3.0.yaml"), None, memory());
    assert_eq!(rt.data_plane_auth, DataPlaneAuth::default());
    assert_eq!(rt.data_plane_auth.mode, DataPlaneAuthMode::None);
    assert_eq!(rt.data_plane_auth.header, "x-api-key");
    rt.recorder = Some(collect.clone());
    let router = http_rest::router(Arc::new(rt));
    let resp = send(&router, get("/orders/o1?api_key=anything&x=1", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("www-authenticate").is_none());
    // In `none` mode `api_key` is an ordinary query parameter.
    let records = collect.records.lock().unwrap().clone();
    assert_eq!(
        records[0].request["query"],
        json!({"api_key": "anything", "x": "1"})
    );

    // A secured contract is not enforced in `none` mode.
    let rt = runtime(
        "widgets",
        bundle("openapi/secured-3.0.yaml"),
        None,
        memory(),
    );
    let router = http_rest::router(Arc::new(rt));
    let resp = send(&router, get("/widgets", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
}
