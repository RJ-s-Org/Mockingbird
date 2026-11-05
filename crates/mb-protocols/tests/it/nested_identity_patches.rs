//! A record keyed by a member of its object metadata (`info.label`, the `Info` gadgets
//! and widgets share), a create whose
//! answer holds server-set members beside what the request sent, `PATCH` nulls that
//! remove a member, and a path parameter that may hold either of two members.
#![cfg(test)]
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Capabilities, ChatRequest, ChatResponse, LlmError, Provider, ResponseFormat, Usage};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Shelves, version: "1"}
paths:
  /racks/{rack}/gadgets:
    parameters:
      - {name: rack, in: path, required: true, schema: {type: string}}
    get:
      operationId: listGadgets
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  kind: {type: string}
                  items: {type: array, items: {$ref: "#/components/schemas/Gadget"}}
    post:
      operationId: createGadget
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/Gadget"}
      responses:
        "201":
          description: made
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Gadget"}
        "409":
          description: The label is taken in this rack.
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Problem"}
  /racks/{rack}/gadgets/{label}:
    parameters:
      - {name: rack, in: path, required: true, schema: {type: string}}
      - {name: label, in: path, required: true, schema: {type: string}}
    get:
      operationId: readGadget
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Gadget"}
        "404":
          description: none
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Problem"}
    patch:
      operationId: patchGadget
      description: A JSON merge patch.
      requestBody:
        content:
          application/merge-patch+json:
            schema: {type: object, additionalProperties: true}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Gadget"}
  /widgets/{widget}:
    get:
      operationId: readWidget
      parameters:
        - {name: widget, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Widget"}
  /apps:
    post:
      operationId: createApp
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                name: {type: string}
      responses:
        "201":
          description: made
          content:
            application/json:
              schema: {$ref: "#/components/schemas/App"}
  /apps/{app_id_or_name}:
    parameters:
      - {name: app_id_or_name, in: path, required: true, schema: {type: string}}
    get:
      operationId: readApp
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/App"}
    patch:
      operationId: updateApp
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                name: {type: string}
                paused: {type: boolean}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/App"}
  /apps/{app_id_or_name}/vars:
    parameters:
      - {name: app_id_or_name, in: path, required: true, schema: {type: string}}
    get:
      operationId: readVars
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Vars"}
    patch:
      operationId: updateVars
      requestBody:
        content:
          application/json:
            schema:
              type: object
              additionalProperties: {type: string, nullable: true}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Vars"}
components:
  schemas:
    Info:
      type: object
      properties:
        label: {type: string}
        rack: {type: string, readOnly: true}
        serial: {type: string, format: uuid, readOnly: true}
        revision: {type: string}
        tags:
          type: object
          additionalProperties: {type: string}
    Gadget:
      type: object
      properties:
        kind: {type: string}
        info: {$ref: "#/components/schemas/Info"}
        settings:
          type: object
          additionalProperties: {type: string}
    Widget:
      type: object
      properties:
        kind: {type: string}
        info: {$ref: "#/components/schemas/Info"}
    Problem:
      type: object
      properties:
        reason: {type: string}
    App:
      type: object
      properties:
        id: {type: string, format: uuid}
        name: {type: string}
        paused: {type: boolean}
    Vars:
      type: object
      additionalProperties: {type: string}
"##;

const SERIAL: &str = "6f1d2c3b-4a5e-4f60-8a7b-9c0d1e2f3a4b";
const APP_ID: &str = "0b7e5f2a-1c3d-4e5f-8a9b-0c1d2e3f4a5b";

/// Answers each request with the next of its answers, wrapped in the effects envelope
/// when the schema sent asks for one; counts the requests.
struct Scripted {
    answers: Mutex<VecDeque<Value>>,
    requests: Mutex<usize>,
}

#[async_trait]
impl Provider for Scripted {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: true,
            json_mode: true,
        }
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let enveloped = match &req.response_format {
            ResponseFormat::JsonSchema { schema, .. } => {
                schema.pointer("/properties/effects").is_some()
            }
            _ => false,
        };
        *self.requests.lock().unwrap() += 1;
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| LlmError::Empty("script exhausted".into()))?;
        let content = match enveloped {
            true => json!({"effects": [], "response": answer}),
            false => answer,
        };
        Ok(ChatResponse {
            content: content.to_string(),
            usage: Usage::default(),
        })
    }
}

