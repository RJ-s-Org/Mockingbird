//! A request is not refused for a member its schema does not declare: an app's SDK
//! may be pinned to another version of the API than the contract. A contract the
//! smart import inferred from documentation guesses at its request schemas, so its
//! requests are not refused for what a guess gets wrong either: a missing member, a
//! scalar of another type. `strict_requests` validates every request strictly, and a
//! contract not inferred refuses all but undeclared members.
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::{self, HarnessOptions};

/// Slack's incoming webhook as a doc import guessed it: `text` required, no
/// `attachments`, closed; and Opsgenie's `details` values as strings.
fn contract(inferred: bool) -> String {
    format!(
        r##"
openapi: 3.1.0
info: {{title: Hooks, version: "1", x-mockingbird-inferred: {inferred}}}
paths:
  /hook:
    post:
      operationId: postHook
      requestBody:
        content:
          application/json:
            schema:
              type: object
              additionalProperties: false
              required: [text]
              properties:
                text: {{type: string}}
                mode: {{type: string, enum: [plain, rich]}}
                details: {{type: object, additionalProperties: {{type: string}}}}
                responder: {{anyOf: [{{$ref: "#/components/schemas/Responder"}}, {{type: "null"}}]}}
                meta: {{type: object, additionalProperties: false, properties: {{source: {{type: string}}}}}}
      responses:
        "200": {{description: ok, content: {{application/json: {{schema: {{type: object, properties: {{ok: {{type: boolean}}}}}}}}}}}}
  /payment_intents:
    post:
      operationId: createPaymentIntent
      requestBody:
        content:
          application/x-www-form-urlencoded:
            schema:
              type: object
              additionalProperties: false
              required: [amount]
              properties:
                amount: {{type: integer}}
                metadata: {{type: object, additionalProperties: false, properties: {{order: {{type: string}}}}}}
      responses:
        "200": {{description: ok, content: {{application/json: {{schema: {{type: object, properties: {{ok: {{type: boolean}}}}}}}}}}}}
  /items:
    get:
      operationId: listItems
      parameters:
        - {{name: limit, in: query, required: true, schema: {{type: integer}}}}
      responses:
        "200": {{description: ok, content: {{application/json: {{schema: {{type: object, properties: {{ok: {{type: boolean}}}}}}}}}}}}
components:
  schemas:
    Responder:
      type: object
      required: [id, type]
      properties:
        id: {{type: string}}
        type: {{type: string}}
"##
    )
}

async fn status_of(inferred: bool, strict: bool, body: Value) -> StatusCode {
    let h = support::harness(
        &contract(inferred),
        vec![json!({"ok": true})],
        &HarnessOptions::default(),
        |mut rt| {
            rt.strict_requests = strict;
            rt
        },
    );
    h.send("POST", "/hook", Some(body), &[]).await.0
}

fn healthchecks_body() -> Value {
    // `responder` is a guessed nullable member (`anyOf` with `null`) lacking a guessed
    // required member.
    json!({"attachments": [{"color": "danger"}], "details": {"Total pings": 0}, "responder": {"id": "r1"}})
}

async fn query_status(inferred: bool, query: &str) -> StatusCode {
    let h = support::harness(
        &contract(inferred),
        vec![json!({"ok": true})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    h.send("GET", &format!("/items{query}"), None, &[]).await.0
}

/// The query of an inferred contract is lenient too: a missing or mistyped
/// parameter is taken.
#[tokio::test]
async fn an_inferred_contract_takes_a_query_its_guesses_refuse() {
    assert_eq!(query_status(true, "").await, StatusCode::OK);
    assert_eq!(query_status(true, "?limit=ten").await, StatusCode::OK);
    assert_eq!(query_status(false, "").await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_inferred_contract_takes_what_its_guesses_refuse() {
    assert_eq!(
        status_of(true, false, healthchecks_body()).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_inferred_contract_still_refuses_a_wrong_shape_or_enum() {
    let shape = json!({"text": {"a": 1}});
    assert_eq!(status_of(true, false, shape).await, StatusCode::BAD_REQUEST);
    let enumerated = json!({"text": "x", "mode": "loud"});
    assert_eq!(
        status_of(true, false, enumerated).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn strict_requests_or_a_contract_not_inferred_validates_strictly() {
    assert_eq!(
        status_of(true, true, healthchecks_body()).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        status_of(false, false, healthchecks_body()).await,
        StatusCode::BAD_REQUEST
    );
}

/// Stripe's newest PaymentIntent create no longer declares `payment_method_types`,
/// which an app's older stripe-node still sends; also undeclared one level down.
fn versioned_body() -> Value {
    json!({"text": "hi", "payment_method_types": ["card"], "meta": {"source": "app", "legacy": true}})
}

#[tokio::test]
async fn a_contract_not_inferred_takes_an_undeclared_member() {
    assert_eq!(
        status_of(false, false, versioned_body()).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn strict_requests_refuses_an_undeclared_member() {
    assert_eq!(
        status_of(false, true, versioned_body()).await,
        StatusCode::BAD_REQUEST
    );
}

/// Taking an undeclared member is all: a missing required member, a wrong type or a
/// bad enum value beside it is still refused.
#[tokio::test]
async fn a_contract_not_inferred_still_refuses_a_missing_member_or_wrong_type() {
    for body in [
        json!({"payment_method_types": ["card"]}),
        json!({"text": 5, "payment_method_types": ["card"]}),
        json!({"text": "hi", "mode": "loud", "payment_method_types": ["card"]}),
        json!({"text": "hi", "meta": {"source": 1, "legacy": true}}),
    ] {
        assert_eq!(
            status_of(false, false, body.clone()).await,
            StatusCode::BAD_REQUEST,
            "{body}"
        );
    }
}

async fn form_status(strict: bool, body: &str) -> StatusCode {
    let h = support::harness(
        &contract(false),
        vec![json!({"ok": true})],
        &HarnessOptions::default(),
        |mut rt| {
            rt.strict_requests = strict;
            rt
        },
    );
    h.send_raw(
        ("POST", "/payment_intents"),
        body,
        "application/x-www-form-urlencoded",
    )
    .await
}

/// stripe-node's form body: an undeclared bracketed array and an undeclared nested
/// member are taken, unless `strict_requests`; a missing or mistyped one is not.
#[tokio::test]
async fn a_form_body_with_undeclared_members_is_taken() {
    let body = "amount=2000&payment_method_types[]=card&metadata[order]=6735&metadata[legacy]=1";
    assert_eq!(form_status(false, body).await, StatusCode::OK);
    assert_eq!(form_status(true, body).await, StatusCode::BAD_REQUEST);
    let missing = "payment_method_types[]=card";
    assert_eq!(form_status(false, missing).await, StatusCode::BAD_REQUEST);
    let mistyped = "amount=lots&payment_method_types[]=card";
    assert_eq!(form_status(false, mistyped).await, StatusCode::BAD_REQUEST);
}
