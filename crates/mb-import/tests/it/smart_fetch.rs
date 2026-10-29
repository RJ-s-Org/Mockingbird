//! SSRF-guarded URL fetch of the smart importer.
#![cfg(test)]

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mb_import::smart::SmartError;
use mb_import::smart::fetch::{
    AllowEntry, FetchError, FetchPolicy, Resolver, fetch, fetch_with, is_blocked_ip, validate_url,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn allow(entries: &[&str]) -> FetchPolicy {
    let entries: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
    FetchPolicy::from_allowlist(&entries).unwrap()
}

struct MapResolver {
    host: &'static str,
    addrs: Vec<SocketAddr>,
    calls: AtomicUsize,
}

impl MapResolver {
    fn new(host: &'static str, addrs: Vec<SocketAddr>) -> Self {
        Self {
            host,
            addrs,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Resolver for MapResolver {
    async fn resolve(&self, host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if host == self.host {
            Ok(self.addrs.clone())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "unknown host",
            ))
        }
    }
}

#[test]
fn blocks_private_and_special_ranges() {
    for blocked in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "::1",
        "fd00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "64:ff9b::7f00:1",
    ] {
        assert!(is_blocked_ip(ip(blocked)), "{blocked} must be blocked");
    }
    for allowed in ["93.184.216.34", "2606:4700::1111"] {
        assert!(!is_blocked_ip(ip(allowed)), "{allowed} must be allowed");
    }
}

#[test]
fn rejects_bad_urls() {
    for raw in [
        "ftp://x/",
        "file:///etc/passwd",
        "http://user:pw@example.com/",
        "not a url",
    ] {
        assert!(
            matches!(validate_url(raw), Err(FetchError::InvalidUrl(_))),
            "{raw} must be rejected"
        );
    }
    assert!(validate_url("https://example.com/docs").is_ok());
}

#[tokio::test]
async fn literal_loopback_blocked_without_allowlist() {
    let server = MockServer::start().await;
    let err = fetch(&server.uri(), &FetchPolicy::default())
        .await
        .unwrap_err();
    match err {
        FetchError::Blocked { ip: addr, .. } => assert_eq!(addr, ip("127.0.0.1")),
        other => panic!("expected Blocked, got {other:?}"),
    }
}

#[tokio::test]
async fn allowlisted_cidr_fetches() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/docs.md"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("# Docs", "Text/Markdown; charset=utf-8"),
        )
        .mount(&server)
        .await;
    let fetched = fetch(
        &format!("{}/docs.md", server.uri()),
        &allow(&["127.0.0.1/32"]),
    )
    .await
    .unwrap();
    assert_eq!(fetched.bytes, b"# Docs");
    assert_eq!(fetched.content_type.as_deref(), Some("text/markdown"));
    assert_eq!(fetched.url.path(), "/docs.md");
}

#[tokio::test]
async fn resolves_once_and_pins() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/x"))
        .respond_with(ResponseTemplate::new(200).set_body_string("pinned"))
        .mount(&server)
        .await;
    let resolver = MapResolver::new("docs.test", vec![*server.address()]);
    let port = server.address().port();
    let fetched = fetch_with(
        &format!("http://docs.test:{port}/x"),
        &allow(&["docs.test"]),
        &resolver,
    )
    .await
    .unwrap();
    assert_eq!(fetched.bytes, b"pinned");
    assert_eq!(fetched.url.host_str(), Some("docs.test"));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rebinding_resolver_blocked() {
    let resolver = MapResolver::new(
        "evil.test",
        vec![
            SocketAddr::new(ip("93.184.216.34"), 80),
            SocketAddr::new(ip("127.0.0.1"), 80),
        ],
    );
    let err = fetch_with("http://evil.test/", &FetchPolicy::default(), &resolver)
        .await
        .unwrap_err();
    match err {
        FetchError::Blocked { host, ip: addr } => {
            assert_eq!(host, "evil.test");
            assert_eq!(addr, ip("127.0.0.1"));
        }
        other => panic!("expected Blocked, got {other:?}"),
    }
}

