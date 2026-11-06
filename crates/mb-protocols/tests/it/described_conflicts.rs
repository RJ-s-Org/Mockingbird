//! Unique members, concurrency tokens and idempotency from a world description: a
//! described unique set refuses a create or update repeating another record's values
//! before any LLM call; a described token refuses a stale write from the record's first
//! write and advances after an update; a described idempotency header answers a repeat
//! of a key as the first call was answered. Without a description, today's heuristics
//! answer as before.
#![cfg(test)]
#![cfg(feature = "http-rest")]

use std::collections::HashMap;

use axum::http::StatusCode;
use mb_protocols::world_gaps::GapCode;
use serde_json::{Value, json};

use crate::support::{self, DescribedHarness, HarnessOptions};

/// Folders named within a parent, with an `etag`; notes with an integer `version`.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Folders, version: "1"}
paths:
  /folders:
    post:
      operationId: createFolder
      parameters:
        - {name: Idempotency-Key, in: header, schema: {type: string}}
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/FolderInput"}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}
        "409": {description: taken, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
  /folders/{folderId}:
    parameters: [{name: folderId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getFolder
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
    patch:
      operationId: updateFolder
      parameters: [{name: If-Match, in: header, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/FolderInput"}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}
        "409": {description: taken, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
        "412": {description: stale, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
  /notes:
    post:
      operationId: createNote
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/NoteInput"}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
  /notes/{noteId}:
    parameters: [{name: noteId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getNote
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
    patch:
      operationId: updateNote
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/NoteInput"}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
        "409": {description: stale, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
components:
  schemas:
    Folder:
      type: object
      required: [id]
      properties:
        id: {type: string}
        displayName: {type: string}
        parent: {type: string}
        etag: {type: string, readOnly: true}
    FolderInput:
      type: object
      properties: {displayName: {type: string}, parent: {type: string}}
    Note:
      type: object
      required: [id]
      properties:
        id: {type: string}
        title: {type: string}
        version: {type: integer, readOnly: true}
    NoteInput:
      type: object
      properties: {title: {type: string}, version: {type: integer}}
    Problem:
      type: object
      properties: {message: {type: string}}
"##;

/// Templates created under an id the client chooses, with a request reference the name
/// rule does not take for an idempotency key.
const TEMPLATES: &str = r##"
openapi: 3.0.0
info: {title: Templates, version: "1"}
paths:
  /templates:
    post:
      operationId: createTemplate
      parameters:
        - {name: X-Dedupe-Ref, in: header, schema: {type: string}}
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {id: {type: string}, title: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}
        "400": {description: The request is invalid., content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
  /templates/{templateId}:
    get:
      operationId: getTemplate
      parameters: [{name: templateId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}
components:
  schemas:
    Template:
      type: object
      required: [id]
      properties: {id: {type: string}, title: {type: string}}
    Problem:
      type: object
      properties: {message: {type: string}}
"##;

fn folder(id: &str, name: &str, parent: &str) -> Value {
    json!({"id": id, "displayName": name, "parent": parent, "etag": "e-1"})
}

fn unique() -> Value {
    json!({"types": {"folders": {"unique": [{"members": ["displayName"], "scope": "parent", "conflict": 409}]}}})
}

fn calls(h: &DescribedHarness) -> usize {
    h.provider.requests().len()
}

fn prompt(h: &DescribedHarness, n: usize) -> String {
    let requests = h.provider.requests();
    requests[n]
        .messages
        .last()
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

async fn create_folder(h: &DescribedHarness, name: &str, parent: &str) -> (StatusCode, Value) {
    let (status, _, body) = h
        .send(
            "POST",
            "/folders",
            Some(json!({"displayName": name, "parent": parent})),
            &[],
        )
        .await;
    (status, body)
}

#[tokio::test]
async fn a_taken_unique_set_is_refused_before_generation() {
    let h = support::described(CONTRACT, unique(), vec![folder("f1", "Docs", "p1")]);
    let (status, _) = create_folder(&h, "Docs", "p1").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = create_folder(&h, "Docs", "p1").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(calls(&h), 1);
}

#[tokio::test]
async fn the_same_values_under_another_parent_are_accepted() {
    let h = support::described(
        CONTRACT,
        unique(),
        vec![folder("f1", "Docs", "p1"), folder("f2", "Docs", "p2")],
    );
    create_folder(&h, "Docs", "p1").await;
    let (status, body) = create_folder(&h, "Docs", "p2").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(calls(&h), 2);
    assert!(h.stored("folders", "f2").await.is_some());
}

#[tokio::test]
async fn an_update_onto_another_records_values_is_refused() {
    let h = support::described(
        CONTRACT,
        unique(),
        vec![
            folder("f1", "Docs", "p1"),
            folder("f2", "Notes", "p1"),
            folder("f1", "Docs", "p1"),
        ],
    );
    create_folder(&h, "Docs", "p1").await;
    create_folder(&h, "Notes", "p1").await;
    let (status, _, _) = h
        .send(
            "PATCH",
            "/folders/f2",
            Some(json!({"displayName": "Docs"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(calls(&h), 2);
    assert_eq!(
        h.stored("folders", "f2")
            .await
            .map(|r| r["displayName"].clone()),
        Some(json!("Notes"))
    );
    // A record keeping its own values is no conflict.
    let (status, _, _) = h
        .send(
            "PATCH",
            "/folders/f1",
            Some(json!({"displayName": "Docs"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(calls(&h), 3);
}

fn etag_world() -> Value {
    json!({"types": {"folders": {"concurrency": {"member": "etag", "header": "If-Match", "conflict": 412}}}})
}

#[tokio::test]
async fn a_stale_if_match_is_refused_on_the_first_write() {
    let h = support::described(CONTRACT, etag_world(), vec![folder("f1", "Docs", "p1")]);
    create_folder(&h, "Docs", "p1").await;
    let (status, _, _) = h
        .send(
            "PATCH",
            "/folders/f1",
            Some(json!({"displayName": "Other"})),
            &[("If-Match", "\"e-9\"")],
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(calls(&h), 1);
    assert_eq!(
        h.stored("folders", "f1")
            .await
            .map(|r| r["displayName"].clone()),
        Some(json!("Docs"))
    );
}

#[tokio::test]
async fn a_described_token_advances_after_an_update() {
    // A string entity tag the model leaves as it was is minted anew.
    let h = support::described(
        CONTRACT,
        etag_world(),
        vec![folder("f1", "Docs", "p1"), folder("f1", "Renamed", "p1")],
    );
    create_folder(&h, "Docs", "p1").await;
    let (status, _, body) = h
        .send(
            "PATCH",
            "/folders/f1",
            Some(json!({"displayName": "Renamed"})),
            &[("If-Match", "\"e-1\"")],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let etag = h.stored("folders", "f1").await.map(|r| r["etag"].clone());
    assert_eq!(etag, Some(json!("e-2")));
    assert_eq!(body["etag"], "e-2");
    let (status, _, _) = h
        .send(
            "PATCH",
            "/folders/f1",
            Some(json!({"displayName": "Again"})),
            &[("If-Match", "\"e-1\"")],
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(calls(&h), 2);

    // An integer version in the body, left as it was by the model, goes one up.
    let h = support::described(
        CONTRACT,
        json!({"types": {"notes": {"concurrency": {"member": "version", "conflict": 409}}}}),
        vec![
            json!({"id": "n1", "title": "a", "version": 1}),
            json!({"id": "n1", "title": "b", "version": 1}),
        ],
    );
    h.send("POST", "/notes", Some(json!({"title": "a"})), &[])
        .await;
    let (status, _, _) = h
        .send(
            "PATCH",
            "/notes/n1",
            Some(json!({"title": "b", "version": 1})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        h.stored("notes", "n1").await.map(|r| r["version"].clone()),
        Some(json!(2))
    );
    let (status, _, _) = h
        .send(
            "PATCH",
            "/notes/n1",
            Some(json!({"title": "c", "version": 1})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(calls(&h), 2);
}

#[tokio::test]
async fn if_match_star_and_no_token_pass() {
    let h = support::described(
        CONTRACT,
        etag_world(),
        vec![
            folder("f1", "Docs", "p1"),
            folder("f1", "Star", "p1"),
            folder("f1", "None", "p1"),
        ],
    );
    create_folder(&h, "Docs", "p1").await;
    let (status, _, _) = h
        .send(
            "PATCH",
            "/folders/f1",
            Some(json!({"displayName": "Star"})),
            &[("If-Match", "*")],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = h
        .send(
            "PATCH",
            "/folders/f1",
            Some(json!({"displayName": "None"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(calls(&h), 3);
}

/// `extra`'s top-level keys, with `createFolder`'s idempotency header described.
fn idempotent(extra: Value) -> Value {
    let mut world =
        json!({"operations": {"createFolder": {"idempotency": {"header": "Idempotency-Key"}}}});
    if let (Some(w), Some(e)) = (world.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            w.insert(k.clone(), v.clone());
        }
    }
    world
}

#[tokio::test]
async fn an_idempotent_replay_answers_as_the_first() {
    let gaps: HashMap<String, Vec<GapCode>> =
        HashMap::from([("createFolder".to_owned(), vec![GapCode::NoTarget])]);
    let h = support::described_with_gaps(
        CONTRACT,
        idempotent(json!({})),
        vec![folder("f1", "Docs", "p1"), folder("f2", "Other", "p1")],
        gaps,
    );
    let key = [("Idempotency-Key", "k-1")];
    let (s1, _, b1) = h
        .send(
            "POST",
            "/folders",
            Some(json!({"displayName": "Docs", "parent": "p1"})),
            &key,
        )
        .await;
    // A replay with another body still answers the first call's answer.
    let (s2, _, b2) = h
        .send(
            "POST",
            "/folders",
            Some(json!({"displayName": "Other", "parent": "p1"})),
            &key,
        )
        .await;
    assert_eq!(s1, StatusCode::CREATED);
    assert_eq!((s1, &b1), (s2, &b2));
    assert_eq!(calls(&h), 1);
    assert!(h.stored("folders", "f2").await.is_none());
    let records = h.records();
    assert_eq!(records[1].request["world_gap"], json!(["no_target"]));
    // Another key is a new request.
    let (s3, _, b3) = h
        .send(
            "POST",
            "/folders",
            Some(json!({"displayName": "Other", "parent": "p1"})),
            &[("Idempotency-Key", "k-2")],
        )
        .await;
    assert_eq!(s3, StatusCode::CREATED);
    assert_eq!(b3["id"], "f2");
    assert_eq!(calls(&h), 2);
}

#[tokio::test]
async fn a_new_key_with_a_taken_unique_set_is_refused() {
    let h = support::described(
        CONTRACT,
        idempotent(unique()),
        vec![folder("f1", "Docs", "p1")],
    );
    let body = json!({"displayName": "Docs", "parent": "p1"});
    h.send(
        "POST",
        "/folders",
        Some(body.clone()),
        &[("Idempotency-Key", "k-1")],
    )
    .await;
    let (status, _, _) = h
        .send(
            "POST",
            "/folders",
            Some(body),
            &[("Idempotency-Key", "k-2")],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(calls(&h), 1);
}

/// Two creates of template `tp1`, each with its own request reference.
async fn create_template_twice(h: &DescribedHarness) {
    for token in ["t-1", "t-2"] {
        h.send(
            "POST",
            "/templates",
            Some(json!({"id": "tp1", "title": "First"})),
            &[("X-Dedupe-Ref", token)],
        )
        .await;
    }
}

#[tokio::test]
async fn a_described_key_header_the_name_rule_misses_skips_repeat_detection() {
    let template = |title: &str| json!({"id": "tp1", "title": title});
    let script = || vec![template("First"), template("Second")];
    // Undescribed, the reference is no idempotency key: the repeat is offered a refusal.
    let plain = support::described(TEMPLATES, json!({}), script());
    create_template_twice(&plain).await;
    assert!(prompt(&plain, 1).contains("refus"), "{}", prompt(&plain, 1));
    // Described, a repeat may be a retry: it is offered none.
    let described = support::described(
        TEMPLATES,
        json!({"operations": {"createTemplate": {"idempotency": {"header": "X-Dedupe-Ref"}}}}),
        script(),
    );
    create_template_twice(&described).await;
    assert!(
        !prompt(&described, 1).contains("refus"),
        "{}",
        prompt(&described, 1)
    );
}

#[tokio::test]
async fn a_replayed_refusal_is_refused_again() {
    let h = support::described(
        CONTRACT,
        idempotent(unique()),
        vec![folder("f1", "Docs", "p1"), folder("f1", "Renamed", "p1")],
    );
    let body = json!({"displayName": "Docs", "parent": "p1"});
    let k2 = [("Idempotency-Key", "k-2")];
    h.send(
        "POST",
        "/folders",
        Some(body.clone()),
        &[("Idempotency-Key", "k-1")],
    )
    .await;
    let (status, _, refused) = h.send("POST", "/folders", Some(body.clone()), &k2).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // The name is free again, but a retry of the refused key is refused as before.
    let (status, _, _) = h
        .send(
            "PATCH",
            "/folders/f1",
            Some(json!({"displayName": "Renamed"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, again) = h.send("POST", "/folders", Some(body), &k2).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refused, again);
    assert_eq!(calls(&h), 2);
}

#[tokio::test]
async fn an_undescribed_idempotency_key_behaves_as_today() {
    // No world at all: a repeated key is generated again, as a retry the model answers.
    let h = support::harness(
        CONTRACT,
        vec![folder("f1", "Docs", "p1"), folder("f2", "Docs", "p1")],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let key = [("Idempotency-Key", "k-1")];
    let body = json!({"displayName": "Docs", "parent": "p1"});
    h.send("POST", "/folders", Some(body.clone()), &key).await;
    let (status, _, answer) = h.send("POST", "/folders", Some(body), &key).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(answer["id"], "f2");
    assert_eq!(calls(&h), 2);
}

#[tokio::test]
async fn a_heuristic_idempotency_key_does_not_replay_under_tier0() {
    let script = || vec![folder("f1", "Docs", "p1"), folder("f2", "Docs", "p1")];
    let plain = support::harness(CONTRACT, script(), &HarnessOptions::default(), |rt| rt);
    let tier0 = support::described(CONTRACT, json!({}), script());
    let key = [("Idempotency-Key", "k-1")];
    let body = json!({"displayName": "Docs", "parent": "p1"});
    for _ in 0..2 {
        let a = plain
            .send("POST", "/folders", Some(body.clone()), &key)
            .await;
        let b = tier0
            .send("POST", "/folders", Some(body.clone()), &key)
            .await;
        assert_eq!((a.0, &a.2), (b.0, &b.2));
    }
    assert_eq!(calls(&plain), 2);
    assert_eq!(calls(&tier0), 2);
    for id in ["f1", "f2"] {
        assert!(tier0.stored("folders", id).await.is_some(), "{id}");
        assert_eq!(
            plain.stored("folders", id).await,
            tier0.stored("folders", id).await
        );
    }
}
