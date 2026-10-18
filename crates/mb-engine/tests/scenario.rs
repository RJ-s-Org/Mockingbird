#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mb_engine::{
    Engine, EngineConfig, GenerationContext, OperationCall, ScenarioSpec, SchemaValidator, Source,
    apply_set,
};
use mb_import::compile;
use mb_llm::{Provider, Role, ScriptedProvider, Usage};
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

fn order_validator(bundle: &Bundle) -> SchemaValidator {
    SchemaValidator::new(&bundle.with_defs(&json!({"$ref": "#/$defs/Order"}))).unwrap()
}

fn valid_order() -> Value {
    json!({
        "id": "7f9c2ba4-e88f-4c1b-9f4a-3d2f0c6b1a10",
        "customerId": "c1",
        "status": "pending",
        "total": 42.5
    })
}

fn call() -> OperationCall {
    let mut path_params = Map::new();
    path_params.insert("orderId".into(), json!("o1"));
    OperationCall {
        operation_id: "getOrder".into(),
        path_params,
        query: Map::new(),
        headers: Map::new(),
        body: None,
    }
}

fn engine_with(provider: Arc<ScriptedProvider>) -> Engine {
    let dyn_provider: Arc<dyn Provider> = provider;
    Engine::new(Some(dyn_provider), EngineConfig::default())
}

fn usage(prompt_tokens: u32, completion_tokens: u32) -> Usage {
    Usage {
        prompt_tokens,
        completion_tokens,
    }
}

fn ctx(scenario: Option<&ScenarioSpec>) -> GenerationContext<'_> {
    GenerationContext {
        scenario,
        ..GenerationContext::default()
    }
}

#[tokio::test]
async fn scenario_instructions_in_user_message_only() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let scenario = ScenarioSpec {
        name: "shipped".into(),
        instructions: Some("The order has already shipped.".into()),
        set: None,
        status: None,
    };

    let plain = Arc::new(ScriptedProvider::new(vec![Ok(valid_order().to_string())]));
    engine_with(plain.clone())
        .generate(&bundle, op, &call(), ctx(None))
        .await
        .unwrap();
    let with_scenario = Arc::new(ScriptedProvider::new(vec![Ok(valid_order().to_string())]));
    engine_with(with_scenario.clone())
        .generate(&bundle, op, &call(), ctx(Some(&scenario)))
        .await
        .unwrap();

    let plain = &plain.requests()[0].messages;
    let scen = &with_scenario.requests()[0].messages;
    assert_eq!(plain[0].role, Role::System);
    assert_eq!(scen[0].role, Role::System);
    assert_eq!(plain[0].content, scen[0].content);
    assert!(!scen[0].content.contains("shipped\">"));

    let user = &scen[1].content;
    assert_eq!(scen[1].role, Role::User);
    assert!(user.contains("<scenario name=\"shipped\">"), "{user}");
    assert!(user.contains("The order has already shipped."), "{user}");
    let request_at = user.find("<request>").unwrap();
    let scenario_at = user.find("<scenario name=").unwrap();
    assert!(scenario_at > request_at, "{user}");
    assert!(!plain[1].content.contains("<scenario"));
}

#[tokio::test]
async fn scenario_set_applied_and_validated() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let scenario = ScenarioSpec {
        name: "shipped".into(),
        instructions: None,
        set: Some(json!({"status": "shipped"})),
        status: None,
    };
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(valid_order().to_string())]));
    let out = engine_with(provider)
        .generate(&bundle, op, &call(), ctx(Some(&scenario)))
        .await
        .unwrap();

    assert_eq!(out.source, Source::Llm);
    let body = out.body.unwrap();
    assert_eq!(body["status"], json!("shipped"));
    assert_eq!(body["id"], json!("o1"));
    assert_eq!(body["total"], json!(42.5));
    order_validator(&bundle).validate(&body).unwrap();
}

fn stored_order() -> StoredResource {
    StoredResource {
        resource_type: "orders".into(),
        id: "o1".into(),
        data: json!({"id": "o1", "customerId": "c9", "status": "pending", "total": 5}),
    }
}

