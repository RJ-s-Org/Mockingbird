//! The `required` and nullable post-pass over draft schemas.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use mb_import::smart::complete::complete_schema;
use serde_json::json;

#[test]
fn required_from_examples_and_nullable_from_nulls() {
    let mut s = json!({"type": "object", "properties": {
        "id": {"type": "integer"}, "tagline": {"type": "string"}, "homepage": {"type": "string"}}});
    let a = json!({"id": 1, "tagline": null, "homepage": "https://x"});
    let b = json!({"id": 2, "tagline": "t"});
    complete_schema(&mut s, &[&a, &b]);
    assert_eq!(s["required"], json!(["id", "tagline"]));
    assert_eq!(
        s["properties"]["tagline"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(s["properties"]["homepage"]["type"], "string");
}

#[test]
fn without_examples_docs_decide() {
    let mut s = json!({"type": "object", "properties": {
        "name": {"type": "string", "description": "Required. The name."},
        "note": {"type": "string", "description": "Optional note"},
        "collection": {"$ref": "#/components/schemas/C", "description": "May be null."},
        "status": {"type": "string", "enum": ["a", "b"], "description": "nullable"}}});
    complete_schema(&mut s, &[]);
    assert_eq!(s["required"], json!(["name"]));
    assert_eq!(s["properties"]["note"]["type"], json!(["string", "null"]));
    assert_eq!(
        s["properties"]["collection"]["anyOf"][1],
        json!({"type": "null"})
    );
    assert_eq!(s["properties"]["status"]["enum"], json!(["a", "b", null]));
}

#[test]
fn existing_required_is_kept_and_items_recurse() {
    let mut s = json!({"type": "array", "items": {"type": "object", "required": ["id"],
        "properties": {"id": {"type": "integer"}, "sub": {"type": "object", "properties": {"x": {"type": "string"}}}}}});
    let e = json!([{"id": 1, "sub": {"x": null}}]);
    complete_schema(&mut s, &[&e]);
    assert_eq!(s["items"]["required"], json!(["id"]));
    assert_eq!(s["items"]["properties"]["sub"]["required"], json!(["x"]));
    assert_eq!(
        s["items"]["properties"]["sub"]["properties"]["x"]["type"],
        json!(["string", "null"])
    );
}
