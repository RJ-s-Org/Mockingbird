//! Reads and writes that the contract's structure alone decides: an owner named by a
//! compound path parameter, an answer's identity the model got right, a declared
//! status standing for "not here", and a declared projection on a stored read. Each
//! over a small synthetic contract, with a scripted LLM.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
}

fn harness(contract: &str, script: Vec<Value>) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        store as Arc<dyn StateStore>,
    );
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
    }
}

impl Harness {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let source = resp
            .headers()
            .get("x-mockingbird-source")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, source, body)
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }
}

/// Boards nest under a workspace that the path names by its id or its slug, through
/// one compound parameter.
const BOARDS: &str = r##"
openapi: 3.0.0
info: {title: Boards, version: "1"}
paths:
  /api/workspaces/{workspace_id_or_slug}/boards/:
    post:
      operationId: createBoard
      parameters: [{$ref: "#/components/parameters/workspace"}]
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [name]
              properties:
                name: {type: string}
                slug: {type: string}
      responses:
        "201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}
  /api/boards/{workspace_id_or_slug}/{board_id_or_slug}/:
    get:
      operationId: getBoard
      parameters:
        - {$ref: "#/components/parameters/workspace"}
        - {name: board_id_or_slug, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}
        "404": {description: none, content: {application/json: {schema: {type: object, properties: {detail: {type: string}}}}}}
components:
  parameters:
    workspace: {name: workspace_id_or_slug, in: path, required: true, schema: {type: string}}
  schemas:
    WorkspaceSummary:
      type: object
      properties:
        id: {type: string}
        slug: {type: string}
    Board:
      type: object
      required: [id, slug, name]
      properties:
        id: {type: string}
        slug: {type: string}
        name: {type: string}
        workspace: {$ref: "#/components/schemas/WorkspaceSummary"}
"##;

