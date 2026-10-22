#![allow(clippy::unwrap_used)]

use mb_engine::faker::Costs;
use mb_engine::{Engine, EngineConfig, Faker, GenerationContext, OperationCall, SchemaValidator};
use mb_import::compile;
use mb_mcm::Bundle;
use proptest::prelude::*;
use rand::SeedableRng;
use rand::rngs::StdRng;
use serde_json::{Value, json};

fn orders() -> Bundle {
    let path = format!(
        "{}/../../fixtures/openapi/orders-3.0.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    compile(&std::fs::read(path).unwrap()).unwrap()
}

fn check_def(name: &str) -> Vec<Value> {
    let schema = orders().with_defs(&json!({"$ref": format!("#/$defs/{name}")}));
    let validator = SchemaValidator::new(&schema).unwrap();
    let faker = Faker::default();
    (0..200u64)
        .map(|seed| {
            let mut rng = StdRng::seed_from_u64(seed);
            let v = faker.fake(&schema, &mut rng);
            if let Err(e) = validator.validate(&v) {
                panic!("seed {seed}: {e:?} for {v}");
            }
            v
        })
        .collect()
}

#[test]
fn fakes_orders_order_valid() {
    check_def("Order");
}

#[test]
fn fakes_recursive_schema_within_depth() {
    for v in check_def("Category") {
        assert!(serde_json::to_vec(&v).unwrap().len() < 64 * 1024);
    }
}

#[test]
fn fakes_formats() {
    let faker = Faker::default();
    let mut rng = StdRng::seed_from_u64(1);
    let v = faker.fake(&json!({"type":"string","format":"uuid"}), &mut rng);
    uuid::Uuid::parse_str(v.as_str().unwrap()).unwrap();
    let v = faker.fake(&json!({"type":"string","format":"date-time"}), &mut rng);
    chrono::DateTime::parse_from_rfc3339(v.as_str().unwrap()).unwrap();
    let v = faker.fake(&json!({"type":"string","format":"date"}), &mut rng);
    chrono::NaiveDate::parse_from_str(v.as_str().unwrap(), "%Y-%m-%d").unwrap();
}

#[test]
fn validator_reports_paths() {
    let schema = orders().with_defs(&json!({"$ref": "#/$defs/Order"}));
    let validator = SchemaValidator::new(&schema).unwrap();
    let bad = json!({"id": "x", "customerId": "c", "status": "pending"});
    let err = validator.validate(&bad).unwrap_err();
    assert!(err[0].contains("total"), "{err:?}");
}

fn leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(json!({"type": "boolean"})),
        Just(json!({"type": "string"})),
        (0u64..8, 0u64..8)
            .prop_map(|(a, b)| { json!({"type": "string", "minLength": a, "maxLength": a + b}) }),
        Just(json!({"type": "string", "format": "uuid"})),
        Just(json!({"type": "string", "format": "date-time"})),
        Just(json!({"type": "integer"})),
        (-50i64..50, 0i64..50)
            .prop_map(|(lo, d)| { json!({"type": "integer", "minimum": lo, "maximum": lo + d}) }),
        (-50i64..50, 1i64..10).prop_map(|(lo, d)| {
            json!({"type": "integer", "exclusiveMinimum": lo, "exclusiveMaximum": lo + d + 1})
        }),
        (1i64..6, 0i64..40).prop_map(|(m, lo)| {
            json!({"type": "integer", "multipleOf": m, "minimum": lo, "maximum": lo + 5 * m})
        }),
        Just(json!({"type": "number"})),
        (-50i64..50, 1i64..50)
            .prop_map(|(lo, d)| { json!({"type": "number", "minimum": lo, "maximum": lo + d}) }),
        (-50i64..50, 1i64..50).prop_map(|(lo, d)| {
            json!({"type": "number", "exclusiveMinimum": lo, "exclusiveMaximum": lo + d})
        }),
        Just(json!({"enum": ["a", 1, null, {"k": true}]})),
        Just(json!({"const": {"x": [1, 2]}})),
        Just(json!({"type": ["string", "null"]})),
        Just(json!({"type": ["integer", "null"], "minimum": 3})),
    ]
}

