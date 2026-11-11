//! A contract that declares its errors only as `default` (Stripe's): a world refusal,
//! `not_found` and `unknown_ids: 404` answer their status with the `default` response;
//! a read whose world description writes its target's path parameter, whatever the
//! name, is a read by id for `unknown_ids: 404`.
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::json;

use crate::support::{self, HarnessOptions};

/// Payment intents and checkout sessions read by `{intent}` and `{session}`; every
/// error is the `default` response, its body nesting `type`, `code` and `message`
/// under `error`.
const PAYMENTS: &str = r##"
openapi: 3.0.0
info: {title: Payments, version: "1"}
paths:
  /v1/payment_intents:
    post:
      operationId: PostPaymentIntents
      requestBody: {content: {application/json: {schema: {type: object, properties: {amount: {type: integer}, currency: {type: string}, payment_method_types: {type: array, items: {type: string}}}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/PaymentIntent"}}}}
        default: {description: error, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /v1/payment_intents/{intent}:
    get:
      operationId: GetPaymentIntentsIntent
      parameters: [{name: intent, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/PaymentIntent"}}}}
        default: {description: error, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /v1/checkout/sessions/{session}:
    get:
      operationId: GetCheckoutSessionsSession
      parameters: [{name: session, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Session"}}}}
        default: {description: error, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
components:
  schemas:
    PaymentIntent:
      type: object
      required: [id, amount, currency]
      properties:
        id: {type: string}
        amount: {type: integer}
        currency: {type: string}
    Session:
      type: object
      required: [id, status]
      properties:
        id: {type: string}
        status: {type: string}
    Error:
      type: object
      required: [error]
      properties:
        error:
          type: object
          required: [type]
          properties:
            type: {type: string}
            code: {type: string}
            message: {type: string}
"##;

fn options() -> HarnessOptions {
    HarnessOptions {
        unknown_ids_not_found: true,
        ..HarnessOptions::default()
    }
}

/// `refuse` with a 400 and `not_found: 404` validate against `default` alone, and
/// answer those statuses with the `default` response's body.
#[tokio::test]
async fn a_default_error_response_declares_a_refusal_and_not_found() {
    let world = json!({"operations": {
        "PostPaymentIntents": {"refuse": [{
            "when": "request.body.currency == 'usd' && 'au_becs_debit' in request.body.payment_method_types",
            "status": 400,
            "code": "payment_intent_invalid_currency",
            "message": "au_becs_debit takes aud"
        }]},
        "GetPaymentIntentsIntent": {"target": {"path_param": "intent"}, "not_found": 404}
    }});
    let (_, _, refused) = support::effective_world(
        &mb_import::compile(PAYMENTS.as_bytes()).unwrap(),
        world.clone(),
    );
    assert!(refused.is_empty(), "{refused:?}");
    let h = support::described_with(PAYMENTS, world, vec![], &options(), false);

    let (status, _, body) = h
        .send(
            "POST",
            "/v1/payment_intents",
            Some(json!({"amount": 100, "currency": "usd", "payment_method_types": ["au_becs_debit"]})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "payment_intent_invalid_currency");
    assert_eq!(body["error"]["message"], "au_becs_debit takes aud");

    let (status, _, body) = h
        .send("GET", "/v1/payment_intents/pi_missing", None, &[])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"]["type"].is_string(), "{body}");
}

/// A read whose world target is `{session}`, a name that does not look like an id,
/// answers an id the session does not hold with its 404 under `unknown_ids: 404`.
#[tokio::test]
async fn unknown_ids_follow_the_target_path_parameter() {
    let world = json!({"operations": {
        "GetCheckoutSessionsSession": {"target": {"path_param": "session"}}
    }});
    let h = support::described_with(PAYMENTS, world, vec![], &options(), false);
    let (status, _, body) = h
        .send("GET", "/v1/checkout/sessions/cs_test_invalid", None, &[])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"]["type"].is_string(), "{body}");
}

/// A contract that lists its errors by code still refuses only a status it lists.
#[test]
fn listed_errors_still_bound_a_refusal() {
    let listed = PAYMENTS.replacen(
        "default: {description: error",
        "\"422\": {description: error",
        1,
    );
    let world = json!({"operations": {"PostPaymentIntents": {"refuse": [
        {"when": "true", "status": 400},
        {"when": "true", "status": 422}
    ]}}});
    let (_, effective, refused) =
        support::effective_world(&mb_import::compile(listed.as_bytes()).unwrap(), world);
    let pointers: Vec<&str> = refused.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        pointers,
        ["/operations/PostPaymentIntents/refuse/0/status"],
        "{refused:?}"
    );
    let kept = &effective.operations["PostPaymentIntents"].refuse;
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].status, 422);
}
