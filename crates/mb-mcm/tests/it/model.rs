use mb_mcm::*;
use serde_json::json;

fn op(id: &str, kind: OperationKind, outputs: Vec<OutputVariant>) -> Operation {
    Operation {
        id: id.into(),
        protocol: Protocol::Http,
        binding: Binding::Http {
            method: HttpMethod::Get,
            path: "/orders/{id}".into(),
        },
        kind,
        summary: Some("s".into()),
        input: InputSchemas {
            path: Some(json!({"type": "object"})),
            query: None,
            headers: None,
            body: None,
            body_required: false,
        },
        outputs,
        resource_type: Some("orders".into()),
        identity: Some(Identity {
            path_param: Some("id".into()),
            field: "id".into(),
            input_field: None,
            path_field: None,
        }),
        semantic_hints: vec!["orders".into()],
        security: Vec::new(),
    }
}

fn out(status: StatusSelector) -> OutputVariant {
    OutputVariant {
        status,
        content_type: Some("application/json".into()),
        schema: None,
        headers: Default::default(),
    }
}

#[test]
fn bundle_round_trips_through_json() {
    let mut defs = serde_json::Map::new();
    defs.insert("Order".into(), json!({"type": "object"}));
    let bundle = Bundle {
        id: "orders-api".into(),
        title: "Orders API".into(),
        version: "1.0.0".into(),
        source_digest: "abc".into(),
        operations: vec![
            op(
                "createOrder",
                OperationKind::Command(Effect::Create),
                vec![out(StatusSelector::Code(201))],
            ),
            op(
                "getOrder",
                OperationKind::Query,
                vec![out(StatusSelector::Default)],
            ),
        ],
        defs,
        protocol_payload: None,
        channels: Vec::new(),
        webhooks: Vec::new(),
    };
    let value = serde_json::to_value(&bundle).unwrap();
    let back: Bundle = serde_json::from_value(value).unwrap();
    assert_eq!(bundle, back);
    assert!(bundle.operation("getOrder").is_some());
    assert!(bundle.operation("nope").is_none());
}

#[test]
fn success_output_prefers_lowest_2xx() {
    let o = op(
        "a",
        OperationKind::Query,
        vec![
            out(StatusSelector::Code(404)),
            out(StatusSelector::Code(201)),
            out(StatusSelector::Code(200)),
            out(StatusSelector::Default),
        ],
    );
    assert_eq!(
        o.success_output().unwrap().status,
        StatusSelector::Code(200)
    );
}

#[test]
fn success_output_falls_back_to_default() {
    let o = op(
        "a",
        OperationKind::Query,
        vec![out(StatusSelector::Code(400)), out(StatusSelector::Default)],
    );
    assert_eq!(o.success_output().unwrap().status, StatusSelector::Default);
}

#[test]
fn output_for_status_uses_default_when_missing() {
    let o = op(
        "a",
        OperationKind::Query,
        vec![out(StatusSelector::Code(200)), out(StatusSelector::Default)],
    );
    assert_eq!(
        o.output_for_status(418).unwrap().status,
        StatusSelector::Default
    );
    let o = op(
        "a",
        OperationKind::Query,
        vec![out(StatusSelector::Code(200))],
    );
    assert!(o.output_for_status(418).is_none());
}

#[test]
fn error_output_prefers_listed_then_any_4xx() {
    let o = op(
        "a",
        OperationKind::Query,
        vec![
            out(StatusSelector::Code(422)),
            out(StatusSelector::Code(404)),
        ],
    );
    assert_eq!(
        o.error_output(&[404, 400]).unwrap().status,
        StatusSelector::Code(404)
    );
    assert_eq!(
        o.error_output(&[400]).unwrap().status,
        StatusSelector::Code(422)
    );
}

#[test]
fn with_defs_attaches_definitions() {
    let mut defs = serde_json::Map::new();
    let order = json!({"type": "object"});
    defs.insert("Order".into(), order.clone());
    let bundle = Bundle {
        id: "x".into(),
        title: "x".into(),
        version: "1".into(),
        source_digest: "d".into(),
        operations: vec![],
        defs,
        protocol_payload: None,
        channels: Vec::new(),
        webhooks: Vec::new(),
    };
    let v = bundle.with_defs(&json!({"$ref":"#/$defs/Order"}));
    assert_eq!(v["$defs"]["Order"], order);
}

