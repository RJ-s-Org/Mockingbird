//! Two documented HTTP conventions, found from the contract's structure alone:
//! `If-None-Match` on a read declaring a `304` (RFC 9110), answered `304` with no LLM
//! call while the stored record's entity tag is current; and lists paged by the `Range`
//! request header with a `206` and `Next-Range` (Heroku's), ordered and cut from what
//! the session holds. Driven over HTTP with a scripted LLM.
#![cfg(test)]
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Capabilities, ChatRequest, ChatResponse, LlmError, Provider, ResponseFormat, Usage};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Events read with `If-None-Match` and a `304`, guarded by `If-Match` on their `etag`;
/// memos whose read declares the header but no `304`; tags whose `etag` the client
/// sets. Apps paged by `Range` with a `206` and `Next-Range`; widgets whose list takes
/// a `Range` header but declares no `206`.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Desk, version: "1"}
paths:
  /events:
    post:
      operationId: createEvent
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {summary: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Event"}}}}
  /events/{event_id}:
    parameters: [{name: event_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getEvent
      parameters: [{name: If-None-Match, in: header, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Event"}}}}
        "304": {description: Not modified.}
        "404": {description: missing, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
    patch:
      operationId: patchEvent
      parameters: [{name: If-Match, in: header, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {summary: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Event"}}}}
        "412": {description: Precondition failed., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /memos:
    post:
      operationId: createMemo
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {text: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Memo"}}}}
  /memos/{memo_id}:
    get:
      operationId: getMemo
      parameters:
        - {name: memo_id, in: path, required: true, schema: {type: string}}
        - {name: If-None-Match, in: header, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Memo"}}}}
  /tags:
    post:
      operationId: createTag
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {label: {type: string}, etag: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Tag"}}}}
  /tags/{tag_id}:
    get:
      operationId: getTag
      parameters:
        - {name: tag_id, in: path, required: true, schema: {type: string}}
        - {name: If-None-Match, in: header, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Tag"}}}}
        "304": {description: Not modified.}
  /apps:
    post:
      operationId: createApp
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/App"}}}}
    get:
      operationId: listApps
      parameters: [{name: Range, in: header, schema: {type: string}}]
      responses:
        "200":
          description: Every remaining app.
          headers: {Accept-Ranges: {schema: {type: string}}}
          content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/App"}}}}
        "206":
          description: A page of apps.
          headers:
            Next-Range: {schema: {type: string}}
            Accept-Ranges: {schema: {type: string}}
          content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/App"}}}}
  /widgets:
    post:
      operationId: createWidget
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
    get:
      operationId: listWidgets
      parameters: [{name: Range, in: header, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Widget"}}}}}
components:
  schemas:
    Event:
      type: object
      properties:
        id: {type: string}
        summary: {type: string}
        etag: {type: string, readOnly: true}
        htmlLink: {type: string, readOnly: true}
    Memo:
      type: object
      properties:
        id: {type: string}
        text: {type: string}
        etag: {type: string, readOnly: true}
    Tag:
      type: object
      properties:
        id: {type: string}
        label: {type: string}
        etag: {type: string}
    App:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
        rank: {type: integer}
    Widget:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
    Error:
      type: object
      properties: {code: {type: string}}
"##;

/// Answers each request with the next of its answers, wrapped in the effects envelope
/// when the schema sent asks for one; counts the requests.
struct Scripted {
    answers: Mutex<VecDeque<Value>>,
    calls: Mutex<usize>,
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
        *self.calls.lock().unwrap() += 1;
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
        calls: Mutex::new(0),
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
    async fn send(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
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
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, bytes.to_vec())
    }

    async fn get(&self, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, Value) {
        let (status, headers, bytes) = self.send("GET", uri, None, headers).await;
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, body)
    }

    async fn create(&self, path: &str, body: Value) {
        let (status, _, bytes) = self.send("POST", path, Some(body), &[]).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
    }

    fn calls(&self) -> usize {
        *self.provider.calls.lock().unwrap()
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn names(body: &Value) -> Vec<String> {
    body.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn event(summary: &str, etag: &str) -> Value {
    json!({"id": "ev1", "summary": summary, "etag": etag, "htmlLink": "https://cal.example/ev1"})
}

/// The current etag in `If-None-Match` answers `304`, with no body, the tag in an
/// `ETag` header, and no LLM call: as sent, weak, or in a list. An old one, or a tag
/// that is no member's, answers the event. After an `If-Match` update pairs the
/// `etag`, `*` answers `304` too and the old tag answers the changed event.
#[tokio::test]
async fn if_none_match_with_the_current_etag_answers_304() {
    let h = harness(vec![
        event("Planning", "\"e1\""),
        event("Planning (moved)", "\"e2\""),
    ]);
    h.create("/events", json!({"summary": "Planning"})).await;
    assert_eq!(h.calls(), 1);

    for sent in ["\"e1\"", "W/\"e1\"", "\"x\", \"e1\""] {
        let (status, headers, bytes) = h
            .send("GET", "/events/ev1", None, &[("If-None-Match", sent)])
            .await;
        assert_eq!(status, StatusCode::NOT_MODIFIED, "{sent}");
        assert!(bytes.is_empty(), "a 304 has no body");
        assert_eq!(header(&headers, "etag"), Some("\"e1\""), "{sent}");
    }
    assert_eq!(h.calls(), 1, "no LLM call for a 304");

    let (status, _, body) = h.get("/events/ev1", &[("If-None-Match", "\"e0\"")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["summary"], "Planning");
    // A tag that is the value of a member other than the etag is no tie either.
    let (status, _, _) = h
        .get("/events/ev1", &[("If-None-Match", "\"Planning\"")])
        .await;
    assert_eq!(status, StatusCode::OK);
    // `*` with two members that may be the etag (`etag`, `htmlLink`): no tie yet.
    let (status, _, _) = h.get("/events/ev1", &[("If-None-Match", "*")]).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, bytes) = h
        .send(
            "PATCH",
            "/events/ev1",
            Some(json!({"summary": "Planning (moved)"})),
            &[("If-Match", "\"e1\"")],
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let (status, _, body) = h.get("/events/ev1", &[("If-None-Match", "\"e1\"")]).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the old etag answers the changed event"
    );
    assert_eq!(body["etag"], "\"e2\"");
    let (status, headers, _) = h.get("/events/ev1", &[("If-None-Match", "*")]).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED, "the pairing ties `*`");
    assert_eq!(header(&headers, "etag"), Some("\"e2\""));
}

/// No `304` for a record nothing stores, a read that declares none, or an `etag` the
/// client sets.
#[tokio::test]
async fn no_304_without_a_stored_record_a_declared_304_or_a_server_etag() {
    let h = harness(vec![
        json!({"id": "m1", "text": "hi", "etag": "\"m1\""}),
        json!({"id": "t1", "label": "red", "etag": "\"t1\""}),
        json!({"id": "ev9", "summary": "Elsewhere", "etag": "\"e9\""}),
        json!({"id": "ev8", "summary": "Other", "etag": "\"e8\""}),
    ]);
    h.create("/memos", json!({"text": "hi"})).await;
    h.create("/tags", json!({"label": "red"})).await;

    let (status, _, _) = h.get("/memos/m1", &[("If-None-Match", "\"m1\"")]).await;
    assert_eq!(status, StatusCode::OK, "the read declares no 304");
    let (status, _, _) = h.get("/tags/t1", &[("If-None-Match", "\"t1\"")]).await;
    assert_eq!(status, StatusCode::OK, "a tag's etag is the client's");
    let (status, _, _) = h.get("/events/ev9", &[("If-None-Match", "\"e9\"")]).await;
    assert_ne!(status, StatusCode::NOT_MODIFIED, "nothing is stored");
    let (status, _, _) = h.get("/events/ev8", &[("If-None-Match", "*")]).await;
    assert_ne!(status, StatusCode::NOT_MODIFIED, "nothing is stored");
    // The read stored what it answered: that record is current now.
    let (status, _, _) = h.get("/events/ev9", &[("If-None-Match", "\"e9\"")]).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
}

/// Three stored apps: a bounded range of one answers `206` with the first and a
/// `Next-Range` that reaches the next, then the last answers `200` with no
/// `Next-Range`. A descending range reverses them; numbers sort as numbers. An item the
/// model made up outside the bounds stays out.
#[tokio::test]
async fn a_range_pages_the_stored_records() {
    let h = harness(vec![
        json!({"id": "a1", "name": "alpha", "rank": 10}),
        json!({"id": "a2", "name": "beta", "rank": 9}),
        json!({"id": "a3", "name": "gamma", "rank": 100}),
        json!([{"id": "zz", "name": "omega"}]),
        json!([]),
        json!([]),
        json!([]),
        json!([]),
    ]);
    h.create("/apps", json!({"name": "alpha"})).await;
    h.create("/apps", json!({"name": "beta"})).await;
    h.create("/apps", json!({"name": "gamma"})).await;

    let (status, headers, body) = h
        .get("/apps", &[("Range", "name alpha..gamma; max=1;")])
        .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT, "{body}");
    assert_eq!(names(&body), ["alpha"]);
    let next = header(&headers, "next-range").unwrap().to_string();
    assert_eq!(next, "name ]alpha..gamma; max=1;");

    let (status, headers, body) = h.get("/apps", &[("Range", &next)]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT, "{body}");
    assert_eq!(names(&body), ["beta"]);
    let next = header(&headers, "next-range").unwrap().to_string();

    let (status, headers, body) = h.get("/apps", &[("Range", &next)]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names(&body), ["gamma"]);
    assert_eq!(header(&headers, "next-range"), None);

    let (status, _, body) = h
        .get("/apps", &[("Range", "name ..; order=desc,max=10;")])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The item the model gave the first page is the session's since that list seeded it.
    assert_eq!(names(&body), ["omega", "gamma", "beta", "alpha"]);

    let (status, _, body) = h.get("/apps", &[("Range", "rank ..; max=10")]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // `omega` holds no rank: it is in no range of ranks.
    assert_eq!(names(&body), ["beta", "alpha", "gamma"], "9 < 10 < 100");
}

/// No `Range` header, one that does not read as a range, a field the items do not
/// declare, or a list declaring no `206`: the list is the model's, as before.
#[tokio::test]
async fn a_list_without_a_readable_range_is_as_answered() {
    let answer = json!([{"id": "a2", "name": "beta"}, {"id": "a1", "name": "alpha"}]);
    let h = harness(vec![
        json!({"id": "a1", "name": "alpha"}),
        json!({"id": "a2", "name": "beta"}),
        answer.clone(),
        answer.clone(),
        answer.clone(),
        json!({"id": "w1", "name": "one"}),
        json!({"id": "w2", "name": "two"}),
        json!([{"id": "w2", "name": "two"}, {"id": "w1", "name": "one"}]),
    ]);
    h.create("/apps", json!({"name": "alpha"})).await;
    h.create("/apps", json!({"name": "beta"})).await;

    for sent in [None, Some("bytes=0-499"), Some("color ..; max=1")] {
        let headers: Vec<(&str, &str)> = sent.map(|v| ("Range", v)).into_iter().collect();
        let (status, response, body) = h.get("/apps", &headers).await;
        assert_eq!(status, StatusCode::OK, "{sent:?}: {body}");
        assert_eq!(names(&body), ["beta", "alpha"], "{sent:?}");
        assert_eq!(header(&response, "next-range"), None);
    }

    h.create("/widgets", json!({"name": "one"})).await;
    h.create("/widgets", json!({"name": "two"})).await;
    let (status, _, body) = h.get("/widgets", &[("Range", "name ..; max=1")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(names(&body), ["two", "one"], "no 206 declared: no slot");
}
