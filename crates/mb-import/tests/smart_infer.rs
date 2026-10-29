//! Deterministic JSON Schema inference from examples.

use mb_import::smart::infer::{infer_scalar, infer_schema, schema_richness};
use proptest::prelude::*;
use serde_json::{Value, json};

#[test]
fn objects_required_are_common_keys() {
    let schema = infer_schema(&[
        json!({"id": 1, "name": "rex", "tag": "dog"}),
        json!({"name": "tom", "id": 2}),
    ]);
    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer"},
                "name": {"type": "string"},
                "tag": {"type": "string"}
            },
            "required": ["id", "name"]
        })
    );
    let disjoint = infer_schema(&[json!({"a": true}), json!({"b": false})]);
    assert!(disjoint.get("required").is_none());
}

#[test]
fn integer_and_number_widen() {
    assert_eq!(
        infer_schema(&[json!(1), json!(2.5)]),
        json!({"type": "number"})
    );
    assert_eq!(
        infer_schema(&[json!(1), json!(u64::MAX)]),
        json!({"type": "integer"})
    );
    assert_eq!(infer_schema(&[json!(0.5)]), json!({"type": "number"}));
}

#[test]
fn nullable_union() {
    assert_eq!(
        infer_schema(&[json!("x"), Value::Null]),
        json!({"type": ["string", "null"]})
    );
    assert_eq!(
        infer_schema(&[json!({"a": 1}), Value::Null]),
        json!({
            "type": ["object", "null"],
            "properties": {"a": {"type": "integer"}},
            "required": ["a"]
        })
    );
    assert_eq!(infer_schema(&[Value::Null]), json!({"type": "null"}));
}

#[test]
fn mixed_types_any_of() {
    assert_eq!(
        infer_schema(&[
            json!(true),
            json!(1),
            Value::Null,
            json!("s"),
            json!([1]),
            json!({})
        ]),
        json!({
            "anyOf": [
                {"type": "object"},
                {"type": "array", "items": {"type": "integer"}},
                {"type": "string"},
                {"type": "integer"},
                {"type": "boolean"},
                {"type": "null"}
            ]
        })
    );
    assert_eq!(
        infer_schema(&[json!(1.5), json!("s"), json!(2)]),
        json!({"anyOf": [{"type": "string"}, {"type": "number"}]})
    );
}

#[test]
fn string_formats() {
    assert_eq!(
        infer_schema(&[
            json!("123e4567-e89b-12d3-a456-426614174000"),
            json!("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE")
        ]),
        json!({"type": "string", "format": "uuid"})
    );
    assert_eq!(
        infer_schema(&[
            json!("2024-01-02T03:04:05Z"),
            json!("2024-01-02T03:04:05.5+02:00")
        ]),
        json!({"type": "string", "format": "date-time"})
    );
    assert_eq!(
        infer_schema(&[json!("a@example.com")]),
        json!({"type": "string", "format": "email"})
    );
    assert_eq!(
        infer_schema(&[json!("https://example.com/a?b=c")]),
        json!({"type": "string", "format": "uri"})
    );
    // A format applies only when every sample matches it.
    assert_eq!(
        infer_schema(&[json!("a@example.com"), json!("plain")]),
        json!({"type": "string"})
    );
    // Near misses.
    for s in [
        "a@b@c.com",
        "a b@c.com",
        "a@localhost",
        "mailto:x",
        "2024-01-02",
    ] {
        assert_eq!(infer_schema(&[json!(s)]), json!({"type": "string"}), "{s}");
    }
}

#[test]
fn enum_for_repeated_short_strings() {
    assert_eq!(
        infer_schema(&[json!("b"), json!("a"), json!("b")]),
        json!({"type": "string", "enum": ["a", "b"]})
    );
    assert_eq!(
        infer_schema(&[json!("b"), json!("a"), json!("b"), Value::Null]),
        json!({"type": ["string", "null"], "enum": ["a", "b", null]})
    );
}

