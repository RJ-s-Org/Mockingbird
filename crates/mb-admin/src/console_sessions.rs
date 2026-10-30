//! `/api/v1/console-sessions…`: list and revoke console login sessions
//! (the `mb_session` cookies of `auth.mode: oidc`). A sysadmin sees and
//! ends anyone's sessions; everyone else only their own, which is how the
//! console signs out everywhere. In `auth.mode: none` there are none.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use mb_auth::{Principal, SESSION_COOKIE, SessionInfo, SessionManager};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::access::{CurrentPrincipal, request_id};
use crate::audit::AuditEntry;
use crate::auth_routes::cookie;
use crate::problem::{ApiResult, Json, Path, Problem, Query};

/// A live console session (neither expired nor idle).
#[derive(Serialize, ToSchema, Debug, Clone)]
pub struct ConsoleSessionView {
    /// Names the session for `DELETE /api/v1/console-sessions/{id}`; it
    /// cannot be used to sign in.
    pub id: String,
    /// The signed-in principal's subject.
    pub subject: String,
    pub display_name: String,
    /// The login.
    pub created_at: DateTime<Utc>,
    /// The session ends then regardless of activity (`auth.session_ttl`).
    pub expires_at: DateTime<Utc>,
    /// The last request with this session, to within a minute.
    pub last_seen: DateTime<Utc>,
    /// The session ends then unless it is used again
    /// (`auth.session_idle_timeout`); absent without an idle timeout.
    pub idle_expires_at: Option<DateTime<Utc>>,
    /// Whether this is the session the request came with.
    pub current: bool,
}

impl ConsoleSessionView {
    fn new(s: SessionInfo, current: Option<&str>) -> Self {
        ConsoleSessionView {
            current: current == Some(s.id.as_str()),
            id: s.id,
            subject: s.subject,
            display_name: s.display_name,
            created_at: s.created_at,
            expires_at: s.expires_at,
            last_seen: s.last_seen,
            idle_expires_at: s.idle_expires_at,
        }
    }
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct SessionsQuery {
    /// Only this principal's sessions. Without it a sysadmin gets
    /// everyone's and anyone else their own; anyone but a sysadmin may
    /// only name themselves.
    pub subject: Option<String>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct RevokeAllQuery {
    /// The principal whose sessions end. Anyone but a sysadmin may only
    /// name themselves.
    pub subject: Option<String>,
}

/// How many sessions `DELETE /api/v1/console-sessions` ended.
#[derive(Serialize, ToSchema, Debug)]
pub struct RevokedSessions {
    pub revoked: u64,
}

#[derive(Deserialize, Debug)]
pub struct SessionPath {
    id: String,
}

/// A 403 problem unless the caller may see and end `subject`'s sessions:
/// a sysadmin anyone's, anyone else only their own.
fn allowed(principal: &Principal, subject: &str) -> ApiResult<()> {
    if principal.sysadmin || principal.subject == subject {
        Ok(())
    } else {
        Err(Problem::forbidden(
            "only a sysadmin can see or end another principal's console sessions",
        ))
    }
}

fn sessions(st: &AdminState) -> Option<&SessionManager> {
    st.web_auth.as_ref().map(|w| &w.sessions)
}

#[utoipa::path(
    get,
    path = "/api/v1/console-sessions",
    tag = "console-sessions",
    params(SessionsQuery),
    responses(
        (status = 200, description = "The live console sessions, newest first (none in `auth.mode: none`)", body = Vec<ConsoleSessionView>),
        (status = 403, description = "Another principal's sessions, asked for by a non-sysadmin", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    CurrentPrincipal(principal): CurrentPrincipal,
    headers: HeaderMap,
    Query(q): Query<SessionsQuery>,
) -> ApiResult<Json<Vec<ConsoleSessionView>>> {
    let subject = match q.subject {
        Some(s) => {
            allowed(&principal, &s)?;
            Some(s)
        }
        None if principal.sysadmin => None,
        None => Some(principal.subject.clone()),
    };
    let Some(sessions) = sessions(&st) else {
        return Ok(Json(Vec::new()));
    };
    let current = cookie(&headers, SESSION_COOKIE).map(SessionManager::session_id);
    let views = sessions
        .list(subject.as_deref())
        .await?
        .into_iter()
        .map(|s| ConsoleSessionView::new(s, current.as_deref()))
        .collect();
    Ok(Json(views))
}

#[utoipa::path(
    delete,
    path = "/api/v1/console-sessions/{id}",
    tag = "console-sessions",
    params(("id" = String, Path, description = "Session id")),
    responses(
        (status = 204, description = "Ended: requests with that session get 401 from now on"),
        (status = 404, description = "No such live session, or (for a non-sysadmin) another principal's", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn revoke(
    State(st): State<AdminState>,
    CurrentPrincipal(principal): CurrentPrincipal,
    headers: HeaderMap,
    Path(SessionPath { id }): Path<SessionPath>,
) -> ApiResult<StatusCode> {
    let missing = || Problem::not_found(format!("no console session {id:?}"));
    let sessions = sessions(&st).ok_or_else(missing)?;
    let info = sessions
        .get(&id)
        .await?
        .filter(|s| principal.sysadmin || s.subject == principal.subject)
        .ok_or_else(missing)?;
    if !sessions.revoke(&id).await? {
        return Err(missing());
    }
    st.audit
        .record_by(
            &principal,
            AuditEntry {
                workspace: None,
                request_id: request_id(&headers),
                action: "console_session.revoke",
                target: format!("user:{}", info.subject),
                before: None,
                after: Some(json!({ "session": id })),
            },
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/api/v1/console-sessions",
    tag = "console-sessions",
    params(RevokeAllQuery),
    responses(
        (status = 200, description = "Every session of `subject` ended (sign out everywhere)", body = RevokedSessions),
        (status = 400, description = "No `subject`", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Another principal's sessions, by a non-sysadmin", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn revoke_all(
    State(st): State<AdminState>,
    CurrentPrincipal(principal): CurrentPrincipal,
    headers: HeaderMap,
    Query(q): Query<RevokeAllQuery>,
) -> ApiResult<Json<RevokedSessions>> {
    let subject = q
        .subject
        .ok_or_else(|| Problem::bad_request("`subject` names whose sessions end"))?;
    allowed(&principal, &subject)?;
    let Some(sessions) = sessions(&st) else {
        return Ok(Json(RevokedSessions { revoked: 0 }));
    };
    let revoked = sessions.revoke_all(&subject).await?;
    st.audit
        .record_by(
            &principal,
            AuditEntry {
                workspace: None,
                request_id: request_id(&headers),
                action: "console_session.revoke_all",
                target: format!("user:{subject}"),
                before: None,
                after: Some(json!({ "revoked": revoked })),
            },
        )
        .await;
    Ok(Json(RevokedSessions { revoked }))
}
