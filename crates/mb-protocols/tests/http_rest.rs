#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use mb_engine::{Engine, EngineConfig, SchemaValidator};
use mb_import::compile;
use mb_llm::{LlmError, Provider, ScriptedProvider};
use mb_mcm::{Bundle, HttpMethod};
use mb_protocols::MockRuntime;
use mb_protocols::http_rest::{self, RouteTable};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};
use tower::ServiceExt;

fn orders() -> Arc<Bundle> {
    let path = format!(
        "{}/../../fixtures/openapi/orders-3.0.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    Arc::new(compile(&std::fs::read(path).unwrap()).unwrap())
}

struct Harness {
    rt: Arc<MockRuntime>,
    router: Router,
    provider: Option<Arc<ScriptedProvider>>,
}

fn harness_with(
    script: Option<Vec<Result<String, LlmError>>>,
    state: Arc<dyn StateStore>,
    configure: impl FnOnce(&mut MockRuntime),
) -> Harness {
    let provider = script.map(|s| Arc::new(ScriptedProvider::new(s)));
    let dyn_provider = provider.clone().map(|p| p as Arc<dyn Provider>);
    let mut rt = MockRuntime::new(
        "orders",
        orders(),
        Arc::new(Engine::new(dyn_provider, EngineConfig::default())),
        state,
    );
    configure(&mut rt);
    let rt = Arc::new(rt);
    Harness {
        router: http_rest::router(Arc::clone(&rt)),
        rt,
        provider,
    }
}

fn harness(script: Option<Vec<Result<String, LlmError>>>, header_overrides: bool) -> Harness {
    harness_with(script, memory_state(), |rt| {
        rt.header_overrides = header_overrides
    })
}

fn memory_state() -> Arc<dyn StateStore> {
    Arc::new(MemoryStore::new(Duration::from_secs(3600)))
}

fn offline() -> Harness {
    harness(None, true)
}

fn session(id: &str) -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "orders".into(),
        session: id.into(),
    }
}

fn order(id: &str, customer: &str) -> Value {
    json!({"id": id, "customerId": customer, "status": "pending", "total": 10})
}

async fn send(h: &Harness, req: Request<Body>) -> Response {
    h.router.clone().oneshot(req).await.unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

fn post_json(uri: &str, body: &str) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_bytes(resp: Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

async fn body_json(resp: Response) -> Value {
    serde_json::from_slice(&body_bytes(resp).await).unwrap()
}

fn header_str<'a>(resp: &'a Response, name: &str) -> &'a str {
    resp.headers().get(name).unwrap().to_str().unwrap()
}

