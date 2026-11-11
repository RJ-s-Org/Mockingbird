//! A read whose operation declares a `language` (or `lang`, `locale`) query parameter
//! asks for the same record in that language. A record a read generated answers its own
//! language; a read in another one is generated once and kept, so each language reads
//! the same every time and none replaces the stored record. A record a write stored
//! answers every language.
#![cfg(feature = "http-rest")]

use serde_json::json;

use crate::support::{self, HarnessOptions};

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Shows, version: "1"}
paths:
  /tv:
    post:
      operationId: createShow
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/Show"}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Show"}}}}
  /tv/{tv_id}:
    patch:
      operationId: updateShow
      parameters:
        - {name: tv_id, in: path, required: true, schema: {type: integer}}
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {name: {type: string}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Show"}}}}
    get:
      operationId: getShow
      parameters:
        - {name: tv_id, in: path, required: true, schema: {type: integer}}
        - {name: language, in: query, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Show"}}}}
components:
  schemas:
    Show:
      type: object
      required: [id, name]
      properties:
        id: {type: integer}
        name: {type: string}
"##;

/// Without the response cache, so each answer comes from state or a generation.
fn options() -> HarnessOptions {
    HarnessOptions::default()
}

#[tokio::test]
async fn each_language_reads_its_own_stable_variant() {
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": 549, "name": "Law & Order"}),
            json!({"id": 549, "name": "New York Police Judiciaire"}),
            json!({"id": 549, "name": "a third answer"}),
        ],
        &options(),
        |rt| rt,
    );
    let name = |body: serde_json::Value| body["name"].as_str().unwrap_or_default().to_string();
    let (_, _, first) = h.send("GET", "/tv/549", None, &[]).await;
    assert_eq!(name(first), "Law & Order");
    let (_, _, french) = h.send("GET", "/tv/549?language=fr-FR", None, &[]).await;
    assert_eq!(name(french), "New York Police Judiciaire");
    // Each language again: the same, with no further generation.
    let (_, _, french) = h.send("GET", "/tv/549?language=fr-FR", None, &[]).await;
    assert_eq!(name(french), "New York Police Judiciaire");
    let (_, _, again) = h.send("GET", "/tv/549", None, &[]).await;
    assert_eq!(name(again), "Law & Order");
}

#[tokio::test]
async fn a_written_record_answers_every_language() {
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": 7, "name": "Written"}),
            json!({"id": 7, "name": "a generated variant"}),
        ],
        &options(),
        |rt| rt,
    );
    let (status, _, _) = h
        .send(
            "POST",
            "/tv",
            Some(json!({"id": 7, "name": "Written"})),
            &[],
        )
        .await;
    assert!(status.is_success());
    let (_, _, french) = h.send("GET", "/tv/7?language=fr-FR", None, &[]).await;
    assert_eq!(french["name"], "Written");
}

#[tokio::test]
async fn a_write_ends_the_kept_variants() {
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": 549, "name": "Law & Order"}),
            json!({"id": 549, "name": "New York Police Judiciaire"}),
            json!({"id": 549, "name": "Renamed"}),
            json!({"id": 549, "name": "a generated variant"}),
        ],
        &options(),
        |rt| rt,
    );
    h.send("GET", "/tv/549", None, &[]).await;
    let (_, _, french) = h.send("GET", "/tv/549?language=fr-FR", None, &[]).await;
    assert_eq!(french["name"], "New York Police Judiciaire");
    let (status, _, _) = h
        .send("PATCH", "/tv/549", Some(json!({"name": "Renamed"})), &[])
        .await;
    assert!(status.is_success());
    let (_, _, french) = h.send("GET", "/tv/549?language=fr-FR", None, &[]).await;
    assert_eq!(french["name"], "Renamed");
}

/// The user message of each LLM call so far.
fn prompts(h: &support::DescribedHarness) -> Vec<String> {
    h.provider
        .requests()
        .iter()
        .map(|r| r.messages.last().unwrap().content.clone())
        .collect()
}

const LINKED: &str = r##"
openapi: 3.0.0
info: {title: Shows, version: "1"}
paths:
  /genre/tv/list:
    get:
      operationId: listGenres
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  genres: {type: array, items: {type: object, properties: {id: {type: integer}, name: {type: string}}}}
  /network/{network_id}:
    get:
      operationId: getNetwork
      parameters:
        - {name: network_id, in: path, required: true, schema: {type: integer}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Network"}}}}
  /tv/{tv_id}:
    get:
      operationId: getShow
      parameters:
        - {name: tv_id, in: path, required: true, schema: {type: integer}}
        - {name: language, in: query, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Show"}}}}
components:
  schemas:
    Network:
      type: object
      required: [id, name]
      properties:
        id: {type: integer}
        name: {type: string}
    Show:
      type: object
      required: [id, name]
      properties:
        id: {type: integer}
        name: {type: string}
        overview: {type: string}
        network_id: {type: integer}
"##;

/// A read in another language restates the stored record: its prompt holds that record
/// and the language asked for, not the session's other records and earlier answers.
#[tokio::test]
async fn a_variant_read_is_shown_only_its_stored_record() {
    let h = support::harness(
        LINKED,
        vec![
            json!({"genres": [{"id": 18, "name": "Drama"}, {"id": 80, "name": "Crime"}]}),
            json!({"id": 6, "name": "Studio Seventy-Seven"}),
            json!({"id": 549, "name": "Law & Order", "overview": "Detectives and prosecutors.",
                   "network_id": 6}),
            json!({"id": 549, "name": "New York Police Judiciaire",
                   "overview": "Inspecteurs et procureurs.", "network_id": 6}),
        ],
        &options(),
        |rt| rt,
    );
    h.send("GET", "/genre/tv/list", None, &[]).await;
    h.send("GET", "/network/6", None, &[]).await;
    h.send("GET", "/tv/549", None, &[]).await;
    let (_, _, french) = h.send("GET", "/tv/549?language=fr-FR", None, &[]).await;
    assert_eq!(french["name"], "New York Police Judiciaire");
    let prompts = prompts(&h);
    assert_eq!(prompts.len(), 4);
    let variant = &prompts[3];
    assert!(variant.contains("fr-FR"), "{variant}");
    assert!(variant.contains("Detectives and prosecutors."), "{variant}");
    assert!(!variant.contains("Studio Seventy-Seven"), "{variant}");
    assert!(!variant.contains("Drama"), "{variant}");
}