fn schema() -> impl Strategy<Value = Value> {
    leaf().prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            (inner.clone(), 0u64..3, 0u64..3).prop_map(|(items, min, extra)| {
                json!({"type": "array", "items": items, "minItems": min, "maxItems": min + extra})
            }),
            inner.clone().prop_map(|items| {
                json!({"type": "array", "items": items, "uniqueItems": false})
            }),
            prop::collection::btree_map("[a-z]{1,5}", (inner.clone(), any::<bool>()), 0..4)
                .prop_map(|m| {
                    let required: Vec<&String> =
                        m.iter().filter(|(_, (_, r))| *r).map(|(k, _)| k).collect();
                    let props: serde_json::Map<String, Value> =
                        m.iter().map(|(k, (s, _))| (k.clone(), s.clone())).collect();
                    json!({"type": "object", "properties": props, "required": required})
                }),
            inner
                .clone()
                .prop_map(|s| json!({"anyOf": [s, {"type": "null"}]})),
            (inner.clone(), inner).prop_map(|(a, b)| {
                json!({"allOf": [
                    {"type": "object", "properties": {"p": a}, "required": ["p"]},
                    {"type": "object", "properties": {"q": b}}
                ]})
            }),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn faker_output_always_valid(s in schema(), seed in any::<u64>(), depth in 0usize..8) {
        let validator = SchemaValidator::new(&s).unwrap();
        let mut rng = StdRng::seed_from_u64(seed);
        let v = Faker::new(depth).fake(&s, &mut rng);
        prop_assert!(validator.validate(&v).is_ok(), "schema {s} value {v}: {:?}", validator.validate(&v));
    }
}

/// `Expr: oneOf [And{left: Expr, right: Expr (required)}, Leaf]` — only the
/// leaf branch terminates.
pub fn expr_schema() -> Value {
    json!({
        "$ref": "#/$defs/Expr",
        "$defs": {
            "Expr": {"oneOf": [{"$ref": "#/$defs/And"}, {"$ref": "#/$defs/Leaf"}]},
            "And": {
                "type": "object",
                "required": ["op", "left", "right"],
                "properties": {
                    "op": {"const": "and"},
                    "left": {"$ref": "#/$defs/Expr"},
                    "right": {"$ref": "#/$defs/Expr"}
                }
            },
            "Leaf": {
                "type": "object",
                "required": ["value"],
                "properties": {"value": {"type": "integer"}}
            }
        }
    })
}

#[test]
fn fakes_required_recursive_one_of_without_overflow() {
    let schema = expr_schema();
    let validator = SchemaValidator::new(&schema).unwrap();
    for depth in [0usize, 1, 3, 6] {
        for seed in 0..50u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let v = Faker::new(depth).fake(&schema, &mut rng);
            assert!(
                validator.validate(&v).is_ok(),
                "depth {depth} seed {seed}: {v}"
            );
        }
    }
}

#[test]
fn hard_depth_cutoff_terminates_unsatisfiable_recursion() {
    // A required self-reference has no finite instance; the faker must still
    // return (with some value) instead of overflowing the stack.
    let schema = json!({
        "$ref": "#/$defs/Node",
        "$defs": {"Node": {
            "type": "object",
            "required": ["next"],
            "properties": {"next": {"$ref": "#/$defs/Node"}}
        }}
    });
    let mut rng = StdRng::seed_from_u64(7);
    let v = Faker::new(4).fake(&schema, &mut rng);
    assert!(v.is_object());
}

#[test]
fn fakes_number_multiple_of_with_exclusive_maximum() {
    let schema = json!({"type": "number", "multipleOf": 0.5, "minimum": 0, "exclusiveMaximum": 1});
    let validator = SchemaValidator::new(&schema).unwrap();
    for seed in 0..200u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let v = Faker::default().fake(&schema, &mut rng);
        assert!(validator.validate(&v).is_ok(), "seed {seed}: {v}");
    }
}

