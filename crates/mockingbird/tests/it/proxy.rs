//! `mockingbird proxy`: HTTPS interception for hosts a mock claims,
//! passthrough or block for the rest, placeholder images.
#![cfg(test)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::support::{ephemeral, fixture, wait_until};
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
        webhook_log: Default::default(),
    }
}

/// A registry with one mock `orders` at `/` for `host`.
async fn hosted_registry(host: &str) -> Arc<MockRegistry> {
    hosted_registry_with(host, "/", Default::default()).await
}

/// A registry with one mock `orders` at `prefix` for `host`.
async fn hosted_registry_with(
    host: &str,
    prefix: &str,
    behavior: mb_store::MockBehavior,
) -> Arc<MockRegistry> {
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
                route_prefix: Some(prefix.into()),
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::Api,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: Default::default(),
                hosts: vec![host.into()],
                behavior,
                webhooks: Vec::new(),
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
    // The proxy writes its CA as it starts.
    let ca_pem = ca_dir.path().join("ca.pem");
    wait_until("the proxy writes ca.pem", Duration::from_secs(5), || {
        std::fs::read_to_string(&ca_pem).is_ok_and(|pem| pem.contains("-----END CERTIFICATE-----"))
    })
    .await;
    let ca = std::fs::read(&ca_pem).unwrap();
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

#[tokio::test]
async fn pac_file_routes_through_the_proxy() {
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, _ca_dir, _stop) = start_proxy(registry, Unmapped::Block, vec![]).await;
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{proxy}/proxy.pac"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-type"],
        "application/x-ns-proxy-autoconfig"
    );
    let text = r.text().await.unwrap();
    let authority = proxy.trim_start_matches("http://");
    assert!(text.contains(&format!("\"PROXY {authority}\"")), "{text}");
    assert!(text.contains("FindProxyForURL"));
}

#[tokio::test]
async fn plain_http_inside_connect_is_served() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, _ca_dir, _stop) = start_proxy(registry, Unmapped::Block, vec![]).await;
    let mut tcp = tokio::net::TcpStream::connect(proxy.trim_start_matches("http://"))
        .await
        .unwrap();
    tcp.write_all(b"CONNECT api.orders.test:80 HTTP/1.1\r\nHost: api.orders.test:80\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 1024];
    let n = tcp.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    tcp.write_all(b"GET /orders HTTP/1.1\r\nHost: api.orders.test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    tcp.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
}

#[tokio::test]
async fn image_path_prefixes_get_placeholders_on_a_mock_host() {
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) = start_proxy(
        registry,
        Unmapped::Block,
        vec!["api.orders.test/ipx".into()],
    )
    .await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let http = client(&proxy, &ca);
    let r = http
        .get("https://api.orders.test/ipx/s_40x60/tmdb/a.jpg")
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["content-type"], "image/png");
    let body = r.bytes().await.unwrap();
    assert_eq!(u32::from_be_bytes(body[16..20].try_into().unwrap()), 40);
    assert_eq!(u32::from_be_bytes(body[20..24].try_into().unwrap()), 60);
    let r = http
        .get("https://api.orders.test/orders")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_ne!(r.headers()["content-type"], "image/png");
}

#[tokio::test]
async fn path_rewrite_serves_a_contract_under_another_base_path() {
    let behavior = mb_store::MockBehavior {
        path_rewrite: mb_store::PathRewrite {
            strip_prefix: Some("/v9".into()),
            add_prefix: None,
        },
        ..Default::default()
    };
    let registry = hosted_registry_with("api.orders.test", "/api", behavior).await;
    let (proxy, ca_dir, _stop) = start_proxy(registry, Unmapped::Block, vec![]).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let r = client(&proxy, &ca)
        .get("https://api.orders.test/api/v9/orders")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
}

/// `node`, when installed.
fn node() -> Option<PathBuf> {
    let out = std::process::Command::new("node")
        .arg("--version")
        .output()
        .ok()?;
    out.status.success().then(|| PathBuf::from("node"))
}

/// A custom `https.Agent` (as stripe-node creates), `fetch` and a custom
/// `http.Agent` all reach the mock through the preload, with and without
/// Node's own `NODE_USE_ENV_PROXY`. Unmapped hosts are blocked, so a
/// request that bypassed the proxy would fail.
#[tokio::test(flavor = "multi_thread")]
async fn node_preload_routes_custom_agents_and_fetch() {
    let Some(node) = node() else {
        eprintln!("skipped: node is not installed");
        return;
    };
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) = start_proxy(registry, Unmapped::Block, vec![]).await;
    let port: u16 = proxy.rsplit(':').next().unwrap().parse().unwrap();
    let env = mockingbird::proxy::env::proxy_env_with(
        &mockingbird::proxy::env::ProxyEnvOptions {
            host: "127.0.0.1".into(),
            port,
            ca_dir: ca_dir.path().into(),
        },
        |_| None,
    )
    .unwrap();
    let script = r#"
