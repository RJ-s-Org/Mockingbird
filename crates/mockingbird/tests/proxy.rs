#![allow(clippy::unwrap_used)]
//! `mockingbird proxy`: HTTPS interception for hosts a mock claims,
//! passthrough or block for the rest, placeholder images.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mb_engine::StreamConfig;
use mb_ext::Extensions;
use mb_protocols::ProtocolSettings;
use mb_protocols::events::EventDeps;
use mb_state::{MemoryStore, StateStore};
use mb_store::{
    ContractStatus, ControlStore, DEFAULT_WORKSPACE, NewContractVersion, NewMock, ResourceSource,
    SqliteControlStore,
};
use mockingbird::config::{ServeArgs, apply_flags, build_app};
use mockingbird::file_config::{FileConfig, Unmapped};
use mockingbird::proxy::ca::LocalCa;
use mockingbird::proxy::{ProxyState, StandaloneOptions, run_standalone, serve};
use mockingbird::registry::MockRegistry;
use mockingbird::runtime::RuntimeDeps;
use mockingbird::server::{Listeners, serve_app};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::watch;

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel)
}

fn deps(store: Arc<dyn ControlStore>) -> RuntimeDeps {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    RuntimeDeps {
        store,
        state,
        ext: Extensions::default(),
        recorder: None,
        offline: true,
        default_profile: None.into(),
        data_plane_keys: None,
        protocols: ProtocolSettings::default(),
        stream: StreamConfig::default(),
        events: EventDeps::default(),
        limiter: mb_engine::GenerationLimiter::new(64),
        scrubber: None,
        limits: Default::default(),
        http: Default::default(),
        mode: mockingbird::runtime::ServeMode::Full,
    }
}

/// A registry with one mock `orders` at `/` for `host`.
async fn hosted_registry(host: &str) -> Arc<MockRegistry> {
    let store: Arc<dyn ControlStore> = Arc::new(
        SqliteControlStore::connect("sqlite::memory:")
            .await
            .unwrap(),
    );
    let mock = store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: "orders".into(),
                route_prefix: Some("/".into()),
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::Api,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: Default::default(),
                hosts: vec![host.into()],
            },
        )
        .await
        .unwrap();
    let source = std::fs::read(fixture("openapi/orders-3.0.yaml")).unwrap();
    let v = store
        .add_contract_version(
            mock.id,
            NewContractVersion {
                bundle: mb_import::compile(&source).unwrap(),
                source,
                status: ContractStatus::Approved,
                created_by: None,
            },
        )
        .await
        .unwrap();
    store
        .activate_contract_version(mock.id, v.version)
        .await
        .unwrap();
    let registry = MockRegistry::new();
    registry.reload_all(&deps(store)).await.unwrap();
    registry
}

async fn start_proxy(
    registry: Arc<MockRegistry>,
    unmapped: Unmapped,
    images: Vec<String>,
) -> (String, TempDir, watch::Sender<bool>) {
    let dir = tempfile::tempdir().unwrap();
    let ca = Arc::new(LocalCa::load_or_create(dir.path()).unwrap());
    let state = ProxyState::new(registry.clone(), registry.router(), ca, unmapped, images).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stop_rx) = watch::channel(false);
    tokio::spawn(serve(listener, state, stop_rx));
    (url, dir, stop)
}

fn client(proxy_url: &str, ca_pem: &[u8]) -> reqwest::Client {
    reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(proxy_url).unwrap())
        .tls_certs_only([reqwest::Certificate::from_pem(ca_pem).unwrap()])
        .build()
        .unwrap()
}

/// A TLS server with the fixture certificate (`localhost`), answering
/// "upstream" to every request.
async fn fixture_tls_server() -> std::net::SocketAddr {
    let rustls = mockingbird::tls::load_rustls(&mockingbird::file_config::TlsConfig {
        cert_file: fixture("tls/server.pem"),
        key_file: fixture("tls/server-key.pem"),
    })
    .await
    .unwrap();
    let router = axum::Router::new().fallback(|| async { "upstream" });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let server = axum_server::from_tcp_rustls(listener, rustls).unwrap();
    tokio::spawn(server.serve(router.into_make_service()));
    addr
}