#[test]
fn fakes_long_min_length_in_linear_time() {
    let schema = json!({"type": "string", "minLength": 300_000});
    let start = std::time::Instant::now();
    let mut rng = StdRng::seed_from_u64(1);
    let v = Faker::default().fake(&schema, &mut rng);
    assert_eq!(v.as_str().unwrap().chars().count(), 300_000);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

/// Discriminator-style polymorphism: the base declares `petType: string`,
/// each branch narrows it to a distinct constant.
pub fn pet_schema() -> Value {
    json!({
        "$ref": "#/$defs/Pet",
        "$defs": {
            "Pet": {
                "type": "object",
                "required": ["petType", "name"],
                "properties": {"petType": {"type": "string"}, "name": {"type": "string"}},
                "oneOf": [{"$ref": "#/$defs/Cat"}, {"$ref": "#/$defs/Dog"}]
            },
            "Cat": {
                "type": "object",
                "required": ["petType", "name"],
                "properties": {"petType": {"const": "cat"}, "name": {"type": "string"}}
            },
            "Dog": {
                "type": "object",
                "required": ["petType", "name"],
                "properties": {"petType": {"enum": ["dog"]}, "name": {"type": "string"}}
            }
        }
    })
}

#[test]
fn fakes_each_one_of_branch_validly() {
    let schema = pet_schema();
    let validator = SchemaValidator::new(&schema).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for branch in 0..2usize {
        for seed in 0..20u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let v = Faker::default().fake_with_branch(&schema, Some(branch), &mut rng);
            assert!(validator.validate(&v).is_ok(), "branch {branch}: {v}");
            seen.insert(v["petType"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(seen.len(), 2, "{seen:?}");
}

/// `Expr: oneOf [And{l, r: Expr}, Leaf{refs to S required}]`, recursive
/// branch first. `leaf_refs` required `$ref`s make the leaf look at least as
/// "expensive" as the recursive branch to a recursion-blind cost.
fn fanout_expr_schema(leaf_refs: usize) -> Value {
    let names: Vec<String> = (0..leaf_refs).map(|i| format!("f{i}")).collect();
    let props: serde_json::Map<String, Value> = names
        .iter()
        .map(|n| (n.clone(), json!({"$ref": "#/$defs/S"})))
        .collect();
    json!({
        "$ref": "#/$defs/Expr",
        "$defs": {
            "Expr": {"oneOf": [{"$ref": "#/$defs/And"}, {"$ref": "#/$defs/Leaf"}]},
            "And": {
                "type": "object",
                "required": ["l", "r"],
                "properties": {"l": {"$ref": "#/$defs/Expr"}, "r": {"$ref": "#/$defs/Expr"}}
            },
            "Leaf": {"type": "object", "required": names, "properties": props},
            "S": {"type": "string"}
        }
    })
}

fn assert_fast_and_valid(schema: &Value) {
    let validator = SchemaValidator::new(schema).unwrap();
    for (label, faker) in [("new(0)", Faker::new(0)), ("default", Faker::default())] {
        for branch in [Some(0usize), Some(1), None] {
            for seed in 0..10u64 {
                let mut rng = StdRng::seed_from_u64(seed);
                let start = std::time::Instant::now();
                let v = faker.fake_with_branch(schema, branch, &mut rng);
                let elapsed = start.elapsed();
                assert!(
                    elapsed < std::time::Duration::from_secs(1),
                    "{label} branch {branch:?} seed {seed}: {elapsed:?}"
                );
                assert!(
                    validator.validate(&v).is_ok(),
                    "{label} branch {branch:?} seed {seed}: {v}"
                );
            }
        }
    }
}

#[test]
fn recursion_aware_branch_choice_with_three_ref_leaf() {
    assert_fast_and_valid(&fanout_expr_schema(3));
}

#[test]
fn recursion_aware_branch_choice_with_tied_ref_counts() {
    // Leaf and And both have exactly two required refs; And is listed first.
    assert_fast_and_valid(&fanout_expr_schema(2));
}

#[test]
fn node_budget_bounds_wide_unsatisfiable_recursion() {
    // Every instance recurses forever with fan-out 2; the node caps (not the
    // 2^depth tree) bound the work.
    let schema = json!({
        "$ref": "#/$defs/Node",
        "$defs": {"Node": {
            "type": "object",
            "required": ["a", "b"],
            "properties": {"a": {"$ref": "#/$defs/Node"}, "b": {"$ref": "#/$defs/Node"}}
        }}
    });
    let start = std::time::Instant::now();
    let mut rng = StdRng::seed_from_u64(3);
    let v = Faker::default().fake(&schema, &mut rng);
    assert!(v.is_object());
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

/// `Expr: oneOf [And, Leaf]` where `And` splits its object across `allOf`:
/// one member declares the recursive properties, another requires them.
/// `Leaf` has three required `$ref`s, so a cost that ignores the split
/// `required` makes the non-terminating `And` look cheaper.
fn allof_split_expr_schema() -> Value {
    json!({
        "$ref": "#/$defs/Expr",
        "$defs": {
            "Expr": {"oneOf": [{"$ref": "#/$defs/And"}, {"$ref": "#/$defs/Leaf"}]},
            "And": {
                "type": "object",
                "allOf": [
                    {"properties": {"l": {"$ref": "#/$defs/Expr"}, "r": {"$ref": "#/$defs/Expr"}}},
                    {"required": ["l", "r"]}
                ]
            },
            "Leaf": {
                "type": "object",
                "required": ["f0", "f1", "f2"],
                "properties": {
                    "f0": {"$ref": "#/$defs/S"},
                    "f1": {"$ref": "#/$defs/S"},
                    "f2": {"$ref": "#/$defs/S"}
                }
            },
            "S": {"type": "string"}
        }
    })
}

#[test]
fn allof_split_required_is_costed() {
    let schema = allof_split_expr_schema();
    let validator = SchemaValidator::new(&schema).unwrap();
    for depth in [0usize, 6] {
        let faker = Faker::new(depth);
        for seed in 0..30u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let start = std::time::Instant::now();
            let v = faker.fake(&schema, &mut rng);
            let elapsed = start.elapsed();
            // Guards against exponential blowup (seconds per value), not
            // speed: a loaded CI runner can take tens of milliseconds.
            assert!(
                elapsed < std::time::Duration::from_secs(1),
                "depth {depth} seed {seed}: {elapsed:?}"
            );
            assert!(
                validator.validate(&v).is_ok(),
                "depth {depth} seed {seed}: {v}"
            );
        }
    }
}

#[test]
fn fake_with_costs_matches_fake_with_branch() {
    let schema = allof_split_expr_schema();
    let costs = Costs::compute(&schema);
    let faker = Faker::default();
    for seed in 0..10u64 {
        let a = faker.fake_with_branch(&schema, Some(1), &mut StdRng::seed_from_u64(seed));
        let b = faker.fake_with_costs(&schema, Some(1), &costs, &mut StdRng::seed_from_u64(seed));
        assert_eq!(a, b, "seed {seed}");
    }
}

/// An OpenAPI contract whose response is a linear chain of `n` definitions,
/// each requiring the next. Every faker draw is cut off by the hard depth
/// limit, so `Engine::fallback` runs all of its attempts.
fn chain_contract(n: usize) -> String {
    let mut schemas = serde_json::Map::new();
    for i in 0..n {
        let def = if i + 1 == n {
            json!({"type": "string"})
        } else {
            json!({
                "type": "object",
                "required": ["next"],
                "properties": {"next": {"$ref": format!("#/components/schemas/D{:03}", i + 1)}}
            })
        };
        schemas.insert(format!("D{i:03}"), def);
    }
    json!({
        "openapi": "3.1.0",
        "info": {"title": "Chain", "version": "1"},
        "paths": {"/chain": {"get": {
            "operationId": "getChain",
            "responses": {"200": {
                "description": "OK",
                "content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/D000"}
                }}
            }}
        }}},
        "components": {"schemas": schemas}
    })
    .to_string()
}

#[test]
fn costs_computed_once_per_response() {
    let bundle = compile(chain_contract(400).as_bytes()).unwrap();
    let op = bundle.operation("getChain").unwrap();
    let variant = op.success_output().unwrap();
    let engine = Engine::new(None, EngineConfig::default());
    let call = OperationCall {
        operation_id: "getChain".into(),
        ..OperationCall::default()
    };
    let ctx = GenerationContext::default();
    let start = std::time::Instant::now();
    let out = engine.fallback(&bundle, op, variant, 200, &call, &ctx);
    let elapsed = start.elapsed();
    assert!(out.body.is_some());
    assert!(
        elapsed < std::time::Duration::from_millis(300),
        "{elapsed:?}"
    );
}

#[test]
fn faker_formats_match_patterns() {
    use base64::Engine as _;
    let faker = Faker::default();
    let cases = [
        ("int64", "^-?[0-9]+$"),
        ("uint64", "^[0-9]+$"),
        ("byte", "^[A-Za-z0-9+/]*={0,2}$"),
        ("time", "^[0-9]{2}:[0-9]{2}:[0-9]{2}Z$"),
        (
            "duration",
            "^P(?:[0-9]+D)?T[0-9]+H[0-9]+M[0-9]+S$|^P[0-9]+D$",
        ),
        ("proto-duration", "^-?[0-9]+(\\.[0-9]+)?s$"),
        ("hex", "^[0-9A-Fa-f]*$"),
    ];
    for (format, pattern) in cases {
        let schema = json!({"type": "string", "format": format, "pattern": pattern});
        let validator = SchemaValidator::new(&schema).unwrap();
        for seed in 0..100u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let v = faker.fake(&json!({"type": "string", "format": format}), &mut rng);
            assert!(validator.validate(&v).is_ok(), "{format}: {v}");
            let s = v.as_str().unwrap();
            match format {
                "int64" => {
                    s.parse::<i64>().unwrap();
                }
                "uint64" => {
                    s.parse::<u64>().unwrap();
                }
                "byte" => {
                    base64::engine::general_purpose::STANDARD.decode(s).unwrap();
                }
                "time" => {
                    chrono::NaiveTime::parse_from_str(s, "%H:%M:%SZ").unwrap();
                }
                "hex" => assert_eq!(s.len() % 2, 0, "{s}"),
                _ => {}
            }
        }
    }
}

