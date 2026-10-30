use mb_engine::{
    Engine, EngineConfig, EngineError, GenerationContext, JointPart, OperationCall,
    SchemaValidator, Source,
};
use mb_import::compile;
use std::sync::Arc;

use mb_llm::{Capabilities, LlmError, Provider, Role, ScriptedProvider};
use mb_mcm::Bundle;
use mb_state::StoredResource;
use serde_json::{Map, Value, json};

use crate::support::{orders, scripted, valid_order};

fn validator(bundle: &Bundle, def: &str) -> SchemaValidator {
    SchemaValidator::new(&bundle.with_defs(&json!({"$ref": format!("#/$defs/{def}")}))).unwrap()
}

/// `valid_order()` as served for path id `o1`: the identity field follows
/// the path (spec §1.2, criterion 4).
fn valid_order_at_o1() -> Value {
    let mut v = valid_order();
    v["id"] = json!("o1");
    v
}

fn call(op: &str) -> OperationCall {
    let mut path_params = Map::new();
    path_params.insert("orderId".into(), json!("o1"));
    OperationCall {
        operation_id: op.into(),
        path_params,
        query: Map::new(),
        headers: Map::new(),
        body: None,
    }
}

fn ctx() -> GenerationContext<'static> {
    GenerationContext {
        related: &[],
        target: None,
        forced_status: None,
        scenario: None,
        response_schema: None,
    }
}

#[tokio::test]
async fn valid_llm_output_is_llm_source() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (provider, engine) = scripted(vec![Ok(valid_order().to_string())]);
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Llm);
    assert_eq!(out.status, 200);
    assert_eq!(out.body, Some(valid_order_at_o1()));
    assert_eq!(out.source.to_string(), "llm");
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn invalid_then_repaired() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (provider, engine) = scripted(vec![
        Ok(json!({"id": "x"}).to_string()),
        Ok(valid_order().to_string()),
    ]);
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Repaired);
    assert_eq!(out.source.to_string(), "repaired");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let last = requests[1].messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(last.content.contains("Errors:"), "{}", last.content);
    assert!(last.content.contains("total"), "{}", last.content);
    let prior = &requests[1].messages[requests[1].messages.len() - 2];
    assert_eq!(prior.role, Role::Assistant);
    assert_eq!(prior.content, json!({"id": "x"}).to_string());
}

#[tokio::test]
async fn invalid_twice_falls_back() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (provider, engine) = scripted(vec![
        Ok(json!({"id": "x"}).to_string()),
        Ok("not json at all".into()),
    ]);
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Fallback);
    assert_eq!(out.source.to_string(), "fallback");
    assert_eq!(provider.requests().len(), 2);
    validator(&bundle, "Order")
        .validate(out.body.as_ref().unwrap())
        .unwrap();
}

#[tokio::test]
async fn provider_error_falls_back() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (_provider, engine) = scripted(vec![Err(LlmError::Timeout)]);
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Fallback);
    assert_eq!(out.status, 200);
    validator(&bundle, "Order")
        .validate(out.body.as_ref().unwrap())
        .unwrap();
}

#[tokio::test]
async fn extracts_json_from_fenced_output() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let text = format!("Here you go:\n```json\n{:#}\n```", valid_order());
    let (_provider, engine) = scripted(vec![Ok(text)]);
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Llm);
    assert_eq!(out.body, Some(valid_order_at_o1()));
}

#[tokio::test]
async fn no_content_skips_llm() {
    let bundle = orders();
    let op = bundle.operation("deleteOrder").unwrap();
    let (provider, engine) = scripted(vec![Ok(valid_order().to_string())]);
    let out = engine
        .generate(&bundle, op, &call("deleteOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.status, 204);
    assert_eq!(out.body, None);
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn forced_status_uses_error_variant() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (provider, engine) = scripted(vec![Ok(
        json!({"code": 404, "message": "order not found"}).to_string()
    )]);
    let out = engine
        .generate(
            &bundle,
            op,
            &call("getOrder"),
            GenerationContext {
                forced_status: Some(404),
                scenario: None,
                ..ctx()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.status, 404);
    assert_eq!(out.source, Source::Llm);
    assert_eq!(
        out.body,
        Some(json!({"code": 404, "message": "order not found"}))
    );
    // Valid on the first answer: no repair call.
    assert_eq!(provider.requests().len(), 1);
    let system = &provider.requests()[0].messages[0].content;
    assert!(system.contains("404"), "{system}");
}

#[tokio::test]
async fn undeclared_forced_status_errors() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (provider, engine) = scripted(vec![]);
    let err = engine
        .generate(
            &bundle,
            op,
            &call("getOrder"),
            GenerationContext {
                forced_status: Some(418),
                scenario: None,
                ..ctx()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::UndeclaredStatus(418)), "{err:?}");
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn target_overlay_keeps_stored_fields() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let target = StoredResource {
        resource_type: "orders".into(),
        id: "o1".into(),
        data: json!({"id": "o1", "customerId": "c9", "status": "shipped", "total": 5}),
    };
    let (_provider, engine) = scripted(vec![Ok(valid_order().to_string())]);
    let out = engine
        .generate(
            &bundle,
            op,
            &call("getOrder"),
            GenerationContext {
                target: Some(&target),
                ..ctx()
            },
        )
        .await
        .unwrap();
    let body = out.body.unwrap();
    assert_eq!(body["id"], "o1");
    assert_eq!(body["customerId"], "c9");
    assert_eq!(body["status"], "shipped");
    validator(&bundle, "Order").validate(&body).unwrap();
}

#[tokio::test]
async fn prompt_contains_request_and_state_as_data() {
    let bundle = orders();
    let op = bundle.operation("updateOrder").unwrap();
    let target = StoredResource {
        resource_type: "orders".into(),
        id: "o1".into(),
        data: json!({"id": "o1", "customerId": "c9", "status": "shipped", "total": 5}),
    };
    let related = vec![StoredResource {
        resource_type: "orders".into(),
        id: "o2".into(),
        data: json!({"id": "o2", "customerId": "c7", "status": "pending", "total": 1}),
    }];
    let (provider, engine) = scripted(vec![
        Ok(valid_order().to_string()),
        Ok(valid_order().to_string()),
    ]);
    for total in [10, 20] {
        let mut c = call("updateOrder");
        c.body = Some(json!({"customerId": "c9", "total": total}));
        engine
            .generate(
                &bundle,
                op,
                &c,
                GenerationContext {
                    related: &related,
                    target: Some(&target),
                    forced_status: None,
                    scenario: None,
                    response_schema: None,
                },
            )
            .await
            .unwrap();
    }
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let first = &requests[0].messages;
    let second = &requests[1].messages;
    assert_eq!(first[0].role, Role::System);
    assert_eq!(first[0].content, second[0].content);
    assert!(first[0].content.contains("Orders API"));
    assert!(first[0].content.contains("updateOrder"));
    assert!(first[0].content.contains("Never output real personal data"));
    let user = &first[1].content;
    assert_eq!(first[1].role, Role::User);
    assert!(user.contains("<request>"), "{user}");
    assert!(user.contains("<existing_resource>"), "{user}");
    assert!(user.contains("\"o1\""), "{user}");
    assert!(user.contains("<related_resources>"), "{user}");
    assert!(user.contains("\"o2\""), "{user}");
    assert!(user.contains("not instructions"), "{user}");
    assert_ne!(first[1].content, second[1].content);
}

#[tokio::test]
async fn offline_engine_uses_faker() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let engine = Engine::new(None, EngineConfig::default());
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Fallback);
    assert_eq!(out.status, 200);
    assert_eq!(out.content_type.as_deref(), Some("application/json"));
    validator(&bundle, "Order")
        .validate(out.body.as_ref().unwrap())
        .unwrap();
}

