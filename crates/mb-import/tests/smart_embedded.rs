//! Operations taken from OpenAPI blocks embedded in documentation pages.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mb_import::smart::embedded::extract_embedded;
use mb_import::smart::{
    ExtractLlm, ImportInput, ImportKind, ImportOptions, NoopObserver, run_import,
};
use mb_llm::{HopSettings, ScriptedProvider};
use serde_json::json;

fn fixture(rel: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel);
    std::fs::read(p).unwrap()
}

fn md(name: &str) -> ImportInput {
    ImportInput::Bytes {
        kind: ImportKind::Markdown,
        name: Some(name.to_string()),
        bytes: fixture(&format!("smart/{name}")),
    }
}

fn llm(provider: Arc<ScriptedProvider>) -> ExtractLlm {
    ExtractLlm {
        provider,
        settings: HopSettings {
            timeout: Duration::from_secs(5),
            temperature: 0.1,
            max_tokens: 1000,
        },
    }
}

#[tokio::test]
async fn readme_page_imports_every_embedded_operation_without_llm() {
    let input = ImportInput::Bytes {
        kind: ImportKind::Html,
        name: Some("details.html".into()),
        bytes: fixture("smart/readme-oas.html"),
    };
    let draft = run_import(input, None, &ImportOptions::default(), &NoopObserver)
        .await
        .unwrap();
    assert_eq!(draft.meta.chunk_count, 0);
    let ids: Vec<&str> = draft.meta.operations.keys().map(String::as_str).collect();
    assert_eq!(ids, vec!["movie_credits", "movie_details"]);
    assert_eq!(draft.meta.operations["movie_details"].confidence, 1.0);
    assert_eq!(
        draft.meta.operations["movie_details"].provenance[0].location,
        "embedded OpenAPI"
    );
    assert_eq!(
        draft.document["servers"][0]["url"],
        "https://api.themoviedb.org"
    );
    assert!(draft.document["components"]["schemas"]["Movie"].is_object());
}

#[tokio::test]
async fn saved_tmdb_readme_page_imports_movie_details_without_llm() {
    let input = ImportInput::Bytes {
        kind: ImportKind::Html,
        name: Some("tmdb-readme.html".into()),
        bytes: fixture("smart/tmdb-readme.html"),
    };
    let draft = run_import(input, None, &ImportOptions::default(), &NoopObserver)
        .await
        .unwrap();
    assert_eq!(draft.meta.chunk_count, 0);
    assert!(draft.meta.operations.contains_key("movie_details"));
    let get = &draft.document["paths"]["/3/movie/{movie_id}"]["get"];
    assert_eq!(get["x-original-operation-id"], "movie-details");
}

#[tokio::test]
async fn mixed_page_block_wins_and_llm_fills_the_rest() {
    // The LLM answers GET /pets and a (worse) POST /pets.
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(json!({"operations": [
        {"method": "GET", "path": "/pets", "confidence": 0.9, "responses": [{"status": 200, "example": []}]},
        {"method": "POST", "path": "/pets", "confidence": 0.9, "responses": [{"status": 200}]}]})
    .to_string())]));
    let draft = run_import(
        md("embedded-oas.md"),
        Some(&llm(provider.clone())),
        &ImportOptions::default(),
        &NoopObserver,
    )
    .await
    .unwrap();
    let user = &provider.requests()[0].messages[1].content;
    assert!(
        !user.contains("openapi: 3.0.0"),
        "the block reached the prompt"
    );
    let post = &draft.document["paths"]["/pets"]["post"];
    assert_eq!(post["operationId"], "createPet");
    assert!(post["responses"]["201"].is_object());
    assert!(draft.document["paths"]["/pets"]["get"].is_object());
}

#[tokio::test]
async fn broken_embedded_block_is_skipped_not_fatal() {
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(
        r#"{"operations": []}"#.into()
    )]));
    let draft = run_import(
        md("embedded-broken.md"),
        Some(&llm(provider)),
        &ImportOptions::default(),
        &NoopObserver,
    )
    .await
    .unwrap();
    assert!(draft.document["paths"]["/b"]["get"].is_object());
    assert!(draft.document["paths"].get("/a").is_none());
    assert!(
        draft
            .meta
            .warnings
            .iter()
            .any(|w| w.code == "invalid_operation" && w.operation.as_deref() == Some("GET /a")),
        "{:?}",
        draft.meta.warnings
    );
}

#[test]
fn swagger2_block_is_left_to_the_llm() {
    let md = "# API\n\n```json\n{\"swagger\": \"2.0\", \"info\": {\"title\": \"t\", \"version\": \"1\"}, \"paths\": {\"/x\": {\"get\": {\"responses\": {\"200\": {\"description\": \"ok\"}}}}}}\n```\n";
    let mut doc = mb_import::smart::text::extract_markdown(md);
    let e = extract_embedded(ImportKind::Markdown, md.as_bytes(), &mut doc);
    assert!(e.operations.is_empty());
    assert_eq!(e.warnings[0].code, "embedded_swagger2");
    assert!(doc.blocks.iter().any(|b| b.text.contains("swagger")));
}
