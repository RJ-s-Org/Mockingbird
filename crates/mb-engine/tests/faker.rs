#![allow(clippy::unwrap_used)]

use mb_engine::{Faker, SchemaValidator};
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
