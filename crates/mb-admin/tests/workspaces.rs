//! Workspace endpoints, workspace-scoped resources and the P2 alias paths,
//! as the Local principal (`auth.mode: none`).
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use mb_admin::{AdminState, router};
use mb_auth::{AuthError, Authenticate, Principal, PrincipalKind, Role};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use mb_store::ControlStore;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    app: Router,
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
}

struct Resp {
    status: StatusCode,
    content_type: String,
    text: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.text))
    }

    fn ok(&self, status: StatusCode) -> Value {
        assert_eq!(self.status, status, "body: {}", self.text);
        if self.text.is_empty() {
            Value::Null
        } else {
            self.json()
        }
    }

    fn problem(&self, status: StatusCode) -> Value {
        assert_eq!(self.status, status, "body: {}", self.text);
        assert_eq!(
            self.content_type, "application/problem+json",
            "{}",
            self.text
        );
        self.json()
    }
}

async fn harness() -> Harness {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let app = router(AdminState::new(store.clone(), state.clone()));
    Harness { app, store, state }
}

impl Harness {
    async fn send(&self, method: Method, uri: &str, body: Option<Value>) -> Resp {
        let mut req = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let resp = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        Resp {
            status,
            content_type,
            text: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn get(&self, uri: &str) -> Resp {
        self.send(Method::GET, uri, None).await
    }

    async fn post(&self, uri: &str, body: Value) -> Resp {
        self.send(Method::POST, uri, Some(body)).await
    }

    async fn delete(&self, uri: &str) -> Resp {
        self.send(Method::DELETE, uri, None).await
    }

    async fn create_workspace(&self, name: &str) {
        self.post("/api/v1/workspaces", json!({ "name": name }))
            .await
            .ok(StatusCode::CREATED);
    }
}

#[tokio::test]
async fn workspace_crud() {
    let h = harness().await;

    let created = h
        .post(
            "/api/v1/workspaces",
            json!({ "name": "team-a", "display_name": "Team A" }),
        )
        .await
        .ok(StatusCode::CREATED);
    assert_eq!(created["name"], "team-a");
    assert_eq!(created["display_name"], "Team A");
    assert_eq!(created["role"], "owner");
    assert!(created["created_at"].is_string(), "{created}");

    // display_name defaults to the name.
    let b = h
        .post("/api/v1/workspaces", json!({ "name": "team-b" }))
        .await
        .ok(StatusCode::CREATED);
    assert_eq!(b["display_name"], "team-b");

    h.post("/api/v1/workspaces", json!({ "name": "team-a" }))
        .await
        .problem(StatusCode::CONFLICT);
    h.post("/api/v1/workspaces", json!({ "name": "Bad Name" }))
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY);

    let list = h.get("/api/v1/workspaces").await.ok(StatusCode::OK);
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["default", "team-a", "team-b"]);

    let one = h.get("/api/v1/workspaces/team-a").await.ok(StatusCode::OK);
    assert_eq!(one["display_name"], "Team A");

    let patched = h
        .send(
            Method::PATCH,
            "/api/v1/workspaces/team-a",
            Some(json!({ "display_name": "Team Alpha" })),
        )
        .await
        .ok(StatusCode::OK);
    assert_eq!(patched["display_name"], "Team Alpha");
    assert_eq!(
        h.get("/api/v1/workspaces/team-a").await.json()["display_name"],
        "Team Alpha"
    );

    h.delete("/api/v1/workspaces/team-a")
        .await
        .ok(StatusCode::NO_CONTENT);
    h.get("/api/v1/workspaces/team-a")
        .await
        .problem(StatusCode::NOT_FOUND);
    h.delete("/api/v1/workspaces/team-a")
        .await
        .problem(StatusCode::NOT_FOUND);