#[test]
fn no_enum_for_unique_strings() {
    assert_eq!(
        infer_schema(&[json!("a"), json!("b"), json!("c")]),
        json!({"type": "string"})
    );
    // Too few samples.
    assert_eq!(
        infer_schema(&[json!("a"), json!("a")]),
        json!({"type": "string"})
    );
    // Too long a value.
    let long = "x".repeat(25);
    assert_eq!(
        infer_schema(&[json!(long), json!(long), json!("a")]),
        json!({"type": "string"})
    );
    // Too many distinct values.
    let many: Vec<Value> = ["a", "b", "c", "d", "e", "f", "a"]
        .iter()
        .map(|s| json!(s))
        .collect();
    assert_eq!(infer_schema(&many), json!({"type": "string"}));
}

#[test]
fn empty_arrays_have_no_items() {
    assert_eq!(
        infer_schema(&[json!([]), json!([])]),
        json!({"type": "array"})
    );
    assert_eq!(
        infer_schema(&[json!([]), json!([1, 2.5])]),
        json!({"type": "array", "items": {"type": "number"}})
    );
}

#[test]
fn no_samples_is_empty_schema() {
    assert_eq!(infer_schema(&[]), json!({}));
    assert_eq!(infer_scalar(&[]), json!({}));
}

#[test]
fn deep_nesting_is_cut_off() {
    let mut v = json!(1);
    for _ in 0..100 {
        v = json!([v]);
    }
    let schema = infer_schema(&[v.clone()]);
    assert!(jsonschema::validator_for(&schema).unwrap().is_valid(&v));
    let mut depth = 0;
    let mut cur = &schema;
    while let Some(items) = cur.get("items") {
        depth += 1;
        cur = items;
    }
    assert_eq!(depth, 32);
    assert_eq!(cur, &json!({}));
}

#[test]
fn scalar_strings() {
    assert_eq!(infer_scalar(&["1", "2"]), json!({"type": "integer"}));
    assert_eq!(infer_scalar(&["true", "false"]), json!({"type": "boolean"}));
    assert_eq!(infer_scalar(&["1.5", "2"]), json!({"type": "number"}));
    assert_eq!(infer_scalar(&["abc"]), json!({"type": "string"}));
    assert_eq!(infer_scalar(&["NaN"]), json!({"type": "string"}));
    assert_eq!(
        infer_scalar(&["123e4567-e89b-12d3-a456-426614174000"]),
        json!({"type": "string", "format": "uuid"})
    );
    assert_eq!(
        infer_scalar(&["asc", "desc", "asc"]),
        json!({"type": "string", "enum": ["asc", "desc"]})
    );
}

#[test]
fn richness_counts_keys() {
    assert_eq!(schema_richness(&json!({})), 0);
    assert_eq!(schema_richness(&json!({"type": "string"})), 1);
    assert_eq!(
        schema_richness(&json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "array", "items": {"type": "integer"}}},
            "required": ["a"]
        })),
        // type, properties, required, a, a.type, b, b.type, b.items, items.type
        9
    );
    assert_eq!(
        schema_richness(&json!({"anyOf": [{"type": "string"}, {"type": "null"}]})),
        3
    );
}

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        any::<u64>().prop_map(|n| json!(n)),
        (-1.0e6f64..1.0e6).prop_map(|f| json!(f)),
        prop_oneof![
            "[a-c]{0,3}",
            "[a-z]{1,8}@[a-z]{1,5}\\.com",
            "https://[a-z]{1,8}\\.test/[a-z]{0,4}",
            "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
            "20[0-9]{2}-0[1-9]-1[0-9]T0[0-9]:[0-5][0-9]:[0-5][0-9]Z",
            ".{0,30}",
        ]
        .prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            proptest::collection::btree_map("[a-d]", inner, 0..4)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

proptest! {
    #[test]
    fn inferred_schema_validates_every_sample(
        samples in proptest::collection::vec(json_value(), 1..=6),
    ) {
        let schema = infer_schema(&samples);
        let validator = jsonschema::validator_for(&schema).unwrap();
        for sample in &samples {
            prop_assert!(validator.is_valid(sample), "{sample} vs {schema}");
        }
    }

    #[test]
    fn inference_is_order_independent(
        (samples, shuffled) in proptest::collection::vec(json_value(), 1..=6)
            .prop_flat_map(|v| (Just(v.clone()), Just(v).prop_shuffle())),
    ) {
        prop_assert_eq!(infer_schema(&samples), infer_schema(&shuffled));
    }
}
