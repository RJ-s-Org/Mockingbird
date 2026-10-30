//! An in-process broker: used by tests and by `memory` brokers.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use mb_mcm::ChannelBinding;
use tokio::sync::Notify;

use super::address::address_matches;
use super::broker::{BrokerClient, BrokerError, MessageStream, SubscriptionSender, subscription};
use super::{InMessage, OutMessage};

/// A live subscription: its address template and where to deliver.
struct Subscriber {
    template: String,
    tx: SubscriptionSender,
}

/// Published messages kept for [`MemoryBroker::published`].
const HISTORY: usize = 1000;

#[derive(Default)]
struct State {
    /// The last [`HISTORY`] published messages.
    published: VecDeque<(ChannelBinding, OutMessage)>,
    /// Every message ever published.
    published_count: usize,
    subscribers: Vec<Subscriber>,
    /// Publishes left to succeed before `failures` apply.
    successes_first: usize,
    /// Publishes left to fail.
    failures: usize,
    /// The consumer group of every subscription, in order.
    groups: Vec<String>,
}

/// A broker that delivers each published message to the subscribers
/// whose address template matches, and keeps the last 1,000.
#[derive(Default)]
pub struct MemoryBroker {
    state: Mutex<State>,
    published: Notify,
}

/// Shows counts, not the messages.
impl std::fmt::Debug for MemoryBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state();
        f.debug_struct("MemoryBroker")
            .field("published_count", &state.published_count)
            .field("subscribers", &state.subscribers.len())
            .finish_non_exhaustive()
    }
}

impl MemoryBroker {
    /// An empty broker, shared: every client of one memory broker holds the
    /// same instance.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The last 1,000 messages published, in order.
    pub fn published(&self) -> Vec<(ChannelBinding, OutMessage)> {
        self.state().published.iter().cloned().collect()
    }

    /// How many messages were ever published.
    pub fn published_count(&self) -> usize {
        self.state().published_count
    }

    /// The published messages once there are at least `n`, or whatever
    /// there is when `timeout` elapses.
    pub async fn wait_published(
        &self,
        n: usize,
        timeout: Duration,
    ) -> Vec<(ChannelBinding, OutMessage)> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.published.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let published = self.published();
            if published.len() >= n {
                return published;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.published();
            }
        }
    }

    /// Deliver `msg` to the subscribers matching `binding`'s address, as
    /// if a broker had sent it. It is not recorded as published.
    pub fn inject(&self, binding: &ChannelBinding, msg: InMessage) {
        deliver(&mut self.state(), binding.address(), &msg);
    }

    /// The consumer group named by each subscription so far, in order.
    pub fn subscribed_groups(&self) -> Vec<String> {
        self.state().groups.clone()
    }

    /// Make the next `n` publishes fail with [`BrokerError::Publish`].
    pub fn fail_publishes(&self, n: usize) {
        self.fail_publishes_after(0, n);
    }

    /// Let the next `ok` publishes succeed, then make `n` fail with
    /// [`BrokerError::Publish`].
    pub fn fail_publishes_after(&self, ok: usize, n: usize) {
        let mut state = self.state();
        state.successes_first = ok;
        state.failures = n;
    }
}

/// Send `msg` to every live subscriber whose template matches `address`,
/// dropping subscribers whose stream is gone.
fn deliver(state: &mut State, address: &str, msg: &InMessage) {
    state.subscribers.retain_mut(|sub| {
        if sub.tx.is_closed() {
            return false;
        }
        if !address_matches(&sub.template, address) {
            return true;
        }
        sub.tx.offer(msg.clone())
    });
}

#[async_trait]
impl BrokerClient for MemoryBroker {
    async fn publish(&self, target: &ChannelBinding, msg: OutMessage) -> Result<(), BrokerError> {
        {
            let mut state = self.state();
            if state.successes_first > 0 {
                state.successes_first -= 1;
            } else if state.failures > 0 {
                state.failures -= 1;
                return Err(BrokerError::Publish("injected failure".into()));
            }
            let address = target.address();
            let delivered = InMessage {
                address: address.to_string(),
                key: msg.key.clone(),
                headers: msg.headers.clone(),
                payload: msg.payload.clone(),
            };
            deliver(&mut state, address, &delivered);
            if state.published.len() >= HISTORY {
                state.published.pop_front();
            }
            state.published.push_back((target.clone(), msg));
            state.published_count += 1;
        }
        self.published.notify_waiters();
        Ok(())
    }

    async fn subscribe(
        &self,
        binding: &ChannelBinding,
        group: &str,
    ) -> Result<MessageStream, BrokerError> {
        let template = binding.address().to_string();
        let (tx, rx) = subscription("memory", &template);
        let mut state = self.state();
        state.groups.push(group.to_string());
        state.subscribers.push(Subscriber { template, tx });
        Ok(rx)
    }
}
