//! TLS termination (`server.tls`) and the outbound HTTP settings
//! (`outbound.ca_file`, `outbound.proxy`, `outbound.no_proxy`). The
//! certificates are the test-only ones in `fixtures/tls` (see its README):
//! `server.pem` and `server2.pem` for `localhost`/`127.0.0.1`, both signed
//! by `ca.pem`.
#![cfg(test)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use mb_llm::{
    ChatMessage, ChatRequest, HttpOptions, OpenAiCompatible, OpenAiConfig, Provider, ResponseFormat,
};
use mockingbird::config::{App, ServeArgs, build_app};
use mockingbird::file_config::TlsConfig;
use mockingbird::server::{Listeners, serve_app};
use mockingbird::tls;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel)
}

fn ca_pem() -> Vec<u8> {
    std::fs::read(fixture("tls/ca.pem")).unwrap()
}

/// The DER of the first certificate in the PEM file `path`.
fn cert_der(path: &Path) -> Vec<u8> {
    CertificateDer::from_pem_file(path).unwrap().to_vec()
}

/// A config file in `dir` serving TLS with `cert`/`key`, plus `extra`.
fn tls_config(dir: &Path, cert: &Path, key: &Path, extra: &str) -> PathBuf {
    let path = dir.join("mockingbird.yaml");
    let yaml = format!(
        "server:\n  tls:\n    cert_file: {}\n    key_file: {}\nstorage:\n  url: sqlite://{}\nauth:\n  external_url: https://127.0.0.1:9090\n{extra}",
        cert.display(),
        key.display(),
        dir.join("mb.db").display()
    );
    std::fs::write(&path, yaml).unwrap();
    path
}

fn args(path: &Path) -> ServeArgs {
    ServeArgs {
        config: Some(path.to_path_buf()),
        offline: true,
        ..ServeArgs::default()
    }
}

async fn ephemeral() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

/// The addresses of a served app (data, admin, gRPC).
struct Running {
    data: String,
    admin: String,
    grpc: String,
    _stop: watch::Sender<bool>,
}

async fn run(app: App) -> Running {
    let (data, admin, grpc) = (ephemeral().await, ephemeral().await, ephemeral().await);
    let addr = |l: &TcpListener| l.local_addr().unwrap().to_string();
    let (d, a, g) = (addr(&data), addr(&admin), addr(&grpc));
    let (stop, stop_rx) = watch::channel(false);
    let listeners = Listeners {
        data: Some(data),
        admin: Some(admin),
        grpc: Some(grpc),
        proxy: None,
    };
    tokio::spawn(async move { serve_app(app, listeners, stop_rx).await.unwrap() });
    Running {
        data: d,
        admin: a,
        grpc: g,
        _stop: stop,
    }
}

/// A client trusting only the fixture CA, recording the server's
/// certificate, and opening a new connection per request.
fn ca_client() -> reqwest::Client {
    reqwest::Client::builder()
        .tls_certs_only([reqwest::Certificate::from_pem(&ca_pem()).unwrap()])
        .tls_info(true)
        .pool_max_idle_per_host(0)
        .no_proxy()
        .build()
        .unwrap()
}

async fn fixture_app(dir: &Path) -> App {
    let path = tls_config(
        dir,
        &fixture("tls/server.pem"),
        &fixture("tls/server-key.pem"),
        "",
    );
    build_app(&args(&path), None).await.unwrap()
}

#[tokio::test]
async fn serves_data_admin_and_grpc_over_tls() {
    let dir = tempfile::tempdir().unwrap();
    let running = run(fixture_app(dir.path()).await).await;
    let client = ca_client();
    let data = client
        .get(format!("https://{}/__mockingbird/healthz", running.data))
        .send()
        .await
        .unwrap();
    assert_eq!(data.status(), 200);
    let admin = client
        .get(format!("https://{}/healthz", running.admin))
        .send()
        .await
        .unwrap();
    assert_eq!(admin.status(), 200);

    let tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(ca_pem()))
        .domain_name("localhost");
    let channel = tonic::transport::Endpoint::from_shared(format!("https://{}", running.grpc))
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut health = tonic_health::pb::health_client::HealthClient::new(channel);
    let resp = health
        .check(tonic_health::pb::HealthCheckRequest {
            service: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        resp.status,
        tonic_health::pb::health_check_response::ServingStatus::Serving as i32
    );
}

