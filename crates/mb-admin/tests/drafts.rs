//! Draft review: `…/contracts/{version}/draft`, `/approve` and `/preview`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use mb_admin::{AdminState, openapi_document, router};
use mb_auth::{AuthError, Authenticate, Principal, PrincipalKind, Role};
use mb_engine::SchemaValidator;
use mb_import::smart::{ImportInput, ImportKind, ImportOptions, NoopObserver, run_import};
use mb_state::{MemoryStore, StateStore};
use mb_store::{AuditFilter, ContractStatus, DEFAULT_WORKSPACE, Mock, NewContractVersion};
use serde_json::{Value, json};
use tower::ServiceExt;

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

fn fixture(rel: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel);
    std::fs::read(p).unwrap()
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

async fn harness() -> Harness {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let st = AdminState {
        auth: Arc::new(TestAuth),
        ..AdminState::new(store, state)
    };
    let h = Harness {
        app: router(st.clone()),
        st,
    };
    h.call(
        "editor",
        Method::POST,
        BASE,
        Some(json!({ "name": "pets" })),
    )
    .await
    .expect(StatusCode::CREATED, "create mock");
    h
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

    async fn mock(&self) -> Mock {
        self.st
            .store
            .get_mock(DEFAULT_WORKSPACE, "pets")
            .await
            .unwrap()
            .unwrap()
    }

    /// Stores the HAR fixture's import as a draft; returns its version.
    async fn har_draft(&self) -> i64 {
        let draft = run_import(
            ImportInput::Bytes {
                kind: ImportKind::Har,
                name: Some("petstore.har".into()),
                bytes: fixture("smart/petstore.har"),
            },
            None,
            &ImportOptions::default(),
            &NoopObserver,
        )
        .await
        .unwrap();
        let mock = self.mock().await;
        self.st
            .store
            .add_draft_contract_version(
                mock.id,
                NewContractVersion {
                    source: draft.source,
                    bundle: draft.bundle,
                    status: ContractStatus::Draft,
                    created_by: Some("editor".into()),
                },
                serde_json::to_value(&draft.meta).unwrap(),
            )
            .await
            .unwrap()
            .version
    }

    async fn upload_orders(&self) {
        let req = Request::post(format!("{BASE}/pets/contracts"))
            .header(header::AUTHORIZATION, "Bearer editor")
            .header(header::CONTENT_TYPE, "application/yaml")
            .body(Body::from(fixture("openapi/orders-3.0.yaml")))
            .unwrap();
        self.send(req)
            .await
            .expect(StatusCode::CREATED, "upload orders");
    }

    fn url(version: i64, rest: &str) -> String {
        format!("{BASE}/pets/contracts/{version}{rest}")
    }

    async fn draft(&self, version: i64) -> Value {
        self.call("viewer", Method::GET, &Self::url(version, "/draft"), None)
            .await
            .expect(StatusCode::OK, "get draft")
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
}

fn operation<'a>(view: &'a Value, method: &str, path: &str) -> &'a Value {
    view["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["method"] == method && o["path"] == path)
        .unwrap_or_else(|| panic!("{method} {path} missing: {view}"))
}

const PET_PATH: &str = "/api/pets/{petId}";