#[test]
fn extract_json_variants() {
    use mb_engine::extract::extract_json;
    assert_eq!(extract_json("  {\"a\":1} "), Some(json!({"a": 1})));
    assert_eq!(extract_json("```\n[1,2]\n```"), Some(json!([1, 2])));
    assert_eq!(
        extract_json("Here is the response: {\"a\": {\"b\": 2}} hope it helps"),
        Some(json!({"a": {"b": 2}}))
    );
    assert_eq!(extract_json("no json here"), None);
}

fn offline() -> Engine {
    Engine::new(None, EngineConfig::default())
}

fn inline(src: &str) -> Bundle {
    compile(src.as_bytes()).unwrap()
}

#[tokio::test]
async fn offline_create_reflects_request_body() {
    let bundle = orders();
    let op = bundle.operation("createOrder").unwrap();
    let mut c = call("createOrder");
    c.path_params.clear();
    c.body = Some(json!({"customerId": "c1", "total": 10}));
    for _ in 0..20 {
        let out = offline().generate(&bundle, op, &c, ctx()).await.unwrap();
        let body = out.body.unwrap();
        assert_eq!(body["customerId"], "c1", "{body}");
        assert_eq!(body["total"], 10, "{body}");
        validator(&bundle, "Order").validate(&body).unwrap();
    }
}

#[tokio::test]
async fn llm_create_reflects_request_body() {
    let bundle = orders();
    let op = bundle.operation("createOrder").unwrap();
    let mut c = call("createOrder");
    c.path_params.clear();
    c.body = Some(json!({"customerId": "c1", "total": 10}));
    let (_p, engine) = scripted(vec![Ok(json!({
        "id": THING_ID, "customerId": "someone-else", "status": "pending", "total": 99
    })
    .to_string())]);
    let out = engine.generate(&bundle, op, &c, ctx()).await.unwrap();
    assert_eq!(out.source, Source::Llm);
    let body = out.body.unwrap();
    assert_eq!(body["id"], THING_ID);
    assert_eq!(body["customerId"], "c1");
    assert_eq!(body["total"], 10);
}

/// Only request members the response schema declares are copied into the
/// answer: Twilio's `To`/`Body` beside the schema's `to`/`body`, or a
/// credential sent in the body, never reach the response.
#[tokio::test]
async fn create_echoes_only_declared_request_fields() {
    let bundle = orders();
    let op = bundle.operation("createOrder").unwrap();
    let mut c = call("createOrder");
    c.path_params.clear();
    c.body = Some(json!({
        "customerId": "c1", "total": 10, "note": "leave at the door",
        "secret": "sk_test_123", "Body": "hi"
    }));
    let (_p, engine) = scripted(vec![Ok(valid_order().to_string())]);
    let out = engine.generate(&bundle, op, &c, ctx()).await.unwrap();
    let body = out.body.unwrap();
    assert_eq!(body["customerId"], "c1", "{body}");
    assert_eq!(body["note"], "leave at the door", "{body}");
    assert!(body.get("secret").is_none(), "{body}");
    assert!(body.get("Body").is_none(), "{body}");
    let out = offline().generate(&bundle, op, &c, ctx()).await.unwrap();
    let body = out.body.unwrap();
    assert_eq!(body["customerId"], "c1", "{body}");
    assert!(body.get("secret").is_none(), "{body}");
}

#[tokio::test]
async fn update_keeps_identity_and_applies_body() {
    let bundle = orders();
    let op = bundle.operation("updateOrder").unwrap();
    let target = StoredResource {
        resource_type: "orders".into(),
        id: "o1".into(),
        data: json!({"id": "o1", "customerId": "c9", "status": "shipped", "total": 5}),
    };
    let mut c = call("updateOrder");
    c.body = Some(json!({"customerId": "c2", "total": 7}));
    let context = GenerationContext {
        target: Some(&target),
        ..ctx()
    };
    // Fallback path.
    let out = offline().generate(&bundle, op, &c, context).await.unwrap();
    let body = out.body.unwrap();
    assert_eq!(body["id"], "o1", "{body}");
    assert_eq!(body["status"], "shipped", "{body}");
    assert_eq!(body["customerId"], "c2", "{body}");
    assert_eq!(body["total"], 7, "{body}");
    // LLM path: an LLM that invents a new id is corrected.
    let (_p, engine) = scripted(vec![Ok(valid_order().to_string())]);
    let out = engine.generate(&bundle, op, &c, context).await.unwrap();
    let body = out.body.unwrap();
    assert_eq!(body["id"], "o1", "{body}");
    assert_eq!(body["customerId"], "c2", "{body}");
}

#[tokio::test]
async fn get_without_state_uses_path_id() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let mut c = call("getOrder");
    c.path_params.insert("orderId".into(), json!("abc"));
    let out = offline().generate(&bundle, op, &c, ctx()).await.unwrap();
    assert_eq!(out.body.unwrap()["id"], "abc");
    let (_p, engine) = scripted(vec![Ok(valid_order().to_string())]);
    let out = engine.generate(&bundle, op, &c, ctx()).await.unwrap();
    assert_eq!(out.body.unwrap()["id"], "abc");
}

