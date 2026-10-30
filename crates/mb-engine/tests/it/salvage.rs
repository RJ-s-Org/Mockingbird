//! Answers cut short (W4b): salvaged locally, else repaired with a hint,
//! else a fallback whose reason names the truncation; and the structural
//! hints of a repair request.

use std::sync::Arc;

use mb_engine::{Engine, EngineConfig, GenerationContext, OperationCall, SchemaValidator, Source};
use mb_llm::{Capabilities, LlmError, Provider, Role, ScriptedProvider, Usage};
use serde_json::{Map, Value, json};

use crate::support::{orders, scripted, valid_order};

const ID: &str = "7f9c2ba4-e88f-4c1b-9f4a-3d2f0c6b1a10";

fn call(op: &str) -> OperationCall {
    let mut path_params = Map::new();
    path_params.insert("orderId".into(), json!(ID));
    OperationCall {
        operation_id: op.into(),
        path_params,
        query: Map::new(),
        headers: Map::new(),
        body: None,
    }
}

/// The provider's error for a reply cut at `max_tokens` after `partial`.
fn cut(partial: &str) -> LlmError {
    LlmError::Truncated {
        message: "stopped at max_tokens (64) (finish_reason=length)".into(),
        partial: partial.into(),
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 64,
            reasoning_tokens: 0,
        },
    }
}

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

async fn generate(engine: &Engine, op: &str) -> mb_engine::GeneratedResponse {
    let bundle = orders();
    engine
        .generate(
            &bundle,
            bundle.operation(op).unwrap(),
            &call(op),
            GenerationContext::default(),
        )
        .await
        .unwrap()
}

fn order_validator() -> SchemaValidator {
    let bundle = orders();
    SchemaValidator::new(&bundle.with_defs(&json!({"$ref": "#/$defs/Order"}))).unwrap()
}

