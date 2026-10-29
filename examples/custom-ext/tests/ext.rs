//! The example's extensions, wired into an in-process offline app.
#![cfg(test)]

use std::path::{Path, PathBuf};

use mockingbird::config::{App, ServeArgs, build_app_with};
use mockingbird_custom_ext_example::extensions;
use serde_json::Value;

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel)
}

/// The `orders` mock served offline on a loopback port; [`Server::stop`]
/// shuts it down.
struct Server {
    base: String,
    app: App,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Server {
    /// Stops accepting, waits for the server task and drains the app.
    async fn stop(self) {
        let _ = self.stop.send(());
        self.task.await.unwrap().unwrap();
        self.app.shutdown().await;
    }
}

/// Serves the `orders` mock offline with `extensions(per_tenant)`.
async fn serve(dir: &Path, per_tenant: u64) -> Server {
    let path = dir.join("mockingbird.yaml");
    std::fs::write(
        &path,
        format!(
            "mocks:\n  orders:\n    contract: {}\n",
            fixture("openapi/orders-3.1.yaml").display()
        ),
    )
    .unwrap();
    let args = ServeArgs {
        config: Some(path),
        offline: true,
        ..ServeArgs::default()
    };
    let app = build_app_with(&args, None, extensions(per_tenant))
        .await
        .unwrap();
    // Port 0, and the bound listener goes to the server: no port race.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(
        axum::serve(listener, app.data.clone())
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .into_future(),
    );
    Server {
        base: format!("http://{addr}"),
        app,
        stop,
        task,
    }
}

async fn call(base: &str, tenant: Option<&str>) -> reqwest::Response {
    let mut req = reqwest::Client::new().get(format!("{base}/orders/orders"));
    if let Some(t) = tenant {
        req = req.header("X-Tenant", t);
    }
    req.send().await.unwrap()
}

/// A served call: 200 with a `listOrders` body (an array of orders), so a
/// different route that also answers 200 fails the test.
async fn assert_orders(base: &str, tenant: Option<&str>) {
    let r = call(base, tenant).await;
    assert_eq!(r.status(), 200, "tenant {tenant:?}");
    let body: Value = r.json().await.unwrap();
    let orders = body.as_array().unwrap_or_else(|| panic!("{body}"));
    // The faker makes at least one element of a top-level array.
    assert!(!orders.is_empty(), "{body}");
    for order in orders {
        assert!(order["id"].is_string(), "{order}");
        assert!(order["customerId"].is_string(), "{order}");
        assert!(
            matches!(order["status"].as_str(), Some("pending" | "shipped")),
            "{order}"
        );
        assert!(order["total"].as_f64().is_some_and(|t| t >= 0.0), "{order}");
    }
}

/// A call the quota refused: 429, `Retry-After: 60` and an RFC 9457 problem
/// naming the tenant.
async fn assert_quota_exceeded(base: &str, tenant: Option<&str>, named: &str, limit: u64) {
    let r = call(base, tenant).await;
    assert_eq!(r.status(), 429, "tenant {tenant:?}");
    assert_eq!(
        r.headers().get("retry-after").unwrap().to_str().unwrap(),
        "60"
    );
    assert_eq!(
        r.headers().get("content-type").unwrap().to_str().unwrap(),
        "application/problem+json"
    );
    let problem: Value = r.json().await.unwrap();
    assert_eq!(problem["status"], 429, "{problem}");
    assert_eq!(problem["title"], "Quota exceeded", "{problem}");
    assert_eq!(
        problem["detail"],
        format!("tenant \"{named}\" has used all {limit} calls of its quota"),
        "{problem}"
    );
}

#[tokio::test]
async fn quota_rejects_after_limit_per_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), 2).await;
    let base = &server.base;

    assert_orders(base, Some("a")).await;
    assert_orders(base, Some("a")).await;
    assert_quota_exceeded(base, Some("a"), "a", 2).await;
    assert_orders(base, Some("b")).await;
    server.stop().await;
}

#[tokio::test]
async fn missing_header_is_anonymous_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), 1).await;
    let base = &server.base;

    assert_orders(base, None).await;
    // The header-less call used up tenant `anonymous`'s single call.
    assert_quota_exceeded(base, Some("anonymous"), "anonymous", 1).await;
    assert_quota_exceeded(base, None, "anonymous", 1).await;
    assert_orders(base, Some("other")).await;
    server.stop().await;
}
