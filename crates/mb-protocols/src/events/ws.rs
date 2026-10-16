//! WebSocket channels: one route per path template, serving the mock's
//! enabled WebSocket Publish channels (hub messages out) and Subscribe
//! channels (client frames in, validated, recorded and answered).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use futures::{FutureExt, StreamExt};
use mb_mcm::{Channel, ChannelBinding, Direction};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use super::runtime::{ChannelInner, DEFAULT_SESSION};
use super::{InMessage, MAX_MESSAGE_BYTES, address_matches};
use crate::dp_auth::{Unauthorized, verify_key};
use crate::ws::{ChannelConfig, ChannelIo, Outbound, serve_channel};
use crate::{RequestKind, RequestRecord};

/// Close code sent when the mock's channel runtime is replaced or dropped.
pub const CLOSE_RUNTIME_REPLACED: u16 = 1012;
const CLOSE_REASON: &str = "mock reloaded";
/// Longest request path matched against the channel templates; longer
/// paths get 404 without matching.
const MAX_PATH_BYTES: usize = 2048;

/// The WebSocket path of `channel`, when it is an enabled WebSocket channel.
fn served_path<'a>(inner: &ChannelInner, channel: &'a Channel) -> Option<&'a str> {
    match &channel.binding {
        ChannelBinding::WebSocket { path } if inner.enabled(channel) => Some(path),
        _ => None,
    }
}

