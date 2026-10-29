//! Helpers shared by the integration-test modules.

use std::future::Future;
#[cfg(any(
    feature = "grpc",
    feature = "graphql",
    feature = "soap",
    feature = "sse"
))]
use std::sync::Mutex;
use std::time::Duration;

#[cfg(any(
    feature = "grpc",
    feature = "graphql",
    feature = "soap",
    feature = "sse"
))]
use mb_protocols::{RequestRecord, RequestRecorder};

/// Polls `check` every `poll` until it returns `Ok`, and returns that value.
///
/// Panics once `timeout` has passed, with the message of the last `Err`, so a
/// condition that never holds fails the test with the state it last saw
/// instead of letting the test carry on with partial results.
pub async fn wait_until<T, F, Fut>(timeout: Duration, poll: Duration, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let last = match check().await {
            Ok(value) => return value,
            Err(last) => last,
        };
        if tokio::time::Instant::now() >= deadline {
            panic!("condition not met within {timeout:?}: {last}");
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(any(
    feature = "grpc",
    feature = "graphql",
    feature = "soap",
    feature = "sse"
))]
/// A [`RequestRecorder`] that keeps every record in memory.
#[derive(Default)]
pub struct Records(pub Mutex<Vec<RequestRecord>>);

#[cfg(any(
    feature = "grpc",
    feature = "graphql",
    feature = "soap",
    feature = "sse"
))]
impl RequestRecorder for Records {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

#[cfg(feature = "graphql")]
impl Records {
    /// Waits up to 2 s for at least `n` records and returns them all.
    pub async fn wait(&self, n: usize) -> Vec<RequestRecord> {
        wait_until(
            Duration::from_secs(2),
            Duration::from_millis(20),
            || async {
                let got = self.0.lock().unwrap().clone();
                if got.len() >= n {
                    Ok(got)
                } else {
                    Err(format!(
                        "wanted {n} request records, got {}: {got:?}",
                        got.len()
                    ))
                }
            },
        )
        .await
    }
}
