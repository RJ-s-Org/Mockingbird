use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

const PROBLEM_JSON: &str = "application/problem+json";

/// An RFC 9457 `application/problem+json` response.
pub fn problem(status: u16, title: &str, detail: impl Into<String>) -> Response {
    let (code, body) = problem_body(status, title, detail.into());
    render(code, &body)
}

/// The problem document for `status`, and the status code actually used
/// (500 when `status` is not a valid code).
pub(crate) fn problem_body(status: u16, title: &str, detail: String) -> (StatusCode, Value) {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = json!({
        "type": "about:blank",
        "title": title,
        "status": code.as_u16(),
        "detail": detail,
    });
    (code, body)
}

/// Render a problem document built by [`problem_body`].
pub(crate) fn render(code: StatusCode, body: &Value) -> Response {
    let mut resp = (code, body.to_string()).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
    resp
}