struct Harness {
    router: axum::Router,
    provider: Arc<Scripted>,
}

fn harness(answers: Vec<Value>) -> Harness {
    harness_on(CONTRACT, answers)
}

fn harness_on(contract: &str, answers: Vec<Value>) -> Harness {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let provider = Arc::new(Scripted {
        answers: Mutex::new(answers.into()),
        requests: Mutex::new(0),
    });
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        store,
    );
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
    }
}

impl Harness {
    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.send_as(method, uri, "application/json", body).await
    }

    async fn send_as(
        &self,
        method: &str,
        uri: &str,
        content_type: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", content_type)
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn requests(&self) -> usize {
        *self.provider.requests.lock().unwrap()
    }
}

/// What the model answers a create of `alpha`: the server's members in `info` beside
/// what the request sent, and a tag of its own.
fn made_alpha() -> Value {
    json!({
        "kind": "Gadget",
        "info": {
            "label": "alpha", "rack": "r1", "serial": SERIAL, "revision": "7",
            "tags": {"app": "web", "invented": "yes"},
        },
        "settings": {"A": "1", "B": "2"},
    })
}

fn alpha_request() -> Value {
    json!({
        "kind": "Gadget",
        "info": {"label": "alpha", "tags": {"app": "web"}},
        "settings": {"A": "1", "B": "2"},
    })
}

#[tokio::test]
async fn a_create_keeps_the_nested_members_the_server_set_and_the_request_s_map_whole() {
    let h = harness(vec![made_alpha()]);
    let (status, made) = h
        .send("POST", "/racks/r1/gadgets", Some(alpha_request()))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["info"]["label"], "alpha", "{made}");
    assert_eq!(made["info"]["serial"], SERIAL, "{made}");
    assert_eq!(made["info"]["revision"], "7", "{made}");
    assert_eq!(made["info"]["rack"], "r1", "{made}");
    // A map the request sends is all the record's map holds.
    assert_eq!(made["info"]["tags"], json!({"app": "web"}), "{made}");
    assert_eq!(made["settings"], json!({"A": "1", "B": "2"}), "{made}");
}

#[tokio::test]
async fn a_record_keyed_inside_its_metadata_is_stored_read_and_listed_by_it() {
    let h = harness(vec![made_alpha(), json!({"kind": "List"})]);
    h.send("POST", "/racks/r1/gadgets", Some(alpha_request()))
        .await;
    let before = h.requests();
    let (status, read) = h.send("GET", "/racks/r1/gadgets/alpha", None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["info"]["label"], "alpha", "{read}");
    assert_eq!(read["info"]["serial"], SERIAL, "{read}");
    assert!(read.get("info.label").is_none(), "{read}");
    assert_eq!(h.requests(), before, "read from the store");

    let (status, list) = h.send("GET", "/racks/r1/gadgets", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let labels: Vec<&Value> = list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| &item["info"]["label"])
        .collect();
    assert_eq!(labels, [&json!("alpha")], "{list}");
}

