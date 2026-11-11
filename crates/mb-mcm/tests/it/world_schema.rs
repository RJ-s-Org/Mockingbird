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
        json!({"operations": {"o": {"request": {"instructions": {"kind": "kind"}}}}}),
        json!({"operations": {"o": {"request": {"instructions": {"at": "/i", "kind": "kind",
            "kinds": {"k": {"changes": {"m": {"append": "/v", "value": 1}}}}}}}}}),
        json!({"operations": {"o": {"paging": {"request": "p"}}}}),
        json!({"operations": {"o": {"filters": [{"param": "f"}]}}}),
        json!({"operations": {"o": {"scope": {"param": "p", "member": "m", "kind": "under"}}}}),
        json!({"operations": {"o": {"ignores": [""]}}}),
        json!({"operations": {"o": {"filters": [{"param": "f", "kind": "eq", "grammar": "scim"}]}}}),
        json!({"operations": {"o": {"filters": [{"param": "f", "grammar": "wiql"}]}}}),
        json!({"operations": {"o": {"filters": [{"param": "f", "grammar": "odata", "member": "m"}]}}}),
        json!({"operations": {"o": {"filters": [{"param": "f", "kind": "eq", "member_prefix": "m"}]}}}),
        json!({"operations": {"o": {"fields": {"separator": ","}}}}),
        json!({"operations": {"o": {"fields": {"param": "f", "separator": ""}}}}),
        json!({"operations": {"o": {"queue": {"effect": "peek"}}}}),
        json!({"operations": {"o": {"effects": [{"op": "move", "type": "t"}]}}}),
        json!({"operations": {"o": {"effects": [{"op": "delete", "id": "'x'"}]}}}),
        json!({"operations": {"o": {"effects": [{"op": "create", "type": "t", "then": 1}]}}}),
        json!({"types": {"t": {"computed": {"n": 1}}}}),
        json!({"operations": {"o": {"list": {"sort": {"param": "sort"}}}}}),
        json!({"operations": {"o": {"list": {"where": ""}}}}),
        json!({"operations": {"o": {"list": {"filter": "true"}}}}),
        json!({"operations": {"o": {"queue": {"effect": "take", "count": "count"}}}}),
        json!({"operations": {"o": {"queue": {"effect": "take", "requeue": {"at": "/m"}}}}}),
        json!({"types": {"t": {"lifecycle": {"member": "s", "soft_delete": {"state": "x", "unless": {"param": "force"}}}}}}),
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
                "identity": {"member": "id", "pattern": "accounts/{id}", "alternates": ["email"]},
                "parent": {"type": "org", "member": "org_id", "path_param": "org", "pattern": "orgs/{id}", "embedded": "accounts"},
                "lifecycle": {
                    "member": "status",
                    "states": ["open", "closed"],
                    "initial": "open",
                    "soft_delete": {
                        "state": "closed",
                        "listed_with": {"param": "all", "value": true},
                        "unless": {"param": "force", "value": true}
                    }
                },
                "unique": [{"members": ["email"], "scope": "org_id", "conflict": 409}],
                "concurrency": {"member": "etag", "header": "If-Match", "conflict": 412},
                "derived": {
                    "entry_count": {"kind": "count", "of": "entry"},
                    "balance": {"kind": "sum", "of": "entry", "member": "amount", "where": {"state": "posted"}}
                },
                "computed": {"label": "record.name + ' (' + record.status + ')'"}
            }
        },
        "operations": {
            "UpdateAccount": {
                "kind": "update",
                "type": "account",
                "target": {"path_param": "id", "member": "id"},
                "request": {"record": "/account", "records": "/accounts", "instructions": {
                    "at": "/instructions", "kind": "kind",
                    "kinds": {"rename": {"changes": {
                        "name": {"from_instruction": "/value"},
                        "note": {"from_request": "/comment"},
                        "flags.{env}.on": {"value": true},
                        "tags": {"append": "/values"},
                        "labels": {"remove": "/values"}
                    }}}
                }},
                "answer": {"record": "/result", "records": "/results", "long_running": {"done": "done", "result": "response"}, "identity_header": "Location"},
                "transition": {"status": "closed"},
                "changes": {"name": {"from_request": "/name"}, "flag": {"value": false}},
                "set": {"name": "lower(request.body.name)"},
                "refuse": [{"when": "record.status == 'closed'", "status": 409, "code": "closed", "message": {"expr": "'closed: ' + record.id"}}],
                "effects": [
                    {"op": "create", "type": "entry", "set": {"amount": "0"}, "when": "true"},
                    {"op": "update", "type": "org", "id": "record.org_id", "set": {"touched": "now"}},
                    {"op": "delete", "type": "entry", "id": "'e-' + record.id"}
                ],
                "scope": {"param": "/path", "member": "path", "kind": "prefix", "recursive": "/recursive"},
                "filters": [
                    {"param": "q", "member": "name", "kind": "text"},
                    {"param": "filter", "grammar": "deep_object", "member_prefix": "meta"}
                ],
                "order": {"member": "created", "descending": true},
                "list": {"where": "it.status != 'closed'", "sort": {"param": "sort", "by": "[{'member': value, 'desc': false}]"}},
                "paging": {"style": "header_cursor", "request": "cursor", "response": "next", "response_header": "Link", "size": "limit"},
                "ignores": ["version", "/ActiveList/Include"],
                "idempotency": {"header": "Idempotency-Key"},
                "not_found": 404,
                "fields": {"param": "fields", "separator": ",", "always": ["id"], "default": []},
                "queue": {"effect": "take", "count": "/count", "requeue": {"at": "/mode", "value": ["keep"]}, "redelivered": "redelivered"}
            }
        }
    });
    let world: World = serde_json::from_value(full.clone()).unwrap();
    let back = serde_json::to_value(&world).unwrap();
    assert_eq!(back, full, "the types keep every field");
    assert_eq!(errors(&validator(), &back), Vec::<String>::new());
}

