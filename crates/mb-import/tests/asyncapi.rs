#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mb_import::{ImportError, SourceFormat, compile, detect_format, sniff, validate_triggers};
use mb_mcm::{Bundle, ChannelBinding, Direction, StateChangeKind, Trigger};
use serde_json::json;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../../fixtures/asyncapi/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(path).unwrap()
}

fn streetlights() -> Bundle {
    compile(&fixture("streetlights-kafka-2.6.yaml")).unwrap()
}

fn orders() -> Bundle {
    compile(&fixture("orders-amqp-2.6.yaml")).unwrap()
}

fn invalid(src: &str) -> (String, String) {
    match compile(src.as_bytes()) {
        Err(ImportError::Invalid { pointer, message }) => (pointer, message),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn detects_asyncapi_versions() {
    for v in ["2.0.0", "2.6.0"] {
        assert_eq!(
            detect_format(&json!({"asyncapi": v})).unwrap(),
            SourceFormat::AsyncApi2
        );
    }
    assert_eq!(
        detect_format(&json!({"asyncapi": "3.0.0"})).unwrap(),
        SourceFormat::AsyncApi3
    );
    match detect_format(&json!({"asyncapi": "1.2.0"})) {
        Err(ImportError::Unsupported(m)) => assert_eq!(m, "asyncapi 1.2.0"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        sniff(b"asyncapi: 2.6.0\ninfo: {title: T, version: '1'}\n").unwrap(),
        SourceFormat::AsyncApi2
    );
    assert_eq!(
        sniff(br#"{"asyncapi": "2.3.0", "info": {"title": "T", "version": "1"}}"#).unwrap(),
        SourceFormat::AsyncApi2
    );
}

#[test]
fn streetlights_26_channels() {
    let b = streetlights();
    let ids: Vec<&str> = b.channels.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        ["dimLight", "receiveLightMeasurement", "turnOff", "turnOn"]
    );
    for c in &b.channels {
        let expected = if c.id == "receiveLightMeasurement" {
            Direction::Subscribe
        } else {
            Direction::Publish
        };
        assert_eq!(c.direction, expected, "{}", c.id);
        assert_eq!(c.parameters["streetlightId"], json!({"type": "string"}));
        assert!(
            c.headers_schema.as_ref().unwrap()["properties"]
                .get("my-app-header")
                .is_some(),
            "{}",
            c.id
        );
    }
    let measured = b.channel("receiveLightMeasurement").unwrap();
    assert_eq!(
        measured.binding,
        ChannelBinding::Kafka {
            topic: "smartylighting.streetlights.1.0.event.{streetlightId}.lighting.measured".into(),
            partitions_hint: None,
        }
    );
    assert_eq!(measured.message_names, ["lightMeasured"]);
    let turn_on = b.channel("turnOn").unwrap();
    assert_eq!(turn_on.effective_triggers(), vec![Trigger::Manual]);
    assert_eq!(turn_on.content_type, "application/json");
}

#[test]
fn streetlights_kafka_26_snapshot() {
    insta::assert_yaml_snapshot!(streetlights(), {".source_digest" => "[digest]"});
}

#[test]
fn amqp_routing_key_and_queue_bindings() {
    let b = orders();
    let created = b.channel("orderCreated").unwrap();
    assert_eq!(created.direction, Direction::Publish);
    assert_eq!(
        created.binding,
        ChannelBinding::Amqp {
            exchange: "orders".into(),
            exchange_type: "topic".into(),
            routing_key: "orders.created".into(),
            queue: None,
        }
    );
    let cancel = b.channel("cancelOrder").unwrap();
    assert_eq!(cancel.direction, Direction::Subscribe);
    assert_eq!(
        cancel.binding,
        ChannelBinding::Amqp {
            exchange: String::new(),
            exchange_type: "direct".into(),
            routing_key: String::new(),
            queue: Some("order-cancellations".into()),
        }
    );
    assert_eq!(cancel.reply.as_ref().unwrap().channel, "orderCancelled");
    let effect = cancel.state_effect.as_ref().unwrap();
    assert_eq!(effect.id_field, "orderId");
    assert_eq!(effect.resource_type, "cancellations");
    assert!(!effect.delete);
}

#[test]
fn orders_amqp_26_snapshot() {
    insta::assert_yaml_snapshot!(orders(), {".source_digest" => "[digest]"});
}

#[test]
fn websocket_channel_path() {
    let b = compile(&fixture("chat-ws-2.6.yaml")).unwrap();
    let ids: Vec<&str> = b.channels.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["chatMessage", "sendMessage"]);
    for c in &b.channels {
        assert_eq!(
            c.binding,
            ChannelBinding::WebSocket {
                path: "/chat".into()
            }
        );
    }
    assert_eq!(
        b.channel("chatMessage").unwrap().direction,
        Direction::Publish
    );
    let send = b.channel("sendMessage").unwrap();
    assert_eq!(send.direction, Direction::Subscribe);
    assert_eq!(send.reply.as_ref().unwrap().channel, "chatMessage");
}

#[test]
fn avro_payload_converted() {
    let b = compile(&fixture("users-avro-2.6.yaml")).unwrap();
    assert!(b.defs.contains_key("com.example.User"));
    assert!(b.defs.contains_key("com.example.Status"));
    let c = b.channel("userCreated").unwrap();
    assert_eq!(c.direction, Direction::Publish);
    let schema = b.with_defs(&c.message_schema);
    let validator = jsonschema::draft202012::new(&schema).unwrap();
    let mut ok = json!({
        "id": "6f1c2a7e-6b8a-4d3c-9a51-0b1f9e0d4c11",
        "name": "a",
        "age": null,
        "created": "2024-01-01T00:00:00Z",
        "status": "ACTIVE",
        "tags": [],
        "attrs": {}
    });
    assert!(validator.is_valid(&ok), "{schema:#}");
    ok["status"] = json!("X");
    assert!(!validator.is_valid(&ok));
    let required = c.message_schema["required"].as_array().unwrap();
    assert!(!required.contains(&json!("age")));
    assert!(required.contains(&json!("id")));
    assert_eq!(
        c.message_schema["properties"]["created"],
        json!({"type": "string", "format": "date-time"})
    );
}

fn doc(channels: &str) -> String {
    format!(
        "asyncapi: 2.6.0\ninfo: {{title: T, version: '1'}}\nservers:\n  k: {{url: 'localhost:9092', protocol: kafka}}\nchannels:\n{channels}"
    )
}

#[test]
fn unknown_schema_format_unsupported() {
    let src = doc(
        "  users:\n    subscribe:\n      message:\n        schemaFormat: application/vnd.google.protobuf;version=3\n        payload: 'message User {}'\n",
    );
    match compile(src.as_bytes()) {
        Err(ImportError::Unsupported(m)) => {
            assert!(
                m.starts_with(
                    "schemaFormat application/vnd.google.protobuf;version=3 at /channels/users"
                ),
                "{m}"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn missing_protocol_invalid() {
    let src = "asyncapi: 2.6.0\ninfo: {title: T, version: '1'}\nchannels:\n  a/b:\n    subscribe:\n      message: {payload: {type: string}}\n";
    let (pointer, message) = invalid(src);
    assert_eq!(pointer, "/channels/a~1b");
    assert_eq!(
        message,
        "cannot determine the broker protocol: add channel bindings or servers"
    );
}

#[test]
fn reply_to_unknown_channel_invalid() {
    let src = doc(
        "  in:\n    publish:\n      operationId: take\n      x-mockingbird-reply: nowhere\n      message: {payload: {type: string}}\n",
    );
    let (pointer, message) = invalid(&src);
    assert_eq!(pointer, "/channels/in/publish/x-mockingbird-reply");
    assert!(message.contains("nowhere"), "{message}");
}

#[test]
fn runaway_schedule_extension_invalid() {
    let src = doc(
        "  out:\n    subscribe:\n      x-mockingbird-triggers: [{\"schedule\": {\"every_ms\": 50}}]\n      message: {payload: {type: string}}\n",
    );
    let (pointer, message) = invalid(&src);
    assert!(pointer.ends_with("/x-mockingbird-triggers"), "{pointer}");
    assert_eq!(message, "schedule interval must be at least 100ms");
}

#[test]
fn trigger_validation_rules() {
    assert_eq!(
        validate_triggers(&[Trigger::Manual], Direction::Subscribe).unwrap_err(),
        "triggers apply only to channels this mock publishes"
    );
    assert_eq!(
        validate_triggers(
            &[Trigger::Schedule {
                every: Duration::from_millis(99)
            }],
            Direction::Publish
        )
        .unwrap_err(),
        "schedule interval must be at least 100ms"
    );
    let err = validate_triggers(
        &[Trigger::Cron {
            expr: "not a cron".into(),
        }],
        Direction::Publish,
    )
    .unwrap_err();
    assert!(
        err.starts_with("invalid cron expression \"not a cron\": "),
        "{err}"
    );
    assert_eq!(
        validate_triggers(
            &[Trigger::OnStateChange {
                resource_type: String::new(),
                kind: StateChangeKind::Any
            }],
            Direction::Publish
        )
        .unwrap_err(),
        "on_state_change needs a resource_type"
    );
    let ok: Vec<Trigger> =
        serde_json::from_value(json!([{"cron": {"expr": "*/5 * * * *"}}, "manual"])).unwrap();
    assert_eq!(validate_triggers(&ok, Direction::Publish), Ok(()));
    assert_eq!(validate_triggers(&[], Direction::Subscribe), Ok(()));
}

#[test]
fn external_ref_invalid() {
    let src = doc(
        "  users:\n    subscribe:\n      message:\n        $ref: 'messages.yaml#/UserSignedUp'\n",
    );
    let (pointer, message) = invalid(&src);
    assert_eq!(pointer, "/channels/users/subscribe/message");
    assert_eq!(message, "external $ref is not supported");
}

#[test]
fn duplicate_operation_id_invalid() {
    let src = doc(
        "  a:\n    subscribe:\n      operationId: same\n      message: {payload: {type: string}}\n  b:\n    subscribe:\n      operationId: same\n      message: {payload: {type: string}}\n",
    );
    let (pointer, _) = invalid(&src);
    assert_eq!(pointer, "/channels/b/subscribe/operationId");
}

#[test]
fn generated_ids_and_one_of_messages() {
    let src = doc(
        "  user/{id}/events:\n    subscribe:\n      message:\n        oneOf:\n          - {name: created, payload: {type: object}}\n          - {payload: {type: string}}\n",
    );
    let b = compile(src.as_bytes()).unwrap();
    let c = &b.channels[0];
    assert_eq!(c.id, "publish-user-id-events");
    assert_eq!(c.title, "user/{id}/events");
    assert_eq!(
        c.message_names,
        ["created", "publish-user-id-events-message-2"]
    );
    assert_eq!(
        c.message_schema,
        json!({"oneOf": [{"type": "object"}, {"type": "string"}]})
    );
    assert_eq!(c.parameters["id"], json!({"type": "string"}));
}

#[test]
fn state_effect_on_publish_channel_invalid() {
    let src = doc(
        "  out:\n    subscribe:\n      x-mockingbird-state: {resource_type: users, id_field: id}\n      message: {payload: {type: string}}\n",
    );
    let (pointer, _) = invalid(&src);
    assert_eq!(pointer, "/channels/out/subscribe/x-mockingbird-state");
}

#[test]
fn asyncapi3_unsupported_until_compiled() {
    match compile(b"asyncapi: 3.0.0\ninfo: {title: T, version: '1'}\n") {
        Err(ImportError::Unsupported(m)) => assert_eq!(m, "asyncapi 3.0"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn avro_name_colliding_with_component_schema_invalid() {
    let src = format!(
        "{}components:\n  schemas:\n    com.example.User: {{type: object}}\n",
        doc(
            "  users:\n    subscribe:\n      message:\n        schemaFormat: application/vnd.apache.avro;version=1.9.0\n        payload: {type: record, name: User, namespace: com.example, fields: [{name: id, type: string}]}\n"
        )
    );
    let (pointer, message) = invalid(&src);
    assert_eq!(pointer, "/channels/users/subscribe/message/payload");
    assert!(message.contains("com.example.User"), "{message}");
}
