//! `/api/v1/workspaces/{ws}/api-keys`: create, list and revoke API keys.
#![cfg(test)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use mb_admin::{AdminState, router};
use mb_auth::{
    ApiKeys, AuthError, Authenticate, Authenticator, NewKeyRequest, Principal, PrincipalKind, Role,
};
use mb_state::{MemoryStore, StateStore};
use mb_store::ControlStore;
use serde_json::{Value, json};
use tower::ServiceExt;

/// No header → the Local principal (sysadmin); `Bearer editor|admin|owner`
/// → that role in `default`.
struct TestAuth;

#[async_trait::async_trait]
impl Authenticate for TestAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Option<Principal>, AuthError> {
        let Some(value) = headers.get(header::AUTHORIZATION) else {
            return Ok(Some(Principal::local()));
        };
        let name = value.to_str().unwrap().strip_prefix("Bearer ").unwrap();
        let role = match name {
            "editor" => Role::Editor,
            "admin" => Role::Admin,
            "owner" => Role::Owner,
            _ => return Err(AuthError::Unauthenticated("unknown token".into())),
        };
        Ok(Some(Principal {
            subject: name.into(),
            display_name: name.into(),
            kind: PrincipalKind::User,
            sysadmin: false,
            roles: BTreeMap::from([("default".to_string(), role)]),
        }))
    }
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
            self.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
        self.json()
    }
}

