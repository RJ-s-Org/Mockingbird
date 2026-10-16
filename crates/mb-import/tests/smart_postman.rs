//! The Postman v2.0/v2.1 collection importer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use mb_import::smart::SmartError;
use mb_import::smart::model::{DraftOperation, ParamLocation, RecordedImport};
use mb_import::smart::postman::import_postman;
use serde_json::json;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../fixtures/smart/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn petstore() -> RecordedImport {
    import_postman(&fixture("petstore.postman_collection.json")).unwrap()
}

fn legacy() -> RecordedImport {
    import_postman(&fixture("legacy-v2.0.postman_collection.json")).unwrap()
}

fn op<'a>(import: &'a RecordedImport, method: &str, path: &str) -> &'a DraftOperation {
    import
        .operations
        .iter()
        .find(|o| o.method == method && o.path == path)
        .unwrap_or_else(|| panic!("no operation {method} {path}"))
}

#[test]
fn folders_become_tags() {
    let import = petstore();
    assert_eq!(import.title, "Petstore");
    assert!(import.servers.is_empty());
    assert_eq!(op(&import, "GET", "/pets").tags, vec!["Pets"]);
    assert_eq!(op(&import, "GET", "/pets/{petId}").tags, vec!["Pets"]);
    assert_eq!(op(&import, "POST", "/pets").tags, vec!["Pets"]);
    assert_eq!(op(&import, "POST", "/login").tags, vec!["Store"]);
    let login = op(&import, "POST", "/login");
    assert_eq!(login.provenance[0].location, "Store / Login");
    assert_eq!(login.provenance[0].excerpt, "POST {{baseUrl}}/login");
}

#[test]
fn colon_and_variable_params() {
    let import = petstore();
    let get = op(&import, "GET", "/pets/{petId}");
    let param = get
        .parameters
        .iter()
        .find(|p| p.name == "petId")
        .expect("petId parameter");
    assert_eq!(param.location, ParamLocation::Path);
    assert!(param.required);
    assert_eq!(param.schema, Some(json!({"type": "integer"})));
    assert_eq!(param.examples, vec![json!(42)]);
    assert_eq!(get.confidence, 1.0);

    let legacy = legacy();
    let order = op(&legacy, "GET", "/v1/orders/{orderId}");
    let param = order
        .parameters
        .iter()
        .find(|p| p.name == "orderId")
        .expect("orderId parameter");
    assert_eq!(param.location, ParamLocation::Path);
    assert_eq!(param.schema, Some(json!({"type": "integer"})));
    assert_eq!(param.examples, vec![json!(1001)]);
}

#[test]
fn examples_become_responses() {
    let import = petstore();
    let get = op(&import, "GET", "/pets/{petId}");
    let statuses: Vec<u16> = get.responses.iter().map(|r| r.status).collect();
    assert_eq!(statuses, vec![200, 404]);
    let ok = &get.responses[0];
    assert_eq!(ok.content_type.as_deref(), Some("application/json"));
    let schema = ok.schema.as_ref().expect("200 schema");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["id"]["type"], "integer");
    assert_eq!(
        ok.examples,
        vec![json!({"id": 42, "name": "Rex", "tag": "dog"})]
    );
    let missing = &get.responses[1];
    assert_eq!(
        missing.content_type.as_deref(),
        Some("application/problem+json")
    );
    assert_eq!(
        missing.examples,
        vec![json!({"title": "Not Found", "status": 404})]
    );

    let list = op(&import, "GET", "/pets");
    assert_eq!(
        list.responses[0].content_type.as_deref(),
        Some("application/json")
    );
    assert_eq!(list.responses[0].schema.as_ref().unwrap()["type"], "array");
}

#[test]
fn disabled_query_skipped() {
    let import = petstore();
    let list = op(&import, "GET", "/pets");
    let names: Vec<&str> = list.parameters.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["limit"]);
    let limit = &list.parameters[0];
    assert_eq!(limit.location, ParamLocation::Query);
    assert_eq!(limit.schema, Some(json!({"type": "integer"})));
    assert_eq!(limit.examples, vec![json!(10)]);
}

#[test]
fn v20_string_urls() {
    let import = legacy();
    assert_eq!(import.title, "Orders (legacy)");
    let targets: Vec<String> = import.operations.iter().map(|o| o.target()).collect();
    assert_eq!(targets, vec!["GET /v1/orders", "GET /v1/orders/{orderId}"]);
    let list = op(&import, "GET", "/v1/orders");
    assert!(list.tags.is_empty());
    assert_eq!(list.responses.len(), 1);
    assert_eq!(list.responses[0].status, 200);
    assert_eq!(
        list.provenance[0].excerpt,
        "GET https://api.example.com/v1/orders"
    );
    assert_eq!(list.provenance[0].location, "List orders");
}