const https = require('https');
const http = require('http');
const get = (mod, url, agent) => new Promise((resolve, reject) => {
  mod.get(url, { agent }, (r) => { r.resume(); r.on('end', () => resolve(r.statusCode)); })
    .on('error', reject);
});
(async () => {
  const agent = await get(https, 'https://api.orders.test/orders', new https.Agent({ keepAlive: true }));
  const fetched = (await fetch('https://api.orders.test/orders')).status;
  const plain = await get(http, 'http://api.orders.test/orders', new http.Agent());
  console.log(`agent=${agent} fetch=${fetched} http=${plain}`);
  process.exit(0);
})().catch((e) => { console.error(e); process.exit(1); });
"#;
    for use_env_proxy in [true, false] {
        let mut vars = env.vars.clone();
        if !use_env_proxy {
            vars.remove("NODE_USE_ENV_PROXY");
        }
        let node = node.clone();
        let script = script.to_owned();
        let out = tokio::task::spawn_blocking(move || {
            std::process::Command::new(node)
                .arg("-e")
                .arg(script)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .envs(&vars)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.trim(),
            "agent=200 fetch=200 http=200",
            "NODE_USE_ENV_PROXY={use_env_proxy}; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// `python3` (or `MB_TEST_PYTHON`) when it can import urllib3 and certifi.
fn python_with_urllib3() -> Option<PathBuf> {
    let python = std::env::var_os("MB_TEST_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("python3"));
    let out = std::process::Command::new(&python)
        .args(["-c", "import urllib3, certifi"])
        .output()
        .ok()?;
    out.status.success().then_some(python)
}

/// A plain `urllib3.PoolManager` pinned to certifi's roots (as
/// OpenAPI-generated SDKs such as plaid-python build it) reaches the mock
/// through the preload; without it the request would go direct and fail,
/// since the host does not resolve.
#[tokio::test(flavor = "multi_thread")]
async fn python_preload_routes_plain_urllib3_pool_managers() {
    let Some(python) = python_with_urllib3() else {
        // CI runners have them (ubuntu-latest ships python3-urllib3 and
        // python3-certifi): a CI run that lost them must not pass silently.
        assert!(
            std::env::var_os("CI").is_none(),
            "python3 with urllib3 and certifi is missing on CI (set MB_TEST_PYTHON)"
        );
        eprintln!("skipped: python3 with urllib3 and certifi is not installed");
        return;
    };
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) = start_proxy(registry, Unmapped::Block, vec![]).await;
    let port: u16 = proxy.rsplit(':').next().unwrap().parse().unwrap();
    let env = mockingbird::proxy::env::proxy_env_with(
        &mockingbird::proxy::env::ProxyEnvOptions {
            host: "127.0.0.1".into(),
            port,
            ca_dir: ca_dir.path().into(),
        },
        |_| None,
    )
    .unwrap();
    let script = r#"
import certifi, urllib3
pool = urllib3.PoolManager(cert_reqs="CERT_REQUIRED", ca_certs=certifi.where())
r = pool.request("GET", "https://api.orders.test/orders")
print("pool=%d" % r.status)
"#;
    let vars = env.vars.clone();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(python)
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(&vars)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "pool=200",
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn proxy_browser_prints_the_chromium_flags() {
    let dir = tempfile::tempdir().unwrap();
    let out = assert_cmd::Command::cargo_bin("mockingbird")
        .unwrap()
        .env("MB_CA_DIR", dir.path())
        .args([
            "proxy",
            "browser",
            "--print",
            "--port",
            "9999",
            "--browser",
            "/bin/echo",
            "https://movies.test/",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&out).unwrap();
    let args: Vec<&str> = json["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(args[0], "--proxy-server=http://127.0.0.1:9999");
    let spki = json["ca_spki_sha256"].as_str().unwrap();
    assert!(args.contains(&format!("--ignore-certificate-errors-spki-list={spki}").as_str()));
    assert!(args.iter().any(|a| a.starts_with("--user-data-dir=")));
    assert_eq!(args.last(), Some(&"https://movies.test/"));
    assert_eq!(json["pac"], "http://127.0.0.1:9999/proxy.pac");
}