#[tokio::test]
async fn scenario_set_wins_over_stored_target() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let scenario = ScenarioSpec {
        name: "shipped".into(),
        instructions: None,
        set: Some(json!({"status": "shipped", "id": "other"})),
        status: None,
    };
    let target = stored_order();
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(valid_order().to_string())]));
    let out = engine_with(provider)
        .generate(
            &bundle,
            op,
            &call(),
            GenerationContext {
                target: Some(&target),
                ..ctx(Some(&scenario))
            },
        )
        .await
        .unwrap();

    let body = out.body.unwrap();
    assert_eq!(body["status"], json!("shipped"));
    assert_eq!(body["id"], json!("o1"));
    assert_eq!(body["customerId"], json!("c9"));
    order_validator(&bundle).validate(&body).unwrap();
}

#[tokio::test]
async fn scenario_set_on_update_beats_target_but_not_request_body() {
    let bundle = orders();
    let op = bundle.operation("updateOrder").unwrap();
    let scenario = ScenarioSpec {
        name: "shipped".into(),
        instructions: None,
        set: Some(json!({"status": "shipped", "customerId": "c-set", "total": 99})),
        status: None,
    };
    let target = stored_order();
    let mut update = call();
    update.operation_id = "updateOrder".into();
    update.body = Some(json!({"customerId": "c-req"}));
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(valid_order().to_string())]));
    let out = engine_with(provider)
        .generate(
            &bundle,
            op,
            &update,
            GenerationContext {
                target: Some(&target),
                ..ctx(Some(&scenario))
            },
        )
        .await
        .unwrap();

    let body = out.body.unwrap();
    assert_eq!(body["status"], json!("shipped"));
    assert_eq!(body["total"], json!(99));
    assert_eq!(body["customerId"], json!("c-req"));
    assert_eq!(body["id"], json!("o1"));
    order_validator(&bundle).validate(&body).unwrap();
}

#[tokio::test]
async fn scenario_set_applies_to_faker_output() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let scenario = ScenarioSpec {
        name: "shipped".into(),
        instructions: None,
        set: Some(json!({"status": "shipped"})),
        status: None,
    };
    let out = Engine::new(None, EngineConfig::default())
        .generate(&bundle, op, &call(), ctx(Some(&scenario)))
        .await
        .unwrap();
    assert_eq!(out.source, Source::Fallback);
    assert_eq!(out.body.unwrap()["status"], json!("shipped"));
}

#[tokio::test]
async fn invalid_scenario_set_is_ignored() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let scenario = ScenarioSpec {
        name: "broken".into(),
        instructions: None,
        set: Some(json!({"total": "NaN-string"})),
        status: None,
    };
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(valid_order().to_string())]));
    let out = engine_with(provider)
        .generate(&bundle, op, &call(), ctx(Some(&scenario)))
        .await
        .unwrap();

    let body = out.body.unwrap();
    assert!(body["total"].is_number(), "{body}");
    assert_eq!(body["total"], json!(42.5));
    order_validator(&bundle).validate(&body).unwrap();
}

#[tokio::test]
async fn usage_sums_primary_and_repair() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let provider = Arc::new(ScriptedProvider::with_usage(vec![
        Ok((json!({"id": "x"}).to_string(), usage(10, 5))),
        Ok((valid_order().to_string(), usage(20, 7))),
    ]));
    let out = engine_with(provider)
        .generate(&bundle, op, &call(), ctx(None))
        .await
        .unwrap();

    assert_eq!(out.source, Source::Repaired);
    assert_eq!(out.usage, usage(30, 12));
}

#[tokio::test]
async fn faker_usage_is_zero() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let out = Engine::new(None, EngineConfig::default())
        .generate(&bundle, op, &call(), ctx(None))
        .await
        .unwrap();
    assert_eq!(out.source, Source::Fallback);
    assert_eq!(out.usage, Usage::default());
}

#[test]
fn apply_set_is_a_json_merge_patch() {
    let body = json!({"a": 1, "b": {"c": 2, "d": 3}, "e": 4});
    let set = json!({"b": {"c": 9, "d": null}, "e": null, "f": "new"});
    assert_eq!(
        apply_set(&body, &set),
        json!({"a": 1, "b": {"c": 9}, "f": "new"})
    );
}

#[test]
fn usage_adds() {
    assert_eq!(usage(1, 2) + usage(3, 4), usage(4, 6));
}
