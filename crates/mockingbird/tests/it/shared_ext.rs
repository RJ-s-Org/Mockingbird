//! One `Extensions` instance serves the whole binary: the quota guard and
//! usage sink handed to `build_app_with` admit and meter WebSocket channel
//! replies (P5) and admit import jobs (P6).
#![cfg(test)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::support::{repo_path, serve};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use mb_ext::{CallInfo, Extensions, QuotaGuard, Rejection, Tenancy, UsageEvent, UsageSink};
use mockingbird::config::{ServeArgs, build_app_with};
use serde_json::{Value, json};

/// Admits every call and records the operation ids it saw.
#[derive(Default)]
struct RecordingQuota(Mutex<Vec<String>>);

#[async_trait]
impl QuotaGuard for RecordingQuota {
    async fn admit(&self, _t: &Tenancy, call: &CallInfo<'_>) -> Result<(), Rejection> {
        self.0.lock().unwrap().push(call.operation_id.to_string());
        Ok(())
    }
}

#[derive(Default)]
struct RecordingUsage(Mutex<Vec<String>>);

impl UsageSink for RecordingUsage {
    fn record(&self, event: &UsageEvent) {
        self.0.lock().unwrap().push(event.operation_id.clone());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_channels_and_imports_share_quota_and_usage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mockingbird.yaml");
    std::fs::write(
        &path,
        format!(
            "brokers:\n  mem:\n    kind: memory\nmocks:\n  chat:\n    contract: {}\n",
            repo_path("fixtures/asyncapi/chat-ws-2.6.yaml").display()
        ),
    )
    .unwrap();
    let args = ServeArgs {
        config: Some(path),
        offline: true,
        ..ServeArgs::default()
    };
    let quota = Arc::new(RecordingQuota::default());
    let usage = Arc::new(RecordingUsage::default());
    let ext = Extensions::default()
        .with_quota(quota.clone())
        .with_usage(usage.clone());
    let app = build_app_with(&args, None, ext).await.unwrap();
    let data = serve(app.data.clone()).await;
    let admin = serve(app.admin.clone()).await;

    // P5: a WebSocket client's message triggers a reply, admitted and
    // metered through the binary's extensions.
    let ws_url = format!("{}/chat/chat", data.replacen("http://", "ws://", 1));
    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
    socket
        .send(tokio_tungstenite::tungstenite::Message::text(
            json!({"user": "ann", "text": "hi"}).to_string(),
        ))
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let reply: Value = serde_json::from_str(&frame.into_text().unwrap()).unwrap();
    assert!(reply.is_object(), "{reply}");

    // P6: an import job is admitted by the same quota guard.
    let http = reqwest::Client::new();
    let r = http
        .post(format!("{admin}/api/v1/mocks"))
        .json(&json!({"name": "pets"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let har = std::fs::read_to_string(repo_path("fixtures/smart/petstore.har")).unwrap();
    let r = http
        .post(format!("{admin}/api/v1/mocks/pets/imports"))
        .json(&json!({"kind": "har", "text": har, "filename": "petstore.har"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());

    let admitted = quota.0.lock().unwrap().clone();
    assert!(
        admitted.iter().any(|op| op == "chatMessage"),
        "{admitted:?}"
    );
    assert!(
        admitted.iter().any(|op| op == mb_admin::IMPORT_OPERATION),
        "{admitted:?}"
    );
    let metered = usage.0.lock().unwrap().clone();
    assert!(metered.iter().any(|op| op == "chatMessage"), "{metered:?}");
}