#[tokio::test]
async fn integer_identity_is_forced_as_number() {
    let bundle = inline(
        r#"
openapi: 3.1.0
info: {title: T, version: "1"}
paths:
  /items/{itemId}:
    get:
      operationId: getItem
      parameters: [{name: itemId, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema:
                type: object
                required: [id]
                properties: {id: {type: integer}}
"#,
    );
    let op = bundle.operation("getItem").unwrap();
    let mut c = call("getItem");
    c.path_params.clear();
    c.path_params.insert("itemId".into(), json!("42"));
    let out = offline().generate(&bundle, op, &c, ctx()).await.unwrap();
    assert_eq!(out.body.unwrap()["id"], json!(42));
}

const PET_CONTRACT: &str = r##"
openapi: 3.1.0
info: {title: Pets, version: "1"}
paths:
  /pet:
    get:
      operationId: getPet
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Pet"}
  /n:
    get:
      operationId: getN
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema:
                oneOf:
                  - {type: integer, minimum: 0, maximum: 10}
                  - {type: integer, minimum: 5, maximum: 100}
components:
  schemas:
    Pet:
      type: object
      required: [petType, name]
      properties:
        petType: {type: string}
        name: {type: string}
      oneOf:
        - $ref: "#/components/schemas/Cat"
        - $ref: "#/components/schemas/Dog"
    Cat:
      type: object
      required: [petType, name]
      properties:
        petType: {const: cat}
        name: {type: string}
    Dog:
      type: object
      required: [petType, name]
      properties:
        petType: {type: string, enum: [dog]}
        name: {type: string}
"##;

#[tokio::test]
async fn fallback_output_validates_for_one_of() {
    let bundle = inline(PET_CONTRACT);
    for (id, def) in [
        ("getPet", json!({"$ref": "#/$defs/Pet"})),
        ("getN", Value::Null),
    ] {
        let op = bundle.operation(id).unwrap();
        let schema = if def.is_null() {
            op.success_output().unwrap().schema.clone().unwrap()
        } else {
            def
        };
        let v = SchemaValidator::new(&bundle.with_defs(&schema)).unwrap();
        for _ in 0..50 {
            let out = offline()
                .generate(&bundle, op, &call(id), ctx())
                .await
                .unwrap();
            let body = out.body.unwrap();
            assert!(
                v.validate(&body).is_ok(),
                "{id}: {body} {:?}",
                v.validate(&body)
            );
        }
    }
}

#[tokio::test]
async fn prompt_and_format_carry_only_reachable_defs() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (provider, engine) = scripted(vec![Ok(valid_order().to_string())]);
    engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    let req = &provider.requests()[0];
    let system = &req.messages[0].content;
    assert!(system.contains("\"Category\""), "{system}");
    assert!(!system.contains("\"NewOrder\""), "{system}");
    match &req.response_format {
        mb_llm::ResponseFormat::JsonSchema { schema, .. } => {
            let defs = schema["$defs"].as_object().unwrap();
            assert!(defs.contains_key("Order") && defs.contains_key("Category"));
            assert!(!defs.contains_key("NewOrder") && !defs.contains_key("Error"));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn prompt_truncates_large_request_body() {
    let bundle = orders();
    let op = bundle.operation("createOrder").unwrap();
    let (provider, engine) = scripted(vec![Ok(valid_order().to_string())]);
    let mut c = call("createOrder");
    c.body = Some(json!({"customerId": "é".repeat(20_000), "total": 1}));
    engine.generate(&bundle, op, &c, ctx()).await.unwrap();
    let user = &provider.requests()[0].messages[1].content;
    assert!(user.contains("[truncated]"), "{}", &user[..200]);
    assert!(user.len() < 10 * 1024, "{}", user.len());
}

#[tokio::test]
async fn response_format_name_is_sanitized() {
    let bundle = orders();
    let mut op = bundle.operation("getOrder").unwrap().clone();
    op.id = format!("get order/{{id}}.v2!{}", "x".repeat(100));
    let (provider, engine) = scripted(vec![Ok(valid_order().to_string())]);
    engine
        .generate(&bundle, &op, &call("getOrder"), ctx())
        .await
        .unwrap();
    match &provider.requests()[0].response_format {
        mb_llm::ResponseFormat::JsonSchema { name, .. } => {
            assert!(!name.is_empty() && name.len() <= 64, "{name}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{name}"
            );
            assert!(name.starts_with("get_order__id__v2_"), "{name}");
        }
        other => panic!("{other:?}"),
    }
}

const ITEM_CONTRACT: &str = r#"
openapi: 3.1.0
info: {title: Items, version: "1"}
paths:
  /item:
    get:
      operationId: getItem
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema:
                type: object
                required: [id]
                properties: {id: {type: string}, name: {type: string}}
"#;

#[tokio::test]
async fn force_identity_uses_input_field() {
    let mut bundle = inline(ITEM_CONTRACT);
    bundle.operations[0].identity = Some(mb_mcm::Identity {
        path_param: None,
        field: "id".into(),
        input_field: Some("id".into()),
        path_field: None,
    });
    let op = bundle.operation("getItem").unwrap();
    let mut c = call("getItem");
    c.path_params.clear();
    c.body = Some(json!({"id": "42"}));
    for _ in 0..5 {
        let out = offline().generate(&bundle, op, &c, ctx()).await.unwrap();
        assert_eq!(out.body.unwrap()["id"], "42");
    }
}

#[tokio::test]
async fn prompt_names_grpc_binding() {
    let mut bundle = inline(ITEM_CONTRACT);
    bundle.operations[0].protocol = mb_mcm::Protocol::Grpc;
    bundle.operations[0].binding = mb_mcm::Binding::Grpc {
        package: "helloworld".into(),
        service: "Greeter".into(),
        method: "SayHello".into(),
    };
    bundle.operations[0].kind = mb_mcm::OperationKind::StreamServer;
    let op = bundle.operation("getItem").unwrap();
    let (provider, engine) = scripted(vec![Ok(json!({"id": "x"}).to_string())]);
    let mut c = call("getItem");
    c.path_params.clear();
    engine.generate(&bundle, op, &c, ctx()).await.unwrap();
    let system = &provider.requests()[0].messages[0].content;
    assert!(
        system.contains("gRPC: /helloworld.Greeter/SayHello (server streaming)\n"),
        "{system}"
    );
    assert!(!system.contains("HTTP:"), "{system}");
}

/// JSON-object output mode makes models wrap a top-level array in an object
/// (`{"orders": [...]}`); the array inside is what the operation returns.
#[tokio::test]
async fn array_wrapped_in_an_object_is_unwrapped() {
    let bundle = orders();
    let op = bundle.operation("listOrders").unwrap();
    let (provider, engine) = scripted(vec![Ok(json!({"orders": [valid_order()]}).to_string())]);
    let out = engine
        .generate(&bundle, op, &call("listOrders"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Llm);
    assert_eq!(out.body, Some(json!([valid_order()])));
    assert_eq!(provider.requests().len(), 1, "no repair call");
}

/// Asked for a list, models sometimes answer one item: it is the list's
/// only item, not a reason to repair.
#[tokio::test]
async fn a_single_item_for_a_list_is_wrapped() {
    let bundle = orders();
    let op = bundle.operation("listOrders").unwrap();
    let (provider, engine) = scripted(vec![Ok(valid_order().to_string())]);
    let out = engine
        .generate(&bundle, op, &call("listOrders"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Llm);
    assert_eq!(out.body, Some(json!([valid_order()])));
    assert_eq!(provider.requests().len(), 1, "no repair call");
}

#[tokio::test]
async fn fallback_records_its_reason() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (_provider, engine) = scripted(vec![Err(LlmError::Timeout)]);
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Fallback);
    let reason = out.fallback_reason.unwrap();
    assert!(reason.contains("timed out"), "{reason}");
}

#[tokio::test]
async fn llm_answer_has_no_fallback_reason() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (_provider, engine) = scripted(vec![Ok(valid_order().to_string())]);
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.fallback_reason, None);
}

/// A GraphQL selection set narrows the success schema: the prompt, the
/// validation and the faker all use the narrowed schema.
fn narrowed() -> Value {
    json!({
        "type": "object",
        "properties": {"id": {"type": "string"}},
        "required": ["id"],
        "additionalProperties": false
    })
}

#[tokio::test]
async fn narrowed_schema_replaces_the_operations_schema() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let schema = narrowed();
    let (provider, engine) = scripted(vec![Ok(json!({"id": "o1"}).to_string())]);
    let out = engine
        .generate(
            &bundle,
            op,
            &call("getOrder"),
            GenerationContext {
                response_schema: Some(&schema),
                ..ctx()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.source, Source::Llm, "{:?}", out.fallback_reason);
    assert_eq!(out.body, Some(json!({"id": "o1"})));
    let system = &provider.requests()[0].messages[0].content;
    assert!(!system.contains("customerId"), "{system}");
}

#[tokio::test]
async fn narrowed_schema_shapes_the_fallback() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let schema = narrowed();
    let out = offline()
        .generate(
            &bundle,
            op,
            &call("getOrder"),
            GenerationContext {
                response_schema: Some(&schema),
                ..ctx()
            },
        )
        .await
        .unwrap();
    let body = out.body.unwrap();
    assert_eq!(body.as_object().unwrap().len(), 1, "{body}");
    assert!(body["id"].is_string());
}

#[tokio::test]
async fn prompt_schema_is_compact_json() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (provider, engine) = scripted(vec![Ok(valid_order_at_o1().to_string())]);
    engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    let system = &provider.requests()[0].messages[0].content;
    let schema = system.split("Response JSON Schema:\n").nth(1).unwrap();
    assert!(!schema.contains('\n'), "one line: {schema}");
    assert!(schema.contains(r#""type":"object""#), "{schema}");
}

#[tokio::test]
async fn prompt_asks_for_short_arrays() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (provider, engine) = scripted(vec![Ok(valid_order_at_o1().to_string())]);
    engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    let system = &provider.requests()[0].messages[0].content;
    assert!(system.contains("3 to 5 items"), "{system}");
}

/// Two root fields of one GraphQL query answered by one LLM call: one
/// object keyed by response key, split into one response per part.
#[tokio::test]
async fn joint_generation_answers_every_part_in_one_call() {
    let bundle = orders();
    let get = bundle.operation("getOrder").unwrap();
    let list = bundle.operation("listOrders").unwrap();
    let mut other = valid_order();
    other["id"] = json!(THING_ID);
    let (provider, engine) = scripted(vec![Ok(json!({
        "order": valid_order_at_o1(),
        "recent": [other.clone()],
    })
    .to_string())]);
    let parts = [
        JointPart {
            key: "order",
            op: get,
            call: &call("getOrder"),
            ctx: ctx(),
        },
        JointPart {
            key: "recent",
            op: list,
            call: &call("listOrders"),
            ctx: ctx(),
        },
    ];
    let out = engine.generate_joint(&bundle, &parts).await;
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].as_ref().unwrap().source, Source::Llm);
    assert_eq!(out[0].as_ref().unwrap().body, Some(valid_order_at_o1()));
    assert_eq!(out[1].as_ref().unwrap().body, Some(json!([other])));
    let system = &provider.requests()[0].messages[0].content;
    assert!(
        system.contains("`order`") && system.contains("`recent`"),
        "{system}"
    );
}

