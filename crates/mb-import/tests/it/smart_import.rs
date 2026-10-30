//! OpenAPI assembly, draft compilation and the `run_import` pipeline.
#![cfg(test)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mb_import::smart::assemble::{assemble_openapi, compile_draft, diff_bundles, refresh_meta};
use mb_import::smart::fetch::FetchPolicy;
use mb_import::smart::infer::infer_schema;
use mb_import::smart::merge::{MergeBase, merge_drafts};
use mb_import::smart::model::{
    DraftMeta, DraftOperation, DraftParam, DraftResponse, ImportKind, OperationMeta, ParamLocation,
    TokenUsage,
};
use mb_import::smart::{
    DraftResult, ExtractLlm, ImportInput, ImportObserver, ImportOptions, NoopObserver, Phase,
    SmartError, run_import,
};
use mb_llm::{HopSettings, ScriptedProvider, Usage};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::fixture;

fn op(method: &str, path: &str) -> DraftOperation {
    DraftOperation {
        method: method.to_string(),
        path: path.to_string(),
        operation_id: None,
        summary: None,
        description: None,
        tags: Vec::new(),
        parameters: Vec::new(),
        request: None,
        responses: vec![DraftResponse {
            status: 200,
            description: None,
            content_type: None,
            schema: Some(json!({"type": "object"})),
            examples: Vec::new(),
        }],
        confidence: 0.6,
        provenance: Vec::new(),
    }
}

fn with_id(mut o: DraftOperation, id: &str) -> DraftOperation {
    o.operation_id = Some(id.to_string());
    o
}

fn llm(provider: Arc<ScriptedProvider>, max_tokens: u32) -> ExtractLlm {
    ExtractLlm {
        provider,
        settings: HopSettings {
            timeout: Duration::from_secs(5),
            temperature: 0.1,
            max_tokens,
        },
    }
}

fn md_input(bytes: Vec<u8>) -> ImportInput {
    ImportInput::Bytes {
        kind: ImportKind::Markdown,
        name: Some("petstore.md".to_string()),
        bytes,
    }
}

fn pets_answer() -> String {
    json!({"operations": [
        {"method": "GET", "path": "/pets", "summary": "List pets", "confidence": 0.9,
         "evidence": "Returns all pets, newest first.",
         "parameters": [{"name": "limit", "in": "query", "schema": {"type": "integer"}}],
         "responses": [{"status": 200, "example": [{"id": 1, "name": "Rex", "tag": "dog"}]}]},
        {"method": "GET", "path": "/pets/{petId}", "confidence": 0.9,
         "evidence": "Returns the pet with the given id",
         "parameters": [{"name": "petId", "in": "path", "schema": {"type": "integer"}}],
         "responses": [{"status": 200, "example": {"id": 1, "name": "Rex", "tag": "dog"}},
                       {"status": 404, "description": "No such pet"}]},
        {"method": "POST", "path": "/pets", "confidence": 0.9,
         "evidence": "Creates a pet. The request body is JSON:",
         "request": {"example": {"name": "Rex", "tag": "dog"}},
         "responses": [{"status": 201, "example": {"id": 3, "name": "Rex", "tag": "dog"}}]}
    ]})
    .to_string()
}

fn usage(prompt: u32, completion: u32) -> Usage {
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
    }
}

/// Number of operations of the `paths` of an OpenAPI document.
fn path_operations(doc: &Value) -> usize {
    doc["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|item| item.as_object().unwrap().len())
        .sum()
}

fn codes(meta: &DraftMeta) -> Vec<&str> {
    meta.warnings.iter().map(|w| w.code.as_str()).collect()
}

#[test]
fn assemble_fills_inferred_schemas_and_path_params() {
    let examples = vec![
        json!({"id": 1, "name": "Rex"}),
        json!({"id": 2, "name": "Tom"}),
    ];
    let mut o = op("GET", "/pets/{petId}");
    o.summary = Some("Get a pet".to_string());
    o.tags = vec!["pets".to_string()];
    o.parameters = vec![
        DraftParam {
            name: "limit".to_string(),
            location: ParamLocation::Query,
            required: false,
            description: None,
            schema: None,
            examples: vec![json!("10"), json!("20")],
        },
        DraftParam {
            name: "ghost".to_string(),
            location: ParamLocation::Path,
            required: true,
            description: None,
            schema: None,
            examples: Vec::new(),
        },
        DraftParam {
            name: "X-Trace".to_string(),
            location: ParamLocation::Header,
            required: false,
            description: None,
            schema: None,
            examples: Vec::new(),
        },
    ];
    o.responses = vec![DraftResponse {
        status: 200,
        description: None,
        content_type: None,
        schema: None,
        examples: examples.clone(),
    }];
    let (doc, ids, warnings) =
        assemble_openapi("Pets", &["https://pets.example.com".to_string()], &[o]);
    assert_eq!(ids, vec!["get_pets_petid"]);
    assert_eq!(doc["openapi"], "3.1.0");
    assert_eq!(doc["info"]["title"], "Pets");
    assert_eq!(doc["info"]["version"], "0.1.0-draft");
    assert_eq!(
        doc["info"]["description"],
        "Draft generated by Mockingbird smart import"
    );
    assert_eq!(doc["servers"], json!([{"url": "https://pets.example.com"}]));
    let get = &doc["paths"]["/pets/{petId}"]["get"];
    assert_eq!(get["summary"], "Get a pet");
    assert_eq!(get["tags"], json!(["pets"]));
    let params = get["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 3, "{params:?}");
    assert_eq!(
        params[0],
        json!({"name": "petId", "in": "path", "required": true, "schema": {"type": "string"}})
    );
    assert_eq!(params[1]["name"], "limit");
    assert_eq!(params[1]["schema"], json!({"type": "integer"}));
    assert_eq!(params[2]["name"], "X-Trace");
    assert_eq!(params[2]["schema"], json!({"type": "string"}));
    let media = &get["responses"]["200"]["content"]["application/json"];
    assert_eq!(media["schema"], infer_schema(&examples));
    assert_eq!(media["example"], examples[0]);
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, "missing_path_parameter");
    assert_eq!(warnings[0].operation.as_deref(), Some("GET /pets/{petId}"));
    compile_draft(&doc).unwrap();
}

#[test]
fn assemble_unique_operation_ids() {
    let ops = [
        with_id(op("GET", "/pets"), "listPets"),
        with_id(op("GET", "/pets/{id}"), "listPets"),
        with_id(op("POST", "/pets"), "bad id!"),
    ];
    let (doc, ids, _) = assemble_openapi("Pets", &[], &ops);
    assert_eq!(ids, vec!["listPets", "get_pets_id", "bad_id"]);
    assert_eq!(
        doc["paths"]["/pets/{id}"]["get"]["operationId"],
        "get_pets_id"
    );
    let (_, bundle) = compile_draft(&doc).unwrap();
    let mut got: Vec<String> = bundle.operations.iter().map(|o| o.id.clone()).collect();
    got.sort();
    assert_eq!(got, vec!["bad_id", "get_pets_id", "listPets"]);

    let (_, ids, _) = assemble_openapi(
        "Pets",
        &[],
        &[
            op("GET", "/pets"),
            with_id(op("GET", "/pets/x"), "get_pets"),
        ],
    );
    assert_eq!(ids, vec!["get_pets", "get_pets_x"]);
    let (_, ids, _) = assemble_openapi("Pets", &[], &[op("GET", "/a-b"), op("GET", "/a_b")]);
    assert_eq!(ids, vec!["get_a_b", "get_a_b_2"]);
}

#[test]
fn assemble_adds_default_response_with_warning() {
    let mut o = op("DELETE", "/pets");
    o.responses.clear();
    let (doc, _, warnings) = assemble_openapi("Pets", &[], &[o]);
    assert_eq!(
        doc["paths"]["/pets"]["delete"]["responses"],
        json!({"200": {"description": "OK"}})
    );
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, "no_responses");
    compile_draft(&doc).unwrap();
}

