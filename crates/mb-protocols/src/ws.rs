//! A reusable WebSocket channel: subprotocol negotiation, keepalive pings,
//! a frame size limit and text in/out queues, so protocol handlers (GraphQL
//! subscriptions, AsyncAPI channels) deal only with text messages.

use std::future::Future;
use std::time::Duration;

use axum::extract::WebSocketUpgrade;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::response::Response;
use futures::stream::{self, BoxStream};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;

/// Close code for a frame larger than the channel accepts.
const CLOSE_TOO_BIG: u16 = 1009;
/// Close code for a normal close (the handler finished).
const CLOSE_NORMAL: u16 = 1000;
/// Close code for any other read failure.
const CLOSE_PROTOCOL_ERROR: u16 = 1002;
/// Close code for a server-side failure ending an operation (`1011`,
/// internal error), e.g. a WebSocket operation stream chaos drops.
pub const CLOSE_INTERNAL_ERROR: u16 = 1011;
/// Messages queued in each direction before the other side waits.
const QUEUE: usize = 32;

/// A message from the handler to the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outbound {
    /// A text frame.
    Text(String),
    /// A close frame; the socket is closed after it is sent.
    Close { code: u16, reason: String },
}

/// A data frame from the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Inbound {
    /// A text frame.
    Text(String),
    /// A binary frame.
    Binary(Vec<u8>),
}

/// The handler's side of an open channel that takes binary frames too
/// (see [`serve_frames`]).
pub(crate) struct FrameIo {
    /// The negotiated subprotocol, if any.
    pub(crate) protocol: Option<String>,
    /// The client's data frames; ends when the client closes.
    pub(crate) incoming: BoxStream<'static, Inbound>,
    /// Frames to send. Dropping every sender closes the socket.
    pub(crate) outgoing: mpsc::Sender<Outbound>,
}

/// The handler's side of an open channel.
pub struct ChannelIo {
    /// The negotiated subprotocol, if any.
    pub protocol: Option<String>,
    /// The client's text frames; ends when the client closes.
    pub incoming: BoxStream<'static, String>,
    /// Frames to send. Dropping every sender closes the socket.
    pub outgoing: mpsc::Sender<Outbound>,
}

/// How a channel is served.
#[derive(Clone, Debug)]
pub struct ChannelConfig {
    /// Subprotocols this channel speaks, in order of preference.
    pub subprotocols: Vec<&'static str>,
    /// Interval between server pings.
    pub keepalive: Duration,
    /// The largest frame or message accepted; a larger one closes the
    /// socket with 1009. Mock channels use the mock's request size cap
    /// (see [`ChannelConfig::for_mock`]).
    pub max_message_bytes: usize,
}

impl ChannelConfig {
    /// A mock's channel speaking `subprotocols`: the mock's keepalive, and
    /// its request size cap (`limits.max_request_bytes`) as the largest
    /// message.
    pub fn for_mock(rt: &crate::MockRuntime, subprotocols: Vec<&'static str>) -> Self {
        Self {
            subprotocols,
            keepalive: rt.protocols.ws_keepalive,
            max_message_bytes: rt.limits.max_request_bytes,
        }
    }
}

/// Upgrades the connection and runs `handler` on it.
///
/// The subprotocol is the first of `cfg.subprotocols` the client offered.
/// Text frames are queued to `incoming`; binary frames are ignored (see
/// [`serve_frames`] for a handler that takes them); the
/// server pings every `cfg.keepalive`. The socket closes (1000) when the
/// handler returns or drops `outgoing`, with the handler's own code on
/// [`Outbound::Close`], and with 1009 on an oversized frame. The handler
/// runs on the connection's task, so dropping the connection drops it.
pub fn serve_channel<H, F>(upgrade: WebSocketUpgrade, cfg: ChannelConfig, handler: H) -> Response
where
    H: FnOnce(ChannelIo) -> F + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    serve_frames(upgrade, cfg, move |io: FrameIo| {
        let incoming = io
            .incoming
            .filter_map(|frame| async move {
                match frame {
                    Inbound::Text(text) => Some(text),
                    Inbound::Binary(_) => None,
                }
            })
            .boxed();
        handler(ChannelIo {
            protocol: io.protocol,
            incoming,
            outgoing: io.outgoing,
        })
    })
}

