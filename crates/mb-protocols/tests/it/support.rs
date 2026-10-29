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

/// Waits up to 5 s until `broker` has at least `n` subscriptions.
///
/// [`MemoryBroker::inject`] reaches only the subscribers that exist when it
/// runs, so a test injects only once the consumers it expects are in place.
///
/// [`MemoryBroker::inject`]: mb_protocols::events::MemoryBroker::inject
#[cfg(feature = "events")]
pub async fn subscribed(broker: &mb_protocols::events::MemoryBroker, n: usize) {
    wait_until(Duration::from_secs(5), Duration::from_millis(2), || async {
        let groups = broker.subscribed_groups();
        if groups.len() >= n {
            Ok(())
        } else {
            Err(format!("wanted {n} subscriptions, got {groups:?}"))
        }
    })
    .await;
}

/// Waits up to 5 s until two readings of `count` taken `poll` apart agree,
/// and returns that count.
///
/// For a producer that should stop: once the count holds still, a test can
/// bound it and then check that it stays put.
#[cfg(any(feature = "graphql", feature = "sse"))]
pub async fn stable_count(poll: Duration, count: impl Fn() -> usize) -> usize {
    let mut last = count();
    // `wait_until` checks at once: the first reading to compare is `poll` on.
    tokio::time::sleep(poll).await;
    wait_until(Duration::from_secs(5), poll, || {
        let now = count();
        let previous = std::mem::replace(&mut last, now);
        async move {
            if now == previous {
                Ok(now)
            } else {
                Err(format!("still moving: {previous} -> {now}"))
            }
        }
    })
    .await
}
