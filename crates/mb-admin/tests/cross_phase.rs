//! P3 × P4: a multi-file proto archive (P4) uploaded through the
//! workspace-scoped, RBAC-checked and audited contract endpoint (P3).
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use mb_admin::{AdminState, router};
use mb_auth::{AuthError, Authenticate, Principal, PrincipalKind, Role};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

const WS: &str = "team-a";

/// `Bearer team-viewer|team-editor` → that role in `team-a`;
/// `Bearer sysadmin` → sysadmin.
struct TestAuth;

#[async_trait::async_trait]
impl Authenticate for TestAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Option<Principal>, AuthError> {
        let Some(value) = headers.get(header::AUTHORIZATION) else {
            return Ok(None);
        };
        let name = value.to_str().unwrap().trim_start_matches("Bearer ");
        let (sysadmin, roles) = match name {
            "team-viewer" => (false, vec![(WS, Role::Viewer)]),
            "team-editor" => (false, vec![(WS, Role::Editor)]),
            "sysadmin" => (true, vec![]),
            _ => return Err(AuthError::Unauthenticated("unknown token".into())),
        };
        Ok(Some(Principal {
            subject: name.into(),
            display_name: name.into(),
            kind: PrincipalKind::User,
            sysadmin,
            roles: roles
                .into_iter()
                .map(|(ws, r)| (ws.to_string(), r))
                .collect::<BTreeMap<_, _>>(),
        }))
    }
}

fn fixture(rel: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The two-file `proto/multi` fixture (orders.proto imports money.proto)
/// as a zip.
fn proto_zip() -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for name in ["common/v1/money.proto", "orders/v1/orders.proto"] {
        w.start_file(
            format!("multi/{name}"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        w.write_all(&fixture(&format!("proto/multi/{name}")))
            .unwrap();
    }
    w.finish().unwrap().into_inner()
}

async fn send(
    app: &Router,
    who: &str,
    req: axum::http::request::Builder,
    body: Body,
) -> (StatusCode, Value) {
    let req = req
        .header(header::AUTHORIZATION, format!("Bearer {who}"))
        .body(body)
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn call(
    app: &Router,
    who: &str,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(b) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    send(app, who, req, body).await
}

async fn upload_zip(app: &Router, who: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::post(uri).header(header::CONTENT_TYPE, "application/zip");
    send(app, who, req, Body::from(proto_zip())).await
}

#[tokio::test]
async fn proto_archive_upload_is_rbac_checked_and_audited() {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let app = router(AdminState {
        auth: Arc::new(TestAuth),
        ..AdminState::new(store, state)
    });
    let (s, _) = call(
        &app,
        "sysadmin",
        Method::POST,
        "/api/v1/workspaces",
        Some(json!({"name": WS})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let mocks = format!("/api/v1/workspaces/{WS}/mocks");
    let (s, body) = call(
        &app,
        "team-editor",
        Method::POST,
        &mocks,
        Some(json!({"name": "orders"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let contracts = format!("{mocks}/orders/contracts");

    // A viewer may not upload; nothing is stored or audited.
    let (s, body) = upload_zip(&app, "team-viewer", &contracts).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    let (_, versions) = call(&app, "team-viewer", Method::GET, &contracts, None).await;
    assert_eq!(versions, json!([]));

    // An editor uploads the archive: both files compile into one gRPC bundle.
    let expected = mb_import::compile(&proto_zip()).unwrap().operations.len();
    assert!(expected > 0);
    let (s, body) = upload_zip(&app, "team-editor", &contracts).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(
        body,
        json!({"version": 1, "status": "active", "operations": expected})
    );

    // The version lists gRPC operations with their protocol and target.
    let (s, detail) = call(
        &app,
        "team-viewer",
        Method::GET,
        &format!("{contracts}/1"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{detail}");
    let ops = detail["operations"].as_array().unwrap();
    assert_eq!(ops.len(), expected);
    for op in ops {
        assert_eq!(op["protocol"], "grpc", "{op}");
        assert!(op["target"].as_str().unwrap().starts_with('/'), "{op}");
        assert_eq!(op["method"], "", "{op}");
    }

    // Exactly one audit event, by the editor, in the workspace.
    let audit = format!("/api/v1/workspaces/{WS}/audit?action=contract.upload");
    let (s, events) = call(&app, "team-editor", Method::GET, &audit, None).await;
    assert_eq!(s, StatusCode::OK, "{events}");
    let events = events.as_array().unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["actor_subject"], "team-editor");
    assert_eq!(events[0]["workspace"], WS);
    assert_eq!(events[0]["target"], "contract:orders@1");
    assert_eq!(events[0]["after"]["operations"], expected);
}