#[tokio::test]
async fn unresolvable_host() {
    let resolver = MapResolver::new("other.test", vec![]);
    let err = fetch_with("http://nowhere.test/", &FetchPolicy::default(), &resolver)
        .await
        .unwrap_err();
    assert!(
        matches!(err, FetchError::Resolve(ref h) if h == "nowhere.test"),
        "{err:?}"
    );
}

#[tokio::test]
async fn redirect_to_metadata_blocked() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "http://169.254.169.254/latest/meta-data"),
        )
        .mount(&server)
        .await;
    let err = fetch(
        &format!("{}/start", server.uri()),
        &allow(&["127.0.0.1/32"]),
    )
    .await
    .unwrap_err();
    match err {
        FetchError::Blocked { ip: addr, .. } => assert_eq!(addr, ip("169.254.169.254")),
        other => panic!("expected Blocked, got {other:?}"),
    }
}

#[tokio::test]
async fn redirect_followed_and_rechecked() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", "/b"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/b"))
        .respond_with(ResponseTemplate::new(200).set_body_string("landed"))
        .mount(&server)
        .await;
    let fetched = fetch(&format!("{}/a", server.uri()), &allow(&["127.0.0.1"]))
        .await
        .unwrap();
    assert_eq!(fetched.url.path(), "/b");
    assert_eq!(fetched.bytes, b"landed");
}

#[tokio::test]
async fn extra_roots_apply_to_every_hop_and_bad_ones_fail() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/b"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/b"))
        .respond_with(ResponseTemplate::new(200).set_body_string("landed"))
        .mount(&server)
        .await;
    let url = format!("{}/a", server.uri());
    let mut policy = allow(&["127.0.0.1"]);
    policy.extra_roots_pem = crate::support::fixture("tls/ca.pem");
    // Twice: the second fetch reuses the parsed bundle.
    for _ in 0..2 {
        let fetched = fetch(&url, &policy).await.unwrap();
        assert_eq!(fetched.bytes, b"landed");
    }
    policy.extra_roots_pem =
        b"-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n".to_vec();
    for _ in 0..2 {
        let err = fetch(&url, &policy).await.unwrap_err();
        assert!(matches!(err, FetchError::Transport(_)), "{err:?}");
    }
    policy.extra_roots_pem = crate::support::fixture("tls/ca.pem");
    assert_eq!(fetch(&url, &policy).await.unwrap().bytes, b"landed");
}

#[tokio::test]
async fn redirect_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/r"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/r"))
        .mount(&server)
        .await;
    let err = fetch(&format!("{}/r", server.uri()), &allow(&["127.0.0.1/32"]))
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::TooManyRedirects(5)), "{err:?}");
}

#[tokio::test]
async fn body_cap() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'a'; 2048]))
        .mount(&server)
        .await;
    let policy = FetchPolicy {
        max_bytes: 1024,
        ..allow(&["127.0.0.1/32"])
    };
    let err = fetch(&format!("{}/big", server.uri()), &policy)
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::TooLarge(1024)), "{err:?}");
}

#[tokio::test]
async fn timeout() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("late")
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&server)
        .await;
    let policy = FetchPolicy {
        timeout: Duration::from_millis(200),
        ..allow(&["127.0.0.1/32"])
    };
    let err = fetch(&format!("{}/slow", server.uri()), &policy)
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::Timeout), "{err:?}");
}

#[tokio::test]
async fn http_error_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let err = fetch(
        &format!("{}/missing", server.uri()),
        &allow(&["127.0.0.1/32"]),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, FetchError::Http(404)), "{err:?}");
}

