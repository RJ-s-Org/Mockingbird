//! Import jobs: the `…/mocks/{mock}/imports` API and the worker that turns
//! jobs into draft contract versions.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use base64::Engine as _;
use mb_admin::{
    AdminState, LlmResolver, UPLOAD_HEADER, router, run_next_import, spawn_import_workers,
};
use mb_auth::{AuthError, Authenticate, Principal, PrincipalKind, Role};
use mb_engine::{Engine, EngineConfig};
use mb_import::smart::ExtractLlm;
use mb_llm::{
    Capabilities, ChatRequest, ChatResponse, HopSettings, LlmError, Provider, ScriptedProvider,
};
use mb_state::{MemoryStore, StateStore};
use mb_store::{AuditFilter, ControlStore, DEFAULT_WORKSPACE, Mock, NewMock, ResourceSource};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// `Bearer viewer|editor` → that role in `default`.
struct TestAuth;

#[async_trait]
impl Authenticate for TestAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Option<Principal>, AuthError> {
        let Some(value) = headers.get(header::AUTHORIZATION) else {
            return Ok(None);
        };
        let name = value
            .to_str()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap_or_default();
        let role = match name {
            "viewer" => Role::Viewer,
            "editor" => Role::Editor,
            _ => return Err(AuthError::Unauthenticated("unknown token".into())),
        };
        Ok(Some(Principal {
            subject: name.into(),
            display_name: name.into(),
            kind: PrincipalKind::User,
            sysadmin: false,
            roles: [(DEFAULT_WORKSPACE.to_string(), role)]
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
        }))
    }
}

/// Hands every mock the same provider (or none).
struct TestLlm(Option<Arc<dyn Provider>>);

#[async_trait]
impl LlmResolver for TestLlm {
    async fn for_import(&self, _mock: &Mock) -> Result<Option<ExtractLlm>, String> {
        Ok(self.0.clone().map(|provider| ExtractLlm {
            provider,
            settings: HopSettings {
                timeout: Duration::from_secs(5),
                temperature: 0.1,
                max_tokens: 1000,
            },
        }))
    }

    async fn preview_engine(&self, _mock: &Mock) -> Result<Arc<Engine>, String> {
        Ok(Arc::new(Engine::new(None, EngineConfig::default())))
    }
}

/// Cancels the mock's newest job through the store, then answers.
struct CancelingProvider {
    store: Arc<dyn ControlStore>,
    mock_id: Uuid,
}

#[async_trait]
impl Provider for CancelingProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: true,
            json_mode: true,
        }
    }

    async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let jobs = self.store.list_import_jobs(self.mock_id, 1).await.unwrap();
        assert!(
            self.store
                .cancel_import_job(jobs[0].id, chrono::Utc::now())
                .await
                .unwrap()
        );
        Ok(ChatResponse {
            content: pets_answer(),
            usage: Default::default(),
        })
    }
}

/// Panics on every call.
struct PanickingProvider;

#[async_trait]
impl Provider for PanickingProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: true,
            json_mode: true,
        }
    }

    async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
        panic!("the provider blew up");
    }
}

/// Never answers; records when a call starts and when its future is dropped.
#[derive(Default)]
struct HangingProvider {
    entered: std::sync::atomic::AtomicBool,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

struct SetOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl Provider for HangingProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: true,
            json_mode: true,
        }
    }

    async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let _guard = SetOnDrop(self.dropped.clone());
        self.entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        std::future::pending().await
    }
}

fn fixture(rel: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel);
    std::fs::read(p).unwrap()
}

fn pets_answer() -> String {
    json!({"operations": [
        {"method": "GET", "path": "/pets", "summary": "List pets", "confidence": 0.9,
         "evidence": "Returns all pets, newest first.",
         "parameters": [{"name": "limit", "in": "query", "schema": {"type": "integer"}}],
         "responses": [{"status": 200, "example": [{"id": 1, "name": "Rex", "tag": "dog"}]}]},
        {"method": "GET", "path": "/pets/{petId}", "confidence": 0.9,
         "evidence": "Returns the pet with the given id",
         "parameters": [{"name": "petId", "in": "path", "schema": {"type": "integer"}}],
         "responses": [{"status": 200, "example": {"id": 1, "name": "Rex", "tag": "dog"}},
                       {"status": 404, "description": "No such pet"}]},
        {"method": "POST", "path": "/pets", "confidence": 0.9,
         "evidence": "Creates a pet. The request body is JSON:",
         "request": {"example": {"name": "Rex", "tag": "dog"}},
         "responses": [{"status": 201, "example": {"id": 3, "name": "Rex", "tag": "dog"}}]}
    ]})
    .to_string()
}

