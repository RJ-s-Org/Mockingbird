#![allow(clippy::unwrap_used)]
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
        },
        outputs,
        resource_type: Some("orders".into()),
        identity: Some(Identity {
            path_param: Some("id".into()),
            field: "id".into(),
        }),
        semantic_hints: vec!["orders".into()],
    }
}

fn out(status: StatusSelector) -> OutputVariant {
    OutputVariant {
        status,
        content_type: Some("application/json".into()),
        schema: None,
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
    };
    let v = bundle.with_defs(&json!({"$ref":"#/$defs/Order"}));
    assert_eq!(v["$defs"]["Order"], order);
}
