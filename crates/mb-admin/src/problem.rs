//! RFC 9457 problem details, and request extractors whose rejections are
//! problems too, so every admin error is `application/problem+json`.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use mb_state::StateError;
use mb_store::StoreError;
use serde::Serialize;
use utoipa::ToSchema;

pub const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// Title of the 409 returned when a `source=file` resource is mutated.
pub const MANAGED_BY_FILE: &str = "Managed by config file";

/// An RFC 9457 problem details document.
#[derive(Serialize, ToSchema, Debug, Clone)]
pub struct Problem {
    /// Always `about:blank`: the status code carries the meaning.
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
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

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, detail)
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
        (status, [(header::CONTENT_TYPE, PROBLEM_CONTENT_TYPE)], body).into_response()
    }
}

impl From<StoreError> for Problem {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound(d) => Problem::not_found(d),
            StoreError::Conflict(d) => Problem::conflict(d),
            StoreError::Invalid(d) => Problem::unprocessable(d),
            StoreError::Db(d) => Problem::new(StatusCode::INTERNAL_SERVER_ERROR, d),
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
        Problem::new(r.status(), r.body_text())
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