#[tokio::test]
async fn har_draft_openapi_snapshot() {
    let input = ImportInput::Bytes {
        kind: ImportKind::Har,
        name: Some("petstore.har".to_string()),
        bytes: fixture("smart/petstore.har"),
    };
    let draft = run_import(input, None, &ImportOptions::default(), &NoopObserver)
        .await
        .unwrap();
    insta::assert_yaml_snapshot!(draft.document);
    assert_eq!(draft.meta.source_kind, ImportKind::Har);
    assert!(!draft.meta.operations.is_empty());
    for (id, meta) in &draft.meta.operations {
        assert!(meta.confidence >= 0.8, "{id}: {}", meta.confidence);
        assert!(!meta.edited);
    }
    assert_eq!(
        draft.bundle.operations.len(),
        path_operations(&draft.document)
    );
    assert_eq!(draft.meta.operations.len(), draft.bundle.operations.len());
    assert_eq!(
        draft.source,
        serde_json::to_vec_pretty(&draft.document).unwrap()
    );
}

#[tokio::test]
async fn postman_draft_compiles() {
    let input = ImportInput::Bytes {
        kind: ImportKind::Postman,
        name: None,
        bytes: fixture("smart/petstore.postman_collection.json"),
    };
    let draft = run_import(input, None, &ImportOptions::default(), &NoopObserver)
        .await
        .unwrap();
    assert_eq!(draft.meta.source_kind, ImportKind::Postman);
    assert!(!draft.bundle.operations.is_empty());
    for operation in &draft.bundle.operations {
        assert!(
            draft.meta.operations.contains_key(&operation.id),
            "{} missing from meta",
            operation.id
        );
    }
    assert_eq!(draft.meta.chunk_count, 0);
}

#[tokio::test]
async fn markdown_import_with_scripted_llm() {
    let doc = String::from_utf8(fixture("smart/petstore.md")).unwrap();
    let provider = Arc::new(ScriptedProvider::with_usage(vec![Ok((
        pets_answer(),
        usage(1200, 300),
    ))]));
    let draft = run_import(
        md_input(doc.clone().into_bytes()),
        Some(&llm(provider.clone(), 1000)),
        &ImportOptions::default(),
        &NoopObserver,
    )
    .await
    .unwrap();
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(draft.meta.chunk_count, 1);
    assert_eq!(draft.meta.chunks_processed, 1);
    assert_eq!(draft.meta.source_name.as_deref(), Some("petstore.md"));
    assert_eq!(draft.meta.source_url, None);
    assert_eq!(
        draft.meta.usage,
        TokenUsage {
            prompt_tokens: 1200,
            completion_tokens: 300
        }
    );
    assert_eq!(draft.meta.operations.len(), 3);
    assert_eq!(draft.bundle.operations.len(), 3);
    assert_eq!(draft.document["info"]["title"], "Petstore API");
    let normalized_doc = doc.split_whitespace().collect::<Vec<_>>().join(" ");
    for (id, meta) in &draft.meta.operations {
        assert!(!meta.provenance.is_empty(), "{id}");
        for p in &meta.provenance {
            assert!(
                normalized_doc.contains(&p.excerpt),
                "{id}: {:?} is not in the doc",
                p.excerpt
            );
        }
    }
    let post = &draft.document["paths"]["/pets"]["post"];
    assert_eq!(
        post["requestBody"]["content"]["application/json"]["example"],
        json!({"name": "Rex", "tag": "dog"})
    );
}

#[tokio::test]
async fn doc_import_without_llm_fails() {
    let err = run_import(
        md_input(fixture("smart/petstore.md")),
        None,
        &ImportOptions::default(),
        &NoopObserver,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, SmartError::LlmRequired), "{err:?}");
}

/// Markdown of three sections, each large enough to fill a chunk.
fn three_chunk_doc() -> String {
    let mut doc = String::from("# Items API\n\n");
    for section in 1..=3 {
        doc.push_str(&format!("## Section {section}\n\nGET /items{section}\n\n"));
        for line in 0..35 {
            doc.push_str(&format!(
                "Paragraph {line} of section {section} describes the items endpoint in detail. {}\n\n",
                "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(8)
            ));
        }
    }
    doc
}

