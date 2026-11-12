//! A minted id whose member has a pattern takes the shape the contract's example shows:
//! the alternative of the pattern the example takes, and its case, so a message `sid`
//! of `^(SM|MM)[0-9a-fA-F]{32}$` shown as `SMaaaa…` is minted `SM` and lowercase hex,
//! never `MM` or mixed case. So is an id a create answers only in a header.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_protocols::{MockRuntime, http_rest};
use mb_state::MemoryStore;
use serde_json::Value;
use tower::ServiceExt;

const PATTERN: &str = "^(SM|MM)[0-9a-fA-F]{32}$";

/// Messages of an account, sent as forms, with the example of a message's `sid` given
/// on the member (`member_example`) or on the message schema as a whole
/// (`record_example`).
fn contract(member_example: &str, record_example: &str) -> String {
    format!(
        r##"
openapi: 3.0.0
info: {{title: Messaging, version: "2010-04-01"}}
paths:
  /2010-04-01/Accounts/{{AccountSid}}/Messages.json:
    post:
      operationId: CreateMessage
      parameters: [{{name: AccountSid, in: path, required: true, schema: {{type: string}}}}]
      requestBody:
        required: true
        content:
          application/x-www-form-urlencoded:
            schema:
              type: object
              required: [To]
              properties:
                To: {{type: string}}
                From: {{type: string}}
                Body: {{type: string}}
      responses:
        "201":
          description: Created
          content: {{application/json: {{schema: {{$ref: "#/components/schemas/Message"}}}}}}
  /2010-04-01/Accounts/{{AccountSid}}/Messages/{{Sid}}.json:
    get:
      operationId: FetchMessage
      parameters:
        - {{name: AccountSid, in: path, required: true, schema: {{type: string}}}}
        - {{name: Sid, in: path, required: true, schema: {{type: string, pattern: "{PATTERN}"}}}}
      responses:
        "200":
          description: OK
          content: {{application/json: {{schema: {{$ref: "#/components/schemas/Message"}}}}}}
components:
  schemas:
    Message:
      type: object
      {record_example}
      properties:
        sid:
          type: string
          minLength: 34
          maxLength: 34
          pattern: "{PATTERN}"
          nullable: true
          {member_example}
        to: {{type: string, nullable: true}}
        from: {{type: string, nullable: true}}
        body: {{type: string, nullable: true}}
"##
    )
}

const EXAMPLE: &str = "SMaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn router(contract: &str) -> axum::Router {
    router_named("messaging", contract)
}

fn router_named(name: &str, contract: &str) -> axum::Router {
    let rt = MockRuntime::new(
        name,
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    http_rest::router(Arc::new(rt))
}

async fn call(router: &axum::Router, method: &str, uri: &str, form: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("x-mockingbird-session", "s1")
        .body(Body::from(form.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Sends `n` messages and checks each minted `sid`, read back by it.
async fn sids_follow_the_example(contract: &str, n: usize) {
    let router = router(contract);
    // Built at run time: a literal account sid trips secret scanners.
    let base = format!("/2010-04-01/Accounts/AC{}", "0".repeat(32));
    let mut seen = Vec::new();
    for i in 0..n {
        let form = format!("To=%2B1555000{i:04}&From=%2B15550009999&Body=hi");
        let (status, created) =
            call(&router, "POST", &format!("{base}/Messages.json"), &form).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let sid = created["sid"].as_str().unwrap_or_default().to_string();
        // `SM` and 32 lowercase hex digits: a match of the pattern, in the example's
        // alternative and case.
        assert!(
            sid.starts_with("SM") && sid.len() == 34,
            "{sid} is not an SMS sid"
        );
        assert!(
            sid[2..].chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "{sid} is not lowercase hex"
        );
        assert!(!seen.contains(&sid), "{sid} minted twice");
        let (status, read) = call(&router, "GET", &format!("{base}/Messages/{sid}.json"), "").await;
        assert_eq!(status, StatusCode::OK, "{read}");
        assert_eq!(read["sid"], created["sid"]);
        seen.push(sid);
    }
}

#[tokio::test]
async fn a_minted_sid_takes_its_members_example_shape() {
    let contract = contract(&format!("example: {EXAMPLE}"), "");
    sids_follow_the_example(&contract, 20).await;
}

#[tokio::test]
async fn a_minted_sid_takes_its_records_example_shape() {
    let contract = contract(
        "",
        &format!("example: {{sid: {EXAMPLE}, to: \"+15550001111\"}}"),
    );
    sids_follow_the_example(&contract, 20).await;
}

/// Things a create answers only in a header (`header`: an id header, or `Location`),
/// their id patterned and shown by an example.
fn header_contract(header: &str) -> String {
    format!(
        r##"
openapi: 3.0.0
info: {{title: Things, version: "1"}}
paths:
  /things:
    post:
      operationId: createThing
      requestBody:
        required: true
        content: {{application/json: {{schema: {{type: object, properties: {{name: {{type: string}}}}}}}}}}
      responses:
        "201":
          description: created
          headers:
            {header}: {{schema: {{type: string}}}}
  /things/{{thingId}}:
    get:
      operationId: getThing
      parameters: [{{name: thingId, in: path, required: true, schema: {{type: string}}}}]
      responses:
        "200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Thing"}}}}}}}}
components:
  schemas:
    Thing:
      type: object
      properties:
        id: {{type: string, pattern: "^(TH|TX)[0-9a-fA-F]{{24}}$", example: THaaaaaaaaaaaaaaaaaaaaaaaa}}
        name: {{type: string}}
"##
    )
}

#[tokio::test]
async fn an_id_answered_in_a_header_takes_its_examples_shape() {
    for header in ["Thing-Id", "Location"] {
        let router = router_named("things", &header_contract(header));
        let mut seen = Vec::new();
        for i in 0..20 {
            let req = Request::builder()
                .method("POST")
                .uri("/things")
                .header("content-type", "application/json")
                .header("x-mockingbird-session", "s1")
                .body(Body::from(format!(r#"{{"name": "t{i}"}}"#)))
                .unwrap();
            let resp = router.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED, "{header}");
            let served = resp
                .headers()
                .get(header)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let id = served.rsplit('/').next().unwrap_or_default().to_string();
            assert!(id.starts_with("TH") && id.len() == 26, "{header}: {served}");
            assert!(
                id[2..].chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
                "{header}: {served} is not lowercase hex"
            );
            assert!(!seen.contains(&id), "{id} minted twice");
            let (status, read) = call(&router, "GET", &format!("/things/{id}"), "").await;
            assert_eq!(status, StatusCode::OK, "{header}: {read}");
            assert_eq!(read["id"], id.as_str());
            seen.push(id);
        }
    }
}
