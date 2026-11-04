//! A record keyed by a member of its object metadata (`info.label`), a create whose
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
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let provider = Arc::new(Scripted {
        answers: Mutex::new(answers.into()),
        requests: Mutex::new(0),
    });
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(CONTRACT.as_bytes()).unwrap()),
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

    let (_, read) = h.send("GET", "/racks/r1/gadgets/alpha", None).await;
    assert_eq!(read["info"]["tags"], json!({"tier": "db"}), "{read}");
    assert_eq!(read["settings"], json!({"B": "2"}), "{read}");
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
