#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use mb_engine::{Engine, EngineConfig, ScenarioSpec, SchemaValidator};
use mb_import::compile;
use mb_llm::{LlmError, Provider, ScriptedProvider};
use mb_mcm::{Bundle, HttpMethod};
use mb_protocols::http_rest::{self, RouteTable};
use mb_protocols::{MockRuntime, ScenarioRuntime, UnknownIds};
use mb_state::{
    CachingStateStore, DEFAULT_CACHE_ENTRIES, MemoryStore, SessionKey, StateStore, StoredResource,
};
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

/// The id the scripted LLM gives a created order: a uuid, as the
/// contract's `format: uuid` asks (other values are refilled).
const O1: &str = "0e1f2a3b-4c5d-4e6f-8a9b-0c1d2e3f4a5b";

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
            Ok(order(O1, "c1").to_string()),
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
    assert_eq!(body_json(resp).await["id"], O1);

    let resp = send(&h, get(&format!("/orders/{O1}"))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header_str(&resp, "content-type"), "application/json");
    let body = body_json(resp).await;
    assert_eq!(body["id"], O1);
    assert_eq!(body["customerId"], "c1");
}

#[tokio::test]
async fn sessions_isolate_state() {
    let h = harness(Some(vec![Ok(order(O1, "c1").to_string())]), true);
    let req = Request::post("/orders")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-mockingbird-session", "a")
        .body(Body::from(r#"{"customerId":"c1","total":10}"#))
        .unwrap();
    assert_eq!(send(&h, req).await.status(), StatusCode::CREATED);
    assert!(
        h.rt.state
            .get(&session("a"), "orders", O1)
            .await
            .unwrap()
            .is_some()
    );

    // The script is exhausted, so session `b` is served by the faker. The
    // id follows the path, but none of session `a`'s stored fields leak.
    let req = Request::get(format!("/orders/{O1}"))
        .header("x-mockingbird-session", "b")
        .body(Body::empty())
        .unwrap();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["id"], O1);
    assert_ne!(body["customerId"], "c1", "{body}");
    assert!(
        h.rt.state
            .get(&session("b"), "orders", O1)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn delete_returns_204_and_removes_state() {
    let h = harness(Some(vec![Ok(order(O1, "c1").to_string())]), true);
    let resp = send(
        &h,
        post_json("/orders", r#"{"customerId":"c1","total":10}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(
        h.rt.state
            .get(&session("default"), "orders", O1)
            .await
            .unwrap()
            .is_some()
    );

    let req = Request::delete(format!("/orders/{O1}"))
        .body(Body::empty())
        .unwrap();
    let resp = send(&h, req).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "llm");
    assert!(resp.headers().get(header::CONTENT_TYPE).is_none());
    assert!(body_bytes(resp).await.is_empty());
    assert!(
        h.rt.state
            .get(&session("default"), "orders", O1)
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
    // A client error, not a generation that fell back.
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "rejected");
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

const MAX_PARAMS: usize = mb_protocols::limits::MAX_REQUEST_PARAMS;

fn many_pairs(n: usize) -> String {
    (0..n)
        .map(|i| format!("k{i}=v"))
        .collect::<Vec<_>>()
        .join("&")
}

async fn assert_too_many(resp: Response) {
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        header_str(&resp, "content-type"),
        "application/problem+json"
    );
    let body = body_json(resp).await;
    assert_eq!(body["title"], "Too many parameters", "{body}");
}

/// Query and form decoding runs before auth, so the number of pairs is
/// capped: one past the cap is a problem+json 400, the cap itself is served.
#[tokio::test]
async fn too_many_query_or_form_pairs_are_rejected() {
    let h = offline();
    let resp = send(&h, get(&format!("/orders?{}", many_pairs(MAX_PARAMS)))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = send(&h, get(&format!("/orders?{}", many_pairs(MAX_PARAMS + 1)))).await;
    assert_too_many(resp).await;

    let h = offline_contract(FORM_CONTRACT);
    let form = "application/x-www-form-urlencoded";
    let body = format!("name=rex&{}", many_pairs(MAX_PARAMS - 1));
    let resp = send(&h, post_form("/pets", form, &body)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = format!("name=rex&{}", many_pairs(MAX_PARAMS));
    assert_too_many(send(&h, post_form("/pets", form, &body)).await).await;

    let part = "--XyZ\r\nContent-Disposition: form-data; name=\"f\"\r\n\r\nv\r\n";
    let multipart = format!("{}--XyZ--\r\n", part.repeat(MAX_PARAMS + 1));
    let resp = send(
        &h,
        post_form("/upload", "multipart/form-data; boundary=XyZ", &multipart),
    )
    .await;
    assert_too_many(resp).await;
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
    contract_harness_with(src, script, memory_state())
}

fn contract_harness_with(src: &str, script: Vec<Value>, state: Arc<dyn StateStore>) -> Harness {
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
        state,
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
    // Logged as a rejection, so `verify --no-fallbacks` does not blame
    // generation for it.
    assert_eq!(records[0].source.as_deref(), Some("rejected"));
    assert_eq!(records[0].fallback_reason, None);
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
    let h = harness(Some(vec![Ok(order(O1, "c1").to_string())]), true);
    send(
        &h,
        post_json("/orders", r#"{"customerId":"c1","total":10}"#),
    )
    .await;
    let resp = send(&h, get(&format!("/orders/{O1}"))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "state");
    assert_eq!(body_json(resp).await, order(O1, "c1"));
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

#[tokio::test]
async fn unknown_ids_404_when_opted_in() {
    let h = harness_with(None, memory_state(), |rt| {
        rt.unknown_ids = UnknownIds::NotFound
    });
    let resp = send(&h, get("/orders/nope")).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "state");
    let body = body_json(resp).await;
    assert!(
        body["code"].is_i64() && body["message"].is_string(),
        "{body}"
    );

    let created = body_json(
        send(
            &h,
            post_json("/orders", r#"{"customerId":"c1","total":10}"#),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap();
    assert_eq!(
        send(&h, get(&format!("/orders/{id}"))).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn unknown_ids_404_spares_lists_and_forced_status() {
    let h = harness_with(None, memory_state(), |rt| {
        rt.unknown_ids = UnknownIds::NotFound
    });
    assert_eq!(send(&h, get("/orders")).await.status(), StatusCode::OK);
    let forced = Request::get("/orders/nope")
        .header("x-mockingbird-status", "200")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&h, forced).await.status(), StatusCode::OK);
    // Default: keep generating.
    assert_eq!(
        send(&offline(), get("/orders/nope")).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn unknown_id_without_declared_404_is_problem_json() {
    let src = r#"
openapi: 3.1.0
info: {title: Pets, version: "1"}
paths:
  /pets/{petId}:
    get:
      operationId: getPet
      parameters: [{name: petId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: OK, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}
"#;
    let bundle = Arc::new(compile(src.as_bytes()).unwrap());
    let mut rt = MockRuntime::new(
        "inline",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        memory_state(),
    );
    rt.unknown_ids = UnknownIds::NotFound;
    let router = http_rest::router(Arc::new(rt));
    let resp = router.oneshot(get("/pets/p1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        header_str(&resp, "content-type"),
        "application/problem+json"
    );
}

fn cached_state() -> Arc<dyn StateStore> {
    CachingStateStore::wrap(
        memory_state(),
        Duration::from_secs(3600),
        DEFAULT_CACHE_ENTRIES,
    )
}

#[tokio::test]
async fn repeated_list_is_served_from_the_cache_until_a_write() {
    let h = harness_with(None, cached_state(), |_| {});
    let first = send(&h, get("/orders")).await;
    assert_eq!(header_str(&first, "x-mockingbird-source"), "fallback");
    let first = body_json(first).await;
    let second = send(&h, get("/orders")).await;
    assert_eq!(header_str(&second, "x-mockingbird-source"), "cache");
    assert_eq!(body_json(second).await, first);
    send(
        &h,
        post_json("/orders", r#"{"customerId":"c1","total":10}"#),
    )
    .await;
    let third = send(&h, get("/orders")).await;
    assert_eq!(header_str(&third, "x-mockingbird-source"), "fallback");
}

#[tokio::test]
async fn cache_is_bypassed_when_off_or_under_a_scenario() {
    let h = harness_with(None, cached_state(), |rt| rt.consistency = false);
    send(&h, get("/orders")).await;
    assert_eq!(
        header_str(&send(&h, get("/orders")).await, "x-mockingbird-source"),
        "fallback"
    );

    let h = harness_with(None, cached_state(), |rt| {
        rt.scenarios.insert(
            "calm".into(),
            ScenarioRuntime {
                spec: ScenarioSpec {
                    name: "calm".into(),
                    ..ScenarioSpec::default()
                },
                ..ScenarioRuntime::default()
            },
        );
    });
    let with_scenario = || {
        Request::get("/orders")
            .header("x-mockingbird-scenario", "calm")
            .body(Body::empty())
            .unwrap()
    };
    send(&h, with_scenario()).await;
    assert_eq!(
        header_str(&send(&h, with_scenario()).await, "x-mockingbird-source"),
        "fallback"
    );
}

#[tokio::test]
async fn listed_items_are_served_by_id_from_state() {
    let h = offline();
    let mut changes = h.rt.state.subscribe();
    let list = body_json(send(&h, get("/orders")).await).await;
    let item = list[0].clone();
    assert!(changes.try_recv().is_err(), "seeding publishes no change");
    let resp = send(
        &h,
        get(&format!("/orders/{}", item["id"].as_str().unwrap())),
    )
    .await;
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "state");
    assert_eq!(body_json(resp).await, item);
}

#[tokio::test]
async fn listing_keeps_a_created_resource() {
    let h = harness(
        Some(vec![
            Ok(order(O1, "c1").to_string()),
            Ok(json!([order(O1, "someone-else")]).to_string()),
        ]),
        true,
    );
    send(
        &h,
        post_json("/orders", r#"{"customerId":"c1","total":10}"#),
    )
    .await;
    send(&h, get("/orders")).await;
    let kept =
        h.rt.state
            .get(&session("default"), "orders", O1)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(kept.data["customerId"], "c1");
}

#[tokio::test]
async fn no_seeding_when_consistency_is_off() {
    let h = harness_with(None, memory_state(), |rt| rt.consistency = false);
    let list = body_json(send(&h, get("/orders")).await).await;
    let resp = send(
        &h,
        get(&format!("/orders/{}", list[0]["id"].as_str().unwrap())),
    )
    .await;
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "fallback");
}

#[tokio::test]
async fn create_never_reuses_a_stored_id() {
    let h = harness(Some(vec![Ok(order("o1", "c2").to_string())]), true);
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
    let body = body_json(
        send(
            &h,
            post_json("/orders", r#"{"customerId":"c2","total":10}"#),
        )
        .await,
    )
    .await;
    let id = body["id"].as_str().unwrap();
    assert_ne!(id, "o1");
    let kept =
        h.rt.state
            .get(&session("default"), "orders", "o1")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(kept.data["customerId"], "c1");
    assert!(
        h.rt.state
            .get(&session("default"), "orders", id)
            .await
            .unwrap()
            .is_some()
    );
}

/// `/users/{username}` reads by login while state is keyed by `id`: a
/// missing entry says nothing, so `unknown_ids: 404` keeps generating.
#[tokio::test]
async fn unknown_ids_404_only_when_the_path_names_the_identity() {
    let src = r#"
openapi: 3.1.0
info: {title: Users, version: "1"}
paths:
  /users/{username}:
    get:
      operationId: getUser
      parameters: [{name: username, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: OK, content: {application/json: {schema: {type: object, required: [id, login], properties: {id: {type: integer}, login: {type: string}}}}}}
        "404": {description: Not found}
"#;
    let bundle = Arc::new(compile(src.as_bytes()).unwrap());
    let mut rt = MockRuntime::new(
        "users",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        memory_state(),
    );
    rt.unknown_ids = UnknownIds::NotFound;
    let router = http_rest::router(Arc::new(rt));
    let resp = router.oneshot(get("/users/octocat")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

const PLAID_LIKE: &str = r##"
openapi: 3.0.3
info: {title: plaid-like, version: "1"}
paths:
  /accounts/get:
    post:
      operationId: accountsGet
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/Request"}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [accounts, request_id]
                properties:
                  accounts: {type: array, items: {$ref: "#/components/schemas/Account"}}
                  request_id: {type: string}
  /auth/get:
    post:
      operationId: authGet
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/Request"}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [accounts, request_id]
                properties:
                  accounts: {type: array, items: {$ref: "#/components/schemas/Account"}}
                  request_id: {type: string}
components:
  schemas:
    Request:
      type: object
      properties:
        access_token: {type: string}
    Account:
      type: object
      required: [account_id, name]
      properties:
        account_id: {type: string}
        name: {type: string}
"##;

/// RPC-style `POST` reads (Plaid's `/accounts/get`) are reads: a repeat is
/// served from the cache, and their items are seeded, so `/auth/get` is
/// generated knowing the accounts `/accounts/get` answered.
#[tokio::test]
async fn rpc_style_post_reads_are_cached_and_seeded() {
    let bundle = compile(PLAID_LIKE.as_bytes()).unwrap();
    assert_eq!(
        bundle.operation("accountsGet").unwrap().kind,
        mb_mcm::OperationKind::Query
    );
    let accounts = json!({
        "accounts": [
            {"account_id": "BxBXxLj1m4HMXBm9WZZmCWVbPjX16EHwv99vp", "name": "Plaid Checking"},
            {"account_id": "dVzbVMLjrxTnLjX4G66XUp5GLklm4oiZy88yK", "name": "Plaid Saving"},
        ],
        "request_id": "r1",
    });
    let provider = Arc::new(ScriptedProvider::new(vec![
        Ok(accounts.to_string()),
        Ok(json!({"accounts": accounts["accounts"], "request_id": "r2"}).to_string()),
    ]));
    let rt = Arc::new(MockRuntime::new(
        "plaid",
        Arc::new(bundle),
        Arc::new(Engine::new(
            Some(provider.clone() as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        cached_state(),
    ));
    let router = http_rest::router(Arc::clone(&rt));
    let body = r#"{"access_token":"access-sandbox-1"}"#;
    let first = router
        .clone()
        .oneshot(post_json("/accounts/get", body))
        .await
        .unwrap();
    assert_eq!(header_str(&first, "x-mockingbird-source"), "llm");
    let first = body_json(first).await;
    let again = router
        .clone()
        .oneshot(post_json("/accounts/get", body))
        .await
        .unwrap();
    assert_eq!(header_str(&again, "x-mockingbird-source"), "cache");
    assert_eq!(body_json(again).await, first);

    let auth = router
        .clone()
        .oneshot(post_json("/auth/get", body))
        .await
        .unwrap();
    assert_eq!(auth.status(), StatusCode::OK);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let prompt: String = requests[1]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        prompt.contains("BxBXxLj1m4HMXBm9WZZmCWVbPjX16EHwv99vp"),
        "{prompt}"
    );
}

/// Path ids that are not `id`: GitHub's `issue_number`, Twilio's
/// `{Sid}.json`, Jira's `issueIdOrKey`.
const PATH_IDS: &str = r##"
openapi: 3.0.3
info: {title: Ids, version: "1"}
components:
  schemas:
    Issue:
      type: object
      properties: {id: {type: integer}, number: {type: integer}, title: {type: string}, url: {type: string}}
    Message:
      type: object
      properties: {sid: {type: string}, body: {type: string}}
    JiraIssue:
      type: object
      properties: {id: {type: string}, key: {type: string}, summary: {type: string}}
paths:
  /repos/{owner}/{repo}/issues:
    parameters:
      - {name: owner, in: path, required: true, schema: {type: string}}
      - {name: repo, in: path, required: true, schema: {type: string}}
    get:
      operationId: listIssues
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Issue"}}}}}
    post:
      operationId: createIssue
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
  /repos/{owner}/{repo}/issues/{issue_number}:
    get:
      operationId: getIssue
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: issue_number, in: path, required: true, schema: {type: integer}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
  /Accounts/{AccountSid}/Messages.json:
    parameters:
      - {name: AccountSid, in: path, required: true, schema: {type: string}}
    get:
      operationId: ListMessage
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {messages: {type: array, items: {$ref: "#/components/schemas/Message"}}}}}}}
    post:
      operationId: CreateMessage
      requestBody: {content: {application/x-www-form-urlencoded: {schema: {type: object}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Message"}}}}
  /Accounts/{AccountSid}/Messages/{Sid}.json:
    get:
      operationId: FetchMessage
      parameters:
        - {name: AccountSid, in: path, required: true, schema: {type: string}}
        - {name: Sid, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Message"}}}}
  /rest/api/3/issue:
    post:
      operationId: createJiraIssue
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/JiraIssue"}}}}
  /rest/api/3/issue/{issueIdOrKey}:
    get:
      operationId: getJiraIssue
      parameters:
        - {name: issueIdOrKey, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/JiraIssue"}}}}
  /chat.postMessage:
    post:
      operationId: chat_postMessage
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {ok: {type: boolean}, ts: {type: string}}}}}}
  /conversations.history:
    get:
      operationId: conversations_history
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {messages: {type: array, items: {type: object, properties: {ts: {type: string}, text: {type: string}}}}}}}}}
"##;

#[tokio::test]
async fn created_issue_gets_a_new_number_and_reads_back_by_it() {
    let issue = |id: u64, number: u64, title: &str| {
        json!({"id": id, "number": number, "title": title,
               "url": format!("https://api.github.com/repos/o/r/issues/{number}")})
    };
    let h = contract_harness(
        PATH_IDS,
        vec![
            json!([issue(1, 411, "a"), issue(2, 412, "b")]),
            issue(3, 412, "new"),
            issue(9, 999, "stranger"),
        ],
    );
    send(&h, get("/repos/o/r/issues")).await;
    let created =
        body_json(send(&h, post_json("/repos/o/r/issues", r#"{"title":"new"}"#)).await).await;
    assert_eq!(created["number"], 413, "{created}");
    assert_eq!(
        created["url"],
        "https://api.github.com/repos/o/r/issues/413"
    );

    let resp = send(&h, get("/repos/o/r/issues/413")).await;
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "state");
    assert_eq!(body_json(resp).await["title"], "new");
    let resp = send(&h, get("/repos/o/r/issues/411")).await;
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "state");
    assert_eq!(body_json(resp).await["title"], "a");
}

#[tokio::test]
async fn twilio_and_jira_reads_find_the_created_resource() {
    let h = contract_harness(
        PATH_IDS,
        vec![
            json!({"sid": "SM0123", "body": "Hi Dana"}),
            json!({"id": "10001", "key": "PROJ-7", "summary": "Fix login"}),
        ],
    );
    send(
        &h,
        post_form(
            "/Accounts/AC1/Messages.json",
            FORM,
            "To=%2B15550100&Body=Hi+Dana",
        ),
    )
    .await;
    let resp = send(&h, get("/Accounts/AC1/Messages/SM0123.json")).await;
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "state");
    assert_eq!(body_json(resp).await["body"], "Hi Dana");

    send(&h, post_json("/rest/api/3/issue", r#"{"fields":{}}"#)).await;
    for path in ["/rest/api/3/issue/PROJ-7", "/rest/api/3/issue/10001"] {
        let resp = send(&h, get(path)).await;
        assert_eq!(header_str(&resp, "x-mockingbird-source"), "state", "{path}");
        assert_eq!(body_json(resp).await["summary"], "Fix login", "{path}");
    }
}

#[tokio::test]
async fn a_write_clears_cached_lists_of_other_resource_types() {
    let h = contract_harness_with(
        PATH_IDS,
        vec![
            json!({"messages": [{"ts": "1.1", "text": "old"}]}),
            json!({"ok": true, "ts": "2.2"}),
            json!({"messages": [{"ts": "2.2", "text": "new"}, {"ts": "1.1", "text": "old"}]}),
            json!({"messages": [{"sid": "SM1", "body": "old"}]}),
            json!({"sid": "SM2", "body": "Hi"}),
            json!({"messages": [{"sid": "SM2", "body": "Hi"}, {"sid": "SM1", "body": "old"}]}),
        ],
        cached_state(),
    );
    send(&h, get("/conversations.history")).await;
    let cached = send(&h, get("/conversations.history")).await;
    assert_eq!(header_str(&cached, "x-mockingbird-source"), "cache");
    send(
        &h,
        post_json("/chat.postMessage", r#"{"channel":"C1","text":"new"}"#),
    )
    .await;
    let after = send(&h, get("/conversations.history")).await;
    assert_eq!(header_str(&after, "x-mockingbird-source"), "llm");

    // Twilio: the list after a create is generated knowing the message.
    send(&h, get("/Accounts/AC1/Messages.json")).await;
    send(
        &h,
        post_form("/Accounts/AC1/Messages.json", FORM, "Body=Hi"),
    )
    .await;
    let after = send(&h, get("/Accounts/AC1/Messages.json")).await;
    assert_eq!(header_str(&after, "x-mockingbird-source"), "llm");
    let requests = h.provider.as_ref().unwrap().requests();
    let prompt = &requests.last().unwrap().messages[1].content;
    assert!(prompt.contains("\"sid\":\"SM2\""), "{prompt}");
}

const THROTTLED: &str = r##"
openapi: 3.0.3
info: {title: Throttled, version: "1"}
paths:
  /things:
    get:
      operationId: listThings
      responses:
        "200":
          description: ok
          headers:
            X-RateLimit-Remaining: {schema: {type: integer}}
          content: {application/json: {schema: {type: array, items: {type: string}}}}
        "429":
          description: slow down
          headers:
            Retry-After: {schema: {type: integer}}
          content: {application/json: {schema: {type: object, properties: {error: {type: string}}}}}
"##;

#[tokio::test]
async fn declared_and_scenario_headers_are_sent() {
    let scenario = |name: &str, status: u16| ScenarioRuntime {
        spec: ScenarioSpec {
            name: name.into(),
            status: Some(status),
            ..ScenarioSpec::default()
        },
        status: Some(status),
        headers: vec![
            ("Retry-After".into(), "30".into()),
            ("X-Request-Id".into(), "req-1".into()),
        ],
        ..ScenarioRuntime::default()
    };
    let mut rt = MockRuntime::new(
        "inline",
        Arc::new(compile(THROTTLED.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        memory_state(),
    );
    rt.header_overrides = true;
    rt.scenarios
        .insert("throttled".into(), scenario("throttled", 429));
    rt.scenarios
        .insert("outage".into(), scenario("outage", 503));
    let rt = Arc::new(rt);
    let h = Harness {
        router: http_rest::router(Arc::clone(&rt)),
        rt,
        provider: None,
    };

    let ok = send(&h, get("/things")).await;
    assert_eq!(ok.status(), StatusCode::OK);
    header_str(&ok, "x-ratelimit-remaining")
        .parse::<u64>()
        .unwrap();

    let limited = send(&h, with_status("/things", "429")).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = header_str(&limited, "retry-after").parse().unwrap();
    assert!((1..=60).contains(&retry));

    for (name, status) in [("throttled", 429), ("outage", 503)] {
        let req = Request::get("/things")
            .header("x-mockingbird-scenario", name)
            .body(Body::empty())
            .unwrap();
        let resp = send(&h, req).await;
        assert_eq!(resp.status().as_u16(), status, "{name}");
        assert_eq!(header_str(&resp, "retry-after"), "30", "{name}");
        assert_eq!(header_str(&resp, "x-request-id"), "req-1", "{name}");
    }
}

/// Slack's `chat.postMessage` answers `{ok, ts, message: {...}}`: the
/// message is stored where `conversations.history` lists `messages`, so the
/// history read after it is generated knowing the message.
#[tokio::test]
async fn a_wrapped_created_object_joins_the_list_it_belongs_to() {
    let h = contract_harness(
        &PATH_IDS.replace(
            "properties: {ok: {type: boolean}, ts: {type: string}}",
            "properties: {ok: {type: boolean}, ts: {type: string}, message: {type: object, properties: {ts: {type: string}, text: {type: string}}}}",
        ),
        vec![
            json!({"ok": true, "ts": "2.2", "message": {"ts": "2.2", "text": "Deploy finished"}}),
            json!({"messages": [{"ts": "2.2", "text": "Deploy finished"}]}),
        ],
    );
    send(
        &h,
        post_json(
            "/chat.postMessage",
            r#"{"channel":"C1","text":"Deploy finished"}"#,
        ),
    )
    .await;
    let stored =
        h.rt.state
            .get(&session_of(&h, "default"), "messages", "2.2")
            .await
            .unwrap();
    assert!(stored.is_some());
    send(&h, get("/conversations.history")).await;
    let requests = h.provider.as_ref().unwrap().requests();
    let prompt = &requests[1].messages[1].content;
    let related = prompt
        .split("<related_resources>")
        .nth(1)
        .unwrap_or_default();
    assert!(related.contains("Deploy finished"), "{prompt}");
}

fn session_of(h: &Harness, id: &str) -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: h.rt.name.clone(),
        session: id.into(),
    }
}
