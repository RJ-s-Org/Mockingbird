//! `serve` with `auth.mode: oidc`: authentication of the Admin API, API
//! keys from the command line, the host guard, a private `/metrics`, OIDC
//! discovery at startup and the `openapi` subcommand.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use assert_cmd::Command;
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use mb_auth::testing::MockOidcProvider;
use mb_ext::{ExtError, Extensions, SecretStore};
use mockingbird::config::{App, ServeArgs, build_app_with};
use serde_json::{Value, json};
use tower::ServiceExt;

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// Resolves exactly one `env:` reference, to the provider's client secret,
/// so no test has to touch the process environment.
struct OneSecret(String);

#[async_trait]
impl SecretStore for OneSecret {
    async fn resolve(&self, reference: &str) -> Result<String, ExtError> {
        if reference == self.0 {
            Ok(MockOidcProvider::CLIENT_SECRET.to_string())
        } else {
            Err(ExtError::SecretNotFound(reference.to_string()))
        }
    }
}

struct Harness {
    app: App,
    provider: MockOidcProvider,
    /// `host:port` of `auth.external_url`.
    external_host: String,
    _dir: tempfile::TempDir,
}

/// Writes an `oidc`-mode config (plus `extra` YAML under `auth:`) whose
/// `external_url` is `http://<external_host>`, and builds the app.
async fn build(
    provider_issuer: &str,
    external_host: &str,
    extra_auth: &str,
) -> (anyhow::Result<App>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let var = format!(
        "MB_TEST_OIDC_SECRET_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let reference = format!("env:{var}");
    let yaml = format!(
        "auth:\n  mode: oidc\n  external_url: http://{external_host}\n{extra_auth}  oidc:\n    issuer: {provider_issuer}\n    client_id: {}\n    client_secret: {reference}\n",
        MockOidcProvider::CLIENT_ID
    );
    let path = dir.path().join("mockingbird.yaml");
    std::fs::write(&path, yaml).unwrap();
    let args = ServeArgs {
        config: Some(path),
        offline: true,
        ..ServeArgs::default()
    };
    let ext = Extensions::default().with_secrets(Arc::new(OneSecret(reference)));
    (build_app_with(&args, None, ext).await, dir)
}

async fn harness_with(external_host: &str, extra_auth: &str) -> Harness {
    let provider = MockOidcProvider::start().await;
    let (app, dir) = build(&provider.issuer(), external_host, extra_auth).await;
    Harness {
        app: app.unwrap(),
        provider,
        external_host: external_host.to_string(),
        _dir: dir,
    }
}

async fn harness() -> Harness {
    harness_with("mb.example.com:9090", "").await
}

impl Harness {
    fn jwt(&self, sub: &str, groups: &[&str]) -> String {
        self.provider.sign(self.provider.claims(sub, groups, None))
    }

    /// Sends a request to the admin router with `Host` set to the external
    /// host unless `host` overrides it.
    async fn send(
        &self,
        method: &str,
        uri: &str,
        host: Option<&str>,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, host.unwrap_or(&self.external_host));
        if let Some(token) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let req = match body {
            Some(body) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let resp = self.app.admin.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }
}

#[tokio::test]
async fn oidc_mode_api_requires_auth() {
    let h = harness().await;
    let (status, body) = h
        .send("GET", "/api/v1/workspaces/default/mocks", None, None, None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let viewer = h.jwt("alice", &["mb:default:viewer"]);
    let (status, body) = h
        .send(
            "GET",
            "/api/v1/workspaces/default/mocks",
            None,
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = h
        .send(
            "POST",
            "/api/v1/workspaces/default/mocks",
            None,
            Some(&viewer),
            Some(json!({"name": "orders"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // A JWT from another issuer is rejected.
    let other = MockOidcProvider::start().await;
    let foreign = other.sign(other.claims("mallory", &["mb:sysadmin"], None));
    let (status, _) = h
        .send("GET", "/api/v1/mocks", None, Some(&foreign), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Runs the binary off the async runtime so the server keeps serving.
async fn cli(args: Vec<String>) -> (i32, String, String) {
    tokio::task::spawn_blocking(move || {
        let out = Command::cargo_bin("mockingbird")
            .unwrap()
            .args(&args)
            .env_remove("MB_ADMIN_URL")
            .env_remove("MB_ADMIN_TOKEN")
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_key_token_from_cli() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let h = harness_with(&addr, "").await;
    let router = h.app.admin.clone();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let sysadmin = h.jwt("root", &["mb:sysadmin"]);
    let (status, body) = h
        .send(
            "POST",
            "/api/v1/workspaces/default/api-keys",
            None,
            Some(&sysadmin),
            Some(json!({"name": "cli", "role": "viewer"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let created: Value = serde_json::from_str(&body).unwrap();
    let token = created["token"].as_str().unwrap().to_string();

    let admin_url = format!("http://{addr}");
    let (code, stdout, stderr) = cli(vec![
        "--admin-url".into(),
        admin_url.clone(),
        "--token".into(),
        token,
        "mocks".into(),
        "list".into(),
    ])
    .await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(serde_json::from_str::<Value>(&stdout).unwrap(), json!([]));

    let (code, _, stderr) = cli(vec![
        "--admin-url".into(),
        admin_url,
        "mocks".into(),
        "list".into(),
    ])
    .await;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("Unauthorized"), "{stderr}");
}

#[tokio::test]
async fn host_guard_allows_external_host_only() {
    let h = harness().await;
    let (status, body) = h
        .send("GET", "/api/v1/mocks", Some("evil"), None, None)
        .await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{body}");
    let (status, _) = h
        .send("GET", "/api/v1/mocks", Some("127.0.0.1:9090"), None, None)
        .await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);

    let (status, _) = h.send("GET", "/api/v1/mocks", None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = h
        .send(
            "GET",
            "/api/v1/mocks",
            Some("MB.Example.COM:9090"),
            None,
            None,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "host names compare caselessly"
    );
    let viewer = h.jwt("alice", &["mb:default:viewer"]);
    let (status, _) = h
        .send("GET", "/api/v1/mocks", None, Some(&viewer), None)
        .await;
    assert_eq!(status, StatusCode::OK);

    // Probes send pod IPs as Host.
    for path in ["/healthz", "/readyz", "/metrics"] {
        let (status, body) = h.send("GET", path, Some("10.0.0.5"), None, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        if path == "/readyz" {
            // Anonymous callers in oidc mode get no reasons (the status,
            // role and leader flag only).
            let body: Value = serde_json::from_str(&body).unwrap();
            assert!(body.get("reasons").is_none(), "{body}");
            assert_eq!(body.as_object().unwrap().len(), 3, "{body}");
        }
    }
}

#[tokio::test]
async fn metrics_private_when_configured() {
    let h = harness_with("mb.example.com", "  public_metrics: false\n").await;
    let (status, _) = h.send("GET", "/metrics", None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let viewer = h.jwt("alice", &["mb:default:viewer"]);
    let (status, body) = h.send("GET", "/metrics", None, Some(&viewer), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The probes stay public.
    let (status, _) = h.send("GET", "/healthz", None, None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn discovery_failure_is_startup_error() {
    // Nothing listens on port 1.
    let issuer = "http://127.0.0.1:1/realm";
    let (app, _dir) = build(issuer, "mb.example.com", "").await;
    let err = match app {
        Ok(_) => panic!("startup succeeded without a reachable issuer"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains(issuer), "{err}");
}

#[tokio::test]
async fn openapi_subcommand_prints_document() {
    let (code, stdout, stderr) = cli(vec!["openapi".into()]).await;
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).unwrap();
    let paths = doc["paths"].as_object().unwrap();
    assert!(paths.contains_key("/api/v1/workspaces/{ws}/mocks"));
    assert!(paths.contains_key("/api/v1/mocks"));
    assert_eq!(paths["/api/v1/mocks"]["get"]["deprecated"], json!(true));
}

#[tokio::test]
async fn console_served_behind_login() {
    let h = harness().await;
    let nav = |token: Option<String>| {
        let mut req = Request::builder()
            .uri("/w/default/mocks?q=1")
            .header(header::HOST, &h.external_host)
            .header(header::ACCEPT, "text/html,application/xhtml+xml");
        if let Some(token) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        req.body(Body::empty()).unwrap()
    };
    let resp = h.app.admin.clone().oneshot(nav(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        resp.headers()[header::LOCATION],
        "/auth/login?return_to=%2Fw%2Fdefault%2Fmocks%3Fq%3D1"
    );

    let viewer = h.jwt("alice", &["mb:default:viewer"]);
    let resp = h
        .app
        .admin
        .clone()
        .oneshot(nav(Some(viewer)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()[header::CONTENT_SECURITY_POLICY],
        "default-src 'self'; frame-ancestors 'none'"
    );
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-cache");

    // Unknown API paths stay problems, not the console.
    let (status, body) = h.send("GET", "/api/v2/nope", None, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(!body.contains("<html"), "{body}");
}

/// An extension route that echoes the caller's subject.
struct EchoHook;

impl mb_ext::AdminHooks for EchoHook {
    fn routes(&self) -> Option<axum::Router> {
        Some(axum::Router::new().route(
            "/api/v1/ext/whoami",
            axum::routing::get(
                |axum::Extension(p): axum::Extension<mb_auth::Principal>| async move { p.subject },
            ),
        ))
    }
}

#[tokio::test]
async fn admin_hook_routes_are_authenticated() {
    let provider = MockOidcProvider::start().await;
    let dir = tempfile::tempdir().unwrap();
    let reference = format!(
        "env:MB_TEST_OIDC_SECRET_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let yaml = format!(
        "auth:\n  mode: oidc\n  external_url: http://mb.example.com\n  oidc:\n    issuer: {}\n    client_id: {}\n    client_secret: {reference}\n",
        provider.issuer(),
        MockOidcProvider::CLIENT_ID
    );
    let path = dir.path().join("mockingbird.yaml");
    std::fs::write(&path, yaml).unwrap();
    let args = ServeArgs {
        config: Some(path),
        offline: true,
        ..ServeArgs::default()
    };
    let ext = Extensions::default()
        .with_secrets(Arc::new(OneSecret(reference)))
        .with_admin_hooks(Arc::new(EchoHook));
    let h = Harness {
        app: build_app_with(&args, None, ext).await.unwrap(),
        provider,
        external_host: "mb.example.com".to_string(),
        _dir: dir,
    };

    let (status, body) = h.send("GET", "/api/v1/ext/whoami", None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let (status, _) = h
        .send("GET", "/api/v1/ext/whoami", None, Some("garbage"), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let token = h.jwt("alice", &["mb:default:viewer"]);
    let (status, body) = h
        .send("GET", "/api/v1/ext/whoami", None, Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "alice");

    // The hook's routes do not change the console fallback or the probes.
    let req = Request::builder()
        .uri("/w/default/mocks")
        .header(header::HOST, &h.external_host)
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();
    let resp = h.app.admin.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    let (status, _) = h.send("GET", "/healthz", None, None, None).await;
    assert_eq!(status, StatusCode::OK);
}