#[tokio::test]
async fn a_merge_patch_null_removes_the_member_at_any_depth() {
    // The model keeps what the patch removes.
    let patched = json!({
        "kind": "Gadget",
        "info": {"label": "alpha", "rack": "r1", "serial": SERIAL, "revision": "8",
                 "tags": {"app": "web", "tier": "db"}},
        "settings": {"A": "1", "B": "2"},
    });
    let h = harness(vec![made_alpha(), patched]);
    h.send("POST", "/racks/r1/gadgets", Some(alpha_request()))
        .await;
    let (status, answer) = h
        .send_as(
            "PATCH",
            "/racks/r1/gadgets/alpha",
            "application/merge-patch+json",
            Some(json!({"info": {"tags": {"app": null, "tier": "db"}}, "settings": {"A": null}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["info"]["tags"], json!({"tier": "db"}), "{answer}");
    assert_eq!(answer["settings"], json!({"B": "2"}), "{answer}");
    assert_eq!(answer["info"]["serial"], SERIAL, "{answer}");
    assert!(answer.get("info.label").is_none(), "{answer}");

    let (_, read) = h.send("GET", "/racks/r1/gadgets/alpha", None).await;
    assert_eq!(read["info"]["tags"], json!({"tier": "db"}), "{read}");
    assert_eq!(read["settings"], json!({"B": "2"}), "{read}");
    assert!(read.get("info.label").is_none(), "{read}");
}

#[tokio::test]
async fn a_null_the_request_admits_and_the_record_does_not_removes_the_member() {
    let h = harness(vec![
        json!({"X": "1", "Z": "3"}),
        // The model keeps the var the request removes.
        json!({"X": "1", "Y": "2", "Z": "3"}),
    ]);
    let (status, _) = h.send("GET", "/apps/blue/vars", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, answer) = h
        .send(
            "PATCH",
            "/apps/blue/vars",
            Some(json!({"X": null, "Y": "2"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer, json!({"Y": "2", "Z": "3"}));
}

#[tokio::test]
async fn a_compound_path_value_is_written_to_the_member_it_fits_only() {
    let h = harness(vec![
        json!({"id": APP_ID, "name": "blue"}),
        // A rename by the old name, answered right.
        json!({"id": APP_ID, "name": "green", "paused": false}),
    ]);
    let (status, made) = h.send("POST", "/apps", Some(json!({"name": "blue"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["id"], APP_ID);
    let (status, renamed) = h
        .send("PATCH", "/apps/blue", Some(json!({"name": "green"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["name"], "green", "the rename stands: {renamed}");
    assert_eq!(
        renamed["id"], APP_ID,
        "a name is never written into a uuid id: {renamed}"
    );
}

#[tokio::test]
async fn an_update_settled_on_a_nested_key_keeps_it_where_it_sits() {
    let patched = json!({
        "kind": "Gadget",
        "info": {"label": "alpha", "rack": "r1", "serial": SERIAL, "revision": "8",
                 "tags": {"app": "web"}},
        "settings": {"A": "9", "B": "2"},
    });
    let h = harness(vec![made_alpha(), patched]);
    h.send("POST", "/racks/r1/gadgets", Some(alpha_request()))
        .await;
    let (status, answer) = h
        .send_as(
            "PATCH",
            "/racks/r1/gadgets/alpha",
            "application/merge-patch+json",
            Some(json!({"settings": {"A": "9"}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["info"]["label"], "alpha", "{answer}");
    assert!(answer.get("info.label").is_none(), "answer: {answer}");
    let (_, read) = h.send("GET", "/racks/r1/gadgets/alpha", None).await;
    assert_eq!(read["settings"]["A"], "9", "{read}");
    assert!(read.get("info.label").is_none(), "read: {read}");
}

#[tokio::test]
async fn a_merge_patch_null_clears_a_required_member_that_may_be_null() {
    let contract = CONTRACT.replace(
        "    Gadget:\n      type: object\n      properties:\n        kind: {type: string}",
        "    Gadget:\n      type: object\n      required: [kind]\n      properties:\n        kind: {type: string, nullable: true}",
    );
    assert_ne!(contract, CONTRACT);
    // The model keeps the kind the patch clears.
    let patched = json!({
        "kind": "Gadget",
        "info": {"label": "alpha", "rack": "r1", "serial": SERIAL, "revision": "8",
                 "tags": {"app": "web"}},
        "settings": {"A": "1", "B": "2"},
    });
    let h = harness_on(&contract, vec![made_alpha(), patched]);
    h.send("POST", "/racks/r1/gadgets", Some(alpha_request()))
        .await;
    let (status, answer) = h
        .send_as(
            "PATCH",
            "/racks/r1/gadgets/alpha",
            "application/merge-patch+json",
            Some(json!({"kind": null})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer.get("kind"), Some(&Value::Null), "{answer}");
    let (_, read) = h.send("GET", "/racks/r1/gadgets/alpha", None).await;
    assert_eq!(read.get("kind"), Some(&Value::Null), "{read}");
}

/// Boxes answered in a `{data: Box}` envelope; the patch sends the record itself.
const ENVELOPED: &str = r##"
openapi: 3.0.0
info: {title: Boxes, version: "1"}
paths:
  /boxes:
    post:
      operationId: createBox
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/Box"}
      responses:
        "201":
          description: made
          content:
            application/json:
              schema: {$ref: "#/components/schemas/BoxOut"}
  /boxes/{box_id}:
    parameters:
      - {name: box_id, in: path, required: true, schema: {type: string}}
    get:
      operationId: readBox
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/BoxOut"}
    patch:
      operationId: patchBox
      requestBody:
        content:
          application/merge-patch+json:
            schema: {type: object, additionalProperties: true}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/BoxOut"}
components:
  schemas:
    Box:
      type: object
      properties:
        id: {type: string, readOnly: true}
        note: {type: string}
        color: {type: string}
    BoxOut:
      type: object
      required: [data]
      properties:
        data: {$ref: "#/components/schemas/Box"}
"##;

#[tokio::test]
async fn a_merge_patch_null_removes_the_member_of_an_enveloped_record() {
    let h = harness_on(
        ENVELOPED,
        vec![
            json!({"data": {"id": "b1", "note": "n", "color": "red"}}),
            // The model keeps the note the patch removes.
            json!({"data": {"id": "b1", "note": "n", "color": "blue"}}),
        ],
    );
    let (status, made) = h
        .send("POST", "/boxes", Some(json!({"note": "n", "color": "red"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, answer) = h
        .send_as(
            "PATCH",
            "/boxes/b1",
            "application/merge-patch+json",
            Some(json!({"note": null, "color": "blue"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer["data"],
        json!({"id": "b1", "color": "blue"}),
        "{answer}"
    );
    let (_, read) = h.send("GET", "/boxes/b1", None).await;
    assert_eq!(read["data"], json!({"id": "b1", "color": "blue"}), "{read}");
}

/// Profiles read by `{username}`, each holding its manager inline: a related record,
/// which no other type shares as metadata.
const PROFILES: &str = r##"
openapi: 3.0.0
info: {title: Profiles, version: "1"}
paths:
  /profiles:
    post:
      operationId: createProfile
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/Profile"}
      responses:
        "201":
          description: made
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Profile"}
  /profiles/{username}:
    get:
      operationId: readProfile
      parameters:
        - {name: username, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Profile"}
components:
  schemas:
    Person:
      type: object
      properties:
        username: {type: string}
        email: {type: string}
    Profile:
      type: object
      properties:
        handle: {type: string}
        bio: {type: string}
        manager: {$ref: "#/components/schemas/Person"}
"##;

fn profile(handle: &str, bio: &str) -> Value {
    json!({"handle": handle, "bio": bio,
           "manager": {"username": "carol", "email": "carol@example.com"}})
}

#[tokio::test]
async fn records_sharing_a_related_record_held_inline_are_not_keyed_by_it() {
    let h = harness_on(
        PROFILES,
        vec![
            profile("alice", "a"),
            profile("bob", "b"),
            profile("alice", "a"),
            profile("carol", "c"),
        ],
    );
    let (status, alice) = h
        .send("POST", "/profiles", Some(profile("alice", "a")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{alice}");
    let (status, bob) = h.send("POST", "/profiles", Some(profile("bob", "b"))).await;
    assert_eq!(status, StatusCode::CREATED, "{bob}");
    assert_eq!(
        bob["handle"], "bob",
        "bob's create is no repeat of alice's: {bob}"
    );
    assert_eq!(bob["bio"], "b", "{bob}");

    let (status, read) = h.send("GET", "/profiles/alice", None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(
        read["manager"]["username"], "carol",
        "the manager's username is never rewritten to the path's: {read}"
    );
    let (_, carol) = h.send("GET", "/profiles/carol", None).await;
    assert_ne!(
        carol["handle"], "alice",
        "carol's read is not alice's record: {carol}"
    );
}