/// A joint answer that stays invalid after the repair falls back to one
/// call per part, not to the faker.
#[tokio::test]
async fn joint_generation_falls_back_to_separate_calls() {
    let bundle = orders();
    let get = bundle.operation("getOrder").unwrap();
    let list = bundle.operation("listOrders").unwrap();
    let (provider, engine) = scripted(vec![
        Ok(json!({"order": 1}).to_string()),
        Ok(json!({"order": 2}).to_string()),
        Ok(valid_order_at_o1().to_string()),
        Ok(json!([valid_order()]).to_string()),
    ]);
    let parts = [
        JointPart {
            key: "order",
            op: get,
            call: &call("getOrder"),
            ctx: ctx(),
        },
        JointPart {
            key: "recent",
            op: list,
            call: &call("listOrders"),
            ctx: ctx(),
        },
    ];
    let out = engine.generate_joint(&bundle, &parts).await;
    assert_eq!(provider.requests().len(), 4);
    assert!(
        out.iter()
            .all(|r| r.as_ref().unwrap().source == Source::Llm)
    );
}

/// Models asked for a GraphQL field answer like a GraphQL server:
/// `{"data": {"field": ...}}`; the value two levels down is the answer.
#[tokio::test]
async fn answer_wrapped_like_a_graphql_response_is_unwrapped() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (provider, engine) = scripted(vec![Ok(
        json!({"data": {"order": valid_order_at_o1()}}).to_string()
    )]);
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Llm);
    assert_eq!(out.body, Some(valid_order_at_o1()));
    assert_eq!(provider.requests().len(), 1, "no repair call");
}

/// Models answering like a GraphQL server also send `"errors": null`
/// beside `data` (seen on GitHub's `createIssue`).
#[tokio::test]
async fn graphql_envelope_with_null_errors_is_unwrapped() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (provider, engine) = scripted(vec![Ok(json!({
        "data": {"order": valid_order_at_o1()}, "errors": null, "extensions": null
    })
    .to_string())]);
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Llm);
    assert_eq!(out.body, Some(valid_order_at_o1()));
    assert_eq!(provider.requests().len(), 1, "no repair call");
}

/// A part whose schema allows null may be left out of the joint answer
/// (models drop the fields they mean as null), and the joint answer may come
/// wrapped in a GraphQL `data` envelope.
#[tokio::test]
async fn joint_answer_may_omit_nullable_parts() {
    let bundle = orders();
    let get = bundle.operation("getOrder").unwrap();
    let nullable = json!({"anyOf": [{"type": "object"}, {"type": "null"}]});
    let (provider, engine) = scripted(vec![Ok(
        json!({"data": {"order": valid_order_at_o1()}}).to_string()
    )]);
    let parts = [
        JointPart {
            key: "order",
            op: get,
            call: &call("getOrder"),
            ctx: ctx(),
        },
        JointPart {
            key: "maybe",
            op: get,
            call: &call("getOrder"),
            ctx: GenerationContext {
                response_schema: Some(&nullable),
                ..ctx()
            },
        },
    ];
    let out = engine.generate_joint(&bundle, &parts).await;
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(out[0].as_ref().unwrap().body, Some(valid_order_at_o1()));
    assert_eq!(out[1].as_ref().unwrap().body, Some(Value::Null));
    assert_eq!(out[1].as_ref().unwrap().source, Source::Llm);
}

