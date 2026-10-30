//! The faker's answers are schema-valid for every example contract: every
//! operation's success schema, faked under many seeds, passes the output
//! validator (formats asserted). A fallback body is then a safe answer,
//! never a wrong one.

use std::path::{Path, PathBuf};

use mb_engine::faker::Costs;
use mb_engine::{Faker, SchemaValidator};
use mb_import::compile;
use rand::SeedableRng;
use rand::rngs::StdRng;

/// Seeds faked per operation.
const SEEDS: u64 = 16;

/// Contracts (relative to `examples/apis`) that cannot be faked validly,
/// each with the reason. Empty: every example contract has a finite valid
/// instance for every success schema.
const UNSATISFIABLE: &[(&str, &str)] = &[];

/// Spec files: everything under `examples/apis` except hidden files, the
/// per-example config, README and example request.
fn contracts() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/apis");
    let mut out = Vec::new();
    let mut dirs = vec![root];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                dirs.push(path);
            } else if !matches!(
                name.as_str(),
                "mockingbird.yaml" | "README.md" | "example.json"
            ) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn relative(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.split("examples/apis/").nth(1).unwrap_or(&s).to_string()
}

#[test]
fn faked_success_bodies_validate_for_every_example_contract() {
    let faker = Faker::default().with_now(mb_engine::seed::SEEDED_NOW);
    let mut checked = 0usize;
    let mut invalid = Vec::new();
    let paths = contracts();
    assert!(paths.len() >= 20, "found only {paths:?}");
    for path in &paths {
        let name = relative(path);
        if let Some((_, reason)) = UNSATISFIABLE.iter().find(|(c, _)| *c == name) {
            eprintln!("skipping {name}: {reason}");
            continue;
        }
        let bundle = compile(&std::fs::read(path).unwrap())
            .unwrap_or_else(|e| panic!("{name} does not compile: {e}"));
        for op in &bundle.operations {
            let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
                continue;
            };
            let doc = bundle.with_reachable_defs(schema);
            let validator = SchemaValidator::for_output(&doc).unwrap();
            let costs = Costs::compute(&doc);
            for seed in 0..SEEDS {
                let mut rng = StdRng::seed_from_u64(seed);
                let value = faker.fake_with_costs(&doc, None, &costs, &mut rng);
                checked += 1;
                if !validator.is_valid_output(&value) {
                    let errors = validator.validate(&value).err();
                    invalid.push(format!("{name} {} (seed {seed}): {errors:?}", op.id));
                    break;
                }
            }
        }
    }
    assert!(checked > 1000, "only {checked} bodies faked");
    assert!(invalid.is_empty(), "{}", invalid.join("\n"));
}

/// The two GraphQL contracts are covered: their recursive object types
/// (Shopify's `InventoryItem.variant` and `ProductVariant.inventoryItem`
/// require each other) are where the depth limit used to drop members.
#[test]
fn the_graphql_contracts_are_among_them() {
    let names: Vec<String> = contracts().iter().map(|p| relative(p)).collect();
    for wanted in [
        "shopify-admin-graphql/shopify-admin.graphql",
        "github-graphql/github.graphql",
    ] {
        assert!(
            names.iter().any(|n| n == wanted),
            "{wanted} not in {names:?}"
        );
    }
}

/// Fakes `schema` under [`SEEDS`] seeds, asserting each body validates.
fn assert_fakes_validly(schema: &serde_json::Value) -> Vec<serde_json::Value> {
    let validator = SchemaValidator::for_output(schema).unwrap();
    (0..SEEDS)
        .map(|seed| {
            let value = Faker::default().fake(schema, &mut StdRng::seed_from_u64(seed));
            assert!(validator.is_valid_output(&value), "seed {seed}: {value}");
            value
        })
        .collect()
}

/// Two types requiring each other have no finite instance: lists of them
/// stay empty, optional links to them are left out and nullable ones are
/// null, instead of recursing to the hard limit and emitting `{}`.
#[test]
fn required_cycles_are_never_entered() {
    let schema = serde_json::json!({
        "type": "object",
        "required": ["__typename", "variants", "pageInfo", "featured"],
        "properties": {
            "__typename": {"const": "Product"},
            "variants": {"type": "array", "items": {"$ref": "#/$defs/Variant"}},
            "pageInfo": {"$ref": "#/$defs/PageInfo"},
            "featured": {"anyOf": [{"type": "null"}, {"$ref": "#/$defs/Variant"}]},
            "pinned": {"type": ["object", "null"], "required": ["item"],
                       "properties": {"item": {"$ref": "#/$defs/Item"}}},
            "spare": {"$ref": "#/$defs/Item"}
        },
        "$defs": {
            "PageInfo": {
                "type": "object",
                "required": ["__typename", "hasNextPage"],
                "properties": {
                    "__typename": {"const": "PageInfo"},
                    "hasNextPage": {"type": "boolean"}
                }
            },
            "Variant": {
                "type": "object",
                "required": ["id", "item"],
                "properties": {"id": {"type": "string"}, "item": {"$ref": "#/$defs/Item"}}
            },
            "Item": {
                "type": "object",
                "required": ["variant", "levels"],
                "properties": {
                    "variant": {"$ref": "#/$defs/Variant"},
                    "levels": {"$ref": "#/$defs/PageInfo"}
                }
            }
        }
    });
    for value in assert_fakes_validly(&schema) {
        assert_eq!(value["variants"], serde_json::json!([]));
        assert!(value["featured"].is_null(), "{value}");
        assert!(value.get("spare").is_none(), "{value}");
        assert!(
            value.get("pinned").is_none_or(serde_json::Value::is_null),
            "{value}"
        );
        assert_eq!(value["pageInfo"]["__typename"], "PageInfo");
    }
}

/// An `enum` option of another type than the declared one is never drawn:
/// YAML reads the `n` of `enum: [d, n]` as `false`.
#[test]
fn enum_options_keep_the_declared_type() {
    let schema = serde_json::json!({
        "type": "object",
        "required": ["pod"],
        "properties": {"pod": {"type": "string", "enum": ["d", false]}}
    });
    for value in assert_fakes_validly(&schema) {
        assert_eq!(value["pod"], "d");
    }
}

/// Past the depth limit, required members still get their cheapest valid
/// values: consts, nulls, empty lists and asserted formats.
#[test]
fn the_depth_limit_keeps_required_members() {
    let schema = serde_json::json!({
        "$ref": "#/$defs/Node",
        "$defs": {"Node": {
            "type": "object",
            "required": ["__typename", "id", "at", "parent", "children", "meta"],
            "properties": {
                "__typename": {"const": "Node"},
                "id": {"type": "string", "format": "uuid"},
                "at": {"type": "string", "format": "date-time"},
                "parent": {"anyOf": [{"$ref": "#/$defs/Node"}, {"type": "null"}]},
                "children": {"type": "array", "items": {"$ref": "#/$defs/Node"}},
                "meta": {"type": "object", "required": ["kind"],
                         "properties": {"kind": {"enum": ["a", "b"]}}}
            }
        }}
    });
    assert_fakes_validly(&schema);
    let deep = Faker::new(0).fake(&schema, &mut StdRng::seed_from_u64(1));
    assert_eq!(deep["__typename"], "Node");
    assert!(deep["parent"].is_null(), "{deep}");
    assert_eq!(deep["children"], serde_json::json!([]));
    assert!(
        SchemaValidator::for_output(&schema)
            .unwrap()
            .is_valid_output(&deep)
    );
}
