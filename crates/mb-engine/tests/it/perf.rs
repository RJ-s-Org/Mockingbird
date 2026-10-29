//! Schema-keyed validator cache (narrowed GraphQL selections and the
//! faker's event key/header/parameter schemas): results stay per schema,
//! and an ignored timing harness shows the cost per call.

use std::time::Instant;

use mb_engine::{Engine, EngineConfig, GenerationContext, OperationCall};
use mb_mcm::Bundle;
use serde_json::{Map, Value, json};

use crate::support::{orders, scripted, wide};

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

/// Mean time of one `f()` over `n` calls.
fn per_call(n: usize, mut f: impl FnMut()) -> std::time::Duration {
    let started = Instant::now();
    for _ in 0..n {
        f();
    }
    started.elapsed() / n as u32
}

fn success_schema(bundle: &Bundle, op: &str) -> Value {
    bundle
        .operation(op)
        .unwrap()
        .success_output()
        .unwrap()
        .schema
        .clone()
        .unwrap()
}

fn get_item() -> OperationCall {
    let mut path_params = Map::new();
    path_params.insert("id".into(), json!("i1"));
    OperationCall {
        operation_id: "getItem".into(),
        path_params,
        ..OperationCall::default()
    }
}

const STRIPE: &str = "Ids use Stripe prefixes (pi_, cus_, pm_) followed by 24 random alphanumeric chars. A PaymentIntent always has client_secret = \"<id>_secret_<24 random chars>\".";

/// Timing harness of the engine hot paths (MB-ENGINE-PERF-*):
/// `cargo test --release -p mockingbird-engine --test it -- --ignored
/// --nocapture perf::time_`.
#[tokio::test]
#[ignore = "timing harness"]
async fn time_engine_hot_paths() {
    use mb_engine::persona::PersonaFormats;
    use mb_engine::{Faker, SchemaValidator};

    let bundle = wide(4, 12);
    let item = success_schema(&bundle, "getItem");
    let with_defs = bundle.with_reachable_defs(&item);
    let engine = Engine::new(None, EngineConfig::default());

    // PERF-3: the pattern regexes of one schema, compiled per fake call.
    for n in [200usize, 2000] {
        let faker = Faker::new(6);
        let mut rng = rand::rng();
        let per = per_call(n, || {
            faker.fake(&with_defs, &mut rng);
        });
        println!("PERF-3 Faker::fake (48 patterned fields) x{n}: {per:?}");
    }
    // PERF-4: Engine::fake rebuilds the reachable-defs schema and costs.
    for n in [200usize, 2000] {
        let per = per_call(n, || {
            engine.fake(&bundle, &item);
        });
        println!("PERF-4 Engine::fake (4 defs) x{n}: {per:?}");
    }
    // PERF-3/4/7 together: an offline answer.
    let op = bundle.operation("getItem").unwrap();
    for n in [200usize, 2000] {
        let started = Instant::now();
        for _ in 0..n {
            engine
                .generate(&bundle, op, &get_item(), GenerationContext::default())
                .await
                .unwrap();
        }
        println!(
            "PERF-3/4/7 offline generate getItem x{n}: {:?}",
            started.elapsed() / n as u32
        );
    }

    // PERF-6: persona templates checked on every object of a list.
    let formats = PersonaFormats::parse(STRIPE);
    let any = SchemaValidator::new(&json!({})).unwrap();
    for n in [100usize, 1000] {
        let id = format!("pi_{}", "A".repeat(24));
        let object = json!({"id": id, "client_secret": format!("{id}_secret_{}", "b".repeat(24))});
        let body = Value::Array(vec![object; n]);
        let per = per_call(20, || {
            assert!(formats.apply(&any, &body, &[]).is_none());
        });
        println!("PERF-6 persona apply, {n} conforming objects: {per:?}");
    }

    // PERF-7: a boolean check of an invalid value with 12 errors.
    let validator = SchemaValidator::new(&with_defs).unwrap();
    let mut bad = engine.fake(&bundle, &item);
    for j in 0..12 {
        bad[format!("f{j}")] = json!("BAD");
    }
    let per = per_call(20_000, || {
        assert!(validator.validate(&bad).is_err());
    });
    println!("PERF-7 validate(..).is_err(), 12 errors: {per:?}");
    let per = per_call(20_000, || {
        assert!(!validator.is_valid(&bad));
    });
    println!("PERF-7 is_valid(..), 12 errors: {per:?}");

    // PERF-10/11: a create whose request body has one member the answer
    // rejects, so each member is overlaid on its own.
    let wide = wide(1, 40);
    let create = wide.operation("createItem").unwrap();
    let mut body = engine.fake(&wide, &success_schema(&wide, "createItem"));
    body["f0"] = json!("BAD");
    let call = OperationCall {
        operation_id: "createItem".into(),
        body: Some(body),
        ..OperationCall::default()
    };
    let started = Instant::now();
    for _ in 0..500 {
        engine
            .generate(&wide, create, &call, GenerationContext::default())
            .await
            .unwrap();
    }
    println!(
        "PERF-10/11 offline createItem, 40 fields, 1 rejected x500: {:?}",
        started.elapsed() / 500
    );
}

/// Timing harness of local patching (MB-ENGINE-PERF-15): an LLM answer
/// with 12 pattern errors in a contract of 8 definitions.
#[tokio::test]
#[ignore = "timing harness"]
async fn time_local_patch() {
    let bundle = wide(8, 12);
    let item = success_schema(&bundle, "getItem");
    let offline = Engine::new(None, EngineConfig::default());
    let mut answer = offline.fake(&bundle, &item);
    for j in 0..12 {
        answer[format!("f{j}")] = json!("BAD");
    }
    let n = 500;
    let (_, engine) = scripted((0..n).map(|_| Ok(answer.to_string())).collect());
    let op = bundle.operation("getItem").unwrap();
    let started = Instant::now();
    for _ in 0..n {
        let response = engine
            .generate(&bundle, op, &get_item(), GenerationContext::default())
            .await
            .unwrap();
        assert_eq!(response.source, mb_engine::Source::Repaired);
    }
    println!(
        "PERF-15 patched LLM answer (12 errors, 8 defs) x{n}: {:?}",
        started.elapsed() / n as u32
    );
}