#[tokio::test]
async fn plain_http_refused_when_tls_on() {
    let dir = tempfile::tempdir().unwrap();
    let running = run(fixture_app(dir.path()).await).await;
    let plain = reqwest::Client::builder().no_proxy().build().unwrap();
    for url in [
        format!("http://{}/__mockingbird/healthz", running.data),
        format!("http://{}/healthz", running.admin),
    ] {
        let got = plain.get(&url).send().await;
        assert!(got.is_err(), "{url} answered plain HTTP: {got:?}");
    }
}

/// The DER of the certificate `url`'s server presents.
async fn presented_cert(url: &str) -> Vec<u8> {
    let resp = ca_client().get(url).send().await.unwrap();
    resp.extensions()
        .get::<reqwest::tls::TlsInfo>()
        .and_then(|info| info.peer_certificate())
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn cert_reload_on_change() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::copy(fixture("tls/server.pem"), &cert).unwrap();
    std::fs::copy(fixture("tls/server-key.pem"), &key).unwrap();
    let path = tls_config(dir.path(), &cert, &key, "");
    let app = build_app(&args(&path), None).await.unwrap();
    let rustls = app.tls.clone().unwrap();
    let running = run(app).await;
    let url = format!("https://{}/__mockingbird/healthz", running.data);
    assert_eq!(
        presented_cert(&url).await,
        cert_der(&fixture("tls/server.pem"))
    );

    let cfg = TlsConfig {
        cert_file: cert.clone(),
        key_file: key.clone(),
    };
    let _reloader = tls::spawn_reloader_every(cfg, rustls, Duration::from_millis(50));
    // A broken pair is logged and the old certificate kept.
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&cert, "not a certificate").unwrap();
    touch(&cert, 5);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        presented_cert(&url).await,
        cert_der(&fixture("tls/server.pem"))
    );

    std::fs::copy(fixture("tls/server2.pem"), &cert).unwrap();
    std::fs::copy(fixture("tls/server2-key.pem"), &key).unwrap();
    touch(&cert, 10);
    touch(&key, 10);
    let want = cert_der(&fixture("tls/server2.pem"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while presented_cert(&url).await != want {
        assert!(Instant::now() < deadline, "the new certificate within 10 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Sets `path`'s mtime `secs` seconds into the future, so the change is
/// seen even on a coarse-grained file system.
fn touch(path: &Path, secs: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(secs))
        .unwrap();
}

#[tokio::test]
async fn bad_cert_file_is_startup_error() {
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("cert.pem");
    std::fs::write(&cert, "not a certificate").unwrap();
    let path = tls_config(dir.path(), &cert, &fixture("tls/server-key.pem"), "");
    let err = build_app(&args(&path), None).await.err().unwrap();
    let msg = format!("{err:#}");
    assert!(msg.contains("server.tls.cert_file"), "{msg}");

    let missing = dir.path().join("missing-key.pem");
    let path = tls_config(dir.path(), &fixture("tls/server.pem"), &missing, "");
    let err = build_app(&args(&path), None).await.err().unwrap();
    let msg = format!("{err:#}");
    assert!(msg.contains("server.tls.key_file"), "{msg}");
}

#[tokio::test]
async fn bad_outbound_ca_is_startup_error() {
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, "not a certificate").unwrap();
    let path = dir.path().join("mockingbird.yaml");
    std::fs::write(&path, format!("outbound:\n  ca_file: {}\n", ca.display())).unwrap();
    let err = build_app(&args(&path), None).await.err().unwrap();
    let msg = format!("{err:#}");
    assert!(msg.contains("outbound.ca_file"), "{msg}");
}

#[test]
fn outbound_options_read_the_ca_bundle() {
    let cfg = mockingbird::file_config::OutboundConfig {
        ca_file: Some(fixture("tls/ca.pem")),
        proxy: Some("http://proxy.internal:3128".into()),
        no_proxy: vec![".corp.example".into()],
    };
    let opts = tls::outbound_options(&cfg).unwrap();
    assert_eq!(opts.extra_roots_pem, ca_pem());
    assert_eq!(opts.proxy.as_deref(), Some("http://proxy.internal:3128"));
    assert_eq!(opts.no_proxy, vec![".corp.example".to_string()]);
}

fn chat() -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage {
            role: mb_llm::Role::User,
            content: "hi".into(),
        }],
        response_format: ResponseFormat::JsonObject,
        temperature: 0.2,
        max_tokens: 50,
        timeout: Duration::from_secs(5),
    }
}

