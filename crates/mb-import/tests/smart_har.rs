//! Path-parameter inference, redaction and the HAR importer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use mb_import::smart::SmartError;
use mb_import::smart::har::import_har;
use mb_import::smart::model::{DraftOperation, ParamLocation, RecordedImport};
use mb_import::smart::paths::{
    PathTemplate, infer_templates, is_id_like, normalize_path, param_name, path_key,
};
use mb_import::smart::redact::{REDACTED, is_sensitive_name, redact};
use serde_json::json;

fn fixture() -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../fixtures/smart/petstore.har",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn petstore() -> RecordedImport {
    import_har(&fixture()).unwrap()
}

fn op<'a>(import: &'a RecordedImport, method: &str, path: &str) -> &'a DraftOperation {
    import
        .operations
        .iter()
        .find(|o| o.method == method && o.path == path)
        .unwrap_or_else(|| panic!("no operation {method} {path}"))
}

fn templates(paths: &[&str]) -> Vec<String> {
    let owned: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
    let map = infer_templates(&owned);
    owned.iter().map(|p| map[p].template.clone()).collect()
}

#[test]
fn id_segments_become_parameters() {
    assert!(is_id_like("1"));
    assert!(is_id_like("123456"));
    assert!(is_id_like("3f2b8c1e-4a5d-4e6f-9a7b-1c2d3e4f5a6b"));
    assert!(is_id_like("a1b2c3d4e5f6"));
    assert!(!is_id_like("deadbeef"), "hex without a digit");
    assert!(!is_id_like("abc123"), "too short for hex");
    assert!(!is_id_like("pets"));
    assert!(!is_id_like(""));

    assert_eq!(
        templates(&["/api/pets/1", "/api/pets/2", "/api/pets/3", "/api/pets"]),
        vec![
            "/api/pets/{petId}",
            "/api/pets/{petId}",
            "/api/pets/{petId}",
            "/api/pets"
        ]
    );
    assert_eq!(
        templates(&[
            "/orders/3f2b8c1e-4a5d-4e6f-9a7b-1c2d3e4f5a6b",
            "/blobs/a1b2c3d4e5f6"
        ]),
        vec!["/orders/{orderId}", "/blobs/{blobId}"]
    );
    let map = infer_templates(&["/users/1/posts/2".to_string()]);
    assert_eq!(
        map["/users/1/posts/2"],
        PathTemplate {
            template: "/users/{userId}/posts/{postId}".to_string(),
            params: vec!["userId".to_string(), "postId".to_string()],
        }
    );
    let map = infer_templates(&["/pets/1/2".to_string()]);
    assert_eq!(map["/pets/1/2"].template, "/pets/{petId}/{petId2}");
}

#[test]
fn three_distinct_slugs_become_parameter() {
    assert_eq!(
        templates(&["/users/alice", "/users/bob", "/users/carol"]),
        vec!["/users/{userId}"; 3]
    );
    assert_eq!(
        templates(&["/users/alice", "/users/bob"]),
        vec!["/users/alice", "/users/bob"]
    );
    // Agreement on the other (templated) segments is required.
    assert_eq!(
        templates(&[
            "/users/alice/repos",
            "/users/bob/repos",
            "/users/carol/stars"
        ]),
        vec![
            "/users/alice/repos",
            "/users/bob/repos",
            "/users/carol/stars"
        ]
    );
    assert_eq!(
        templates(&[
            "/teams/1/members/alice",
            "/teams/2/members/bob",
            "/teams/3/members/carol"
        ]),
        vec!["/teams/{teamId}/members/{memberId}"; 3]
    );
}

#[test]
fn param_names() {
    assert_eq!(param_name(Some("pets")), "petId");
    assert_eq!(param_name(Some("categories")), "categoryId");
    assert_eq!(param_name(Some("address")), "addressId");
    assert_eq!(param_name(Some("class")), "classId");
    assert_eq!(param_name(None), "id");
}

#[test]
fn normalize_paths() {
    assert_eq!(normalize_path("/pets/:id"), "/pets/{id}");
    assert_eq!(normalize_path("/pets/{{petId}}"), "/pets/{petId}");
    assert_eq!(
        normalize_path("https://api.example.com/v1/pets?limit=1#top"),
        "/v1/pets"
    );
    assert_eq!(normalize_path("https://api.example.com"), "/");
    assert_eq!(normalize_path("/pets/"), "/pets");
    assert_eq!(normalize_path("/"), "/");
    assert_eq!(normalize_path("pets//1"), "/pets/1");
    assert_eq!(normalize_path(""), "/");

    assert_eq!(
        path_key("get", "/pets/{petId}/"),
        ("GET".to_string(), "/pets/{}".to_string())
    );
    assert_eq!(
        path_key("get", "/pets/{id}"),
        path_key("GET", "/pets/{petId}")
    );
    assert_eq!(path_key("post", "/"), ("POST".to_string(), "/".to_string()));
}

#[test]
fn redaction() {
    for name in [
        "password",
        "X-Api-Key",
        "apikey",
        "api_key",
        "Authorization",
        "session_id",
        "refreshToken",
        "client_secret",
        "Credentials",
        "passwd",
        "cookie",
    ] {
        assert!(is_sensitive_name(name), "{name}");
    }
    assert!(!is_sensitive_name("username"));
    let mut value = json!({
        "user": {"name": "amy", "password": "hunter2"},
        "items": [{"token": {"value": "t"}}, {"ok": 1}],
        "Api-Key": "k"
    });
    redact(&mut value);
    assert_eq!(
        value,
        json!({
            "user": {"name": "amy", "password": REDACTED},
            "items": [{"token": REDACTED}, {"ok": 1}],
            "Api-Key": REDACTED
        })
    );
}