#[tokio::test]
async fn get_draft_lists_operations_with_confidence() {
    let h = harness().await;
    let v = h.har_draft().await;
    let view = h.draft(v).await;
    assert_eq!(view["version"], v);
    assert_eq!(view["status"], "draft");
    assert!(
        view["document"]["openapi"]
            .as_str()
            .unwrap()
            .starts_with("3.")
    );
    assert!(
        view["document"]["paths"][PET_PATH]["get"].is_object(),
        "{view}"
    );
    let get = operation(&view, "GET", PET_PATH);
    assert_eq!(get["confidence"], 1.0);
    assert_eq!(get["edited"], false);
    assert!(get["id"].is_string());
    let provenance = get["provenance"].as_array().unwrap();
    assert!(!provenance.is_empty());
    assert!(
        provenance[0]["location"]
            .as_str()
            .unwrap()
            .starts_with("entry"),
        "{get}"
    );
    assert_eq!(view["meta"]["source_kind"], "har");
    assert_eq!(view["meta"]["source_name"], "petstore.har");
    assert_eq!(view["meta"]["edited"], false);
    assert!(view["meta"]["usage"]["prompt_tokens"].is_u64());
    assert!(view["diff"].is_null(), "{view}");

    // Sorted by (path, method).
    let keys: Vec<(String, String)> = view["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["path"].as_str().unwrap().to_string(),
                o["method"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

#[tokio::test]
async fn get_draft_of_plain_upload_404() {
    let h = harness().await;
    h.upload_orders().await;
    let p = h
        .call("viewer", Method::GET, &Harness::url(1, "/draft"), None)
        .await
        .expect(StatusCode::NOT_FOUND, "plain upload");
    assert_eq!(
        p["detail"],
        "contract version 1 of mock \"pets\" is not an imported draft"
    );
    h.call("viewer", Method::GET, &Harness::url(9, "/draft"), None)
        .await
        .expect(StatusCode::NOT_FOUND, "missing version");
}

#[tokio::test]
async fn put_draft_recompiles_and_marks_edited() {
    let h = harness().await;
    let v = h.har_draft().await;
    let before = h.draft(v).await;
    let digest_before =
        h.st.store
            .get_contract_version(h.mock().await.id, v)
            .await
            .unwrap()
            .unwrap()
            .source_digest;

    let mut doc = before["document"].clone();
    doc["paths"][PET_PATH]["get"]["summary"] = json!("Fetch one pet");
    doc["paths"]["/api/health"] = json!({"get": {
        "summary": "Health check",
        "responses": {"200": {"description": "Healthy", "content": {"application/json": {
            "schema": {"type": "object", "properties": {"status": {"type": "string"}}}
        }}}}
    }});
    let view = h
        .call(
            "editor",
            Method::PUT,
            &Harness::url(v, "/draft"),
            Some(json!({ "document": doc })),
        )
        .await
        .expect(StatusCode::OK, "put draft");
    let health = operation(&view, "GET", "/api/health");
    assert_eq!(health["edited"], true);
    assert_eq!(health["confidence"], 1.0);
    let get = operation(&view, "GET", PET_PATH);
    assert_eq!(get["summary"], "Fetch one pet");
    assert_eq!(get["edited"], true);
    assert_eq!(view["meta"]["edited"], true);
    assert_eq!(
        view["document"]["paths"][PET_PATH]["get"]["summary"],
        "Fetch one pet"
    );

    let stored =
        h.st.store
            .get_contract_version(h.mock().await.id, v)
            .await
            .unwrap()
            .unwrap();
    assert_ne!(stored.source_digest, digest_before);
    assert_eq!(stored.draft_meta.unwrap()["edited"], true);
    assert!(
        h.actions()
            .await
            .contains(&"contract.draft_update".to_string())
    );
}

/// Imports produce drafts far larger than axum's default 2 MiB body limit;
/// the PUT that saves every edit must accept them like contract uploads.
#[tokio::test]
async fn put_draft_over_two_mib_is_accepted() {
    let h = harness().await;
    let v = h.har_draft().await;
    let mut doc = h.draft(v).await["document"].clone();
    doc["paths"][PET_PATH]["get"]["description"] = json!("x".repeat(2 * 1024 * 1024 + 1024));
    let body = json!({ "document": doc });
    assert!(body.to_string().len() > 2 * 1024 * 1024);
    let view = h
        .call(
            "editor",
            Method::PUT,
            &Harness::url(v, "/draft"),
            Some(body),
        )
        .await
        .expect(StatusCode::OK, "put large draft");
    assert_eq!(operation(&view, "GET", PET_PATH)["edited"], true);
}

#[tokio::test]
async fn put_draft_invalid_422_with_pointers() {
    let h = harness().await;
    let v = h.har_draft().await;
    let before = h.draft(v).await;
    let mut doc = before["document"].clone();
    doc["paths"][PET_PATH]["get"]["responses"]["200"]["content"]["application/json"]["schema"] =
        json!({"type": 12});
    let p = h
        .call(
            "editor",
            Method::PUT,
            &Harness::url(v, "/draft"),
            Some(json!({ "document": doc })),
        )
        .await
        .expect(StatusCode::UNPROCESSABLE_ENTITY, "invalid draft");
    assert_eq!(p["status"], 422);
    assert_eq!(p["type"], "about:blank");
    let errors = p["errors"].as_array().unwrap();
    assert_eq!(
        p["detail"],
        format!("the draft has {} problem(s)", errors.len())
    );
    assert_eq!(
        errors[0]["pointer"],
        "/paths/~1api~1pets~1{petId}/get/responses/200/content/application~1json/schema"
    );
    assert!(errors[0]["message"].is_string());

    assert_eq!(h.draft(v).await, before, "the stored version is unchanged");
}

#[tokio::test]
async fn put_draft_on_approved_409() {
    let h = harness().await;
    let v = h.har_draft().await;
    let doc = h.draft(v).await["document"].clone();
    h.call(
        "editor",
        Method::POST,
        &Harness::url(v, "/approve"),
        Some(json!({})),
    )
    .await
    .expect(StatusCode::OK, "approve");
    h.call(
        "editor",
        Method::PUT,
        &Harness::url(v, "/draft"),
        Some(json!({ "document": doc })),
    )
    .await
    .expect(StatusCode::CONFLICT, "put on approved");
}

#[tokio::test]
async fn approve_then_activate() {
    let h = harness().await;
    let v = h.har_draft().await;
    let doc = h.draft(v).await["document"].clone();
    h.call(
        "editor",
        Method::PUT,
        &Harness::url(v, "/draft"),
        Some(json!({ "document": doc })),
    )
    .await
    .expect(StatusCode::OK, "put draft");

    // Activation of a draft is refused.
    h.call(
        "editor",
        Method::POST,
        &Harness::url(v, "/activate"),
        Some(json!({})),
    )
    .await
    .expect(StatusCode::CONFLICT, "activate draft");
    // Approval needs a JSON content type.
    h.call("editor", Method::POST, &Harness::url(v, "/approve"), None)
        .await
        .expect(StatusCode::UNSUPPORTED_MEDIA_TYPE, "approve without json");

    let approved = h
        .call(
            "editor",
            Method::POST,
            &Harness::url(v, "/approve"),
            Some(json!({})),
        )
        .await
        .expect(StatusCode::OK, "approve");
    assert_eq!(approved["status"], "approved");
    assert_eq!(approved["version"], v);
    assert_eq!(approved["imported"], true);
    h.call(
        "editor",
        Method::POST,
        &Harness::url(v, "/approve"),
        Some(json!({})),
    )
    .await
    .expect(StatusCode::CONFLICT, "approve again");
    let active = h
        .call(
            "editor",
            Method::POST,
            &Harness::url(v, "/activate"),
            Some(json!({})),
        )
        .await
        .expect(StatusCode::OK, "activate");
    assert_eq!(active["status"], "active");

    let actions = h.actions().await;
    assert!(
        actions.contains(&"contract.draft_update".to_string()),
        "{actions:?}"
    );
    assert!(
        actions.contains(&"contract.approve".to_string()),
        "{actions:?}"
    );
    let approve =
        h.st.store
            .list_audit(&AuditFilter {
                action: Some("contract.approve".into()),
                ..AuditFilter::default()
            })
            .await
            .unwrap();
    assert_eq!(approve[0].target, format!("contract:pets@{v}"));
    assert_eq!(approve[0].before.clone().unwrap()["status"], "draft");
    assert_eq!(approve[0].after.clone().unwrap()["status"], "approved");
}

#[tokio::test]
async fn viewer_reads_but_cannot_edit_or_approve() {
    let h = harness().await;
    let v = h.har_draft().await;
    let view = h.draft(v).await;
    let op_id = operation(&view, "GET", PET_PATH)["id"].clone();
    h.call(
        "viewer",
        Method::PUT,
        &Harness::url(v, "/draft"),
        Some(json!({ "document": view["document"] })),
    )
    .await
    .expect(StatusCode::FORBIDDEN, "viewer put");
    h.call(
        "viewer",
        Method::POST,
        &Harness::url(v, "/approve"),
        Some(json!({})),
    )
    .await
    .expect(StatusCode::FORBIDDEN, "viewer approve");
    h.call(
        "viewer",
        Method::POST,
        &Harness::url(v, "/preview"),
        Some(json!({ "operation_id": op_id })),
    )
    .await
    .expect(StatusCode::FORBIDDEN, "viewer preview");
}

#[tokio::test]
async fn preview_generates_without_touching_state() {
    let h = harness().await;
    let v = h.har_draft().await;
    let view = h.draft(v).await;
    let op_id = operation(&view, "GET", PET_PATH)["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = h
        .call(
            "editor",
            Method::POST,
            &Harness::url(v, "/preview"),
            Some(json!({ "operation_id": op_id })),
        )
        .await
        .expect(StatusCode::OK, "preview");
    assert_eq!(r["source"], "fallback");
    assert_eq!(r["status"], 200);
    assert_eq!(r["content_type"], "application/json");

    let stored =
        h.st.store
            .get_contract_version(h.mock().await.id, v)
            .await
            .unwrap()
            .unwrap();
    let op = stored
        .bundle
        .operations
        .iter()
        .find(|o| o.id == op_id)
        .unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let validator = SchemaValidator::new(&stored.bundle.with_reachable_defs(&schema)).unwrap();
    validator.validate(&r["body"]).unwrap();

    let sessions =
        h.st.state
            .sessions(DEFAULT_WORKSPACE, "pets")
            .await
            .unwrap();
    assert!(sessions.is_empty(), "{sessions:?}");
}

#[tokio::test]
async fn preview_unknown_operation_404() {
    let h = harness().await;
    let v = h.har_draft().await;
    h.call(
        "editor",
        Method::POST,
        &Harness::url(v, "/preview"),
        Some(json!({ "operation_id": "nope" })),
    )
    .await
    .expect(StatusCode::NOT_FOUND, "unknown operation");
}

#[tokio::test]
async fn preview_undeclared_status_422() {
    let h = harness().await;
    let v = h.har_draft().await;
    let view = h.draft(v).await;
    let op_id = operation(&view, "GET", PET_PATH)["id"].clone();
    let p = h
        .call(
            "editor",
            Method::POST,
            &Harness::url(v, "/preview"),
            Some(json!({ "operation_id": op_id, "status": 599 })),
        )
        .await
        .expect(StatusCode::UNPROCESSABLE_ENTITY, "undeclared status");
    assert!(p["detail"].as_str().unwrap().contains("599"), "{p}");
}

#[tokio::test]
async fn diff_against_active_version() {
    let h = harness().await;
    h.upload_orders().await;
    let v = h.har_draft().await;
    assert_eq!(v, 2);
    let view = h.draft(v).await;
    let diff = &view["diff"];
    assert_eq!(diff["against_version"], 1, "{view}");
    let has = |key: &str, target: &str| diff[key].as_array().unwrap().iter().any(|t| t == target);
    assert!(has("added", "GET /api/pets/{petId}"), "{diff}");
    assert!(has("removed", "GET /orders/{orderId}"), "{diff}");
    assert!(diff["changed"].is_array());
}

#[tokio::test]
async fn contracts_summary_marks_imported() {
    let h = harness().await;
    h.upload_orders().await;
    h.har_draft().await;
    let list = h
        .call(
            "viewer",
            Method::GET,
            &format!("{BASE}/pets/contracts"),
            None,
        )
        .await
        .expect(StatusCode::OK, "list");
    let imported: BTreeMap<i64, bool> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["version"].as_i64().unwrap(),
                v["imported"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(imported, BTreeMap::from([(1, false), (2, true)]));
}

#[tokio::test]
async fn openapi_document_lists_import_and_draft_paths() {
    let doc = serde_json::to_value(openapi_document()).unwrap();
    let mut ids = Vec::new();
    for item in doc["paths"].as_object().unwrap().values() {
        for op in item.as_object().unwrap().values() {
            if let Some(id) = op["operationId"].as_str() {
                ids.push(id.to_string());
            }
        }
    }
    for id in [
        "imports_create",
        "imports_cancel",
        "contracts_get_draft",
        "contracts_put_draft",
        "contracts_approve",
        "contracts_preview",
    ] {
        assert!(ids.contains(&id.to_string()), "{id} missing: {ids:?}");
        let alias = format!("{id}_default");
        assert!(ids.contains(&alias), "{alias} missing: {ids:?}");
    }
    let base = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}";
    assert!(doc["paths"][format!("{base}/draft")]["get"].is_object());
    assert!(doc["paths"][format!("{base}/draft")]["put"].is_object());
    assert!(doc["paths"][format!("{base}/approve")]["post"].is_object());
    assert!(doc["paths"][format!("{base}/preview")]["post"].is_object());
    let schemas = doc["components"]["schemas"].as_object().unwrap();
    for name in [
        "DraftView",
        "DraftMetaView",
        "DraftOperationView",
        "DraftWarningView",
        "DraftDiffView",
        "ProvenanceView",
        "TokenUsageView",
        "PutDraft",
        "DraftProblem",
        "DraftIssueView",
        "PreviewRequest",
        "PreviewResponse",
    ] {
        assert!(schemas.contains_key(name), "schema {name} missing");
    }
    assert!(
        schemas["ContractVersionSummary"]["properties"]["imported"].is_object(),
        "imported missing"
    );
}

#[derive(Default)]
struct RefusePreview(std::sync::Mutex<Vec<(mb_ext::Tenancy, String, String)>>);

#[async_trait]
impl mb_ext::QuotaGuard for RefusePreview {
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
            detail: "no previews left".into(),
            retry_after: None,
        })
    }
}

#[derive(Default)]
struct CollectPreview(std::sync::Mutex<Vec<mb_ext::UsageEvent>>);

impl mb_ext::UsageSink for CollectPreview {
    fn record(&self, event: &mb_ext::UsageEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

#[tokio::test]
async fn preview_is_admitted_and_recorded() {
    let mut h = harness().await;
    let sink = Arc::new(CollectPreview::default());
    h.st.ext.usage = sink.clone();
    h.app = router(h.st.clone());
    let v = h.har_draft().await;
    let view = h.draft(v).await;
    let op_id = operation(&view, "GET", PET_PATH)["id"]
        .as_str()
        .unwrap()
        .to_string();
    h.call(
        "editor",
        Method::POST,
        &Harness::url(v, "/preview"),
        Some(json!({ "operation_id": op_id })),
    )
    .await
    .expect(StatusCode::OK, "preview");
    let events = sink.0.lock().unwrap().clone();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].mock, "pets");
    assert_eq!(events[0].operation_id, op_id);
    assert_eq!(events[0].source, "fallback");
    assert_eq!(events[0].status, 200);

    let guard = Arc::new(RefusePreview::default());
    h.st.ext.quota = guard.clone();
    h.app = router(h.st.clone());
    let p = h
        .call(
            "editor",
            Method::POST,
            &Harness::url(v, "/preview"),
            Some(json!({ "operation_id": op_id })),
        )
        .await
        .expect(StatusCode::TOO_MANY_REQUESTS, "refused preview");
    assert_eq!(p["detail"], "no previews left");
    let seen = guard.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0.workspace, DEFAULT_WORKSPACE);
    assert_eq!(seen[0].1, "pets");
    assert_eq!(
        sink.0.lock().unwrap().len(),
        1,
        "a refused preview is not recorded"
    );
}

#[tokio::test]
async fn put_draft_refreshes_schema_warnings() {
    let h = harness().await;
    let v = h.har_draft().await;
    let huge: Value = serde_json::from_slice(&fixture("openapi/huge-response.json")).unwrap();
    let view = h
        .call(
            "editor",
            Method::PUT,
            &Harness::url(v, "/draft"),
            Some(json!({ "document": huge })),
        )
        .await
        .expect(StatusCode::OK, "put huge");
    let codes: Vec<&str> = view["meta"]["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["code"].as_str().unwrap())
        .collect();
    assert_eq!(
        codes.iter().filter(|c| **c == "schema_too_large").count(),
        1,
        "{codes:?}"
    );
    let list = h
        .call(
            "viewer",
            Method::GET,
            &format!("{BASE}/pets/contracts"),
            None,
        )
        .await
        .json();
    assert_eq!(list[0]["warnings"][0]["operation"], "getBig");
}