#[tokio::test]
async fn budget_exhaustion_returns_partial_draft() {
    let answer = json!({"operations": [
        {"method": "GET", "path": "/items1", "confidence": 0.9,
         "responses": [{"status": 200, "schema": {"type": "array"}}]}
    ]})
    .to_string();
    let provider = Arc::new(ScriptedProvider::with_usage(vec![
        Ok((answer.clone(), usage(120_000, 30_000))),
        Ok((answer.clone(), usage(120_000, 30_000))),
        Ok((answer, usage(120_000, 30_000))),
    ]));
    let opts = ImportOptions {
        max_tokens: 200_000,
        ..ImportOptions::default()
    };
    let draft = run_import(
        md_input(three_chunk_doc().into_bytes()),
        Some(&llm(provider.clone(), 50_000)),
        &opts,
        &NoopObserver,
    )
    .await
    .unwrap();
    assert_eq!(draft.meta.chunk_count, 3);
    assert_eq!(draft.meta.chunks_processed, 1);
    assert_eq!(provider.requests().len(), 1);
    assert!(codes(&draft.meta).contains(&"token_budget_exhausted"));
    let warning = draft
        .meta
        .warnings
        .iter()
        .find(|w| w.code == "token_budget_exhausted")
        .unwrap();
    assert_eq!(
        warning.message,
        "stopped after 1 of 3 chunks: the token budget of 200000 tokens is spent"
    );
    assert_eq!(draft.bundle.operations.len(), 1);
}

#[tokio::test]
async fn no_operations_is_an_error() {
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(
        json!({"operations": []}).to_string()
    )]));
    let err = run_import(
        md_input(fixture("smart/petstore.md")),
        Some(&llm(provider, 1000)),
        &ImportOptions::default(),
        &NoopObserver,
    )
    .await
    .unwrap_err();
    match err {
        SmartError::NoOperations(text) => assert_eq!(text, ""),
        other => panic!("unexpected {other:?}"),
    }

    // Warnings explain why nothing was found.
    let provider = Arc::new(ScriptedProvider::new(vec![Ok("not json".to_string())]));
    let err = run_import(
        md_input(fixture("smart/petstore.md")),
        Some(&llm(provider, 1000)),
        &ImportOptions::default(),
        &NoopObserver,
    )
    .await
    .unwrap_err();
    let text = err.to_string();
    assert!(
        text.starts_with("no operations were found: chunk 1 skipped"),
        "{text}"
    );
}

/// Cancels at the first report of `cancel_at`, recording every report.
struct CancelAt {
    cancel_at: Phase,
    seen: std::sync::Mutex<Vec<Phase>>,
}

#[async_trait]
impl ImportObserver for CancelAt {
    async fn progress(&self, phase: Phase, _done: u32, _total: u32) -> bool {
        self.seen.lock().unwrap().push(phase);
        phase != self.cancel_at
    }
}

#[tokio::test]
async fn canceled_observer_stops() {
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(pets_answer())]));
    let observer = CancelAt {
        cancel_at: Phase::Analyzing,
        seen: std::sync::Mutex::new(Vec::new()),
    };
    let err = run_import(
        md_input(fixture("smart/petstore.md")),
        Some(&llm(provider.clone(), 1000)),
        &ImportOptions::default(),
        &observer,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, SmartError::Canceled), "{err:?}");
    assert!(provider.requests().is_empty());
    assert_eq!(
        *observer.seen.lock().unwrap(),
        vec![Phase::Extracting, Phase::Analyzing]
    );
    assert_eq!(Phase::Analyzing.as_str(), "analyzing");
}

fn loopback_options() -> ImportOptions {
    ImportOptions {
        fetch: FetchPolicy::parse_allowlist(&["127.0.0.1/32".to_string()]).unwrap(),
        ..ImportOptions::default()
    }
}

#[tokio::test]
async fn url_import_uses_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/docs/api"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("smart/petstore.md"), "text/markdown; charset=utf-8"),
        )
        .mount(&server)
        .await;
    let url = format!("{}/docs/api", server.uri());
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(pets_answer())]));
    let draft = run_import(
        ImportInput::Url(url.clone()),
        Some(&llm(provider, 1000)),
        &loopback_options(),
        &NoopObserver,
    )
    .await
    .unwrap();
    assert_eq!(draft.meta.source_kind, ImportKind::Markdown);
    assert_eq!(draft.meta.source_url.as_deref(), Some(url.as_str()));
    assert_eq!(draft.meta.source_name.as_deref(), Some(url.as_str()));
    assert_eq!(draft.bundle.operations.len(), 3);
}

#[tokio::test]
async fn url_json_openapi_rejected() {
    let yaml = fixture("openapi/orders-3.0.yaml");
    let doc: Value = serde_saphyr::from_slice(&yaml).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/openapi.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(serde_json::to_vec(&doc).unwrap(), "application/json"),
        )
        .mount(&server)
        .await;
    let err = run_import(
        ImportInput::Url(format!("{}/openapi.json", server.uri())),
        None,
        &loopback_options(),
        &NoopObserver,
    )
    .await
    .unwrap_err();
    match err {
        SmartError::Unsupported(text) => assert!(text.contains("contracts"), "{text}"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn compile_draft_reports_pointers() {
    let doc = json!({
        "openapi": "3.1.0",
        "info": {"version": "1"},
        "paths": {"/pets": {"get": {"responses": {"200": {
            "description": "OK",
            "content": {"application/json": {"schema": {"type": "strange"}}}
        }}}}}
    });
    let issues = compile_draft(&doc).unwrap_err();
    let pointers: Vec<&str> = issues.iter().map(|i| i.pointer.as_str()).collect();
    assert_eq!(
        pointers,
        vec![
            "/info/title",
            "/paths/~1pets/get/responses/200/content/application~1json/schema"
        ]
    );

    let issues = compile_draft(&json!({"openapi": "2.0", "info": {"title": "x"}})).unwrap_err();
    assert_eq!(issues[0].pointer, "/openapi");
    let issues = compile_draft(&json!([1])).unwrap_err();
    assert_eq!(issues[0].pointer, "/openapi");

    // Component references resolve against the document's components.
    let doc = json!({
        "openapi": "3.1.0",
        "info": {"title": "Pets", "version": "1"},
        "components": {"schemas": {"Pet": {"type": "object"}, "Bad": {"type": 7}}},
        "paths": {"/pets": {"get": {"responses": {"200": {
            "description": "OK",
            "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Pet"}}}
        }}}}}
    });
    let issues = compile_draft(&doc).unwrap_err();
    let pointers: Vec<&str> = issues.iter().map(|i| i.pointer.as_str()).collect();
    assert_eq!(pointers, vec!["/components/schemas/Bad"]);
}