#[test]
fn har_operations() {
    let import = petstore();
    assert_eq!(import.servers, vec!["https://petstore.example.com"]);
    assert_eq!(import.title, "petstore.example.com (HAR import)");

    let list = op(&import, "GET", "/api/pets");
    assert_eq!(list.confidence, 1.0);
    assert_eq!(list.parameters.len(), 1);
    let status = &list.parameters[0];
    assert_eq!(status.name, "status");
    assert_eq!(status.location, ParamLocation::Query);
    assert!(!status.required);
    assert_eq!(status.examples, vec![json!("available")]);
    assert_eq!(list.responses.len(), 1);
    assert_eq!(list.responses[0].status, 200);
    assert_eq!(
        list.responses[0].schema.as_ref().unwrap()["type"],
        json!("array")
    );

    let get = op(&import, "GET", "/api/pets/{petId}");
    assert_eq!(get.parameters.len(), 1);
    let pet_id = &get.parameters[0];
    assert_eq!(pet_id.name, "petId");
    assert_eq!(pet_id.location, ParamLocation::Path);
    assert!(pet_id.required);
    assert_eq!(pet_id.schema, Some(json!({"type": "integer"})));
    let ok = &get.responses[0];
    assert_eq!(ok.status, 200);
    assert_eq!(ok.content_type.as_deref(), Some("application/json"));
    let schema = ok.schema.as_ref().unwrap();
    let required = schema["required"].as_array().unwrap();
    assert!(required.contains(&json!("id")));
    assert!(required.contains(&json!("createdAt")));
    assert_eq!(
        schema["properties"]["createdAt"]["format"],
        json!("date-time")
    );
    assert_eq!(ok.examples.len(), 3);
    assert!(get.provenance.len() <= 3);
    assert_eq!(get.provenance[0].location, "entry 2");
    assert_eq!(
        get.provenance[0].excerpt,
        "GET https://petstore.example.com/api/pets/1 → 200"
    );

    let create = op(&import, "POST", "/api/pets");
    let body = create.request.as_ref().unwrap();
    assert_eq!(body.content_type, "application/json");
    assert_eq!(
        body.schema.as_ref().unwrap()["properties"]["name"]["type"],
        json!("string")
    );
    assert_eq!(create.responses[0].status, 201);

    let delete = op(&import, "DELETE", "/api/pets/{petId}");
    assert_eq!(delete.responses.len(), 1);
    assert_eq!(delete.responses[0].status, 204);
    assert!(delete.responses[0].schema.is_none());
    assert!(delete.responses[0].content_type.is_none());
    assert!(delete.responses[0].examples.is_empty());

    let targets: Vec<String> = import.operations.iter().map(|o| o.target()).collect();
    assert_eq!(
        targets,
        vec![
            "POST /api/login",
            "GET /api/pets",
            "POST /api/pets",
            "DELETE /api/pets/{petId}",
            "GET /api/pets/{petId}",
        ]
    );
}

#[test]
fn har_slug_parameter_lowers_confidence() {
    let entries: Vec<_> = ["alice", "bob", "carol"]
        .iter()
        .map(|u| {
            json!({
                "request": {"method": "GET", "url": format!("https://x.test/users/{u}"), "queryString": []},
                "response": {"status": 200, "content": {"mimeType": "application/json", "text": "{\"a\":1}"}}
            })
        })
        .collect();
    let har = json!({"log": {"version": "1.2", "entries": entries}});
    let import = import_har(har.to_string().as_bytes()).unwrap();
    assert_eq!(import.operations.len(), 1);
    assert_eq!(import.operations[0].path, "/users/{userId}");
    assert_eq!(import.operations[0].confidence, 0.8);
    let param = &import.operations[0].parameters[0];
    assert_eq!(param.name, "userId");
    assert_eq!(param.schema.as_ref().unwrap()["type"], json!("string"));
}

#[test]
fn har_skips_assets_and_other_origins() {
    let import = petstore();
    let codes: Vec<&str> = import.warnings.iter().map(|w| w.code.as_str()).collect();
    assert!(codes.contains(&"entries_skipped"), "{codes:?}");
    assert!(codes.contains(&"other_origins"), "{codes:?}");
    let other = import
        .warnings
        .iter()
        .find(|w| w.code == "other_origins")
        .unwrap();
    assert!(other.message.contains("https://cdn.example.net"));
    for o in &import.operations {
        assert!(!o.path.starts_with("/static"), "{}", o.path);
        assert!(!o.path.contains("/x"), "{}", o.path);
    }
}

#[test]
fn har_never_copies_credentials() {
    let text = serde_json::to_string(&petstore()).unwrap();
    for secret in ["s3cr3t", "session=abc", "hunter2", "tok_live_123"] {
        assert!(!text.contains(secret), "{secret} leaked");
    }
    assert!(text.contains(REDACTED));
}

#[test]
fn har_base64_content_decoded() {
    let import = petstore();
    let get = op(&import, "GET", "/api/pets/{petId}");
    assert!(
        get.responses[0]
            .examples
            .iter()
            .any(|e| e["name"] == json!("Tom")),
        "the base64 body of /api/pets/2 is an example"
    );
}

#[test]
fn har_rejects_non_har() {
    assert!(matches!(
        import_har(br#"{"foo":1}"#),
        Err(SmartError::Unsupported(m)) if m == "not a HAR file"
    ));
    assert!(matches!(import_har(b"{"), Err(SmartError::Unreadable(_))));
}

#[test]
fn har_petstore_snapshot() {
    insta::assert_yaml_snapshot!(petstore());
}