#[test]
fn unsupported_schema_version() {
    let doc = json!({
        "info": {
            "name": "Old",
            "schema": "https://schema.getpostman.com/json/collection/v1.0.0/collection.json"
        },
        "item": []
    });
    let err = import_postman(&serde_json::to_vec(&doc).unwrap()).unwrap_err();
    assert!(matches!(err, SmartError::Unsupported(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "only Postman collection v2.0 and v2.1 are supported"
    );
    let err = import_postman(b"{\"item\": []}").unwrap_err();
    assert!(matches!(err, SmartError::Unsupported(_)), "{err:?}");
    let err = import_postman(b"not json").unwrap_err();
    assert!(matches!(err, SmartError::Unreadable(_)), "{err:?}");
}

#[test]
fn summaries_from_item_names() {
    let import = petstore();
    assert_eq!(
        op(&import, "GET", "/pets").summary.as_deref(),
        Some("List pets")
    );
    let get = op(&import, "GET", "/pets/{petId}");
    assert_eq!(get.summary.as_deref(), Some("Get pet"));
    assert_eq!(
        get.description.as_deref(),
        Some("Returns one pet by its id.")
    );
    assert_eq!(
        op(&import, "GET", "/pets").description.as_deref(),
        Some("Lists the pets in the store.")
    );
    let create = op(&import, "POST", "/pets");
    assert_eq!(create.summary.as_deref(), Some("Create pet"));
    let body = create.request.as_ref().expect("request body");
    assert_eq!(body.content_type, "application/json");
    assert_eq!(body.examples, vec![json!({"name": "Rex", "tag": "dog"})]);
}

#[test]
fn urlencoded_body_schema() {
    let import = petstore();
    let login = op(&import, "POST", "/login");
    let body = login.request.as_ref().expect("request body");
    assert_eq!(body.content_type, "application/x-www-form-urlencoded");
    assert_eq!(
        body.schema,
        Some(json!({
            "type": "object",
            "properties": {
                "password": {"type": "string"},
                "username": {"type": "string"}
            }
        }))
    );
    for example in &body.examples {
        assert_ne!(example["password"], json!("hunter2-correct-horse"));
    }
}

#[test]
fn formdata_and_merging() {
    let doc = json!({
        "info": {
            "name": "Uploads",
            "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
        },
        "item": [
            {"name": "A", "item": [{
                "name": "Upload one",
                "request": {
                    "method": "post",
                    "url": {"raw": "https://x.test/files/1?api_key=abc"},
                    "body": {"mode": "formdata", "formdata": [
                        {"key": "file", "type": "file", "src": "/tmp/a"},
                        {"key": "client_secret", "value": "shh-formdata", "type": "text"}
                    ]}
                }
            }]},
            {"name": "B", "item": [{
                "name": "Upload two",
                "request": {
                    "method": "POST",
                    "url": {"raw": "https://x.test/files/:fileId", "path": ["files", ":fileId"]}
                }
            }]}
        ]
    });
    let import = import_postman(&serde_json::to_vec(&doc).unwrap()).unwrap();
    assert_eq!(import.operations.len(), 1, "{:#?}", import.operations);
    let upload = &import.operations[0];
    assert_eq!(upload.target(), "POST /files/{fileId}");
    assert_eq!(upload.tags, vec!["A", "B"]);
    assert_eq!(upload.summary.as_deref(), Some("Upload one"));
    assert_eq!(upload.provenance.len(), 2);
    let body = upload.request.as_ref().unwrap();
    assert_eq!(body.content_type, "multipart/form-data");
    let serialized = serde_json::to_string(&import).unwrap();
    assert!(!serialized.contains("shh-formdata"), "{serialized}");
    assert!(!serialized.contains("abc"), "{serialized}");
}

#[test]
fn postman_never_copies_credentials() {
    for import in [petstore(), legacy()] {
        let serialized = serde_json::to_string(&import).unwrap();
        assert!(!serialized.contains("pm_s3cr3t"), "{serialized}");
        assert!(
            !serialized.contains("hunter2-correct-horse"),
            "{serialized}"
        );
        assert!(
            !serialized.contains("eyJhbGciOiJIUzI1NiJ9.session"),
            "{serialized}"
        );
    }
}

#[test]
fn postman_v21_snapshot() {
    insta::assert_yaml_snapshot!(petstore());
}

#[test]
fn slug_parameter_lowers_confidence() {
    let request = |name: &str, raw: &str| json!({"name": name, "request": {"method": "GET", "url": {"raw": raw}}});
    let doc = json!({
        "info": {
            "name": "Teams",
            "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
        },
        "item": [
            request("Red", "{{host}}/teams/red"),
            request("Blue", "{{host}}/teams/blue"),
            request("Green", "{{host}}/teams/green"),
            request("Redirect", "https://x.test/redirect?next=https://y.test/elsewhere")
        ]
    });
    let import = import_postman(&serde_json::to_vec(&doc).unwrap()).unwrap();
    let team = op(&import, "GET", "/teams/{teamId}");
    assert_eq!(team.confidence, 0.8);
    assert_eq!(team.summary.as_deref(), Some("Red"));
    assert_eq!(team.provenance.len(), 3);
    assert_eq!(
        team.parameters[0].examples,
        vec![json!("red"), json!("blue"), json!("green")]
    );
    let redirect = op(&import, "GET", "/redirect");
    assert_eq!(redirect.confidence, 1.0);
    assert_eq!(redirect.parameters[0].name, "next");
}
