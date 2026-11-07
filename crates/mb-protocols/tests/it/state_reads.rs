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
        self.send(method, uri, body, &[]).await
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
        self.send(method, uri, body, &[crate::support::SEED]).await
    }

    /// A read on today's path: a seed (`X-Mockingbird-Seed`) takes it past the
    /// state-first executor, which answers an envelope's free members itself (a boolean
    /// `true`, any other from the faker, `tests/exec.rs`). Today's path generates such a read and keeps what it
    /// said.
    async fn read(&self, uri: &str) -> (StatusCode, String, Value) {
        self.send("GET", uri, None, &[("x-mockingbird-seed", "1")])
            .await
    }

    async fn send(
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
        .seeded(
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

/// `success` has no value its schema fixes and no answer of the read's envelope said
/// one: a deal a list seeded is not answered from state, so no guess is served.
#[tokio::test]
async fn a_free_member_nothing_said_is_not_guessed() {
    let deal = json!({"id": 7, "title": "Big deal", "value": 1000.0});
    let h = harness(
        PIPEDRIVE,
        vec![
            json!({"success": true, "data": [deal]}),
            json!({"success": true, "data": deal}),
        ],
    );
    let (status, _, listed) = h.call("GET", "/v1/deals", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");

    let (status, source, read) = h.read("/v1/deals/7").await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_ne!(source, "state", "{read}");
    assert_eq!(h.calls(), 2);
}

/// A free boolean a create answered (Pipedrive's `success: true`) is the create's: the
/// first read is generated, and what it answered is what later reads answer.
#[tokio::test]
async fn a_read_answers_the_envelope_it_answered_before() {
    let deal = json!({"id": 7, "title": "Big deal", "value": 1000.0});
    let answer = json!({"success": true, "data": deal});
    let h = harness(PIPEDRIVE, vec![answer.clone(), answer.clone()]);
    let (status, _, created) = h
        .seeded("POST", "/v1/deals", Some(json!({"title": "Big deal"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let (status, source, read) = h.read("/v1/deals/7").await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_ne!(source, "state", "{read}");
    assert_eq!(h.calls(), 2);

    let (status, source, read) = h.read("/v1/deals/7").await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );
    assert_eq!(read, answer);
    assert_eq!(h.calls(), 2);
}

/// Pipedrive's `Deal` with a timestamp whose format a local patch can mend.
fn pipedrive_with_a_timestamp() -> String {
    let contract = PIPEDRIVE.replace(
        "        value: {type: number}\n",
        "        value: {type: number}\n        add_time: {type: string, format: date-time}\n",
    );
    assert_ne!(contract, PIPEDRIVE);
    contract
}

/// A local patch inside the record (a timestamp that broke its format) leaves what the
/// envelope said as the model said it: the next read is answered from state.
#[tokio::test]
async fn a_patch_inside_the_record_still_remembers_the_envelope() {
    let contract = pipedrive_with_a_timestamp();
    let deal = json!({"id": 7, "title": "Big deal", "add_time": "2026-11-03T10:00:00Z"});
    let broken = json!({"id": 7, "title": "Big deal", "add_time": "yesterday at ten"});
    let h = harness(
        &contract,
        vec![
            json!({"success": true, "data": deal}),
            json!({"success": true, "data": broken}),
        ],
    );
    let (status, _, created) = h
        .seeded("POST", "/v1/deals", Some(json!({"title": "Big deal"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let (status, source, read) = h.read("/v1/deals/7").await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "patched"),
        "{read}"
    );

    let (status, source, read) = h.read("/v1/deals/7").await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );
    assert_eq!(read["success"], true, "{read}");
    assert_eq!(h.calls(), 2);
}

/// A sibling a local patch replaced (a `server_time` that broke its format) is the
/// patch's, not the API's: the envelope is not remembered, and the next read is not
/// answered from state.
#[tokio::test]
async fn a_patched_sibling_is_not_remembered() {
    let contract = PIPEDRIVE.replace(
        "      required: [success, data]\n      properties:\n        success: {type: boolean}\n        data: {$ref: \"#/components/schemas/Deal\"}",
        "      required: [success, data, server_time]\n      properties:\n        success: {type: boolean}\n        server_time: {type: string, format: date-time}\n        data: {$ref: \"#/components/schemas/Deal\"}",
    );
    assert_ne!(contract, PIPEDRIVE);
    let deal = json!({"id": 7, "title": "Big deal", "value": 1000.0});
    let at = "2026-11-03T10:00:00Z";
    let h = harness(
        &contract,
        vec![
            json!({"success": true, "server_time": at, "data": deal}),
            json!({"success": true, "server_time": "just now", "data": deal}),
            json!({"success": true, "server_time": at, "data": deal}),
        ],
    );
    let (status, _, created) = h
        .seeded("POST", "/v1/deals", Some(json!({"title": "Big deal"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let (status, source, read) = h.read("/v1/deals/7").await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "patched"),
        "{read}"
    );

    let (status, source, read) = h.read("/v1/deals/7").await;
    assert_eq!(status, StatusCode::OK, "{read}");
    // The read's own answer may come back from the cache, never from state.
    assert_ne!(source, "state", "{read}");
}

/// A string a create answered (`message: "Deal created"`) is that call's, not the
/// read's: the read is generated, and then its own message is what it answers again.
#[tokio::test]
async fn a_string_stays_with_the_operation_that_answered_it() {
    let contract = PIPEDRIVE.replace(
        "      required: [success, data]\n      properties:\n        success: {type: boolean}\n        data: {$ref: \"#/components/schemas/Deal\"}",
        "      required: [success, data, message]\n      properties:\n        success: {type: boolean}\n        message: {type: string}\n        data: {$ref: \"#/components/schemas/Deal\"}",
    );
    assert_ne!(contract, PIPEDRIVE);
    let deal = json!({"id": 7, "title": "Big deal", "value": 1000.0});
    let found = json!({"success": true, "message": "Deal found", "data": deal});
    let h = harness(
        &contract,
        vec![
            json!({"success": true, "message": "Deal created", "data": deal}),
            found.clone(),
        ],
    );
    let (status, _, created) = h
        .seeded("POST", "/v1/deals", Some(json!({"title": "Big deal"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let (status, source, read) = h.read("/v1/deals/7").await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_ne!(source, "state", "{read}");
    assert_ne!(read["message"], "Deal created", "{read}");
    assert_eq!(h.calls(), 2);

    let (status, source, read) = h.read("/v1/deals/7").await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );
    assert_eq!(read, found);
    assert_eq!(h.calls(), 2);
}

/// An answer holding no record (`{success: false, data: null}`) says nothing of the
/// envelope a record is answered in: the stored record is never served with it.
#[tokio::test]
async fn an_envelope_with_no_record_is_not_remembered() {
    let contract = PIPEDRIVE.replace(
        "    Deal:\n      type: object\n",
        "    Deal:\n      type: object\n      nullable: true\n",
    );
    assert_ne!(contract, PIPEDRIVE);
    let deal = json!({"id": 7, "title": "Big deal", "value": 1000.0});
    let h = harness(
        &contract,
        vec![
            json!({"success": true, "data": deal}),
            json!({"success": false, "data": null}),
            json!({"success": true, "data": deal}),
        ],
    );
    let (status, _, created) = h
        .call("POST", "/v1/deals", Some(json!({"title": "Big deal"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let (status, _, missed) = h.read("/v1/deals/7").await;
    assert_eq!(status, StatusCode::OK, "{missed}");
    assert_eq!(missed["success"], false, "{missed}");

    let (status, source, read) = h.read("/v1/deals/7").await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert!(
        source != "state" || read["success"] == true,
        "{source}: {read}"
    );
}

/// A free boolean or number another operation answered is not shared, nor a string the
/// read's schema does not enumerate; an enum value the read's schema holds is. Cal.com's
/// create's `status: "success"` makes the first read one from state, while its `code` and
/// `created` do not reach it.
#[tokio::test]
async fn only_what_the_read_enumerates_is_shared_across_operations() {
    let contract = CAL.replace(
        "    EventTypeOutput:\n      type: object\n      required: [status, data]\n      properties:\n        status: {$ref: \"#/components/schemas/Status\"}\n",
        "    EventTypeOutput:\n      type: object\n      required: [status, data, code]\n      properties:\n        status: {$ref: \"#/components/schemas/Status\"}\n        code: {type: integer}\n        created: {type: boolean}\n",
    );
    assert_ne!(contract, CAL);
    let h = harness(
        &contract,
        vec![
            json!({"status": "success", "code": 201, "created": true, "data": event_type()}),
            json!({"status": "success", "code": 200, "data": event_type()}),
        ],
    );
    let (status, _, created) = h
        .seeded(
            "POST",
            "/v2/event-types",
            Some(json!({"title": "Thirty Minute Chat"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    // `code` is required and only the create said it: the read is generated.
    let (status, source, read) = h.read("/v2/event-types/42").await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_ne!(source, "state", "{read}");
    assert_eq!(h.calls(), 2);
    let (_, source, read) = h.read("/v2/event-types/42").await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["code"], 200, "{read}");
    assert!(read.get("created").is_none(), "{read}");
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
        .seeded(
            "POST",
            "/v2/event-types",
            Some(json!({"title": "Thirty Minute Chat"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let (status, _, booked) = h
        .seeded(
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