/// Schemas are checked together first; each issue still names exactly the
/// schemas that fail on their own, including those that would resolve in
/// a combined document.
#[test]
fn compile_draft_names_each_failing_schema() {
    let response = |schema: Value| {
        json!({"responses": {"200": {
            "description": "OK",
            "content": {"application/json": {"schema": schema}}
        }}})
    };
    let doc = json!({
        "openapi": "3.1.0",
        "info": {"title": "Pets", "version": "1"},
        "components": {"schemas": {
            "Pet": {"type": "object", "properties": {"id": {"type": "string"}}},
            // Checked on its own, Pet is stubbed: the pointer into it fails.
            "PetId": {"$ref": "#/components/schemas/Pet/properties/id"},
            "Tag": {"type": "string", "$dynamicAnchor": "tag"}
        }},
        "paths": {
            "/a": {"get": response(json!({"$ref": "#/components/schemas/Pet"}))},
            "/b": {"get": response(json!({"$ref": "#/components/schemas/Missing"}))},
            "/c": {"get": response(json!({"$ref": "#/components/schemas/Pet/properties/id"}))},
            "/d": {"get": response(json!({"type": "array", "items": {"$ref": "#/components/schemas/Tag"}}))},
            "/e": {"get": response(json!({"type": "object", "minProperties": "two"}))}
        }
    });
    let issues = compile_draft(&doc).unwrap_err();
    let pointers: Vec<&str> = issues.iter().map(|i| i.pointer.as_str()).collect();
    assert_eq!(
        pointers,
        vec![
            "/components/schemas/PetId",
            "/paths/~1b/get/responses/200/content/application~1json/schema",
            "/paths/~1e/get/responses/200/content/application~1json/schema",
            // From compiling the draft into a bundle.
            "#/$defs/Missing",
        ]
    );

    // All valid: the combined check passes and nothing is reported.
    let mut valid = doc.clone();
    let schemas = valid["components"]["schemas"].as_object_mut().unwrap();
    schemas.remove("PetId");
    let paths = valid["paths"].as_object_mut().unwrap();
    paths.remove("/b");
    paths.remove("/c");
    paths.remove("/e");
    compile_draft(&valid).unwrap();
    // Without the anchor, the schemas are checked together only.
    valid["components"]["schemas"]
        .as_object_mut()
        .unwrap()
        .remove("Tag");
    valid["paths"].as_object_mut().unwrap().remove("/d");
    compile_draft(&valid).unwrap();
}

