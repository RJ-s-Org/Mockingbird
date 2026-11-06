//! A written error body is kept for every later call of its operation, status and
//! cause, so a degenerate one (a code repeated for kilobytes) is asked for again, and
//! never kept: past a second degenerate answer, the faker's body stands.

use mb_engine::OperationCall;
use mb_engine::error_body::ErrorCause;
use mb_import::compile;
use mb_mcm::Bundle;
use serde_json::{Value, json};

use crate::support::scripted;

/// `POST /videos` refusing an invalid request with `400 {code, message}`, the
/// response's description naming its code.
fn videos() -> Bundle {
    let src = r##"
openapi: 3.1.0
info: {title: Videos, version: "1"}
paths:
  /videos:
    post:
      operationId: createVideo
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}
        "400": {description: "The request is invalid: `invalid_parameters`.", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
components:
  schemas:
    Error:
      type: object
      required: [code, message]
      properties: {code: {type: string}, message: {type: string}}
"##;
    compile(src.as_bytes()).unwrap()
}

fn call() -> OperationCall {
    OperationCall {
        operation_id: "createVideo".into(),
        body: Some(json!({"title": 7})),
        ..OperationCall::default()
    }
}

fn degenerate() -> String {
    json!({"code": "invalid_parameters!".repeat(200), "message": "The request is invalid."})
        .to_string()
}

fn good() -> Value {
    json!({"code": "invalid_parameters", "message": "The title must be a string."})
}

async fn answer(engine: &mb_engine::Engine, bundle: &Bundle) -> Value {
    let op = bundle.operation("createVideo").unwrap();
    engine
        .error_answer(bundle, op, &call(), 400, ErrorCause::Invalid)
        .await
        .unwrap()
        .body
        .unwrap()
}

#[tokio::test]
async fn a_degenerate_body_is_asked_for_again_and_the_good_one_kept() {
    let bundle = videos();
    let (provider, engine) = scripted(vec![Ok(degenerate()), Ok(good().to_string())]);
    for _ in 0..3 {
        let body = answer(&engine, &bundle).await;
        assert_eq!(body["code"], "invalid_parameters", "{body}");
    }
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn two_degenerate_bodies_leave_the_fakers() {
    let bundle = videos();
    let (provider, engine) = scripted(vec![Ok(degenerate()), Ok(degenerate())]);
    for _ in 0..3 {
        let body = answer(&engine, &bundle).await;
        let code = body["code"].as_str().unwrap();
        assert!(!code.contains("invalid_parameters!"), "{body}");
    }
    // The failure is kept for a while, as a failed write is: nothing more is asked.
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn a_good_body_is_kept_on_the_first_answer() {
    let bundle = videos();
    let (provider, engine) = scripted(vec![Ok(good().to_string())]);
    for _ in 0..3 {
        assert_eq!(answer(&engine, &bundle).await, good());
    }
    assert_eq!(provider.requests().len(), 1);
}