/// `template` in axum route syntax. A segment holding a placeholder becomes
/// a positional `{pN}` capture, so templates differing only in parameter
/// names (or with a placeholder inside a segment) share one route and never
/// make an invalid or conflicting route; the handler matches the exact
/// template.
fn route_path(template: &str) -> String {
    let route = template
        .trim_start_matches('/')
        .split('/')
        .enumerate()
        .map(|(i, segment)| {
            if segment.contains(['{', '}', '*']) {
                format!("{{p{i}}}")
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    format!("/{route}")
}

/// The routes of the enabled WebSocket channels of `inner`, by route path.
pub(crate) fn router(inner: &Arc<ChannelInner>) -> Option<Router> {
    let mut routes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for channel in &inner.rt.bundle().channels {
        if let Some(path) = served_path(inner, channel) {
            let templates = routes.entry(route_path(path)).or_default();
            if !templates.iter().any(|t| t == path) {
                templates.push(path.to_string());
            }
        }
    }
    if routes.is_empty() {
        return None;
    }
    let mut router = Router::new();
    for (route, templates) in routes {
        let inner = Arc::clone(inner);
        let templates = Arc::new(templates);
        router = router.route(
            &route,
            get(
                move |upgrade: WebSocketUpgrade, uri: Uri, headers: HeaderMap| {
                    upgrade_channel(inner, templates, upgrade, uri, headers)
                },
            ),
        );
    }
    Some(router)
}

/// Upgrades a request on a WebSocket channel path. In the mock's `api_key`
/// data-plane auth mode the upgrade needs a valid data-plane key of the
/// mock's workspace, in the key header or the `api_key` query parameter,
/// like every other protocol; without one it is answered `401` (with a
/// `WWW-Authenticate` challenge) and recorded as a refused event call.
async fn upgrade_channel(
    inner: Arc<ChannelInner>,
    templates: Arc<Vec<String>>,
    upgrade: WebSocketUpgrade,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let path = uri.path().to_string();
    if path.len() > MAX_PATH_BYTES {
        return StatusCode::NOT_FOUND.into_response();
    }
    let matched: Vec<&String> = templates
        .iter()
        .filter(|t| address_matches(t, &path))
        .collect();
    if matched.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let query = Query::<Vec<(String, String)>>::try_from_uri(&uri)
        .map(|q| q.0)
        .unwrap_or_default();
    if let Err(refused) = verify_key(&inner.rt, &headers, &query).await {
        record_refusal(&inner, &headers, &path, &refused.detail);
        return unauthorized(refused);
    }
    let subscribers: Vec<Channel> = inner
        .rt
        .bundle()
        .channels
        .iter()
        .filter(|c| c.direction == Direction::Subscribe)
        .filter(|c| served_path(&inner, c).is_some_and(|p| matched.iter().any(|t| *t == p)))
        .cloned()
        .collect();
    // Subscribe before upgrading, so nothing published once the client is
    // connected is missed. Every path is subscribed (the hub carries only
    // Publish channels' messages) so its close reaches every socket.
    let hub = stream::select_all(matched.iter().map(|t| hub_stream(inner.hub.subscribe(t))));
    let cfg = ChannelConfig {
        subprotocols: vec![],
        keepalive: inner.rt.protocols.ws_keepalive,
        max_message_bytes: MAX_MESSAGE_BYTES,
    };
    serve_channel(upgrade, cfg, move |io| {
        session(inner, Arc::new(subscribers), path, hub, io)
    })
}

/// A `401` problem carrying the challenge of `refused`.
fn unauthorized(refused: Unauthorized) -> Response {
    let body = serde_json::json!({
        "type": "about:blank",
        "title": "Unauthorized",
        "status": 401,
        "detail": refused.detail,
    });
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/problem+json")],
        body.to_string(),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(&refused.challenge) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    resp
}

/// Records an upgrade refused by data-plane auth as a `401` event call of
/// the mock (credential headers and the query are left out).
fn record_refusal(inner: &ChannelInner, headers: &HeaderMap, path: &str, detail: &str) {
    let Some(recorder) = &inner.rt.recorder else {
        return;
    };
    recorder.record(RequestRecord {
        workspace: inner.rt.workspace.clone(),
        mock: inner.rt.name.clone(),
        operation_id: None,
        session: DEFAULT_SESSION.into(),
        method: "GET".into(),
        path: path.to_string(),
        status: 401,
        source: None,
        latency: std::time::Duration::ZERO,
        prompt_tokens: 0,
        completion_tokens: 0,
        request: serde_json::json!({
            "headers": crate::call::recorded_headers(&inner.rt, headers),
        }),
        request_body_bytes: None,
        response: Some(serde_json::json!({"detail": detail})),
        kind: RequestKind::Event,
    });
}

/// The texts of `rx`, skipping lagged ones; `None` once its sender is gone.
fn hub_stream(rx: broadcast::Receiver<String>) -> BoxStream<'static, Option<String>> {
    stream::unfold(Some(rx), |rx| async move {
        let mut rx = rx?;
        loop {
            match rx.recv().await {
                Ok(text) => return Some((Some(text), Some(rx))),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Some((None, None)),
            }
        }
    })
    .boxed()
}

/// Await `task`, or never when there is none.
async fn run(task: &mut Option<BoxFuture<'static, ()>>) {
    match task {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}

/// One connected client: forwards hub messages and handles its frames one
/// at a time, until the client leaves or the runtime goes away.
async fn session(
    inner: Arc<ChannelInner>,
    subscribers: Arc<Vec<Channel>>,
    path: String,
    mut hub: stream::SelectAll<BoxStream<'static, Option<String>>>,
    io: ChannelIo,
) {
    let ChannelIo {
        mut incoming,
        outgoing,
        ..
    } = io;
    let mut handling: Option<BoxFuture<'static, ()>> = None;
    loop {
        tokio::select! {
            item = hub.next() => match item {
                Some(Some(text)) => {
                    if outgoing.send(Outbound::Text(text)).await.is_err() {
                        return;
                    }
                }
                Some(None) | None => {
                    let _ = outgoing
                        .send(Outbound::Close {
                            code: CLOSE_RUNTIME_REPLACED,
                            reason: CLOSE_REASON.into(),
                        })
                        .await;
                    return;
                }
            },
            () = run(&mut handling) => handling = None,
            text = incoming.next(), if handling.is_none() => match text {
                Some(text) => {
                    handling = Some(
                        handle_text(
                            Arc::clone(&inner),
                            Arc::clone(&subscribers),
                            path.clone(),
                            text,
                            outgoing.clone(),
                        )
                        .boxed(),
                    );
                }
                None => return,
            },
        }
    }
}

/// Receive one client frame on the first Subscribe channel that accepts
/// it, else the first; a WebSocket reply goes back to this client only.
async fn handle_text(
    inner: Arc<ChannelInner>,
    subscribers: Arc<Vec<Channel>>,
    path: String,
    text: String,
    socket: mpsc::Sender<Outbound>,
) {
    let Some(first) = subscribers.first() else {
        return;
    };
    let channel = match (subscribers.len(), serde_json::from_str::<Value>(&text)) {
        (2.., Ok(payload)) => subscribers
            .iter()
            .find(|c| {
                inner
                    .rt
                    .engine
                    .validate_message(inner.rt.bundle(), c, &payload)
                    .is_ok()
            })
            .unwrap_or(first),
        _ => first,
    };
    let msg = InMessage {
        address: path,
        key: None,
        headers: Vec::new(),
        payload: text.into_bytes(),
    };
    inner.receive_from(channel, msg, Some(&socket)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_paths() {
        assert_eq!(route_path("/chat"), "/chat");
        assert_eq!(route_path("/rooms/{room}"), "/rooms/{p1}");
        assert_eq!(route_path("/rooms/{id}/x"), "/rooms/{p1}/x");
        assert_eq!(route_path("/r/room-{id}"), "/r/{p1}");
        assert_eq!(route_path("/"), "/");
    }
}