#[test]
fn with_reachable_defs_attaches_only_the_ref_closure() {
    let mut defs = serde_json::Map::new();
    defs.insert(
        "Order".into(),
        json!({"type": "object", "properties": {
            "lines": {"type": "array", "items": {"$ref": "#/$defs/Line"}},
            "category": {"$ref": "#/$defs/Category"}
        }}),
    );
    defs.insert("Line".into(), json!({"allOf": [{"$ref": "#/$defs/Money"}]}));
    defs.insert("Money".into(), json!({"type": "number"}));
    defs.insert(
        "Category".into(),
        json!({"type": "object", "properties": {"parent": {"$ref": "#/$defs/Category"}}}),
    );
    defs.insert("Unrelated".into(), json!({"type": "string"}));
    defs.insert("a/b".into(), json!({"type": "boolean"}));
    let bundle = Bundle {
        id: "x".into(),
        title: "x".into(),
        version: "1".into(),
        source_digest: "d".into(),
        operations: vec![],
        defs,
        protocol_payload: None,
        channels: Vec::new(),
        webhooks: Vec::new(),
    };

    let v = bundle.with_reachable_defs(&json!({"$ref": "#/$defs/Order"}));
    let names: Vec<&String> = v["$defs"].as_object().unwrap().keys().collect();
    let mut names: Vec<&str> = names.into_iter().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["Category", "Line", "Money", "Order"]);

    // JSON-pointer escapes in def names, and nested pointers into a def.
    let v = bundle.with_reachable_defs(&json!({"anyOf": [
        {"$ref": "#/$defs/a~1b"},
        {"$ref": "#/$defs/Category/properties/parent"}
    ]}));
    let defs = v["$defs"].as_object().unwrap();
    assert_eq!(defs.len(), 2, "{defs:?}");
    assert!(defs.contains_key("a/b") && defs.contains_key("Category"));

    // No refs: an empty `$defs`; boolean schemas are wrapped.
    let v = bundle.with_reachable_defs(&json!({"type": "string"}));
    assert!(v["$defs"].as_object().unwrap().is_empty());
    let v = bundle.with_reachable_defs(&json!(true));
    assert_eq!(v["allOf"], json!([true]));
}

/// A P2-era orders bundle: no `protocol_payload`, no `input_field`.
const P2_ORDERS: &str = r##"{"id":"orders-api","title":"Orders API","version":"1.0.0","source_digest":"abc","operations":[{"id":"getOrder","protocol":"http","binding":{"http":{"method":"get","path":"/orders/{orderId}"}},"kind":"query","summary":"Get an order","input":{"path":{"type":"object"},"query":null,"headers":null,"body":null,"body_required":false},"outputs":[{"status":{"code":200},"content_type":"application/json","schema":{"$ref":"#/$defs/Order"}}],"resource_type":"orders","identity":{"path_param":"orderId","field":"id"},"semantic_hints":["orders"]},{"id":"createOrder","protocol":"http","binding":{"http":{"method":"post","path":"/orders"}},"kind":{"command":"create"},"summary":null,"input":{"path":null,"query":null,"headers":null,"body":{"$ref":"#/$defs/Order"},"body_required":true},"outputs":[{"status":"default","content_type":null,"schema":null}],"resource_type":"orders","identity":null,"semantic_hints":[]}],"defs":{"Order":{"type":"object"}}}"##;

#[test]
fn p2_bundle_without_payload_deserializes() {
    let bundle: Bundle = serde_json::from_str(P2_ORDERS).unwrap();
    assert_eq!(bundle.protocol_payload, None);
    let get = bundle.operation("getOrder").unwrap();
    assert_eq!(get.identity.as_ref().unwrap().input_field, None);
    assert!(bundle.channels.is_empty());
    let text = serde_json::to_string(&bundle).unwrap();
    assert_eq!(text, P2_ORDERS);
    assert!(!text.contains("\"channels\""));
}