fn meta_for(ids: &[&str]) -> DraftMeta {
    DraftMeta {
        source_kind: ImportKind::Markdown,
        source_name: Some("pets.md".to_string()),
        source_url: None,
        chunk_count: 1,
        chunks_processed: 1,
        warnings: Vec::new(),
        operations: ids
            .iter()
            .map(|id| {
                (
                    id.to_string(),
                    OperationMeta {
                        confidence: 0.6,
                        provenance: Vec::new(),
                        edited: false,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
        usage: TokenUsage::default(),
        edited: false,
        sources: Vec::new(),
        merged_from: None,
    }
}

#[test]
fn refresh_meta_marks_edits() {
    let ops = [
        with_id(op("GET", "/pets"), "listPets"),
        with_id(op("GET", "/pets/{id}"), "getPet"),
        with_id(op("DELETE", "/pets/{id}"), "deletePet"),
    ];
    let (doc, _, _) = assemble_openapi("Pets", &[], &ops);
    let (_, old) = compile_draft(&doc).unwrap();
    let meta = meta_for(&["listPets", "getPet", "deletePet"]);

    let mut edited = doc.clone();
    edited["paths"]["/pets/{id}"]["get"]["summary"] = json!("Get a pet");
    edited["paths"]["/pets/{id}"]
        .as_object_mut()
        .unwrap()
        .remove("delete");
    edited["paths"]["/pets"]["post"] = json!({
        "operationId": "createPet",
        "responses": {"201": {"description": "Created"}}
    });
    let (_, new) = compile_draft(&edited).unwrap();

    let refreshed = refresh_meta(&meta, &old, &new);
    assert!(refreshed.edited);
    let ids: Vec<&str> = refreshed.operations.keys().map(String::as_str).collect();
    assert_eq!(ids, vec!["createPet", "getPet", "listPets"]);
    assert_eq!(
        refreshed.operations["listPets"],
        meta.operations["listPets"]
    );
    for id in ["createPet", "getPet"] {
        assert_eq!(refreshed.operations[id].confidence, 1.0);
        assert!(refreshed.operations[id].edited);
    }
    assert_eq!(refreshed.source_name, meta.source_name);
}

#[test]
fn diff_bundles_added_removed_changed() {
    let active_ops = [
        with_id(op("GET", "/pets"), "listPets"),
        with_id(op("GET", "/pets/{id}"), "getPet"),
        with_id(op("PUT", "/pets/{id}"), "putPet"),
        with_id(op("DELETE", "/pets/{id}"), "deletePet"),
    ];
    let (doc, _, _) = assemble_openapi("Pets", &[], &active_ops);
    let (_, active) = compile_draft(&doc).unwrap();

    let mut changed = op("GET", "/pets/{petId}");
    changed.summary = Some("Get a pet".to_string());
    let draft_ops = [
        op("GET", "/pets"),
        changed,
        op("PUT", "/pets/{id}"),
        op("POST", "/pets"),
    ];
    let (doc, _, _) = assemble_openapi("Pets", &[], &draft_ops);
    let (_, draft) = compile_draft(&doc).unwrap();

    let diff = diff_bundles(&active, &draft);
    assert_eq!(diff.added, vec!["POST /pets"]);
    assert_eq!(diff.removed, vec!["DELETE /pets/{id}"]);
    assert_eq!(diff.changed, vec!["GET /pets/{petId}"]);
    assert_eq!(diff_bundles(&draft, &draft), Default::default());
}

/// A HAR file with one JSON exchange per `(method, url, response body)`.
fn har(entries: &[(&str, String, String)]) -> Vec<u8> {
    let entries: Vec<Value> = entries
        .iter()
        .map(|(method, url, body)| {
            json!({
                "startedDateTime": "2026-10-01T10:00:00.000Z",
                "time": 1,
                "request": {"method": method, "url": url, "httpVersion": "HTTP/1.1",
                            "headers": [], "queryString": [], "cookies": [],
                            "headersSize": -1, "bodySize": 0},
                "response": {"status": 200, "statusText": "OK", "httpVersion": "HTTP/1.1",
                             "headers": [{"name": "Content-Type", "value": "application/json"}],
                             "cookies": [],
                             "content": {"size": body.len(), "mimeType": "application/json",
                                         "text": body},
                             "redirectURL": "", "headersSize": -1, "bodySize": body.len()},
                "cache": {}, "timings": {"send": 0, "wait": 1, "receive": 0}
            })
        })
        .collect();
    serde_json::to_vec(
        &json!({"log": {"version": "1.2", "creator": {"name": "t", "version": "1"},
                                       "entries": entries}}),
    )
    .unwrap()
}

/// `{"a": {"a": … 1 …}}`, `levels` objects deep.
fn nested(levels: usize) -> Value {
    let mut v = json!(1);
    for _ in 0..levels {
        v = json!({ "a": v });
    }
    v
}

/// What a ticker task saw while an import ran next to it.
#[derive(Debug)]
struct Stall {
    /// The longest the ticker went unpolled while the import ran.
    longest: Duration,
    /// How long the import took.
    elapsed: Duration,
    /// How many times the ticker ran while the import ran.
    ticks: u32,
}

/// The ticker's last poll, its longest gap and its poll count.
struct Ticks {
    last: std::time::Instant,
    longest: Duration,
    count: u32,
}

/// Runs `fut` on its own current-thread runtime next to a task ticking every
/// millisecond, and reports the longest the ticker went unpolled.
///
/// The work being checked runs on the blocking pool, so the ticker keeps
/// running unless that work moves onto the runtime thread. The verdict
/// ([`assert_runtime_kept_running`]) compares the longest gap with the
/// import's own duration rather than with a fixed budget: CPU-bound work on
/// the runtime thread leaves one gap spanning most of the import however
/// loaded the machine is, while scheduler noise does not grow with it.
fn measure_stall<F: std::future::Future>(fut: F) -> (Stall, F::Output) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let state = Arc::new(std::sync::Mutex::new(Ticks {
            last: std::time::Instant::now(),
            longest: Duration::ZERO,
            count: 0,
        }));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let ticker = tokio::spawn({
            let state = state.clone();
            async move {
                let mut started = Some(started_tx);
                loop {
                    {
                        let mut s = state.lock().unwrap();
                        let now = std::time::Instant::now();
                        s.longest = s.longest.max(now - s.last);
                        s.last = now;
                        s.count += 1;
                    }
                    if let Some(tx) = started.take() {
                        let _ = tx.send(());
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        });
        // The ticker is running; measure from here.
        started_rx.await.unwrap();
        {
            let mut s = state.lock().unwrap();
            s.last = std::time::Instant::now();
            s.longest = Duration::ZERO;
            s.count = 0;
        }
        let begun = std::time::Instant::now();
        let out = fut.await;
        let elapsed = begun.elapsed();
        ticker.abort();
        let s = state.lock().unwrap();
        // Count the gap still open when the import finished, too.
        let longest = s.longest.max(s.last.elapsed().min(elapsed));
        let stall = Stall {
            longest,
            elapsed,
            ticks: s.count,
        };
        (stall, out)
    })
}

/// Fails when the runtime thread was blocked for half the import or more.
/// With the CPU-bound stages inline on the runtime thread, the longest gap is
/// 80-99% of the import; on the blocking pool it is a few milliseconds.
fn assert_runtime_kept_running(stall: &Stall) {
    assert!(stall.ticks > 0, "the ticker never ran: {stall:?}");
    assert!(
        stall.longest < stall.elapsed / 2,
        "the runtime stalled for {:?} of the import's {:?}: CPU-bound work ran on the runtime thread",
        stall.longest,
        stall.elapsed
    );
}

/// The HTML extraction test's input size. Extraction takes about 0.35 s per
/// MiB in a dev build, which keeps the import long next to scheduler noise.
const LARGE_HTML_BYTES: usize = 1024 * 1024;

#[test]
fn large_html_extraction_does_not_block_the_runtime() {
    let mut html = String::from("<!doctype html><html><head><title>Big</title></head><body>");
    while html.len() < LARGE_HTML_BYTES {
        html.push_str("<h2>GET /things/{id}</h2><p>Returns the thing with the given id, as JSON.</p><table><tr><td>id</td><td>integer</td></tr></table>\n");
    }
    html.push_str("</body></html>");
    let input = ImportInput::Bytes {
        kind: ImportKind::Html,
        name: Some("big.html".into()),
        bytes: html.into_bytes(),
    };
    // A budget too small for any chunk: only extraction and chunking run.
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let opts = ImportOptions {
        max_tokens: 1,
        ..ImportOptions::default()
    };
    let (stall, result) = measure_stall(async {
        run_import(
            input,
            Some(&llm(provider.clone(), 1000)),
            &opts,
            &NoopObserver,
        )
        .await
    });
    assert!(
        matches!(result, Err(SmartError::NoOperations(_))),
        "{result:?}"
    );
    assert!(provider.requests().is_empty(), "no chunk fits the budget");
    assert_runtime_kept_running(&stall);
}

/// The HAR test's entry count. Parsing the HAR dominates its import (about
/// 0.07 ms per entry in a dev build), so this test guards the parse stage;
/// assembling its one operation takes a few milliseconds.
const LARGE_HAR_ENTRIES: usize = 3000;

#[test]
fn large_har_assembly_does_not_block_the_runtime() {
    let entries: Vec<(&str, String, String)> = (0..LARGE_HAR_ENTRIES)
        .map(|i| {
            (
                "GET",
                format!("https://api.example.com/r{i}/items?page=1"),
                json!({"id": i, "name": format!("item {i}"), "tags": ["a", "b"],
                       "owner": {"id": i, "email": "a@example.com"}})
                .to_string(),
            )
        })
        .collect();
    let input = ImportInput::Bytes {
        kind: ImportKind::Har,
        name: None,
        bytes: har(&entries),
    };
    let (stall, result) = measure_stall(async {
        run_import(input, None, &ImportOptions::default(), &NoopObserver).await
    });
    let draft = result.unwrap();
    // The varying first segment is a path parameter: one operation, sampled
    // by every entry.
    assert_eq!(
        draft.meta.operations.len(),
        1,
        "{:?}",
        draft.meta.operations
    );
    assert_runtime_kept_running(&stall);
}

/// Every stored form of a draft parses back: the source, the bundle and the
/// metadata.
fn assert_round_trips(source: &[u8], bundle: &mb_mcm::Bundle, meta: &DraftMeta) {
    let doc: Value = serde_json::from_slice(source).expect("the source parses back");
    assert!(doc.is_object());
    let bundle_json = serde_json::to_vec(bundle).unwrap();
    let back: mb_mcm::Bundle =
        serde_json::from_slice(&bundle_json).expect("the bundle parses back");
    assert_eq!(&back, bundle);
    let meta_json = serde_json::to_vec(meta).unwrap();
    serde_json::from_slice::<DraftMeta>(&meta_json).expect("the meta parses back");
}

#[tokio::test]
async fn deeply_nested_har_example_round_trips() {
    let body = nested(120).to_string();
    let input = ImportInput::Bytes {
        kind: ImportKind::Har,
        name: None,
        bytes: har(&[
            ("GET", "https://api.example.com/deep".into(), body),
            (
                "GET",
                "https://api.example.com/flat".into(),
                json!({"id": 1}).to_string(),
            ),
        ]),
    };
    let draft = run_import(input, None, &ImportOptions::default(), &NoopObserver)
        .await
        .unwrap();
    assert_round_trips(&draft.source, &draft.bundle, &draft.meta);
    assert_eq!(draft.bundle.operations.len(), 2);
    assert!(
        codes(&draft.meta).contains(&"too_deep"),
        "{:?}",
        codes(&draft.meta)
    );
}

#[test]
fn deeply_nested_llm_schema_and_examples_round_trip() {
    let mut o = op("POST", "/deep");
    o.request = Some(mb_import::smart::model::DraftBody {
        content_type: "application/json".into(),
        schema: None,
        examples: vec![nested(120)],
    });
    o.responses[0].schema = Some(nested(120));
    o.responses[0].examples = vec![nested(120)];
    o.parameters.push(DraftParam {
        name: "q".into(),
        location: ParamLocation::Query,
        required: false,
        description: None,
        schema: Some(nested(120)),
        examples: vec![nested(120)],
    });
    let (doc, _ids, warnings) = assemble_openapi("Deep", &[], &[o]);
    let (source, bundle) = compile_draft(&doc).unwrap();
    assert_round_trips(&source, &bundle, &meta_for(&[]));
    assert!(
        warnings.iter().any(|w| w.code == "too_deep"),
        "{warnings:?}"
    );
}

#[test]
fn compile_draft_rejects_documents_too_deep_to_store() {
    let doc = json!({
        "openapi": "3.1.0",
        "info": {"title": "Deep", "version": "1"},
        "paths": {"/deep": {"get": {"responses": {"200": {
            "description": "OK",
            "content": {"application/json": {"example": nested(115)}}
        }}}}}
    });
    let issues = compile_draft(&doc).unwrap_err();
    assert!(
        issues.iter().any(|i| i.message.contains("nested")),
        "{issues:?}"
    );
}

#[tokio::test]
async fn url_import_redacts_query_values() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/docs/api"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("smart/petstore.md"), "text/markdown; charset=utf-8"),
        )
        .mount(&server)
        .await;
    let url = format!(
        "{}/docs/api?X-Amz-Signature=s3cret&expires=99#token=t0ken",
        server.uri()
    );
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(pets_answer())]));
    let draft = run_import(
        ImportInput::Url(url),
        Some(&llm(provider.clone(), 1000)),
        &loopback_options(),
        &NoopObserver,
    )
    .await
    .unwrap();
    let expected = format!(
        "{}/docs/api?X-Amz-Signature=REDACTED&expires=REDACTED",
        server.uri()
    );
    assert_eq!(draft.meta.source_url.as_deref(), Some(expected.as_str()));
    assert_eq!(draft.meta.source_name.as_deref(), Some(expected.as_str()));
    let prompts = format!("{:?}", provider.requests());
    assert!(!prompts.contains("s3cret"), "the prompt carries the secret");
    assert!(
        !prompts.contains("t0ken"),
        "the prompt carries the fragment"
    );
}

