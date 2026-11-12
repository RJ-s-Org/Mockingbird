use mb_engine::faker::Costs;
use mb_engine::{Engine, EngineConfig, Faker, GenerationContext, OperationCall, SchemaValidator};
use mb_import::compile;
use proptest::prelude::*;
use rand::SeedableRng;
use rand::rngs::StdRng;
use serde_json::{Value, json};
use std::collections::HashSet;

use crate::support::orders;

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
            (inner.clone(), inner.clone()).prop_map(|(a, b)| {
                json!({"allOf": [
                    {"type": "object", "properties": {"p": a}, "required": ["p"]},
                    {"type": "object", "properties": {"q": b}}
                ]})
            }),
            (inner.clone(), 0u64..3, 0u64..3).prop_map(|(values, min, extra)| {
                json!({"type": "object", "additionalProperties": values,
                       "minProperties": min, "maxProperties": min + extra})
            }),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| {
                json!({"type": "array", "prefixItems": [a], "items": b, "minItems": 1})
            }),
            // The faker intersects `contains` with `items` only as far as
            // `allOf` merging goes; an item schema that is `contains` itself
            // is always satisfiable.
            (inner, 0u64..3).prop_map(|(items, n)| {
                json!({"type": "array", "items": items, "contains": items, "minContains": n})
            }),
        ]
    })
}

/// Fakes `schema` with 200 seeds and checks every value against it.
fn always_valid(schema: &Value) -> Vec<Value> {
    let validator = SchemaValidator::new(schema).unwrap();
    let faker = Faker::default();
    (0..200u64)
        .map(|seed| {
            let mut rng = StdRng::seed_from_u64(seed);
            let v = faker.fake(schema, &mut rng);
            if let Err(e) = validator.validate(&v) {
                panic!("seed {seed}: {e:?} for {v} of {schema}");
            }
            v
        })
        .collect()
}

#[test]
fn fakes_maps_from_additional_and_pattern_properties() {
    // An Avro map: no declared properties, values from additionalProperties.
    let map =
        json!({"type": "object", "additionalProperties": {"type": "integer"}, "minProperties": 2});
    for v in always_valid(&map) {
        assert!(v.as_object().unwrap().len() >= 2, "{v}");
    }
    let maxed = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}, "b": {"type": "string"}, "c": {"type": "string"}},
        "maxProperties": 1
    });
    always_valid(&maxed);
    let patterned = json!({
        "type": "object",
        "patternProperties": {"^x-[a-z]{2,4}$": {"type": "boolean"}},
        "additionalProperties": false,
        "minProperties": 1
    });
    for v in always_valid(&patterned) {
        assert!(!v.as_object().unwrap().is_empty(), "{v}");
    }
    let mixed = json!({
        "type": "object",
        "properties": {"id": {"type": "string"}},
        "required": ["id"],
        "additionalProperties": {"type": "string", "format": "uuid"},
        "minProperties": 3,
        "maxProperties": 4
    });
    always_valid(&mixed);
}