#[test]
fn grpc_payload_round_trips_base64() {
    let payload = ProtocolPayload::Grpc {
        file_descriptor_set: vec![0, 1, 2, 255],
    };
    let v = serde_json::to_value(&payload).unwrap();
    assert_eq!(v, json!({"grpc": {"file_descriptor_set": "AAEC/w=="}}));
    let back: ProtocolPayload = serde_json::from_value(v).unwrap();
    assert_eq!(back, payload);
    assert!(
        serde_json::from_value::<ProtocolPayload>(
            json!({"grpc": {"file_descriptor_set": "not base64!"}})
        )
        .is_err()
    );
    for (p, tag) in [
        (
            ProtocolPayload::Graphql {
                sdl: "type Query { a: Int }".into(),
            },
            "graphql",
        ),
        (
            ProtocolPayload::Soap {
                wsdl: "<definitions/>".into(),
            },
            "soap",
        ),
    ] {
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get(tag).is_some(), "{v}");
        assert_eq!(serde_json::from_value::<ProtocolPayload>(v).unwrap(), p);
    }
}

#[test]
fn new_bindings_and_kinds_round_trip() {
    let b = json!({"graphql": {"operation_type": "subscription", "field": "reviewAdded"}});
    let binding: Binding = serde_json::from_value(b.clone()).unwrap();
    assert_eq!(
        binding,
        Binding::Graphql {
            operation_type: GraphqlOperationType::Subscription,
            field: "reviewAdded".into(),
        }
    );
    assert_eq!(serde_json::to_value(&binding).unwrap(), b);

    let kind: OperationKind = serde_json::from_value(json!("stream_bidi")).unwrap();
    assert_eq!(kind, OperationKind::StreamBidi);
    assert_eq!(serde_json::to_value(kind).unwrap(), json!("stream_bidi"));
    assert!(kind.streams_request() && kind.streams_response());
    assert!(OperationKind::StreamServer.streams_response());
    assert!(!OperationKind::StreamServer.streams_request());
    assert!(OperationKind::StreamClient.streams_request());
    assert!(!OperationKind::StreamClient.streams_response());
    assert!(!OperationKind::Query.streams_request() && !OperationKind::Query.streams_response());
    assert_eq!(OperationKind::StreamClient.as_str(), "stream-client");
    assert_eq!(OperationKind::Command(Effect::Delete).as_str(), "delete");

    let soap = Binding::Soap {
        service: "Calculator".into(),
        port: "CalculatorSoap12".into(),
        operation: "Add".into(),
        soap_action: Some("http://tempuri.org/Add".into()),
        version: SoapVersion::Soap12,
        address_path: "/calculator.asmx".into(),
    };
    let v = serde_json::to_value(&soap).unwrap();
    assert_eq!(v["soap"]["version"], "soap12");
    assert_eq!(serde_json::from_value::<Binding>(v).unwrap(), soap);
    assert_eq!(serde_json::to_value(SoapVersion::Soap11).unwrap(), "soap11");

    let identity: Identity =
        serde_json::from_value(json!({"path_param": null, "field": "id", "input_field": "id"}))
            .unwrap();
    assert_eq!(identity.input_field.as_deref(), Some("id"));
}

#[test]
fn bundle_protocol_is_first_operations() {
    let mut bundle: Bundle = serde_json::from_str(P2_ORDERS).unwrap();
    assert_eq!(bundle.protocol(), Protocol::Http);
    bundle.operations[0].protocol = Protocol::Grpc;
    assert_eq!(bundle.protocol(), Protocol::Grpc);
    bundle.operations.clear();
    assert_eq!(bundle.protocol(), Protocol::Http);
}

#[test]
fn grpc_path_formats() {
    let b = Binding::Grpc {
        package: "helloworld".into(),
        service: "Greeter".into(),
        method: "SayHello".into(),
    };
    assert_eq!(
        b.grpc_path().as_deref(),
        Some("/helloworld.Greeter/SayHello")
    );
    let b = Binding::Grpc {
        package: String::new(),
        service: "Greeter".into(),
        method: "SayHello".into(),
    };
    assert_eq!(b.grpc_path().as_deref(), Some("/Greeter/SayHello"));
    let http = Binding::Http {
        method: HttpMethod::Get,
        path: "/x".into(),
    };
    assert_eq!(http.grpc_path(), None);
}

