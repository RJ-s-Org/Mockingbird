#![cfg(test)]
#![cfg(feature = "websocket")]

use std::time::Duration;

use axum::Router;
use axum::extract::WebSocketUpgrade;
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use mb_protocols::ws::{ChannelConfig, Outbound, serve_channel};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// Serves an echo channel on an ephemeral port; returns its `ws://` URL.
async fn serve_echo(cfg: fn() -> ChannelConfig) -> String {
    let router = Router::new().route(
        "/ws",
        get(move |upgrade: WebSocketUpgrade| async move {
            serve_channel(upgrade, cfg(), |mut io| async move {
                while let Some(text) = io.incoming.next().await {
                    if io.outgoing.send(Outbound::Text(text)).await.is_err() {
                        break;
                    }
                }
            })
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("ws://{addr}/ws")
}

fn fast_pings() -> ChannelConfig {
    ChannelConfig {
        subprotocols: vec!["proto-a", "proto-b"],
        keepalive: Duration::from_millis(100),
        max_message_bytes: 1024,
    }
}

async fn within<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), f)
        .await
        .expect("timed out")
}

#[tokio::test]
async fn serve_channel_negotiates_and_pings() {
    let url = serve_echo(fast_pings).await;
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("sec-websocket-protocol", "other, proto-b".parse().unwrap());
    let (mut ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert_eq!(resp.headers()["sec-websocket-protocol"], "proto-b");

    ws.send(Message::text("hello")).await.unwrap();
    let mut echoed = false;
    let mut pinged = false;
    while !(echoed && pinged) {
        match within(ws.next()).await.unwrap().unwrap() {
            Message::Text(t) => {
                assert_eq!(t.as_str(), "hello");
                echoed = true;
            }
            Message::Ping(_) => pinged = true,
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn oversized_frame_closes_1009() {
    let url = serve_echo(fast_pings).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws.send(Message::text("x".repeat(2048))).await.unwrap();
    loop {
        match within(ws.next()).await {
            Some(Ok(Message::Close(Some(frame)))) => {
                assert_eq!(frame.code, CloseCode::Size);
                assert_eq!(u16::from(frame.code), 1009);
                break;
            }
            Some(Ok(Message::Ping(_))) => continue,
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}
