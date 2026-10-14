use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

const PROBLEM_JSON: &str = "application/problem+json";

/// An RFC 9457 `application/problem+json` response.
pub fn problem(status: u16, title: &str, detail: impl Into<String>) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = json!({
        "type": "about:blank",
        "title": title,
        "status": code.as_u16(),
        "detail": detail.into(),
    });
    let mut resp = (code, body.to_string()).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
    resp
}