#[test]
fn operation_target_and_stream_item_schema() {
    let mut o = op(
        "a",
        OperationKind::Query,
        vec![OutputVariant {
            status: StatusSelector::Code(200),
            content_type: None,
            schema: Some(json!({"type": "string"})),
            headers: Default::default(),
        }],
    );
    assert_eq!(o.target(), "GET /orders/{id}");
    assert_eq!(o.stream_item_schema(), None);
    o.kind = OperationKind::StreamServer;
    assert_eq!(o.stream_item_schema(), Some(&json!({"type": "string"})));
    o.binding = Binding::Graphql {
        operation_type: GraphqlOperationType::Query,
        field: "user".into(),
    };
    assert_eq!(o.target(), "query user");
    o.binding = Binding::Grpc {
        package: "pkg".into(),
        service: "Svc".into(),
        method: "M".into(),
    };
    assert_eq!(o.target(), "/pkg.Svc/M");
    o.binding = Binding::Soap {
        service: "Calc".into(),
        port: "CalcSoap".into(),
        operation: "Add".into(),
        soap_action: None,
        version: SoapVersion::Soap11,
        address_path: "/calc".into(),
    };
    assert_eq!(o.target(), "Calc/CalcSoap Add");
}

/// A pre-P4 bundle that uses the P3 fields (`protocol_payload`,
/// `input_field`) but has no channels: the later fields survive a round
/// trip and `channels` stays absent from the output.
#[test]
fn p4_bundle_without_channels_round_trips() {
    let mut bundle: Bundle = serde_json::from_str(P2_ORDERS).unwrap();
    bundle.protocol_payload = Some(ProtocolPayload::Grpc {
        file_descriptor_set: vec![7, 8, 9],
    });
    bundle.operations[0].identity.as_mut().unwrap().input_field = Some("orderId".into());
    let text = serde_json::to_string(&bundle).unwrap();
    assert!(!text.contains("\"channels\""), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["protocol_payload"],
        json!({"grpc": {"file_descriptor_set": "BwgJ"}})
    );
    assert_eq!(v["operations"][0]["identity"]["input_field"], "orderId");
    let back: Bundle = serde_json::from_str(&text).unwrap();
    assert!(back.channels.is_empty());
    assert_eq!(back, bundle);
}

fn kafka_channel() -> Channel {
    let mut parameters = serde_json::Map::new();
    parameters.insert("region".into(), json!({"type": "string"}));
    Channel {
        id: "orderCreated".into(),
        title: "Order created".into(),
        address: "orders.{region}.created".into(),
        binding: ChannelBinding::Kafka {
            topic: "orders.{region}.created".into(),
            partitions_hint: Some(3),
        },
        direction: Direction::Publish,
        message_schema: json!({"$ref": "#/$defs/Order"}),
        message_names: vec!["OrderCreated".into()],
        content_type: "application/json".into(),
        key_schema: Some(json!({"type": "string"})),
        headers_schema: Some(json!({"type": "object"})),
        parameters,
        triggers: vec![
            Trigger::Schedule {
                every: std::time::Duration::from_secs(10),
            },
            Trigger::Cron {
                expr: "*/5 * * * *".into(),
            },
            Trigger::OnStateChange {
                resource_type: "orders".into(),
                kind: StateChangeKind::Upserted,
            },
            Trigger::Manual,
        ],
        reply: Some(Reply {
            channel: "orderReplies".into(),
            address_header: Some("reply-to".into()),
        }),
        state_effect: Some(StateEffect {
            resource_type: "orders".into(),
            id_field: "id".into(),
            delete: false,
        }),
        semantic_hints: vec!["orders".into()],
    }
}