/// A `null` the schema does not allow is patched locally instead of costing
/// a repair call: dropped where the property is optional, faked where it is
/// required. Real APIs return null where their docs never say so (TMDB's
/// `belongs_to_collection`), and models copy them.
#[tokio::test]
async fn disallowed_nulls_are_patched_without_a_repair_call() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let mut answer = valid_order_at_o1();
    answer["category"] = Value::Null;
    answer["customerId"] = Value::Null;
    let (provider, engine) = scripted(vec![Ok(answer.to_string())]);
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(provider.requests().len(), 1, "no repair call");
    assert_eq!(out.source, Source::Patched);
    let body = out.body.unwrap();
    assert!(body.get("category").is_none(), "{body}");
    assert!(body["customerId"].is_string(), "{body}");
    assert_eq!(body["total"], 42.5);
}

/// Anything else wrong still goes to the model for a repair.
#[tokio::test]
async fn nulls_with_other_errors_still_ask_for_a_repair() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let mut answer = valid_order_at_o1();
    answer["category"] = Value::Null;
    answer["total"] = json!("forty-two");
    let (provider, engine) = scripted(vec![
        Ok(answer.to_string()),
        Ok(valid_order_at_o1().to_string()),
    ]);
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(out.body, Some(valid_order_at_o1()));
}

#[test]
fn stored_resource_that_validates_is_served_as_state() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let out = offline()
        .serve_stored(&bundle, op, &valid_order_at_o1(), None)
        .unwrap();
    assert_eq!(out.status, 200);
    assert_eq!(out.source, Source::State);
    assert_eq!(out.source.to_string(), "state");
    assert_eq!(out.body, Some(valid_order_at_o1()));
    assert_eq!(Source::Cache.as_str(), "cache");
    assert_eq!(Source::State.worst(Source::Llm), Source::State);
    assert_eq!(Source::Cache.worst(Source::Fallback), Source::Fallback);
}

#[test]
fn partial_stored_resource_is_not_served() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let partial = json!({"id": "o1", "customerId": "c1"});
    assert!(
        offline()
            .serve_stored(&bundle, op, &partial, None)
            .is_none()
    );
}

#[test]
fn not_found_uses_the_declared_404_else_problem_json() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let out = offline().not_found(&bundle, op, &call("getOrder"), "orders o1 does not exist");
    assert_eq!(out.status, 404);
    assert_eq!(out.source, Source::State);
    let body = out.body.unwrap();
    assert!(
        body["code"].is_i64() && body["message"].is_string(),
        "{body}"
    );

    let bundle = inline(
        r#"
openapi: 3.1.0
info: {title: Pets, version: "1"}
paths:
  /pets/{petId}:
    get:
      operationId: getPet
      parameters: [{name: petId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: OK, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}
"#,
    );
    let op = bundle.operation("getPet").unwrap();
    let out = offline().not_found(
        &bundle,
        op,
        &OperationCall::default(),
        "pets p1 does not exist",
    );
    assert_eq!(
        out.content_type.as_deref(),
        Some("application/problem+json")
    );
    assert_eq!(out.body.unwrap()["detail"], "pets p1 does not exist");
}

#[tokio::test]
async fn duplicate_ids_in_a_list_are_made_unique() {
    let bundle = orders();
    let op = bundle.operation("listOrders").unwrap();
    let mut a2 = valid_order();
    a2["id"] = json!(DUP_ID);
    let mut a1 = a2.clone();
    a1["total"] = json!(1);
    let mut b = valid_order();
    b["id"] = json!(THING_ID);
    let (_, engine) = scripted(vec![Ok(json!([a1, a2, b]).to_string())]);
    let mut c = call("listOrders");
    c.path_params.clear();
    let out = engine.generate(&bundle, op, &c, ctx()).await.unwrap();
    let ids: Vec<&str> = out
        .body
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids[0], DUP_ID);
    assert_eq!(ids[2], THING_ID);
    assert!(ids[1] != DUP_ID && ids[1] != THING_ID, "{ids:?}");
}

#[test]
fn list_items_finds_arrays_and_envelopes() {
    use mb_engine::consistency::{list_identity, list_items};
    assert_eq!(list_items(&json!([{"id": 1}])).unwrap().len(), 1);
    assert_eq!(
        list_items(&json!({"object": "list", "data": [{"id": 1}, {"id": 2}]}))
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        list_items(&json!({"page": 1, "results": [{"id": 1}]}))
            .unwrap()
            .len(),
        1
    );
    assert!(list_items(&json!({"a": [{"id": 1}], "b": [{"id": 2}]})).is_none());
    let bundle = orders();
    assert_eq!(
        list_identity(&bundle, bundle.operation("listOrders").unwrap()),
        "id"
    );
}

#[test]
fn fresh_identity_changes_only_the_id() {
    let bundle = orders();
    let op = bundle.operation("createOrder").unwrap();
    let fresh = offline()
        .fresh_identity(&bundle, op, &valid_order())
        .unwrap();
    assert_ne!(fresh["id"], valid_order()["id"]);
    assert_eq!(fresh["customerId"], "c1");
    validator(&bundle, "Order").validate(&fresh).unwrap();
}

#[tokio::test]
async fn offline_answers_echo_path_parameters() {
    let bundle = inline(
        r#"
openapi: 3.1.0
info: {title: Users, version: "1"}
paths:
  /users/{login}:
    get:
      operationId: getUser
      parameters: [{name: login, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: OK, content: {application/json: {schema: {type: object, required: [login], properties: {login: {type: string}}}}}}
"#,
    );
    let op = bundle.operation("getUser").unwrap();
    let mut c = OperationCall::default();
    c.path_params.insert("login".into(), json!("octocat"));
    let out = offline().generate(&bundle, op, &c, ctx()).await.unwrap();
    assert_eq!(out.body.unwrap()["login"], "octocat");
}

#[tokio::test]
async fn pattern_errors_are_patched_without_a_repair_call() {
    let bundle = inline(
        r#"
openapi: 3.1.0
info: {title: Images, version: "1"}
paths:
  /images/{id}:
    get:
      operationId: getImage
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: OK, content: {application/json: {schema: {type: object, required: [file_path], properties: {file_path: {type: string, pattern: "^/[A-Za-z0-9]{22}\\.jpg$"}}}}}}
"#,
    );
    let op = bundle.operation("getImage").unwrap();
    let (provider, engine) = scripted(vec![Ok(json!({"file_path": "/poster.jpg"}).to_string())]);
    let out = engine
        .generate(&bundle, op, &OperationCall::default(), ctx())
        .await
        .unwrap();
    assert_eq!(provider.requests().len(), 1, "no repair call");
    assert_eq!(out.source, Source::Patched);
    let path = out.body.unwrap()["file_path"].as_str().unwrap().to_string();
    assert!(
        regex::Regex::new(r"^/[A-Za-z0-9]{22}\.jpg$")
            .unwrap()
            .is_match(&path),
        "{path}"
    );
}