#[test]
fn schema_warnings_name_the_operation_target() {
    let bundle = mb_import::compile(&fixture("openapi/huge-response.json")).unwrap();
    let ws = mb_import::smart::assemble::schema_warnings(&bundle);
    assert_eq!(ws.len(), 1);
    assert_eq!(ws[0].code, "schema_too_large");
    assert_eq!(ws[0].operation.as_deref(), Some("GET /big"));
}

#[test]
fn docs_operation_ids_are_converted_not_replaced() {
    let ops = [
        with_id(op("GET", "/3/movie/{movie_id}"), "movie-details"),
        with_id(op("GET", "/3/tv"), "tv.list"),
        with_id(op("GET", "/x"), "getX"),
    ];
    let (doc, ids, _) = assemble_openapi("TMDB", &[], &ops);
    assert_eq!(ids, vec!["movie_details", "tv_list", "getX"]);
    let get = &doc["paths"]["/3/movie/{movie_id}"]["get"];
    assert_eq!(get["x-original-operation-id"], "movie-details");
    assert!(
        doc["paths"]["/x"]["get"]
            .get("x-original-operation-id")
            .is_none()
    );
    compile_draft(&doc).unwrap();
}

#[tokio::test]
async fn markdown_draft_has_required_and_nullable() {
    let schema = json!({"type": "object", "properties": {
        "id": {"type": "integer"},
        "tagline": {"type": "string"},
        "homepage": {"type": "string"},
        "belongs_to_collection": {"type": "object", "description": "May be null."}}});
    let answer = json!({"operations": [
        {"method": "GET", "path": "/3/movie/{movie_id}", "operation_id": "movie-details",
         "confidence": 0.9, "evidence": "Get the top level details of a movie by ID.",
         "parameters": [{"name": "movie_id", "in": "path", "schema": {"type": "integer"}}],
         "responses": [{"status": 200, "schema": schema,
            "example": {"id": 550, "tagline": null, "homepage": "https://www.foxmovies.com/movies/fight-club"}}]},
        {"method": "GET", "path": "/3/movie/{movie_id}", "operation_id": "movie-details",
         "confidence": 0.9, "evidence": "Get the top level details of a movie by ID.",
         "responses": [{"status": 200, "example": {"id": 551, "tagline": "Hell, upside down"}}]}
    ]})
    .to_string();
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(answer)]));
    let draft = run_import(
        md_input(fixture("smart/draft-quality.md")),
        Some(&llm(provider, 4000)),
        &ImportOptions::default(),
        &NoopObserver,
    )
    .await
    .unwrap();
    let schema = &draft.document["paths"]["/3/movie/{movie_id}"]["get"]["responses"]["200"]["content"]
        ["application/json"]["schema"];
    assert_eq!(schema["required"], json!(["id", "tagline"]), "{schema}");
    assert_eq!(
        schema["properties"]["tagline"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        schema["properties"]["belongs_to_collection"]["type"],
        json!(["object", "null"])
    );
    assert!(draft.meta.operations.contains_key("movie_details"));
}

