#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mb_engine::{Engine, EngineConfig, MessageContext, SchemaValidator, Source};
use mb_llm::{LlmError, Provider, ResponseFormat, Role, ScriptedProvider};
use mb_mcm::{Bundle, Channel, ChannelBinding, Direction};
use serde_json::{Map, Value, json};

fn message_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {"type": "string"},
            "status": {"type": "string", "enum": ["a", "b"]}
        },
        "required": ["id", "status"]
    })
}

fn message_bundle() -> Bundle {
    let channel = Channel {
        id: "orderEvents".into(),
        title: "Order events".into(),
        address: "orders".into(),
        binding: ChannelBinding::Kafka {
            topic: "orders".into(),
            partitions_hint: None,
        },
        direction: Direction::Publish,
        message_schema: message_schema(),
        message_names: vec!["OrderCreated".into(), "OrderUpdated".into()],
        content_type: "application/json".into(),
        key_schema: None,
        headers_schema: None,
        parameters: Map::new(),
        triggers: Vec::new(),
        reply: None,
        state_effect: None,
        semantic_hints: vec!["status a means accepted".into()],
    };
    Bundle {
        id: "orders-events".into(),
        title: "Orders Events".into(),
        version: "1.0.0".into(),
        source_digest: "d1".into(),
        operations: Vec::new(),
        defs: Map::new(),
        protocol_payload: None,
        channels: vec![channel],
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

fn ctx() -> MessageContext {
    MessageContext {
        trigger: json!({"kind": "schedule"}),
        ..MessageContext::default()
    }
}

fn assert_valid(value: &Value) {
    SchemaValidator::new(&message_schema())
        .unwrap()
        .validate(value)
        .unwrap();
}

#[tokio::test]
async fn offline_message_is_valid_fallback() {
    let bundle = message_bundle();
    let engine = Engine::new(None, EngineConfig::default());
    let out = engine
        .generate_message(&bundle, &bundle.channels[0], &ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Fallback);
    assert_valid(&out.payload);
    assert_eq!(out.usage, Default::default());
}

#[tokio::test]
async fn llm_message_validated() {
    let bundle = message_bundle();
    let (provider, engine) = scripted(vec![Ok(json!({"id": "x", "status": "a"}).to_string())]);
    let out = engine
        .generate_message(&bundle, &bundle.channels[0], &ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Llm);
    assert_eq!(out.payload, json!({"id": "x", "status": "a"}));
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    match &requests[0].response_format {
        ResponseFormat::JsonSchema { name, .. } => assert_eq!(name, "orderEvents"),
        other => panic!("unexpected format {other:?}"),
    }
}

#[tokio::test]
async fn invalid_message_is_repaired() {
    let bundle = message_bundle();
    let (provider, engine) = scripted(vec![
        // A wrong type is not patched locally (an off-enum value would be).
        Ok(json!({"id": "x", "status": 5}).to_string()),
        Ok(json!({"id": "x", "status": "b"}).to_string()),
    ]);
    let out = engine
        .generate_message(&bundle, &bundle.channels[0], &ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Repaired);
    assert_eq!(out.payload, json!({"id": "x", "status": "b"}));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let last = requests[1].messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(last.content.contains("Errors:"), "{}", last.content);
}

#[tokio::test]
async fn unrepairable_message_falls_back_to_faker() {
    let bundle = message_bundle();
    let (provider, engine) = scripted(vec![
        Ok(json!({"id": 1}).to_string()),
        Ok("not json".into()),
    ]);
    let out = engine
        .generate_message(&bundle, &bundle.channels[0], &ctx())
        .await
        .unwrap();
    assert_eq!(out.source, Source::Fallback);
    assert_valid(&out.payload);
    assert_eq!(provider.requests().len(), 2);
}

#[test]
fn message_prompt_blocks() {
    let bundle = message_bundle();
    let ctx = MessageContext {
        trigger: json!({"kind": "manual"}),
        context: Some(json!({"note": "<b>hi</b>"})),
        received: Some(json!({"id": "r1", "status": "a"})),
        related: Vec::new(),
    };
    let messages = mb_engine::prompt::build_message_messages(
        &bundle,
        &bundle.channels[0],
        &ctx,
        Some("a pirate"),
    );
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, Role::System);
    let system = &messages[0].content;
    assert!(system.contains("Kafka topic: orders"), "{system}");
    assert!(system.contains("Message JSON Schema:"), "{system}");
    assert!(system.contains("Channel: orderEvents"), "{system}");
    assert!(system.contains("API: Orders Events"), "{system}");
    assert!(
        system.contains("Messages: OrderCreated, OrderUpdated"),
        "{system}"
    );
    assert!(system.contains("Summary: Order events"), "{system}");
    assert!(system.contains("Persona: a pirate"), "{system}");
    assert!(system.contains("- status a means accepted"), "{system}");
    let user = &messages[1].content;
    assert_eq!(messages[1].role, Role::User);
    assert!(
        user.contains("<trigger>\n{\"kind\":\"manual\"}\n</trigger>"),
        "{user}"
    );
    assert!(user.contains("<context>\n"), "{user}");
    assert!(user.contains("<received_message>\n"), "{user}");
    assert!(!user.contains("<related_resources>"), "{user}");
    assert!(user.contains("\\u003cb>hi\\u003c/b>"), "{user}");
    assert!(!user.contains("<b>"), "{user}");
}

#[test]
fn amqp_mqtt_ws_binding_lines() {
    let mut bundle = message_bundle();
    let ctx = ctx();
    let cases = [
        (
            ChannelBinding::Amqp {
                exchange: String::new(),
                exchange_type: "direct".into(),
                routing_key: "rk".into(),
                queue: Some("q1".into()),
            },
            "AMQP exchange: (default) (direct), routing key: rk, queue: q1\n",
        ),
        (
            ChannelBinding::Amqp {
                exchange: "ex".into(),
                exchange_type: "topic".into(),
                routing_key: "a.b".into(),
                queue: None,
            },
            "AMQP exchange: ex (topic), routing key: a.b\n",
        ),
        (
            ChannelBinding::Mqtt {
                topic: "s/1".into(),
                qos: 1,
                retain: false,
            },
            "MQTT topic: s/1 (QoS 1)\n",
        ),
        (
            ChannelBinding::WebSocket {
                path: "/chat".into(),
            },
            "WebSocket path: /chat\n",
        ),
    ];
    for (binding, line) in cases {
        bundle.channels[0].binding = binding;
        let messages =
            mb_engine::prompt::build_message_messages(&bundle, &bundle.channels[0], &ctx, None);
        assert!(
            messages[0].content.contains(line),
            "{}",
            messages[0].content
        );
    }
}

#[test]
fn system_message_is_static() {
    let bundle = message_bundle();
    let a = mb_engine::prompt::build_message_messages(&bundle, &bundle.channels[0], &ctx(), None);
    let other = MessageContext {
        trigger: json!({"kind": "on_state_change", "resource_type": "orders", "id": "42", "change": "upserted"}),
        context: Some(json!({"x": 1})),
        received: None,
        related: vec![mb_state::StoredResource {
            resource_type: "orders".into(),
            id: "42".into(),
            data: json!({"id": "42"}),
        }],
    };
    let b = mb_engine::prompt::build_message_messages(&bundle, &bundle.channels[0], &other, None);
    assert_eq!(a[0].content, b[0].content);
    assert_ne!(a[1].content, b[1].content);
    assert!(
        b[1].content
            .contains("<related_resources>\n{\"id\":\"42\"}\n</related_resources>")
    );
}

#[test]
fn validate_message_reports_errors() {
    let bundle = message_bundle();
    let engine = Engine::new(None, EngineConfig::default());
    let channel = &bundle.channels[0];
    assert!(
        engine
            .validate_message(&bundle, channel, &json!({"id": "x", "status": "a"}))
            .is_ok()
    );
    let errors = engine
        .validate_message(&bundle, channel, &json!({"id": "x", "status": "c"}))
        .unwrap_err();
    assert!(!errors.is_empty());
    let errors = engine
        .validate_message(&bundle, channel, &json!({"status": "a"}))
        .unwrap_err();
    assert!(errors.iter().any(|e| e.contains("id")), "{errors:?}");
}

#[test]
fn fake_is_validated() {
    let bundle = message_bundle();
    let engine = Engine::new(None, EngineConfig::default());
    for _ in 0..10 {
        assert_valid(&engine.fake(&bundle, &message_schema()));
    }
}

#[tokio::test]
async fn uncompilable_schema_is_schema_error() {
    let mut bundle = message_bundle();
    bundle.channels[0].message_schema = json!({"type": 12});
    let engine = Engine::new(None, EngineConfig::default());
    let err = engine
        .generate_message(&bundle, &bundle.channels[0], &ctx())
        .await
        .unwrap_err();
    assert!(matches!(err, mb_engine::EngineError::Schema(_)), "{err:?}");
}
