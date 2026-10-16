//! Glue between the channel runtimes and the rest of the process: the
//! store-backed scheduler lease and the [`ChannelControl`] the Admin API
//! publishes through.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mb_admin::channels::{
    BrokerView, ChannelActivityView, ChannelControl, ChannelControlError, PublishedMessage,
};
use mb_protocols::events::{Brokers, LeaseProvider, PublishError};
use mb_store::ControlStore;
use serde_json::Value;
use uuid::Uuid;

use crate::registry::MockRegistry;

/// Scheduler leases kept in the control store, so one replica at a time
/// fires a mock's schedule and cron triggers. The lease key is the mock's
/// id (UUID text).
pub struct StoreLease {
    pub store: Arc<dyn ControlStore>,
}

#[async_trait]
impl LeaseProvider for StoreLease {
    async fn try_acquire(&self, key: &str, holder: &str, ttl: Duration) -> Result<bool, String> {
        let mock_id = Uuid::parse_str(key)
            .map_err(|e| format!("scheduler lease key {key:?} is not a mock id: {e}"))?;
        self.store
            .acquire_scheduler_lease(mock_id, holder, ttl)
            .await
            .map_err(|e| e.to_string())
    }
}

/// The mounted mocks' channel runtimes, as the Admin API sees them.
pub struct RegistryChannels {
    pub registry: Arc<MockRegistry>,
    pub brokers: Arc<Brokers>,
}

#[async_trait]
impl ChannelControl for RegistryChannels {
    async fn publish(
        &self,
        ws: &str,
        mock: &str,
        channel: &str,
        count: usize,
        context: Option<Value>,
    ) -> Result<Vec<PublishedMessage>, ChannelControlError> {
        let Some(runtime) = self.registry.channel_runtime(ws, mock) else {
            return Err(ChannelControlError::NotRunning(format!(
                "mock {} has no running channels",
                crate::registry::mock_label(ws, mock)
            )));
        };
        let published = runtime
            .publish_manual(channel, count, context)
            .await
            .map_err(control_error)?;
        Ok(published
            .into_iter()
            .map(|p| PublishedMessage {
                address: p.address,
                key: p.key,
                headers: p.headers,
                payload: p.payload,
                source: p.source.as_str().to_string(),
            })
            .collect())
    }

    fn activity(&self, ws: &str, mock: &str) -> BTreeMap<String, ChannelActivityView> {
        let Some(runtime) = self.registry.channel_runtime(ws, mock) else {
            return BTreeMap::new();
        };
        runtime
            .activity()
            .into_iter()
            .map(|(id, a)| {
                let view = ChannelActivityView {
                    published: a.published,
                    received: a.received,
                    last_published_at: a.last_published_at,
                    last_received_at: a.last_received_at,
                    last_error: a.last_error,
                };
                (id, view)
            })
            .collect()
    }

    fn brokers(&self) -> Vec<BrokerView> {
        self.brokers
            .infos()
            .into_iter()
            .map(|info| BrokerView {
                name: info.name,
                kind: info.kind.as_str().to_string(),
                url: info.url,
                status: info.status,
            })
            .collect()
    }
}

fn control_error(error: PublishError) -> ChannelControlError {
    match error {
        PublishError::UnknownChannel(_) => ChannelControlError::Unknown(error.to_string()),
        PublishError::NotPublishable(_) | PublishError::Disabled(_) => {
            ChannelControlError::Conflict(error.to_string())
        }
        PublishError::NoBroker(reason) => ChannelControlError::NotRunning(reason),
        PublishError::Broker(_) | PublishError::TooLarge(_) => {
            ChannelControlError::Failed(error.to_string())
        }
    }
}
