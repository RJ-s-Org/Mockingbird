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
        self.call_with(method, uri, body, &[]).await
    }

    /// [`Self::call`] of a create, seeded: a seeded create takes the generated path,
    /// which stores the record the model answered under the id it chose, as these
    /// tests expect.
    async fn seeded(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Value) {
        self.call_with(method, uri, body, &[crate::support::SEED])
            .await
    }

    async fn call_with(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, String, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
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
        .seeded(
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
  /showings/{showingId}/seats/{seat_id}:
    get:
      operationId: getSeat
      parameters:
        - {name: showingId, in: path, required: true, schema: {type: integer, format: int64}}
        - {name: seat_id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/SeatBooked"}}}}
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

/// Books a seat on showing 78245196304 over `contract`; the booking's answer.
async fn booked_seat(contract: &str) -> Value {
    let h = harness(
        contract,
        vec![
            json!({"id": 78_245_196_304_u64, "title": "Matinee"}),
            json!({"id": 78_245_196_304_u64, "seat_id": "st-a81f", "title": "Matinee"}),
        ],
    );
    let (status, _, showing) = h
        .seeded("POST", "/showings", Some(json!({"title": "Matinee"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{showing}");
    let (status, _, booked) = h
        .seeded(
            "POST",
            "/showings/78245196304/seats",
            Some(json!({"email": "ann@example.com"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{booked}");
    booked
}

/// The booking answers the showing's id under `id`, as its schema says, beside its
/// own `seat_id`, both of which the seat's read declares: that `id` is no taken
/// identity, and the client gets it as answered.
#[tokio::test]
async fn an_answer_keyed_apart_keeps_the_id_it_names() {
    let booked = booked_seat(SEATS).await;
    assert_eq!(booked["id"], 78_245_196_304_u64, "{booked}");
    assert_eq!(booked["seat_id"], "st-a81f", "{booked}");
}

/// With no read of a seat, the booking's own declared answer, holding `id` and
/// `seat_id` both, keys it apart (Zoom's registrant, whose list item declares no
/// `registrant_id`).
#[tokio::test]
async fn an_answer_its_operation_declares_keyed_apart_keeps_the_id() {
    let booked = booked_seat(&unread_seats()).await;
    assert_eq!(booked["id"], 78_245_196_304_u64, "{booked}");
}

/// When no schema declares `seat_id`, the answer's `seat_id` keys nothing apart, and
/// the showing's id under `id` is re-minted, as before.
#[tokio::test]
async fn an_answer_keyed_apart_only_by_its_body_is_re_minted() {
    let undeclared = unread_seats().replace(
        "      required: [id, seat_id, title]\n",
        "      required: [id, title]\n      additionalProperties: true\n",
    );
    let undeclared = undeclared.replace("        seat_id: {type: string}\n", "");
    assert!(!undeclared.contains("seat_id: {"));
    let booked = booked_seat(&undeclared).await;
    assert_ne!(booked["id"], 78_245_196_304_u64, "{booked}");
}

/// [`SEATS`] with no read of a seat.
fn unread_seats() -> String {
    let start = SEATS
        .find("  /showings/{showingId}/seats/{seat_id}:")
        .unwrap();
    let end = SEATS.find("components:").unwrap();
    format!("{}{}", &SEATS[..start], &SEATS[end..])
}

/// Products whose read declares no `404`: a missing item answers the declared
/// `410`.
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
        "410": {$ref: "#/components/responses/Gone"}
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
      description: "403 `FORBIDDEN`: the token may not access the item."
      content: {application/json: {schema: {$ref: "#/components/schemas/Errors"}}}
    Gone:
      description: "410: the item was deleted."
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

/// A read of a deleted product answers the read's declared `410`, not an undeclared
/// `404`, nor its `403`, which is about who asks.
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
    assert_eq!(status, StatusCode::GONE, "{read}");
    assert_eq!(source, "state", "{read}");
    assert!(read["errors"].is_array(), "{read}");
}
