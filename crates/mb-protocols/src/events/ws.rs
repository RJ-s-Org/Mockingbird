//! WebSocket channels: one route per path template, serving the mock's
//! enabled WebSocket Publish channels (hub messages out) and Subscribe
//! channels (client frames in, validated, recorded and answered).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::WebSocketUpgrade;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use futures::{FutureExt, StreamExt};
use mb_mcm::{Channel, ChannelBinding, Direction};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use super::runtime::ChannelInner;
use super::{InMessage, MAX_MESSAGE_BYTES, address_matches};
use crate::ws::{ChannelConfig, ChannelIo, Outbound, serve_channel};

/// Close code sent when the mock's channel runtime is replaced or dropped.
pub const CLOSE_RUNTIME_REPLACED: u16 = 1012;
const CLOSE_REASON: &str = "mock reloaded";

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
    for channel in &inner.rt.bundle.channels {
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
            get(move |upgrade: WebSocketUpgrade, uri: Uri| {
                upgrade_channel(inner, templates, upgrade, uri)
            }),
        );
    }
    Some(router)
}

async fn upgrade_channel(
    inner: Arc<ChannelInner>,
    templates: Arc<Vec<String>>,
    upgrade: WebSocketUpgrade,
    uri: Uri,
) -> Response {
    let path = uri.path().to_string();
    let matched: Vec<&String> = templates
        .iter()
        .filter(|t| address_matches(t, &path))
        .collect();
    if matched.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let subscribers: Vec<Channel> = inner
        .rt
        .bundle
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
                    .validate_message(&inner.rt.bundle, c, &payload)
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