#[test]
fn fakes_prefix_items_and_contains() {
    let tuple = json!({
        "type": "array",
        "prefixItems": [{"type": "string"}, {"type": "integer"}, {"type": "boolean"}],
        "items": false,
        "minItems": 3
    });
    for v in always_valid(&tuple) {
        assert_eq!(v.as_array().unwrap().len(), 3, "{v}");
    }
    let open = json!({
        "type": "array",
        "prefixItems": [{"const": "head"}],
        "items": {"type": "integer"},
        "minItems": 2
    });
    always_valid(&open);
    let contains = json!({
        "type": "array",
        "items": {"type": ["string", "integer"]},
        "contains": {"type": "integer", "minimum": 10},
        "minContains": 2
    });
    always_valid(&contains);
    let untyped = json!({"contains": {"const": 7}});
    always_valid(&untyped);
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
    // Counted, not timed: every faker attempt is cut off by the depth limit,
    // so a fallback that recomputed the table per attempt would build it
    // FALLBACK_ATTEMPTS times.
    let before = Costs::computed_on_this_thread();
    let out = engine.fallback(&bundle, op, variant, 200, &call, &ctx);
    let computed = Costs::computed_on_this_thread() - before;
    assert!(out.body.is_some());
    assert_eq!(
        computed, 1,
        "cost table built {computed} times for one response"
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

/// For a client's selection (a GraphQL query, `FakerHints::objects`), a nullable
/// object (`repository: Repository`, imported as `anyOf` with `null`), an interface
/// and an `allOf` composite are drawn as objects; a nullable scalar may still be null.
/// Elsewhere a nullable object stays nullable.
#[test]
fn nullable_objects_of_a_selection_are_drawn_as_objects() {
    let selected = Faker::default().with_hints(mb_engine::FakerHints {
        objects: true,
        ..Default::default()
    });
    let draws_of = |faker: &Faker, schema: &Value| -> Vec<Value> {
        (0..200)
            .map(|seed| faker.fake(schema, &mut StdRng::seed_from_u64(seed)))
            .collect()
    };
    let named = |key: &str| json!({"type": "object", "properties": {key: {"type": "string"}}, "required": [key]});
    let object = json!({"anyOf": [named("name"), {"type": "null"}]});
    let union = json!({"anyOf": [{"anyOf": [named("login"), named("name")]}, {"type": "null"}]});
    let composite = json!({"oneOf": [{"allOf": [named("id"), named("name")]}, {"type": "null"}]});
    for schema in [&object, &union, &composite] {
        for v in draws_of(&selected, schema) {
            assert!(v.is_object(), "{schema}: {v}");
        }
    }
    let scalar = json!({"anyOf": [{"type": "string"}, {"type": "null"}]});
    assert!(draws_of(&selected, &scalar).iter().any(Value::is_null));
    assert!(
        draws_of(&Faker::default(), &object)
            .iter()
            .any(Value::is_null)
    );
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

#[test]
fn strings_match_their_pattern() {
    let pattern = r"^/[A-Za-z0-9]{22}\.jpg$";
    let re = regex::Regex::new(pattern).unwrap();
    for v in draws(json!({"type": "string", "pattern": pattern}), 100) {
        let s = v.as_str().unwrap();
        assert!(re.is_match(s) && s.len() == 27, "{s}");
    }
    let re = regex::Regex::new(r"^[a-z]+-\d+$").unwrap();
    for v in draws(
        json!({"type": "string", "pattern": r"^[a-z]+-\d+$", "maxLength": 20}),
        100,
    ) {
        let s = v.as_str().unwrap();
        assert!(re.is_match(s) && s.len() <= 20, "{s}");
    }
}

#[test]
fn a_pattern_with_an_example_is_drawn_in_the_examples_shape() {
    let pattern = "^(SM|MM)[0-9a-fA-F]{32}$";
    let re = regex::Regex::new(pattern).unwrap();
    let example = "SMaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let shaped = |schema: Value| {
        let drawn = draws(schema, 50);
        for v in &drawn {
            let s = v.as_str().unwrap();
            assert!(re.is_match(s), "{s}");
            assert!(s.starts_with("SM"), "{s}");
            assert!(
                s[2..].chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
                "{s}"
            );
        }
        let distinct: HashSet<&Value> = drawn.iter().collect();
        assert!(distinct.len() > 45, "{drawn:?}");
    };
    // Its own `examples` (3.0's `example` is imported as one), or a 3.1 `example`.
    shaped(json!({"type": "string", "pattern": pattern, "examples": [example]}));
    shaped(json!({"type": "string", "pattern": pattern, "example": example}));
    // An example the pattern refuses says nothing.
    for v in draws(
        json!({"type": "string", "pattern": "^[0-9]{6}$", "examples": ["SM1"]}),
        20,
    ) {
        assert!(
            v.as_str().unwrap().bytes().all(|b| b.is_ascii_digit()),
            "{v}"
        );
    }
    // The member's value in its object's examples.
    let object = json!({
        "type": "object",
        "required": ["sid"],
        "properties": {"sid": {"type": "string", "pattern": pattern}},
        "examples": [{"sid": example}]
    });
    for v in draws(object, 50) {
        let sid = v["sid"].as_str().unwrap();
        assert!(re.is_match(sid) && sid.starts_with("SM"), "{sid}");
        assert!(
            sid[2..].chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "{sid}"
        );
    }
}

#[test]
fn a_class_is_drawn_never_pinned_to_the_examples_characters() {
    // `[0-9]` is a class of ten: the example's leading digits are not a literal.
    for (pattern, example) in [("^[0-9]{12}$", "123456789012"), ("^[0-9]{8}$", "12345678")] {
        let n = example.len();
        let drawn = draws(
            json!({"type": "string", "pattern": pattern, "examples": [example]}),
            50,
        );
        for v in &drawn {
            let s = v.as_str().unwrap();
            assert!(s.len() == n && s.bytes().all(|b| b.is_ascii_digit()), "{s}");
        }
        let heads: HashSet<&str> = drawn.iter().map(|v| &v.as_str().unwrap()[..2]).collect();
        assert!(heads.len() > 10, "{pattern}: {heads:?}");
    }
}

#[test]
fn a_members_own_example_failing_the_pattern_gives_way_to_its_objects() {
    let pattern = "^(SM|MM)[0-9a-fA-F]{32}$";
    let object = json!({
        "type": "object",
        "required": ["sid"],
        "properties": {"sid": {"type": "string", "pattern": pattern, "examples": ["string"]}},
        "examples": [{"sid": "SMaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]
    });
    for v in draws(object, 50) {
        let sid = v["sid"].as_str().unwrap();
        assert!(sid.starts_with("SM") && sid.len() == 34, "{sid}");
        assert!(
            sid[2..].chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "{sid}"
        );
    }
}

#[test]
fn an_objects_example_failing_the_pattern_is_ignored() {
    let pattern = "^(SM|MM)[0-9a-fA-F]{32}$";
    let re = regex::Regex::new(pattern).unwrap();
    let object = json!({
        "type": "object",
        "required": ["sid"],
        "properties": {"sid": {"type": "string", "pattern": pattern}},
        "examples": [{"sid": "not-a-sid"}]
    });
    let drawn = draws(object, 200);
    for v in &drawn {
        assert!(re.is_match(v["sid"].as_str().unwrap()), "{v}");
    }
    let starts = |p: &str| {
        drawn
            .iter()
            .any(|v| v["sid"].as_str().unwrap().starts_with(p))
    };
    assert!(starts("SM") && starts("MM"), "{drawn:?}");
}

#[test]
fn a_variable_length_pattern_keeps_the_examples_length() {
    let pattern = "^cus_[A-Za-z0-9]+$";
    let re = regex::Regex::new(pattern).unwrap();
    let example = "cus_Abc123Xyz789Qwe";
    let drawn = draws(
        json!({"type": "string", "pattern": pattern, "examples": [example]}),
        50,
    );
    for v in &drawn {
        let s = v.as_str().unwrap();
        assert!(re.is_match(s) && s.len() == example.len(), "{s}");
    }
    let distinct: HashSet<&Value> = drawn.iter().collect();
    assert!(distinct.len() > 45, "{drawn:?}");
}

#[test]
fn a_pattern_with_no_example_draws_any_match() {
    let pattern = "^(SM|MM)[0-9a-fA-F]{32}$";
    let re = regex::Regex::new(pattern).unwrap();
    let drawn = draws(json!({"type": "string", "pattern": pattern}), 200);
    for v in &drawn {
        assert!(re.is_match(v.as_str().unwrap()), "{v}");
    }
    let starts = |p: &str| drawn.iter().any(|v| v.as_str().unwrap().starts_with(p));
    let upper = drawn.iter().any(|v| {
        v.as_str().unwrap()[2..]
            .bytes()
            .any(|b| b.is_ascii_uppercase())
    });
    assert!(starts("SM") && starts("MM") && upper, "{drawn:?}");
}

#[test]
fn an_example_too_narrow_to_tell_draws_apart_is_not_followed() {
    // `^[0-9]+$` with `"7"`: a shape of ten draws would repeat ids.
    for v in draws(
        json!({"type": "string", "pattern": "^[0-9]+$", "examples": ["7"]}),
        20,
    ) {
        assert!(
            v.as_str().unwrap().bytes().all(|b| b.is_ascii_digit()),
            "{v}"
        );
    }
    let drawn = draws(
        json!({"type": "string", "pattern": "^[0-9]+$", "examples": ["7"]}),
        50,
    );
    assert!(
        drawn.iter().any(|v| v.as_str().unwrap().len() > 1),
        "{drawn:?}"
    );
}

#[test]
fn broad_classes_in_patterns_draw_readable_ascii() {
    // `.` and a negated class can match bytes that are not UTF-8, so these
    // used to be drawn from all of Unicode.
    for pattern in [
        r"^id:.{22}$",
        r"^[^/]{8}\.[a-z]{3}$",
        r"^\S+@\S+$",
        r"^\p{L}{6}$",
    ] {
        let re = regex::Regex::new(pattern).unwrap();
        for v in draws(json!({"type": "string", "pattern": pattern}), 100) {
            let s = v.as_str().unwrap();
            assert!(re.is_match(s), "{pattern}: {s:?}");
            assert!(s.chars().all(|c| c.is_ascii_graphic()), "{pattern}: {s:?}");
        }
    }
    let re = regex::Regex::new(r"^id:.{22}$").unwrap();
    for v in draws(json!({"type": "string", "pattern": r"^id:.{22}$"}), 50) {
        let s = v.as_str().unwrap();
        assert!(
            re.is_match(s) && s[3..].chars().all(|c| c.is_ascii_alphanumeric()),
            "{s}"
        );
    }
}

#[test]
fn unsupported_or_too_long_patterns_fall_back() {
    for v in draws(json!({"type": "string", "pattern": r"^(a)\1$"}), 20) {
        assert!(v.is_string());
    }
    for v in draws(
        json!({"type": "string", "pattern": "^x{100}$", "maxLength": 10}),
        20,
    ) {
        assert!(v.as_str().unwrap().chars().count() <= 10);
    }
    assert_eq!(
        draws(json!({"type": "string", "pattern": "^v1$"}), 1)[0],
        json!("v1")
    );
}

#[test]
fn echoed_values_fill_same_named_root_fields() {
    let mut echoes = serde_json::Map::new();
    echoes.insert("login".into(), json!("octocat"));
    echoes.insert("page".into(), json!("3"));
    let faker = Faker::default().with_hints(mb_engine::FakerHints {
        echoes,
        ..Default::default()
    });
    let schema = json!({"type": "object", "required": ["login", "page", "owner"], "properties": {
        "login": {"type": "string"}, "page": {"type": "integer"},
        "owner": {"type": "object", "required": ["login"], "properties": {"login": {"type": "string"}}}}});
    let v = faker.fake(&schema, &mut StdRng::seed_from_u64(7));
    assert_eq!(v["login"], "octocat");
    assert_eq!(v["page"], 3);
    assert_ne!(v["owner"]["login"], "octocat", "only root fields echo");
}

#[test]
fn a_value_echoed_into_an_object_field_names_the_object() {
    let mut echoes = serde_json::Map::new();
    echoes.insert("owner".into(), json!("fernwood"));
    echoes.insert("team".into(), json!("core"));
    let faker = Faker::default().with_hints(mb_engine::FakerHints {
        echoes,
        ..Default::default()
    });
    let schema = json!({"type": "object", "required": ["owner", "team"], "properties": {
        "owner": {"type": "object", "required": ["login", "name"], "properties": {
            "login": {"type": "string"}, "name": {"type": "string"}}},
        "team": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}}}});
    let v = faker.fake(&schema, &mut StdRng::seed_from_u64(7));
    // A handle before a display name.
    assert_eq!(v["owner"]["login"], "fernwood", "{v}");
    assert_ne!(v["owner"]["name"], "fernwood", "{v}");
    assert_eq!(v["team"]["name"], "core", "{v}");
}

#[test]
fn an_echoed_object_name_keeps_to_its_member_s_schema() {
    let mut echoes = serde_json::Map::new();
    echoes.insert("project".into(), json!("10000"));
    echoes.insert("size".into(), json!("huge"));
    echoes.insert("deep".into(), json!("x"));
    echoes.insert("count".into(), json!(3));
    let faker = Faker::default().with_hints(mb_engine::FakerHints {
        echoes,
        ..Default::default()
    });
    let schema = json!({"type": "object", "required": ["project", "size", "deep", "count"], "properties": {
        // The key's pattern refuses the value; the name takes it.
        "project": {"type": "object", "required": ["key", "name"], "properties": {
            "key": {"type": "string", "pattern": "^[A-Z]{2,5}$"}, "name": {"type": "string"}}},
        // An enum member is never overwritten.
        "size": {"type": "object", "required": ["key"], "properties": {
            "key": {"type": "string", "enum": ["s", "m"]}}},
        "deep": {"type": "object", "required": ["inner"], "properties": {
            "inner": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}}}},
        // Only a string value names an object.
        "count": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}}}});
    let v = faker.fake(&schema, &mut StdRng::seed_from_u64(7));
    assert_ne!(v["project"]["key"], "10000", "{v}");
    assert_eq!(v["project"]["name"], "10000", "{v}");
    assert!(
        ["s", "m"].contains(&v["size"]["key"].as_str().unwrap()),
        "{v}"
    );
    assert_ne!(
        v["deep"]["inner"]["name"], "x",
        "only root fields are named: {v}"
    );
    assert_ne!(v["count"]["name"], "3", "{v}");
}