/// The root pointer `""` is a record pointer only: `answer.record` and
/// `request.record` take it, the `records` pointers and `from_request` do not.
#[test]
fn schema_accepts_a_root_answer_record_only() {
    let v = validator();
    for good in [
        json!({"operations": {"o": {"answer": {"record": ""}}}}),
        json!({"operations": {"o": {"request": {"record": ""}}}}),
        json!({"operations": {"o": {"answer": {"record": "/data/result/0"}}}}),
    ] {
        assert_eq!(errors(&v, &good), Vec::<String>::new(), "{good}");
    }
    for bad in [
        json!({"operations": {"o": {"answer": {"records": ""}}}}),
        json!({"operations": {"o": {"request": {"records": ""}}}}),
        json!({"operations": {"o": {"changes": {"m": {"from_request": ""}}}}}),
        json!({"operations": {"o": {"answer": {"record": "data"}}}}),
    ] {
        assert!(!v.is_valid(&bad), "should be refused: {bad}");
    }
}

#[test]
fn schema_accepts_alternates_and_identity_header() {
    let v = validator();
    let mut doc: Value = serde_json::from_str(SPEC_EXCERPT).unwrap();
    doc["types"]["probe"] =
        json!({"identity": {"member": "id", "alternates": ["alias", "meta.slug"]}});
    for header in [json!("Resource-ID"), json!(false)] {
        doc["operations"]["probeCreate"] = json!({"answer": {"identity_header": header}});
        assert_eq!(errors(&v, &doc), Vec::<String>::new(), "{header}");
        let world: World = serde_json::from_value(doc.clone()).unwrap();
        assert_eq!(serde_json::to_value(&world).unwrap(), doc);
    }
    for bad in [json!(true), json!(""), json!(3)] {
        doc["operations"]["probeCreate"] = json!({"answer": {"identity_header": bad}});
        assert!(!v.is_valid(&doc), "should be refused: {bad}");
    }
}

#[test]
fn schema_accepts_an_identity_without_member() {
    let v = validator();
    let doc = json!({"types": {"t": {"identity": {"alternates": ["a"]}}}});
    assert_eq!(errors(&v, &doc), Vec::<String>::new());
    assert!(serde_json::from_value::<World>(doc).is_ok());
    assert!(!v.is_valid(&json!({"types": {"t": {"identity": {"alternates": "a"}}}})));
}
