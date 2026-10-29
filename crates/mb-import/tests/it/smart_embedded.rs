//! Operations taken from OpenAPI blocks embedded in documentation pages.
#![cfg(test)]

use std::sync::Arc;

use mb_import::smart::embedded::extract_embedded;
use mb_import::smart::{ImportInput, ImportKind, ImportOptions, NoopObserver, run_import};
use mb_llm::ScriptedProvider;
use serde_json::json;

use crate::support::{fixture, llm};

fn md(name: &str) -> ImportInput {
    ImportInput::Bytes {
        kind: ImportKind::Markdown,
        name: Some(name.to_string()),
        bytes: fixture(&format!("smart/{name}")),
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

/// A Markdown page holding one Swagger 2.0 block.
const SWAGGER2_PAGE: &str = r##"# Pets API

```json
{
  "swagger": "2.0",
  "info": {"title": "Pets", "version": "1"},
  "host": "api.example.com",
  "basePath": "/v1",
  "schemes": ["https"],
  "produces": ["application/json"],
  "paths": {
    "/pets/{id}": {
      "parameters": [{"name": "id", "in": "path", "required": true, "type": "integer"}],
      "get": {
        "operationId": "getPet",
        "parameters": [{"name": "fields", "in": "query", "type": "array", "items": {"type": "string"}}],
        "responses": {
          "200": {"description": "ok", "schema": {"$ref": "#/definitions/Pet"},
                  "headers": {"X-Rate": {"type": "integer", "description": "left"}}},
          "404": {"$ref": "#/responses/NotFound"}
        }
      }
    },
    "/pets": {
      "post": {
        "operationId": "createPet",
        "consumes": ["application/json"],
        "parameters": [{"name": "body", "in": "body", "required": true, "schema": {"$ref": "#/definitions/Pet"}}],
        "responses": {"201": {"description": "created", "schema": {"$ref": "#/definitions/Pet"}}}
      }
    },
    "/pets/{id}/photo": {
      "put": {
        "operationId": "putPhoto",
        "consumes": ["multipart/form-data"],
        "parameters": [
          {"name": "id", "in": "path", "required": true, "type": "integer"},
          {"name": "file", "in": "formData", "required": true, "type": "file"},
          {"name": "caption", "in": "formData", "type": "string"}
        ],
        "responses": {"204": {"description": "stored"}}
      }
    }
  },
  "responses": {"NotFound": {"description": "missing", "schema": {"$ref": "#/definitions/Error"}}},
  "definitions": {
    "Pet": {"type": "object", "required": ["id"], "properties": {"id": {"type": "integer"}, "tag": {"$ref": "#/definitions/Tag"}}},
    "Tag": {"type": "string", "x-nullable": true},
    "Error": {"type": "object", "properties": {"message": {"type": "string"}}}
  }
}
```

Call `GET /v1/pets/{id}` to read a pet.
"##;

#[test]
fn swagger2_block_is_converted() {
    let mut doc = mb_import::smart::text::extract_markdown(SWAGGER2_PAGE);
    let e = extract_embedded(ImportKind::Markdown, SWAGGER2_PAGE.as_bytes(), &mut doc);
    assert!(e.warnings.is_empty(), "{:?}", e.warnings);
    assert!(!doc.blocks.iter().any(|b| b.text.contains("swagger")));
    let targets: Vec<String> = e.operations.iter().map(|o| o.target()).collect();
    assert_eq!(
        targets,
        ["POST /pets", "GET /pets/{id}", "PUT /pets/{id}/photo"]
    );
    assert_eq!(e.servers, ["https://api.example.com/v1"]);
    assert!(mb_import::smart::embedded::covers_endpoints(&e, &doc));

    let get = &e.operations[1].operation;
    let params = get["parameters"].as_array().unwrap();
    let id = params.iter().find(|p| p["name"] == "id").unwrap();
    assert_eq!(id["schema"], json!({"type": "integer"}));
    let fields = params.iter().find(|p| p["name"] == "fields").unwrap();
    assert_eq!(
        fields["schema"],
        json!({"type": "array", "items": {"type": "string"}})
    );
    assert_eq!(
        get["responses"]["200"]["content"]["application/json"]["schema"],
        json!({"$ref": "#/components/schemas/Pet"})
    );
    assert_eq!(
        get["responses"]["200"]["headers"]["X-Rate"],
        json!({"description": "left", "schema": {"type": "integer"}})
    );
    assert_eq!(
        get["responses"]["404"],
        json!({"$ref": "#/components/responses/NotFound"})
    );
    let post = &e.operations[0].operation;
    assert_eq!(
        post["requestBody"],
        json!({"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Pet"}}}})
    );
    let put = &e.operations[2].operation;
    assert_eq!(
        put["requestBody"]["content"]["multipart/form-data"]["schema"],
        json!({"type": "object", "properties": {
            "file": {"type": "string", "format": "binary"},
            "caption": {"type": "string"}
        }, "required": ["file"]})
    );
    assert_eq!(
        e.components["schemas"]["Pet"]["properties"]["tag"],
        json!({"$ref": "#/components/schemas/Tag"})
    );
    assert_eq!(
        e.components["schemas"]["Tag"],
        json!({"type": "string", "nullable": true})
    );
    assert!(e.components["responses"]["NotFound"].is_object());
}

#[tokio::test]
async fn swagger2_page_imports_without_llm() {
    let input = ImportInput::Bytes {
        kind: ImportKind::Markdown,
        name: Some("pets.md".into()),
        bytes: SWAGGER2_PAGE.as_bytes().to_vec(),
    };
    let draft = run_import(input, None, &ImportOptions::default(), &NoopObserver)
        .await
        .unwrap();
    assert_eq!(draft.meta.chunk_count, 0);
    let ids: Vec<&str> = draft.meta.operations.keys().map(String::as_str).collect();
    assert_eq!(ids, ["createPet", "getPet", "putPhoto"]);
    assert!(
        !draft
            .meta
            .warnings
            .iter()
            .any(|w| w.code == "invalid_operation"),
        "{:?}",
        draft.meta.warnings
    );
}