#[tokio::test]
async fn intercepts_https_for_a_mapped_host() {
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) = start_proxy(registry, Unmapped::Pass, vec![]).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let r = client(&proxy, &ca)
        .get("https://api.orders.test/orders")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().contains_key("x-mockingbird-source"));
}

#[tokio::test]
async fn plain_http_to_a_mapped_host_is_served() {
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) = start_proxy(registry, Unmapped::Block, vec![]).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let r = client(&proxy, &ca)
        .get("http://api.orders.test/orders")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn unmapped_connect_is_tunnelled_untouched() {
    let upstream = fixture_tls_server().await;
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, _ca, _stop) = start_proxy(registry, Unmapped::Pass, vec![]).await;
    let fixture_ca = std::fs::read(fixture("tls/ca.pem")).unwrap();
    let r = client(&proxy, &fixture_ca)
        .get(format!("https://localhost:{}/", upstream.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.text().await.unwrap(), "upstream");
}

#[tokio::test]
async fn block_mode_answers_502_naming_the_host() {
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) = start_proxy(registry, Unmapped::Block, vec![]).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let c = client(&proxy, &ca);
    let r = c.get("http://elsewhere.test/x").send().await.unwrap();
    assert_eq!(r.status(), 502);
    assert_eq!(r.headers()["content-type"], "application/problem+json");
    assert!(r.text().await.unwrap().contains("elsewhere.test"));
    assert!(c.get("https://elsewhere.test/x").send().await.is_err());
}

#[tokio::test]
async fn image_hosts_get_placeholder_pngs() {
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) =
        start_proxy(registry, Unmapped::Block, vec!["image.tmdb.test".into()]).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let r = client(&proxy, &ca)
        .get("https://image.tmdb.test/t/p/w92/a.jpg")
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["content-type"], "image/png");
    let body = r.bytes().await.unwrap();
    assert_eq!(u32::from_be_bytes(body[16..20].try_into().unwrap()), 92);
}

#[test]
fn unmapped_hosts_are_logged_once() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let registry = rt.block_on(hosted_registry("a.test"));
    let dir = tempfile::tempdir().unwrap();
    let ca = Arc::new(LocalCa::load_or_create(dir.path()).unwrap());
    let st = ProxyState::new(
        registry.clone(),
        registry.router(),
        ca,
        Unmapped::Pass,
        vec![],
    )
    .unwrap();
    assert!(st.first_sighting("x.test"));
    assert!(!st.first_sighting("x.test"));
}

