//! Helpers shared by the test modules.

use std::time::Duration;

/// The URL in `var`, or `None` (with a "skipped" note) when it is unset, so
/// tests that need a Postgres or Redis server skip without one.
pub fn env_url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipped: {var} not set");
            None
        }
    }
}

/// Polls `done` every `poll` (yielding to the runtime in between) until it
/// returns true, and panics naming `what` if that takes longer than `within`.
pub async fn wait_until(
    within: Duration,
    poll: Duration,
    what: &str,
    mut done: impl FnMut() -> bool,
) {
    let waited = tokio::time::timeout(within, async {
        while !done() {
            if poll.is_zero() {
                tokio::task::yield_now().await;
            } else {
                tokio::time::sleep(poll).await;
            }
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "timed out after {within:?} waiting until {what}"
    );
}
