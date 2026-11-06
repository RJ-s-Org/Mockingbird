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

/// `POST /aliases` refusing with `422` whose body is `error` (YAML).
fn refusing_with(error: &str) -> Bundle {
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
        "422": {{description: refused, content: {{application/json: {{schema: {error}}}}}}}
"##
    );
    compile(src.as_bytes()).unwrap()
}

/// A map of reasons (`{"alias": ["…"]}`) is keyed by the taken member, with or
/// without a model (its schema has no code for one to write).
#[tokio::test]
async fn a_map_of_reasons_is_keyed_by_the_taken_member() {
    let bundle =
        refusing_with("{type: object, additionalProperties: {type: array, items: {type: string}}}");
    let op = bundle.operation("createAlias").unwrap();
    let (provider, engine) = scripted(vec![]);
    let out = engine
        .refuse_held(&bundle, op, &call(), 422, ErrorCause::Taken, &taken())
        .await
        .unwrap();
    assert_eq!((out.status, out.source), (422, Source::State));
    assert_eq!(
        out.body.unwrap(),
        json!({"alias": ["alias 'welcome-1' is already in use"]})
    );
    assert!(provider.requests().is_empty());

    // Facts about no member: each of the map's keys holds the message.
    let out = engine
        .refuse_held(
            &bundle,
            op,
            &call(),
            422,
            ErrorCause::Stale,
            &RefusalFacts::stale(),
        )
        .await
        .unwrap();
    let body = out.body.unwrap();
    let map = body.as_object().unwrap();
    assert!(!map.is_empty(), "{body}");
    for reasons in map.values() {
        assert_eq!(reasons, &json!([RefusalFacts::stale().text()]), "{body}");
    }
}

/// An array of error objects naming no field (JSON:API's `{status, title}`) says the
/// message in each item's message-like member.
#[tokio::test]
async fn an_array_of_error_objects_says_the_message() {
    let bundle = refusing_with(
        "{type: object, properties: {errors: {type: array, minItems: 1, items: {type: object, properties: {status: {type: string}, title: {type: string}}}}}}",
    );
    let op = bundle.operation("createAlias").unwrap();
    let engine = Engine::new(None, EngineConfig::default());
    let out = engine
        .refuse_held(&bundle, op, &call(), 422, ErrorCause::Taken, &taken())
        .await
        .unwrap();
    assert_eq!((out.status, out.source), (422, Source::State));
    let body = out.body.unwrap();
    let errors = body["errors"].as_array().unwrap();
    assert!(!errors.is_empty(), "{body}");
    for error in errors {
        assert_eq!(
            error["title"], "alias 'welcome-1' is already in use",
            "{body}"
        );
    }
}

