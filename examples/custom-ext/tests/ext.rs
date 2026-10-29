//! The example's extensions, wired into an in-process offline app.
#![cfg(test)]

use std::path::{Path, PathBuf};

use mockingbird::config::{ServeArgs, build_app_with};
use mockingbird_custom_ext_example::extensions;

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel)
}

/// Serves the `orders` mock offline with `extensions(per_tenant)` and
/// returns the data-plane base URL.
async fn serve(dir: &Path, per_tenant: u64) -> String {
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.data.clone();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

async fn call(base: &str, tenant: Option<&str>) -> reqwest::Response {
    let mut req = reqwest::Client::new().get(format!("{base}/orders/orders"));
    if let Some(t) = tenant {
        req = req.header("X-Tenant", t);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn quota_rejects_after_limit_per_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let base = serve(dir.path(), 2).await;

    assert_eq!(call(&base, Some("a")).await.status(), 200);
    assert_eq!(call(&base, Some("a")).await.status(), 200);
    let r = call(&base, Some("a")).await;
    assert_eq!(r.status(), 429);
    assert_eq!(
        r.headers().get("retry-after").unwrap().to_str().unwrap(),
        "60"
    );
    assert_eq!(call(&base, Some("b")).await.status(), 200);
}

#[tokio::test]
async fn missing_header_is_anonymous_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let base = serve(dir.path(), 1).await;

    assert_eq!(call(&base, None).await.status(), 200);
    // The header-less call used up tenant `anonymous`'s single call.
    assert_eq!(call(&base, Some("anonymous")).await.status(), 429);
    assert_eq!(call(&base, None).await.status(), 429);
    assert_eq!(call(&base, Some("other")).await.status(), 200);
}