fn completion() -> serde_json::Value {
    json!({"choices":[{"message":{"content":"{\"ok\":true}"}}],"usage":{"prompt_tokens":1,"completion_tokens":1}})
}

fn provider(base_url: String, http: HttpOptions) -> OpenAiCompatible {
    OpenAiCompatible::new(OpenAiConfig {
        headers: Vec::new(),
        extra_body: Default::default(),
        base_url,
        api_key: None,
        model: "m".into(),
        json_schema: false,
        http,
    })
    .unwrap()
}

#[tokio::test]
async fn llm_client_trusts_outbound_ca() {
    // An HTTPS stub LLM with the fixture certificate.
    let rustls = tls::load_rustls(&TlsConfig {
        cert_file: fixture("tls/server.pem"),
        key_file: fixture("tls/server-key.pem"),
    })
    .await
    .unwrap();
    let router = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|| async { axum::Json(completion()) }),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let server = axum_server::from_tcp_rustls(listener, rustls).unwrap();
    tokio::spawn(server.serve(router.into_make_service()));
    let base = format!("https://localhost:{}/v1", addr.port());

    let trusting = provider(
        base.clone(),
        HttpOptions {
            extra_roots_pem: ca_pem(),
            ..HttpOptions::default()
        },
    );
    let resp = trusting.complete(chat()).await.unwrap();
    assert_eq!(resp.content, "{\"ok\":true}");

    let untrusting = provider(base, HttpOptions::default());
    assert!(untrusting.complete(chat()).await.is_err());
}

/// A stub HTTP proxy: answers every request with a chat completion and
/// records each request line.
async fn stub_proxy() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = ephemeral().await;
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let record = record.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let line = head.lines().next().unwrap_or_default().to_string();
                record.lock().unwrap().push(line);
                let body = completion().to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (url, seen)
}

#[tokio::test]
async fn llm_client_uses_proxy() {
    let (proxy, seen) = stub_proxy().await;
    let llm = provider(
        "http://llm.invalid/v1".into(),
        HttpOptions {
            proxy: Some(proxy),
            ..HttpOptions::default()
        },
    );
    let resp = llm.complete(chat()).await.unwrap();
    assert_eq!(resp.content, "{\"ok\":true}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].starts_with("POST http://llm.invalid/v1/chat/completions")
            || seen[0].starts_with("CONNECT llm.invalid"),
        "{seen:?}"
    );
}

#[tokio::test]
async fn no_proxy_bypasses() {
    let (proxy, seen) = stub_proxy().await;
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(completion()))
        .mount(&upstream)
        .await;
    let llm = provider(
        format!("{}/v1", upstream.uri()),
        HttpOptions {
            proxy: Some(proxy),
            no_proxy: vec!["127.0.0.1".into(), ".internal.example".into()],
            ..HttpOptions::default()
        },
    );
    llm.complete(chat()).await.unwrap();
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn external_url_must_be_https_with_tls() {
    let dir = tempfile::tempdir().unwrap();
    let path = tls_config(
        dir.path(),
        &fixture("tls/server.pem"),
        &fixture("tls/server-key.pem"),
        "",
    );
    let yaml = std::fs::read_to_string(&path)
        .unwrap()
        .replace("https://127.0.0.1:9090", "http://127.0.0.1:9090");
    std::fs::write(&path, yaml).unwrap();
    let err = build_app(&args(&path), None).await.err().unwrap();
    let msg = format!("{err:#}");
    assert!(msg.contains("auth.external_url"), "{msg}");
    assert!(msg.contains("https://"), "{msg}");
}
