#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mb_engine::{
    Engine, EngineConfig, EngineError, GenerationContext, JointPart, OperationCall,
    SchemaValidator, Source,
};
use mb_import::compile;
use mb_llm::{LlmError, Provider, Role, ScriptedProvider};
use mb_mcm::Bundle;
use mb_state::StoredResource;
use serde_json::{Map, Value, json};

fn orders() -> Bundle {
    let path = format!(
        "{}/../../fixtures/openapi/orders-3.0.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    compile(&std::fs::read(path).unwrap()).unwrap()
}

fn validator(bundle: &Bundle, def: &str) -> SchemaValidator {
    SchemaValidator::new(&bundle.with_defs(&json!({"$ref": format!("#/$defs/{def}")}))).unwrap()
}

fn valid_order() -> Value {
    json!({
        "id": "7f9c2ba4-e88f-4c1b-9f4a-3d2f0c6b1a10",
        "customerId": "c1",
        "status": "pending",
        "total": 42.5
    })
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

fn scripted(script: Vec<Result<String, LlmError>>) -> (Arc<ScriptedProvider>, Engine) {
    let provider = Arc::new(ScriptedProvider::new(script));
    let dyn_provider: Arc<dyn Provider> = provider.clone();
    (
        provider,
        Engine::new(Some(dyn_provider), EngineConfig::default()),
    )
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
    let body = out.body.unwrap();
    assert!(body.get("code").is_some() && body.get("message").is_some());
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
        "id": "o1", "customerId": "someone-else", "status": "pending", "total": 99
    })
    .to_string())]);
    let out = engine.generate(&bundle, op, &c, ctx()).await.unwrap();
    assert_eq!(out.source, Source::Llm);
    let body = out.body.unwrap();
    assert_eq!(body["id"], "o1");
    assert_eq!(body["customerId"], "c1");
    assert_eq!(body["total"], 10);
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
    other["id"] = json!("o2");
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
    assert_eq!(out.source, Source::Repaired);
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
    answer["total"] = json!(-1);
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
