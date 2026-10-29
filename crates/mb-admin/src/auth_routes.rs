//! Console login: `GET /auth/login`, `GET /auth/callback` and
//! `POST /auth/logout` (OIDC Authorization Code + PKCE with server-side
//! admin sessions), `GET /api/v1/me`, and [`console_gate`], which guards the
//! console and a private `/metrics`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use mb_auth::{AuthError, Principal, PrincipalKind, Role, SESSION_COOKIE};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

use crate::AdminState;
use crate::access::{CurrentPrincipal, request_id};
use crate::problem::{ApiResult, Json, Problem, Query, RequireJson};
use crate::workspaces::RoleSchema;

/// Name of the cookie that carries a started login's OIDC `state`.
pub const LOGIN_STATE_COOKIE: &str = "mb_login";

/// How long a started login may take, in seconds (the `mb_login` cookie's
/// `Max-Age`).
const LOGIN_STATE_MAX_AGE: u64 = 600;

/// Title of every failed callback.
const LOGIN_FAILED: &str = "Login failed";

/// Browser login in `auth.mode: oidc`.
#[derive(Clone)]
pub struct WebAuth {
    pub oidc: Arc<mb_auth::OidcClient>,
    pub sessions: mb_auth::SessionManager,
    /// Whether cookies carry `Secure` (`auth.external_url` is https).
    pub secure_cookies: bool,
}

impl WebAuth {
    fn login_cookie(&self, value: &str, max_age: u64) -> String {
        let mut c = format!(
            "{LOGIN_STATE_COOKIE}={value}; HttpOnly; SameSite=Lax; Path=/auth; Max-Age={max_age}"
        );
        if self.secure_cookies {
            c.push_str("; Secure");
        }
        c
    }
}

/// The caller and the workspaces they have a role in.
#[derive(Serialize, ToSchema, Debug, Clone)]
pub struct MeView {
    pub subject: String,
    pub display_name: String,
    #[schema(value_type = PrincipalKindSchema)]
    pub kind: PrincipalKind,
    /// A sysadmin is `owner` in every workspace.
    pub sysadmin: bool,
    /// Every existing workspace the caller has a role in, by name.
    pub workspaces: Vec<MeWorkspace>,
}

/// A workspace and the caller's effective role in it.
#[derive(Serialize, ToSchema, Debug, Clone)]
pub struct MeWorkspace {
    pub name: String,
    #[schema(value_type = RoleSchema)]
    pub role: Role,
}

/// The OpenAPI schema of [`PrincipalKind`] (which lives in `mb-auth`).
#[derive(ToSchema)]
#[schema(as = PrincipalKind, rename_all = "snake_case")]
#[allow(dead_code)]
pub enum PrincipalKindSchema {
    User,
    ApiKey,
    Local,
}

