//! Helpers shared by the test modules.

use std::future::Future;
use std::time::Duration;

/// How often [`wait_until`] polls.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Polls `poll` until it returns `Some`, and returns that value.
///
/// Panics with a message naming `what` if `timeout` passes first, so a
/// condition that never holds fails the test where it waited.
pub async fn wait_until<T, F, Fut>(what: &str, timeout: Duration, mut poll: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = poll().await {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