fn md_text() -> String {
    String::from_utf8(fixture("smart/petstore.md")).unwrap()
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.text))
    }

    fn expect(&self, status: StatusCode, what: &str) -> Value {
        assert_eq!(self.status, status, "{what}: {}", self.text);
        if !status.is_success() {
            assert_eq!(
                self.headers[header::CONTENT_TYPE],
                "application/problem+json",
                "{what}"
            );
        }
        self.json()
    }
}

struct Harness {
    st: AdminState,
    app: Router,
}

const BASE: &str = "/api/v1/workspaces/default/mocks";

async fn harness_with(llm: Option<Arc<dyn Provider>>) -> Harness {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut st = AdminState {
        auth: Arc::new(TestAuth),
        ..AdminState::new(store, state)
    };
    if let Some(p) = llm {
        st.llm = Arc::new(TestLlm(Some(p)));
    }
    let h = Harness {
        app: router(st.clone()),
        st,
    };
    h.create_mock("pets").await;
    h
}

async fn harness() -> Harness {
    harness_with(None).await
}

fn scripted(answers: Vec<String>) -> Arc<dyn Provider> {
    Arc::new(ScriptedProvider::new(answers.into_iter().map(Ok).collect()))
}

impl Harness {
    async fn send(&self, req: Request<Body>) -> Resp {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        Resp {
            status,
            headers,
            text: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn call(&self, who: &str, method: Method, uri: &str, body: Option<Value>) -> Resp {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {who}"));
        let body = match body {
            Some(b) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        self.send(req.body(body).unwrap()).await
    }

    async fn create_mock(&self, name: &str) {
        self.call("editor", Method::POST, BASE, Some(json!({ "name": name })))
            .await
            .expect(StatusCode::CREATED, "create mock");
    }

    async fn mock(&self, name: &str) -> Mock {
        self.st
            .store
            .get_mock(DEFAULT_WORKSPACE, name)
            .await
            .unwrap()
            .unwrap()
    }

    /// Posts `(name, filename, content_type, bytes)` parts as multipart.
    async fn multipart(
        &self,
        uri: &str,
        upload_header: bool,
        parts: &[(&str, Option<&str>, &[u8])],
    ) -> Resp {
        let boundary = "mbboundary7MA4YWxkTrZu0gW";
        let mut body = Vec::new();
        for (name, filename, bytes) in parts {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match filename {
                Some(f) => body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n\
                         Content-Type: application/octet-stream\r\n\r\n"
                    )
                    .as_bytes(),
                ),
                None => body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                ),
            }
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let mut req = Request::post(uri)
            .header(header::AUTHORIZATION, "Bearer editor")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            );
        if upload_header {
            req = req.header(UPLOAD_HEADER, "1");
        }
        self.send(req.body(Body::from(body)).unwrap()).await
    }

    async fn upload_har(&self, mock: &str) -> Value {
        let har = fixture("smart/petstore.har");
        self.multipart(
            &format!("{BASE}/{mock}/imports"),
            true,
            &[("file", Some("petstore.har"), &har)],
        )
        .await
        .expect(StatusCode::ACCEPTED, "har upload")
    }

    async fn job(&self, mock: &str, id: &str) -> Value {
        self.call(
            "viewer",
            Method::GET,
            &format!("{BASE}/{mock}/imports/{id}"),
            None,
        )
        .await
        .expect(StatusCode::OK, "get job")
    }

    async fn actions(&self) -> Vec<String> {
        self.st
            .store
            .list_audit(&AuditFilter {
                workspace: Some(DEFAULT_WORKSPACE.into()),
                ..AuditFilter::default()
            })
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.action)
            .collect()
    }

    async fn run(&self) -> bool {
        run_next_import(&self.st).await.unwrap()
    }
}

