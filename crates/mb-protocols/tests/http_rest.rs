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
use mb_state::{MemoryStore, SessionKey, StoredResource};
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

fn harness(script: Option<Vec<Result<String, LlmError>>>, header_overrides: bool) -> Harness {
    let provider = script.map(|s| Arc::new(ScriptedProvider::new(s)));
    let dyn_provider = provider.clone().map(|p| p as Arc<dyn Provider>);
    let rt = Arc::new(MockRuntime {
        name: "orders".into(),
        bundle: orders(),
        engine: Arc::new(Engine::new(dyn_provider, EngineConfig::default())),
        state: Arc::new(MemoryStore::new(Duration::from_secs(3600))),
        header_overrides,
    });
    Harness {
        router: http_rest::router(Arc::clone(&rt)),
        rt,
        provider,
    }
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
        .await;
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
    let rt = Arc::new(MockRuntime {
        name: "inline".into(),
        bundle,
        engine: Arc::new(Engine::new(None, EngineConfig::default())),
        state: Arc::new(MemoryStore::new(Duration::from_secs(3600))),
        header_overrides: true,
    });
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