#[test]
fn channel_round_trips() {
    let channel = kafka_channel();
    let expected = json!({
        "id": "orderCreated",
        "title": "Order created",
        "address": "orders.{region}.created",
        "binding": {"kafka": {"topic": "orders.{region}.created", "partitions_hint": 3}},
        "direction": "publish",
        "message_schema": {"$ref": "#/$defs/Order"},
        "message_names": ["OrderCreated"],
        "content_type": "application/json",
        "key_schema": {"type": "string"},
        "headers_schema": {"type": "object"},
        "parameters": {"region": {"type": "string"}},
        "triggers": [
            {"schedule": {"every_ms": 10000}},
            {"cron": {"expr": "*/5 * * * *"}},
            {"on_state_change": {"resource_type": "orders", "kind": "upserted"}},
            "manual"
        ],
        "reply": {"channel": "orderReplies", "address_header": "reply-to"},
        "state_effect": {"resource_type": "orders", "id_field": "id", "delete": false},
        "semantic_hints": ["orders"]
    });
    let v = serde_json::to_value(&channel).unwrap();
    assert_eq!(v, expected);
    let back: Channel = serde_json::from_value(v).unwrap();
    assert_eq!(back, channel);

    // Inside a bundle, and found by id.
    let mut bundle: Bundle = serde_json::from_str(P2_ORDERS).unwrap();
    bundle.channels.push(channel.clone());
    let text = serde_json::to_string(&bundle).unwrap();
    let back: Bundle = serde_json::from_str(&text).unwrap();
    assert_eq!(back, bundle);
    assert_eq!(bundle.channel("orderCreated"), Some(&channel));
    assert!(bundle.channel("nope").is_none());

    // Optional fields are omitted when empty.
    let minimal = Channel {
        key_schema: None,
        headers_schema: None,
        parameters: serde_json::Map::new(),
        triggers: vec![],
        reply: None,
        state_effect: None,
        semantic_hints: vec![],
        ..channel
    };
    let v = serde_json::to_value(&minimal).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    for absent in [
        "key_schema",
        "headers_schema",
        "parameters",
        "triggers",
        "reply",
        "state_effect",
        "semantic_hints",
    ] {
        assert!(!keys.contains(&absent), "{absent} in {v}");
    }
    assert_eq!(serde_json::from_value::<Channel>(v).unwrap(), minimal);
}

#[test]
fn websocket_binding_serializes_as_websocket() {
    let b = ChannelBinding::WebSocket {
        path: "/chat".into(),
    };
    let v = serde_json::to_value(&b).unwrap();
    assert_eq!(v, json!({"websocket": {"path": "/chat"}}));
    assert_eq!(serde_json::from_value::<ChannelBinding>(v).unwrap(), b);
}

#[test]
fn state_change_kind_defaults_to_any() {
    assert_eq!(StateChangeKind::default(), StateChangeKind::Any);
    let t: Trigger =
        serde_json::from_value(json!({"on_state_change": {"resource_type": "orders"}})).unwrap();
    assert_eq!(
        t,
        Trigger::OnStateChange {
            resource_type: "orders".into(),
            kind: StateChangeKind::Any,
        }
    );
    assert_eq!(
        serde_json::to_value(StateChangeKind::Deleted).unwrap(),
        json!("deleted")
    );
}

#[test]
fn effective_triggers_defaults() {
    let mut c = kafka_channel();
    c.triggers.clear();
    assert_eq!(c.effective_triggers(), vec![Trigger::Manual]);
    c.triggers = vec![Trigger::Cron {
        expr: "* * * * *".into(),
    }];
    assert_eq!(c.effective_triggers(), c.triggers);
    c.direction = Direction::Subscribe;
    assert_eq!(c.effective_triggers(), vec![]);
    c.triggers.clear();
    assert_eq!(c.effective_triggers(), vec![]);
    assert_eq!(MIN_SCHEDULE_INTERVAL, std::time::Duration::from_millis(100));
}

#[test]
fn normalized_cron_adds_seconds() {
    assert_eq!(Trigger::normalized_cron("*/5 * * * *"), "0 */5 * * * *");
    assert_eq!(Trigger::normalized_cron("  */5 * * * *  "), "0 */5 * * * *");
    assert_eq!(Trigger::normalized_cron("30 */5 * * * *"), "30 */5 * * * *");
    assert_eq!(
        Trigger::normalized_cron(" 30 */5 * * * * "),
        "30 */5 * * * *"
    );
}

