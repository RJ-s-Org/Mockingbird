//! URL imports of a contract whose `$ref`s name other documents: the refs
//! are resolved against the contract's URL and every referenced document is
//! fetched through the same guards as the contract itself.
#![cfg(test)]

use std::time::Duration;

use mb_import::smart::fetch::FetchPolicy;
use mb_import::smart::{
    DraftResult, ImportInput, ImportKind, ImportOptions, MAX_REF_DEPTH, MAX_REF_DOCUMENTS,
    NoopObserver, SmartError, run_import,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn loopback() -> FetchPolicy {
    FetchPolicy::parse_allowlist(&["127.0.0.1/32".to_string()]).unwrap()
}

fn options(fetch: FetchPolicy) -> ImportOptions {
    ImportOptions {
        fetch,
        ..ImportOptions::default()
    }
}

/// Serves `body` (YAML) at `at`, expecting exactly `times` requests.
async fn serve(server: &MockServer, at: &str, body: &str, times: u64) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_owned(), "application/yaml"))
        .expect(times)
        .mount(server)
        .await;
}

/// A contract with one `GET /pets` operation answering `schema`.
fn root(schema: &str) -> String {
    format!(
        r#"openapi: 3.0.3
info: {{title: Pets, version: "1"}}
paths:
  /pets:
    get:
      operationId: listPets
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {schema}
"#
    )
}

async fn import(url: String, fetch: FetchPolicy) -> Result<DraftResult, SmartError> {
    run_import(ImportInput::Url(url), None, &options(fetch), &NoopObserver).await
}

fn fetch_error(result: Result<DraftResult, SmartError>) -> String {
    match result {
        Err(SmartError::Fetch(text)) => text,
        other => panic!("expected a fetch error, got {other:?}"),
    }
}

/// The response schema of `GET /pets`, with its local ref followed.
fn pets_schema(draft: &DraftResult) -> &Value {
    let schema = &draft.document["paths"]["/pets"]["get"]["responses"]["200"]["content"]["application/json"]
        ["schema"];
    match schema["$ref"].as_str() {
        Some(r) => draft
            .document
            .pointer(r.trim_start_matches('#'))
            .unwrap_or(&Value::Null),
        None => schema,
    }
}

#[tokio::test]
async fn two_level_relative_refs_are_fetched_and_bundled() {
    let server = MockServer::start().await;
    let mut spec = root(r#"{$ref: "./schemas/pet.yaml"}"#);
    spec.push_str("  /owners:\n    $ref: \"paths/owners.yaml\"\n");
    serve(&server, "/specs/openapi.yaml", &spec, 1).await;
    serve(
        &server,
        "/specs/schemas/pet.yaml",
        r#"{type: object, required: [name], properties: {name: {type: string}, owner: {$ref: "../common/owner.yaml#/Owner"}}}"#,
        1,
    )
    .await;
    serve(
        &server,
        "/specs/paths/owners.yaml",
        r#"get: {operationId: listOwners, responses: {"200": {description: OK, content: {application/json: {schema: {type: array, items: {$ref: "../common/owner.yaml#/Owner"}}}}}}}"#,
        1,
    )
    .await;
    serve(
        &server,
        "/specs/common/owner.yaml",
        "Owner: {type: object, properties: {email: {type: string, format: email}}}",
        1,
    )
    .await;

    let draft = import(format!("{}/specs/openapi.yaml", server.uri()), loopback())
        .await
        .unwrap();
    server.verify().await;
    assert_eq!(draft.meta.source_kind, ImportKind::Text);
    let mut targets: Vec<String> = draft
        .bundle
        .operations
        .iter()
        .map(|op| op.id.clone())
        .collect();
    targets.sort();
    assert_eq!(targets, vec!["listOwners", "listPets"]);
    let pet = pets_schema(&draft);
    assert_eq!(pet["required"], json!(["name"]), "{pet}");
    let owner = draft.document["components"]["schemas"]["Owner"].clone();
    assert_eq!(owner["properties"]["email"]["format"], "email", "{owner}");
    assert_eq!(draft.meta.operations["listPets"].confidence, 1.0);
}

#[tokio::test]
async fn a_ref_to_a_blocked_host_fails_the_import() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/openapi.yaml",
        &root(r#"{$ref: "http://10.20.30.40/pet.yaml"}"#),
        1,
    )
    .await;
    let text = fetch_error(import(format!("{}/openapi.yaml", server.uri()), loopback()).await);
    assert!(text.contains("destination not allowed"), "{text}");
    assert!(text.contains("http://10.20.30.40/pet.yaml"), "{text}");
}

#[tokio::test]
async fn file_and_other_scheme_refs_are_refused() {
    for (r, scheme) in [
        ("file:///etc/passwd", "file"),
        ("https://example.com/pet.yaml", "https"),
        ("ftp://example.com/pet.yaml", "ftp"),
    ] {
        let server = MockServer::start().await;
        serve(
            &server,
            "/openapi.yaml",
            &root(&format!("{{$ref: \"{r}\"}}")),
            1,
        )
        .await;
        let err = import(format!("{}/openapi.yaml", server.uri()), loopback())
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains(r), "{text}");
        assert!(text.contains(&format!("the {scheme} scheme")), "{text}");
        assert!(server.received_requests().await.unwrap().len() == 1);
    }
}

