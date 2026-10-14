#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mb_engine::{
    Engine, EngineConfig, EngineError, GenerationContext, OperationCall, SchemaValidator, Source,
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
    assert_eq!(out.body, Some(valid_order()));
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
    assert_eq!(out.body, Some(valid_order()));
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