#[tokio::test]
async fn har_upload_creates_job_and_draft() {
    let h = harness().await;
    let har = fixture("smart/petstore.har");
    let r = h
        .multipart(
            &format!("{BASE}/pets/imports"),
            true,
            &[("file", Some("petstore.har"), &har)],
        )
        .await;
    let view = r.expect(StatusCode::ACCEPTED, "upload");
    let id = view["id"].as_str().unwrap().to_string();
    assert_eq!(view["kind"], "har");
    assert_eq!(view["status"], "queued");
    assert_eq!(view["mock"], "pets");
    assert_eq!(view["source_name"], "petstore.har");
    assert_eq!(view["created_by"], "editor");
    assert_eq!(view["progress"]["phase"], "queued");
    assert_eq!(
        r.headers[header::LOCATION],
        format!("/api/v1/workspaces/default/mocks/pets/imports/{id}")
    );

    assert!(h.run().await);
    let job = h.job("pets", &id).await;
    assert_eq!(job["status"], "done", "{job}");
    assert_eq!(job["result_version"], 1);
    assert!(job["finished_at"].is_string());

    let versions = h
        .call(
            "viewer",
            Method::GET,
            &format!("{BASE}/pets/contracts"),
            None,
        )
        .await
        .expect(StatusCode::OK, "contracts");
    assert_eq!(versions.as_array().unwrap().len(), 1);
    assert_eq!(versions[0]["version"], 1);
    assert_eq!(versions[0]["status"], "draft");
    assert_eq!(versions[0]["created_by"], "editor");
    let stored =
        h.st.store
            .get_contract_version(h.mock("pets").await.id, 1)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(stored.draft_meta.unwrap()["source_kind"], "har");

    let actions = h.actions().await;
    assert!(
        actions.contains(&"import.create".to_string()),
        "{actions:?}"
    );
    assert!(
        actions.contains(&"import.complete".to_string()),
        "{actions:?}"
    );
    let complete =
        h.st.store
            .list_audit(&AuditFilter {
                action: Some("import.complete".into()),
                ..AuditFilter::default()
            })
            .await
            .unwrap();
    assert_eq!(complete[0].actor_kind, "system");
    assert_eq!(complete[0].actor_subject, "editor");
    assert_eq!(complete[0].target, "contract:pets@1");
    let after = complete[0].after.clone().unwrap();
    assert_eq!(after["job"], id.as_str());
    assert!(after["operations"].as_u64().unwrap() > 0);
    assert!(after["warnings"].is_u64());

    assert!(!h.run().await, "nothing left to run");
}

#[tokio::test]
async fn multipart_without_upload_header_is_415() {
    let h = harness().await;
    let har = fixture("smart/petstore.har");
    h.multipart(
        &format!("{BASE}/pets/imports"),
        false,
        &[("file", Some("petstore.har"), &har)],
    )
    .await
    .expect(StatusCode::UNSUPPORTED_MEDIA_TYPE, "no header");
    // Neither JSON nor multipart.
    let r = h
        .send(
            Request::post(format!("{BASE}/pets/imports"))
                .header(header::AUTHORIZATION, "Bearer editor")
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from("hello"))
                .unwrap(),
        )
        .await;
    let p = r.expect(StatusCode::UNSUPPORTED_MEDIA_TYPE, "text/plain");
    assert!(
        p["detail"]
            .as_str()
            .unwrap()
            .contains("application/json, multipart/form-data"),
        "{p}"
    );
}

#[tokio::test]
async fn multipart_parts_are_checked() {
    let h = harness().await;
    let uri = format!("{BASE}/pets/imports");
    h.multipart(&uri, true, &[("kind", None, b"har")])
        .await
        .expect(StatusCode::BAD_REQUEST, "no file part");
    let har = fixture("smart/petstore.har");
    h.multipart(
        &uri,
        true,
        &[("file", Some("x.har"), &har), ("other", None, b"x")],
    )
    .await
    .expect(StatusCode::BAD_REQUEST, "unknown part");
    // An explicit kind wins over detection.
    let view = h
        .multipart(
            &uri,
            true,
            &[
                ("kind", None, b"har"),
                ("file", Some("recording.bin"), &har),
            ],
        )
        .await
        .expect(StatusCode::ACCEPTED, "explicit kind");
    assert_eq!(view["kind"], "har");
}