#[test]
fn allow_entry_parsing() {
    assert_eq!(
        "API.Internal".parse::<AllowEntry>(),
        Ok(AllowEntry::Host("api.internal".into()))
    );
    assert_eq!(
        "*.corp.example".parse::<AllowEntry>(),
        Ok(AllowEntry::Suffix("corp.example".into()))
    );
    assert_eq!(
        "10.0.0.0/8".parse::<AllowEntry>(),
        Ok(AllowEntry::Net("10.0.0.0/8".parse().unwrap()))
    );
    assert_eq!(
        "10.1.2.3".parse::<AllowEntry>(),
        Ok(AllowEntry::Net("10.1.2.3/32".parse().unwrap()))
    );
    assert_eq!(
        "::1".parse::<AllowEntry>(),
        Ok(AllowEntry::Net("::1/128".parse().unwrap()))
    );
    assert!("".parse::<AllowEntry>().is_err());
    assert!("10.0.0.0/33".parse::<AllowEntry>().is_err());
    assert!(FetchPolicy::from_allowlist(&["10.0.0.0/33".to_string()]).is_err());
}

#[test]
fn suffix_entry_matches_subdomains_only() {
    let policy = allow(&["*.corp.example"]);
    assert!(policy.host_allowed("api.corp.example"));
    assert!(policy.host_allowed("API.Corp.Example"));
    assert!(!policy.host_allowed("corp.example"));
    assert!(!policy.host_allowed("evilcorp.example"));
}

#[test]
fn defaults_match_constants() {
    let policy = FetchPolicy::default();
    assert!(policy.allowlist.is_empty());
    assert_eq!(policy.max_bytes, 10 * 1024 * 1024);
    assert_eq!(policy.timeout, Duration::from_secs(20));
    assert_eq!(policy.max_redirects, 5);
}

#[test]
fn fetch_error_converts_to_smart_error() {
    let err: SmartError = FetchError::Http(500).into();
    assert!(matches!(err, SmartError::Fetch(ref m) if m == "the server answered HTTP 500"));
}

#[tokio::test]
async fn blocked_destination_does_not_echo_the_address() {
    let resolver = MapResolver::new(
        "intranet.test",
        vec![SocketAddr::new(ip("10.20.30.40"), 80)],
    );
    let err = fetch_with("http://intranet.test/", &FetchPolicy::default(), &resolver)
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::Blocked { .. }), "{err:?}");
    let shown = SmartError::from(err).to_string();
    assert!(shown.contains("destination not allowed"), "{shown}");
    assert!(!shown.contains("10.20.30.40"), "{shown}");
    assert!(!shown.contains("intranet.test"), "{shown}");
}

#[tokio::test]
async fn transport_errors_do_not_echo_the_url() {
    // A server on the (allowlisted) loopback address that hangs up on every
    // connection before answering: the fetch fails in transport. The
    // listener stays bound, so no other process can take the port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            drop(conn);
        }
    });
    let err = fetch(
        &format!("http://127.0.0.1:{port}/doc.md?sig=s3cret"),
        &allow(&["127.0.0.1/32"]),
    )
    .await
    .unwrap_err();
    server.abort();
    assert!(matches!(err, FetchError::Transport(_)), "{err:?}");
    let shown = SmartError::from(err).to_string();
    assert!(shown.contains("transport error"), "{shown}");
    assert!(!shown.contains("s3cret"), "{shown}");
    assert!(!shown.contains("doc.md"), "{shown}");
}

#[test]
fn redacted_url_keeps_keys_only() {
    use mb_import::smart::redact::redacted_url;
    let url = validate_url("https://h.test/a/b?sig=abc&X-Amz-Date=1&flag#frag").unwrap();
    assert_eq!(
        redacted_url(&url),
        "https://h.test/a/b?sig=REDACTED&X-Amz-Date=REDACTED&flag=REDACTED"
    );
    let plain = validate_url("https://h.test/a").unwrap();
    assert_eq!(redacted_url(&plain), "https://h.test/a");
}
