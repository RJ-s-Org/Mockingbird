//! [`Engine::fill`]: the members of a stored record that nothing else decides.

use mb_engine::{FillContext, OperationCall};
use mb_import::compile;
use mb_llm::ResponseFormat;
use serde_json::{Map, Value, json};

use crate::support::scripted;

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /intents:
    post:
      operationId: createIntent
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Intent"}
components:
  schemas:
    Intent:
      type: object
      required: [id, status, description, next_action]
      properties:
        id: {type: string}
        status: {type: string}
        description: {type: string, nullable: true}
        next_action:
          nullable: true
          allOf: [{$ref: "#/components/schemas/Action"}]
    Action:
      type: object
      properties:
        type: {type: string}
"##;

/// An object member that may be null and that nothing set is stored as null and
/// not asked of the model; a nullable scalar and the others are asked.
#[tokio::test]
async fn a_nullable_member_nothing_set_is_null_and_not_asked() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (provider, engine) = scripted(vec![Ok(
        json!({"status": "requires_payment_method", "description": "Order 42"}).to_string(),
    )]);
    let call = OperationCall {
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: Some(json!({})),
    };
    let known: Map<String, Value> = json!({"id": "pi_1"}).as_object().cloned().unwrap();
    let missing: Vec<String> = ["status", "description", "next_action"]
        .map(String::from)
        .to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &known,
            &missing,
            FillContext { op, call: &call },
        )
        .await;
    assert_eq!(filled.members["status"], "requires_payment_method");
    assert_eq!(filled.members["description"], "Order 42");
    assert_eq!(filled.members["next_action"], Value::Null);
    let sent = provider.requests();
    let ResponseFormat::JsonSchema { schema, .. } = &sent[0].response_format else {
        panic!("no schema sent");
    };
    let mut asked: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    asked.sort();
    assert_eq!(asked, vec!["description", "status"]);
}

/// With every missing member a nullable object, no model call is made.
#[tokio::test]
async fn only_nullable_members_ask_no_model() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (provider, engine) = scripted(vec![]);
    let call = OperationCall {
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: None,
    };
    let missing: Vec<String> = ["next_action"].map(String::from).to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext { op, call: &call },
        )
        .await;
    assert_eq!(filled.members["next_action"], Value::Null);
    assert!(provider.requests().is_empty());
}
