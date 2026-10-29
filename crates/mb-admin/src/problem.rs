//! RFC 9457 problem details, and request extractors whose rejections are
//! problems too, so every admin error is `application/problem+json`.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use mb_auth::AuthError;
use mb_state::StateError;
use mb_store::StoreError;
use serde::Serialize;
use utoipa::ToSchema;

pub const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// Title of the 409 returned when a `source=file` resource is mutated.
pub const MANAGED_BY_FILE: &str = "Managed by config file";

/// `WWW-Authenticate` value of every 401 admin answer.
pub const WWW_AUTHENTICATE: &str = "Bearer realm=\"mockingbird\"";

/// An RFC 9457 problem details document.
#[derive(Serialize, ToSchema, Debug, Clone)]
pub struct Problem {
    /// Always `about:blank`: the status code carries the meaning.
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
    /// Sent as a `Retry-After` header (whole seconds), not in the body.
    #[serde(skip)]
    #[schema(ignore)]
    pub retry_after: Option<std::time::Duration>,
}

impl Problem {
    pub fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self::titled(status, status.canonical_reason().unwrap_or("Error"), detail)
    }

    pub fn titled(status: StatusCode, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind: "about:blank".into(),
            title: title.into(),
            status: status.as_u16(),
            detail: detail.into(),
            retry_after: None,
        }
    }

    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, detail)
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, detail)
    }

    pub fn unprocessable(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, detail)
    }

    /// A 422 with a title naming what failed validation (rather than the
    /// generic "Unprocessable Entity").
    pub fn invalid(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::titled(StatusCode::UNPROCESSABLE_ENTITY, title, detail)
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, detail)
    }

    /// A 401; its response carries [`WWW_AUTHENTICATE`].
    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, detail)
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, detail)
    }

    /// The 409 for a mutation of a `source=file` resource.
    pub fn managed_by_file(what: impl std::fmt::Display) -> Self {
        Self::titled(
            StatusCode::CONFLICT,
            MANAGED_BY_FILE,
            format!("{what} is declared in the config file and is read-only through the API"),
        )
    }

    fn status_code(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = serde_json::to_vec(&self).unwrap_or_default();
        let mut resp =
            (status, [(header::CONTENT_TYPE, PROBLEM_CONTENT_TYPE)], body).into_response();
        if status == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static(WWW_AUTHENTICATE),
            );
        }
        if let Some(after) = self.retry_after {
            resp.headers_mut().insert(
                header::RETRY_AFTER,
                header::HeaderValue::from(after.as_secs().max(1)),
            );
        }
        if status.is_server_error() {
            resp.extensions_mut().insert(ProblemDetail(self.detail));
        }
        resp
    }
}

/// An extension's refusal (quota, provider policy) with its status, title,
/// detail and `Retry-After`.
impl From<mb_ext::Rejection> for Problem {
    fn from(r: mb_ext::Rejection) -> Self {
        let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::TOO_MANY_REQUESTS);
        Self {
            retry_after: r.retry_after,
            ..Self::titled(status, r.title, r.detail)
        }
    }
}

/// The detail of a 5xx [`Problem`], attached to its response for
/// [`log_server_errors`].
#[derive(Clone, Debug)]
pub struct ProblemDetail(pub String);

/// Middleware: logs every 5xx answer at error level with the request's
/// method and path, and the problem detail when there is one.
pub async fn log_server_errors(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = next.run(req).await;
    if resp.status().is_server_error() {
        let detail = resp
            .extensions()
            .get::<ProblemDetail>()
            .map(|d| d.0.as_str())
            .unwrap_or_default();
        tracing::error!(
            %method,
            %path,
            status = resp.status().as_u16(),
            %detail,
            "admin request failed"
        );
    }
    resp
}

impl From<StoreError> for Problem {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound(d) => Problem::not_found(d),
            StoreError::Conflict(d) => Problem::conflict(d),
            StoreError::Invalid(d) => Problem::invalid("Validation failed", d),
            StoreError::Db(d) => Problem::new(StatusCode::INTERNAL_SERVER_ERROR, d),
        }
    }
}

/// The detail of every 502 caused by the identity provider.
pub const UPSTREAM_DETAIL: &str = "identity provider unavailable";

