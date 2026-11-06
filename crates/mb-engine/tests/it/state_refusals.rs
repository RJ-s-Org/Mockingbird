//! Refusals the mock decides from state (a unique value already held, a stale
//! concurrency token) answer the error body the model writes once per operation, status
//! and cause, with the call's facts laid over it; offline, the faker's body with those
//! facts.

use mb_engine::error_body::{ErrorCause, RefusalFacts};
use mb_engine::{Engine, EngineConfig, OperationCall, Source};
use mb_import::compile;
use mb_mcm::Bundle;
use serde_json::json;

use crate::support::scripted;

/// `POST /aliases` refusing a taken alias with `409` and a stale one with `412`, both
/// `{ErrorCode, Message}`; `field` holds the violated member when the schema has one.
fn aliases(with_field: bool) -> Bundle {
    let field = if with_field {
        ", Field: {type: string}"
    } else {
        ""
    };
    let src = format!(
        r##"
openapi: 3.1.0
info: {{title: Aliases, version: "1"}}
paths:
  /aliases:
    post:
      operationId: createAlias
      requestBody: {{content: {{application/json: {{schema: {{type: object, properties: {{alias: {{type: string}}}}}}}}}}}}
      responses:
        "201": {{description: created, content: {{application/json: {{schema: {{type: object, properties: {{id: {{type: string}}}}}}}}}}}}
        "409": {{description: conflict, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Error"}}}}}}}}
        "412": {{description: stale, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Error"}}}}}}}}
components:
  schemas:
    Error:
      type: object
      required: [ErrorCode, Message]
      properties: {{ErrorCode: {{type: integer}}, Message: {{type: string}}{field}}}
"##
    );
    compile(src.as_bytes()).unwrap()
}

fn call() -> OperationCall {
    OperationCall {
        operation_id: "createAlias".into(),
        body: Some(json!({"alias": "welcome-1"})),
        ..OperationCall::default()
    }
}

fn taken() -> RefusalFacts {
    RefusalFacts::taken(&[("alias".to_string(), json!("welcome-1"))])
}

#[tokio::test]
async fn a_taken_value_answers_the_models_body_once_naming_the_member() {
    let bundle = aliases(true);
    let op = bundle.operation("createAlias").unwrap();
    let written =
        json!({"ErrorCode": 1122, "Message": "The alias is already in use", "Field": "x"});
    let (provider, engine) = scripted(vec![Ok(written.to_string())]);
    for _ in 0..2 {
        let out = engine
            .refuse_held(&bundle, op, &call(), 409, ErrorCause::Taken, &taken())
            .await
            .unwrap();
        assert_eq!((out.status, out.source), (409, Source::State));
        let body = out.body.unwrap();
        assert_eq!(body["ErrorCode"], 1122, "{body}");
        let message = body["Message"].as_str().unwrap();
        assert!(
            message.contains("alias") && message.contains("welcome-1"),
            "{message}"
        );
        assert_eq!(body["Field"], "alias", "{body}");
    }
    // The second refusal asks nothing: the body is kept.
    assert_eq!(provider.requests().len(), 1);
    let requests = provider.requests();
    let prompt = &requests[0].messages.last().unwrap().content;
    assert!(
        prompt.contains("already held by another record"),
        "{prompt}"
    );
}

#[tokio::test]
async fn offline_the_fakers_body_names_the_taken_member() {
    let bundle = aliases(false);
    let op = bundle.operation("createAlias").unwrap();
    let engine = Engine::new(None, EngineConfig::default());
    let out = engine
        .refuse_held(&bundle, op, &call(), 409, ErrorCause::Taken, &taken())
        .await
        .unwrap();
    assert_eq!((out.status, out.source), (409, Source::State));
    let body = out.body.unwrap();
    assert!(body["ErrorCode"].is_i64(), "{body}");
    assert_eq!(body["Message"], "alias 'welcome-1' is already in use");
}

#[tokio::test]
async fn a_failed_write_serves_the_fakers_body_with_the_facts() {
    let bundle = aliases(false);
    let op = bundle.operation("createAlias").unwrap();
    let (_, engine) = scripted(vec![Ok("not json".into()), Ok("still not".into())]);
    let out = engine
        .refuse_held(&bundle, op, &call(), 409, ErrorCause::Taken, &taken())
        .await
        .unwrap();
    assert_eq!((out.status, out.source), (409, Source::State));
    assert_eq!(
        out.body.unwrap()["Message"],
        "alias 'welcome-1' is already in use"
    );
}

#[tokio::test]
async fn a_stale_token_has_its_own_written_body() {
    let bundle = aliases(false);
    let op = bundle.operation("createAlias").unwrap();
    let (provider, engine) = scripted(vec![
        Ok(json!({"ErrorCode": 1122, "Message": "taken"}).to_string()),
        Ok(json!({"ErrorCode": 3001, "Message": "stale"}).to_string()),
    ]);
    let refuse = |code, cause, facts: RefusalFacts| {
        let engine = &engine;
        let bundle = &bundle;
        async move {
            let out = engine
                .refuse_held(bundle, op, &call(), code, cause, &facts)
                .await
                .unwrap();
            assert_eq!(out.source, Source::State);
            out.body.unwrap()
        }
    };
    let first = refuse(409, ErrorCause::Taken, taken()).await;
    assert_eq!(first["ErrorCode"], 1122);
    // Another cause is another body, even for the very same status.
    let stale = refuse(409, ErrorCause::Stale, RefusalFacts::stale()).await;
    assert_eq!(stale["ErrorCode"], 3001, "{stale}");
    let message = stale["Message"].as_str().unwrap();
    assert!(message.contains("current version"), "{message}");
    assert_eq!(provider.requests().len(), 2);
    let again = refuse(409, ErrorCause::Stale, RefusalFacts::stale()).await;
    assert_eq!(again["ErrorCode"], 3001);
    assert_eq!(provider.requests().len(), 2);
    let requests = provider.requests();
    let prompt = &requests[1].messages.last().unwrap().content;
    assert!(prompt.contains("concurrency token"), "{prompt}");
}

#[tokio::test]
async fn an_undeclared_status_is_no_refusal() {
    let bundle = aliases(false);
    let op = bundle.operation("createAlias").unwrap();
    let engine = Engine::new(None, EngineConfig::default());
    assert!(
        engine
            .refuse_held(&bundle, op, &call(), 422, ErrorCause::Taken, &taken())
            .await
            .is_none()
    );
}

#[test]
fn a_taken_set_names_each_member_and_value() {
    let facts = RefusalFacts::taken(&[
        ("displayName".to_string(), json!("Docs")),
        ("rank".to_string(), json!(3)),
    ]);
    assert_eq!(
        facts.text(),
        "displayName 'Docs' with rank 3 is already in use"
    );
    assert_eq!(facts.members(), ["displayName", "rank"]);
}