fn draws(schema: Value, n: u64) -> Vec<Value> {
    let faker = Faker::default();
    (0..n)
        .map(|seed| faker.fake(&schema, &mut StdRng::seed_from_u64(seed)))
        .collect()
}

#[test]
fn counts_are_small_and_non_negative() {
    let graphql_int = json!({"type": "object", "required": ["n"], "properties": {"n": {"type": "integer", "minimum": -2147483648i64, "maximum": 2147483647}}});
    let named = json!({"type": "object", "required": ["total_count", "pageSize"], "properties": {
        "total_count": {"type": "integer", "minimum": 0, "maximum": 1000000}, "pageSize": {"type": "integer"}}});
    for v in draws(graphql_int, 200) {
        assert!((0..=100).contains(&v["n"].as_i64().unwrap()), "{v}");
    }
    for v in draws(named, 200) {
        assert!(
            (0..=100).contains(&v["total_count"].as_i64().unwrap()),
            "{v}"
        );
        assert!((0..=100).contains(&v["pageSize"].as_i64().unwrap()), "{v}");
    }
    for v in draws(json!({"type": "integer", "minimum": 5, "maximum": 10}), 100) {
        assert!((5..=10).contains(&v.as_i64().unwrap()));
    }
    for v in draws(json!({"type": "integer", "minimum": 1}), 100) {
        assert!((1..=101).contains(&v.as_i64().unwrap()));
    }
}

#[test]
fn nullable_lists_are_never_null() {
    for v in draws(
        json!({"anyOf": [{"type": "array", "items": {"type": "integer"}}, {"type": "null"}]}),
        200,
    ) {
        assert!(v.is_array(), "{v}");
    }
    let at_limit = Faker::new(0).fake(
        &json!({"type": ["array", "null"], "items": {"type": "string"}}),
        &mut StdRng::seed_from_u64(1),
    );
    assert_eq!(at_limit, json!([]));
}

#[test]
fn unconstrained_strings_stay_short_and_new_formats_parse() {
    for v in draws(json!({"type": "string", "minLength": 10}), 50) {
        let n = v.as_str().unwrap().chars().count();
        assert!((10..=64).contains(&n), "{v}");
    }
    let s = |format: &str| {
        draws(json!({"type": "string", "format": format}), 1)[0]
            .as_str()
            .unwrap()
            .to_string()
    };
    s("ipv4").parse::<std::net::Ipv4Addr>().unwrap();
    s("ipv6").parse::<std::net::Ipv6Addr>().unwrap();
    assert!(s("hostname").ends_with(".example.com"));
    assert!(s("uri-reference").starts_with('/'));
}
