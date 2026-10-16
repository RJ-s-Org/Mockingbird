#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use mb_admin::{AdminState, DEFAULT_API_SECRET_REFS, router};
use mb_state::{
    MemoryStore, SessionInfo, SessionKey, StateChanged, StateError, StateStore, StoredResource,
};
use mb_store::{
    ContractStatus, ControlStore, DEFAULT_WORKSPACE, LlmProfile, NewContractVersion, NewMock,
    NewRequestLogEntry, RequestKind, ResourceSource, Scenario, ScenarioRules,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tower::ServiceExt;

struct Harness {
    app: Router,
    store: Arc<dyn ControlStore>,
}

struct Resp {
    status: StatusCode,
    content_type: String,
    text: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.text))
    }

    fn assert_problem(&self, status: StatusCode) -> Value {
        assert_eq!(self.status, status, "body: {}", self.text);
        assert_eq!(
            self.content_type, "application/problem+json",
            "{}",
            self.text
        );
        let body = self.json();
        assert_eq!(body["status"], status.as_u16(), "{body}");
        assert!(body["title"].is_string(), "{body}");
        assert!(body["type"].is_string(), "{body}");
        body
    }
}

async fn harness() -> Harness {
    harness_with(
        DEFAULT_API_SECRET_REFS
            .iter()
            .map(|r| r.to_string())
            .collect(),
    )
    .await
}

/// A harness whose API accepts the secret refs matching `api_secret_refs`.
async fn harness_with(api_secret_refs: Vec<String>) -> Harness {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    harness_full(api_secret_refs, state).await
}

/// A harness over the given state store.
async fn harness_full(api_secret_refs: Vec<String>, state: Arc<dyn StateStore>) -> Harness {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let app = router(AdminState {
        store: store.clone(),
        state,
        api_secret_refs: Arc::new(api_secret_refs),
    });
    Harness { app, store }
}

impl Harness {
    async fn send(
        &self,
        method: Method,
        uri: &str,
        body: Body,
        content_type: Option<&str>,
    ) -> Resp {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(ct) = content_type {
            req = req.header("content-type", ct);
        }
        let resp = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        Resp {
            status,
            content_type,
            text: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get(&self, uri: &str) -> Resp {
        self.send(Method::GET, uri, Body::empty(), None).await
    }

    async fn delete(&self, uri: &str) -> Resp {
        self.send(Method::DELETE, uri, Body::empty(), None).await
    }

    async fn json(&self, method: Method, uri: &str, body: Value) -> Resp {
        self.send(
            method,
            uri,
            Body::from(body.to_string()),
            Some("application/json"),
        )
        .await
    }

    async fn raw(&self, method: Method, uri: &str, body: &str) -> Resp {
        self.send(method, uri, Body::from(body.to_owned()), None)
            .await
    }

    async fn create_mock(&self, name: &str) -> Value {
        let r = self
            .json(Method::POST, "/api/v1/mocks", json!({ "name": name }))
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        r.json()
    }

    async fn upload(&self, mock: &str, contract: &str, activate: Option<bool>) -> Resp {
        let uri = match activate {
            Some(a) => format!("/api/v1/mocks/{mock}/contracts?activate={a}"),
            None => format!("/api/v1/mocks/{mock}/contracts"),
        };
        self.send(
            Method::POST,
            &uri,
            Body::from(contract.to_owned()),
            Some("application/json"),
        )
        .await
    }

    /// Upload raw contract bytes with a content type.
    async fn upload_bytes(&self, mock: &str, bytes: Vec<u8>, content_type: &str) -> Resp {
        self.send(
            Method::POST,
            &format!("/api/v1/mocks/{mock}/contracts"),
            Body::from(bytes),
            Some(content_type),
        )
        .await
    }

    /// `POST uri` with no body, as JSON (e.g. activation).
    async fn post_empty_json(&self, uri: &str) -> Resp {
        self.send(Method::POST, uri, Body::empty(), Some("application/json"))
            .await
    }

    async fn active_version(&self, mock: &str) -> Value {
        let r = self.get(&format!("/api/v1/mocks/{mock}")).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        r.json()["active_version"].clone()
    }
}

/// An OpenAPI 3.0 contract with three operations, as JSON.
fn contract_json(version: &str) -> String {
    json!({
        "openapi": "3.0.3",
        "info": { "title": "Orders", "version": version },
        "paths": {
            "/orders": {
                "get": {
                    "operationId": "listOrders",
                    "responses": { "200": { "description": "ok", "content": { "application/json": {
                        "schema": { "type": "array", "items": { "$ref": "#/components/schemas/Order" } }
                    } } } }
                },
                "post": {
                    "operationId": "createOrder",
                    "requestBody": { "required": true, "content": { "application/json": {
                        "schema": { "$ref": "#/components/schemas/Order" }
                    } } },
                    "responses": { "201": { "description": "created", "content": { "application/json": {
                        "schema": { "$ref": "#/components/schemas/Order" }
                    } } } }
                }
            },
            "/orders/{id}": {
                "get": {
                    "operationId": "getOrder",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "ok", "content": { "application/json": {
                        "schema": { "$ref": "#/components/schemas/Order" }
                    } } } }
                }
            }
        },
        "components": { "schemas": { "Order": {
            "type": "object",
            "required": ["id"],
            "properties": { "id": { "type": "string" }, "total": { "type": "number" } }
        } } }
    })
    .to_string()
}

/// The same shape as YAML, with one operation.
const CONTRACT_YAML: &str = r#"openapi: "3.0.3"
info:
  title: Orders
  version: "2.0"
paths:
  /orders:
    get:
      operationId: listOrders
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: array
                items:
                  type: object
                  properties:
                    id:
                      type: string
"#;

