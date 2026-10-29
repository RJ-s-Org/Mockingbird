//! Schema-keyed validator cache (narrowed GraphQL selections and the
//! faker's event key/header/parameter schemas): results stay per schema,
//! and an ignored timing harness shows the cost per call.

use std::time::Instant;

use mb_engine::{Engine, EngineConfig, GenerationContext, OperationCall};
use mb_mcm::Bundle;
use serde_json::{Map, Value, json};

use crate::support::orders;

fn call() -> OperationCall {
    let mut path_params = Map::new();
    path_params.insert("orderId".into(), json!("o1"));
    OperationCall {
        operation_id: "getOrder".into(),
        path_params,
        ..OperationCall::default()
    }
}

fn narrowed(fields: &[&str]) -> Value {
    let properties: Map<String, Value> = fields
        .iter()
        .map(|f| ((*f).to_string(), json!({"type": "string", "minLength": 1})))
        .collect();
    json!({
        "type": "object",
        "properties": properties,
        "required": fields,
        "additionalProperties": false
    })
}

async fn generate(engine: &Engine, bundle: &Bundle, schema: &Value) -> Value {
    let op = bundle.operation("getOrder").unwrap();
    engine
        .generate(
            bundle,
            op,
            &call(),
            GenerationContext {
                response_schema: Some(schema),
                ..GenerationContext::default()
            },
        )
        .await
        .unwrap()
        .body
        .unwrap()
}

/// Each narrowed selection keeps its own validator: a second, different
/// selection is not answered with the first one's shape.
#[tokio::test]
async fn narrowed_validators_are_per_selection() {
    let engine = Engine::new(None, EngineConfig::default());
    let bundle = orders();
    for _ in 0..2 {
        let a = generate(&engine, &bundle, &narrowed(&["id"])).await;
        assert_eq!(a.as_object().unwrap().len(), 1, "{a}");
        let b = generate(&engine, &bundle, &narrowed(&["id", "customerId"])).await;
        assert_eq!(b.as_object().unwrap().len(), 2, "{b}");
        assert!(b["customerId"].is_string(), "{b}");
    }
}

/// `fake` validates against the schema it is given, cached or not.
#[test]
fn fake_is_per_schema() {
    let engine = Engine::new(None, EngineConfig::default());
    let bundle = orders();
    for _ in 0..2 {
        let a = engine.fake(
            &bundle,
            &json!({"type": "integer", "minimum": 5, "maximum": 5}),
        );
        assert_eq!(a, json!(5));
        let b = engine.fake(&bundle, &json!({"type": "string", "enum": ["x"]}));
        assert_eq!(b, json!("x"));
    }
}

/// Timing harness: `cargo test -p mockingbird-engine --test it -- --ignored
/// --nocapture perf::`.
#[tokio::test]
#[ignore = "timing harness"]
async fn time_narrowed_generate_and_fake() {
    let engine = Engine::new(None, EngineConfig::default());
    let bundle = orders();
    // A GraphQL-sized selection: a dozen patterned fields and a nested
    // object of more.
    let fields = [
        "id",
        "customerId",
        "status",
        "note",
        "sku",
        "region",
        "a",
        "b",
        "c",
        "d",
        "e",
        "f",
    ];
    let mut schema = narrowed(&fields);
    for f in fields {
        schema["properties"][f]["pattern"] = json!("^[A-Za-z0-9_-]{1,40}$");
    }
    schema["properties"]["owner"] = narrowed(&fields);
    schema["properties"]["owner"]["properties"]["login"] =
        json!({"type": "string", "format": "email"});
    for n in [200usize, 2000] {
        let started = Instant::now();
        for _ in 0..n {
            generate(&engine, &bundle, &schema).await;
        }
        let per = started.elapsed() / n as u32;
        println!("narrowed generate x{n}: {per:?} per call");
        let started = Instant::now();
        for _ in 0..n {
            engine.fake(&bundle, &schema);
        }
        let per = started.elapsed() / n as u32;
        println!("fake x{n}: {per:?} per call");
    }
}