#[tokio::test]
async fn low_confidence_operations_are_dropped_with_a_reason() {
    let answer = json!({"operations": [
        {"method": "GET", "path": "/pets", "confidence": 0.9, "evidence": "Returns all pets, newest first.",
         "responses": [{"status": 200, "example": [{"id": 1}]}]},
        {"method": "DELETE", "path": "/pets/{petId}", "confidence": 0.5},
        {"method": "GET", "path": "/owners", "confidence": 0.7,
         "responses": [{"status": 200, "example": []}]}]})
    .to_string();
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(answer)]));
    let draft = run_import(
        md_input(fixture("smart/petstore.md")),
        Some(&llm(provider, 1000)),
        &ImportOptions::default(),
        &NoopObserver,
    )
    .await
    .unwrap();
    assert_eq!(path_operations(&draft.document), 1);
    let low: Vec<_> = draft
        .meta
        .warnings
        .iter()
        .filter(|w| w.code == "low_confidence")
        .collect();
    assert_eq!(low.len(), 2, "{:?}", draft.meta.warnings);
    let delete = low
        .iter()
        .find(|w| w.operation.as_deref() == Some("DELETE /pets/{petId}"))
        .unwrap();
    assert!(
        delete
            .message
            .contains("0.30 is below 0.40 (the documentation gives no response shape)"),
        "{}",
        delete.message
    );
    let owners = low
        .iter()
        .find(|w| w.operation.as_deref() == Some("GET /owners"))
        .unwrap();
    // `/owners` is not in petstore.md: 0.7 is halved to 0.35 with an `ungrounded_operation` warning.
    assert!(
        owners
            .message
            .contains("0.35 is below 0.40 (GET /owners is not grounded"),
        "{}",
        owners.message
    );
}

#[tokio::test]
async fn all_low_confidence_fails_quoting_the_reasons() {
    let answer = json!({"operations": [
        {"method": "GET", "path": "/owners", "confidence": 0.7, "responses": [{"status": 200, "example": []}]},
        {"method": "GET", "path": "/vets", "confidence": 0.7, "responses": [{"status": 200, "example": []}]},
        {"method": "GET", "path": "/visits", "confidence": 0.7, "responses": [{"status": 200, "example": []}]},
        {"method": "DELETE", "path": "/pets/{petId}", "confidence": 0.3}]})
    .to_string();
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(answer)]));
    let err = run_import(
        md_input(fixture("smart/petstore.md")),
        Some(&llm(provider, 1000)),
        &ImportOptions::default(),
        &NoopObserver,
    )
    .await
    .unwrap_err();
    let text = err.to_string();
    assert!(matches!(err, SmartError::NoOperations(_)), "{text}");
    assert!(text.contains("was left out"), "{text}");
}

#[tokio::test]
async fn two_sources_make_one_draft() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/pets.md"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/markdown")
                .set_body_bytes(fixture("smart/petstore.md")),
        )
        .mount(&server)
        .await;
    let har = ImportInput::Bytes {
        kind: ImportKind::Har,
        name: Some("petstore.har".into()),
        bytes: fixture("smart/petstore.har"),
    };
    let url = ImportInput::Url(format!("{}/pets.md", server.uri()));
    let missing = ImportInput::Url(format!("{}/missing.md", server.uri()));
    // The HAR's summaries call comes first; its answer is not used.
    let provider = Arc::new(ScriptedProvider::new(vec![
        Ok(r#"{"summaries": []}"#.into()),
        Ok(pets_answer()),
    ]));
    let draft = mb_import::smart::run_import_sources(
        vec![har, url, missing],
        Some(&llm(provider, 1000)),
        &loopback_options(),
        &NoopObserver,
        &mb_import::smart::fetch::SystemResolver,
    )
    .await
    .unwrap();
    assert_eq!(draft.meta.sources.len(), 3);
    assert_eq!(draft.meta.sources[0], "petstore.har");
    assert!(draft.document["paths"]["/api/pets/{petId}"]["get"].is_object()); // from the HAR
    assert!(draft.document["paths"]["/pets/{petId}"]["get"].is_object()); // from the docs
    assert!(
        draft
            .meta
            .warnings
            .iter()
            .any(|w| w.code == "source_skipped" && w.message.contains("missing.md")),
        "{:?}",
        draft.meta.warnings
    );
}

/// `run_import` of fixtures/smart/petstore.har with no LLM.
async fn har_draft() -> DraftResult {
    let input = ImportInput::Bytes {
        kind: ImportKind::Har,
        name: Some("petstore.har".into()),
        bytes: fixture("smart/petstore.har"),
    };
    run_import(input, None, &ImportOptions::default(), &NoopObserver)
        .await
        .unwrap()
}

/// `run_import` of fixtures/smart/legacy-v2.0.postman_collection.json with no LLM.
async fn postman_draft() -> DraftResult {
    let input = ImportInput::Bytes {
        kind: ImportKind::Postman,
        name: Some("legacy.json".into()),
        bytes: fixture("smart/legacy-v2.0.postman_collection.json"),
    };
    run_import(input, None, &ImportOptions::default(), &NoopObserver)
        .await
        .unwrap()
}

#[tokio::test]
async fn merge_adds_and_replaces_without_removing() {
    let base = har_draft().await;
    let base_ids: Vec<String> = base.meta.operations.keys().cloned().collect();
    let mb = MergeBase {
        version: 3,
        source: base.source.clone(),
        meta: Some(base.meta.clone()),
    };
    let merged = merge_drafts(&mb, postman_draft().await).unwrap();
    for id in &base_ids {
        assert!(merged.meta.operations.contains_key(id), "{id} lost");
    }
    assert!(merged.document["paths"]["/v1/orders/{orderId}"]["get"].is_object());
    assert_eq!(merged.meta.merged_from, Some(3));
}

#[tokio::test]
async fn merging_a_draft_into_itself_is_idempotent() {
    let d = har_draft().await;
    let mb = MergeBase {
        version: 1,
        source: d.source.clone(),
        meta: Some(d.meta.clone()),
    };
    let merged = merge_drafts(&mb, har_draft().await).unwrap();
    assert_eq!(
        merged.meta.operations.keys().collect::<Vec<_>>(),
        d.meta.operations.keys().collect::<Vec<_>>()
    );
    assert_eq!(merged.bundle.operations.len(), d.bundle.operations.len());
}