#[tokio::test]
async fn create_then_get_returns_stored_resource() {
    let h = harness(
        Some(vec![
            Ok(order("o1", "c1").to_string()),
            Ok(order("zzz", "other").to_string()),
        ]),
        true,
    );
    let resp = send(
        &h,
        post_json("/orders", r#"{"customerId":"c1","total":10}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "llm");
    assert_eq!(body_json(resp).await["id"], "o1");

    let resp = send(&h, get("/orders/o1")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header_str(&resp, "content-type"), "application/json");
    let body = body_json(resp).await;
    assert_eq!(body["id"], "o1");
    assert_eq!(body["customerId"], "c1");
}

#[tokio::test]
async fn sessions_isolate_state() {
    let h = harness(Some(vec![Ok(order("o1", "c1").to_string())]), true);
    let req = Request::post("/orders")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-mockingbird-session", "a")
        .body(Body::from(r#"{"customerId":"c1","total":10}"#))
        .unwrap();
    assert_eq!(send(&h, req).await.status(), StatusCode::CREATED);
    assert!(
        h.rt.state
            .get(&session("a"), "orders", "o1")
            .await
            .unwrap()
            .is_some()
    );

    // The script is exhausted, so session `b` is served by the faker. The
    // id follows the path, but none of session `a`'s stored fields leak.
    let req = Request::get("/orders/o1")
        .header("x-mockingbird-session", "b")
        .body(Body::empty())
        .unwrap();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["id"], "o1");
    assert_ne!(body["customerId"], "c1", "{body}");
    assert!(
        h.rt.state
            .get(&session("b"), "orders", "o1")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn delete_returns_204_and_removes_state() {
    let h = harness(Some(vec![Ok(order("o1", "c1").to_string())]), true);
    let resp = send(
        &h,
        post_json("/orders", r#"{"customerId":"c1","total":10}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(
        h.rt.state
            .get(&session("default"), "orders", "o1")
            .await
            .unwrap()
            .is_some()
    );

    let req = Request::delete("/orders/o1").body(Body::empty()).unwrap();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "llm");
    assert!(resp.headers().get(header::CONTENT_TYPE).is_none());
    assert!(body_bytes(resp).await.is_empty());
    assert!(
        h.rt.state
            .get(&session("default"), "orders", "o1")
            .await
            .unwrap()
            .is_none()
    );
    // Only the create call reached the LLM.
    assert_eq!(h.provider.as_ref().unwrap().requests().len(), 1);
}

#[tokio::test]
async fn routes_trailing_slash_query_and_unknown() {
    let bundle = orders();
    let table = RouteTable::new(&bundle).unwrap();
    let (op, params) = table.find(HttpMethod::Get, "/orders/o%201").unwrap();
    assert_eq!(op, "getOrder");
    assert_eq!(params["orderId"], "o 1");
    let (op, _) = table.find(HttpMethod::Get, "/orders/").unwrap();
    assert_eq!(op, "listOrders");

    let h = offline();
    let resp = send(&h, get("/orders/?status=shipped")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_json(resp).await.is_array());

    // The decoded path parameter is used as the state key.
    h.rt.state
        .upsert(
            &session("default"),
            StoredResource {
                resource_type: "orders".into(),
                id: "o 1".into(),
                data: order("o 1", "c9"),
            },
        )
        .await
        .unwrap();
    let resp = send(&h, get("/orders/o%201")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["id"], "o 1");
    assert_eq!(body["customerId"], "c9");

    let resp = send(&h, get("/nope")).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        header_str(&resp, "content-type"),
        "application/problem+json"
    );
    assert_eq!(body_json(resp).await["title"], "Not Found");

    let req = Request::patch("/orders").body(Body::empty()).unwrap();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    let allow = header_str(&resp, "allow");
    assert!(allow.contains("GET"), "{allow}");
    assert!(allow.contains("POST"), "{allow}");
}

#[tokio::test]
async fn invalid_body_returns_400_without_llm() {
    let h = harness(Some(vec![]), true);
    let bundle = orders();
    let error = SchemaValidator::new(&bundle.with_defs(&json!({"$ref": "#/$defs/Error"}))).unwrap();

    let resp = send(&h, post_json("/orders", "not json")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(error.validate(&body_json(resp).await).is_ok());

    let resp = send(&h, post_json("/orders", r#"{"total":-1}"#)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "fallback");
    assert!(error.validate(&body_json(resp).await).is_ok());

    assert!(h.provider.as_ref().unwrap().requests().is_empty());
}

#[tokio::test]
async fn invalid_input_without_error_variant_is_problem() {
    let h = offline();
    // `listOrders` declares no 4xx variant, so a bad query is a problem+json 400.
    let resp = send(&h, get("/orders?status=lost")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        header_str(&resp, "content-type"),
        "application/problem+json"
    );
    assert_eq!(body_json(resp).await["title"], "Invalid request");
}

#[tokio::test]
async fn status_override_returns_error_variant() {
    let h = offline();
    let req = Request::get("/orders/o1")
        .header("x-mockingbird-status", "404")
        .body(Body::empty())
        .unwrap();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let req = Request::get("/orders/o1")
        .header("x-mockingbird-status", "nope")
        .body(Body::empty())
        .unwrap();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        header_str(&resp, "content-type"),
        "application/problem+json"
    );

    // createOrder declares no 404 and no default variant.
    let req = Request::post("/orders")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-mockingbird-status", "404")
        .body(Body::from(r#"{"customerId":"c1","total":10}"#))
        .unwrap();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["title"], "Undeclared status override");
}

#[tokio::test(start_paused = true)]
async fn latency_override_delays() {
    let h = offline();
    let req = Request::get("/orders/o1")
        .header("x-mockingbird-latency", "150ms")
        .body(Body::empty())
        .unwrap();
    let start = tokio::time::Instant::now();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(start.elapsed() >= Duration::from_millis(150));
}

#[tokio::test]
async fn overrides_disabled_ignores_headers() {
    let h = harness(None, false);
    let req = Request::get("/orders/o1")
        .header("x-mockingbird-status", "404")
        .body(Body::empty())
        .unwrap();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn fallback_source_header() {
    let h = offline();
    let resp = send(&h, get("/orders/o1")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "fallback");
    assert_eq!(header_str(&resp, "content-type"), "application/json");
}

/// A runtime for an inline contract, served offline (faker only).
fn offline_contract(src: &str) -> Harness {
    let bundle = Arc::new(compile(src.as_bytes()).unwrap());
    let rt = Arc::new(MockRuntime::new(
        "inline",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    ));
    Harness {
        router: http_rest::router(Arc::clone(&rt)),
        rt,
        provider: None,
    }
}

const EXPR_CONTRACT: &str = r##"
openapi: 3.1.0
info: {title: Expr API, version: "1"}
paths:
  /expr:
    get:
      operationId: getExpr
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Expr"}
components:
  schemas:
    Expr:
      oneOf:
        - $ref: "#/components/schemas/And"
        - $ref: "#/components/schemas/Leaf"
    And:
      type: object
      required: [left, right]
      properties:
        left: {$ref: "#/components/schemas/Expr"}
        right: {$ref: "#/components/schemas/Expr"}
    Leaf:
      type: object
      required: [value]
      properties:
        value: {type: integer}
"##;

#[tokio::test]
async fn recursive_one_of_contract_does_not_crash() {
    let h = offline_contract(EXPR_CONTRACT);
    for _ in 0..20 {
        let resp = send(&h, get("/expr")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_json(resp).await.is_object());
    }
    // The process (and runtime) survived; a second request still works.
    assert_eq!(send(&h, get("/expr")).await.status(), StatusCode::OK);
    assert_eq!(h.rt.name, "inline");
}

/// Same shape as [`EXPR_CONTRACT`], but the leaf has three required
/// references, so a recursion-blind branch cost prefers the recursive branch.
const FANOUT_EXPR_CONTRACT: &str = r##"
openapi: 3.1.0
info: {title: Expr API, version: "1"}
paths:
  /expr:
    get:
      operationId: getExpr
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Expr"}
components:
  schemas:
    Expr:
      oneOf:
        - $ref: "#/components/schemas/And"
        - $ref: "#/components/schemas/Leaf"
    And:
      type: object
      required: [left, right]
      properties:
        left: {$ref: "#/components/schemas/Expr"}
        right: {$ref: "#/components/schemas/Expr"}
    Leaf:
      type: object
      required: [a, b, c]
      properties:
        a: {$ref: "#/components/schemas/S"}
        b: {$ref: "#/components/schemas/S"}
        c: {$ref: "#/components/schemas/S"}
    S: {type: string}
"##;

#[tokio::test]
async fn recursive_one_of_with_ref_heavy_leaf_returns_quickly() {
    let h = offline_contract(FANOUT_EXPR_CONTRACT);
    let start = std::time::Instant::now();
    for _ in 0..10 {
        let resp = send(&h, get("/expr")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_json(resp).await.is_object());
    }
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

fn put_json(uri: &str, body: &str) -> Request<Body> {
    Request::put(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn offline_writes_are_reflected() {
    let h = offline();
    let resp = send(
        &h,
        post_json("/orders", r#"{"customerId":"c1","total":10}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    assert_eq!(created["customerId"], "c1", "{created}");
    assert_eq!(created["total"], 10, "{created}");
    let id = created["id"].as_str().unwrap().to_string();

    let resp = send(&h, get(&format!("/orders/{id}"))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["id"], id.as_str());
    assert_eq!(body["customerId"], "c1", "{body}");

    let resp = send(
        &h,
        put_json(&format!("/orders/{id}"), r#"{"customerId":"c2","total":5}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["id"], id.as_str(), "{body}");
    assert_eq!(body["customerId"], "c2", "{body}");
    assert_eq!(body["status"], created["status"], "{body}");

    let resp = send(&h, get(&format!("/orders/{id}"))).await;
    let body = body_json(resp).await;
    assert_eq!(body["id"], id.as_str());
    assert_eq!(body["customerId"], "c2", "{body}");
}

#[tokio::test]
async fn get_unknown_id_returns_path_id() {
    let h = offline();
    let resp = send(&h, get("/orders/abc")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["id"], "abc");
}

const FORM_CONTRACT: &str = r#"{
  "swagger": "2.0",
  "info": {"title": "Pets", "version": "1"},
  "paths": {
    "/pets": {"post": {
      "operationId": "addPet",
      "consumes": ["application/x-www-form-urlencoded"],
      "parameters": [
        {"name": "name", "in": "formData", "type": "string", "required": true},
        {"name": "age", "in": "formData", "type": "integer"}
      ],
      "responses": {"201": {"description": "Created", "schema": {
        "type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}
      }}}
    }},
    "/upload": {"post": {
      "operationId": "upload",
      "consumes": ["multipart/form-data"],
      "parameters": [{"name": "file", "in": "formData", "type": "file", "required": true}],
      "responses": {"204": {"description": "Stored"}}
    }}
  }
}"#;

fn post_form(uri: &str, content_type: &str, body: &str) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn form_data_bodies_are_decoded() {
    let h = offline_contract(FORM_CONTRACT);
    let form = "application/x-www-form-urlencoded";
    let resp = send(&h, post_form("/pets", form, "name=rex&age=3")).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["name"], "rex");

    let resp = send(&h, post_form("/pets", form, "name=rex&age=old")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = send(&h, post_form("/pets", form, "age=3")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let multipart = "--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nhello\r\n--XyZ--\r\n";
    let resp = send(
        &h,
        post_form("/upload", "multipart/form-data; boundary=XyZ", multipart),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn cors_preflight_and_simple_requests() {
    let h = offline();
    let req = Request::options("/orders")
        .header(header::ORIGIN, "http://localhost:3000")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(
            header::ACCESS_CONTROL_REQUEST_HEADERS,
            "content-type,x-mockingbird-session",
        )
        .body(Body::empty())
        .unwrap();
    let resp = send(&h, req).await;
    assert!(
        matches!(resp.status(), StatusCode::OK | StatusCode::NO_CONTENT),
        "{}",
        resp.status()
    );
    assert_eq!(header_str(&resp, "access-control-allow-origin"), "*");
    assert!(resp.headers().contains_key("access-control-allow-methods"));
    assert!(resp.headers().contains_key("access-control-allow-headers"));

    let req = Request::get("/orders/o1")
        .header(header::ORIGIN, "http://localhost:3000")
        .body(Body::empty())
        .unwrap();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header_str(&resp, "access-control-allow-origin"), "*");
    let exposed = header_str(&resp, "access-control-expose-headers").to_ascii_lowercase();
    assert!(exposed.contains("x-mockingbird-source"), "{exposed}");
    assert!(
        exposed.contains("x-mockingbird-fallback-reason"),
        "{exposed}"
    );
}

const ONLY_404_CONTRACT: &str = r#"
openapi: 3.0.3
info: {title: T, version: "1"}
paths:
  /items/{id}:
    get:
      operationId: getItem
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
        - {name: limit, in: query, schema: {type: integer}}
      responses:
        "200":
          description: OK
          content:
            application/vnd.api+json:
              schema: {type: object, properties: {id: {type: string}}}
        "404":
          description: Not found
          content:
            application/json:
              schema: {type: object, required: [gone], properties: {gone: {type: boolean}}}
        "409":
          description: Conflict
          content:
            application/problem+json:
              schema: {type: object, required: [title], properties: {title: {type: string}}}
  /notes:
    post:
      operationId: addNote
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {text: {type: string}}}
      responses:
        "201":
          description: Created
          content:
            application/json:
              schema: {type: object, properties: {text: {type: string}}}
"#;

#[tokio::test]
async fn invalid_input_ignores_unrelated_4xx_variants() {
    let h = offline_contract(ONLY_404_CONTRACT);
    let resp = send(&h, get("/items/a?limit=abc")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        header_str(&resp, "content-type"),
        "application/problem+json"
    );
    assert_eq!(body_json(resp).await["title"], "Invalid request");
}

#[tokio::test]
async fn responses_use_declared_json_content_type() {
    let h = offline_contract(ONLY_404_CONTRACT);
    let resp = send(&h, get("/items/a")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        header_str(&resp, "content-type"),
        "application/vnd.api+json"
    );

    let req = Request::get("/items/a")
        .header("x-mockingbird-status", "409")
        .body(Body::empty())
        .unwrap();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        header_str(&resp, "content-type"),
        "application/problem+json"
    );
}

#[tokio::test]
async fn optional_request_body_may_be_omitted() {
    let h = offline_contract(ONLY_404_CONTRACT);
    let req = Request::post("/notes").body(Body::empty()).unwrap();
    assert_eq!(send(&h, req).await.status(), StatusCode::CREATED);
    // A required body is still enforced.
    let h = offline();
    let req = Request::post("/orders").body(Body::empty()).unwrap();
    assert_eq!(send(&h, req).await.status(), StatusCode::BAD_REQUEST);
}

/// Without the `sse` feature an event-stream operation is a 501 problem.
#[cfg(not(feature = "sse"))]
#[tokio::test]
async fn event_stream_without_sse_feature_is_not_implemented() {
    let path = format!(
        "{}/../../fixtures/openapi/events-3.1.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let bundle = Arc::new(compile(&std::fs::read(path).unwrap()).unwrap());
    let rt = Arc::new(MockRuntime::new(
        "events",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    ));
    let resp = http_rest::router(rt).oneshot(get("/ticks")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(body_json(resp).await["title"], "Streaming not enabled");
}

const SERVER_ERRORS_CONTRACT: &str = r##"
openapi: 3.0.3
info: {title: Outage API, version: "1"}
paths:
  /things:
    get:
      operationId: listThings
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {type: array, items: {type: string}}
        "500":
          description: broken
          content:
            application/json:
              schema:
                type: object
                required: [error]
                properties: {error: {type: string}}
        "503":
          description: down
          content:
            application/json:
              schema:
                type: object
                required: [retryIn]
                properties: {retryIn: {type: integer}}
"##;

fn server_errors() -> Harness {
    let bundle = Arc::new(compile(SERVER_ERRORS_CONTRACT.as_bytes()).unwrap());
    let mut rt = MockRuntime::new(
        "outage",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    rt.header_overrides = true;
    let rt = Arc::new(rt);
    Harness {
        router: http_rest::router(Arc::clone(&rt)),
        rt,
        provider: None,
    }
}

fn with_status(uri: &str, status: &str) -> Request<Body> {
    Request::get(uri)
        .header("x-mockingbird-status", status)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn forced_declared_server_errors_use_their_variants() {
    let h = server_errors();
    let resp = send(&h, with_status("/things", "500")).await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(header_str(&resp, "content-type"), "application/json");
    assert!(body_json(resp).await["error"].is_string());

    let resp = send(&h, with_status("/things", "503")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header_str(&resp, "content-type"), "application/json");
    assert!(body_json(resp).await["retryIn"].is_i64());
}

#[tokio::test]
async fn forced_undeclared_server_errors_are_native_problems() {
    // getOrder declares only 200 and 404: an undeclared 5xx is answered
    // as a plain problem with that status, like the other protocols do.
    let h = offline();
    for (code, title) in [
        ("500", "Internal Server Error"),
        ("503", "Service Unavailable"),
    ] {
        let resp = send(&h, with_status("/orders/o1", code)).await;
        assert_eq!(resp.status().as_str(), code);
        assert_eq!(
            header_str(&resp, "content-type"),
            "application/problem+json"
        );
        let body = body_json(resp).await;
        assert_eq!(body["title"], title);
        assert_eq!(body["status"], code.parse::<u16>().unwrap());
    }
}

#[tokio::test]
async fn forced_status_undeclared_client_error_and_invalid_value_are_400() {
    let h = offline();
    // createOrder declares no 404 and no default variant.
    let req = Request::post("/orders")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-mockingbird-status", "404")
        .body(Body::from(r#"{"customerId":"c1","total":10}"#))
        .unwrap();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["title"], "Undeclared status override");

    for bad in ["600", "5xx", "99"] {
        let resp = send(&h, with_status("/orders/o1", bad)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(body_json(resp).await["title"], "Invalid override");
    }
}

#[tokio::test]
async fn llm_failure_sends_the_fallback_reason() {
    let h = harness(Some(vec![Err(LlmError::Timeout)]), true);
    let resp = send(&h, get("/orders/o1")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "fallback");
    let reason = header_str(&resp, "x-mockingbird-fallback-reason");
    assert!(reason.contains("timed out"), "{reason}");
}

#[tokio::test]
async fn offline_fallback_says_no_llm_is_configured() {
    let h = offline();
    let resp = send(&h, get("/orders/o1")).await;
    let reason = header_str(&resp, "x-mockingbird-fallback-reason");
    assert!(reason.contains("no LLM"), "{reason}");
}

#[tokio::test]
async fn llm_answer_sends_no_fallback_reason() {
    let h = harness(Some(vec![Ok(order("o1", "c1").to_string())]), true);
    let resp = send(&h, get("/orders/o1")).await;
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "llm");
    assert!(
        resp.headers()
            .get("x-mockingbird-fallback-reason")
            .is_none()
    );
}

/// A Stripe-shaped OpenAPI 3 contract: form-encoded bodies and
/// bracket-notation query parameters.
const STRIPE_LIKE: &str = r##"
openapi: 3.0.0
info: {title: Payments, version: "1"}
paths:
  /v1/payment_intents:
    post:
      operationId: PostPaymentIntents
      requestBody:
        required: true
        content:
          application/x-www-form-urlencoded:
            schema:
              type: object
              required: [amount, currency]
              properties:
                amount: {type: integer}
                currency: {type: string}
                payment_method_types: {type: array, items: {type: string}}
                expand: {type: array, items: {type: string, maxLength: 5000}}
                automatic_payment_methods:
                  type: object
                  properties: {enabled: {type: boolean}}
                metadata:
                  anyOf:
                    - {type: object, additionalProperties: {type: string}}
                    - {type: string, enum: [""]}
                shipping: {$ref: "#/components/schemas/shipping"}
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {$ref: "#/components/schemas/payment_intent"}
        default:
          description: Error
          content:
            application/json:
              schema: {$ref: "#/components/schemas/error"}
    get:
      operationId: GetPaymentIntents
      parameters:
        - name: created
          in: query
          style: deepObject
          explode: true
          schema:
            anyOf:
              - {type: object, properties: {gte: {type: integer}, lt: {type: integer}}}
              - {type: integer}
        - {name: expand, in: query, style: deepObject, explode: true,
           schema: {type: array, items: {type: string}}}
        - {name: limit, in: query, schema: {type: integer}}
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {type: object, properties: {data: {type: array, items: {$ref: "#/components/schemas/payment_intent"}}}}
  /v1/files:
    post:
      operationId: PostFiles
      requestBody:
        content:
          multipart/form-data:
            schema:
              type: object
              required: [file, purpose]
              properties:
                file: {type: string, format: binary}
                purpose: {type: string, enum: [dispute_evidence, identity_document]}
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {type: object, properties: {id: {type: string}}}
  /v1/ping:
    post:
      operationId: Ping
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {type: object, properties: {ok: {type: boolean}}}
components:
  schemas:
    shipping:
      type: object
      properties:
        name: {type: string}
        address: {type: object, properties: {line1: {type: string}, postal_code: {type: string}}}
    payment_intent:
      type: object
      required: [id, amount]
      properties:
        id: {type: string}
        amount: {type: integer}
    error:
      type: object
      properties:
        error:
          type: object
          properties:
            message: {type: string}
            type: {type: string}
"##;

fn contract_harness(src: &str, script: Vec<Value>) -> Harness {
    let provider = Arc::new(ScriptedProvider::new(
        script.into_iter().map(|v| Ok(v.to_string())).collect(),
    ));
    let rt = Arc::new(MockRuntime::new(
        "inline",
        Arc::new(compile(src.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    ));
    Harness {
        router: http_rest::router(Arc::clone(&rt)),
        rt,
        provider: Some(provider),
    }
}

/// The `<request>` block of the `n`th prompt the engine sent.
fn engine_request(h: &Harness, n: usize) -> Value {
    let requests = h.provider.as_ref().unwrap().requests();
    let prompt = requests[n]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .find(|c| c.contains("<request>"))
        .unwrap();
    let start = prompt.find("<request>\n").unwrap() + "<request>\n".len();
    let end = prompt[start..].find("\n</request>").unwrap() + start;
    serde_json::from_str(&prompt[start..end]).unwrap()
}

const FORM: &str = "application/x-www-form-urlencoded";

#[tokio::test]
async fn openapi3_form_body_reaches_the_engine_decoded() {
    let pi = json!({"id": "pi_1", "amount": 5999});
    let h = contract_harness(STRIPE_LIKE, vec![pi.clone(), pi]);
    let body = "amount=5999&currency=eur&payment_method_types[0]=card\
        &metadata[order_id]=42&expand[]=customer&expand[]=latest_charge\
        &automatic_payment_methods[enabled]=true\
        &shipping[name]=Jo&shipping[address][postal_code]=10115";
    let resp = send(&h, post_form("/v1/payment_intents", FORM, body)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        engine_request(&h, 0)["body"],
        json!({
            "amount": 5999,
            "currency": "eur",
            "payment_method_types": ["card"],
            "metadata": {"order_id": "42"},
            "expand": ["customer", "latest_charge"],
            "automatic_payment_methods": {"enabled": true},
            "shipping": {"name": "Jo", "address": {"postal_code": "10115"}},
        })
    );

    // Percent-encoded brackets, as stripe-node sends them.
    let body = "amount=5999&currency=eur&payment_method_types%5B0%5D=card&metadata=";
    let resp = send(&h, post_form("/v1/payment_intents", FORM, body)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        engine_request(&h, 1)["body"],
        json!({"amount": 5999, "currency": "eur", "payment_method_types": ["card"], "metadata": ""})
    );

    // Still validated against the declared schema.
    let resp = send(
        &h,
        post_form("/v1/payment_intents", FORM, "amount=lots&currency=eur"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn bracket_query_parameters_are_nested() {
    let h = contract_harness(STRIPE_LIKE, vec![json!({"data": []})]);
    let resp = send(
        &h,
        get("/v1/payment_intents?created[gte]=10&expand[]=data.customer&expand[]=data.invoice&limit=3"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        engine_request(&h, 0)["query"],
        json!({"created": {"gte": 10}, "expand": ["data.customer", "data.invoice"], "limit": 3})
    );
}

#[tokio::test]
async fn openapi3_multipart_body_is_decoded() {
    let h = contract_harness(STRIPE_LIKE, vec![json!({"id": "file_1"})]);
    let multipart = "--XyZ\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\ndispute_evidence\r\n\
        --XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\n\
        Content-Type: image/png\r\n\r\n\x01\x02\r\n--XyZ--\r\n";
    let resp = send(
        &h,
        post_form("/v1/files", "multipart/form-data; boundary=XyZ", multipart),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        engine_request(&h, 0)["body"],
        json!({"purpose": "dispute_evidence", "file": "a.png"})
    );
}

/// A body the contract does not describe still reaches the engine.
#[tokio::test]
async fn undeclared_bodies_are_never_null() {
    let ok = json!({"ok": true});
    let h = contract_harness(STRIPE_LIKE, vec![ok.clone(), ok.clone(), ok]);
    send(&h, post_form("/v1/ping", FORM, "a[b]=1&c=2")).await;
    assert_eq!(
        engine_request(&h, 0)["body"],
        json!({"a": {"b": "1"}, "c": "2"})
    );
    send(&h, post_form("/v1/ping", "text/plain", "hello")).await;
    assert_eq!(engine_request(&h, 1)["body"], json!("hello"));
    send(&h, post_json("/v1/ping", r#"{"x":1}"#)).await;
    assert_eq!(engine_request(&h, 2)["body"], json!({"x": 1}));
}

#[derive(Default)]
struct Recorded(std::sync::Mutex<Vec<mb_protocols::RequestRecord>>);

impl mb_protocols::RequestRecorder for Recorded {
    fn record(&self, r: mb_protocols::RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

/// A request failing validation is answered with the contract's error
/// shape, which carries the reasons, as do a response header and the
/// request record.
#[tokio::test]
async fn validation_failures_stay_visible() {
    let recorded = Arc::new(Recorded::default());
    let mut rt = MockRuntime::new(
        "inline",
        Arc::new(compile(STRIPE_LIKE.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    rt.recorder = Some(recorded.clone());
    let router = http_rest::router(Arc::new(rt));
    let resp = router
        .oneshot(post_form(
            "/v1/payment_intents",
            FORM,
            "amount=lots&currency=eur&metadata[k]=v",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let header = header_str(&resp, "x-mockingbird-validation").to_string();
    assert!(header.contains("/amount"), "{header}");
    let body = body_json(resp).await;
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("/amount"), "{body}");

    let records = recorded.0.lock().unwrap().clone();
    // The record shows the body as decoded.
    assert_eq!(
        records[0].request["body"],
        json!({"amount": "lots", "currency": "eur", "metadata": {"k": "v"}})
    );
    let validation = records[0].request["validation"].as_array().unwrap();
    assert!(
        validation[0].as_str().unwrap().contains("/amount"),
        "{validation:?}"
    );

    // The orders contract's 400 `Error` has a top-level `message`.
    let h = offline();
    let resp = send(&h, post_json("/orders", r#"{"total":-1}"#)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(!header_str(&resp, "x-mockingbird-validation").is_empty());
    let body = body_json(resp).await;
    assert!(body["message"].as_str().unwrap().contains("body"), "{body}");
}

#[tokio::test]
async fn get_after_create_is_served_from_state() {
    let h = harness(Some(vec![Ok(order("o1", "c1").to_string())]), true);
    send(
        &h,
        post_json("/orders", r#"{"customerId":"c1","total":10}"#),
    )
    .await;
    let resp = send(&h, get("/orders/o1")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "state");
    assert_eq!(body_json(resp).await, order("o1", "c1"));
    assert_eq!(
        h.provider.as_ref().unwrap().requests().len(),
        1,
        "no LLM call for the GET"
    );
}

#[tokio::test]
async fn stored_partial_resource_is_generated_and_overlaid() {
    let h = offline();
    h.rt.state
        .upsert(
            &session("default"),
            StoredResource {
                resource_type: "orders".into(),
                id: "o1".into(),
                data: json!({"id": "o1", "customerId": "c9"}),
            },
        )
        .await
        .unwrap();
    let resp = send(&h, get("/orders/o1")).await;
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "fallback");
    let body = body_json(resp).await;
    assert_eq!(body["customerId"], "c9");
    assert!(body["total"].is_number(), "{body}");
}

#[tokio::test]
async fn scenario_and_forced_status_skip_the_stored_answer() {
    let h = offline();
    h.rt.state
        .upsert(
            &session("default"),
            StoredResource {
                resource_type: "orders".into(),
                id: "o1".into(),
                data: order("o1", "c1"),
            },
        )
        .await
        .unwrap();
    let forced = Request::get("/orders/o1")
        .header("x-mockingbird-status", "404")
        .body(Body::empty())
        .unwrap();
    let resp = send(&h, forced).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_ne!(header_str(&resp, "x-mockingbird-source"), "state");
}