/// A number outside `minimum`/`maximum` (a negative balance where the
/// schema says `minimum: 0`) is refilled locally like a pattern error.
#[tokio::test]
async fn range_errors_are_patched_without_a_repair_call() {
    let bundle = inline(
        r#"
openapi: 3.1.0
info: {title: Accounts, version: "1"}
paths:
  /accounts/{id}:
    get:
      operationId: getAccount
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: OK, content: {application/json: {schema: {type: object, required: [current, percent], properties: {current: {type: number, minimum: 0}, percent: {type: integer, maximum: 100}}}}}}
"#,
    );
    let op = bundle.operation("getAccount").unwrap();
    let (provider, engine) = scripted(vec![Ok(
        json!({"current": -342.87, "percent": 140}).to_string()
    )]);
    let out = engine
        .generate(&bundle, op, &OperationCall::default(), ctx())
        .await
        .unwrap();
    assert_eq!(provider.requests().len(), 1, "no repair call");
    assert_eq!(out.source, Source::Patched);
    let body = out.body.unwrap();
    assert!(body["current"].as_f64().unwrap() >= 0.0, "{body}");
    assert!(body["percent"].as_i64().unwrap() <= 100, "{body}");
}

#[tokio::test]
async fn prompt_asks_for_short_free_text() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (provider, engine) = scripted(vec![Ok(valid_order_at_o1().to_string())]);
    engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    let system = &provider.requests()[0].messages[0].content;
    assert!(system.contains("under 200 characters"), "{system}");
}

#[tokio::test]
async fn runaway_strings_are_truncated_unless_a_max_length_allows_them() {
    let bundle = inline(
        r#"
openapi: 3.1.0
info: {title: Places, version: "1"}
paths:
  /places/{id}:
    get:
      operationId: getPlace
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: OK, content: {application/json: {schema: {type: object, properties: {city: {type: string}, essay: {type: string, maxLength: 5000}}}}}}
"#,
    );
    let op = bundle.operation("getPlace").unwrap();
    let answer = json!({"city": "é".repeat(3000), "essay": "x".repeat(3000)});
    let (provider, engine) = scripted(vec![Ok(answer.to_string())]);
    let out = engine
        .generate(&bundle, op, &OperationCall::default(), ctx())
        .await
        .unwrap();
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(out.source, Source::Patched);
    let body = out.body.unwrap();
    assert_eq!(body["city"].as_str().unwrap().chars().count(), 200);
    assert_eq!(body["essay"].as_str().unwrap().len(), 3000);
}

#[test]
fn fresh_integer_identities_are_spread_out() {
    let bundle = inline(
        r#"
openapi: 3.1.0
info: {title: Items, version: "1"}
paths:
  /items:
    post:
      operationId: createItem
      responses:
        "201": {description: OK, content: {application/json: {schema: {type: object, required: [id], properties: {id: {type: integer}}}}}}
"#,
    );
    let op = bundle.operation("createItem").unwrap();
    let engine = offline();
    let big = (0..20)
        .filter_map(|_| engine.fresh_identity(&bundle, op, &json!({"id": 1})))
        .filter(|v| v["id"].as_i64().unwrap() > 100)
        .count();
    assert!(big >= 15, "{big} of 20 fresh ids above 100");
}

const CONTENTS: &str = r#"
openapi: 3.0.3
info: {title: contents, version: "1"}
paths:
  /readme:
    get:
      operationId: getReadme
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [name, encoding, content, avatar]
                properties:
                  name: {type: string}
                  encoding: {type: string}
                  content: {type: string}
                  avatar: {type: string, format: byte}
"#;

fn decode64(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    let compact: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(compact)
        .unwrap()
}

/// Base64 fields are asked for as text and encoded locally: a model's
/// plain text is encoded, and corrupt base64 (here: valid base64 of invalid
/// UTF-8) keeps its readable text, so the README decodes as UTF-8.
#[tokio::test]
async fn base64_fields_are_encoded_locally() {
    use base64::Engine as _;
    let bundle = compile(CONTENTS.as_bytes()).unwrap();
    let op = bundle.operation("getReadme").unwrap();
    let mut corrupt = b"# Trellis\n".to_vec();
    corrupt.extend([0xc3, 0x28, 0xa0]);
    let corrupt = base64::engine::general_purpose::STANDARD.encode(&corrupt);
    let (provider, engine) = scripted(vec![
        Ok(json!({"name": "README.md", "encoding": "base64", "content": "# Trellis\n\nA router.", "avatar": "PNG bytes"}).to_string()),
        Ok(json!({"name": "README.md", "encoding": "base64", "content": corrupt, "avatar": "aGk="}).to_string()),
    ]);
    let out = engine
        .generate(&bundle, op, &call("getReadme"), ctx())
        .await
        .unwrap();
    let body = out.body.unwrap();
    assert_eq!(
        decode64(body["content"].as_str().unwrap()),
        b"# Trellis\n\nA router."
    );
    assert_eq!(decode64(body["avatar"].as_str().unwrap()), b"PNG bytes");
    let prompt = &provider.requests()[0].messages[0].content;
    assert!(prompt.contains("plain decoded text"), "{prompt}");

    let out = engine
        .generate(&bundle, op, &call("getReadme"), ctx())
        .await
        .unwrap();
    let body = out.body.unwrap();
    let readme = String::from_utf8(decode64(body["content"].as_str().unwrap())).unwrap();
    assert!(readme.starts_with("# Trellis\n"), "{readme:?}");
    // Valid base64 is kept as it is.
    assert_eq!(body["avatar"], "aGk=");
}

