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
use mockingbird::file_config::Unmapped;
use mockingbird::proxy::ca::LocalCa;
use mockingbird::proxy::{ProxyState, serve};
use mockingbird::registry::MockRegistry;
use mockingbird::runtime::RuntimeDeps;
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
    let state = ProxyState::new(registry.clone(), registry.router(), ca, unmapped, images);
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
    );
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
