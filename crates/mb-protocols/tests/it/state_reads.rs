//! Reads a stored record answers, and the records a write refers to, where the
//! contract holds its records in envelopes with members of their own (Cal.com's
//! `{status, data}`, Pipedrive's `{success, data}`) and names its types in kebab case
//! (`event-types`, referred to as `eventTypeId`). Driven over HTTP with a scripted LLM.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{CachingStateStore, MemoryStore, StateStore};
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
        CachingStateStore::wrap(
            store as Arc<dyn StateStore>,
            Duration::from_secs(3600),
            mb_state::DEFAULT_CACHE_ENTRIES,
        ),
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
        (
            status,
            source,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }
}

/// Cal.com's shape: every answer is `{status, data}`, both required, `status` an enum.
const CAL: &str = r##"
openapi: 3.0.0
info: {title: Cal, version: "2"}
paths:
  /v2/event-types:
    get:
      operationId: listEventTypes
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/EventTypeList"}}}}}
    post:
      operationId: createEventType
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/EventTypeOutput"}}}}}
  /v2/event-types/{eventTypeId}:
    parameters: [{name: eventTypeId, in: path, required: true, schema: {type: integer}}]
    get:
      operationId: getEventType
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/EventTypeOutput"}}}}}
  /v2/bookings:
    get:
      operationId: listBookings
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/BookingList"}}}}}
    post:
      operationId: createBooking
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/BookingOutput"}}}}}
  /v2/bookings/{bookingUid}:
    parameters: [{name: bookingUid, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getBooking
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/BookingOutput"}}}}}
components:
  schemas:
    Status: {type: string, enum: [success, error]}
    EventType:
      type: object
      required: [id, title, slug]
      properties:
        id: {type: integer}
        title: {type: string}
        slug: {type: string}
    EventTypeOutput:
      type: object
      required: [status, data]
      properties:
        status: {$ref: "#/components/schemas/Status"}
        data: {$ref: "#/components/schemas/EventType"}
    EventTypeList:
      type: object
      required: [status, data]
      properties:
        status: {$ref: "#/components/schemas/Status"}
        data: {type: array, items: {$ref: "#/components/schemas/EventType"}}
    Booking:
      type: object
      required: [id, uid, eventTypeId]
      properties:
        id: {type: integer}
        uid: {type: string}
        eventTypeId: {type: integer}
        title: {type: string}
    BookingOutput:
      type: object
      required: [status, data]
      properties:
        status: {$ref: "#/components/schemas/Status"}
        data: {$ref: "#/components/schemas/Booking"}
    BookingList:
      type: object
      required: [status, data]
      properties:
        status: {$ref: "#/components/schemas/Status"}
        data: {type: array, items: {$ref: "#/components/schemas/Booking"}}
"##;

/// Pipedrive's shape: `{success, data}`, `success` a plain boolean.
const PIPEDRIVE: &str = r##"
openapi: 3.0.0
info: {title: Pipedrive, version: "1"}
paths:
  /v1/deals:
    get:
      operationId: getDeals
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/DealList"}}}}}
    post:
      operationId: addDeal
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/DealOutput"}}}}}
  /v1/deals/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
    get:
      operationId: getDeal
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/DealOutput"}}}}}
components:
  schemas:
    Deal:
      type: object
      required: [id, title]
      properties:
        id: {type: integer}
        title: {type: string}
        value: {type: number}
    DealOutput:
      type: object
      required: [success, data]
      properties:
        success: {type: boolean}
        data: {$ref: "#/components/schemas/Deal"}
    DealList:
      type: object
      required: [success, data]
      properties:
        success: {type: boolean}
        data: {type: array, items: {$ref: "#/components/schemas/Deal"}}
"##;

fn event_type() -> Value {
    json!({"id": 42, "title": "Thirty Minute Chat", "slug": "thirty-minute-chat"})
}

/// The stored event type answers its read, in an envelope with the `status` its
/// schema requires, from state and without another model call.
#[tokio::test]
async fn a_record_in_an_envelope_with_an_enum_member_is_read_from_state() {
    let h = harness(
        CAL,
        vec![json!({"status": "success", "data": event_type()})],
    );
    let (status, _, created) = h
        .call(
            "POST",
            "/v2/event-types",
            Some(json!({"title": "Thirty Minute Chat"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["data"]["id"], 42, "{created}");

    let (status, source, read) = h.call("GET", "/v2/event-types/42", None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );
    assert_eq!(read, json!({"status": "success", "data": event_type()}));
    assert_eq!(h.calls(), 1);
}

/// `success` has no value its schema fixes: the faker gives it one, and the record
/// is served as it was stored.
#[tokio::test]
async fn a_record_in_an_envelope_with_a_free_member_is_read_from_state() {
    let deal = json!({"id": 7, "title": "Big deal", "value": 1000.0});
    let h = harness(PIPEDRIVE, vec![json!({"success": true, "data": deal})]);
    let (status, _, created) = h
        .call("POST", "/v1/deals", Some(json!({"title": "Big deal"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    for _ in 0..2 {
        let (status, source, read) = h.call("GET", "/v1/deals/7", None).await;
        assert_eq!(
            (status, source.as_str()),
            (StatusCode::OK, "state"),
            "{read}"
        );
        assert_eq!(read["data"], deal);
        assert!(read["success"].is_boolean(), "{read}");
    }
    assert_eq!(h.calls(), 1);
}

/// What the create's envelope said is what a read of the same envelope says: Pipedrive's
/// `success: true`, which its schema (a plain boolean) does not fix, and a request id no
/// schema could give.
#[tokio::test]
async fn a_read_answers_the_envelope_the_create_answered() {
    let contract = PIPEDRIVE.replace(
        "      required: [success, data]\n      properties:\n        success: {type: boolean}\n        data: {$ref: \"#/components/schemas/Deal\"}",
        "      required: [success, data, request_id]\n      properties:\n        success: {type: boolean}\n        request_id: {type: string}\n        data: {$ref: \"#/components/schemas/Deal\"}",
    );
    assert_ne!(contract, PIPEDRIVE);
    let deal = json!({"id": 7, "title": "Big deal", "value": 1000.0});
    let answer = json!({"success": true, "request_id": "req_8f2c1a", "data": deal});
    let h = harness(&contract, vec![answer.clone()]);
    let (status, _, created) = h
        .call("POST", "/v1/deals", Some(json!({"title": "Big deal"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let (status, source, read) = h.call("GET", "/v1/deals/7", None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );
    assert_eq!(read, answer);
    assert_eq!(h.calls(), 1);
}

/// A booking's `eventTypeId` refers to an `event-types` record: the create's prompt
/// is shown it.
#[tokio::test]
async fn a_camel_case_reference_finds_a_kebab_case_type() {
    let booking = json!({"id": 1, "uid": "bk_1", "eventTypeId": 42, "title": "Chat"});
    let h = harness(
        CAL,
        vec![
            json!({"status": "success", "data": event_type()}),
            json!({"status": "success", "data": booking}),
        ],
    );
    let (status, _, created) = h
        .call(
            "POST",
            "/v2/event-types",
            Some(json!({"title": "Thirty Minute Chat"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let (status, _, booked) = h
        .call(
            "POST",
            "/v2/bookings",
            Some(json!({"eventTypeId": 42, "start": "2026-11-04T10:00:00Z"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{booked}");
    assert_eq!(h.calls(), 2);
    let prompt = format!("{:?}", h.provider.requests()[1]);
    assert!(prompt.contains("Thirty Minute Chat"), "{prompt}");
}
