//! The Mockingbird console: the Vite build in `dist/` (built from `web/`),
//! embedded at compile time and served on the admin port.
//!
//! When `dist/` holds no build, `build.rs` writes a placeholder
//! `index.html` that says how to build the console, so `cargo build` never
//! needs Node.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "dist/"]
struct Dist;

/// Text only the placeholder page contains.
const PLACEHOLDER_MARKER: &str = "Console not built";

const CSP: &str = "default-src 'self'; frame-ancestors 'none'";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";

/// First path segments that belong to the Admin API, the login routes and
/// the ops endpoints, never to the console.
const RESERVED: [&str; 5] = ["api", "auth", "healthz", "readyz", "metrics"];

/// Serves the console for `GET`/`HEAD` on any path (anything else is a 405):
///
/// - an embedded file under `/assets/` with
///   `Cache-Control: public, max-age=31536000, immutable` (a missing one is
///   a 404, so a stale bundle never gets HTML for JavaScript);
/// - any other embedded file, with `Cache-Control: no-cache`;
/// - otherwise `index.html` (SPA fallback), with `Cache-Control: no-cache`,
///   except under `/api/`, `/auth/` and the ops paths, which are a 404.
///
/// Every response carries
/// `Content-Security-Policy: default-src 'self'; frame-ancestors 'none'` and
/// `X-Content-Type-Options: nosniff`. The caller puts it behind
/// authentication (the binary uses it as the admin port's fallback).
pub fn router() -> Router {
    Router::new().fallback(serve)
}

/// Whether the crate was built without a real console (the `build.rs`
/// placeholder page is embedded).
pub fn is_placeholder() -> bool {
    Dist::get("index.html")
        .is_none_or(|f| std::str::from_utf8(&f.data).is_ok_and(|s| s.contains(PLACEHOLDER_MARKER)))
}

async fn serve(req: Request<Body>) -> Response {
    let head = req.method() == Method::HEAD;
    if !head && req.method() != Method::GET {
        let mut resp = StatusCode::METHOD_NOT_ALLOWED.into_response();
        resp.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return secured(resp);
    }
    let path = req.uri().path().trim_start_matches('/');
    let first = path.split('/').next().unwrap_or_default();
    let resp = if RESERVED.contains(&first) {
        StatusCode::NOT_FOUND.into_response()
    } else if let Some(file) = (!path.is_empty()).then(|| Dist::get(path)).flatten() {
        let cache = if first == "assets" {
            IMMUTABLE
        } else {
            NO_CACHE
        };
        file_response(file, cache, head)
    } else if first == "assets" {
        StatusCode::NOT_FOUND.into_response()
    } else {
        match Dist::get("index.html") {
            Some(file) => file_response(file, NO_CACHE, head),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    };
    secured(resp)
}

fn file_response(file: rust_embed::EmbeddedFile, cache: &'static str, head: bool) -> Response {
    let content_type = HeaderValue::from_str(file.metadata.mimetype())
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let length = file.data.len();
    let body = if head {
        Body::empty()
    } else {
        Body::from(file.data.into_owned())
    };
    let mut resp = (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
        ],
        body,
    )
        .into_response();
    resp.headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    resp
}

/// `resp` with the headers every console response carries.
fn secured(mut resp: Response) -> Response {
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}
