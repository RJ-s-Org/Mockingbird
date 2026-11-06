//! Error bodies the mock makes itself: a request that fails validation, and a read of
//! a record the session does not hold. Their members come from the call and the
//! contract instead of the faker: the violated field goes into a path-like member and
//! the reasons into the message-like ones, a code-like member is written once by the
//! LLM for each operation, status and cause, and a missing record answers the status
//! the operation declares for it. Synthetic contracts only.
#![cfg(test)]
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use mb_engine::{Engine, EngineConfig};
use mb_ext::{CallInfo, QuotaGuard, Rejection, Tenancy};
use mb_import::compile;
use mb_llm::{Capabilities, ChatRequest, ChatResponse, LlmError, Provider, ScriptedProvider};
use mb_protocols::{
    DataPlaneAuth, DataPlaneAuthMode, DataPlaneKeys, MockRuntime, RateLimiter, UnknownIds,
    http_rest,
};
use mb_state::MemoryStore;
use serde_json::{Value, json};
use tower::ServiceExt;

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Shelf, version: "1"}
paths:
  /widgets:
    post:
      operationId: createWidget
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewItem"}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "400": {description: Invalid., content: {application/json: {schema: {$ref: "#/components/schemas/Listed"}}}}
  /widgets/{widget_id}:
    get:
      operationId: getWidget
      parameters: [{name: widget_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "401": {description: No key., content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
        "422": {description: Refused., content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
  /gadgets:
    post:
      operationId: createGadget
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewItem"}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "422": {description: Invalid., content: {application/json: {schema: {$ref: "#/components/schemas/Pointed"}}}}
  /gizmos:
    get:
      operationId: listGizmos
      parameters: [{name: limit, in: query, schema: {type: integer, maximum: 10}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Item"}}}}}
        "400": {description: Invalid., content: {application/json: {schema: {$ref: "#/components/schemas/Sourced"}}}}
    post:
      operationId: createGizmo
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewItem"}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "400": {description: Invalid., content: {application/json: {schema: {$ref: "#/components/schemas/Typed"}}}}
  /gizmos/{gizmo_id}:
    get:
      operationId: getGizmo
      parameters: [{name: gizmo_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "404": {description: Not found., content: {application/json: {schema: {$ref: "#/components/schemas/Typed"}}}}
  /doohickeys:
    post:
      operationId: createDoohickey
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewItem"}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "400":
          description: Invalid.
          content:
            application/json:
              schema: {type: array, minItems: 1, items: {type: object, required: [code, message], properties: {code: {type: string}, message: {type: string}}}}
  /notes:
    post:
      operationId: createNote
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewItem"}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "400": {description: Invalid., content: {application/json: {schema: {$ref: "#/components/schemas/JsonApi"}}}}
  /notes/{note_id}:
    get:
      operationId: getNote
      parameters: [{name: note_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "404": {description: Not found., content: {application/json: {schema: {$ref: "#/components/schemas/JsonApi"}}}}
  /memos:
    post:
      operationId: createMemo
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewItem"}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "400": {description: Invalid., content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
components:
  schemas:
    JsonApi:
      type: object
      required: [errors]
      properties:
        errors:
          type: array
          minItems: 1
          items:
            type: object
            required: [status, code, detail]
            properties:
              status: {type: string, examples: ["422"]}
              code: {type: string}
              detail: {type: string}
              source: {type: object, properties: {pointer: {type: string}}}
    Problem:
      type: object
      required: [status, code, detail]
      properties:
        status: {type: integer}
        code: {type: string}
        detail: {type: string}
    NewItem:
      type: object
      required: [name]
      properties:
        name: {type: string, minLength: 1}
        size: {type: integer}
    Item:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
    Listed:
      type: object
      required: [message, errors]
      properties:
        message: {type: string}
        errors:
          type: array
          minItems: 1
          items:
            type: object
            required: [path, message]
            properties:
              path: {type: string}
              message: {type: string}
    Pointed:
      type: object
      required: [errors]
      properties:
        errors:
          type: array
          minItems: 1
          items:
            type: object
            required: [source, detail]
            properties:
              source: {type: object, required: [pointer], properties: {pointer: {type: string, default: /data/attributes/title}}}
              detail: {type: string}
    Sourced:
      type: object
      required: [errors]
      properties:
        errors:
          type: array
          minItems: 1
          items:
            type: object
            required: [source, detail]
            properties:
              source:
                type: object
                required: [pointer, parameter]
                properties:
                  pointer: {type: string}
                  parameter: {type: string}
              detail: {type: string}
    Coded:
      type: object
      required: [ErrorCode, Message]
      properties:
        ErrorCode: {type: integer}
        Message: {type: string}
    Typed:
      type: object
      required: [error]
      properties:
        error:
          type: object
          required: [type, messages]
          properties:
            type: {type: string}
            messages: {type: array, minItems: 1, items: {type: string}}
"##;

struct Harness {
    router: axum::Router,
    provider: Option<Arc<ScriptedProvider>>,
}

fn harness(script: Option<Vec<Value>>) -> Harness {
    harness_with(script, |_| {})
}

/// [`harness`] with the runtime set up by `configure` before it serves.
fn harness_with(script: Option<Vec<Value>>, configure: impl FnOnce(&mut MockRuntime)) -> Harness {
    let provider = script.map(scripted);
    let dyn_provider = provider.clone().map(|p| p as Arc<dyn Provider>);
    harness_of(dyn_provider, provider, configure)
}

fn scripted(script: Vec<Value>) -> Arc<ScriptedProvider> {
    Arc::new(ScriptedProvider::new(
        script.into_iter().map(|v| Ok(v.to_string())).collect(),
    ))
}

fn harness_of(
    dyn_provider: Option<Arc<dyn Provider>>,
    provider: Option<Arc<ScriptedProvider>>,
    configure: impl FnOnce(&mut MockRuntime),
) -> Harness {
    let mut rt = MockRuntime::new(
        "shelf",
        Arc::new(compile(CONTRACT.as_bytes()).unwrap()),
        Arc::new(Engine::new(dyn_provider, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    rt.unknown_ids = UnknownIds::NotFound;
    configure(&mut rt);
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
    }
}

fn post_json(uri: &str, body: &str) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

async fn send(h: &Harness, req: Request<Body>) -> Response {
    h.router.clone().oneshot(req).await.unwrap()
}

async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// A path-like member of an item of the error's array names the violated field, and
/// the item's message says why: for a value that breaks a keyword and for a missing
/// required member alike.
#[tokio::test]
async fn a_refused_body_names_the_violated_field() {
    let h = harness(None);
    let resp = send(&h, post_json("/widgets", r#"{"name": ""}"#)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["errors"][0]["path"], "name", "{body}");
    let message = body["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("minLength"), "{body}");

    let resp = send(&h, post_json("/widgets", r#"{"size": 3}"#)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["errors"][0]["path"], "name", "{body}");
}

/// A member named as a pointer takes the violated field as a JSON pointer, even when
/// the contract shows one for it (it starts with `/`, as a request URI does).
#[tokio::test]
async fn a_pointer_member_takes_a_json_pointer() {
    let h = harness(None);
    let resp = send(&h, post_json("/gadgets", r#"{"name": 7}"#)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["errors"][0]["source"]["pointer"], "/name", "{body}");
    let detail = body["errors"][0]["detail"].as_str().unwrap();
    assert!(detail.contains("type"), "{body}");
}

/// Each violation has its own item of the error array, which grows past the one the
/// faker drew.
#[tokio::test]
async fn each_violation_has_its_own_error_item() {
    let h = harness(None);
    let resp = send(&h, post_json("/gadgets", r#"{"name": 7, "size": "big"}"#)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    let errors = body["errors"].as_array().unwrap();
    let mut pointers: Vec<&str> = errors
        .iter()
        .map(|e| e["source"]["pointer"].as_str().unwrap())
        .collect();
    pointers.sort_unstable();
    assert_eq!(pointers, ["/name", "/size"], "{body}");
    for e in errors {
        assert!(e["detail"].as_str().unwrap().contains("type"), "{body}");
    }
}

/// A code-like member is the LLM's, asked once for the operation, status and cause;
/// the reasons still go into the message array, the call's own.
#[tokio::test]
async fn a_code_member_is_written_once_by_the_llm() {
    let written = json!({"error": {"type": "invalid_parameters", "messages": ["bad"]}});
    let h = harness(Some(vec![written]));
    for body in [r#"{"name": ""}"#, r#"{"size": 1}"#] {
        let resp = send(&h, post_json("/gizmos", body)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["type"], "invalid_parameters", "{body}");
        let first = body["error"]["messages"][0].as_str().unwrap();
        assert!(first.contains("name"), "{body}");
    }
    let requests = h.provider.as_ref().unwrap().requests();
    assert_eq!(requests.len(), 1);
    let user = &requests[0].messages.last().unwrap().content;
    assert!(user.contains("400"), "{user}");
}

/// Offline, a code-like member keeps the faker's value: no LLM is asked.
#[tokio::test]
async fn offline_a_code_member_is_the_fakers() {
    let h = harness(None);
    let resp = send(&h, post_json("/gizmos", r#"{"name": ""}"#)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(body["error"]["type"].is_string(), "{body}");
}

/// A read of a record the session does not hold answers the declared 404, its
/// messages saying what is missing, and its code the LLM's.
#[tokio::test]
async fn a_missing_record_answers_the_declared_404() {
    let written = json!({"error": {"type": "not_found", "messages": ["gone"]}});
    let h = harness(Some(vec![written]));
    for id in ["g1", "g2"] {
        let resp = send(&h, get(&format!("/gizmos/{id}"))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["type"], "not_found", "{body}");
        let first = body["error"]["messages"][0].as_str().unwrap();
        assert!(first.contains(id), "{body}");
    }
    assert_eq!(h.provider.as_ref().unwrap().requests().len(), 1);
}

/// With no 404 declared, a missing record answers the operation's one declared 4xx
/// that is not about who asks (a 401 is not): its code the LLM's, its message the
/// mock's.
#[tokio::test]
async fn a_missing_record_answers_the_one_declared_4xx() {
    let written = json!({"ErrorCode": 1101, "Message": "x"});
    let h = harness(Some(vec![written]));
    let resp = send(&h, get("/widgets/w9")).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["ErrorCode"], 1101, "{body}");
    assert!(body["Message"].as_str().unwrap().contains("w9"), "{body}");

    // Offline too, with the faker's code.
    let h = harness(None);
    let resp = send(&h, get("/widgets/w9")).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert!(body["Message"].as_str().unwrap().contains("w9"), "{body}");
}

/// A query parameter's violation names the parameter, never a pointer into the
/// request document: JSON:API's `source.pointer` is about the body.
#[tokio::test]
async fn a_query_violation_names_the_parameter_not_a_pointer() {
    let h = harness(None);
    let resp = send(&h, get("/gizmos?limit=99")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["errors"][0]["source"]["parameter"], "limit", "{body}");
    assert_ne!(body["errors"][0]["source"]["pointer"], "/limit", "{body}");
}

/// Refuses every call.
struct Exhausted;

#[async_trait]
impl QuotaGuard for Exhausted {
    async fn admit(&self, _t: &Tenancy, _call: &CallInfo<'_>) -> Result<(), Rejection> {
        Err(Rejection::new(429, "Too Many Requests", "quota exhausted"))
    }
}

/// Admits every call, counting them.
#[derive(Default)]
struct Counted(AtomicUsize);

#[async_trait]
impl QuotaGuard for Counted {
    async fn admit(&self, _t: &Tenancy, _call: &CallInfo<'_>) -> Result<(), Rejection> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A caller over its quota still gets the declared 400, with the faker's code: the
/// LLM is not asked for it.
#[tokio::test]
async fn over_quota_an_invalid_request_is_the_fakers() {
    let written = json!({"error": {"type": "invalid_parameters", "messages": ["bad"]}});
    let h = harness_with(Some(vec![written]), |rt| rt.ext.quota = Arc::new(Exhausted));
    let resp = send(&h, post_json("/gizmos", r#"{"name": ""}"#)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_ne!(body["error"]["type"], "invalid_parameters", "{body}");
    assert!(body["error"]["type"].is_string(), "{body}");
    assert!(h.provider.as_ref().unwrap().requests().is_empty());
}

/// Over the mock's rate limit, likewise.
#[tokio::test]
async fn over_the_rate_limit_an_invalid_request_is_the_fakers() {
    let written = json!({"error": {"type": "invalid_parameters", "messages": ["bad"]}});
    let limiter = Arc::new(RateLimiter::new(0.0, 1));
    assert!(limiter.try_acquire().is_ok());
    let h = harness_with(Some(vec![written]), |rt| rt.limits.rate = Some(limiter));
    let resp = send(&h, post_json("/gizmos", r#"{"name": ""}"#)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_ne!(body["error"]["type"], "invalid_parameters", "{body}");
    assert!(h.provider.as_ref().unwrap().requests().is_empty());
}

/// The quota is admitted for the generation, not for a body already written.
#[tokio::test]
async fn the_quota_counts_the_generation_once() {
    let written = json!({"error": {"type": "invalid_parameters", "messages": ["bad"]}});
    let quota = Arc::new(Counted::default());
    let guard = Arc::clone(&quota);
    let h = harness_with(Some(vec![written]), |rt| rt.ext.quota = guard);
    for _ in 0..3 {
        let resp = send(&h, post_json("/gizmos", r#"{"name": ""}"#)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"]["type"], "invalid_parameters");
    }
    assert_eq!(quota.0.load(Ordering::SeqCst), 1);
    assert_eq!(h.provider.as_ref().unwrap().requests().len(), 1);
}

/// Answers as its inner provider does, after a pause.
struct Slow(Arc<ScriptedProvider>);

#[async_trait]
impl Provider for Slow {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.0.complete(req).await
    }
}

/// Two invalid requests at once ask the LLM once between them.
#[tokio::test]
async fn concurrent_misses_ask_the_llm_once() {
    let written = json!({"error": {"type": "invalid_parameters", "messages": ["bad"]}});
    let inner = scripted(vec![written.clone(), written]);
    let slow: Arc<dyn Provider> = Arc::new(Slow(Arc::clone(&inner)));
    let h = harness_of(Some(slow), Some(inner), |_| {});
    let (a, b) = tokio::join!(
        send(&h, post_json("/gizmos", r#"{"name": ""}"#)),
        send(&h, post_json("/gizmos", r#"{"size": 1}"#)),
    );
    assert_eq!(a.status(), StatusCode::BAD_REQUEST);
    assert_eq!(b.status(), StatusCode::BAD_REQUEST);
    assert_eq!(h.provider.as_ref().unwrap().requests().len(), 1);
}

/// Accepts the key `good`, counting the checks.
#[derive(Default)]
struct Keys(AtomicUsize);

#[async_trait]
impl DataPlaneKeys for Keys {
    async fn verify(&self, _workspace: &str, key: &str) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        key == "good"
    }
}

fn keyed(keys: Arc<Keys>) -> impl FnOnce(&mut MockRuntime) {
    move |rt| {
        rt.set_data_plane_auth(DataPlaneAuth {
            mode: DataPlaneAuthMode::ApiKey,
            header: "x-api-key".into(),
        });
        rt.data_plane_keys = Some(keys);
    }
}

fn post_keyed(uri: &str, body: &str, key: Option<&str>) -> Request<Body> {
    let mut req = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        req = req.header("x-api-key", key);
    }
    req.body(Body::from(body.to_string())).unwrap()
}

/// An invalid request whose credentials are missing or wrong answers the declared
/// 400, not a 401, with the faker's code: the LLM is not asked.
#[tokio::test]
async fn without_good_credentials_an_invalid_request_is_the_fakers_400() {
    let written = json!({"error": {"type": "invalid_parameters", "messages": ["bad"]}});
    let keys = Arc::new(Keys::default());
    let h = harness_with(Some(vec![written]), keyed(Arc::clone(&keys)));
    for key in [None, Some("wrong")] {
        let resp = send(&h, post_keyed("/gizmos", r#"{"name": ""}"#, key)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{key:?}");
        let body = body_json(resp).await;
        assert_ne!(body["error"]["type"], "invalid_parameters", "{body}");
        assert!(body["error"]["type"].is_string(), "{body}");
    }
    assert!(h.provider.as_ref().unwrap().requests().is_empty());
    // With the good key, the LLM writes it.
    let resp = send(&h, post_keyed("/gizmos", r#"{"name": ""}"#, Some("good"))).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"]["type"], "invalid_parameters");
    assert_eq!(h.provider.as_ref().unwrap().requests().len(), 1);
}

/// A declared error with no open code member asks no LLM and checks no credentials
/// on the validation path.
#[tokio::test]
async fn without_a_code_member_no_llm_and_no_auth() {
    let keys = Arc::new(Keys::default());
    let h = harness_with(Some(Vec::new()), keyed(Arc::clone(&keys)));
    let resp = send(&h, post_keyed("/widgets", r#"{"name": ""}"#, Some("good"))).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["errors"][0]["path"], "name");
    assert!(h.provider.as_ref().unwrap().requests().is_empty());
    assert_eq!(keys.0.load(Ordering::SeqCst), 0);
}

/// Offline, an invalid request is the faker's: no quota is admitted, no rate-limit
/// token is taken and no credentials are checked, as the engine writes nothing.
#[tokio::test]
async fn offline_an_invalid_request_admits_nothing() {
    let quota = Arc::new(Counted::default());
    let guard = Arc::clone(&quota);
    let limiter = Arc::new(RateLimiter::new(0.0, 2));
    let limit = Arc::clone(&limiter);
    let keys = Arc::new(Keys::default());
    let keyed = keyed(Arc::clone(&keys));
    let h = harness_with(None, move |rt| {
        keyed(rt);
        rt.ext.quota = guard;
        rt.limits.rate = Some(limit);
    });
    for _ in 0..3 {
        let resp = send(&h, post_keyed("/gizmos", r#"{"name": ""}"#, Some("good"))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(body_json(resp).await["error"]["type"].is_string());
    }
    assert_eq!(quota.0.load(Ordering::SeqCst), 0);
    assert_eq!(keys.0.load(Ordering::SeqCst), 0);
    assert!(limiter.try_acquire().is_ok());
    assert!(limiter.try_acquire().is_ok());
}

/// An error body the engine cannot ask for (an array root, which a JSON-mode
/// provider is asked to wrap) admits nothing either, and asks no LLM.
#[tokio::test]
async fn an_unwritable_error_body_admits_nothing() {
    let quota = Arc::new(Counted::default());
    let guard = Arc::clone(&quota);
    let limiter = Arc::new(RateLimiter::new(0.0, 1));
    let limit = Arc::clone(&limiter);
    let inner = Arc::new(
        ScriptedProvider::new(Vec::new()).with_capabilities(Capabilities {
            json_schema: false,
            json_mode: true,
        }),
    );
    let provider: Arc<dyn Provider> = inner.clone();
    let h = harness_of(Some(provider), Some(inner), move |rt| {
        rt.ext.quota = guard;
        rt.limits.rate = Some(limit);
    });
    for _ in 0..2 {
        let resp = send(&h, post_json("/doohickeys", r#"{"name": ""}"#)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(body_json(resp).await[0]["code"].is_string());
    }
    assert_eq!(quota.0.load(Ordering::SeqCst), 0);
    assert!(limiter.try_acquire().is_ok());
    assert!(h.provider.as_ref().unwrap().requests().is_empty());
}

/// A JSON:API error body shared by a `400` and a `404`, its `status` shown as the
/// text `"422"`: the body the model writes for an invalid request states `"400"`,
/// whatever status it wrote, and the read of a missing record states `"404"`.
#[tokio::test]
async fn a_written_json_api_error_states_its_own_status() {
    let written = json!({"errors": [{"status": "422", "code": "invalid_note", "detail": "x"}]});
    let h = harness(Some(vec![written]));
    for body in [r#"{"name": ""}"#, r#"{"size": 1}"#] {
        let resp = send(&h, post_json("/notes", body)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["errors"][0]["code"], "invalid_note", "{body}");
        assert_eq!(body["errors"][0]["status"], "400", "{body}");
    }
    assert_eq!(h.provider.as_ref().unwrap().requests().len(), 1);

    let written = json!({"errors": [{"status": "422", "code": "no_note", "detail": "x"}]});
    let h = harness(Some(vec![written]));
    let resp = send(&h, get("/notes/n1")).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["errors"][0]["code"], "no_note", "{body}");
    assert_eq!(body["errors"][0]["status"], "404", "{body}");
}

/// An integer `status` the model writes into an invalid request's body is the status
/// it is answered with.
#[tokio::test]
async fn a_written_integer_status_is_the_answered_one() {
    let written = json!({"status": 422, "code": "invalid_memo", "detail": "x"});
    let h = harness(Some(vec![written]));
    let resp = send(&h, post_json("/memos", r#"{"name": ""}"#)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "invalid_memo", "{body}");
    assert_eq!(body["status"], 400, "{body}");
}