impl From<AuthError> for Problem {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::Unauthenticated(d) => Problem::unauthorized(d),
            AuthError::Forbidden(d) => Problem::forbidden(d),
            AuthError::Invalid(d) => Problem::bad_request(d),
            AuthError::Upstream(chain) => {
                // The chain can name provider URLs: log it, answer generically.
                tracing::warn!(error = %chain, "identity provider unavailable");
                Problem::new(StatusCode::BAD_GATEWAY, UPSTREAM_DETAIL)
            }
            AuthError::Store(e) => e.into(),
        }
    }
}

impl From<StateError> for Problem {
    fn from(err: StateError) -> Self {
        Problem::new(StatusCode::SERVICE_UNAVAILABLE, err.to_string())
    }
}

impl From<JsonRejection> for Problem {
    fn from(r: JsonRejection) -> Self {
        match r {
            JsonRejection::JsonDataError(_) => {
                Problem::invalid("Invalid request body", r.body_text())
            }
            JsonRejection::JsonSyntaxError(_) => {
                Problem::titled(r.status(), "Malformed JSON", r.body_text())
            }
            r => Problem::new(r.status(), r.body_text()),
        }
    }
}

impl From<QueryRejection> for Problem {
    fn from(r: QueryRejection) -> Self {
        Problem::new(r.status(), r.body_text())
    }
}

impl From<PathRejection> for Problem {
    fn from(r: PathRejection) -> Self {
        Problem::new(r.status(), r.body_text())
    }
}

pub type ApiResult<T> = Result<T, Problem>;

/// `axum::Json` whose rejection is a [`Problem`].
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(Problem))]
pub struct Json<T>(pub T);

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// `axum::extract::Query` whose rejection is a [`Problem`].
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(Problem))]
pub struct Query<T>(pub T);

/// `axum::extract::Path` whose rejection is a [`Problem`].
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(Problem))]
pub struct Path<T>(pub T);

/// The lowercased media type of a `Content-Type` value, without parameters.
pub fn media_type(content_type: Option<&axum::http::HeaderValue>) -> Option<String> {
    let text = content_type?.to_str().ok()?;
    let media = text.split(';').next().unwrap_or_default().trim();
    (!media.is_empty()).then(|| media.to_ascii_lowercase())
}

/// A 415 problem naming the media types `what` accepts.
pub fn unsupported_media_type(what: &str, accepted: &str) -> Problem {
    Problem::new(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        format!("{what} requires Content-Type {accepted}"),
    )
}

/// Requires `Content-Type: application/json` on a request whose body is
/// ignored (e.g. activation), so a cross-site form or no-cors `fetch` (which
/// can only send CORS-safelisted content types) cannot trigger it.
pub struct RequireJson;

impl<S: Send + Sync> FromRequestParts<S> for RequireJson {
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        match media_type(parts.headers.get(header::CONTENT_TYPE)).as_deref() {
            Some("application/json") => Ok(RequireJson),
            _ => Err(unsupported_media_type("this endpoint", "application/json")),
        }
    }
}

/// Fallback for unknown routes.
pub async fn not_found() -> Problem {
    Problem::not_found("no such admin endpoint")
}

/// Fallback for a known route with an unsupported method.
pub async fn method_not_allowed() -> Problem {
    Problem::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method not allowed on this endpoint",
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use mb_store::StoreError;
    use tower::ServiceExt;

    use super::{ApiResult, Problem, log_server_errors};

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    async fn failing() -> ApiResult<()> {
        Err(Problem::from(StoreError::Db("database is on fire".into())))
    }

    async fn missing() -> ApiResult<()> {
        Err(Problem::not_found("nope"))
    }

    #[tokio::test]
    async fn server_errors_are_logged_with_path() {
        let buf = Buf::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let app = Router::new()
            .route("/api/v1/mocks", get(failing))
            .route("/api/v1/other", get(missing))
            .layer(axum::middleware::from_fn(log_server_errors));
        for uri in ["/api/v1/mocks", "/api/v1/other"] {
            app.clone()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
        }
        let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("ERROR"), "{logs}");
        assert!(logs.contains("/api/v1/mocks"), "{logs}");
        assert!(logs.contains("database is on fire"), "{logs}");
        assert!(
            !logs.contains("/api/v1/other"),
            "4xx must not be logged: {logs}"
        );
    }
}