#[test]
fn integer_ids_are_positive_and_spread_out() {
    let schema = json!({"type": "object", "required": ["id", "owner_id", "userId"], "properties": {
        "id": {"type": "integer"}, "owner_id": {"type": "integer"}, "userId": {"type": "integer", "minimum": -2147483648i64, "maximum": 2147483647}}});
    let values: Vec<i64> = draws(schema, 100)
        .iter()
        .flat_map(|v| ["id", "owner_id", "userId"].map(|k| v[k].as_i64().unwrap()))
        .collect();
    assert!(values.iter().all(|n| *n >= 1), "{values:?}");
    assert!(
        values.iter().filter(|n| **n > 100).count() > 250,
        "{values:?}"
    );
}

#[test]
fn dates_and_timestamps_are_recent() {
    let now = chrono::Utc::now();
    let month_ago = now - chrono::Duration::days(31);
    let schema = json!({"type": "object", "required": ["at", "on", "created", "updated_at", "n"], "properties": {
        "at": {"type": "string", "format": "date-time"},
        "on": {"type": "string", "format": "date"},
        "created": {"type": "integer"},
        "updated_at": {"type": "integer", "format": "int64"},
        "n": {"type": "integer"}}});
    for v in draws(schema, 50) {
        let at = chrono::DateTime::parse_from_rfc3339(v["at"].as_str().unwrap()).unwrap();
        assert!(at >= month_ago && at <= now, "{v}");
        let on = chrono::NaiveDate::parse_from_str(v["on"].as_str().unwrap(), "%Y-%m-%d").unwrap();
        assert!(
            on >= month_ago.date_naive() && on <= now.date_naive(),
            "{v}"
        );
        for k in ["created", "updated_at"] {
            let secs = v[k].as_i64().unwrap();
            assert!(
                secs >= month_ago.timestamp() && secs <= now.timestamp(),
                "{k}: {v}"
            );
        }
        assert!(v["n"].as_i64().unwrap() < 1_000_000, "{v}");
    }
}

/// An id or the record it names (an expandable reference, as Stripe's `customer`) is
/// drawn as the id or `null`, never the object: an API embeds the record only when
/// asked to. A choice whose object has no `id` is drawn from every branch.
#[test]
fn a_reference_choice_is_drawn_as_its_id() {
    let schema = json!({
        "type": "object",
        "required": ["customer", "setting"],
        "properties": {
            "customer": {"anyOf": [{"type": "string"}, {"$ref": "#/$defs/Customer"}, {"type": "null"}]},
            "setting": {"anyOf": [{"type": "string"}, {"$ref": "#/$defs/Settings"}]}
        },
        "$defs": {
            "Customer": {"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}, "email": {"type": "string"}}},
            "Settings": {"type": "object", "required": ["mode"], "properties": {"mode": {"type": "string"}}}
        }
    });
    let faker = Faker::default();
    let mut settings_objects = 0;
    for seed in 0..200u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let v = faker.fake(&schema, &mut rng);
        assert!(!v["customer"].is_object(), "seed {seed}: {v}");
        settings_objects += usize::from(v["setting"].is_object());
    }
    assert!(settings_objects > 0);
}