#[test]
fn schedule_every_ms_is_whole_milliseconds() {
    let t = Trigger::Schedule {
        every: std::time::Duration::from_micros(1_500_900),
    };
    assert_eq!(
        serde_json::to_value(&t).unwrap(),
        json!({"schedule": {"every_ms": 1500}})
    );
    let back: Trigger = serde_json::from_value(json!({"schedule": {"every_ms": 250}})).unwrap();
    assert_eq!(
        back,
        Trigger::Schedule {
            every: std::time::Duration::from_millis(250)
        }
    );
}

#[test]
fn binding_protocol_and_address() {
    let cases = [
        (
            ChannelBinding::Kafka {
                topic: "orders".into(),
                partitions_hint: None,
            },
            "kafka",
            "orders",
        ),
        (
            ChannelBinding::Amqp {
                exchange: "events".into(),
                exchange_type: "topic".into(),
                routing_key: "orders.created".into(),
                queue: Some("q".into()),
            },
            "amqp",
            "orders.created",
        ),
        (
            ChannelBinding::Amqp {
                exchange: String::new(),
                exchange_type: "direct".into(),
                routing_key: String::new(),
                queue: Some("work".into()),
            },
            "amqp",
            "work",
        ),
        (
            ChannelBinding::Mqtt {
                topic: "sensors/{id}/temp".into(),
                qos: 1,
                retain: false,
            },
            "mqtt",
            "sensors/{id}/temp",
        ),
        (
            ChannelBinding::WebSocket {
                path: "/chat".into(),
            },
            "websocket",
            "/chat",
        ),
    ];
    for (b, protocol, address) in cases {
        assert_eq!(b.protocol(), protocol);
        assert_eq!(b.address(), address);
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(serde_json::from_value::<ChannelBinding>(v).unwrap(), b);
    }
}

#[test]
fn grpc_services_lists_full_service_names() {
    let grpc = |package: &str, service: &str, method: &str| {
        let mut o = op(method, OperationKind::Query, vec![]);
        o.protocol = Protocol::Grpc;
        o.binding = Binding::Grpc {
            package: package.into(),
            service: service.into(),
            method: method.into(),
        };
        o
    };
    let bundle = Bundle {
        id: "b".into(),
        title: "b".into(),
        version: "1".into(),
        source_digest: String::new(),
        operations: vec![
            grpc("routeguide", "RouteGuide", "GetFeature"),
            grpc("routeguide", "RouteGuide", "ListFeatures"),
            grpc("", "Bare", "Ping"),
            op("http", OperationKind::Query, vec![]),
        ],
        defs: serde_json::Map::new(),
        protocol_payload: None,
        channels: vec![],
        webhooks: Vec::new(),
    };
    let services: Vec<String> = bundle.grpc_services().into_iter().collect();
    assert_eq!(services, ["Bare", "routeguide.RouteGuide"]);
}

#[test]
fn string_forms_match_display() {
    for (m, text) in [(HttpMethod::Get, "GET"), (HttpMethod::Options, "OPTIONS")] {
        assert_eq!(m.as_str(), text);
        assert_eq!(m.to_string(), text);
    }
    assert_eq!(GraphqlOperationType::Mutation.to_string(), "mutation");
    let kind = OperationKind::Command(Effect::Update);
    assert_eq!(kind.as_str(), "update");
    assert_eq!(kind.to_string(), kind.as_str());
}

#[test]
fn effective_trigger_slice_matches_effective_triggers() {
    let mut channel: Channel = serde_json::from_value(json!({
        "id": "orders",
        "title": "Orders",
        "address": "orders",
        "binding": { "kafka": { "topic": "orders" } },
        "direction": "publish",
        "message_schema": { "type": "object" },
        "message_names": [],
        "content_type": "application/json"
    }))
    .unwrap();
    assert_eq!(channel.effective_trigger_slice(), &[Trigger::Manual]);
    assert_eq!(channel.effective_triggers(), vec![Trigger::Manual]);
    channel.triggers = vec![Trigger::Cron {
        expr: "* * * * *".into(),
    }];
    assert_eq!(channel.effective_trigger_slice(), channel.triggers.as_slice());
    channel.direction = Direction::Subscribe;
    assert!(channel.effective_trigger_slice().is_empty());
    assert!(channel.effective_triggers().is_empty());
}
