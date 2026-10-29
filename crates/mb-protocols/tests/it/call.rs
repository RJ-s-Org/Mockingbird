#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_protocols::{MockRuntime, STREAM_ITEMS_HEADER, http_rest};
use mb_state::MemoryStore;
use serde_json::Value;
use tower::ServiceExt;

fn runtime() -> MockRuntime {
    let path = format!(
        "{}/../../fixtures/openapi/orders-3.0.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let bundle = Arc::new(compile(&std::fs::read(path).unwrap()).unwrap());
    MockRuntime::new(
        "orders",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    )
}

/// `X-Mockingbird-Stream-Items` must be an integer in 1..=10000.
#[tokio::test]
async fn stream_items_header_validated() {
    let router = http_rest::router(Arc::new(runtime()));
    for bad in ["0", "abc", "10001"] {
        let req = Request::get("/orders/o1")
            .header(STREAM_ITEMS_HEADER, bad)
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "value {bad:?}");
        assert_eq!(resp.headers()["content-type"], "application/problem+json");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["title"], "Invalid override", "value {bad:?}");
    }
    for good in ["1", "10000"] {
        let req = Request::get("/orders/o1")
            .header(STREAM_ITEMS_HEADER, good)
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "value {good:?}");
    }
}