/// A cut nested object loses its incomplete member; the rest is kept,
/// without a second call, and the cut call's tokens are counted.
#[tokio::test]
async fn a_cut_nested_object_is_salvaged_as_patched() {
    let partial = format!(
        r#"{{"id": "{ID}", "customerId": "c1", "status": "pending", "total": 42.5,
"category": {{"name": "books", "parent": {{"na"#
    );
    let (provider, engine) = scripted(vec![Err(cut(&partial))]);
    let out = generate(&engine, "getOrder").await;
    assert_eq!(out.source, Source::Patched);
    let body = out.body.unwrap();
    assert_eq!(body["customerId"], "c1");
    assert_eq!(body["category"]["name"], "books");
    order_validator().validate(&body).unwrap();
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(out.usage.completion_tokens, 64);
}

/// A required member the cut dropped is filled by the faker.
#[tokio::test]
async fn a_member_the_cut_dropped_is_filled() {
    let partial = format!(r#"{{"id": "{ID}", "customerId": "c1", "status": "pending", "total": 4"#);
    let (_, engine) = scripted(vec![Err(cut(&partial))]);
    let out = generate(&engine, "getOrder").await;
    assert_eq!(out.source, Source::Patched);
    let body = out.body.unwrap();
    assert!(body["total"].is_number(), "{body}");
    order_validator().validate(&body).unwrap();
}

/// A cut string is dropped, not served shortened: a required one is
/// refilled whole. Half a surrogate pair goes with it.
#[tokio::test]
async fn a_cut_string_is_dropped_and_refilled() {
    let partial =
        r#"{"customerId": "c1", "status": "pending", "total": 42.5, "id": "7f9c2ba4-e88f"#;
    let (_, engine) = scripted(vec![Err(cut(partial))]);
    let out = generate(&engine, "getOrder").await;
    assert_eq!(out.source, Source::Patched);
    let body = out.body.unwrap();
    assert_ne!(body["id"], "7f9c2ba4-e88f");
    assert_eq!(body["customerId"], "c1");
    order_validator().validate(&body).unwrap();

    let partial = format!(
        r#"{{"id": "{ID}", "customerId": "c1", "status": "pending", "total": 4.5, "note": "hi \uD83D\uDE"#
    );
    let (provider, engine) = scripted(vec![Err(cut(&partial))]);
    let out = generate(&engine, "getOrder").await;
    assert_eq!(out.source, Source::Patched);
    assert_eq!(out.body.unwrap()["total"], 4.5);
    assert_eq!(provider.requests().len(), 1);
}

/// The complete items are kept; the cut one keeps its complete members
/// and is completed, or is dropped when it has none.
#[tokio::test]
async fn a_cut_array_keeps_its_complete_items() {
    let order = valid_order().to_string();
    let partial = format!(r#"[{order}, {order}, {{"id": "{ID}", "customerId": "c3", "st"#);
    let (_, engine) = scripted(vec![Err(cut(&partial))]);
    let out = generate(&engine, "listOrders").await;
    assert_eq!(out.source, Source::Patched);
    let items = out.body.unwrap();
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["customerId"], "c1");
    assert_eq!(items[2]["customerId"], "c3");
    order_validator().validate(&items[2]).unwrap();

    let partial = format!(r#"[{order}, {order}, {{"id"#);
    let (_, engine) = scripted(vec![Err(cut(&partial))]);
    let out = generate(&engine, "listOrders").await;
    assert_eq!(out.source, Source::Patched);
    assert_eq!(out.body.unwrap().as_array().unwrap().len(), 2);
}

/// In JSON-object mode the cut envelope is closed and unwrapped; its last
/// item, cut after its first member, is completed.
#[tokio::test]
async fn a_cut_items_envelope_is_salvaged_and_unwrapped() {
    let order = valid_order().to_string();
    let partial = format!(r#"{{"items": [{order}, {{"id": "{ID}", "customerId": "c2", "sta"#);
    let (_, engine) = json_mode(vec![Err(cut(&partial))]);
    let out = generate(&engine, "listOrders").await;
    assert_eq!(out.source, Source::Patched);
    let body = out.body.unwrap();
    let items = body.as_array().expect("the envelope is unwrapped");
    assert_eq!(items.len(), 2);
    assert_eq!(items[1]["customerId"], "c2");
}

/// An answer the provider did not cut is never salvaged: a missing `}`
/// is repaired, and its real values are not replaced by the faker's.
#[tokio::test]
async fn an_uncut_answer_ending_inside_its_value_is_repaired() {
    let partial =
        format!(r#"{{"id": "{ID}", "customerId": "c1", "status": "pending", "total": 42.5"#);
    let (provider, engine) = scripted(vec![Ok(partial.clone()), Ok(valid_order().to_string())]);
    let out = generate(&engine, "getOrder").await;
    assert_eq!(out.source, Source::Repaired);
    assert_eq!(out.body.unwrap()["total"], 42.5);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let repair = &requests[1].messages.last().unwrap().content;
    assert!(!repair.contains("cut off"), "{repair}");

    let (_, engine) = scripted(vec![Ok(partial)]);
    let reason = generate(&engine, "getOrder").await.fallback_reason.unwrap();
    assert!(!reason.contains("truncated"), "{reason}");
}

/// A complete answer before cut prose is accepted as it is.
#[tokio::test]
async fn a_complete_answer_before_cut_prose_is_llm() {
    let partial = format!(
        "```json\n{}\n```\nThis order is pending and the total",
        valid_order()
    );
    let (provider, engine) = scripted(vec![Err(cut(&partial))]);
    let out = generate(&engine, "getOrder").await;
    assert_eq!(out.source, Source::Llm);
    assert_eq!(out.body.unwrap()["total"], 42.5);
    assert_eq!(provider.requests().len(), 1);
}

/// A cut wrapped answer is completed inside the wrapper: its own values are
/// the answer, with nothing faked around the wrapper.
#[tokio::test]
async fn a_cut_wrapped_answer_is_completed_inside_the_wrapper() {
    let partial = format!(
        r#"{{"payment_intent": {{"id": "{ID}", "customerId": "c1", "status": "pending", "total": 4"#
    );
    let (_, engine) = scripted(vec![Err(cut(&partial))]);
    let out = generate(&engine, "getOrder").await;
    assert_eq!(out.source, Source::Patched);
    let body = out.body.unwrap();
    assert!(body.get("payment_intent").is_none(), "{body}");
    assert_eq!(body["id"], ID);
    assert_eq!(body["customerId"], "c1");
    assert_eq!(body["status"], "pending");
    order_validator().validate(&body).unwrap();
}

/// The salvage closes the outermost value; it never answers with a value
/// nested in the cut one, nor fills an answer that is no order.
#[tokio::test]
async fn the_salvage_never_returns_an_inner_fragment() {
    let inner = valid_order();
    let partial = format!(r#"{{"order": {inner}, "others": [{inner}, {{"id": "#);
    let (provider, engine) = scripted(vec![Err(cut(&partial)), Ok("no json".into())]);
    let out = generate(&engine, "getOrder").await;
    assert_eq!(out.source, Source::Fallback);
    assert_eq!(provider.requests().len(), 2);
    assert_ne!(out.body.unwrap()["customerId"], "c1");

    // Broken before the cut (a comment): the category inside is no root.
    let partial = format!(
        "{{\"id\": \"{ID}\", \"customerId\": \"REAL\", // c\n \"status\": \"pending\", \"total\": 1, \"category\": {{\"name\": \"books\", \"parent\": {{\"na"
    );
    let (provider, engine) = scripted(vec![Err(cut(&partial)), Ok("no json".into())]);
    let out = generate(&engine, "getOrder").await;
    assert_eq!(out.source, Source::Fallback);
    assert_eq!(provider.requests().len(), 2);
    assert!(
        out.fallback_reason
            .unwrap()
            .starts_with("LLM output truncated")
    );
}

/// When nothing complete can be closed, the repair request says the answer
/// was cut; a repaired answer is `Repaired`.
#[tokio::test]
async fn an_unsalvageable_cut_is_repaired_with_a_hint() {
    let (provider, engine) = scripted(vec![Err(cut(r#"{"id"#)), Ok(valid_order().to_string())]);
    let out = generate(&engine, "getOrder").await;
    assert_eq!(out.source, Source::Repaired);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let repair = requests[1].messages.last().unwrap();
    assert_eq!(repair.role, Role::User);
    assert!(
        repair.content.contains("cut off at the output token limit"),
        "{}",
        repair.content
    );
}

/// A cut answer neither salvaged nor repaired falls back with a reason
/// that names the truncation first.
#[tokio::test]
async fn a_cut_that_is_not_salvaged_falls_back_as_truncated() {
    let (_, engine) = scripted(vec![Err(cut(r#"{"id"#)), Ok("no json".into())]);
    let out = generate(&engine, "getOrder").await;
    assert_eq!(out.source, Source::Fallback);
    let reason = out.fallback_reason.unwrap();
    assert!(
        reason.starts_with("LLM output truncated: stopped at max_tokens"),
        "{reason}"
    );
    assert!(reason.contains("finish_reason=length"), "{reason}");
    assert!(reason.contains("not salvaged"), "{reason}");

    // Cut again on the repair call: still a truncation.
    let (_, engine) = scripted(vec![Err(cut(r#"{"id"#)), Err(cut("{"))]);
    let reason = generate(&engine, "getOrder").await.fallback_reason.unwrap();
    assert!(reason.starts_with("LLM output truncated"), "{reason}");
}

/// The repair request of `answer` (invalid, then the script runs out).
async fn repair_request(op: &str, answer: Value, json_object: bool) -> String {
    let script = vec![Ok(answer.to_string())];
    let (provider, engine) = if json_object {
        json_mode(script)
    } else {
        scripted(script)
    };
    let out = generate(&engine, op).await;
    assert_eq!(out.source, Source::Fallback, "{answer}");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "{answer}");
    requests[1].messages.last().unwrap().content.clone()
}

#[tokio::test]
async fn a_wrapper_is_named() {
    let request = repair_request(
        "getOrder",
        json!({"payment_intent": {"id": "x", "status": "pending"}}),
        false,
    )
    .await;
    assert!(
        request.starts_with("You wrapped the answer in `payment_intent`"),
        "{request}"
    );
    assert!(request.contains("Errors:\n- "), "{request}");
}

#[tokio::test]
async fn the_schema_itself_is_named() {
    let request = repair_request(
        "getOrder",
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"id": {"type": "string"}}
        }),
        false,
    )
    .await;
    assert!(
        request.starts_with("You returned the JSON Schema itself"),
        "{request}"
    );
}

/// Data with its own `type` and `properties` members is not the schema; an
/// array root answered with one such object is "array expected".
#[tokio::test]
async fn data_with_type_and_properties_is_not_the_schema() {
    let request = repair_request(
        "getOrder",
        json!({"id": ID, "type": "x", "properties": {"a": 1}}),
        false,
    )
    .await;
    assert!(!request.contains("JSON Schema itself"), "{request}");
    let request = repair_request(
        "listOrders",
        json!({"type": "Feature", "properties": {"name": "a"}}),
        false,
    )
    .await;
    assert!(
        request.starts_with("An array was expected at the root"),
        "{request}"
    );
}

#[tokio::test]
async fn an_empty_object_is_named() {
    let request = repair_request("getOrder", json!({}), false).await;
    assert!(request.starts_with("You returned `{}`"), "{request}");
}

#[tokio::test]
async fn an_object_for_an_array_is_named() {
    let request = repair_request("listOrders", json!({"total": 3, "page": 1}), false).await;
    assert!(
        request.starts_with("An array was expected at the root"),
        "{request}"
    );
    let request = repair_request("listOrders", json!({"orders": [{"id": "x"}]}), false).await;
    assert!(
        request.starts_with("You wrapped the answer in `orders`"),
        "{request}"
    );
    // In JSON-object mode the array belongs under "items".
    let request = repair_request("listOrders", json!({"total": 3, "page": 1}), true).await;
    assert!(
        request.starts_with(r#"An array was expected. Return it under "items""#),
        "{request}"
    );
}

#[tokio::test]
async fn an_array_for_an_object_is_named() {
    let request = repair_request("getOrder", json!([{"id": "x"}]), false).await;
    assert!(
        request.starts_with("An object was expected at the root"),
        "{request}"
    );
}

/// A plain value error gets no shape hint: only the errors.
#[tokio::test]
async fn a_value_error_gets_no_hint() {
    let mut order = valid_order();
    order.as_object_mut().unwrap().remove("customerId");
    let request = repair_request("getOrder", order, false).await;
    assert!(
        request.starts_with("The JSON above is invalid against the schema"),
        "{request}"
    );
}

/// Errors are capped at 10, shallowest first.
#[test]
fn errors_are_capped_shallowest_first() {
    let required: Vec<String> = (0..12).map(|i| format!("r{i}")).collect();
    let validator = SchemaValidator::new(&json!({
        "type": "object",
        "required": required,
        "properties": {
            "deep": {
                "type": "object",
                "properties": {"x": {"type": "object", "properties": {"y": {"type": "integer"}}}}
            }
        }
    }))
    .unwrap();
    let errors = validator
        .validate(&json!({"deep": {"x": {"y": "s"}}}))
        .unwrap_err();
    assert_eq!(errors.len(), 10);
    assert!(errors.iter().all(|e| !e.contains("/deep")), "{errors:?}");
}