#[tokio::test]
async fn forwarding_ignores_proxy_environment() {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(204))
        .mount(&upstream)
        .await;
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) = start_proxy(registry, Unmapped::Pass, vec![]).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let r = client(&proxy, &ca)
        .get(format!("{}/ping", upstream.uri()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
}

async fn ephemeral() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

#[tokio::test]
async fn serve_starts_the_proxy_from_config() {
    let dir = tempfile::tempdir().unwrap();
    let ca_dir = dir.path().join("ca");
    let orders = fixture("openapi/orders-3.0.yaml");
    let cfg = dir.path().join("mockingbird.yaml");
    std::fs::write(
        &cfg,
        format!(
            "storage:\n  url: \"sqlite::memory:\"\nproxy:\n  enabled: true\n  unmapped: block\n  ca_dir: {}\nmocks:\n  orders:\n    contract: {}\n    route_prefix: /\n    hosts: [api.orders.test]\n",
            ca_dir.display(),
            orders.display()
        ),
    )
    .unwrap();
    let app = build_app(
        &ServeArgs {
            config: Some(cfg),
            offline: true,
            ..ServeArgs::default()
        },
        None,
    )
    .await
    .unwrap();
    assert!(app.proxy.is_some());
    let (data, admin, proxy) = (ephemeral().await, ephemeral().await, ephemeral().await);
    let (admin_url, proxy_url) = (
        format!("http://{}", admin.local_addr().unwrap()),
        format!("http://{}", proxy.local_addr().unwrap()),
    );
    let (_stop, stop_rx) = watch::channel(false);
    tokio::spawn(serve_app(
        app,
        Listeners {
            data: Some(data),
            admin: Some(admin),
            grpc: None,
            proxy: Some(proxy),
        },
        stop_rx,
    ));
    let ca = std::fs::read(ca_dir.join("ca.pem")).unwrap();
    let r = client(&proxy_url, &ca)
        .get("https://api.orders.test/orders")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let hosts: Value = reqwest::get(format!("{admin_url}/proxy/hosts"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hosts, json!({"hosts": ["api.orders.test"]}));
}

#[test]
fn proxy_flags_override_config() {
    let mut cfg = FileConfig::default();
    apply_flags(
        &mut cfg,
        &ServeArgs {
            proxy: true,
            proxy_port: Some(9999),
            ..ServeArgs::default()
        },
    );
    assert!(cfg.proxy.enabled);
    assert_eq!(cfg.proxy.port, 9999);
    assert_eq!(FileConfig::default().proxy.port, 8888);
    assert_eq!(FileConfig::default().proxy.unmapped, Unmapped::Pass);
}

#[test]
fn ca_path_prints_the_certificate_path() {
    let dir = tempfile::tempdir().unwrap();
    let out = assert_cmd::Command::cargo_bin("mockingbird")
        .unwrap()
        .env("MB_CA_DIR", dir.path())
        .args(["ca", "path"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let path = dir.path().join("ca.pem");
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!("{}\n", path.display())
    );
    assert!(path.exists());
}

#[test]
fn proxy_env_prints_exports_and_json() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = |args: &[&str]| {
        assert_cmd::Command::cargo_bin("mockingbird")
            .unwrap()
            .env("MB_CA_DIR", dir.path())
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let text = String::from_utf8(cmd(&["proxy", "env", "--port", "9999"])).unwrap();
    assert!(text.contains("export NODE_USE_ENV_PROXY='1'"), "{text}");
    let json: Value = serde_json::from_slice(&cmd(&["proxy", "env", "--json"])).unwrap();
    assert_eq!(json["vars"]["HTTPS_PROXY"], "http://127.0.0.1:8888");
}

/// Serves an app with the `orders` mock on `host` (proxy disabled): data
/// plane and admin port on ephemeral ports.
async fn serve_hosted_app(host: &str) -> (String, String, watch::Sender<bool>, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("mockingbird.yaml");
    std::fs::write(
        &cfg,
        format!(
            "storage:\n  url: \"sqlite::memory:\"\nmocks:\n  orders:\n    contract: {}\n    route_prefix: /\n    hosts: [{host}]\n",
            fixture("openapi/orders-3.0.yaml").display()
        ),
    )
    .unwrap();
    let app = build_app(
        &ServeArgs {
            config: Some(cfg),
            offline: true,
            ..ServeArgs::default()
        },
        None,
    )
    .await
    .unwrap();
    let (data, admin) = (ephemeral().await, ephemeral().await);
    let urls = (
        format!("http://{}", data.local_addr().unwrap()),
        format!("http://{}", admin.local_addr().unwrap()),
    );
    let (stop, stop_rx) = watch::channel(false);
    tokio::spawn(serve_app(
        app,
        Listeners {
            data: Some(data),
            admin: Some(admin),
            grpc: None,
            proxy: None,
        },
        stop_rx,
    ));
    (urls.0, urls.1, stop, dir)
}

#[tokio::test]
async fn standalone_proxy_forwards_to_a_running_server() {
    let (data_url, admin_url, _stop, _dir) = serve_hosted_app("api.orders.test").await;
    let ca_dir = tempfile::tempdir().unwrap();
    let listener = ephemeral().await;
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let (_tx, rx) = watch::channel(false);
    tokio::spawn(run_standalone(
        listener,
        StandaloneOptions {
            data_url: data_url.parse().unwrap(),
            admin_url,
            ca_dir: ca_dir.path().into(),
            unmapped: Unmapped::Block,
            images: vec![],
        },
        rx,
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let r = client(&proxy_url, &ca)
        .get("https://api.orders.test/orders")
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "Host survives forwarding, so the hosted mock answers"
    );
}
