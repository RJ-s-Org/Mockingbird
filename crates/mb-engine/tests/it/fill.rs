//! [`Engine::fill`]: the members of a stored record that nothing else decides.

use mb_engine::{FillContext, OperationCall};
use mb_import::compile;
use mb_llm::{LlmError, ResponseFormat};
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
      required: [id, status, description, next_action, latest_charge]
      properties:
        id: {type: string}
        status: {type: string}
        description: {type: string, nullable: true}
        next_action:
          nullable: true
          allOf: [{$ref: "#/components/schemas/Action"}]
        latest_charge:
          nullable: true
          anyOf:
            - {type: string}
            - {$ref: "#/components/schemas/Charge"}
        shipping:
          nullable: true
          allOf: [{$ref: "#/components/schemas/Action"}]
        discounts: {type: array, nullable: true, items: {type: string}}
        metadata: {type: object, nullable: true}
        receipt_email: {type: string, nullable: true}
        branding:
          allOf: [{$ref: "#/components/schemas/Action"}]
        excluded: {type: array, items: {$ref: "#/components/schemas/Action"}}
        method_types: {type: array, items: {type: string}}
        items:
          type: object
          properties:
            object: {type: string}
            data: {type: array, items: {$ref: "#/components/schemas/Action"}}
    Action:
      type: object
      required: [type]
      properties:
        type: {type: string}
    Charge:
      type: object
      properties:
        id: {type: string}
"##;

/// A nullable reference to another record that nothing set is null and not asked
/// of the model, and so are optional nullable objects and lists; a required nullable
/// object, the scalars (nullable or not) and the other members are asked, and the
/// model's null for the object is kept (no faker object a client would act on).
#[tokio::test]
async fn a_nullable_reference_or_optional_object_nothing_set_is_null_and_not_asked() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (provider, engine) = scripted(vec![Ok(
        json!({"status": "requires_payment_method", "description": "Order 42", "next_action": null, "receipt_email": null})
            .to_string(),
    )]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: Some(json!({})),
    };
    let known: Map<String, Value> = json!({"id": "pi_1"}).as_object().cloned().unwrap();
    let missing: Vec<String> = [
        "status",
        "description",
        "next_action",
        "latest_charge",
        "shipping",
        "discounts",
        "metadata",
        "receipt_email",
    ]
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
    assert_eq!(filled.members["latest_charge"], Value::Null);
    assert_eq!(filled.members["next_action"], Value::Null);
    for optional in ["shipping", "discounts", "metadata"] {
        assert_eq!(filled.members[optional], Value::Null, "{optional}");
    }
    assert_eq!(filled.members["receipt_email"], Value::Null);
    let sent = provider.requests();
    let ResponseFormat::JsonSchema { schema, .. } = &sent[0].response_format else {
        panic!("no schema sent");
    };
    let mut asked: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    asked.sort();
    assert_eq!(
        asked,
        vec!["description", "next_action", "receipt_email", "status"]
    );
}

/// With only nullable references and optional nullable objects missing, no model
/// call is made.
#[tokio::test]
async fn only_nullable_references_and_optional_objects_ask_no_model() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (provider, engine) = scripted(vec![]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: None,
    };
    let missing: Vec<String> = ["latest_charge", "shipping", "discounts"]
        .map(String::from)
        .to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext { op, call: &call },
        )
        .await;
    for member in ["latest_charge", "shipping", "discounts"] {
        assert_eq!(filled.members[member], Value::Null, "{member}");
    }
    assert!(provider.requests().is_empty());
}

/// An optional object or list of objects that may not be null is left out: not
/// asked, and no faker value either. A list of scalars is still asked.
#[tokio::test]
async fn an_optional_object_that_may_not_be_null_is_left_out() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (provider, engine) = scripted(vec![Ok(
        json!({"status": "open", "method_types": ["card"]}).to_string(),
    )]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: None,
    };
    let missing: Vec<String> = ["status", "branding", "excluded", "method_types"]
        .map(String::from)
        .to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext { op, call: &call },
        )
        .await;
    assert_eq!(filled.members["status"], "open");
    assert!(
        !filled.members.contains_key("branding"),
        "{:?}",
        filled.members
    );
    assert!(
        !filled.members.contains_key("excluded"),
        "{:?}",
        filled.members
    );
    let sent = provider.requests();
    let ResponseFormat::JsonSchema { schema, .. } = &sent[0].response_format else {
        panic!("no schema sent");
    };
    assert_eq!(filled.members["method_types"], json!(["card"]));
    let mut asked: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    asked.sort();
    assert_eq!(asked, vec!["method_types", "status"]);
}

/// An optional object that may not be null is asked of the model when the request
/// sent a member of its name (the model carries the sent items over); one the
/// request did not send stays left out.
#[tokio::test]
async fn an_optional_object_the_request_sent_is_asked() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let items = json!({"object": "list", "data": [{"type": "card"}]});
    let (provider, engine) = scripted(vec![Ok(
        json!({"status": "open", "items": items}).to_string()
    )]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: Some(json!({"items": [{"type": "card"}]})),
    };
    let missing: Vec<String> = ["status", "items", "branding"].map(String::from).to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext { op, call: &call },
        )
        .await;
    assert_eq!(filled.members["items"], items);
    assert!(
        !filled.members.contains_key("branding"),
        "{:?}",
        filled.members
    );
    let sent = provider.requests();
    let ResponseFormat::JsonSchema { schema, .. } = &sent[0].response_format else {
        panic!("no schema sent");
    };
    let mut asked: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    asked.sort();
    assert_eq!(asked, vec!["items", "status"]);
}

/// When the model call fails, an optional object the request sent is absent, not
/// faker items standing for what the client sent.
#[tokio::test]
async fn a_failed_call_leaves_a_sent_optional_object_absent() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (_provider, engine) = scripted(vec![Err(LlmError::Transport("down".into()))]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: Some(json!({"items": [{"type": "card"}]})),
    };
    let missing: Vec<String> = ["status", "items"].map(String::from).to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext { op, call: &call },
        )
        .await;
    assert!(
        filled.members.contains_key("status"),
        "{:?}",
        filled.members
    );
    assert!(
        !filled.members.contains_key("items"),
        "{:?}",
        filled.members
    );
}

/// When the model call fails, a required object that may be null is null, not a
/// faker object a client would act on; a scalar is still the faker's.
#[tokio::test]
async fn a_failed_call_leaves_a_nullable_object_null() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (_provider, engine) = scripted(vec![Err(LlmError::Transport("down".into()))]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: None,
    };
    let missing: Vec<String> = ["status", "next_action"].map(String::from).to_vec();
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
    assert!(filled.members["status"].is_string(), "{:?}", filled.members);
}
