//! A contract the smart import inferred from documentation guesses at its request
//! schemas, so its requests are not refused for what a guess gets wrong: a missing
//! member, an undeclared one, a scalar of another type. `strict_requests` validates
//! them strictly, and a contract not inferred always is.
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
