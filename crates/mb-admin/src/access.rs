//! Who is calling, and what they may do in which workspace: the
//! authentication middleware and the [`CurrentPrincipal`] and [`WsAccess`]
//! extractors.

use axum::extract::{FromRequestParts, RawPathParams, Request, State};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use mb_auth::{Permission, Principal};
use mb_store::DEFAULT_WORKSPACE;

use crate::AdminState;
use crate::problem::{ApiResult, Problem};

/// Header carrying the caller's request id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Middleware: resolves the caller with [`AdminState::auth`] and stores the
/// [`Principal`] in the request extensions, or answers 401.
pub async fn authenticate(State(st): State<AdminState>, mut req: Request, next: Next) -> Response {
    match st.auth.authenticate(req.headers()).await {
        Ok(Some(principal)) => {
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
        Ok(None) => Problem::unauthorized("authentication required").into_response(),
        Err(e) => Problem::from(e).into_response(),
    }
}

/// The authenticated caller; 401 when [`authenticate`] did not run or
/// found no principal.
#[derive(Clone, Debug)]
pub struct CurrentPrincipal(pub Principal);

impl<S: Send + Sync> FromRequestParts<S> for CurrentPrincipal {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .map(CurrentPrincipal)
            .ok_or_else(|| Problem::unauthorized("authentication required"))
    }
}

/// The caller's access to the workspace a request addresses: the `ws` path
/// parameter, or `default` on the P2 alias paths.
///
/// Extraction fails with 403 when the caller has no role in the workspace
/// (whether or not it exists) and, for a sysadmin, with 404 when the
/// workspace does not exist. Handlers then check the permission they need
/// with [`WsAccess::require`].
#[derive(Clone, Debug)]
pub struct WsAccess {
    pub principal: Principal,
    pub ws: String,
    /// The `x-request-id` header when it is 1–128 visible ASCII characters,
    /// otherwise a new UUID v4.
    pub request_id: String,
}

impl WsAccess {
    /// A 403 problem unless the caller's role in the workspace grants `p`.
    pub fn require(&self, p: Permission) -> ApiResult<()> {
        if self.principal.can(&self.ws, p) {
            Ok(())
        } else {
            Err(Problem::forbidden(format!(
                "this needs the {} role in workspace {:?}",
                p.min_role().as_str(),
                self.ws
            )))
        }
    }
}

fn no_access(ws: &str) -> Problem {
    Problem::forbidden(format!("you have no role in workspace {ws:?}"))
}

/// The request id of a request (see [`WsAccess::request_id`]).
pub(crate) fn request_id(headers: &HeaderMap) -> String {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| (1..=128).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_graphic()))
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

impl FromRequestParts<AdminState> for WsAccess {
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut Parts,
        st: &AdminState,
    ) -> Result<Self, Self::Rejection> {
        let CurrentPrincipal(principal) = CurrentPrincipal::from_request_parts(parts, st).await?;
        let ws = RawPathParams::from_request_parts(parts, st)
            .await
            .ok()
            .and_then(|params| {
                params
                    .iter()
                    .find(|(name, _)| *name == "ws")
                    .map(|(_, value)| value.to_string())
            })
            .unwrap_or_else(|| DEFAULT_WORKSPACE.to_string());
        if principal.role_in(&ws).is_none() {
            return Err(no_access(&ws));
        }
        if st.store.get_workspace(&ws).await?.is_none() {
            return Err(if principal.sysadmin {
                Problem::not_found(format!("workspace {ws:?} does not exist"))
            } else {
                no_access(&ws)
            });
        }
        Ok(WsAccess {
            request_id: request_id(&parts.headers),
            principal,
            ws,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use axum::http::HeaderMap;

    use super::request_id;

    #[test]
    fn request_ids() {
        let mut h = HeaderMap::new();
        h.insert("x-request-id", "abc-123".parse().unwrap());
        assert_eq!(request_id(&h), "abc-123");
        h.insert("x-request-id", "has space".parse().unwrap());
        assert_eq!(request_id(&h).len(), 36);
        h.insert("x-request-id", "x".repeat(129).parse().unwrap());
        assert_eq!(request_id(&h).len(), 36);
        assert_eq!(request_id(&HeaderMap::new()).len(), 36);
    }
}
