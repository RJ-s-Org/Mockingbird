//! The published world description schema against the Rust types.

use mb_mcm::world::{SCHEMA, World};
use serde_json::{Value, json};

const SPEC_EXCERPT: &str = include_str!("../fixtures/world-spec-excerpt.json");

fn validator() -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    jsonschema::validator_for(&schema).unwrap()
}

fn errors(v: &jsonschema::Validator, doc: &Value) -> Vec<String> {
    v.iter_errors(doc).map(|e| e.to_string()).collect()
}

#[test]
fn schema_accepts_spec_example_and_rejects_unknown_kind() {
    let v = validator();
    let excerpt: Value = serde_json::from_str(SPEC_EXCERPT).unwrap();
    assert_eq!(errors(&v, &excerpt), Vec::<String>::new());
    assert!(!v.is_valid(&json!({"version":1,"operations":{"x":{"kind":"explode"}}})));
}

#[test]
fn schema_rejects_unknown_keys_and_bad_pointers() {
    let v = validator();
    for bad in [
        json!({"version": 1, "bogus": true}),
        json!({"version": 2}),
        json!({"types": {"t": {"identty": {}}}}),
        json!({"types": {"t": {"identity": {"member": "id", "x": 1}}}}),
        json!({"types": {"t": {"derived": {"n": {"kind": "count", "of": "c", "x": 1}}}}}),
        json!({"types": {"t": {"derived": {"n": {"kind": "avg", "of": "c"}}}}}),
        json!({"operations": {"o": {"answer": {"record": "result"}}}}),
        json!({"operations": {"o": {"answer": {"record": "/a~2"}}}}),
        json!({"operations": {"o": {"changes": {"m": {"from_request": "/a", "value": 1}}}}}),
        json!({"operations": {"o": {"not_found": 99}}}),
        json!({"operations": {"o": {"paging": {"request": "p"}}}}),
    ] {
        assert!(!v.is_valid(&bad), "should be refused: {bad}");
    }
}

/// Every field the types serialize is one the schema knows, so the two stay
/// in step.
#[test]
fn schema_accepts_every_serialized_field() {
    let full = json!({
        "version": 1,
        "types": {
            "account": {
                "identity": {"member": "id", "pattern": "accounts/{id}"},
                "parent": {"type": "org", "member": "org_id", "path_param": "org", "pattern": "orgs/{id}"},
                "lifecycle": {
                    "member": "status",
                    "states": ["open", "closed"],
                    "initial": "open",
                    "soft_delete": {"state": "closed", "listed_with": {"param": "all", "value": true}}
                },
                "unique": [{"members": ["email"], "scope": "org_id", "conflict": 409}],
                "concurrency": {"member": "etag", "header": "If-Match", "conflict": 412},
                "derived": {
                    "entry_count": {"kind": "count", "of": "entry"},
                    "balance": {"kind": "sum", "of": "entry", "member": "amount", "where": {"state": "posted"}}
                }
            }
        },
        "operations": {
            "UpdateAccount": {
                "kind": "update",
                "type": "account",
                "target": {"path_param": "id", "member": "id"},
                "request": {"record": "/account", "records": "/accounts"},
                "answer": {"record": "/result", "records": "/results", "long_running": {"done": "done", "result": "response"}},
                "transition": {"status": "closed"},
                "changes": {"name": {"from_request": "/name"}, "flag": {"value": false}},
                "scope": {"param": "org", "member": "org_id"},
                "filters": [{"param": "q", "member": "name", "kind": "text"}],
                "order": {"member": "created", "descending": true},
                "paging": {"style": "header_cursor", "request": "cursor", "response": "next", "response_header": "Link", "size": "limit"},
                "idempotency": {"header": "Idempotency-Key"},
                "not_found": 404
            }
        }
    });
    let world: World = serde_json::from_value(full.clone()).unwrap();
    let back = serde_json::to_value(&world).unwrap();
    assert_eq!(back, full, "the types keep every field");
    assert_eq!(errors(&validator(), &back), Vec::<String>::new());
}