/// A board created under a workspace, read back by both slugs: the stored board's
/// `workspace` shows it is that workspace's, by the compound parameter's stem.
#[tokio::test]
async fn a_compound_owner_parameter_ties_the_record_s_owner_member() {
    let board = json!({"id": "4501", "slug": "roadmap", "name": "Roadmap",
                       "workspace": {"id": "77", "slug": "acme"}});
    let h = harness(
        BOARDS,
        vec![
            board.clone(),
            json!({"id": "9999", "slug": "roadmap", "name": "Made up"}),
        ],
    );
    let (status, _, made) = h
        .call(
            "POST",
            "/api/workspaces/acme/boards/",
            Some(json!({"name": "Roadmap", "slug": "roadmap"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, source, read) = h.call("GET", "/api/boards/acme/roadmap/", None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["id"], "4501", "{read}");
    assert_eq!(h.calls(), 1);
}

/// Seats booked on a showing: the booking's answer holds the showing's `id` beside
/// its own key, `seat_id`, as the contract describes.
const SEATS: &str = r##"
openapi: 3.0.0
info: {title: Seats, version: "1"}
paths:
  /showings:
    post:
      operationId: createShowing
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Showing"}}}}
  /showings/{showingId}:
    get:
      operationId: getShowing
      parameters: [{name: showingId, in: path, required: true, schema: {type: integer, format: int64}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Showing"}}}}
  /showings/{showingId}/seats:
    post:
      operationId: bookSeat
      parameters: [{name: showingId, in: path, required: true, schema: {type: integer, format: int64}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, required: [email], properties: {email: {type: string}}}}}
      responses:
        "201": {description: booked, content: {application/json: {schema: {$ref: "#/components/schemas/SeatBooked"}}}}
components:
  schemas:
    Showing:
      type: object
      required: [id, title]
      properties:
        id: {type: integer, format: int64}
        title: {type: string}
    SeatBooked:
      type: object
      required: [id, seat_id, title]
      properties:
        id: {type: integer, format: int64, description: The showing's id.}
        seat_id: {type: string}
        title: {type: string, description: The showing's title.}
"##;

/// The booking answers the showing's id under `id`, as its schema says, beside its
/// own `seat_id`: that `id` is no taken identity, and the client gets it as answered.
#[tokio::test]
async fn an_answer_keyed_apart_keeps_the_id_it_names() {
    let h = harness(
        SEATS,
        vec![
            json!({"id": 78_245_196_304_u64, "title": "Matinee"}),
            json!({"id": 78_245_196_304_u64, "seat_id": "st-a81f", "title": "Matinee"}),
        ],
    );
    let (status, _, showing) = h
        .call("POST", "/showings", Some(json!({"title": "Matinee"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{showing}");
    let (status, _, booked) = h
        .call(
            "POST",
            "/showings/78245196304/seats",
            Some(json!({"email": "ann@example.com"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{booked}");
    assert_eq!(booked["id"], 78_245_196_304_u64, "{booked}");
    assert_eq!(booked["seat_id"], "st-a81f", "{booked}");
}

/// Products whose read declares no `404`: a missing item answers the declared
/// `403`, whose description says so. `fields` lists the members to answer.
const PRODUCTS: &str = r##"
openapi: 3.0.0
info: {title: Catalog, version: "1"}
paths:
  /items/products:
    post:
      operationId: createProduct
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/ProductInput"}}}
      responses:
        "200": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Envelope"}}}}
  /items/products/{id}:
    get:
      operationId: readProduct
      parameters:
        - {name: id, in: path, required: true, schema: {type: integer}}
        - {name: fields, in: query, description: "Comma-separated fields to answer, such as `id,name`.", schema: {type: string}}
        - {name: sort, in: query, description: "Such as `-price,name`.", schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Envelope"}}}}
        "400": {$ref: "#/components/responses/Error"}
        "403": {$ref: "#/components/responses/Forbidden"}
    delete:
      operationId: deleteProduct
      parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
      responses:
        "204": {description: gone}
components:
  responses:
    Error:
      description: "400 `INVALID_QUERY`."
      content: {application/json: {schema: {$ref: "#/components/schemas/Errors"}}}
    Forbidden:
      description: "403 `FORBIDDEN`: the item does not exist, or the token may not access it."
      content: {application/json: {schema: {$ref: "#/components/schemas/Errors"}}}
  schemas:
    Errors:
      type: object
      required: [errors]
      properties:
        errors:
          type: array
          items:
            type: object
            required: [message]
            properties:
              message: {type: string}
    ProductInput:
      type: object
      required: [name]
      properties:
        name: {type: string}
        price: {type: number}
        dimensions: {type: object, properties: {width: {type: number}, height: {type: number}}}
    Envelope:
      type: object
      required: [data]
      properties:
        data: {$ref: "#/components/schemas/Product"}
    Product:
      type: object
      properties:
        id: {type: integer}
        name: {type: string}
        price: {type: number}
        dimensions: {type: object, properties: {width: {type: number}, height: {type: number}}}
"##;

fn product() -> Value {
    json!({"data": {"id": 731, "name": "Desk Lamp", "price": 49.5,
                    "dimensions": {"width": 12, "height": 40}}})
}

/// A read of a deleted product answers the read's declared `403`, the one status it
/// declares for an item that does not exist, not an undeclared `404`.
#[tokio::test]
async fn a_deleted_record_answers_the_declared_status_for_a_missing_one() {
    let h = harness(PRODUCTS, vec![product(), json!({}), product()]);
    let (status, _, made) = h
        .call(
            "POST",
            "/items/products",
            Some(json!({"name": "Desk Lamp", "price": 49.5})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{made}");
    let (status, _, gone) = h.call("DELETE", "/items/products/731", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{gone}");
    let (status, source, read) = h.call("GET", "/items/products/731", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{read}");
    assert_eq!(source, "state", "{read}");
    assert!(read["errors"].is_array(), "{read}");
}

/// A stored read honours `fields`, which its description ties to the product's
/// members by an example, dotted tokens going through what the record holds; `sort`,
/// whose example is no member list, cuts nothing.
#[tokio::test]
async fn a_stored_read_answers_only_the_fields_asked_for() {
    let h = harness(PRODUCTS, vec![product()]);
    let (status, _, made) = h
        .call(
            "POST",
            "/items/products",
            Some(json!({"name": "Desk Lamp", "price": 49.5,
                        "dimensions": {"width": 12, "height": 40}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{made}");
    let (status, source, read) = h
        .call("GET", "/items/products/731?fields=id,name", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source, "state", "{read}");
    assert_eq!(read, json!({"data": {"id": 731, "name": "Desk Lamp"}}));
    let (_, source, read) = h
        .call(
            "GET",
            "/items/products/731?fields=name,dimensions.width",
            None,
        )
        .await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(
        read,
        json!({"data": {"name": "Desk Lamp", "dimensions": {"width": 12}}})
    );
    let (_, source, read) = h.call("GET", "/items/products/731?sort=name", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read, product());
    assert_eq!(h.calls(), 1);
}
