//! Shared test helpers.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

/// Counts every tracing event it sees.
#[derive(Clone, Default)]
pub struct EventCounter(Arc<AtomicUsize>);

impl EventCounter {
    /// Events seen so far.
    pub fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    /// A subscriber that feeds this counter.
    pub fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync {
        tracing_subscriber::registry().with(self.clone())
    }
}

impl<S: tracing::Subscriber> Layer<S> for EventCounter {
    fn on_event(&self, _event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