#[tokio::test]
async fn an_oversized_ref_fails_the_import() {
    let server = MockServer::start().await;
    serve(&server, "/openapi.yaml", &root(r#"{$ref: "big.yaml"}"#), 1).await;
    let big = format!("{{type: string, description: \"{}\"}}", "x".repeat(8192));
    serve(&server, "/big.yaml", &big, 1).await;
    let policy = FetchPolicy {
        max_bytes: 4096,
        ..loopback()
    };
    let text = fetch_error(import(format!("{}/openapi.yaml", server.uri()), policy).await);
    assert!(text.contains("exceeds 4096 bytes"), "{text}");
    assert!(text.contains("big.yaml"), "{text}");
}

#[tokio::test]
async fn cyclic_refs_are_fetched_once() {
    let server = MockServer::start().await;
    let mut spec = root(r#"{$ref: "./node.yaml"}"#);
    spec.push_str("components:\n  schemas:\n    Tag: {type: string}\n");
    serve(&server, "/openapi.yaml", &spec, 1).await;
    // node.yaml refers to itself, to edge.yaml (which refers back) and to
    // the root.
    serve(
        &server,
        "/node.yaml",
        r##"{type: object, properties: {next: {$ref: "./node.yaml"}, edge: {$ref: "edge.yaml"}, tag: {$ref: "openapi.yaml#/components/schemas/Tag"}}}"##,
        1,
    )
    .await;
    serve(
        &server,
        "/edge.yaml",
        r#"{type: object, properties: {to: {$ref: "node.yaml"}}}"#,
        1,
    )
    .await;
    let draft = import(format!("{}/openapi.yaml", server.uri()), loopback())
        .await
        .unwrap();
    server.verify().await;
    let node = pets_schema(&draft);
    assert_eq!(
        node["properties"]["next"]["$ref"], "#/components/schemas/node",
        "{node}"
    );
    assert_eq!(
        node["properties"]["tag"]["$ref"], "#/components/schemas/Tag",
        "{node}"
    );
}

#[tokio::test]
async fn too_many_documents_fail_before_fetching_them() {
    let server = MockServer::start().await;
    let props: Vec<String> = (0..=MAX_REF_DOCUMENTS)
        .map(|i| format!("p{i}: {{$ref: \"s{i}.yaml\"}}"))
        .collect();
    let schema = format!("{{type: object, properties: {{{}}}}}", props.join(", "));
    serve(&server, "/openapi.yaml", &root(&schema), 1).await;
    let text = fetch_error(import(format!("{}/openapi.yaml", server.uri()), loopback()).await);
    assert!(
        text.contains(&format!("more than {MAX_REF_DOCUMENTS} documents")),
        "{text}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn refs_nested_too_deep_fail() {
    let server = MockServer::start().await;
    serve(&server, "/openapi.yaml", &root(r#"{$ref: "d1.yaml"}"#), 1).await;
    for i in 1..=MAX_REF_DEPTH + 1 {
        let next = format!(
            "{{type: object, properties: {{n: {{$ref: \"d{}.yaml\"}}}}}}",
            i + 1
        );
        Mock::given(method("GET"))
            .and(path(format!("/d{i}.yaml")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(next, "application/yaml"))
            .mount(&server)
            .await;
    }
    let text = fetch_error(import(format!("{}/openapi.yaml", server.uri()), loopback()).await);
    assert!(
        text.contains(&format!("more than {MAX_REF_DEPTH} documents deep")),
        "{text}"
    );
    // The root and MAX_REF_DEPTH levels; the level past the cap is not
    // fetched.
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        MAX_REF_DEPTH + 1
    );
}

#[tokio::test]
async fn a_slow_ref_times_out() {
    let server = MockServer::start().await;
    serve(&server, "/openapi.yaml", &root(r#"{$ref: "slow.yaml"}"#), 1).await;
    Mock::given(method("GET"))
        .and(path("/slow.yaml"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("{type: string}", "application/yaml")
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    let policy = FetchPolicy {
        timeout: Duration::from_millis(500),
        ..loopback()
    };
    let text = fetch_error(import(format!("{}/openapi.yaml", server.uri()), policy).await);
    assert!(text.contains("timed out"), "{text}");
}

#[tokio::test]
async fn query_secrets_stay_out_of_ref_errors() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/openapi.yaml",
        &root(r#"{$ref: "missing.yaml"}"#),
        1,
    )
    .await;
    let text = fetch_error(
        import(
            format!("{}/openapi.yaml?sig=s3cret", server.uri()),
            loopback(),
        )
        .await,
    );
    assert!(text.contains("HTTP 404"), "{text}");
    assert!(!text.contains("s3cret"), "{text}");
}