#[tokio::test]
async fn json_text_import_uses_llm() {
    let h = harness_with(Some(scripted(vec![pets_answer()]))).await;
    let view = h
        .call(
            "editor",
            Method::POST,
            &format!("{BASE}/pets/imports"),
            Some(json!({ "kind": "markdown", "text": md_text(), "filename": "petstore.md" })),
        )
        .await
        .expect(StatusCode::ACCEPTED, "text import");
    assert_eq!(view["kind"], "markdown");
    assert_eq!(view["source_name"], "petstore.md");
    let id = view["id"].as_str().unwrap().to_string();
    assert!(h.run().await);
    let job = h.job("pets", &id).await;
    assert_eq!(job["status"], "done", "{job}");
    let detail = h
        .call(
            "viewer",
            Method::GET,
            &format!("{BASE}/pets/contracts/1"),
            None,
        )
        .await
        .expect(StatusCode::OK, "detail");
    assert_eq!(detail["status"], "draft");
    let mut targets: Vec<&str> = detail["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["target"].as_str().unwrap())
        .collect();
    targets.sort();
    assert_eq!(
        targets,
        vec!["GET /pets", "GET /pets/{petId}", "POST /pets"]
    );
}

#[tokio::test]
async fn json_body_validation() {
    let h = harness_with(Some(scripted(vec![]))).await;
    let uri = format!("{BASE}/pets/imports");
    let bad = |body: Value, what: &'static str| {
        let h = &h;
        let uri = uri.clone();
        async move {
            h.call("editor", Method::POST, &uri, Some(body))
                .await
                .expect(StatusCode::BAD_REQUEST, what)
        }
    };
    let p = bad(json!({ "url": "ftp://x" }), "ftp").await;
    assert!(p["detail"].as_str().unwrap().contains("http"), "{p}");
    bad(
        json!({ "url": "https://x.test/a", "text": "hi" }),
        "url+text",
    )
    .await;
    bad(json!({}), "nothing").await;
    bad(json!({ "text": "hi" }), "text without kind").await;
    bad(json!({ "kind": "pdf", "text": "hi" }), "pdf text").await;
    bad(json!({ "kind": "url", "text": "hi" }), "url text").await;
    bad(
        json!({ "kind": "markdown", "url": "https://x.test/a" }),
        "url with kind",
    )
    .await;
    bad(json!({ "content_base64": "!!!" }), "bad base64").await;
    bad(json!({ "kind": "nope", "text": "hi" }), "unknown kind").await;
    // A URL job.
    let view = h
        .call(
            "editor",
            Method::POST,
            &uri,
            Some(json!({ "url": "https://docs.example.com/api.md" })),
        )
        .await
        .expect(StatusCode::ACCEPTED, "url");
    assert_eq!(view["kind"], "url");
    assert_eq!(view["source_name"], "https://docs.example.com/api.md");
}

#[tokio::test]
async fn doc_import_offline_is_422() {
    let h = harness().await;
    let p = h
        .call(
            "editor",
            Method::POST,
            &format!("{BASE}/pets/imports"),
            Some(json!({ "kind": "markdown", "text": md_text() })),
        )
        .await
        .expect(StatusCode::UNPROCESSABLE_ENTITY, "offline");
    assert!(
        p["detail"]
            .as_str()
            .unwrap()
            .contains("need an LLM profile"),
        "{p}"
    );
    assert!(h.actions().await.iter().all(|a| a != "import.create"));
}

#[tokio::test]
async fn deterministic_contract_redirected() {
    let h = harness().await;
    let content =
        base64::engine::general_purpose::STANDARD.encode(fixture("openapi/orders-3.0.yaml"));
    let p = h
        .call(
            "editor",
            Method::POST,
            &format!("{BASE}/pets/imports"),
            Some(json!({ "content_base64": content })),
        )
        .await
        .expect(StatusCode::UNPROCESSABLE_ENTITY, "openapi");
    let detail = p["detail"].as_str().unwrap();
    assert!(detail.contains("/mocks/pets/contracts"), "{p}");
    // Unknown bytes.
    let content = base64::engine::general_purpose::STANDARD.encode([0u8, 1, 2, 3]);
    let p = h
        .call(
            "editor",
            Method::POST,
            &format!("{BASE}/pets/imports"),
            Some(json!({ "content_base64": content })),
        )
        .await
        .expect(StatusCode::UNPROCESSABLE_ENTITY, "unknown");
    assert!(
        p["detail"].as_str().unwrap().contains("pass \"kind\""),
        "{p}"
    );
}

#[tokio::test]
async fn oversized_upload_413() {
    let h = harness().await;
    let content = base64::engine::general_purpose::STANDARD.encode(vec![b'a'; 21 * 1024 * 1024]);
    h.call(
        "editor",
        Method::POST,
        &format!("{BASE}/pets/imports"),
        Some(json!({ "kind": "har", "content_base64": content })),
    )
    .await
    .expect(StatusCode::PAYLOAD_TOO_LARGE, "oversized");
    // Decoded content over the limit inside an acceptable request body.
    let text = "a".repeat(20 * 1024 * 1024 + 1);
    h.call(
        "editor",
        Method::POST,
        &format!("{BASE}/pets/imports"),
        Some(json!({ "kind": "har", "text": text })),
    )
    .await
    .expect(StatusCode::PAYLOAD_TOO_LARGE, "oversized text");
}

#[tokio::test]
async fn file_mock_import_409() {
    let h = harness().await;
    h.st.store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: "filed".into(),
                route_prefix: None,
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::File,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: Default::default(),
                hosts: vec![],
            },
        )
        .await
        .unwrap();
    let har = fixture("smart/petstore.har");
    let p = h
        .multipart(
            &format!("{BASE}/filed/imports"),
            true,
            &[("file", Some("petstore.har"), &har)],
        )
        .await
        .expect(StatusCode::CONFLICT, "file mock");
    assert_eq!(p["title"], "Managed by config file");
    h.call(
        "editor",
        Method::POST,
        &format!("{BASE}/missing/imports"),
        Some(json!({ "url": "https://x.test/a" })),
    )
    .await
    .expect(StatusCode::NOT_FOUND, "missing mock");
}