/// [`serve_channel`] for a handler that takes binary frames too: they are
/// queued to `incoming` as [`Inbound::Binary`].
pub(crate) fn serve_frames<H, F>(
    upgrade: WebSocketUpgrade,
    cfg: ChannelConfig,
    handler: H,
) -> Response
where
    H: FnOnce(FrameIo) -> F + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    upgrade
        .protocols(cfg.subprotocols.clone())
        .max_message_size(cfg.max_message_bytes)
        .max_frame_size(cfg.max_message_bytes)
        .on_upgrade(move |socket| run(socket, cfg.keepalive, handler))
}

async fn run<H, F>(socket: WebSocket, keepalive: Duration, handler: H)
where
    H: FnOnce(FrameIo) -> F,
    F: Future<Output = ()>,
{
    let protocol = socket
        .protocol()
        .and_then(|p| p.to_str().ok())
        .map(str::to_string);
    let (mut sink, mut frames) = socket.split();
    let (in_tx, in_rx) = mpsc::channel::<Inbound>(QUEUE);
    let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(QUEUE);
    let incoming = stream::unfold(in_rx, |mut rx| async move {
        rx.recv().await.map(|frame| (frame, rx))
    })
    .boxed();
    let handler = handler(FrameIo {
        protocol,
        incoming,
        outgoing: out_tx,
    });
    tokio::pin!(handler);

    let mut in_tx = Some(in_tx);
    // A data frame waiting for room in `incoming`; the socket is not read
    // meanwhile.
    let mut pending: Option<Inbound> = None;
    // Whether the client may still send frames.
    let mut socket_open = true;
    let mut pings = tokio::time::interval_at(tokio::time::Instant::now() + keepalive, keepalive);
    pings.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let close = loop {
        let reading = socket_open && pending.is_none();
        let mut end_incoming = false;
        tokio::select! {
            () = &mut handler => {
                // Flush what the handler queued before it returned.
                let mut close = (CLOSE_NORMAL, String::new());
                while let Ok(out) = out_rx.try_recv() {
                    match out {
                        Outbound::Text(t) => {
                            if sink.send(Message::Text(t.into())).await.is_err() {
                                return;
                            }
                        }
                        Outbound::Close { code, reason } => {
                            close = (code, reason);
                            break;
                        }
                    }
                }
                break close;
            }
            out = out_rx.recv() => match out {
                Some(Outbound::Text(t)) => {
                    if sink.send(Message::Text(t.into())).await.is_err() {
                        return;
                    }
                }
                Some(Outbound::Close { code, reason }) => break (code, reason),
                None => break (CLOSE_NORMAL, String::new()),
            },
            permit = reserve(in_tx.as_ref()), if pending.is_some() => match permit {
                Some(permit) => {
                    if let Some(frame) = pending.take() {
                        permit.send(frame);
                    }
                }
                // The handler dropped `incoming`: later frames are dropped.
                None => {
                    pending = None;
                    end_incoming = true;
                }
            },
            frame = frames.next(), if reading => match frame {
                Some(Ok(Message::Text(t))) => {
                    if in_tx.is_some() {
                        pending = Some(Inbound::Text(t.to_string()));
                    }
                }
                Some(Ok(Message::Binary(b))) => {
                    if in_tx.is_some() {
                        pending = Some(Inbound::Binary(b.to_vec()));
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None => {
                    socket_open = false;
                    end_incoming = true;
                }
                Some(Err(e)) => {
                    // The axum error hides tungstenite's; its message names
                    // a capacity error.
                    let text = e.to_string();
                    let code = if text.contains("Space limit exceeded") {
                        CLOSE_TOO_BIG
                    } else {
                        CLOSE_PROTOCOL_ERROR
                    };
                    tracing::debug!(error = %text, code, "closing WebSocket after a read error");
                    break (code, String::new());
                }
            },
            _ = pings.tick() => {
                if sink.send(Message::Ping(Default::default())).await.is_err() {
                    return;
                }
            }
        }
        if end_incoming {
            in_tx = None;
        }
    };
    let (code, reason) = close;
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _ = sink.send(Message::Close(Some(frame))).await;
    let _ = sink.close().await;
}

/// A permit to queue one incoming frame, or `None` once the handler
/// dropped `incoming`.
async fn reserve(tx: Option<&mpsc::Sender<Inbound>>) -> Option<mpsc::Permit<'_, Inbound>> {
    tx?.reserve().await.ok()
}
