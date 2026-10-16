//! An in-process broker: used by tests and by `memory` brokers.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use mb_mcm::ChannelBinding;
use tokio::sync::Notify;

use super::address::address_matches;
use super::broker::{BrokerClient, BrokerError, MessageStream};
use super::{InMessage, OutMessage};

/// A live subscription: its address template and where to deliver.
struct Subscriber {
    template: String,
    tx: UnboundedSender<Result<InMessage, BrokerError>>,
}

#[derive(Default)]
struct State {
    published: Vec<(ChannelBinding, OutMessage)>,
    subscribers: Vec<Subscriber>,
    /// Publishes left to fail.
    failures: usize,
}

/// A broker that keeps every published message and delivers it to the
/// subscribers whose address template matches.
#[derive(Default)]
pub struct MemoryBroker {
    state: Mutex<State>,
    published: Notify,
}

impl MemoryBroker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Every message published so far, in order.
    pub fn published(&self) -> Vec<(ChannelBinding, OutMessage)> {
        self.state().published.clone()
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

    /// Make the next `n` publishes fail with [`BrokerError::Publish`].
    pub fn fail_publishes(&self, n: usize) {
        self.state().failures = n;
    }
}

/// Send `msg` to every live subscriber whose template matches `address`,
/// dropping subscribers whose stream is gone.
fn deliver(state: &mut State, address: &str, msg: &InMessage) {
    state.subscribers.retain(|sub| {
        if sub.tx.is_closed() {
            return false;
        }
        if !address_matches(&sub.template, address) {
            return true;
        }
        sub.tx.unbounded_send(Ok(msg.clone())).is_ok()
    });
}

#[async_trait]
impl BrokerClient for MemoryBroker {
    async fn publish(&self, target: &ChannelBinding, msg: OutMessage) -> Result<(), BrokerError> {
        {
            let mut state = self.state();
            if state.failures > 0 {
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
            state.published.push((target.clone(), msg));
        }
        self.published.notify_waiters();
        Ok(())
    }

    async fn subscribe(
        &self,
        binding: &ChannelBinding,
        _group: &str,
    ) -> Result<MessageStream, BrokerError> {
        let (tx, rx) = unbounded();
        self.state().subscribers.push(Subscriber {
            template: binding.address().to_string(),
            tx,
        });
        Ok(rx.boxed())
    }
}