#[tokio::test]
async fn viewer_cannot_import_403() {
    let h = harness().await;
    h.call(
        "viewer",
        Method::POST,
        &format!("{BASE}/pets/imports"),
        Some(json!({ "url": "https://x.test/a" })),
    )
    .await
    .expect(StatusCode::FORBIDDEN, "viewer create");
    let view = h.upload_har("pets").await;
    let list = h
        .call("viewer", Method::GET, &format!("{BASE}/pets/imports"), None)
        .await
        .expect(StatusCode::OK, "viewer list");
    assert_eq!(list.as_array().unwrap().len(), 1);
    h.call(
        "viewer",
        Method::DELETE,
        &format!("{BASE}/pets/imports/{}", view["id"].as_str().unwrap()),
        None,
    )
    .await
    .expect(StatusCode::FORBIDDEN, "viewer cancel");
}

#[tokio::test]
async fn cancel_queued_job() {
    let h = harness().await;
    let view = h.upload_har("pets").await;
    let uri = format!("{BASE}/pets/imports/{}", view["id"].as_str().unwrap());
    let canceled = h
        .call("editor", Method::DELETE, &uri, None)
        .await
        .expect(StatusCode::OK, "cancel");
    assert_eq!(canceled["status"], "canceled");
    assert!(!h.run().await);
    let p = h
        .call("editor", Method::DELETE, &uri, None)
        .await
        .expect(StatusCode::CONFLICT, "cancel again");
    assert!(
        p["detail"]
            .as_str()
            .unwrap()
            .contains("has already finished"),
        "{p}"
    );
    assert!(h.actions().await.contains(&"import.cancel".to_string()));
}

