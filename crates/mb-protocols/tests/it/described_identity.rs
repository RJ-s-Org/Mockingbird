//! Identity and record pointers from a world description: where a create's answer or
//! request holds its record or records, the header it names its id in, alternate
//! members a path value names a record by, and patterned ids read by either form.
//! Where the description is silent (no world, or tier 0), the heuristics answer as
//! before.
#![cfg(test)]
#![cfg(feature = "http-rest")]

use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use mb_protocols::MockRuntime;
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};

use crate::support::{self, DescribedHarness, HarnessOptions};

/// Conversations, threads, notes and tickets answer their ids in headers; folders and
/// labels answer the record; zones an envelope holding a one-item array; contacts are
/// put in bulk; a report's create is long-running; templates are read by their id.
/// Files are listed under their folder or project, and pages read under their space
/// by title.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Identity, version: "1"}
paths:
  /conversations:
    post:
      operationId: createConversation
      requestBody:
        content: {application/json: {schema: {type: object, properties: {subject: {type: string}, data: {type: object, properties: {subject: {type: string}}}}}}}
      responses:
        "201":
          description: created
          headers:
            Resource-ID: {schema: {type: string}}
            X-Conv: {schema: {type: string}}
          content: {application/json: {schema: {type: object, properties: {threadIds: {type: array, items: {type: integer}}}}}}
  /conversations/{conversationId}:
    get:
      operationId: getConversation
      parameters: [{name: conversationId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Conversation"}}}}}
  /threads:
    post:
      operationId: createThread
      requestBody: {content: {application/json: {schema: {type: object, properties: {subject: {type: string}}}}}}
      responses:
        "201":
          description: created
          headers:
            X-Thread: {schema: {type: string}}
          content: {application/json: {schema: {type: object, properties: {threadIds: {type: array, items: {type: integer}}}}}}
  /threads/{threadId}:
    get:
      operationId: getThread
      parameters: [{name: threadId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Conversation"}}}}}
  /notes:
    post:
      operationId: createNote
      requestBody: {content: {application/json: {schema: {type: object, properties: {subject: {type: string}}}}}}
      responses:
        "201":
          description: created
          headers:
            Resource-ID: {schema: {type: string}}
            Location: {schema: {type: string}}
  /notes/{noteId}:
    parameters: [{name: noteId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getNote
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
    patch:
      operationId: ackNote
      requestBody: {content: {application/json: {schema: {type: object, properties: {subject: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {ok: {type: boolean}}}}}}}
  /tickets:
    post:
      operationId: createTicket
      requestBody: {content: {application/json: {schema: {type: object, properties: {subject: {type: string}}}}}}
      responses:
        "201":
          description: created
          headers:
            Ticket-Id: {schema: {type: string, format: uuid}}
  /tickets/{ticketId}:
    get:
      operationId: getTicket
      parameters: [{name: ticketId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
  /folders:
    post:
      operationId: createFolder
      requestBody: {content: {application/json: {schema: {type: object, properties: {displayName: {type: string}, slug: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
  /folders/{folderId}:
    parameters: [{name: folderId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getFolder
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}, "404": {description: missing}}
    patch:
      operationId: updateFolder
      requestBody: {content: {application/json: {schema: {type: object, properties: {displayName: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
    delete:
      operationId: deleteFolder
      responses: {"204": {description: gone}}
  /folders/{folderId}/files:
    get:
      operationId: listFiles
      parameters: [{name: folderId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/File"}}}}}}
  /projects:
    post:
      operationId: createProject
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}, code: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}}
  /projects/{project}:
    get:
      operationId: getProject
      parameters: [{name: project, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}, "404": {description: missing}}
  /projects/{project}/files:
    get:
      operationId: listProjectFiles
      parameters: [{name: project, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/File"}}}}}}
  /spaces/{spaceKey}/pages:
    post:
      operationId: createPage
      parameters: [{name: spaceKey, in: path, required: true, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}, handle: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Page"}}}}}
  /spaces/{spaceKey}/pages/{title}:
    get:
      operationId: getPage
      parameters:
        - {name: spaceKey, in: path, required: true, schema: {type: string}}
        - {name: title, in: path, required: true, schema: {type: string}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Page"}}}}, "404": {description: missing}}
  /labels:
    post:
      operationId: createLabel
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Label"}}}}}
  /labels/{labelId}:
    get:
      operationId: getLabel
      parameters: [{name: labelId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Label"}}}}}
  /zones:
    post:
      operationId: createZone
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/ZoneEnvelope"}}}}}
  /zones/{zoneId}:
    get:
      operationId: getZone
      parameters: [{name: zoneId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/ZoneEnvelope"}}}}}
  /contacts:
    put:
      operationId: putContacts
      requestBody: {content: {application/json: {schema: {type: object, properties: {Contacts: {type: array, items: {type: object, properties: {Name: {type: string}}}}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {Contacts: {type: array, items: {$ref: "#/components/schemas/Contact"}}}}}}}}
  /contacts/{ContactID}:
    get:
      operationId: getContact
      parameters: [{name: ContactID, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}}}
  /reports:
    post:
      operationId: createReport
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Operation"}}}}}
  /reports/{reportId}:
    get:
      operationId: getReport
      parameters: [{name: reportId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Report"}}}}}
  /drafts:
    post:
      operationId: createDraft
      requestBody: {content: {application/json: {schema: {type: object, properties: {Alias: {type: string}, Handle: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Draft"}}}}}
  /drafts/{draftIdOrAlias}:
    get:
      operationId: getDraft
      parameters: [{name: draftIdOrAlias, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Draft"}}}}, "404": {description: missing}}
  /templates:
    post:
      operationId: createTemplate
      requestBody: {content: {application/json: {schema: {type: object, properties: {Alias: {type: string}, Handle: {type: string}, Name: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}}
  /templates/{templateId}:
    parameters: [{name: templateId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getTemplate
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}, "404": {description: missing}}
    delete:
      operationId: deleteTemplate
      responses: {"204": {description: gone}}
components:
  schemas:
    Conversation:
      type: object
      properties: {id: {type: string}, subject: {type: string}}
    Note:
      type: object
      properties: {id: {type: string}, subject: {type: string}}
    Folder:
      type: object
      properties: {id: {type: string}, displayName: {type: string}, slug: {type: string}}
    File:
      type: object
      properties: {id: {type: string}, name: {type: string}}
    Project:
      type: object
      properties: {id: {type: string}, title: {type: string}, code: {type: string}}
    Page:
      type: object
      properties: {id: {type: string}, title: {type: string}, handle: {type: string}}
    Label:
      type: object
      properties: {id: {type: string, maxLength: 6}, name: {type: string}}
    Zone:
      type: object
      properties: {id: {type: string}, name: {type: string}}
    ZoneEnvelope:
      type: object
      properties:
        success: {type: boolean}
        result: {type: array, items: {$ref: "#/components/schemas/Zone"}}
    Contact:
      type: object
      properties: {ContactID: {type: string}, Name: {type: string}}
    Report:
      type: object
      properties: {id: {type: string}, title: {type: string}}
    Operation:
      type: object
      properties:
        name: {type: string}
        done: {type: boolean}
        response: {type: object}
    Draft:
      type: object
      properties: {DraftId: {type: string}, Alias: {type: string}, Handle: {type: string}}
    Template:
      type: object
      properties: {TemplateId: {type: string}, Alias: {type: string}, Handle: {type: string}, Name: {type: string}}
"##;

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn source(headers: &HeaderMap) -> String {
    header(headers, "x-mockingbird-source")
}

fn plain(script: Vec<Value>) -> DescribedHarness {
    support::harness(CONTRACT, script, &HarnessOptions::default(), |rt| rt)
}

fn tier0(script: Vec<Value>) -> DescribedHarness {
    support::harness(CONTRACT, script, &HarnessOptions::default(), |rt| {
        let base = mb_mcm::world::tier0(rt.bundle());
        rt.with_world(&base, &base, 0)
    })
}

/// The `world_gap` marks of the `n`th recorded call.
fn gaps(h: &DescribedHarness, n: usize) -> Value {
    h.records()[n]
        .request
        .get("world_gap")
        .cloned()
        .unwrap_or(Value::Null)
}

/// A runtime of the contract over `store`, serving `world` (a revision document) at
/// revision `revision` when given, as a rebuild after a revision does.
fn on_store(
    store: &Arc<dyn StateStore>,
    world: Option<Value>,
    revision: i64,
    script: Vec<Value>,
) -> axum::Router {
    use mb_engine::{Engine, EngineConfig};
    use mb_llm::{Provider, ScriptedProvider};
    let provider = Arc::new(ScriptedProvider::new(
        script.into_iter().map(|v| Ok(v.to_string())).collect(),
    ));
    let rt = MockRuntime::new(
        "described",
        Arc::new(mb_import::compile(CONTRACT.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(store),
    );
    let rt = match world {
        Some(world) => {
            let (base, effective, refused) = support::effective_world(rt.bundle(), world);
            assert!(refused.is_empty(), "{refused:?}");
            rt.with_world(&base, &effective, revision)
        }
        None => rt,
    };
    mb_protocols::http_rest::router(Arc::new(rt))
}

/// The folder stored under `id` in `store`'s default session.
async fn stored(store: &Arc<dyn StateStore>, id: &str) -> Option<Value> {
    let session = mb_state::SessionKey {
        workspace: "default".into(),
        mock: "described".into(),
        session: "default".into(),
    };
    store
        .get(&session, "folders", id)
        .await
        .unwrap()
        .map(|r| r.data)
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut req = axum::http::Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let body = body.map_or_else(axum::body::Body::empty, |b| {
        axum::body::Body::from(b.to_string())
    });
    support::send_to(router, req.body(body).unwrap()).await
}

#[tokio::test]
async fn a_record_one_item_deep_in_an_envelope_is_stored() {
    let world = json!({
        "types": {"zones": {"identity": {"member": "id"}}},
        "operations": {
            "createZone": {"answer": {"record": "/result/0"}},
            "getZone": {"kind": "read", "target": {"path_param": "zoneId"}, "answer": {"record": "/result/0"}}
        }
    });
    let answer = json!({"success": true, "result": [{"id": "z1", "name": "a"}]});
    let h = support::described(CONTRACT, world, vec![answer.clone()]);
    let (status, _, body) = h
        .send("POST", "/zones", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        h.stored("zones", "z1").await,
        Some(json!({"id": "z1", "name": "a"}))
    );
    let (status, headers, body) = h.send("GET", "/zones/z1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(
        body["result"][0],
        json!({"id": "z1", "name": "a"}),
        "{body}"
    );
}

#[tokio::test]
async fn a_bulk_create_stores_each_record() {
    let world = json!({
        "operations": {
            "putContacts": {"kind": "create", "request": {"records": "/Contacts"}, "answer": {"records": "/Contacts"}}
        }
    });
    let answer =
        json!({"Contacts": [{"ContactID": "c1", "Name": "a"}, {"ContactID": "c2", "Name": "b"}]});
    let h = support::described(CONTRACT, world, vec![answer]);
    let request = json!({"Contacts": [{"Name": "a"}, {"Name": "b"}]});
    let (status, _, body) = h.send("PUT", "/contacts", Some(request), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for (id, name) in [("c1", "a"), ("c2", "b")] {
        let (status, headers, body) = h.send("GET", &format!("/contacts/{id}"), None, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(source(&headers), "state", "{body}");
        assert_eq!(body["Name"], json!(name), "{body}");
    }
    assert_eq!(gaps(&h, 0), Value::Null);
}

#[tokio::test]
async fn a_described_identity_header_reaches_a_header_the_detector_misses() {
    let world =
        json!({"operations": {"createThread": {"answer": {"identity_header": "X-Thread"}}}});
    let h = support::described(CONTRACT, world, vec![json!({"threadIds": [1]})]);
    let (status, headers, _) = h
        .send("POST", "/threads", Some(json!({"subject": "hello"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = header(&headers, "x-thread");
    assert!(!id.is_empty(), "{headers:?}");
    let (status, headers, body) = h.send("GET", &format!("/threads/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["subject"], json!("hello"), "{body}");
    // With no world, the detector finds no header: no id is minted for one.
    let h = plain(vec![json!({"threadIds": [1]})]);
    let (_, headers, _) = h
        .send("POST", "/threads", Some(json!({"subject": "hello"})), &[])
        .await;
    let placed = header(&headers, "x-thread");
    assert_eq!(h.stored("threads", &placed).await, None, "{headers:?}");
}

/// Sends the same create to `a` and `b`; asserts equal answers and stored records.
async fn answer_alike(a: &DescribedHarness, b: &DescribedHarness, t: &str, header_name: &str) {
    let request = Some(json!({"subject": "hello"}));
    let (a_status, a_headers, a_body) =
        a.send("POST", "/conversations", request.clone(), &[]).await;
    let (b_status, b_headers, b_body) = b.send("POST", "/conversations", request, &[]).await;
    assert_eq!(a_status, b_status);
    assert_eq!(a_body, b_body);
    let names = |h: &HeaderMap| {
        let mut names: Vec<String> = h
            .keys()
            .map(|k| k.as_str().to_string())
            .filter(|k| k != "date")
            .collect();
        names.sort();
        names
    };
    assert_eq!(names(&a_headers), names(&b_headers));
    let (a_id, b_id) = (
        header(&a_headers, header_name),
        header(&b_headers, header_name),
    );
    assert!(!a_id.is_empty() && !b_id.is_empty(), "{a_headers:?}");
    assert_eq!(
        a.stored(t, &a_id).await.map(|r| r["subject"].clone()),
        Some(json!("hello"))
    );
    assert_eq!(
        b.stored(t, &b_id).await.map(|r| r["subject"].clone()),
        Some(json!("hello"))
    );
}

#[tokio::test]
async fn a_detected_header_and_the_same_header_described_answer_alike() {
    let script = || vec![json!({"threadIds": [1]})];
    let no_world = plain(script());
    let world = json!({"operations": {"createConversation": {"answer": {"identity_header": "Resource-ID"}}}});
    let described = support::described(CONTRACT, world, script());
    answer_alike(&no_world, &described, "conversations", "resource-id").await;
}

#[tokio::test]
async fn a_location_identity_header_is_the_item_url() {
    // The detector takes `Resource-ID` over `Location`; the description says `Location`.
    let world = json!({"operations": {"createNote": {"answer": {"identity_header": "Location"}}}});
    let h = support::described(CONTRACT, world, vec![Value::Null]);
    let (status, headers, _) = h
        .send("POST", "/notes", Some(json!({"subject": "hello"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let location = header(&headers, "location");
    let id = location
        .rsplit_once("/notes/")
        .map(|(_, id)| id)
        .unwrap_or_else(|| panic!("{location}"));
    assert!(header(&headers, "resource-id").is_empty() || header(&headers, "resource-id") != id);
    let (_, headers, body) = h.send("GET", &format!("/notes/{id}"), None, &[]).await;
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["subject"], json!("hello"));
}

#[tokio::test]
async fn identity_header_false_turns_the_detected_header_off() {
    let world =
        json!({"operations": {"createConversation": {"answer": {"identity_header": false}}}});
    let h = support::described(CONTRACT, world, vec![json!({"threadIds": [1]})]);
    let (status, headers, _) = h
        .send(
            "POST",
            "/conversations",
            Some(json!({"subject": "hello"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let placed = header(&headers, "resource-id");
    // `declared_headers` may fill the header with a faker value; no record is under it.
    if !placed.is_empty() {
        assert_eq!(h.stored("conversations", &placed).await, None);
    }
    assert_eq!(gaps(&h, 0), json!(["no_record"]));
    // With no world the same create mints, places the header and stores.
    let h = plain(vec![json!({"threadIds": [1]})]);
    let (_, headers, _) = h
        .send(
            "POST",
            "/conversations",
            Some(json!({"subject": "hello"})),
            &[],
        )
        .await;
    let id = header(&headers, "resource-id");
    assert!(
        h.stored("conversations", &id).await.is_some(),
        "{headers:?}"
    );
}

#[tokio::test]
async fn a_described_header_replaces_the_detected_one() {
    let world =
        json!({"operations": {"createConversation": {"answer": {"identity_header": "X-Conv"}}}});
    let h = support::described(CONTRACT, world, vec![json!({"threadIds": [1]})]);
    let (_, headers, _) = h
        .send(
            "POST",
            "/conversations",
            Some(json!({"subject": "hello"})),
            &[],
        )
        .await;
    let id = header(&headers, "x-conv");
    assert_eq!(
        h.stored("conversations", &id)
            .await
            .map(|r| r["subject"].clone()),
        Some(json!("hello")),
        "{headers:?}"
    );
    let detected = header(&headers, "resource-id");
    assert_ne!(detected, id);
    if !detected.is_empty() {
        assert_eq!(h.stored("conversations", &detected).await, None);
    }
}

#[tokio::test]
async fn a_long_running_create_stores_its_result() {
    let world = json!({
        "operations": {"createReport": {"answer": {"long_running": {"done": "done", "result": "response"}}}}
    });
    let answer =
        json!({"name": "operations/1", "done": false, "response": {"id": "r7", "title": "a"}});
    let h = support::described(CONTRACT, world, vec![answer]);
    let (status, _, body) = h
        .send("POST", "/reports", Some(json!({"title": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["done"], json!(true), "{body}");
    let (_, headers, body) = h.send("GET", "/reports/r7", None, &[]).await;
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["title"], json!("a"));
}

fn patterned() -> Value {
    json!({"types": {"folders": {"identity": {"pattern": "fld_{id}"}}}})
}

#[tokio::test]
async fn a_patterned_identity_reads_by_bare_or_full_id() {
    let h = support::described(
        CONTRACT,
        patterned(),
        vec![json!({"displayName": "a"}), json!({"displayName": "b"})],
    );
    let (status, _, body) = h
        .send("POST", "/folders", Some(json!({"displayName": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let full = body["id"].as_str().unwrap_or_default().to_string();
    let bare = full
        .strip_prefix("fld_")
        .unwrap_or_else(|| panic!("{body}"))
        .to_string();
    assert!(h.stored("folders", &full).await.is_some());
    for id in [&bare, &full] {
        let (status, headers, body) = h.send("GET", &format!("/folders/{id}"), None, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(source(&headers), "state", "{id}: {body}");
    }
    let (status, _, body) = h
        .send(
            "PATCH",
            &format!("/folders/{bare}"),
            Some(json!({"displayName": "b"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(h.stored("folders", &bare).await, None);
    assert_eq!(
        h.stored("folders", &full)
            .await
            .map(|r| r["displayName"].clone()),
        Some(json!("b"))
    );
    let (status, _, _) = h
        .send("DELETE", &format!("/folders/{bare}"), None, &[])
        .await;
    assert!(status.is_success());
    assert_eq!(h.stored("folders", &full).await, None);
}

#[tokio::test]
async fn a_minted_id_the_schema_refuses_stores_nothing() {
    // A label's id is at most 6 characters: the pattern makes every minted one longer.
    let world = json!({"types": {"labels": {"identity": {"pattern": "label-{id}"}}}});
    let h = support::described(CONTRACT, world, vec![json!({"name": "a"})]);
    let (status, _, body) = h
        .send("POST", "/labels", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body.get("id").is_none_or(Value::is_null), "{body}");
    assert_eq!(gaps(&h, 0), json!(["no_record"]));
}

#[tokio::test]
async fn a_minted_id_the_header_schema_refuses_is_not_placed() {
    // `Ticket-Id` is a uuid: a patterned id is none.
    let world = json!({"types": {"tickets": {"identity": {"pattern": "tickets/{id}"}}}});
    let h = support::described(CONTRACT, world, vec![Value::Null]);
    let (status, headers, _) = h
        .send("POST", "/tickets", Some(json!({"subject": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let placed = header(&headers, "ticket-id");
    assert!(!placed.starts_with("tickets/"), "{placed}");
    assert_eq!(gaps(&h, 0), json!(["no_record"]));
}

fn template(id: &str, alias: &str, handle: &str) -> Value {
    json!({"TemplateId": id, "Alias": alias, "Handle": handle, "Name": "n"})
}

#[tokio::test]
async fn an_alternate_member_reaches_the_same_record() {
    let world = json!({"types": {"templates": {"identity": {"alternates": ["Alias"]}}}});
    let h = support::described(CONTRACT, world, vec![template("t1", "welcome", "h1")]);
    h.send("POST", "/templates", Some(json!({"Alias": "welcome"})), &[])
        .await;
    let (_, by_alias_headers, by_alias) = h.send("GET", "/templates/welcome", None, &[]).await;
    let (_, by_id_headers, by_id) = h.send("GET", "/templates/t1", None, &[]).await;
    assert_eq!(source(&by_alias_headers), "state", "{by_alias}");
    assert_eq!(source(&by_id_headers), "state", "{by_id}");
    assert_eq!(by_alias, by_id);
    let (status, _, _) = h.send("DELETE", "/templates/welcome", None, &[]).await;
    assert!(status.is_success());
    assert_eq!(h.stored("templates", "t1").await, None);
}

#[tokio::test]
async fn an_alternate_is_not_tried_without_a_description() {
    for h in [
        plain(vec![template("t1", "welcome", "h1")]),
        tier0(vec![template("t1", "welcome", "h1")]),
    ] {
        h.send("POST", "/templates", Some(json!({"Alias": "welcome"})), &[])
            .await;
        let (_, headers, _) = h.send("GET", "/templates/welcome", None, &[]).await;
        assert_ne!(source(&headers), "state");
    }
}

#[tokio::test]
async fn the_identity_member_is_tried_before_an_alternate() {
    let world = json!({"types": {"templates": {"identity": {"alternates": ["Alias"]}}}});
    let h = support::described(
        CONTRACT,
        world,
        vec![template("x", "a1", "h1"), template("t2", "x", "h2")],
    );
    h.send("POST", "/templates", Some(json!({"Alias": "a1"})), &[])
        .await;
    h.send("POST", "/templates", Some(json!({"Alias": "x"})), &[])
        .await;
    let (_, headers, body) = h.send("GET", "/templates/x", None, &[]).await;
    assert_eq!(source(&headers), "state");
    assert_eq!(body["TemplateId"], json!("x"), "{body}");
}

#[tokio::test]
async fn a_create_whose_request_wraps_the_record_stores_the_inner_record() {
    let world = json!({"operations": {"createConversation": {"request": {"record": "/data"}}}});
    let h = support::described(CONTRACT, world, vec![json!({"threadIds": [1]})]);
    let (_, headers, _) = h
        .send(
            "POST",
            "/conversations",
            Some(json!({"data": {"subject": "a"}})),
            &[],
        )
        .await;
    let id = header(&headers, "resource-id");
    let stored = h.stored("conversations", &id).await.unwrap_or_default();
    assert_eq!(stored["subject"], json!("a"), "{stored}");
    assert!(stored.get("data").is_none(), "{stored}");
    // With no world, the whole body is the request's record, as today.
    let h = plain(vec![json!({"threadIds": [1]})]);
    let (_, headers, _) = h
        .send(
            "POST",
            "/conversations",
            Some(json!({"data": {"subject": "a"}})),
            &[],
        )
        .await;
    let id = header(&headers, "resource-id");
    let stored = h.stored("conversations", &id).await.unwrap_or_default();
    assert!(stored.get("subject").is_none(), "{stored}");
}

#[tokio::test]
async fn a_create_that_stores_nothing_is_marked_no_record_once() {
    // The thread's create names its id nowhere the runtime finds it.
    let h = support::described_with_gaps(
        CONTRACT,
        json!({}),
        vec![json!({"threadIds": [1]})],
        [(
            "createThread".to_string(),
            vec![mb_protocols::world_gaps::GapCode::NoRecord],
        )]
        .into_iter()
        .collect(),
    );
    h.send("POST", "/threads", Some(json!({"subject": "a"})), &[])
        .await;
    assert_eq!(gaps(&h, 0), json!(["no_record"]));
    // With no static gap, the create that stored nothing is marked by itself.
    let h = support::described_with_gaps(
        CONTRACT,
        json!({}),
        vec![json!({"threadIds": [1]})],
        [("createThread".to_string(), Vec::new())]
            .into_iter()
            .collect(),
    );
    h.send("POST", "/threads", Some(json!({"subject": "a"})), &[])
        .await;
    assert_eq!(gaps(&h, 0), json!(["no_record"]));
}

#[tokio::test]
async fn a_tier0_update_of_a_held_record_logs_no_no_record() {
    let script = || {
        vec![
            json!({"id": "f1", "displayName": "a"}),
            json!({"id": "f1", "displayName": "b"}),
            json!({"id": "f1", "slug": "c"}),
            json!([{"id": "f1", "displayName": "b"}]),
        ]
    };
    for h in [plain(script()), tier0(script())] {
        h.send("POST", "/folders", Some(json!({"displayName": "a"})), &[])
            .await;
        h.send(
            "PATCH",
            "/folders/f1",
            Some(json!({"displayName": "b"})),
            &[],
        )
        .await;
        h.send(
            "PATCH",
            "/folders/f1",
            Some(json!({"displayName": "c"})),
            &[],
        )
        .await;
        h.send("DELETE", "/folders/f1", None, &[]).await;
        for (n, record) in h.records().iter().enumerate() {
            let marks = record
                .request
                .get("world_gap")
                .cloned()
                .unwrap_or(Value::Null);
            assert!(
                !marks.to_string().contains("no_record"),
                "call {n}: {marks}"
            );
        }
    }
}

#[tokio::test]
async fn an_update_of_an_unheld_record_answering_an_ack_is_marked_no_record() {
    // No static gap: the mark comes from the write that found no record.
    let h = support::described_with_gaps(
        CONTRACT,
        json!({}),
        vec![json!({"ok": true})],
        [("ackNote".to_string(), Vec::new())].into_iter().collect(),
    );
    let (status, _, _) = h
        .send("PATCH", "/notes/n1", Some(json!({"subject": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(gaps(&h, 0), json!(["no_record"]));
}

#[tokio::test]
async fn a_revision_that_changes_the_identity_member_still_reaches_earlier_records() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let before = on_store(
        &store,
        None,
        0,
        vec![json!({"id": "f1", "displayName": "a", "slug": "docs"})],
    );
    send(
        &before,
        "POST",
        "/folders",
        Some(json!({"displayName": "a", "slug": "docs"})),
    )
    .await;
    let world = json!({"types": {"folders": {"identity": {"member": "slug"}}}});
    let after = on_store(
        &store,
        Some(world),
        1,
        vec![json!({"id": "f2", "displayName": "b", "slug": "notes"})],
    );
    let (_, headers, body) = send(&after, "GET", "/folders/docs", None).await;
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["id"], json!("f1"));
    let (status, _, _) = send(&after, "DELETE", "/folders/docs", None).await;
    assert!(status.is_success());
    let (status, _, body) = send(&after, "GET", "/folders/docs", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(stored(&store, "f1").await, None);
    send(
        &after,
        "POST",
        "/folders",
        Some(json!({"displayName": "b", "slug": "notes"})),
    )
    .await;
    let (_, headers, body) = send(&after, "GET", "/folders/notes", None).await;
    assert_eq!(source(&headers), "state", "{body}");
}

#[tokio::test]
async fn a_revision_that_adds_a_pattern_still_reaches_bare_keyed_records() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let before = on_store(
        &store,
        None,
        0,
        vec![json!({"id": "7", "displayName": "a"})],
    );
    send(
        &before,
        "POST",
        "/folders",
        Some(json!({"displayName": "a"})),
    )
    .await;
    let after = on_store(
        &store,
        Some(patterned()),
        1,
        vec![json!({"id": "7", "displayName": "b"})],
    );
    for id in ["7", "fld_7"] {
        let (_, headers, body) = send(&after, "GET", &format!("/folders/{id}"), None).await;
        assert_eq!(source(&headers), "state", "{id}: {body}");
    }
    let (status, _, body) = send(
        &after,
        "PATCH",
        "/folders/fld_7",
        Some(json!({"displayName": "b"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, _, body) = send(&after, "GET", "/folders/7", None).await;
    assert_eq!(body["displayName"], json!("b"), "{body}");
    let (status, _, _) = send(&after, "DELETE", "/folders/fld_7", None).await;
    assert!(status.is_success());
    let (status, _, body) = send(&after, "GET", "/folders/7", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(stored(&store, "7").await, None);
}

#[tokio::test]
async fn a_described_alternate_adds_to_the_detected_members() {
    // `draftIdOrAlias` offers `Alias` (fix 5); the description adds `Handle`.
    let draft = || vec![json!({"DraftId": "d1", "Alias": "welcome", "Handle": "hi"})];
    let world = json!({"types": {"drafts": {"identity": {"alternates": ["Handle"]}}}});
    let h = support::described(CONTRACT, world, draft());
    h.send(
        "POST",
        "/drafts",
        Some(json!({"Alias": "welcome", "Handle": "hi"})),
        &[],
    )
    .await;
    for value in ["d1", "welcome", "hi"] {
        let (_, headers, body) = h.send("GET", &format!("/drafts/{value}"), None, &[]).await;
        assert_eq!(source(&headers), "state", "{value}: {body}");
        assert_eq!(body["DraftId"], json!("d1"), "{value}: {body}");
    }
    // With no world (and under tier 0) only the detected `Alias` reaches it.
    for h in [plain(draft()), tier0(draft())] {
        h.send(
            "POST",
            "/drafts",
            Some(json!({"Alias": "welcome", "Handle": "hi"})),
            &[],
        )
        .await;
        let (_, headers, _) = h.send("GET", "/drafts/welcome", None, &[]).await;
        assert_eq!(source(&headers), "state");
        let (_, headers, _) = h.send("GET", "/drafts/hi", None, &[]).await;
        assert_ne!(source(&headers), "state");
    }
}

/// The last message of the `n`th prompt the model was sent.
fn prompt(h: &DescribedHarness, n: usize) -> String {
    h.provider.requests()[n]
        .messages
        .last()
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

/// Every contact stored in the default session.
async fn contacts(h: &DescribedHarness) -> Vec<Value> {
    let session = mb_state::SessionKey {
        workspace: h.rt.workspace.clone(),
        mock: h.rt.name.clone(),
        session: "default".into(),
    };
    let mut held: Vec<Value> =
        h.rt.state
            .list(&session, "contacts", 100)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.data)
            .collect();
    held.sort_by_key(|c| c["Name"].as_str().unwrap_or_default().to_string());
    held
}

#[tokio::test]
async fn a_bulk_create_from_the_request_alone_stores_each_record_under_a_minted_id() {
    let world = json!({
        "operations": {"putContacts": {"kind": "create", "request": {"records": "/Contacts"}}}
    });
    let h = support::described(CONTRACT, world, vec![json!({"Contacts": []})]);
    let request = json!({"Contacts": [{"Name": "a"}, {"Name": "b"}]});
    let (status, _, body) = h.send("PUT", "/contacts", Some(request), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let held = contacts(&h).await;
    assert_eq!(held.len(), 2, "{held:?}");
    let ids: Vec<String> = held
        .iter()
        .map(|c| c["ContactID"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(ids.iter().all(|id| !id.is_empty()), "{held:?}");
    assert_ne!(ids[0], ids[1]);
    for (id, name) in ids.iter().zip(["a", "b"]) {
        let (status, headers, body) = h.send("GET", &format!("/contacts/{id}"), None, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(source(&headers), "state", "{body}");
        assert_eq!(body["Name"], json!(name), "{body}");
    }
    assert_eq!(gaps(&h, 0), Value::Null);
}

#[tokio::test]
async fn a_bulk_answer_whose_records_hold_no_ids_is_given_them() {
    let world = json!({
        "operations": {"putContacts": {"kind": "create", "answer": {"records": "/Contacts"}}}
    });
    let answer = json!({"Contacts": [{"Name": "a"}, {"Name": "b"}]});
    let h = support::described(CONTRACT, world, vec![answer]);
    let request = json!({"Contacts": [{"Name": "a"}, {"Name": "b"}]});
    let (status, _, body) = h.send("PUT", "/contacts", Some(request), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for (item, name) in body["Contacts"].as_array().unwrap().iter().zip(["a", "b"]) {
        let id = item["ContactID"]
            .as_str()
            .unwrap_or_else(|| panic!("{body}"));
        let (status, headers, read) = h.send("GET", &format!("/contacts/{id}"), None, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(source(&headers), "state", "{read}");
        assert_eq!(read["Name"], json!(name), "{read}");
    }
    assert_eq!(contacts(&h).await.len(), 2);
}

#[tokio::test]
async fn a_long_running_result_with_no_id_is_given_one() {
    let world = json!({
        "operations": {"createReport": {"answer": {"long_running": {"done": "done", "result": "response"}}}}
    });
    let answer = json!({"name": "operations/1", "done": false, "response": {"title": "a"}});
    let h = support::described(CONTRACT, world, vec![answer]);
    let (status, _, body) = h
        .send("POST", "/reports", Some(json!({"title": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["response"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{body}"));
    let (_, headers, read) = h.send("GET", &format!("/reports/{id}"), None, &[]).await;
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["title"], json!("a"));
}

#[tokio::test]
async fn a_nested_route_reaches_a_patterned_parent_by_its_bare_id() {
    // Stored under the patterned id, named bare by the path; then the reverse.
    for (stored_as, named) in [("fld_7", "7"), ("7", "fld_7")] {
        let folder = json!({"id": stored_as, "displayName": "Quarterly-Plans"});
        let h = support::described(CONTRACT, patterned(), vec![folder, json!([])]);
        let (status, _, body) = h
            .send(
                "POST",
                "/folders",
                Some(json!({"displayName": "Quarterly-Plans"})),
                &[],
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert!(h.stored("folders", stored_as).await.is_some(), "{body}");
        let (status, _, body) = h
            .send("GET", &format!("/folders/{named}/files"), None, &[])
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            prompt(&h, 1).contains("Quarterly-Plans"),
            "{stored_as} by {named}: {}",
            prompt(&h, 1)
        );
    }
}

#[tokio::test]
async fn a_nested_route_reaches_its_parent_by_a_described_alternate() {
    // `{project}` names no member: the parent is looked up by name, then by `code`.
    let script = || {
        vec![
            json!({"id": "pr1", "title": "Annual-Budget", "code": "RM"}),
            json!([]),
        ]
    };
    let world = json!({"types": {"projects": {"identity": {"alternates": ["code"]}}}});
    let described = support::described(CONTRACT, world, script());
    let plain = plain(script());
    let mut shown = Vec::new();
    for h in [&described, &plain] {
        let request = json!({"title": "Annual-Budget", "code": "RM"});
        h.send("POST", "/projects", Some(request), &[]).await;
        let (status, _, body) = h.send("GET", "/projects/RM/files", None, &[]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        shown.push(prompt(h, 1).contains("Annual-Budget"));
    }
    // With no world the alternate is not tried.
    assert_eq!(shown, [true, false]);
}

#[tokio::test]
async fn another_owners_record_named_by_a_described_alternate_is_not_missing() {
    let options = HarnessOptions {
        unknown_ids_not_found: true,
        ..HarnessOptions::default()
    };
    let script = || {
        vec![
            json!({"id": "p1", "title": "a", "handle": "hx"}),
            json!({"id": "p2", "title": "b", "handle": "hx"}),
        ]
    };
    let world = json!({"types": {"pages": {"identity": {"alternates": ["handle"]}}}});
    let described = support::described_with(CONTRACT, world, script(), &options, false);
    let plain = support::harness(CONTRACT, script(), &options, |rt| rt);
    let mut statuses = Vec::new();
    for h in [&described, &plain] {
        let (status, _, body) = h
            .send(
                "POST",
                "/spaces/s1/pages",
                Some(json!({"title": "a", "handle": "hx"})),
                &[],
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        // Another space's page goes by `hx`: a route that cannot tell whose it is does
        // not call it missing.
        let (status, _, _) = h.send("GET", "/spaces/s2/pages/hx", None, &[]).await;
        statuses.push(status);
        let (status, _, _) = h.send("GET", "/spaces/s2/pages/zz9", None, &[]).await;
        statuses.push(status);
        let (status, _, _) = h.send("GET", "/spaces/s1/pages/hx", None, &[]).await;
        statuses.push(status);
    }
    // Under its own space it is read by `handle` too; a value no page holds is missing.
    // With no world `handle` names nothing.
    let (ok, missing) = (StatusCode::OK, StatusCode::NOT_FOUND);
    assert_eq!(statuses, [ok, missing, ok, missing, missing, missing]);
}
