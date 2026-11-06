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

#[tokio::test]
async fn a_root_create_and_a_scoped_one_with_the_same_values_are_accepted() {
    // A flat collection whose scope member is in the body: no scope value is another
    // scope than any value, both ways.
    let root = |id: &str| json!({"id": id, "displayName": "Docs", "etag": "e-1"});
    let h = support::described(
        CONTRACT,
        unique(),
        vec![
            folder("f1", "Docs", "p1"),
            root("f2"),
            folder("f3", "Docs", "p2"),
        ],
    );
    let (status, _) = create_folder(&h, "Docs", "p1").await;
    assert_eq!(status, StatusCode::CREATED);
    let send_root = || {
        h.send(
            "POST",
            "/folders",
            Some(json!({"displayName": "Docs"})),
            &[],
        )
    };
    let (status, _, body) = send_root().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = create_folder(&h, "Docs", "p2").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(calls(&h), 3);
    for id in ["f1", "f2", "f3"] {
        assert!(h.stored("folders", id).await.is_some(), "{id}");
    }
    // Two records with no scope value share it.
    let (status, _, _) = send_root().await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(calls(&h), 3);
}

/// Pages whose `stamp` is a date-time and whose `rev` is at most three characters.
const PAGES: &str = r##"
openapi: 3.0.0
info: {title: Pages, version: "1"}
paths:
  /pages:
    post:
      operationId: createPage
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/PageInput"}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Page"}}}}
  /pages/{pageId}:
    parameters: [{name: pageId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getPage
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Page"}}}}}
    patch:
      operationId: updatePage
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/PageInput"}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Page"}}}}
        "409": {description: stale, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
components:
  schemas:
    Page:
      type: object
      required: [id]
      properties:
        id: {type: string}
        title: {type: string}
        stamp: {type: string, format: date-time, readOnly: true}
        rev: {type: string, maxLength: 3, readOnly: true}
    PageInput:
      type: object
      properties: {title: {type: string}, stamp: {type: string}, rev: {type: string}}
    Problem:
      type: object
      properties: {message: {type: string}}
"##;

#[tokio::test]
async fn a_described_token_is_not_minted_past_its_schema() {
    const STAMP: &str = "2024-01-01T00:00:00Z";
    for (member, held) in [("stamp", STAMP), ("rev", "e-9")] {
        let page = |title: &str| json!({"id": "g1", "title": title, member: held});
        let h = support::described(
            PAGES,
            json!({"types": {"pages": {"concurrency": {"member": member, "conflict": 409}}}}),
            vec![page("a"), page("b")],
        );
        h.send("POST", "/pages", Some(json!({"title": "a"})), &[])
            .await;
        let (status, _, body) = h
            .send(
                "PATCH",
                "/pages/g1",
                Some(json!({"title": "b", member: held})),
                &[],
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{member}: {body}");
        // The model's value stays: a minted one would not be a date-time, or would be
        // four characters long.
        assert_eq!(body[member], held, "{member}");
        assert_eq!(
            h.stored("pages", "g1").await.map(|r| r[member].clone()),
            Some(json!(held)),
            "{member}"
        );
        assert_eq!(calls(&h), 2);
    }
}

/// Aliases unique by `alias` (with `slug`, unique on its own), refused with
/// `{ErrorCode, Message}`: `create` declares `codes` for its refusals, and the update
/// declares `409` and `412`, so a set's `409` validates whatever the create declares.
fn aliases(codes: &[u16]) -> String {
    let refusals: String = codes
        .iter()
        .map(|c| {
            format!(
                "        \"{c}\": {{description: refused, content: {{application/json: {{schema: {{$ref: \"#/components/schemas/Error\"}}}}}}}}\n"
            )
        })
        .collect();
    format!(
        r##"
openapi: 3.0.0
info: {{title: Aliases, version: "1"}}
paths:
  /aliases:
    post:
      operationId: createAlias
      parameters:
        - {{name: Idempotency-Key, in: header, schema: {{type: string}}}}
      requestBody: {{content: {{application/json: {{schema: {{$ref: "#/components/schemas/AliasInput"}}}}}}}}
      responses:
        "201": {{description: created, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Alias"}}}}}}}}
{refusals}  /aliases/{{aliasId}}:
    parameters: [{{name: aliasId, in: path, required: true, schema: {{type: string}}}}]
    get:
      operationId: getAlias
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Alias"}}}}}}}}}}
    patch:
      operationId: updateAlias
      parameters: [{{name: If-Match, in: header, schema: {{type: string}}}}]
      requestBody: {{content: {{application/json: {{schema: {{$ref: "#/components/schemas/AliasInput"}}}}}}}}
      responses:
        "200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Alias"}}}}}}}}
        "403": {{description: refused, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Error"}}}}}}}}
        "409": {{description: taken, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Error"}}}}}}}}
        "412": {{description: stale, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Error"}}}}}}}}
components:
  schemas:
    Alias:
      type: object
      required: [id]
      properties:
        id: {{type: string}}
        alias: {{type: string}}
        slug: {{type: string}}
        etag: {{type: string, readOnly: true}}
    AliasInput:
      type: object
      properties: {{alias: {{type: string}}, slug: {{type: string}}}}
    Error:
      type: object
      required: [ErrorCode, Message]
      properties: {{ErrorCode: {{type: integer}}, Message: {{type: string}}}}
"##
    )
}

fn alias(id: &str, name: &str) -> Value {
    json!({"id": id, "alias": name, "slug": name, "etag": "e-1"})
}

fn unique_alias() -> Value {
    json!({"types": {"aliases": {"unique": [{"members": ["alias"], "conflict": 409}]}}})
}

async fn create_alias(
    h: &DescribedHarness,
    name: &str,
    key: &[(&str, &str)],
) -> (StatusCode, Value) {
    let (status, _, body) = h
        .send(
            "POST",
            "/aliases",
            Some(json!({"alias": name, "slug": name})),
            key,
        )
        .await;
    (status, body)
}

#[tokio::test]
async fn a_taken_unique_value_answers_the_models_written_body_naming_it() {
    let written = json!({"ErrorCode": 1122, "Message": "The alias is already in use"});
    let h = support::described(
        &aliases(&[409]),
        unique_alias(),
        vec![alias("a1", "welcome-1"), written],
    );
    create_alias(&h, "welcome-1", &[]).await;
    for _ in 0..2 {
        let (status, body) = create_alias(&h, "welcome-1", &[]).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["ErrorCode"], 1122, "{body}");
        let message = body["Message"].as_str().unwrap_or_default();
        assert!(
            message.contains("alias") && message.contains("welcome-1"),
            "{message}"
        );
    }
    // One create, one written body: the second refusal asks nothing.
    assert_eq!(calls(&h), 2);
    let records = h.records();
    assert_eq!(records[1].source.as_deref(), Some("state"));
    assert_eq!(records[2].source.as_deref(), Some("state"));
}

#[tokio::test]
async fn a_stale_token_answers_its_own_written_body() {
    let world = json!({"types": {"aliases": {
        "unique": [{"members": ["alias"], "conflict": 409}],
        "concurrency": {"member": "etag", "header": "If-Match", "conflict": 412}
    }}});
    let h = support::described(
        &aliases(&[409]),
        world,
        vec![
            alias("a1", "welcome-1"),
            json!({"ErrorCode": 1122, "Message": "taken"}),
            json!({"ErrorCode": 3001, "Message": "stale"}),
        ],
    );
    create_alias(&h, "welcome-1", &[]).await;
    let (status, taken) = create_alias(&h, "welcome-1", &[]).await;
    assert_eq!(
        (status, &taken["ErrorCode"]),
        (StatusCode::CONFLICT, &json!(1122))
    );
    let (status, _, stale) = h
        .send(
            "PATCH",
            "/aliases/a1",
            Some(json!({"slug": "other"})),
            &[("If-Match", "\"e-9\"")],
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{stale}");
    assert_eq!(stale["ErrorCode"], 3001, "{stale}");
    assert_eq!(calls(&h), 3);
}

#[tokio::test]
async fn a_set_whose_status_the_create_does_not_declare_takes_its_declared_refusal() {
    // #301: the set names 409, the create declares only 422.
    let h = support::described(
        &aliases(&[422]),
        unique_alias(),
        vec![alias("a1", "welcome-1")],
    );
    create_alias(&h, "welcome-1", &[]).await;
    let (status, body) = create_alias(&h, "welcome-1", &[]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(calls(&h), 1 + 1, "the create, then the 422's body");
}

#[tokio::test]
async fn a_set_the_create_cannot_refuse_is_skipped_for_a_later_one() {
    // The first set's 409 is not the create's, and it declares no refusal of its own:
    // the second set, refused with the 403 it declares, still refuses.
    let world = json!({"types": {"aliases": {"unique": [
        {"members": ["alias"], "conflict": 409},
        {"members": ["slug"], "conflict": 403}
    ]}}});
    let h = support::described(&aliases(&[403]), world, vec![alias("a1", "welcome-1")]);
    create_alias(&h, "welcome-1", &[]).await;
    let (status, body) = create_alias(&h, "welcome-1", &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let message = body["Message"].as_str().unwrap_or_default();
    assert!(message.contains("slug"), "{message}");
}

#[tokio::test]
async fn a_refusal_with_a_written_body_is_replayed_for_its_key() {
    // #300: a retry of the refused key answers the refusal as it was, asking nothing.
    let world = json!({
        "operations": {"createAlias": {"idempotency": {"header": "Idempotency-Key"}}},
        "types": {"aliases": {"unique": [{"members": ["alias"], "conflict": 409}]}}
    });
    let h = support::described(
        &aliases(&[409]),
        world,
        vec![
            alias("a1", "welcome-1"),
            json!({"ErrorCode": 1122, "Message": "taken"}),
            alias("a1", "renamed"),
        ],
    );
    create_alias(&h, "welcome-1", &[("Idempotency-Key", "k-1")]).await;
    let k2 = [("Idempotency-Key", "k-2")];
    let (status, refused) = create_alias(&h, "welcome-1", &k2).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _, _) = h
        .send(
            "PATCH",
            "/aliases/a1",
            Some(json!({"alias": "renamed", "slug": "renamed"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, again) = create_alias(&h, "welcome-1", &k2).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refused, again);
    assert_eq!(calls(&h), 3);
}