#[utoipa::path(
    get,
    path = "/api/v1/me",
    tag = "me",
    responses(
        (status = 200, description = "The caller", body = MeView),
        (status = 401, description = "No valid credentials", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn me(
    State(st): State<AdminState>,
    CurrentPrincipal(principal): CurrentPrincipal,
) -> ApiResult<Json<MeView>> {
    let mut workspaces: Vec<MeWorkspace> = st
        .store
        .list_workspaces()
        .await?
        .into_iter()
        .filter_map(|w| {
            principal
                .role_in(&w.name)
                .map(|role| MeWorkspace { name: w.name, role })
        })
        .collect();
    workspaces.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(MeView {
        subject: principal.subject,
        display_name: principal.display_name,
        kind: principal.kind,
        sysadmin: principal.sysadmin,
        workspaces,
    }))
}

/// `/auth/login`, `/auth/callback` and `/auth/logout`.
pub fn auth_router(st: AdminState) -> Router {
    routes().with_state(st)
}

pub(crate) fn routes() -> Router<AdminState> {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
}

#[derive(Deserialize, Debug)]
struct LoginQuery {
    return_to: Option<String>,
}

fn redirect(location: &str) -> ApiResult<Response> {
    let value = HeaderValue::from_str(location)
        .map_err(|_| Problem::bad_request("the redirect target is not a valid header value"))?;
    Ok((StatusCode::FOUND, [(header::LOCATION, value)]).into_response())
}

fn with_cookies(mut resp: Response, cookies: &[String]) -> Response {
    for c in cookies {
        if let Ok(v) = HeaderValue::from_str(c) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    resp
}

async fn login(State(st): State<AdminState>, Query(q): Query<LoginQuery>) -> ApiResult<Response> {
    let return_to = mb_auth::validate_return_to(q.return_to.as_deref().unwrap_or("/"))?;
    let Some(web) = &st.web_auth else {
        return redirect(&return_to);
    };
    let started = web.oidc.start_login(&return_to).await?;
    let resp = redirect(&started.url)?;
    Ok(with_cookies(
        resp,
        &[web.login_cookie(&started.state, LOGIN_STATE_MAX_AGE)],
    ))
}

#[derive(Deserialize, Debug)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(st): State<AdminState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> ApiResult<Response> {
    let Some(web) = &st.web_auth else {
        return Err(Problem::not_found("login is not enabled (auth.mode: none)"));
    };
    let rid = request_id(&headers);
    let completed = match complete(web, &headers, &q).await {
        Ok(c) => c,
        Err((reason, attributable)) => {
            // Only a failure of a login this browser started is audited, so
            // anonymous requests cannot grow the audit log or fill the
            // webhook queue. Those are rate limited too (per source address
            // and overall); the excess is only counted.
            let source = peer.map(|Extension(ConnectInfo(addr))| addr.ip());
            if attributable && !st.login_failures.allow(source) {
                tracing::warn!(%reason, "console login failed (audit event suppressed)");
            } else if attributable {
                tracing::warn!(%reason, "console login failed");
                st.audit
                    .record(mb_store::NewAuditEvent {
                        workspace: None,
                        actor_subject: "anonymous".to_string(),
                        actor_kind: "user".to_string(),
                        action: "auth.login_failed".to_string(),
                        target: "user:unknown".to_string(),
                        before: None,
                        after: Some(json!({ "reason": reason.code() })),
                        request_id: Some(rid),
                        at: chrono::Utc::now(),
                    })
                    .await;
            } else {
                tracing::debug!(%reason, "console login callback without a login in progress");
                metrics::counter!("mb_auth_callback_rejected_total", "reason" => reason.code())
                    .increment(1);
            }
            let p = Problem::titled(StatusCode::BAD_REQUEST, LOGIN_FAILED, reason.detail());
            return Ok(with_cookies(p.into_response(), &[web.login_cookie("", 0)]));
        }
    };

    // A session id the browser brought along is never reused (fixation).
    if let Some(old) = cookie(&headers, SESSION_COOKIE) {
        web.sessions.destroy(old).await?;
    }
    let principal = completed.principal;
    let session = web.sessions.create(&principal).await?;
    st.audit
        .record_by(
            &principal,
            None,
            rid,
            "auth.login",
            format!("user:{}", principal.subject),
            None,
            Some(json!({
                "display_name": principal.display_name,
                "sysadmin": principal.sysadmin,
                "roles": principal.roles,
            })),
        )
        .await;
    let resp = redirect(&completed.return_to)?;
    Ok(with_cookies(
        resp,
        &[web.sessions.set_cookie(&session), web.login_cookie("", 0)],
    ))
}

/// Why a callback was rejected.
enum Failure {
    Provider(String),
    MissingParams,
    StateMismatch,
    Rejected(AuthError),
}

impl Failure {
    /// The reason recorded in the audit event (no provider-supplied text).
    fn code(&self) -> &'static str {
        match self {
            Failure::Provider(_) => "provider_error",
            Failure::MissingParams => "missing_parameters",
            Failure::StateMismatch => "state_mismatch",
            Failure::Rejected(_) => "rejected",
        }
    }

    /// The problem detail the browser sees: fixed text, never anything the
    /// provider or the token exchange returned.
    fn detail(&self) -> &'static str {
        match self {
            Failure::Provider(_) => "the identity provider reported an error",
            Failure::MissingParams => "the callback needs `code` and `state`",
            Failure::StateMismatch => "the login state does not match this browser's login",
            Failure::Rejected(_) => "the login could not be completed",
        }
    }
}

/// The full reason, for the server log only.
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Provider(e) => write!(f, "the identity provider answered with error {e:?}"),
            Failure::Rejected(e) => write!(f, "the login could not be completed: {e}"),
            other => f.write_str(other.detail()),
        }
    }
}