/// `POST /aliases` whose `422` and `404` share a JSON:API error body, its `status` a
/// string member declared by `status`: a shared schema, so no example of it is taken
/// for this status alone.
fn json_api(status: &str) -> Bundle {
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
        "404": {{description: missing, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Errors"}}}}}}}}
        "422": {{description: refused, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Errors"}}}}}}}}
components:
  schemas:
    Errors:
      type: object
      required: [errors]
      properties:
        errors:
          type: array
          minItems: 1
          items:
            type: object
            properties:
              status: {status}
              title: {{type: string}}
              detail: {{type: string}}
              source: {{type: object, properties: {{pointer: {{type: string}}}}}}
"##
    );
    compile(src.as_bytes()).unwrap()
}

/// A status member whose examples are 3-digit codes holds the refusal's own status,
/// offline, every time.
#[tokio::test]
async fn a_status_text_member_says_the_refusals_status_offline() {
    let bundle = json_api(r#"{type: string, examples: ["422"]}"#);
    let op = bundle.operation("createAlias").unwrap();
    let engine = Engine::new(None, EngineConfig::default());
    for _ in 0..2 {
        let out = engine
            .refuse_held(&bundle, op, &call(), 422, ErrorCause::Taken, &taken())
            .await
            .unwrap();
        assert_eq!((out.status, out.source), (422, Source::State));
        let body = out.body.unwrap();
        assert_eq!(body["errors"][0]["status"], "422", "{body}");
        assert_eq!(
            body["errors"][0]["detail"], "alias 'welcome-1' is already in use",
            "{body}"
        );
        assert_eq!(body["errors"][0]["source"]["pointer"], "/alias", "{body}");
    }
}

/// A free text member nothing else fills (`title`) has the model write the body, once
/// per status and cause, though the schema has no code; its status member still says
/// the refusal's status.
#[tokio::test]
async fn a_free_text_member_has_the_model_write_the_body_once() {
    let bundle = json_api(r#"{type: string, examples: ["422"]}"#);
    let op = bundle.operation("createAlias").unwrap();
    let written = json!({"errors": [{
        "status": "409",
        "title": "Alias already taken",
        "detail": "x",
        "source": {"pointer": "/x"}
    }]});
    let (provider, engine) = scripted(vec![Ok(written.to_string())]);
    for _ in 0..2 {
        let out = engine
            .refuse_held(&bundle, op, &call(), 422, ErrorCause::Taken, &taken())
            .await
            .unwrap();
        assert_eq!((out.status, out.source), (422, Source::State));
        let body = out.body.unwrap();
        let error = &body["errors"][0];
        assert_eq!(error["title"], "Alias already taken", "{body}");
        assert_eq!(error["status"], "422", "{body}");
        assert_eq!(error["detail"], "alias 'welcome-1' is already in use");
        assert_eq!(error["source"]["pointer"], "/alias", "{body}");
    }
    assert_eq!(provider.requests().len(), 1);
}

/// A status member whose examples are words is the API's own: left as it is.
#[tokio::test]
async fn a_status_member_with_word_examples_is_left_alone() {
    let bundle = json_api(r#"{type: string, examples: ["fail", "error"]}"#);
    let op = bundle.operation("createAlias").unwrap();
    let engine = Engine::new(None, EngineConfig::default());
    let out = engine
        .refuse_held(&bundle, op, &call(), 422, ErrorCause::Taken, &taken())
        .await
        .unwrap();
    let body = out.body.unwrap();
    assert_ne!(body["errors"][0]["status"], "422", "{body}");
}

/// A status member whose pattern takes 3 digits holds the status too; one with an
/// enum of codes that names it does as well.
#[tokio::test]
async fn a_status_member_patterned_or_enumerated_as_codes_says_the_status() {
    for status in [
        r#"{type: string, pattern: "^[1-5][0-9]{2}$"}"#,
        r#"{type: string, enum: ["404", "422"]}"#,
    ] {
        let bundle = json_api(status);
        let op = bundle.operation("createAlias").unwrap();
        let engine = Engine::new(None, EngineConfig::default());
        let out = engine
            .refuse_held(&bundle, op, &call(), 422, ErrorCause::Taken, &taken())
            .await
            .unwrap();
        let body = out.body.unwrap();
        assert_eq!(body["errors"][0]["status"], "422", "{status}: {body}");
    }
}

/// A body whose members are all filled by the facts or fixed (a message and a
/// pointer, a status text) asks no model.
#[tokio::test]
async fn a_body_the_facts_fill_asks_no_model() {
    let bundle = refusing_with(
        "{type: object, properties: {errors: {type: array, minItems: 1, items: {type: object, properties: {status: {type: string, examples: [\"422\"]}, detail: {type: string}, kind: {type: string, enum: [conflict, other]}, source: {type: object, properties: {pointer: {type: string}}}}}}}}",
    );
    let op = bundle.operation("createAlias").unwrap();
    let (provider, engine) = scripted(vec![]);
    let out = engine
        .refuse_held(&bundle, op, &call(), 422, ErrorCause::Taken, &taken())
        .await
        .unwrap();
    assert_eq!(out.body.unwrap()["errors"][0]["status"], "422");
    assert!(provider.requests().is_empty());
}

/// `POST /aliases` refusing with `422` described as `description`, whose body is
/// `error` (YAML).
fn refusing_described(description: &str, error: &str) -> Bundle {
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
        "422": {{description: "{description}", content: {{application/json: {{schema: {error}}}}}}}
"##
    );
    compile(src.as_bytes()).unwrap()
}

/// A text member the contract gives a value for (its example, in a schema no other
/// status uses, or the value the response's description names for it) is no free text:
/// the model writes no body, and the member keeps the contract's value.
#[tokio::test]
async fn a_text_member_the_contract_names_is_not_written() {
    let item = |title: &str| {
        format!(
            "{{type: object, properties: {{errors: {{type: array, minItems: 1, items: {{type: object, required: [title], properties: {{title: {title}, detail: {{type: string}}, source: {{type: object, properties: {{pointer: {{type: string}}}}}}}}}}}}}}}}"
        )
    };
    for bundle in [
        refusing_with(&item("{type: string, examples: [Conflict]}")),
        refusing_described("Refused: `title` is `Conflict`.", &item("{type: string}")),
    ] {
        let op = bundle.operation("createAlias").unwrap();
        let (provider, engine) = scripted(vec![]);
        let out = engine
            .refuse_held(&bundle, op, &call(), 422, ErrorCause::Taken, &taken())
            .await
            .unwrap();
        let body = out.body.unwrap();
        assert_eq!(body["errors"][0]["title"], "Conflict", "{body}");
        assert_eq!(
            body["errors"][0]["detail"], "alias 'welcome-1' is already in use",
            "{body}"
        );
        assert!(provider.requests().is_empty());
    }
}