/// Plaid-style lists: items identified by `<singular>_id`, and the list
/// members of a schema whose items are objects, `allOf` included.
#[test]
fn plaid_style_list_identity_and_members() {
    use mb_engine::consistency::{item_identity, list_members};
    let bundle = orders();
    let op = bundle.operation("listOrders").unwrap();
    let body = json!({"accounts": [{"account_id": "a1", "name": "Checking"}], "request_id": "r"});
    assert_eq!(item_identity(&bundle, op, &body), "account_id");
    assert_eq!(item_identity(&bundle, op, &json!([{"id": "o1"}])), "id");
    let schema = json!({"type": "object", "properties": {
        "accounts": {"type": "array", "items": {"allOf": [{"type": "object"}, {"properties": {"owners": {}}}]}},
        "numbers": {"type": "object"},
        "tags": {"type": "array", "items": {"type": "string"}},
        "item": {"type": "object"},
    }});
    assert_eq!(list_members(&bundle, &schema), vec!["accounts".to_string()]);
    // Slack: `anyOf [success, error]`.
    let either = json!({"anyOf": [
        {"type": "object", "properties": {"ok": {"const": true}, "messages": {"type": "array", "items": {"type": "object"}}}},
        {"type": "object", "properties": {"ok": {"const": false}, "error": {"type": "string"}}},
    ]});
    assert_eq!(list_members(&bundle, &either), vec!["messages".to_string()]);
}

/// JSON-object output mode makes models wrap a list: `{"orders": [...]}`.
/// When the list inside needs a local patch (an item with a bad enum), the
/// patched list is the answer, not a repair of the wrapper.
#[tokio::test]
async fn a_wrapped_list_is_unwrapped_and_patched() {
    let bundle = orders();
    let op = bundle.operation("listOrders").unwrap();
    let mut bad = valid_order();
    bad["status"] = json!("lost");
    let (provider, engine) = scripted(vec![
        Ok(json!({"orders": [valid_order(), bad]}).to_string()),
    ]);
    let out = engine
        .generate(&bundle, op, &call("listOrders"), ctx())
        .await
        .unwrap();
    assert_eq!(provider.requests().len(), 1);
    let items = out.body.as_ref().unwrap().as_array().unwrap();
    assert_eq!(items.len(), 2, "{:?}", out.body);
    assert_eq!(items[0], valid_order());
    assert!(["pending", "shipped"].contains(&items[1]["status"].as_str().unwrap()));
}

/// A contract whose `GET /things/{thingId}?owner=` answer carries formatted
/// strings: `id` and `ownerId` (`uuid`), `createdAt` (`date-time`) and
/// `label` (a custom format, an annotation only).
fn things() -> Bundle {
    inline(
        r##"{
  "openapi": "3.1.0",
  "info": {"title": "Things", "version": "1"},
  "paths": {
    "/things/{thingId}": {
      "get": {
        "operationId": "getThing",
        "parameters": [
          {"name": "thingId", "in": "path", "required": true, "schema": {"type": "string"}},
          {"name": "owner", "in": "query", "schema": {"type": "string"}}
        ],
        "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {
          "type": "object",
          "required": ["id", "ownerId", "createdAt", "label"],
          "properties": {
            "id": {"type": "string", "format": "uuid"},
            "ownerId": {"type": "string", "format": "uuid"},
            "createdAt": {"type": "string", "format": "date-time"},
            "label": {"type": "string", "format": "x-house-style"}
          }
        }}}}}
      }
    }
  }
}"##,
    )
}

fn thing_call(id: &str, owner: Option<&str>) -> OperationCall {
    let mut path_params = Map::new();
    path_params.insert("thingId".into(), json!(id));
    let mut query = Map::new();
    if let Some(owner) = owner {
        query.insert("owner".into(), json!(owner));
    }
    OperationCall {
        operation_id: "getThing".into(),
        path_params,
        query,
        headers: Map::new(),
        body: None,
    }
}

const THING_ID: &str = "5f0c6a1e-2b7d-4c3a-9e8f-1a2b3c4d5e6f";

/// A second valid uuid.
const DUP_ID: &str = "0b7e2c1d-9a4f-4e3b-8c2d-6f5a4b3c2d1e";

/// MB-ENGINE-STUB-1: a value breaking a format the faker produces is
/// patched locally (no repair round), and the answer is `patched`.
#[tokio::test]
async fn invalid_uuid_and_date_time_are_patched_locally() {
    let bundle = things();
    let op = bundle.operation("getThing").unwrap();
    let (provider, engine) = scripted(vec![Ok(json!({
        "id": THING_ID,
        "ownerId": "not-a-uuid",
        "createdAt": "yesterday",
        "label": "anything goes"
    })
    .to_string())]);
    let out = engine
        .generate(&bundle, op, &thing_call(THING_ID, None), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Patched, "{:?}", out.fallback_reason);
    assert_eq!(
        provider.requests().len(),
        1,
        "format errors never cost a repair call"
    );
    let body = out.body.unwrap();
    uuid::Uuid::parse_str(body["ownerId"].as_str().unwrap()).unwrap();
    chrono::DateTime::parse_from_rfc3339(body["createdAt"].as_str().unwrap()).unwrap();
    // A custom format stays an annotation.
    assert_eq!(body["label"], "anything goes");
    assert_eq!(body["id"], THING_ID);
}

/// A value equal to a request path or query value is echoed, not
/// overwritten, even where it breaks the declared format.
#[tokio::test]
async fn request_values_survive_format_patching() {
    let bundle = things();
    let op = bundle.operation("getThing").unwrap();
    let (_p, engine) = scripted(vec![Ok(json!({
        "id": "t-1",
        "ownerId": "bob",
        "createdAt": "2026-10-01T10:00:00Z",
        "label": "x"
    })
    .to_string())]);
    let out = engine
        .generate(&bundle, op, &thing_call("t-1", Some("bob")), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Llm, "{:?}", out.fallback_reason);
    let body = out.body.unwrap();
    assert_eq!(body["id"], "t-1");
    assert_eq!(body["ownerId"], "bob");
}

/// Request validators built with `SchemaValidator::new` keep formats as
/// annotations; output validators assert the formats the faker produces.
#[test]
fn only_output_validators_assert_formats() {
    let schema = json!({"type": "string", "format": "uuid"});
    assert!(SchemaValidator::new(&schema).unwrap().is_valid(&json!("x")));
    let output = SchemaValidator::for_output(&schema).unwrap();
    assert!(!output.is_valid_output(&json!("x")));
    assert!(output.is_valid_output(&json!(THING_ID)));
    // `is_valid` stays the annotation-only verdict the overlays rely on.
    assert!(output.is_valid(&json!("x")));
    let other =
        SchemaValidator::for_output(&json!({"type": "string", "format": "duration"})).unwrap();
    assert!(other.is_valid_output(&json!("not a duration")));
}