/// Completes the callback's login. On failure, also whether the failure
/// belongs to a login this browser started: its `state` matches the
/// `mb_login` cookie and names a pending login.
async fn complete(
    web: &WebAuth,
    headers: &HeaderMap,
    q: &CallbackQuery,
) -> Result<mb_auth::CompletedLogin, (Failure, bool)> {
    let own_state = q
        .state
        .as_deref()
        .filter(|s| cookie(headers, LOGIN_STATE_COOKIE) == Some(*s));
    if let Some(e) = &q.error {
        let attributable = match own_state {
            Some(s) => web.oidc.cancel_login(s).await.unwrap_or_else(|err| {
                tracing::warn!(%err, "could not end the pending login");
                false
            }),
            None => false,
        };
        return Err((
            Failure::Provider(e.chars().take(64).collect()),
            attributable,
        ));
    }
    let (Some(code), Some(state)) = (&q.code, &q.state) else {
        return Err((Failure::MissingParams, false));
    };
    if own_state.is_none() {
        return Err((Failure::StateMismatch, false));
    }
    let attributable = web.oidc.login_pending(state).await.unwrap_or_else(|err| {
        tracing::warn!(%err, "could not look up the pending login");
        false
    });
    web.oidc
        .complete_login(state, code)
        .await
        .map_err(|e| (Failure::Rejected(e), attributable))
}

/// The body of `POST /auth/logout` when the provider supports RP-initiated
/// logout: the browser goes there next to end the provider's session too.
#[derive(Serialize, Debug)]
struct LogoutView {
    end_session_url: String,
}

/// Ends the admin session. Answers `204` or, when the provider advertises
/// an `end_session_endpoint`, `200` with [`LogoutView`]. The local session
/// ends either way; the provider is not contacted.
async fn logout(
    State(st): State<AdminState>,
    headers: HeaderMap,
    _json: RequireJson,
) -> ApiResult<Response> {
    let Some(web) = &st.web_auth else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    if let Some(value) = cookie(&headers, SESSION_COOKIE)
        && let Some(principal) = web.sessions.lookup(value).await?
    {
        web.sessions.destroy(value).await?;
        st.audit
            .record_by(
                &principal,
                None,
                request_id(&headers),
                "auth.logout",
                format!("user:{}", principal.subject),
                None,
                None,
            )
            .await;
    }
    let resp = match web.oidc.end_session_url() {
        Some(end_session_url) => {
            (StatusCode::OK, Json(LogoutView { end_session_url })).into_response()
        }
        None => StatusCode::NO_CONTENT.into_response(),
    };
    Ok(with_cookies(resp, &[web.sessions.clear_cookie()]))
}

/// Middleware for the console and a private `/metrics`: lets a request with
/// valid credentials through (with its [`Principal`] in the extensions).
/// Otherwise a browser navigation (a GET accepting `text/html`) is sent to
/// `/auth/login?return_to=<path+query>`, and anything else gets a 401.
pub async fn console_gate(State(st): State<AdminState>, mut req: Request, next: Next) -> Response {
    let problem = match st.auth.authenticate(req.headers()).await {
        Ok(Some(principal)) => {
            req.extensions_mut().insert::<Principal>(principal);
            return next.run(req).await;
        }
        Ok(None) => Problem::unauthorized("authentication required"),
        Err(e) => Problem::from(e),
    };
    let navigation = req.method() == Method::GET && accepts_html(req.headers());
    if navigation && problem.status == StatusCode::UNAUTHORIZED.as_u16() {
        // A path too long to be a `return_to` returns to the start page.
        let target = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .filter(|t| t.len() <= mb_auth::MAX_RETURN_TO)
            .unwrap_or("/")
            .to_string();
        let location = format!("/auth/login?return_to={}", percent_encode(&target));
        if let Ok(v) = HeaderValue::from_str(&location) {
            return (StatusCode::FOUND, [(header::LOCATION, v)]).into_response();
        }
    }
    problem.into_response()
}

/// Whether any `Accept` header lists `text/html`.
fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|m| {
            m.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("text/html")
        })
}

/// `s` with every byte outside the URI unreserved set percent-encoded.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The value of the first cookie called `name` across all `Cookie` headers.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;

    use super::{accepts_html, cookie, percent_encode};

    #[test]
    fn encodes_paths() {
        assert_eq!(
            percent_encode("/w/default/mocks?q=1"),
            "%2Fw%2Fdefault%2Fmocks%3Fq%3D1"
        );
        assert_eq!(percent_encode("a b&c~"), "a%20b%26c~");
    }

    #[test]
    fn reads_cookies_and_accept() {
        let mut h = HeaderMap::new();
        h.append("cookie", "a=1; mb_login=xyz".parse().unwrap());
        h.append("cookie", "mb_session=s".parse().unwrap());
        assert_eq!(cookie(&h, "mb_login"), Some("xyz"));
        assert_eq!(cookie(&h, "mb_session"), Some("s"));
        assert_eq!(cookie(&h, "mb"), None);
        assert!(!accepts_html(&h));
        h.insert(
            "accept",
            "application/json, TEXT/HTML;q=0.5".parse().unwrap(),
        );
        assert!(accepts_html(&h));
    }
}