    // Unknown paths and methods under a workspace are problems too.
    h.get("/api/v1/workspaces/default/nothing-here")
        .await
        .problem(StatusCode::NOT_FOUND);
    h.send(Method::PUT, "/api/v1/workspaces/default/mocks", None)
        .await
        .problem(StatusCode::METHOD_NOT_ALLOWED);
    h.send(Method::PUT, "/api/v1/workspaces", None)
        .await
        .problem(StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn delete_default_409() {
    let h = harness().await;
    h.delete("/api/v1/workspaces/default")
        .await
        .problem(StatusCode::CONFLICT);
    h.delete("/api/v1/workspaces/default?force=true")
        .await
        .problem(StatusCode::CONFLICT);
    h.get("/api/v1/workspaces/default").await.ok(StatusCode::OK);
}

#[tokio::test]
async fn delete_nonempty_requires_force() {
    let h = harness().await;
    h.create_workspace("team-a").await;
    h.post(
        "/api/v1/workspaces/team-a/mocks",
        json!({ "name": "orders" }),
    )
    .await
    .ok(StatusCode::CREATED);
    let key = SessionKey {
        workspace: "team-a".into(),
        mock: "orders".into(),
        session: "s1".into(),
    };
    h.state
        .seed(
            &key,
            vec![StoredResource {
                resource_type: "order".into(),
                id: "1".into(),
                data: json!({ "id": "1" }),
            }],
        )
        .await
        .unwrap();

    h.delete("/api/v1/workspaces/team-a")
        .await
        .problem(StatusCode::CONFLICT);
    h.get("/api/v1/workspaces/team-a/mocks/orders")
        .await
        .ok(StatusCode::OK);
    assert_eq!(h.state.export(&key).await.unwrap().len(), 1);

    h.delete("/api/v1/workspaces/team-a?force=true")
        .await
        .ok(StatusCode::NO_CONTENT);
    assert!(h.store.get_workspace("team-a").await.unwrap().is_none());
    assert!(
        h.store
            .get_mock("team-a", "orders")
            .await
            .unwrap()
            .is_none()
    );
    assert!(h.state.export(&key).await.unwrap().is_empty());
}

#[tokio::test]
async fn same_mock_name_two_workspaces_ok() {
    let h = harness().await;
    h.create_workspace("team-a").await;
    let a = h
        .post(
            "/api/v1/workspaces/default/mocks",
            json!({ "name": "orders" }),
        )
        .await
        .ok(StatusCode::CREATED);
    let b = h
        .post(
            "/api/v1/workspaces/team-a/mocks",
            json!({ "name": "orders", "route_prefix": "/team-a/orders" }),
        )
        .await
        .ok(StatusCode::CREATED);
    assert_eq!(a["workspace"], "default");
    assert_eq!(b["workspace"], "team-a");
    assert_ne!(a["id"], b["id"]);

    let list = h
        .get("/api/v1/workspaces/team-a/mocks")
        .await
        .ok(StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["route_prefix"], "/team-a/orders");

    // Deleting one leaves the other.
    h.delete("/api/v1/workspaces/team-a/mocks/orders")
        .await
        .ok(StatusCode::NO_CONTENT);
    h.get("/api/v1/workspaces/default/mocks/orders")
        .await
        .ok(StatusCode::OK);
    h.get("/api/v1/workspaces/team-a/mocks/orders")
        .await
        .problem(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn prefix_collision_across_workspaces_409() {
    let h = harness().await;
    h.create_workspace("team-a").await;
    h.post(
        "/api/v1/workspaces/team-a/mocks",
        json!({ "name": "orders" }),
    )
    .await
    .ok(StatusCode::CREATED);

    // The default prefix `/orders` is taken by team-a/orders.
    let p = h
        .post("/api/v1/mocks", json!({ "name": "orders" }))
        .await
        .problem(StatusCode::CONFLICT);
    assert!(
        p["detail"].as_str().unwrap().contains("team-a/orders"),
        "{p}"
    );

    h.post(
        "/api/v1/mocks",
        json!({ "name": "billing", "route_prefix": "/billing" }),
    )
    .await
    .ok(StatusCode::CREATED);
    let p = h
        .send(
            Method::PATCH,
            "/api/v1/mocks/billing",
            Some(json!({ "route_prefix": "/orders" })),
        )
        .await
        .problem(StatusCode::CONFLICT);
    assert!(
        p["detail"].as_str().unwrap().contains("team-a/orders"),
        "{p}"
    );
}

#[tokio::test]
async fn alias_routes_act_on_default() {
    let h = harness().await;
    h.post(
        "/api/v1/mocks",
        json!({
            "name": "x",
            "data_plane_auth": { "mode": "api_key", "header": "x-key" }
        }),
    )
    .await
    .ok(StatusCode::CREATED);
    let m = h
        .get("/api/v1/workspaces/default/mocks/x")
        .await
        .ok(StatusCode::OK);
    assert_eq!(m["workspace"], "default");
    assert_eq!(m["data_plane_auth"]["mode"], "api_key");
    assert_eq!(m["data_plane_auth"]["header"], "x-key");

    h.post("/api/v1/workspaces/default/mocks", json!({ "name": "y" }))
        .await
        .ok(StatusCode::CREATED);
    let y = h.get("/api/v1/mocks/y").await.ok(StatusCode::OK);
    assert_eq!(y["data_plane_auth"]["mode"], "none");
    assert_eq!(y["data_plane_auth"]["header"], "x-api-key");

    // Patching data-plane auth through either path.
    let patched = h
        .send(
            Method::PATCH,
            "/api/v1/workspaces/default/mocks/y",
            Some(json!({ "data_plane_auth": { "mode": "passthrough_validate" } })),
        )
        .await
        .ok(StatusCode::OK);
    assert_eq!(patched["data_plane_auth"]["mode"], "passthrough_validate");

    let profile = json!({ "base_url": "https://llm", "model": "m" });
    h.send(
        Method::PUT,
        "/api/v1/llm-profiles/fast",
        Some(profile.clone()),
    )
    .await
    .ok(StatusCode::OK);
    h.get("/api/v1/workspaces/default/llm-profiles/fast")
        .await
        .ok(StatusCode::OK);

    // A resource in another workspace is not visible through the alias.
    h.create_workspace("team-a").await;
    h.send(
        Method::PUT,
        "/api/v1/workspaces/team-a/llm-profiles/slow",
        Some(profile),
    )
    .await
    .ok(StatusCode::OK);
    h.get("/api/v1/llm-profiles/slow")
        .await
        .problem(StatusCode::NOT_FOUND);
    let listed = h.get("/api/v1/mocks").await.ok(StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 2, "{listed}");
}

#[tokio::test]
async fn openapi_has_canonical_and_deprecated_alias_paths() {
    let h = harness().await;
    let doc = h.get("/api/openapi.json").await.ok(StatusCode::OK);
    let paths = doc["paths"].as_object().unwrap();

    let canonical = &paths["/api/v1/workspaces/{ws}/mocks/{mock}"];
    assert!(canonical["get"].is_object(), "{canonical}");
    let params = canonical["get"]["parameters"].as_array().unwrap();
    assert!(params.iter().any(|p| p["name"] == "ws"), "{canonical}");
    assert!(
        canonical["get"]
            .get("deprecated")
            .is_none_or(|d| d == false)
    );

    for path in [
        "/api/v1/workspaces",
        "/api/v1/workspaces/{ws}",
        "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/activate",
        "/api/v1/workspaces/{ws}/mocks/{mock}/requests",
        "/api/v1/workspaces/{ws}/llm-profiles/{name}",
        "/api/v1/workspaces/{ws}/mocks/{mock}/channels",
        "/api/v1/workspaces/{ws}/mocks/{mock}/channels/{channel}/publish",
        "/api/v1/workspaces/{ws}/mocks/{mock}/event-settings",
        "/api/v1/brokers",
    ] {
        assert!(paths.contains_key(path), "{path} missing");
    }

    let mut aliases = 0;
    for (path, item) in paths {
        let Some(rest) = path.strip_prefix("/api/v1/workspaces/{ws}") else {
            continue;
        };
        if !(rest.starts_with("/mocks") || rest.starts_with("/llm-profiles")) {
            continue;
        }
        let alias_path = format!("/api/v1{rest}");
        let alias = &paths[&alias_path];
        for (method, op) in item.as_object().unwrap() {
            let alias_op = &alias[method];
            assert_eq!(alias_op["deprecated"], true, "{method} {alias_path}");
            let params = alias_op["parameters"].as_array().unwrap();
            assert!(
                params.iter().all(|p| p["name"] != "ws"),
                "{method} {alias_path}"
            );
            assert_eq!(
                params.len() + 1,
                op["parameters"].as_array().unwrap().len(),
                "{method} {alias_path}"
            );
            if let Some(id) = op["operationId"].as_str() {
                assert_eq!(
                    alias_op["operationId"],
                    format!("{id}_default"),
                    "{method} {alias_path}"
                );
            }
            aliases += 1;
        }
    }
    // 23 P3 operations and the 6 channel and event-settings operations (P5).
    assert_eq!(aliases, 29, "every mock and profile operation has an alias");
    assert!(paths["/api/v1/mocks"]["post"]["deprecated"] == true);

    let schemas = doc["components"]["schemas"].as_object().unwrap();
    for name in [
        "WorkspaceView",
        "CreateWorkspace",
        "PatchWorkspace",
        "DataPlaneAuthConfig",
    ] {
        assert!(schemas.contains_key(name), "schema {name} missing");
    }
}

/// Generated clients key operations by id, so every id must be unique.
#[tokio::test]
async fn openapi_operation_ids_are_unique() {
    let h = harness().await;
    let doc = h.get("/api/openapi.json").await.ok(StatusCode::OK);
    let mut seen = std::collections::BTreeSet::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            if let Some(id) = op["operationId"].as_str() {
                assert!(
                    seen.insert(id.to_string()),
                    "duplicate {id} at {method} {path}"
                );
            }
        }
    }
    let paths = &doc["paths"];
    assert_eq!(
        paths["/api/v1/workspaces/{ws}/mocks"]["get"]["operationId"],
        "mocks_list"
    );
    assert_eq!(
        paths["/api/v1/workspaces/{ws}/mocks/{mock}/contracts"]["get"]["operationId"],
        "contracts_list"
    );
    assert_eq!(
        paths["/api/v1/mocks"]["get"]["operationId"],
        "mocks_list_default"
    );
    assert_eq!(paths["/api/v1/me"]["get"]["operationId"], "me");
}

/// Authenticates every request as a viewer of `team-a` only.
struct TeamAViewer;

#[async_trait::async_trait]
impl Authenticate for TeamAViewer {
    async fn authenticate(&self, _: &HeaderMap) -> Result<Option<Principal>, AuthError> {
        Ok(Some(Principal {
            subject: "v".into(),
            display_name: "V".into(),
            kind: PrincipalKind::User,
            sysadmin: false,
            roles: BTreeMap::from([("team-a".to_string(), Role::Viewer)]),
        }))
    }
}

#[tokio::test]
async fn list_shows_only_visible_workspaces() {
    let mut h = harness().await;
    for ws in ["team-a", "team-b"] {
        h.create_workspace(ws).await;
    }
    // The Local principal is a sysadmin and sees every workspace as owner.
    let all = h.get("/api/v1/workspaces").await.ok(StatusCode::OK);
    assert_eq!(all.as_array().unwrap().len(), 3, "{all}");
    assert!(
        all.as_array().unwrap().iter().all(|w| w["role"] == "owner"),
        "{all}"
    );

    h.app = router(AdminState {
        auth: Arc::new(TeamAViewer),
        ..AdminState::new(h.store.clone(), h.state.clone())
    });
    let list = h.get("/api/v1/workspaces").await.ok(StatusCode::OK);
    assert_eq!(
        list,
        json!([{
            "name": "team-a",
            "display_name": "team-a",
            "created_at": list[0]["created_at"],
            "role": "viewer",
        }])
    );
}