#[tokio::test]
async fn canceled_job_stores_no_draft() {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    // Build the harness by hand: the provider needs the store and the mock.
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut st = AdminState {
        auth: Arc::new(TestAuth),
        ..AdminState::new(store.clone(), state)
    };
    let h0 = Harness {
        app: router(st.clone()),
        st: st.clone(),
    };
    h0.create_mock("pets").await;
    let mock = h0.mock("pets").await;
    st.llm = Arc::new(TestLlm(Some(Arc::new(CancelingProvider {
        store,
        mock_id: mock.id,
    }))));
    let h = Harness {
        app: router(st.clone()),
        st,
    };
    let view = h
        .call(
            "editor",
            Method::POST,
            &format!("{BASE}/pets/imports"),
            Some(json!({ "kind": "markdown", "text": md_text() })),
        )
        .await
        .expect(StatusCode::ACCEPTED, "create");
    assert!(h.run().await);
    let job = h.job("pets", view["id"].as_str().unwrap()).await;
    assert_eq!(job["status"], "canceled", "{job}");
    assert!(
        h.st.store
            .list_contract_versions(mock.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn failed_job_reports_error() {
    let h = harness().await;
    let view = h
        .call(
            "editor",
            Method::POST,
            &format!("{BASE}/pets/imports"),
            Some(json!({ "url": "http://127.0.0.1:9/doc.md" })),
        )
        .await
        .expect(StatusCode::ACCEPTED, "create");
    assert!(h.run().await);
    let job = h.job("pets", view["id"].as_str().unwrap()).await;
    assert_eq!(job["status"], "failed", "{job}");
    let error = job["error"].as_str().unwrap();
    assert!(error.contains("destination not allowed"), "{job}");
    assert!(!error.contains("127.0.0.1"), "{job}");
}

#[tokio::test]
async fn url_import_stores_and_audits_redacted_url() {
    let h = harness().await;
    let url = "https://docs.example.com/api.md?X-Amz-Signature=s3cret&expires=9#t0ken";
    let view = h
        .call(
            "editor",
            Method::POST,
            &format!("{BASE}/pets/imports"),
            Some(json!({ "url": url })),
        )
        .await
        .expect(StatusCode::ACCEPTED, "url import");
    let redacted = "https://docs.example.com/api.md?X-Amz-Signature=REDACTED&expires=REDACTED";
    assert_eq!(view["source_name"], redacted);
    let id: Uuid = view["id"].as_str().unwrap().parse().unwrap();
    let stored = h.st.store.get_import_job(id).await.unwrap().unwrap();
    assert_eq!(stored.source_name.as_deref(), Some(redacted));
    // The fetch still uses the real URL.
    let input = h.st.store.import_job_input(id).await.unwrap().unwrap();
    assert!(String::from_utf8(input).unwrap().contains("s3cret"));
    let audit =
        h.st.store
            .list_audit(&AuditFilter {
                action: Some("import.create".into()),
                ..AuditFilter::default()
            })
            .await
            .unwrap();
    let after = audit[0].after.clone().unwrap().to_string();
    assert!(after.contains("REDACTED"), "{after}");
    assert!(
        !after.contains("s3cret") && !after.contains("t0ken"),
        "{after}"
    );
}

/// The control stores to run backend-parity tests on: SQLite, and Postgres
/// in a fresh schema when `MB_TEST_POSTGRES_URL` is set.
async fn parity_stores() -> Vec<(&'static str, Arc<dyn ControlStore>)> {
    let mut stores: Vec<(&'static str, Arc<dyn ControlStore>)> = vec![(
        "sqlite",
        mb_store::connect("sqlite::memory:").await.unwrap(),
    )];
    if let Ok(base) = std::env::var("MB_TEST_POSTGRES_URL") {
        use sqlx::Connection;
        let schema = format!("mb_admin_{}", Uuid::new_v4().simple());
        let mut conn = sqlx::postgres::PgConnection::connect(&base).await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let sep = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{sep}options=-c%20search_path%3D{schema}");
        stores.push(("postgres", mb_store::connect(&url).await.unwrap()));
    } else {
        println!("postgres skipped: MB_TEST_POSTGRES_URL not set");
    }
    stores
}

#[tokio::test]
async fn control_characters_in_names_are_stripped_on_every_backend() {
    for (backend, store) in parity_stores().await {
        let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
        let st = AdminState {
            auth: Arc::new(TestAuth),
            ..AdminState::new(store, state)
        };
        let h = Harness {
            app: router(st.clone()),
            st,
        };
        h.create_mock("pets").await;
        let har = String::from_utf8(fixture("smart/petstore.har")).unwrap();
        let view = h
            .call(
                "editor",
                Method::POST,
                &format!("{BASE}/pets/imports"),
                Some(
                    json!({ "kind": "har", "text": har, "filename": "pet\u{0}store\n.har\u{7f}" }),
                ),
            )
            .await
            .expect(StatusCode::ACCEPTED, backend);
        assert_eq!(view["source_name"], "petstore.har", "{backend}");
        // Only control characters: no name at all.
        let view = h
            .call(
                "editor",
                Method::POST,
                &format!("{BASE}/pets/imports"),
                Some(json!({ "kind": "har", "text": har, "filename": "\u{0}\t" })),
            )
            .await
            .expect(StatusCode::ACCEPTED, backend);
        assert!(view["source_name"].is_null(), "{backend}: {view}");
        // A URL is stored in its parsed (percent-encoded) form.
        let view = h
            .call(
                "editor",
                Method::POST,
                &format!("{BASE}/pets/imports"),
                Some(json!({ "url": "https://docs.example.com/a\u{0}b" })),
            )
            .await
            .expect(StatusCode::ACCEPTED, backend);
        assert_eq!(
            view["source_name"], "https://docs.example.com/a%00b",
            "{backend}"
        );
        let jobs = h
            .call("viewer", Method::GET, &format!("{BASE}/pets/imports"), None)
            .await
            .expect(StatusCode::OK, backend);
        assert_eq!(jobs.as_array().unwrap().len(), 3, "{backend}");
    }
}

#[tokio::test]
async fn panicking_pipeline_fails_job() {
    let h = harness_with(Some(Arc::new(PanickingProvider))).await;
    let view = h
        .call(
            "editor",
            Method::POST,
            &format!("{BASE}/pets/imports"),
            Some(json!({ "kind": "markdown", "text": md_text() })),
        )
        .await
        .expect(StatusCode::ACCEPTED, "create");
    assert!(run_next_import(&h.st).await.is_ok());
    let job = h.job("pets", view["id"].as_str().unwrap()).await;
    assert_eq!(job["status"], "failed", "{job}");
    assert!(
        job["error"].as_str().unwrap().contains("unexpectedly"),
        "{job}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workers_pick_up_jobs() {
    let mut h = harness().await;
    h.st.imports.settings.poll_interval = Duration::from_millis(50);
    h.app = router(h.st.clone());
    let _workers = spawn_import_workers(&h.st);
    let view = h.upload_har("pets").await;
    let id = view["id"].as_str().unwrap().to_string();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let job = h.job("pets", &id).await;
        if job["status"] == "done" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "job not done in time: {job}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn job_of_other_mock_404() {
    let h = harness().await;
    h.create_mock("other").await;
    let view = h.upload_har("pets").await;
    let id = view["id"].as_str().unwrap();
    h.call(
        "viewer",
        Method::GET,
        &format!("{BASE}/other/imports/{id}"),
        None,
    )
    .await
    .expect(StatusCode::NOT_FOUND, "other get");
    h.call(
        "editor",
        Method::DELETE,
        &format!("{BASE}/other/imports/{id}"),
        None,
    )
    .await
    .expect(StatusCode::NOT_FOUND, "other cancel");
    h.call(
        "viewer",
        Method::GET,
        &format!("{BASE}/pets/imports/{}", Uuid::new_v4()),
        None,
    )
    .await
    .expect(StatusCode::NOT_FOUND, "unknown job");
    // The P2 alias path serves the same job.
    let alias = h
        .call(
            "viewer",
            Method::GET,
            &format!("/api/v1/mocks/pets/imports/{id}"),
            None,
        )
        .await
        .expect(StatusCode::OK, "alias");
    assert_eq!(alias["id"], id);
}

#[tokio::test]
async fn jobs_listed_newest_first() {
    let h = harness().await;
    let first = h.upload_har("pets").await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let second = h.upload_har("pets").await;
    let list = h
        .call("viewer", Method::GET, &format!("{BASE}/pets/imports"), None)
        .await
        .expect(StatusCode::OK, "list");
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![
            second["id"].as_str().unwrap(),
            first["id"].as_str().unwrap()
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_workers_aborts_a_running_pipeline() {
    use std::sync::atomic::Ordering;
    let provider = Arc::new(HangingProvider::default());
    let mut h = harness_with(Some(provider.clone())).await;
    h.st.imports.settings.poll_interval = Duration::from_millis(20);
    h.app = router(h.st.clone());
    let workers = spawn_import_workers(&h.st);
    h.call(
        "editor",
        Method::POST,
        &format!("{BASE}/pets/imports"),
        Some(json!({ "kind": "markdown", "text": md_text() })),
    )
    .await
    .expect(StatusCode::ACCEPTED, "create");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !provider.entered.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no LLM call started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(workers);
    // Well before the call's own 5 s timeout would drop it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while !provider.dropped.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pipeline outlived its worker"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn active_jobs_per_mock_are_capped() {
    let mut h = harness().await;
    h.st.imports.settings.max_active_jobs_per_mock = 2;
    h.app = router(h.st.clone());
    h.create_mock("other").await;
    let first = h.upload_har("pets").await;
    h.upload_har("pets").await;
    let har = fixture("smart/petstore.har");
    let r = h
        .multipart(
            &format!("{BASE}/pets/imports"),
            true,
            &[("file", Some("petstore.har"), &har)],
        )
        .await;
    let p = r.expect(StatusCode::TOO_MANY_REQUESTS, "third job");
    assert!(p["detail"].as_str().unwrap().contains('2'), "{p}");
    // Other mocks have their own allowance.
    h.upload_har("other").await;
    // A running job still counts; a finished one does not.
    let claimed =
        h.st.store
            .claim_import_job(chrono::Utc::now())
            .await
            .unwrap();
    assert_eq!(
        claimed.unwrap().id.to_string(),
        first["id"].as_str().unwrap()
    );
    h.call(
        "editor",
        Method::POST,
        &format!("{BASE}/pets/imports"),
        Some(json!({ "url": "https://x.test/a" })),
    )
    .await
    .expect(StatusCode::TOO_MANY_REQUESTS, "still full");
    h.call(
        "editor",
        Method::DELETE,
        &format!("{BASE}/pets/imports/{}", first["id"].as_str().unwrap()),
        None,
    )
    .await
    .expect(StatusCode::OK, "cancel");
    h.upload_har("pets").await;
}

/// Refuses every call with 429 and `Retry-After: 7`, recording what it saw.
#[derive(Default)]
struct Refuse(std::sync::Mutex<Vec<(mb_ext::Tenancy, String, String)>>);

#[async_trait]
impl mb_ext::QuotaGuard for Refuse {
    async fn admit(
        &self,
        t: &mb_ext::Tenancy,
        call: &mb_ext::CallInfo<'_>,
    ) -> Result<(), mb_ext::Rejection> {
        self.0.lock().unwrap().push((
            t.clone(),
            call.mock.to_string(),
            call.operation_id.to_string(),
        ));
        Err(mb_ext::Rejection {
            status: 429,
            title: "Quota exceeded".into(),
            detail: "the workspace spent its import allowance".into(),
            retry_after: Some(Duration::from_secs(7)),
        })
    }
}

/// Collects every usage event.
#[derive(Default)]
struct Collect(std::sync::Mutex<Vec<mb_ext::UsageEvent>>);

impl mb_ext::UsageSink for Collect {
    fn record(&self, event: &mb_ext::UsageEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

fn default_tenancy() -> mb_ext::Tenancy {
    mb_ext::Tenancy {
        tenant: "default".into(),
        workspace: DEFAULT_WORKSPACE.into(),
    }
}

#[tokio::test]
async fn import_create_is_admitted_by_the_quota_guard() {
    let mut h = harness().await;
    let guard = Arc::new(Refuse::default());
    h.st.ext.quota = guard.clone();
    h.app = router(h.st.clone());
    let har = fixture("smart/petstore.har");
    let r = h
        .multipart(
            &format!("{BASE}/pets/imports"),
            true,
            &[("file", Some("petstore.har"), &har)],
        )
        .await;
    let p = r.expect(StatusCode::TOO_MANY_REQUESTS, "refused");
    assert_eq!(p["title"], "Quota exceeded");
    assert_eq!(p["detail"], "the workspace spent its import allowance");
    assert_eq!(r.headers[header::RETRY_AFTER], "7");
    assert_eq!(
        guard.0.lock().unwrap().clone(),
        vec![(default_tenancy(), "pets".to_string(), "import".to_string())]
    );
    assert!(
        h.st.store
            .list_import_jobs(h.mock("pets").await.id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(h.actions().await.iter().all(|a| a != "import.create"));
}

fn usage(prompt: u32, completion: u32) -> mb_llm::Usage {
    mb_llm::Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
    }
}

/// A harness whose usage sink is collected.
async fn metered(provider: Arc<dyn Provider>) -> (Harness, Arc<Collect>) {
    let mut h = harness_with(Some(provider)).await;
    let sink = Arc::new(Collect::default());
    h.st.ext.usage = sink.clone();
    h.app = router(h.st.clone());
    (h, sink)
}

async fn import_markdown(h: &Harness) -> String {
    let view = h
        .call(
            "editor",
            Method::POST,
            &format!("{BASE}/pets/imports"),
            Some(json!({ "kind": "markdown", "text": md_text() })),
        )
        .await
        .expect(StatusCode::ACCEPTED, "create");
    assert!(h.run().await);
    let job = h.job("pets", view["id"].as_str().unwrap()).await;
    job["status"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn import_llm_calls_are_recorded_as_usage() {
    let provider = Arc::new(ScriptedProvider::with_usage(vec![Ok((
        pets_answer(),
        usage(100, 20),
    ))]));
    let (h, sink) = metered(provider).await;
    assert_eq!(import_markdown(&h).await, "done");
    let events = sink.0.lock().unwrap().clone();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].tenancy, default_tenancy());
    assert_eq!(events[0].mock, "pets");
    assert_eq!(events[0].operation_id, "import");
    assert_eq!(events[0].source, "import");
    assert_eq!(events[0].status, 200);
    assert_eq!(
        (events[0].prompt_tokens, events[0].completion_tokens),
        (100, 20)
    );
}

#[tokio::test]
async fn failed_import_llm_calls_are_recorded_as_usage() {
    let provider = Arc::new(ScriptedProvider::new(vec![Err(LlmError::Timeout)]));
    let (h, sink) = metered(provider).await;
    assert_eq!(import_markdown(&h).await, "failed");
    let events = sink.0.lock().unwrap().clone();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].status, 502);
    assert_eq!(events[0].mock, "pets");
}

#[tokio::test]
async fn canceled_import_llm_calls_are_recorded_as_usage() {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut st = AdminState {
        auth: Arc::new(TestAuth),
        ..AdminState::new(store.clone(), state)
    };
    let sink = Arc::new(Collect::default());
    st.ext.usage = sink.clone();
    let h0 = Harness {
        app: router(st.clone()),
        st: st.clone(),
    };
    h0.create_mock("pets").await;
    let mock = h0.mock("pets").await;
    st.llm = Arc::new(TestLlm(Some(Arc::new(CancelingProvider {
        store,
        mock_id: mock.id,
    }))));
    let h = Harness {
        app: router(st.clone()),
        st,
    };
    assert_eq!(import_markdown(&h).await, "canceled");
    assert_eq!(sink.0.lock().unwrap().len(), 1);
}

#[test]
fn import_tokens_metric_counts_unfinished_jobs() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                // An answer with no operations: the job fails, the tokens
                // were still spent.
                let provider = Arc::new(ScriptedProvider::with_usage(vec![Ok((
                    json!({"operations": []}).to_string(),
                    usage(40, 2),
                ))]));
                let (h, _sink) = metered(provider).await;
                assert_eq!(import_markdown(&h).await, "failed");
            });
    });
    let tokens: u64 = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(key, ..)| key.key().name() == "mb_import_tokens_total")
        .map(|(.., value)| match value {
            DebugValue::Counter(n) => n,
            other => panic!("unexpected {other:?}"),
        })
        .sum();
    assert_eq!(tokens, 42);
}
