//! The console router: index, SPA fallback, hashed assets and the placeholder.
#![cfg(test)]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

const CSP: &str = "default-src 'self'; frame-ancestors 'none'";

async fn send(method: Method, path: &str) -> axum::response::Response {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap();
    mb_console::router().oneshot(req).await.unwrap()
}

async fn text(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn header_of(resp: &axum::response::Response, name: header::HeaderName) -> &str {
    resp.headers()
        .get(&name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .to_str()
        .unwrap()
}

fn assert_security_headers(resp: &axum::response::Response) {
    assert_eq!(header_of(resp, header::CONTENT_SECURITY_POLICY), CSP);
    assert_eq!(header_of(resp, header::X_CONTENT_TYPE_OPTIONS), "nosniff");
}

#[tokio::test]
async fn index_served_with_no_cache_and_csp() {
    let resp = send(Method::GET, "/").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_security_headers(&resp);
    assert_eq!(header_of(&resp, header::CACHE_CONTROL), "no-cache");
    assert!(header_of(&resp, header::CONTENT_TYPE).starts_with("text/html"));
    assert!(text(resp).await.contains("<html"));

    let head = send(Method::HEAD, "/index.html").await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_security_headers(&head);
    assert_eq!(header_of(&head, header::CACHE_CONTROL), "no-cache");
    assert!(text(head).await.is_empty());
}

#[tokio::test]
async fn spa_fallback_returns_index() {
    let index = text(send(Method::GET, "/").await).await;
    let resp = send(Method::GET, "/w/default/mocks").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_security_headers(&resp);
    assert_eq!(header_of(&resp, header::CACHE_CONTROL), "no-cache");
    assert!(header_of(&resp, header::CONTENT_TYPE).starts_with("text/html"));
    assert_eq!(text(resp).await, index);
}

/// The first path segments the console never serves (`RESERVED` in
/// src/lib.rs): the Admin API, the login routes and the ops endpoints.
const RESERVED: [&str; 5] = ["api", "auth", "healthz", "readyz", "metrics"];

#[tokio::test]
async fn reserved_paths_and_other_methods_are_not_the_console() {
    let index = text(send(Method::GET, "/").await).await;
    for segment in RESERVED {
        for path in [format!("/{segment}"), format!("/{segment}/nope")] {
            let resp = send(Method::GET, &path).await;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
            assert_security_headers(&resp);
            assert!(
                !text(resp).await.contains("<html"),
                "{path} must not get the console page"
            );
        }
        // Only the whole first segment is reserved: `/apix` is a console
        // route like any other.
        let lookalike = format!("/{segment}x");
        let resp = send(Method::GET, &lookalike).await;
        assert_eq!(resp.status(), StatusCode::OK, "{lookalike}");
        assert_eq!(text(resp).await, index, "{lookalike}");
    }
    let resp = send(Method::GET, "/assets/missing-abc123.js").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_security_headers(&resp);
    assert!(text(resp).await.is_empty(), "a missing asset is never HTML");

    for method in [Method::POST, Method::PUT, Method::DELETE] {
        let resp = send(method.clone(), "/w/default/mocks").await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(header_of(&resp, header::ALLOW), "GET, HEAD");
        assert_security_headers(&resp);
    }
}

#[tokio::test]
async fn hashed_asset_immutable_cache() {
    if mb_console::is_placeholder() {
        eprintln!("skipped: console not built");
        return;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dist/assets");
    let name = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().into_string().unwrap())
        .find(|n| n.ends_with(".js"))
        .expect("a built JavaScript asset");
    let resp = send(Method::GET, &format!("/assets/{name}")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_security_headers(&resp);
    assert_eq!(
        header_of(&resp, header::CACHE_CONTROL),
        "public, max-age=31536000, immutable"
    );
    assert!(header_of(&resp, header::CONTENT_TYPE).contains("javascript"));
}

#[tokio::test]
async fn placeholder_mentions_build_command() {
    if !mb_console::is_placeholder() {
        eprintln!("skipped: console is built");
        return;
    }
    let body = text(send(Method::GET, "/").await).await;
    assert!(
        body.contains("Console not built — run npm --prefix crates/mb-console/web run build"),
        "{body}"
    );
}
