//! Helpers shared by the modules of this test binary. Each one replaces
//! copies that were identical across the former per-file test binaries.
#![cfg(test)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::net::TcpListener;

/// The repository root.
pub fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `rel` under the repository root.
pub fn repo_path(rel: &str) -> PathBuf {
    repo().join(rel)
}

/// `rel` under the repository's `fixtures/`.
pub fn fixture(rel: &str) -> PathBuf {
    repo().join("fixtures").join(rel)
}

/// The contents of `path`, panicking with the path on error.
pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A loopback port that was free a moment ago (another process may take it
/// before the caller binds it).
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A listener on an ephemeral loopback port.
pub async fn ephemeral() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

/// Serves `app` on an ephemeral loopback port; returns its base URL.
pub async fn serve(app: axum::Router) -> String {
    let listener = ephemeral().await;
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// Polls `done` every 20 ms until it returns true; panics naming `what`
/// once `within` has passed.
pub async fn wait_until(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(
            Instant::now() < deadline,
            "timed out after {within:?} waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