#[tokio::test]
async fn mock_lifecycle() {
    let h = harness().await;

    // Source is forced to `api`, the route prefix defaults to `/<name>`.
    let r = h
        .json(
            Method::POST,
            "/api/v1/mocks",
            json!({ "name": "orders", "source": "file" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let created = r.json();
    assert_eq!(created["name"], "orders");
    assert_eq!(created["source"], "api");
    assert_eq!(created["route_prefix"], "/orders");
    assert_eq!(created["active_version"], Value::Null);

    let r = h
        .json(Method::POST, "/api/v1/mocks", json!({ "name": "orders" }))
        .await;
    r.assert_problem(StatusCode::CONFLICT);

    let r = h
        .json(Method::POST, "/api/v1/mocks", json!({ "name": "Bad Name" }))
        .await;
    r.assert_problem(StatusCode::UNPROCESSABLE_ENTITY);

    let r = h
        .json(
            Method::POST,
            "/api/v1/mocks",
            json!({ "name": "other", "route_prefix": "no-slash" }),
        )
        .await;
    r.assert_problem(StatusCode::UNPROCESSABLE_ENTITY);

    let r = h.get("/api/v1/mocks").await;
    assert_eq!(r.status, StatusCode::OK);
    let list = r.json();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["name"], "orders");
    assert_eq!(list[0]["active_version"], Value::Null);

    let r = h
        .json(
            Method::PATCH,
            "/api/v1/mocks/orders",
            json!({
                "route_prefix": "/v2/orders",
                "persona": "a terse clerk",
                "llm_profile": "fast",
                "header_overrides": false
            }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let patched = r.json();
    assert_eq!(patched["route_prefix"], "/v2/orders");
    assert_eq!(patched["persona"], "a terse clerk");
    assert_eq!(patched["llm_profile"], "fast");
    assert_eq!(patched["header_overrides"], false);

    // `null` clears an optional field; absent fields are kept.
    let r = h
        .json(
            Method::PATCH,
            "/api/v1/mocks/orders",
            json!({ "persona": null }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let patched = r.json();
    assert_eq!(patched["persona"], Value::Null);
    assert_eq!(patched["llm_profile"], "fast");
    assert_eq!(patched["route_prefix"], "/v2/orders");

    let r = h
        .json(
            Method::PATCH,
            "/api/v1/mocks/orders",
            json!({ "route_prefix": "bad" }),
        )
        .await;
    r.assert_problem(StatusCode::UNPROCESSABLE_ENTITY);

    let r = h
        .json(
            Method::PATCH,
            "/api/v1/mocks/orders",
            json!({ "name": "x" }),
        )
        .await;
    r.assert_problem(StatusCode::UNPROCESSABLE_ENTITY);

    let r = h.get("/api/v1/mocks/orders").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["route_prefix"], "/v2/orders");

    let r = h.delete("/api/v1/mocks/orders").await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
    h.get("/api/v1/mocks/orders")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
    h.delete("/api/v1/mocks/orders")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
    h.json(
        Method::PATCH,
        "/api/v1/mocks/orders",
        json!({ "persona": "x" }),
    )
    .await
    .assert_problem(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn route_prefix_collisions_conflict() {
    let h = harness().await;
    h.create_mock("orders").await;
    h.create_mock("billing").await;

    // Exactly another mock's prefix: 409, on create and on patch.
    let p = h
        .json(
            Method::POST,
            "/api/v1/mocks",
            json!({ "name": "copy", "route_prefix": "/orders" }),
        )
        .await
        .assert_problem(StatusCode::CONFLICT);
    assert!(p["detail"].as_str().unwrap().contains("orders"), "{p}");
    assert!(h.get("/api/v1/mocks/copy").await.status == StatusCode::NOT_FOUND);
    h.json(
        Method::PATCH,
        "/api/v1/mocks/billing",
        json!({ "route_prefix": "/orders" }),
    )
    .await
    .assert_problem(StatusCode::CONFLICT);
    // The default prefix `/<name>` counts too.
    h.json(
        Method::POST,
        "/api/v1/mocks",
        json!({ "name": "other", "route_prefix": "/billing" }),
    )
    .await
    .assert_problem(StatusCode::CONFLICT);

    // Nested prefixes and re-saving a mock's own prefix are fine.
    let r = h
        .json(
            Method::POST,
            "/api/v1/mocks",
            json!({ "name": "nested", "route_prefix": "/orders/v2" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let r = h
        .json(
            Method::PATCH,
            "/api/v1/mocks/orders",
            json!({ "route_prefix": "/orders" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
}

#[tokio::test]
async fn upload_activate_and_list_versions() {
    let h = harness().await;
    h.create_mock("orders").await;

    // The first upload is activated even without `activate=true`.
    let v1 = contract_json("1.0");
    let r = h.upload("orders", &v1, None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(
        r.json(),
        json!({ "version": 1, "status": "active", "operations": 3, "channels": 0 })
    );

    // Later uploads stay approved unless asked to activate; YAML is accepted.
    let r = h.upload("orders", CONTRACT_YAML, None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(
        r.json(),
        json!({ "version": 2, "status": "approved", "operations": 1, "channels": 0 })
    );
    assert_eq!(h.active_version("orders").await, 1);

    let r = h.upload("orders", &contract_json("3.0"), Some(true)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(
        r.json(),
        json!({ "version": 3, "status": "active", "operations": 3, "channels": 0 })
    );
    assert_eq!(h.active_version("orders").await, 3);

    let r = h.upload("orders", &contract_json("4.0"), Some(false)).await;
    assert_eq!(r.json()["status"], "approved");
    assert_eq!(h.active_version("orders").await, 3);

    let r = h.get("/api/v1/mocks/orders/contracts").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let list = r.json();
    let versions: Vec<_> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["version"].as_i64().unwrap(),
                v["status"].clone(),
                v["operations"].clone(),
            )
        })
        .collect();
    assert_eq!(
        versions,
        vec![
            (4, json!("approved"), json!(3)),
            (3, json!("active"), json!(3)),
            (2, json!("approved"), json!(1)),
            (1, json!("approved"), json!(3)),
        ]
    );
    for v in list.as_array().unwrap() {
        assert!(v.get("source").is_none(), "{v}");
        assert!(v.get("bundle").is_none(), "{v}");
        assert!(v["source_digest"].is_string(), "{v}");
        assert!(v["created_at"].is_string(), "{v}");
    }

    let r = h.get("/api/v1/mocks/orders/contracts/1").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let detail = r.json();
    assert_eq!(detail["version"], 1);
    assert_eq!(detail["status"], "approved");
    assert!(detail.get("source").is_none(), "{detail}");
    let mut ops: Vec<_> = detail["operations"].as_array().unwrap().clone();
    ops.sort_by_key(|o| o["id"].as_str().unwrap().to_owned());
    assert_eq!(
        Value::Array(ops),
        json!([
            { "id": "createOrder", "method": "POST", "path": "/orders", "kind": "create",
              "protocol": "http", "target": "POST /orders" },
            { "id": "getOrder", "method": "GET", "path": "/orders/{id}", "kind": "query",
              "protocol": "http", "target": "GET /orders/{id}" },
            { "id": "listOrders", "method": "GET", "path": "/orders", "kind": "query",
              "protocol": "http", "target": "GET /orders" },
        ])
    );

    let r = h.get("/api/v1/mocks/orders/contracts/1/source").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.text, v1);
    let r = h.get("/api/v1/mocks/orders/contracts/2/source").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.text, CONTRACT_YAML);

    h.get("/api/v1/mocks/orders/contracts/9")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
    h.get("/api/v1/mocks/orders/contracts/9/source")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
    h.upload("nope", &v1, None)
        .await
        .assert_problem(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn operation_summaries_report_protocol_and_target() {
    let h = harness().await;
    h.create_mock("orders").await;
    let path = format!(
        "{}/../../fixtures/openapi/orders-3.0.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let source = std::fs::read_to_string(path).unwrap();
    let r = h.upload("orders", &source, None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let r = h.get("/api/v1/mocks/orders/contracts/1").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let detail = r.json();
    let ops = detail["operations"].as_array().unwrap();
    let get = ops.iter().find(|o| o["id"] == "getOrder").unwrap();
    assert_eq!(get["protocol"], "http", "{get}");
    assert_eq!(get["target"], "GET /orders/{orderId}", "{get}");
    assert_eq!(get["kind"], "query", "{get}");
    for op in ops {
        assert_eq!(op["protocol"], "http", "{op}");
    }
}

#[tokio::test]
async fn bad_contract_upload_keeps_active_version() {
    let h = harness().await;
    h.create_mock("orders").await;
    let r = h.upload("orders", &contract_json("1.0"), None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);

    let garbage = "title: not a contract\n";
    let expected = mb_import::compile(garbage.as_bytes())
        .unwrap_err()
        .to_string();
    let r = h.upload("orders", garbage, Some(true)).await;
    let problem = r.assert_problem(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        problem["detail"].as_str().unwrap().contains(&expected),
        "{problem} lacks {expected:?}"
    );

    assert_eq!(h.active_version("orders").await, 1);
    let list = h.get("/api/v1/mocks/orders/contracts").await.json();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
}

#[tokio::test]
async fn activate_rollback() {
    let h = harness().await;
    let mock = h.create_mock("orders").await;
    h.upload("orders", &contract_json("1.0"), None).await;
    h.upload("orders", &contract_json("2.0"), Some(true)).await;
    assert_eq!(h.active_version("orders").await, 2);

    let r = h
        .post_empty_json("/api/v1/mocks/orders/contracts/1/activate")
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let body = r.json();
    assert_eq!(body["version"], 1);
    assert_eq!(body["status"], "active");
    assert_eq!(h.active_version("orders").await, 1);

    let list = h.get("/api/v1/mocks/orders/contracts").await.json();
    assert_eq!(list[0]["version"], 2);
    assert_eq!(list[0]["status"], "approved");
    assert_eq!(list[1]["status"], "active");

    // A draft cannot be activated.
    let mock_id = mock["id"].as_str().unwrap().parse().unwrap();
    let bytes = contract_json("3.0").into_bytes();
    let bundle = mb_import::compile(&bytes).unwrap();
    let draft = h
        .store
        .add_contract_version(
            mock_id,
            NewContractVersion {
                source: bytes,
                bundle,
                status: ContractStatus::Draft,
                created_by: None,
            },
        )
        .await
        .unwrap();
    h.post_empty_json(&format!(
        "/api/v1/mocks/orders/contracts/{}/activate",
        draft.version
    ))
    .await
    .assert_problem(StatusCode::CONFLICT);
    assert_eq!(h.active_version("orders").await, 1);

    h.post_empty_json("/api/v1/mocks/orders/contracts/99/activate")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn scenarios_crud() {
    let h = harness().await;
    h.create_mock("orders").await;

    let r = h.get("/api/v1/mocks/orders/scenarios").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json(), json!([]));

    let r = h
        .json(
            Method::PUT,
            "/api/v1/mocks/orders/scenarios/slow",
            json!({
                "instructions": "respond slowly",
                "latency_ms": 500,
                "set": { "status": "pending" },
                "operations": ["listOrders"]
            }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(
        r.json(),
        json!({
            "name": "slow",
            "instructions": "respond slowly",
            "status": null,
            "latency_ms": 500,
            "set": { "status": "pending" },
            "operations": ["listOrders"],
            "source": "api"
        })
    );

    // PUT replaces the whole scenario.
    let r = h
        .json(
            Method::PUT,
            "/api/v1/mocks/orders/scenarios/slow",
            json!({ "status": 503 }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let list = h.get("/api/v1/mocks/orders/scenarios").await.json();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["name"], "slow");
    assert_eq!(list[0]["status"], 503);
    assert_eq!(list[0]["latency_ms"], Value::Null);
    assert_eq!(list[0]["operations"], json!([]));

    h.json(
        Method::PUT,
        "/api/v1/mocks/orders/scenarios/bad",
        json!({ "status": 42 }),
    )
    .await
    .assert_problem(StatusCode::UNPROCESSABLE_ENTITY);
    h.json(
        Method::PUT,
        "/api/v1/mocks/orders/scenarios/bad",
        json!({ "colour": "red" }),
    )
    .await
    .assert_problem(StatusCode::UNPROCESSABLE_ENTITY);
    h.json(Method::PUT, "/api/v1/mocks/nope/scenarios/slow", json!({}))
        .await
        .assert_problem(StatusCode::NOT_FOUND);

    let r = h.delete("/api/v1/mocks/orders/scenarios/slow").await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
    h.delete("/api/v1/mocks/orders/scenarios/slow")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
    assert_eq!(
        h.get("/api/v1/mocks/orders/scenarios").await.json(),
        json!([])
    );
}

/// The stream settings of a scenario are stored and listed; absent ones are
/// omitted from the view.
#[tokio::test]
async fn scenario_stream_fields_round_trip() {
    let h = harness().await;
    h.create_mock("orders").await;

    let r = h
        .json(
            Method::PUT,
            "/api/v1/mocks/orders/scenarios/trickle",
            json!({ "stream_items": 3, "stream_item_interval_ms": 250 }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json()["stream_items"], 3);
    assert_eq!(r.json()["stream_item_interval_ms"], 250);
    let list = h.get("/api/v1/mocks/orders/scenarios").await.json();
    assert_eq!(list[0]["stream_items"], 3);
    assert_eq!(list[0]["stream_item_interval_ms"], 250);
    let m = h
        .store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    let stored = h.store.list_scenarios(m.id).await.unwrap();
    assert_eq!(stored[0].rules.stream_items, Some(3));
    assert_eq!(stored[0].rules.stream_item_interval_ms, Some(250));

    // PUT replaces the whole scenario: absent fields are cleared and omitted.
    let r = h
        .json(
            Method::PUT,
            "/api/v1/mocks/orders/scenarios/trickle",
            json!({ "status": 503 }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let view = r.json();
    assert!(view.get("stream_items").is_none(), "{view}");
    assert!(view.get("stream_item_interval_ms").is_none(), "{view}");

    h.json(
        Method::PUT,
        "/api/v1/mocks/orders/scenarios/trickle",
        json!({ "stream_items": 0 }),
    )
    .await
    .assert_problem(StatusCode::UNPROCESSABLE_ENTITY);
}

/// With an active contract, a scenario's operations must exist in it; with
/// none, any operation ids are accepted.
#[tokio::test]
async fn scenario_operations_are_checked_against_the_contract() {
    let h = harness().await;
    h.create_mock("orders").await;
    let put = |ops: Value| {
        let h = &h;
        async move {
            h.json(
                Method::PUT,
                "/api/v1/mocks/orders/scenarios/s",
                json!({ "operations": ops }),
            )
            .await
        }
    };

    // No contract yet: accepted.
    let r = put(json!(["anything"])).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);

    let r = h
        .upload("orders", &contract_json("1.0.0"), Some(true))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);

    let r = put(json!(["listOrders"])).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);

    let body = put(json!(["zzz", "listOrders", "aaa"]))
        .await
        .assert_problem(StatusCode::UNPROCESSABLE_ENTITY);
    let detail = body["detail"].as_str().unwrap();
    assert!(detail.contains("\"aaa\", \"zzz\""), "{detail}");
    assert!(!detail.contains("listOrders"), "{detail}");
}

#[tokio::test]
async fn profiles_never_return_secret_values() {
    // The API never resolves a secret_ref, so no variable needs setting.
    let h = harness().await;

    let r = h
        .json(
            Method::PUT,
            "/api/v1/llm-profiles/fast",
            json!({
                "base_url": "https://llm.example.com",
                "model": "fast-1",
                "secret_ref": "env:MB_LLM_ADMIN_TEST_PROFILE_KEY"
            }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(!r.text.contains("sk-123"), "{}", r.text);
    let body = r.json();
    assert_eq!(body["name"], "fast");
    assert_eq!(body["source"], "api");
    assert_eq!(body["temperature"], 0.7);
    assert_eq!(body["timeout_ms"], 3000);
    assert_eq!(body["max_tokens"], 2048);
    assert_eq!(body["json_schema"], false);

    let r = h.get("/api/v1/llm-profiles/fast").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(
        r.text
            .contains(r#""secret_ref":"env:MB_LLM_ADMIN_TEST_PROFILE_KEY""#),
        "{}",
        r.text
    );
    assert!(!r.text.contains("sk-123"), "{}", r.text);

    let r = h.get("/api/v1/llm-profiles").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json().as_array().unwrap().len(), 1);
    assert!(!r.text.contains("sk-123"), "{}", r.text);

    let r = h
        .json(
            Method::PUT,
            "/api/v1/llm-profiles/fast",
            json!({ "base_url": "https://x", "model": "m", "api_key": "sk-123" }),
        )
        .await;
    let problem = r.assert_problem(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("use secret_ref"),
        "{problem}"
    );
    assert!(!r.text.contains("sk-123"), "{}", r.text);

    for bad in ["sk-123", "vault:x", "env:", "file:"] {
        let r = h
            .json(
                Method::PUT,
                "/api/v1/llm-profiles/fast",
                json!({ "base_url": "https://x", "model": "m", "secret_ref": bad }),
            )
            .await;
        r.assert_problem(StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            !r.text.contains("sk-123"),
            "a rejected secret_ref is echoed: {}",
            r.text
        );
    }

    // Absent is fine.
    let r = h
        .json(
            Method::PUT,
            "/api/v1/llm-profiles/local",
            json!({ "base_url": "http://localhost:11434", "model": "llama" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json()["secret_ref"], Value::Null);

    let r = h.delete("/api/v1/llm-profiles/fast").await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    h.get("/api/v1/llm-profiles/fast")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
    h.delete("/api/v1/llm-profiles/fast")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_secret_refs_are_restricted() {
    let h = harness().await;
    let put = |secret_ref: &'static str| json!({ "base_url": "https://x", "model": "m", "secret_ref": secret_ref });
    // The defaults: env:MB_LLM_* and env:*_API_KEY.
    for ok in [
        "env:MB_LLM_API_KEY",
        "env:MB_LLM_OTHER",
        "env:OPENAI_API_KEY",
    ] {
        let r = h.json(Method::PUT, "/api/v1/llm-profiles/p", put(ok)).await;
        assert_eq!(r.status, StatusCode::OK, "{ok}: {}", r.text);
    }
    for bad in [
        "env:HOME",
        "env:AWS_SECRET_ACCESS_KEY",
        "file:/etc/passwd",
        "file:/run/secrets/llm",
    ] {
        let r = h
            .json(Method::PUT, "/api/v1/llm-profiles/p", put(bad))
            .await;
        let p = r.assert_problem(StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            p["detail"]
                .as_str()
                .unwrap()
                .contains("secret_ref not allowed by server.api_secret_refs"),
            "{bad}: {p}"
        );
        assert!(
            !r.text.contains(bad),
            "the rejected ref is echoed: {}",
            r.text
        );
    }
    // The profile kept its last accepted ref.
    assert_eq!(
        h.get("/api/v1/llm-profiles/p").await.json()["secret_ref"],
        "env:OPENAI_API_KEY"
    );

    // A configured allowlist replaces the defaults.
    let h = harness_with(vec!["file:/run/secrets/*".into()]).await;
    let r = h
        .json(
            Method::PUT,
            "/api/v1/llm-profiles/p",
            put("file:/run/secrets/llm"),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    h.json(
        Method::PUT,
        "/api/v1/llm-profiles/p",
        put("env:MB_LLM_API_KEY"),
    )
    .await
    .assert_problem(StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn mutations_require_non_simple_content_types() {
    let h = harness().await;
    h.create_mock("orders").await;
    h.upload("orders", &contract_json("1.0"), None).await;

    // JSON endpoints: a form, text or missing content type is 415.
    let json_endpoints: [(Method, &str, &str); 6] = [
        (Method::POST, "/api/v1/mocks", r#"{"name":"x"}"#),
        (Method::PATCH, "/api/v1/mocks/orders", r#"{"persona":"p"}"#),
        (
            Method::PUT,
            "/api/v1/mocks/orders/scenarios/s",
            r#"{"status":500}"#,
        ),
        (
            Method::PUT,
            "/api/v1/llm-profiles/p",
            r#"{"base_url":"http://x","model":"m"}"#,
        ),
        (Method::PUT, "/api/v1/mocks/orders/sessions/s", "[]"),
        (
            Method::POST,
            "/api/v1/mocks/orders/contracts/1/activate",
            "",
        ),
    ];
    for (method, uri, body) in json_endpoints {
        for ct in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
            Some("multipart/form-data; boundary=x"),
        ] {
            h.send(method.clone(), uri, Body::from(body), ct)
                .await
                .assert_problem(StatusCode::UNSUPPORTED_MEDIA_TYPE);
        }
    }
    let r = h
        .post_empty_json("/api/v1/mocks/orders/contracts/1/activate")
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);

    // Contract upload: contract media types only, none CORS-safelisted.
    let uri = "/api/v1/mocks/orders/contracts";
    for ct in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data; boundary=x"),
    ] {
        h.send(Method::POST, uri, Body::from(contract_json("2.0")), ct)
            .await
            .assert_problem(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    for ct in [
        "application/json",
        "application/json; charset=utf-8",
        "application/yaml",
        "application/x-yaml",
        "text/yaml",
        "application/vnd.oai.openapi",
        "application/vnd.oai.openapi+json",
        "Application/VND.OAI.OpenAPI+YAML; version=3.0",
    ] {
        let r = h
            .send(
                Method::POST,
                uri,
                Body::from(contract_json("2.0")),
                Some(ct),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{ct}: {}", r.text);
    }
}

/// Deleting a mock drops its session state: a new mock with the same name
/// starts with none.
#[tokio::test]
async fn deleting_a_mock_resets_its_state() {
    let h = harness().await;
    h.create_mock("shop").await;
    let fixtures = json!([{ "resource_type": "orders", "id": "1", "data": { "id": "1" } }]);
    for session in ["alice", "bob"] {
        let r = h
            .json(
                Method::PUT,
                &format!("/api/v1/mocks/shop/sessions/{session}"),
                fixtures.clone(),
            )
            .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
    }
    assert_eq!(
        h.get("/api/v1/mocks/shop/sessions")
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let r = h.delete("/api/v1/mocks/shop").await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
    h.create_mock("shop").await;
    assert_eq!(h.get("/api/v1/mocks/shop/sessions").await.json(), json!([]));
    assert_eq!(
        h.get("/api/v1/mocks/shop/sessions/alice").await.json(),
        json!([])
    );
}

/// A memory state store whose `reset_mock` fails.
struct ResetFails(MemoryStore);

#[async_trait::async_trait]
impl StateStore for ResetFails {
    async fn get(
        &self,
        s: &SessionKey,
        t: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        self.0.get(s, t, id).await
    }
    async fn list(
        &self,
        s: &SessionKey,
        t: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        self.0.list(s, t, limit).await
    }
    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        self.0.upsert(s, r).await
    }
    async fn delete(&self, s: &SessionKey, t: &str, id: &str) -> Result<bool, StateError> {
        self.0.delete(s, t, id).await
    }
    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        self.0.reset(s).await
    }
    async fn reset_mock(&self, _: &str, _: &str) -> Result<(), StateError> {
        Err(StateError::Backend("state backend down".into()))
    }
    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.0.touch(s).await
    }
    async fn sessions(&self, w: &str, m: &str) -> Result<Vec<SessionInfo>, StateError> {
        self.0.sessions(w, m).await
    }
    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        self.0.export(s).await
    }
    async fn seed(&self, s: &SessionKey, r: Vec<StoredResource>) -> Result<(), StateError> {
        self.0.seed(s, r).await
    }
    async fn ping(&self) -> Result<(), StateError> {
        self.0.ping().await
    }
    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.0.subscribe()
    }
}

/// A failed state reset leaves the mock in place, so the DELETE can be
/// retried.
#[tokio::test]
async fn failed_state_reset_keeps_the_mock() {
    let state: Arc<dyn StateStore> =
        Arc::new(ResetFails(MemoryStore::new(Duration::from_secs(3600))));
    let refs = DEFAULT_API_SECRET_REFS
        .iter()
        .map(|r| r.to_string())
        .collect();
    let h = harness_full(refs, state).await;
    h.create_mock("shop").await;

    h.delete("/api/v1/mocks/shop")
        .await
        .assert_problem(StatusCode::SERVICE_UNAVAILABLE);
    let r = h.get("/api/v1/mocks/shop").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
}

#[tokio::test]
async fn sessions_export_seed_reset() {
    let h = harness().await;
    h.create_mock("orders").await;

    let r = h.get("/api/v1/mocks/orders/sessions").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json(), json!([]));

    let fixtures = json!([
        { "resource_type": "orders", "id": "1", "data": { "id": "1", "total": 3 } },
        { "resource_type": "orders", "id": "2", "data": { "id": "2", "total": 5 } }
    ]);
    let r = h
        .json(
            Method::PUT,
            "/api/v1/mocks/orders/sessions/alice",
            fixtures.clone(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);

    let r = h.get("/api/v1/mocks/orders/sessions").await;
    let list = r.json();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["session"], "alice");
    assert_eq!(list[0]["resources"], 2);
    assert!(list[0]["last_touched"].is_string(), "{list}");

    let r = h.get("/api/v1/mocks/orders/sessions/alice").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json(), fixtures);

    let r = h.delete("/api/v1/mocks/orders/sessions/alice").await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
    assert_eq!(
        h.get("/api/v1/mocks/orders/sessions").await.json(),
        json!([])
    );
    assert_eq!(
        h.get("/api/v1/mocks/orders/sessions/alice").await.json(),
        json!([])
    );

    h.get("/api/v1/mocks/orders/sessions/bad%20id")
        .await
        .assert_problem(StatusCode::BAD_REQUEST);
    h.get("/api/v1/mocks/nope/sessions")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
    h.json(Method::PUT, "/api/v1/mocks/nope/sessions/alice", json!([]))
        .await
        .assert_problem(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn request_log_query() {
    let h = harness().await;
    let mock = h.create_mock("orders").await;
    let mock_id = mock["id"].as_str().unwrap().parse().unwrap();

    let mut ids = Vec::new();
    for (op, session, source) in [
        ("listOrders", "a", "llm"),
        ("createOrder", "a", "fallback"),
        ("listOrders", "b", "fallback"),
        ("getOrder", "b", "llm"),
    ] {
        let id = h
            .store
            .append_request_log(
                NewRequestLogEntry {
                    mock_id,
                    operation_id: Some(op.into()),
                    session: session.into(),
                    method: "GET".into(),
                    path: "/orders".into(),
                    status: 200,
                    source: Some(source.into()),
                    latency_ms: 12,
                    prompt_tokens: 3,
                    completion_tokens: 4,
                    request: json!({ "query": {} }),
                    response: Some(json!({ "ok": true })),
                    at: Utc::now(),
                    kind: RequestKind::Http,
                },
                1000,
            )
            .await
            .unwrap();
        ids.push(id);
    }

    let ops = |v: Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|e| e["operation_id"].as_str().unwrap().to_owned())
            .collect()
    };

    let r = h.get("/api/v1/mocks/orders/requests").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let all = r.json();
    assert_eq!(
        ops(all.clone()),
        ["getOrder", "listOrders", "createOrder", "listOrders"]
    );
    assert_eq!(all[0]["session"], "b");
    assert_eq!(all[0]["source"], "llm");
    assert_eq!(all[0]["status"], 200);
    assert_eq!(all[0]["response"], json!({ "ok": true }));

    let q = |s: &str| format!("/api/v1/mocks/orders/requests?{s}");
    assert_eq!(
        ops(h.get(&q("operation=listOrders")).await.json()),
        ["listOrders", "listOrders"]
    );
    assert_eq!(
        ops(h.get(&q("session=a")).await.json()),
        ["createOrder", "listOrders"]
    );
    assert_eq!(
        ops(h.get(&q("source=fallback")).await.json()),
        ["listOrders", "createOrder"]
    );
    assert_eq!(ops(h.get(&q("limit=1")).await.json()), ["getOrder"]);
    assert_eq!(
        ops(h.get(&q(&format!("before={}", ids[2]))).await.json()),
        ["createOrder", "listOrders"]
    );
    assert_eq!(
        ops(h.get(&q("session=b&source=fallback")).await.json()),
        ["listOrders"]
    );

    h.get(&q("limit=lots"))
        .await
        .assert_problem(StatusCode::BAD_REQUEST);
    h.get("/api/v1/mocks/nope/requests")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn request_log_kind_query() {
    let h = harness().await;
    let mock = h.create_mock("orders").await;
    let mock_id = mock["id"].as_str().unwrap().parse().unwrap();
    for (op, method, kind) in [
        ("listOrders", "GET", RequestKind::Http),
        ("orderPlaced", "PUBLISH", RequestKind::Event),
    ] {
        h.store
            .append_request_log(
                NewRequestLogEntry {
                    mock_id,
                    operation_id: Some(op.into()),
                    session: "default".into(),
                    method: method.into(),
                    path: "orders.placed".into(),
                    status: 200,
                    source: Some("llm".into()),
                    latency_ms: 1,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    request: json!({}),
                    response: None,
                    at: Utc::now(),
                    kind,
                },
                1000,
            )
            .await
            .unwrap();
    }

    let r = h.get("/api/v1/mocks/orders/requests").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let kinds: Vec<Value> = r
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].clone())
        .collect();
    assert_eq!(kinds, [json!("event"), json!("http")]);

    let r = h.get("/api/v1/mocks/orders/requests?kind=event").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let events = r.json();
    assert_eq!(events.as_array().unwrap().len(), 1, "{events}");
    assert_eq!(events[0]["operation_id"], "orderPlaced");
    assert_eq!(events[0]["kind"], "event");

    let r = h.get("/api/v1/mocks/orders/requests?kind=http").await;
    assert_eq!(r.json()[0]["operation_id"], "listOrders");

    let body = h
        .get("/api/v1/mocks/orders/requests?kind=bogus")
        .await
        .assert_problem(StatusCode::BAD_REQUEST);
    assert_eq!(body["detail"], "unknown request kind \"bogus\"", "{body}");
}

#[tokio::test]
async fn file_managed_mock_is_read_only() {
    let h = harness().await;
    let mock = h
        .store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: "filed".into(),
                route_prefix: None,
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::File,
            },
        )
        .await
        .unwrap();
    let bytes = contract_json("1.0").into_bytes();
    let bundle = mb_import::compile(&bytes).unwrap();
    h.store
        .add_contract_version(
            mock.id,
            NewContractVersion {
                source: bytes,
                bundle,
                status: ContractStatus::Approved,
                created_by: None,
            },
        )
        .await
        .unwrap();
    h.store.activate_contract_version(mock.id, 1).await.unwrap();
    h.store
        .put_scenario(&Scenario {
            mock_id: mock.id,
            name: "outage".into(),
            instructions: None,
            rules: ScenarioRules {
                status: Some(503),
                ..ScenarioRules::default()
            },
            source: ResourceSource::File,
        })
        .await
        .unwrap();
    h.store
        .put_profile(&LlmProfile {
            workspace: DEFAULT_WORKSPACE.into(),
            name: "filed".into(),
            base_url: "https://x".into(),
            model: "m".into(),
            secret_ref: None,
            json_schema: false,
            temperature: 0.7,
            timeout_ms: 3000,
            max_tokens: 2048,
            fallback_profile: None,
            source: ResourceSource::File,
        })
        .await
        .unwrap();

    let read_only = |r: Resp| {
        let p = r.assert_problem(StatusCode::CONFLICT);
        assert_eq!(p["title"], "Managed by config file", "{p}");
    };
    read_only(
        h.json(
            Method::PATCH,
            "/api/v1/mocks/filed",
            json!({ "persona": "x" }),
        )
        .await,
    );
    read_only(h.delete("/api/v1/mocks/filed").await);
    read_only(h.upload("filed", &contract_json("2.0"), Some(true)).await);
    read_only(
        h.post_empty_json("/api/v1/mocks/filed/contracts/1/activate")
            .await,
    );
    read_only(
        h.json(
            Method::PUT,
            "/api/v1/mocks/filed/scenarios/outage",
            json!({ "status": 500 }),
        )
        .await,
    );
    read_only(h.delete("/api/v1/mocks/filed/scenarios/outage").await);
    read_only(
        h.json(
            Method::PUT,
            "/api/v1/llm-profiles/filed",
            json!({ "base_url": "https://y", "model": "m" }),
        )
        .await,
    );
    read_only(h.delete("/api/v1/llm-profiles/filed").await);

    // Nothing changed, and reads still work.
    let r = h.get("/api/v1/mocks/filed").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["source"], "file");
    assert_eq!(r.json()["active_version"], 1);
    let list = h.get("/api/v1/mocks/filed/contracts").await.json();
    assert_eq!(list.as_array().unwrap().len(), 1);
    let scenarios = h.get("/api/v1/mocks/filed/scenarios").await.json();
    assert_eq!(scenarios[0]["status"], 503);
    assert_eq!(scenarios[0]["source"], "file");
    let profile = h.get("/api/v1/llm-profiles/filed").await.json();
    assert_eq!(profile["base_url"], "https://x");
}

#[tokio::test]
async fn openapi_lists_all_paths() {
    let h = harness().await;
    let r = h.get("/api/openapi.json").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let doc = r.json();
    assert!(doc["openapi"].as_str().unwrap().starts_with("3."), "{doc}");

    let expected: &[(&str, &[&str])] = &[
        ("/api/v1/mocks", &["get", "post"]),
        ("/api/v1/mocks/{mock}", &["get", "patch", "delete"]),
        ("/api/v1/mocks/{mock}/contracts", &["get", "post"]),
        ("/api/v1/mocks/{mock}/contracts/{version}", &["get"]),
        ("/api/v1/mocks/{mock}/contracts/{version}/source", &["get"]),
        (
            "/api/v1/mocks/{mock}/contracts/{version}/activate",
            &["post"],
        ),
        ("/api/v1/mocks/{mock}/scenarios", &["get"]),
        ("/api/v1/mocks/{mock}/scenarios/{name}", &["put", "delete"]),
        ("/api/v1/llm-profiles", &["get"]),
        ("/api/v1/llm-profiles/{name}", &["get", "put", "delete"]),
        ("/api/v1/mocks/{mock}/sessions", &["get"]),
        (
            "/api/v1/mocks/{mock}/sessions/{session}",
            &["get", "put", "delete"],
        ),
        ("/api/v1/mocks/{mock}/requests", &["get"]),
    ];
    for (path, methods) in expected {
        let item = &doc["paths"][*path];
        assert!(item.is_object(), "{path} missing from the OpenAPI document");
        for m in *methods {
            assert!(item[*m].is_object(), "{m} {path} missing");
        }
    }
    let documented: usize = doc["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|item| item.as_object().unwrap().len())
        .sum();
    let listed: usize = expected.iter().map(|(_, m)| m.len()).sum();
    assert_eq!(documented, listed, "unexpected extra operations");
    let schemas = doc["components"]["schemas"].as_object().unwrap();
    for name in ["Problem", "MockView", "LlmProfileView", "ScenarioView"] {
        assert!(schemas.contains_key(name), "schema {name} missing");
    }
}

#[tokio::test]
async fn errors_are_problem_json() {
    let h = harness().await;

    let p = h
        .get("/api/v1/mocks/nope")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
    assert!(p["detail"].as_str().unwrap().contains("nope"), "{p}");

    // Unknown route and wrong method.
    h.get("/api/v1/nothing-here")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
    h.raw(Method::PUT, "/api/v1/mocks", "")
        .await
        .assert_problem(StatusCode::METHOD_NOT_ALLOWED);

    // Malformed JSON, missing content type, wrong field type.
    h.send(
        Method::POST,
        "/api/v1/mocks",
        Body::from("{not json"),
        Some("application/json"),
    )
    .await
    .assert_problem(StatusCode::BAD_REQUEST);
    h.raw(Method::POST, "/api/v1/mocks", r#"{"name":"x"}"#)
        .await
        .assert_problem(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    h.json(Method::POST, "/api/v1/mocks", json!({ "name": 5 }))
        .await
        .assert_problem(StatusCode::UNPROCESSABLE_ENTITY);

    // Bad path and query parameters.
    h.create_mock("orders").await;
    h.get("/api/v1/mocks/orders/contracts/one")
        .await
        .assert_problem(StatusCode::BAD_REQUEST);
    h.get("/api/v1/mocks/orders/requests?before=x")
        .await
        .assert_problem(StatusCode::BAD_REQUEST);
    h.upload("orders", &contract_json("1.0"), None).await;
    h.raw(
        Method::POST,
        "/api/v1/mocks/orders/contracts?activate=maybe",
        &contract_json("2.0"),
    )
    .await
    .assert_problem(StatusCode::BAD_REQUEST);

    // An oversized contract upload.
    let huge = "x".repeat(20 * 1024 * 1024 + 1);
    h.send(
        Method::POST,
        "/api/v1/mocks/orders/contracts",
        Body::from(huge),
        Some("application/yaml"),
    )
    .await
    .assert_problem(StatusCode::PAYLOAD_TOO_LARGE);
}

fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../fixtures/{path}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn fixture_dir(path: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(path)
}

/// A zip of `(name, bytes)` entries, names stored verbatim.
fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        w.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(bytes).unwrap();
    }
    w.finish().unwrap().into_inner()
}

#[tokio::test]
async fn upload_graphql_proto_wsdl_and_archive() {
    let h = harness().await;
    let multi = mb_import::pack_dir(&fixture_dir("proto/multi")).unwrap();
    let multi_zip = zip_of(&[
        (
            "multi/common/v1/money.proto",
            &fixture("proto/multi/common/v1/money.proto"),
        ),
        (
            "multi/orders/v1/orders.proto",
            &fixture("proto/multi/orders/v1/orders.proto"),
        ),
    ]);
    let cases: Vec<(&str, Vec<u8>, &str, &str)> = vec![
        (
            "sw",
            fixture("graphql/starwars.graphql"),
            "application/graphql",
            "text/plain; charset=utf-8",
        ),
        (
            "rg",
            fixture("proto/route_guide.proto"),
            "text/x-protobuf",
            "text/plain; charset=utf-8",
        ),
        (
            "calc",
            fixture("wsdl/calculator.wsdl"),
            "application/wsdl+xml",
            "application/xml",
        ),
        (
            "calc-xml",
            fixture("wsdl/calculator.wsdl"),
            "text/xml",
            "application/xml",
        ),
        ("multi", multi, "application/gzip", "application/gzip"),
        ("multi-zip", multi_zip, "application/zip", "application/zip"),
    ];
    for (mock, bytes, content_type, source_type) in cases {
        h.create_mock(mock).await;
        let expected = mb_import::compile(&bytes).unwrap().operations.len();
        assert!(expected > 0, "{mock}");
        let r = h.upload_bytes(mock, bytes.clone(), content_type).await;
        assert_eq!(r.status, StatusCode::CREATED, "{mock}: {}", r.text);
        assert_eq!(
            r.json(),
            json!({ "version": 1, "status": "active", "operations": expected, "channels": 0 }),
            "{mock}"
        );
        let r = h
            .get(&format!("/api/v1/mocks/{mock}/contracts/1/source"))
            .await;
        assert_eq!(r.status, StatusCode::OK, "{mock}");
        assert_eq!(r.content_type, source_type, "{mock}");
    }

    // A descriptor set uploads as application/x-protobuf; text/plain stays
    // refused (it is CORS-safelisted).
    h.create_mock("plain").await;
    h.upload_bytes("plain", fixture("proto/route_guide.proto"), "text/plain")
        .await
        .assert_problem(StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn upload_zip_slip_422() {
    let h = harness().await;
    h.create_mock("evil").await;
    let zip = zip_of(&[("../evil.proto", b"syntax = \"proto3\";")]);
    let problem = h
        .upload_bytes("evil", zip, "application/zip")
        .await
        .assert_problem(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("../evil.proto"),
        "{problem}"
    );
    let list = h.get("/api/v1/mocks/evil/contracts").await.json();
    assert_eq!(list, json!([]));
}

#[tokio::test]
async fn upload_over_20_mib_413() {
    let h = harness().await;
    h.create_mock("big").await;
    let mut body = b"PK\x03\x04".to_vec();
    body.resize(20 * 1024 * 1024 + 1, 0);
    h.upload_bytes("big", body, "application/zip")
        .await
        .assert_problem(StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn grpc_contract_detail_lists_protocol_target_kind() {
    let h = harness().await;
    h.create_mock("rg").await;
    let r = h
        .upload_bytes("rg", fixture("proto/route_guide.proto"), "text/x-protobuf")
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let detail = h.get("/api/v1/mocks/rg/contracts/1").await.json();
    let ops = detail["operations"].as_array().unwrap();
    let list = ops
        .iter()
        .find(|o| o["target"] == "/routeguide.RouteGuide/ListFeatures")
        .unwrap_or_else(|| panic!("{detail}"));
    assert_eq!(list["protocol"], "grpc", "{list}");
    assert_eq!(list["kind"], "stream-server", "{list}");
    assert_eq!(list["method"], "", "{list}");
    assert_eq!(list["path"], "", "{list}");
}

#[tokio::test]
async fn upload_asyncapi_reports_channels() {
    let h = harness().await;
    h.create_mock("lights").await;
    let r = h
        .upload_bytes(
            "lights",
            fixture("asyncapi/streetlights-kafka-3.0.yaml"),
            "application/vnd.aai.asyncapi+yaml",
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(
        r.json(),
        json!({ "version": 1, "status": "active", "operations": 0, "channels": 4 })
    );

    let r = h.get("/api/v1/mocks/lights/contracts").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json()[0]["channels"], 4);

    let r = h.get("/api/v1/mocks/lights/contracts/1").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let detail = r.json();
    assert_eq!(detail["operations"], json!([]));
    let ids: Vec<&str> = detail["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["dimLight", "receiveLightMeasurement", "turnOff", "turnOn"]
    );
    assert_eq!(
        detail["channels"][3],
        json!({
            "id": "turnOn",
            "direction": "publish",
            "protocol": "kafka",
            "address": "smartylighting.streetlights.1.0.action.{streetlightId}.turn.on",
            "messages": ["turnOnOff"],
        })
    );
    assert_eq!(detail["channels"][1]["direction"], "subscribe");

    for media in [
        "application/vnd.aai.asyncapi",
        "application/vnd.aai.asyncapi+json",
    ] {
        h.create_mock(&format!("m-{}", media.len())).await;
        let r = h
            .upload_bytes(
                &format!("m-{}", media.len()),
                fixture("asyncapi/sensors-mqtt-3.0.yaml"),
                media,
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{media}: {}", r.text);
        assert_eq!(r.json()["channels"], 3, "{media}");
    }
}