#[tokio::test]
async fn merge_into_yaml_openapi_works_and_graphql_is_refused() {
    let yaml = MergeBase {
        version: 1,
        source: fixture("openapi/orders-3.0.yaml"),
        meta: None,
    };
    let merged = merge_drafts(&yaml, har_draft().await).unwrap();
    let orders_ops = mb_import::compile(&fixture("openapi/orders-3.0.yaml"))
        .unwrap()
        .operations
        .len();
    assert!(merged.bundle.operations.len() > orders_ops);
    assert!(
        merged
            .meta
            .operations
            .values()
            .any(|m| m.confidence == 1.0 && m.provenance.is_empty())
    );
    let sdl = MergeBase {
        version: 2,
        source: b"type Query { a: Int }".to_vec(),
        meta: None,
    };
    let err = merge_drafts(&sdl, har_draft().await).unwrap_err();
    assert!(
        err.to_string().contains("import with mode replace"),
        "{err}"
    );
}

#[tokio::test]
async fn merge_renames_a_colliding_operation_id() {
    // base: GET /a with operationId "list"; incoming: GET /b with operationId "list".
    let (doc, _, _) = assemble_openapi("T", &[], &[with_id(op("GET", "/a"), "list")]);
    let (source, _) = compile_draft(&doc).unwrap();
    let (doc_b, _, _) = assemble_openapi("T", &[], &[with_id(op("GET", "/b"), "list")]);
    let (source_b, bundle_b) = compile_draft(&doc_b).unwrap();
    let incoming = DraftResult {
        document: doc_b,
        source: source_b,
        bundle: bundle_b,
        meta: meta_for(&["list"]),
    };
    let merged = merge_drafts(
        &MergeBase {
            version: 1,
            source,
            meta: None,
        },
        incoming,
    )
    .unwrap();
    assert_eq!(
        merged.document["paths"]["/b"]["get"]["operationId"],
        "list_2"
    );
    assert_eq!(
        merged.document["paths"]["/b"]["get"]["x-original-operation-id"],
        "list"
    );
    assert!(merged.meta.operations.contains_key("list_2"));
    assert!(merged.meta.operations.contains_key("list"));
}

#[tokio::test]
async fn merge_keeps_the_base_component_and_warns_on_a_conflict() {
    let with_component = |path: &str, kind: &str| {
        let (mut doc, _, _) = assemble_openapi("T", &[], &[op("GET", path)]);
        doc["components"] = json!({"schemas": {"Pet": {"type": kind}}});
        doc
    };
    let (source, _) = compile_draft(&with_component("/a", "object")).unwrap();
    let doc_b = with_component("/b", "string");
    let (source_b, bundle_b) = compile_draft(&doc_b).unwrap();
    let incoming = DraftResult {
        document: doc_b,
        source: source_b,
        bundle: bundle_b,
        meta: meta_for(&["get_b"]),
    };
    let base = MergeBase {
        version: 4,
        source,
        meta: None,
    };
    let merged = merge_drafts(&base, incoming).unwrap();
    assert_eq!(
        merged.document["components"]["schemas"]["Pet"]["type"],
        "object"
    );
    assert!(
        merged
            .meta
            .warnings
            .iter()
            .any(|w| w.code == "conflicting_component" && w.message.contains("Pet")),
        "{:?}",
        merged.meta.warnings
    );
}

#[tokio::test]
async fn re_merging_the_same_import_does_not_pile_up_warnings() {
    let answer = || {
        json!({"operations": [
            {"method": "GET", "path": "/pets", "confidence": 0.9, "evidence": "Returns all pets, newest first.",
             "responses": [{"status": 200, "example": [{"id": 1}]}]},
            {"method": "DELETE", "path": "/pets/{petId}", "confidence": 0.3}]})
        .to_string()
    };
    let import = || async {
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(answer())]));
        run_import(
            md_input(fixture("smart/petstore.md")),
            Some(&llm(provider, 1000)),
            &ImportOptions::default(),
            &NoopObserver,
        )
        .await
        .unwrap()
    };
    let first = import().await;
    let mut base = MergeBase {
        version: 1,
        source: first.source.clone(),
        meta: Some(first.meta.clone()),
    };
    for version in 2..4 {
        let merged = merge_drafts(&base, import().await).unwrap();
        assert_eq!(merged.meta.warnings, first.meta.warnings, "v{version}");
        base = MergeBase {
            version,
            source: merged.source.clone(),
            meta: Some(merged.meta.clone()),
        };
    }
}

#[test]
fn valid_docs_operation_ids_are_kept_as_written() {
    let ops = [
        with_id(op("GET", "/a"), "_internal"),
        with_id(op("GET", "/b"), "get__pets"),
    ];
    let (doc, ids, _) = assemble_openapi("T", &[], &ops);
    assert_eq!(ids, vec!["_internal", "get__pets"]);
    assert!(
        doc["paths"]["/a"]["get"]
            .get("x-original-operation-id")
            .is_none()
    );
}

#[test]
fn extract_llm_debug_omits_the_provider() {
    let provider = Arc::new(ScriptedProvider::new(vec![Ok("sk-sentinel-answer".into())]));
    let debug = format!("{:?}", llm(provider, 512));
    assert!(debug.starts_with("ExtractLlm {"), "{debug}");
    assert!(debug.contains("max_tokens: 512"), "{debug}");
    assert!(!debug.contains("sk-sentinel"), "{debug}");
}

#[test]
fn import_kind_and_phase_display_their_wire_names() {
    for kind in [
        ImportKind::Markdown,
        ImportKind::Html,
        ImportKind::Pdf,
        ImportKind::Text,
        ImportKind::Har,
        ImportKind::Postman,
    ] {
        assert_eq!(kind.to_string(), kind.as_str());
        assert_eq!(kind.as_str().parse::<ImportKind>(), Ok(kind));
        assert_eq!(ImportKind::parse(kind.as_str()), Some(kind));
    }
    let err = "docx".parse::<ImportKind>().unwrap_err();
    assert_eq!(err.0, "docx");
    assert!(err.to_string().contains("\"docx\""), "{err}");
    assert_eq!(Phase::Summarizing.to_string(), "summarizing");
    assert_eq!(format!("{NoopObserver:?}"), "NoopObserver");
}