/// MB-ENGINE-STUB-7: an offline bodyless answer is the faker's, not the
/// LLM's.
#[tokio::test]
async fn offline_bodyless_answer_is_fallback() {
    let bundle = orders();
    let op = bundle.operation("deleteOrder").unwrap();
    let out = offline()
        .generate(&bundle, op, &call("deleteOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.status, 204);
    assert_eq!(out.body, None);
    assert_eq!(out.source, Source::Fallback);
    assert_eq!(
        out.fallback_reason.as_deref(),
        Some(mb_engine::OFFLINE_REASON)
    );
}

/// MB-ENGINE-STUB-2: when no faker draw validates, the answer says so in
/// its fallback reason instead of passing as schema-valid.
#[tokio::test]
async fn unsatisfiable_faker_output_is_flagged() {
    let bundle = inline(
        r##"{
  "openapi": "3.1.0",
  "info": {"title": "Odd", "version": "1"},
  "paths": {"/odd": {"get": {"operationId": "getOdd", "responses": {"200": {
    "description": "ok",
    "content": {"application/json": {"schema": {"type": "string", "not": {"type": "string"}}}}
  }}}}}
}"##,
    );
    let op = bundle.operation("getOdd").unwrap();
    let mut c = call("getOdd");
    c.path_params.clear();
    let out = offline().generate(&bundle, op, &c, ctx()).await.unwrap();
    assert_eq!(out.source, Source::Fallback);
    let reason = out.fallback_reason.unwrap();
    assert!(reason.starts_with(mb_engine::OFFLINE_REASON), "{reason}");
    assert!(reason.contains("does not validate"), "{reason}");
}

const TEXT_CONTRACT: &str = r##"
openapi: 3.1.0
info: {title: T, version: "1"}
paths:
  /csv:
    get:
      operationId: getCsv
      responses:
        "200":
          description: ok
          content:
            text/csv:
              schema: {type: string, examples: ["a,b\n1,2\n"]}
  /text:
    get:
      operationId: getText
      responses:
        "200":
          description: ok
          content:
            text/plain:
              schema: {type: string, enum: [hi]}
  /bin:
    get:
      operationId: getBin
      responses:
        "200":
          description: ok
          content:
            application/octet-stream:
              schema: {type: string, format: binary}
"##;

/// A REST response in a non-JSON, non-XML media type is answered from its
/// declared example, else faked (`text/*`) or left empty, and never by the
/// LLM: its JSON-mode answer could not be that media type.
#[tokio::test]
async fn non_json_text_responses_come_from_the_example_or_the_faker() {
    let bundle = compile(TEXT_CONTRACT.as_bytes()).unwrap();
    let (provider, engine) = scripted(vec![]);
    let csv = bundle.operation("getCsv").unwrap();
    let out = engine
        .generate(&bundle, csv, &call("getCsv"), ctx())
        .await
        .unwrap();
    assert_eq!(out.content_type.as_deref(), Some("text/csv"));
    assert_eq!(out.body, Some(json!("a,b\n1,2\n")));
    assert_eq!(out.source, Source::Fallback);
    assert_eq!(
        out.fallback_reason.as_deref(),
        Some(mb_engine::NON_JSON_MEDIA_REASON)
    );

    let text = bundle.operation("getText").unwrap();
    let out = engine
        .generate(&bundle, text, &call("getText"), ctx())
        .await
        .unwrap();
    assert_eq!(out.body, Some(json!("hi")));
    let variant = text.success_output().unwrap();
    let fallback = engine.fallback(&bundle, text, variant, 200, &call("getText"), &ctx());
    assert_eq!(fallback.body, Some(json!("hi")));

    let bin = bundle.operation("getBin").unwrap();
    let out = engine
        .generate(&bundle, bin, &call("getBin"), ctx())
        .await
        .unwrap();
    assert_eq!(
        out.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(out.body, None);
    assert!(provider.requests().is_empty());
}

/// An engine whose provider replays `script` in JSON-object mode (no
/// structured outputs), and that provider.
fn json_mode(script: Vec<Result<String, LlmError>>) -> (Arc<ScriptedProvider>, Engine) {
    let provider = Arc::new(
        ScriptedProvider::new(script).with_capabilities(Capabilities {
            json_schema: false,
            json_mode: true,
        }),
    );
    let dyn_provider: Arc<dyn Provider> = provider.clone();
    (
        provider,
        Engine::new(Some(dyn_provider), EngineConfig::default()),
    )
}

/// JSON mode forces an object at the top level: an array answer is asked
/// for as `{"items": [...]}` and unwrapped, whatever else the model put
/// beside it, without a repair call.
#[tokio::test]
async fn array_root_in_json_mode_is_asked_for_and_unwrapped_from_items() {
    let bundle = orders();
    let op = bundle.operation("listOrders").unwrap();
    let list = json!([valid_order(), valid_order()]);
    for answer in [
        json!({"items": list}),
        json!({"items": list, "total": 2}),
        list.clone(),
    ] {
        let (provider, engine) = json_mode(vec![Ok(answer.to_string())]);
        let out = engine
            .generate(&bundle, op, &call("listOrders"), ctx())
            .await
            .unwrap();
        assert_eq!(out.source, Source::Llm, "{answer}");
        let items = out.body.as_ref().and_then(Value::as_array).unwrap();
        assert_eq!(items.len(), 2, "{answer}");
        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        let user = requests[0].messages.last().unwrap();
        assert_eq!(user.role, Role::User);
        assert!(
            user.content.contains(r#"{"items": [...]}"#),
            "{}",
            user.content
        );
    }
}

/// With structured outputs (or for an object root) nothing changes: no
/// envelope is asked for, and an `items` member is not unwrapped.
#[tokio::test]
async fn items_envelope_only_for_array_roots_in_json_mode() {
    let bundle = orders();
    let list = json!([valid_order()]);
    let (provider, engine) = scripted(vec![Ok(list.to_string())]);
    let op = bundle.operation("listOrders").unwrap();
    let out = engine
        .generate(&bundle, op, &call("listOrders"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Llm);
    assert!(
        !provider.requests()[0]
            .messages
            .last()
            .unwrap()
            .content
            .contains(r#""items""#)
    );

    let (provider, engine) = json_mode(vec![Ok(valid_order().to_string())]);
    let op = bundle.operation("getOrder").unwrap();
    let out = engine
        .generate(&bundle, op, &call("getOrder"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Llm);
    assert!(
        !provider.requests()[0]
            .messages
            .last()
            .unwrap()
            .content
            .contains(r#"{"items": [...]}"#)
    );
}

/// `none` mode reports no JSON mode: an array is asked for as an array.
#[tokio::test]
async fn no_items_envelope_without_json_mode() {
    let bundle = orders();
    let op = bundle.operation("listOrders").unwrap();
    let provider = Arc::new(
        ScriptedProvider::new(vec![Ok(json!([valid_order()]).to_string())]).with_capabilities(
            Capabilities {
                json_schema: false,
                json_mode: false,
            },
        ),
    );
    let dyn_provider: Arc<dyn Provider> = provider.clone();
    let engine = Engine::new(Some(dyn_provider), EngineConfig::default());
    let out = engine
        .generate(&bundle, op, &call("listOrders"), ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Llm);
    assert!(
        !provider.requests()[0]
            .messages
            .last()
            .unwrap()
            .content
            .contains(r#"{"items": [...]}"#)
    );
}