async fn call(
    app: &Router,
    token: Option<&str>,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> Resp {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let body = match body {
        Some(b) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
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

async fn stores() -> (Arc<dyn ControlStore>, Arc<dyn StateStore>) {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    (store, state)
}

async fn app() -> Router {
    let (store, state) = stores().await;
    router(AdminState {
        auth: Arc::new(TestAuth),
        ..AdminState::new(store, state)
    })
}

const KEYS: &str = "/api/v1/workspaces/default/api-keys";

#[tokio::test]
async fn create_list_revoke() {
    let app = app().await;
    let created = call(
        &app,
        Some("admin"),
        Method::POST,
        KEYS,
        Some(json!({"name": "ci", "role": "editor", "scopes": ["admin", "data-plane"]})),
    )
    .await
    .ok(StatusCode::CREATED);
    let key = &created["key"];
    let key_id = key["key_id"].as_str().unwrap().to_string();
    assert_eq!(key_id.len(), 12);
    assert_eq!(key["name"], "ci");
    assert_eq!(key["role"], "editor");
    assert_eq!(key["scopes"], json!(["admin", "data-plane"]));
    assert_eq!(key["sysadmin"], false);
    assert_eq!(key["created_by"], "admin");
    assert!(key["created_at"].is_string());
    assert_eq!(key["revoked_at"], Value::Null);
    assert!(
        created["token"]
            .as_str()
            .unwrap()
            .starts_with(&format!("mbk_{key_id}_"))
    );

    // Defaults: role viewer, scope admin.
    let d = call(
        &app,
        None,
        Method::POST,
        KEYS,
        Some(json!({"name": "dflt"})),
    )
    .await
    .ok(StatusCode::CREATED);
    assert_eq!(d["key"]["role"], "viewer");
    assert_eq!(d["key"]["scopes"], json!(["admin"]));

    let list = call(&app, Some("admin"), Method::GET, KEYS, None)
        .await
        .ok(StatusCode::OK);
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["key_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&key_id.as_str()));

    // Invalid names.
    call(&app, None, Method::POST, KEYS, Some(json!({"name": ""})))
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY);
    call(
        &app,
        None,
        Method::POST,
        KEYS,
        Some(json!({"name": "x".repeat(129)})),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY);

    // Another workspace does not see or revoke it.
    call(
        &app,
        None,
        Method::POST,
        "/api/v1/workspaces",
        Some(json!({"name": "team-a"})),
    )
    .await
    .ok(StatusCode::CREATED);
    let other = call(
        &app,
        None,
        Method::GET,
        "/api/v1/workspaces/team-a/api-keys",
        None,
    )
    .await
    .ok(StatusCode::OK);
    assert_eq!(other, json!([]));
    call(
        &app,
        None,
        Method::DELETE,
        &format!("/api/v1/workspaces/team-a/api-keys/{key_id}"),
        None,
    )
    .await
    .problem(StatusCode::NOT_FOUND);

    let one = format!("{KEYS}/{key_id}");
    call(&app, Some("admin"), Method::DELETE, &one, None)
        .await
        .ok(StatusCode::NO_CONTENT);
    call(&app, Some("admin"), Method::DELETE, &one, None)
        .await
        .problem(StatusCode::NOT_FOUND);
    call(
        &app,
        Some("admin"),
        Method::DELETE,
        &format!("{KEYS}/nope"),
        None,
    )
    .await
    .problem(StatusCode::NOT_FOUND);
    let list = call(&app, Some("admin"), Method::GET, KEYS, None)
        .await
        .ok(StatusCode::OK);
    let revoked = list
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["key_id"] == key_id.as_str())
        .unwrap();
    assert!(revoked["revoked_at"].is_string(), "{revoked}");
}

#[tokio::test]
async fn token_shown_once() {
    let app = app().await;
    let created = call(&app, None, Method::POST, KEYS, Some(json!({"name": "ci"})))
        .await
        .ok(StatusCode::CREATED);
    let token = created["token"].as_str().unwrap();
    assert!(created["key"].get("secret_hash").is_none(), "{created}");
    let list = call(&app, None, Method::GET, KEYS, None).await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(!list.text.contains(token), "{}", list.text);
    assert!(!list.text.contains("token"), "{}", list.text);
    assert!(!list.text.contains("secret_hash"), "{}", list.text);
    let secret = secret_of(token);
    assert!(!list.text.contains(secret), "{}", list.text);
}

/// The secret part of an `mbk_<key_id>_<secret>` token (the secret itself
/// may contain `_`).
fn secret_of(token: &str) -> &str {
    assert!(mb_auth::parse_token(token).is_some(), "{token}");
    &token["mbk_".len() + 12 + 1..]
}

#[test]
fn secret_of_handles_underscores_in_the_secret() {
    let secret = "QYS7Til8dMXkE_qTNbuuCmPzW-phLSFD8rJ8IKHM_t8";
    assert_eq!(secret.len(), 43);
    let token = format!("mbk_abcdefghijkl_{secret}");
    assert!(mb_auth::parse_token(&token).is_some());
    assert_eq!(secret_of(&token), secret);
}

#[tokio::test]
async fn editor_cannot_manage_keys() {
    let app = app().await;
    let created = call(&app, None, Method::POST, KEYS, Some(json!({"name": "ci"})))
        .await
        .ok(StatusCode::CREATED);
    let key_id = created["key"]["key_id"].as_str().unwrap();
    call(&app, Some("editor"), Method::GET, KEYS, None)
        .await
        .problem(StatusCode::FORBIDDEN);
    call(
        &app,
        Some("editor"),
        Method::POST,
        KEYS,
        Some(json!({"name": "x"})),
    )
    .await
    .problem(StatusCode::FORBIDDEN);
    call(
        &app,
        Some("editor"),
        Method::DELETE,
        &format!("{KEYS}/{key_id}"),
        None,
    )
    .await
    .problem(StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn role_above_creator_403() {
    let app = app().await;
    call(
        &app,
        Some("admin"),
        Method::POST,
        KEYS,
        Some(json!({"name": "k", "role": "owner"})),
    )
    .await
    .problem(StatusCode::FORBIDDEN);
    call(
        &app,
        Some("admin"),
        Method::POST,
        KEYS,
        Some(json!({"name": "k", "role": "admin"})),
    )
    .await
    .ok(StatusCode::CREATED);
    call(
        &app,
        Some("owner"),
        Method::POST,
        KEYS,
        Some(json!({"name": "k", "role": "owner"})),
    )
    .await
    .ok(StatusCode::CREATED);
}

#[tokio::test]
async fn sysadmin_key_requires_sysadmin() {
    let app = app().await;
    call(
        &app,
        Some("owner"),
        Method::POST,
        KEYS,
        Some(json!({"name": "k", "sysadmin": true})),
    )
    .await
    .problem(StatusCode::FORBIDDEN);
    let created = call(
        &app,
        None,
        Method::POST,
        KEYS,
        Some(json!({"name": "k", "sysadmin": true})),
    )
    .await
    .ok(StatusCode::CREATED);
    assert_eq!(created["key"]["sysadmin"], true);
}

#[tokio::test]
async fn revoked_key_401_on_admin_api() {
    let (store, state) = stores().await;
    let api_keys = ApiKeys::new(store.clone());
    let (key, token) = api_keys
        .create(
            &Principal::local(),
            "default",
            NewKeyRequest {
                name: "self".into(),
                role: Role::Admin,
                scopes: vec![],
                expires_at: None,
                sysadmin: false,
            },
        )
        .await
        .unwrap();
    let app = router(AdminState {
        auth: Arc::new(Authenticator {
            api_keys,
            sessions: None,
            bearer: None,
        }),
        ..AdminState::new(store, state)
    });
    let mocks = "/api/v1/workspaces/default/mocks";
    call(&app, Some(&token), Method::GET, mocks, None)
        .await
        .ok(StatusCode::OK);
    // The key revokes itself.
    call(
        &app,
        Some(&token),
        Method::DELETE,
        &format!("{KEYS}/{}", key.key_id),
        None,
    )
    .await
    .ok(StatusCode::NO_CONTENT);
    let r = call(&app, Some(&token), Method::GET, mocks, None).await;
    r.problem(StatusCode::UNAUTHORIZED);
    assert_eq!(
        r.headers[header::WWW_AUTHENTICATE],
        "Bearer realm=\"mockingbird\""
    );
}

#[tokio::test]
async fn api_key_cannot_create_api_keys() {
    let (store, state) = stores().await;
    let api_keys = ApiKeys::new(store.clone());
    let (_, token) = api_keys
        .create(
            &Principal::local(),
            "default",
            NewKeyRequest {
                name: "automation".into(),
                role: Role::Owner,
                scopes: vec![],
                expires_at: None,
                sysadmin: true,
            },
        )
        .await
        .unwrap();
    let app = router(AdminState {
        auth: Arc::new(Authenticator {
            api_keys,
            sessions: None,
            bearer: None,
        }),
        ..AdminState::new(store, state)
    });
    // Even a sysadmin owner key may list keys but not mint new ones.
    call(&app, Some(&token), Method::GET, KEYS, None)
        .await
        .ok(StatusCode::OK);
    let r = call(
        &app,
        Some(&token),
        Method::POST,
        KEYS,
        Some(json!({"name": "child", "role": "viewer"})),
    )
    .await;
    let p = r.problem(StatusCode::FORBIDDEN);
    assert!(p["detail"].as_str().unwrap().contains("API key"), "{p}");
    let listed = call(&app, Some(&token), Method::GET, KEYS, None)
        .await
        .ok(StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
}
