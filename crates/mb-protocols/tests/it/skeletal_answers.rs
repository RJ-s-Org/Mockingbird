//! A model that writes only the members a schema requires, against a contract whose
//! record schemas require nothing: a create answered with no id still stores its
//! record under a fresh one, and a list answered with no list member still serves
//! what the session stored.
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

/// Nothing in it is required, as in many real contracts.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Gadgets, version: "1"}
paths:
  /gadgets:
    post:
      operationId: createGadget
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              properties:
                label: {type: string}
                color: {type: string}
                passcode: {type: string, writeOnly: true}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Gadget"}}}}
    get:
      operationId: listGadgets
      parameters: [{name: q, in: query, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  count: {type: integer}
                  gadgets: {type: array, items: {$ref: "#/components/schemas/Gadget"}}
  /gadgets/{gadget_id}:
    get:
      operationId: getGadget
      parameters: [{name: gadget_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Gadget"}}}}
components:
  schemas:
    Gadget:
      type: object
      properties:
        id: {type: string, format: uuid}
        label: {type: string}
        color: {type: string}
        passcode: {type: string, writeOnly: true}
"##;

/// Answers each request with the next of its answers, minimal ones, wrapped in the
/// effects envelope when the schema sent asks for one; records every request.
struct Minimal {
    answers: Mutex<VecDeque<Value>>,
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl Provider for Minimal {
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
        self.requests.lock().unwrap().push(req);
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
    provider: Arc<Minimal>,
}

fn harness(answers: Vec<Value>) -> Harness {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let provider = Arc::new(Minimal {
        answers: Mutex::new(answers.into()),
        requests: Mutex::new(Vec::new()),
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
        self.send_with(method, uri, body, &[]).await
    }

    /// [`Self::send`] of a create, seeded: a seeded create takes the generated path,
    /// which stores the record the model answered under the id it chose, as these
    /// tests expect.
    async fn seeded(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.send_with(method, uri, body, &[crate::support::SEED])
            .await
    }

    async fn send_with(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let req = req
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

    /// The schema sent with the `n`th request, out of its effects envelope.
    fn schema_sent(&self, n: usize) -> Value {
        let requests = self.provider.requests.lock().unwrap();
        let ResponseFormat::JsonSchema { schema, .. } = &requests[n].response_format else {
            panic!("no schema sent");
        };
        schema
            .pointer("/properties/response")
            .cloned()
            .map(|response| {
                let mut response = response;
                if let Some(defs) = schema.get("$defs") {
                    response["$defs"] = defs.clone();
                }
                response
            })
            .unwrap_or_else(|| schema.clone())
    }

    fn requests(&self) -> usize {
        self.provider.requests.lock().unwrap().len()
    }
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

/// The record schema at the root of a schema sent: inlined, or a `$ref` to its `$defs`.
fn root_record(schema: &Value) -> Value {
    match schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
    {
        Some(name) => schema["$defs"][name].clone(),
        None => schema.clone(),
    }
}

fn required(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn a_create_answered_without_an_id_is_stored_under_a_minted_one() {
    let h = harness(vec![json!({"label": "lamp"})]);
    let (status, created) = h
        .send("POST", "/gadgets", Some(json!({"label": "lamp"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("an id is minted").to_string();
    assert!(is_uuid(&id), "drawn in the member's declared format: {id}");
    assert_eq!(created["label"], "lamp");

    // The record is stored: the read by id is answered from it, with no model call.
    let before = h.requests();
    let (status, read) = h.send("GET", &format!("/gadgets/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["id"], id.as_str());
    assert_eq!(read["label"], "lamp");
    assert_eq!(h.requests(), before, "read from the store");
}

#[tokio::test]
async fn identical_skeletal_creates_get_distinct_ids() {
    let h = harness(vec![json!({"label": "lamp"}), json!({"label": "lamp"})]);
    let (_, first) = h
        .send("POST", "/gadgets", Some(json!({"label": "lamp"})))
        .await;
    let (_, second) = h
        .send("POST", "/gadgets", Some(json!({"label": "lamp"})))
        .await;
    let (a, b) = (
        first["id"].as_str().unwrap(),
        second["id"].as_str().unwrap(),
    );
    assert_ne!(a, b, "unique for the type in the session");
}

#[tokio::test]
async fn a_list_answered_without_its_list_member_serves_the_stored_records() {
    let h = harness(vec![
        json!({"label": "lamp"}),
        json!({"label": "desk"}),
        json!({"count": 0}),
    ]);
    let (_, lamp) = h
        .seeded("POST", "/gadgets", Some(json!({"label": "lamp"})))
        .await;
    let (_, desk) = h
        .seeded("POST", "/gadgets", Some(json!({"label": "desk"})))
        .await;
    let (status, list) = h.send("GET", "/gadgets", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let ids: Vec<&str> = list["gadgets"]
        .as_array()
        .expect("the list member is there")
        .iter()
        .filter_map(|g| g["id"].as_str())
        .collect();
    assert_eq!(
        ids,
        [lamp["id"].as_str().unwrap(), desk["id"].as_str().unwrap()],
        "{list}"
    );
}

#[tokio::test]
async fn a_stored_record_the_query_does_not_show_stays_off_the_filled_page() {
    let h = harness(vec![json!({"label": "lamp"}), json!({"count": 0})]);
    h.seeded("POST", "/gadgets", Some(json!({"label": "lamp"})))
        .await;
    let (status, list) = h.send("GET", "/gadgets?q=zebra", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["gadgets"], json!([]), "{list}");
}

#[tokio::test]
async fn a_list_answered_without_its_list_member_and_nothing_stored_is_empty() {
    let h = harness(vec![json!({"count": 0})]);
    let (status, list) = h.send("GET", "/gadgets", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["gadgets"], json!([]), "{list}");
}

#[tokio::test]
async fn the_prompt_schema_requires_what_the_answer_is_sure_to_need() {
    let h = harness(vec![json!({"label": "lamp"}), json!({"count": 0})]);
    h.seeded(
        "POST",
        "/gadgets",
        Some(json!({"label": "lamp", "passcode": "hunter2"})),
    )
    .await;
    let create = root_record(&h.schema_sent(0));
    let create_required = required(&create);
    assert!(create_required.contains(&"id".to_string()), "{create}");
    assert!(create_required.contains(&"label".to_string()), "{create}");
    assert!(
        !create_required.contains(&"passcode".to_string()),
        "a writeOnly member is never asked for: {create}"
    );
    assert!(
        !create_required.contains(&"color".to_string()),
        "not sent: {create}"
    );

    h.send("GET", "/gadgets", None).await;
    let list = root_record(&h.schema_sent(1));
    assert_eq!(required(&list), ["gadgets"], "{list}");
}
