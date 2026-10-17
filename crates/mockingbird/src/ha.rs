//! High availability: carries control-plane, state and request-log changes
//! between nodes over the cross-node [`Bus`].
//!
//! Bus payloads carry ids only; receivers reload from the shared stores.
//! - Control store changes made on this node go to the local hub
//!   ([`Ha::changes`]) and onto the bus; mock, profile and workspace events
//!   from other nodes go to the hub too, and a bus [`BusEvent::Resync`]
//!   becomes [`HubEvent::Resync`] (reload everything).
//! - [`Ha::shared_state`] wraps the state store so its change feed carries
//!   both this node's changes (also announced on the bus) and other nodes'
//!   (marked [`ChangeOrigin::Remote`]).
//! - Request-log announcements are handled by
//!   [`StoreRecorder::with_bus`](crate::recorder::StoreRecorder::with_bus).
//!
//! With the in-process [`LocalBus`] of a single node nothing is announced
//! and nothing arrives, so behaviour is that of a node without HA.

use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use mb_state::{
    Bus, BusEvent, ChangeKind, ChangeOrigin, LeaderBackend, LocalBus, PgBus, RedisBus, SessionInfo,
    SessionKey, StateChanged, StateError, StateStore, StoredResource,
};
use mb_store::{ControlChange, ControlStore};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::file_config::{BusTarget, FileConfig};

/// Capacity of the hub and of the merged state change feed.
const CHANNEL_CAPACITY: usize = 1024;

/// What the local hub delivers to this node's receivers (the registry).
#[derive(Clone, Debug, PartialEq)]
pub enum HubEvent {
    /// A control-plane change, made on this node or another.
    Change(ControlChange),
    /// Changes may have been missed: reload everything.
    Resync,
}

/// This node's connection to the others: the bus, and the hub fed by the
/// bridges. Dropping the last handle stops the bridges.
pub struct Ha {
    pub bus: Arc<dyn Bus>,
    pub node_id: String,
    hub: broadcast::Sender<HubEvent>,
    tasks: Vec<JoinHandle<()>>,
}

impl Ha {
    /// Connects the bus `cfg.bus_target()` names (as node `node_id`) and
    /// starts the bridges.
    pub async fn start(
        cfg: &FileConfig,
        store: Arc<dyn ControlStore>,
        node_id: String,
    ) -> anyhow::Result<Arc<Self>> {
        let bus: Arc<dyn Bus> = match cfg.bus_target() {
            BusTarget::Local => LocalBus::new(node_id),
            BusTarget::Postgres(url) => PgBus::connect(&url, node_id)
                .await
                .map_err(|e| anyhow!("connecting the postgres bus: {e}"))?,
            BusTarget::Redis(url) => RedisBus::connect(&url, node_id)
                .await
                .map_err(|e| anyhow!("connecting the redis bus: {e}"))?,
        };
        Ok(Self::start_with_bus(bus, store).await)
    }

    /// Starts the bridges over `bus`; the node id is the bus's.
    pub async fn start_with_bus(bus: Arc<dyn Bus>, store: Arc<dyn ControlStore>) -> Arc<Self> {
        let (hub, _) = broadcast::channel(CHANNEL_CAPACITY);
        // Subscribe before returning, so nothing after it is missed.
        let local = store.subscribe();
        let remote = bus.subscribe();
        let tasks = vec![
            tokio::spawn(control_to_bus(local, Arc::clone(&bus), hub.clone())),
            tokio::spawn(bus_to_hub(remote, hub.clone())),
        ];
        Arc::new(Self {
            node_id: bus.node_id().to_owned(),
            bus,
            hub,
            tasks,
        })
    }

    /// Control-plane changes from every node, and resync requests.
    pub fn changes(&self) -> broadcast::Receiver<HubEvent> {
        self.hub.subscribe()
    }

    /// `inner` with a change feed shared across nodes (see [`BusStateStore`]).
    pub fn shared_state(&self, inner: Arc<dyn StateStore>) -> Arc<dyn StateStore> {
        BusStateStore::new(inner, Arc::clone(&self.bus))
    }

    /// Where leader election runs: the bus's backend, or always the leader
    /// on a single node.
    pub fn leader_backend(cfg: &FileConfig) -> LeaderBackend {
        match cfg.bus_target() {
            BusTarget::Local => LeaderBackend::Always,
            BusTarget::Postgres(url) => LeaderBackend::Postgres(url),
            BusTarget::Redis(url) => LeaderBackend::Redis(url),
        }
    }
}

impl Drop for Ha {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// The bus event announcing `change`.
fn bus_event(change: &ControlChange) -> BusEvent {
    match change.clone() {
        ControlChange::Mock { workspace, name } => BusEvent::Mock { workspace, name },
        ControlChange::Profile { workspace, name } => BusEvent::Profile { workspace, name },
        ControlChange::Workspace { name } => BusEvent::Workspace { name },
    }
}

/// Bridge 1: this node's control store changes go to the hub and the bus.
/// When changes were missed, this node resyncs and asks the others to.
async fn control_to_bus(
    mut changes: broadcast::Receiver<ControlChange>,
    bus: Arc<dyn Bus>,
    hub: broadcast::Sender<HubEvent>,
) {
    loop {
        let (local, announce) = match changes.recv().await {
            Ok(change) => {
                let event = bus_event(&change);
                (HubEvent::Change(change), event)
            }
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "control changes lagged; resyncing");
                (HubEvent::Resync, BusEvent::Resync)
            }
            Err(RecvError::Closed) => return,
        };
        let _ = hub.send(local);
        if !bus.is_local()
            && let Err(error) = bus.publish(announce).await
        {
            // The bus remembers the failure and asks the other nodes to
            // resync once it can publish again.
            tracing::warn!(%error, "cannot announce a control change on the bus; the other nodes will resync once it recovers");
        }
    }
}

/// Bridge 2: control events and resync requests from the bus go to the hub.
async fn bus_to_hub(mut events: broadcast::Receiver<BusEvent>, hub: broadcast::Sender<HubEvent>) {
    loop {
        let event = match events.recv().await {
            Ok(BusEvent::Mock { workspace, name }) => {
                HubEvent::Change(ControlChange::Mock { workspace, name })
            }
            Ok(BusEvent::Profile { workspace, name }) => {
                HubEvent::Change(ControlChange::Profile { workspace, name })
            }
            Ok(BusEvent::Workspace { name }) => HubEvent::Change(ControlChange::Workspace { name }),
            Ok(BusEvent::Resync) => HubEvent::Resync,
            Ok(BusEvent::State { .. } | BusEvent::RequestLogged { .. }) => continue,
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "bus events lagged; resyncing");
                HubEvent::Resync
            }
            Err(RecvError::Closed) => return,
        };
        let _ = hub.send(event);
    }
}

/// A state store whose change feed spans the nodes: it delegates every
/// method to the wrapped store, and [`StateStore::subscribe`] gives this
/// node's changes ([`ChangeOrigin::Local`], each also announced as a
/// [`BusEvent::State`]) merged with the other nodes' (`Remote`).
pub struct BusStateStore {
    inner: Arc<dyn StateStore>,
    merged: broadcast::Sender<StateChanged>,
    task: JoinHandle<()>,
}

impl BusStateStore {
    pub fn new(inner: Arc<dyn StateStore>, bus: Arc<dyn Bus>) -> Arc<Self> {
        let (merged, _) = broadcast::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(merge_state_changes(
            inner.subscribe(),
            bus.subscribe(),
            bus,
            merged.clone(),
        ));
        Arc::new(Self {
            inner,
            merged,
            task,
        })
    }
}

impl Drop for BusStateStore {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bridge 3: forwards local changes (and announces them) and relays remote
/// ones, until the wrapped store's feed closes.
async fn merge_state_changes(
    mut local: broadcast::Receiver<StateChanged>,
    mut remote: broadcast::Receiver<BusEvent>,
    bus: Arc<dyn Bus>,
    merged: broadcast::Sender<StateChanged>,
) {
    let mut remote_open = true;
    loop {
        tokio::select! {
            change = local.recv() => match change {
                Ok(mut change) => {
                    change.origin = ChangeOrigin::Local;
                    let _ = merged.send(change.clone());
                    if !bus.is_local()
                        && let Err(error) = bus.publish(state_event(&change)).await
                    {
                        tracing::warn!(%error, "cannot announce a state change on the bus");
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "state changes lagged");
                }
                Err(RecvError::Closed) => return,
            },
            event = remote.recv(), if remote_open => match event {
                Ok(BusEvent::State { workspace, mock, session, resource_type, id, deleted }) => {
                    let _ = merged.send(StateChanged {
                        session: SessionKey { workspace, mock, session },
                        resource_type,
                        id,
                        kind: if deleted { ChangeKind::Deleted } else { ChangeKind::Upserted },
                        origin: ChangeOrigin::Remote,
                    });
                }
                Ok(_) => {}
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "remote state changes lagged");
                }
                Err(RecvError::Closed) => remote_open = false,
            },
        }
    }
}

fn state_event(change: &StateChanged) -> BusEvent {
    BusEvent::State {
        workspace: change.session.workspace.clone(),
        mock: change.session.mock.clone(),
        session: change.session.session.clone(),
        resource_type: change.resource_type.clone(),
        id: change.id.clone(),
        deleted: change.kind == ChangeKind::Deleted,
    }
}

#[async_trait]
impl StateStore for BusStateStore {
    async fn get(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        self.inner.get(s, resource_type, id).await
    }
    async fn list(
        &self,
        s: &SessionKey,
        resource_type: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        self.inner.list(s, resource_type, limit).await
    }
    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        self.inner.upsert(s, r).await
    }
    async fn delete(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<bool, StateError> {
        self.inner.delete(s, resource_type, id).await
    }
    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.reset(s).await
    }
    async fn reset_mock(&self, workspace: &str, mock: &str) -> Result<(), StateError> {
        self.inner.reset_mock(workspace, mock).await
    }
    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.touch(s).await
    }
    async fn sessions(&self, workspace: &str, mock: &str) -> Result<Vec<SessionInfo>, StateError> {
        self.inner.sessions(workspace, mock).await
    }
    async fn session_count(&self, workspace: &str, mock: &str) -> Result<usize, StateError> {
        self.inner.session_count(workspace, mock).await
    }
    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        self.inner.export(s).await
    }
    async fn seed(&self, s: &SessionKey, resources: Vec<StoredResource>) -> Result<(), StateError> {
        self.inner.seed(s, resources).await
    }
    async fn ping(&self) -> Result<(), StateError> {
        self.inner.ping().await
    }
    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.merged.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mb_state::{MemoryBusHub, MemoryStore};

    use super::*;

    fn key() -> SessionKey {
        SessionKey {
            workspace: "default".into(),
            mock: "m".into(),
            session: "s".into(),
        }
    }

    fn resource(id: &str) -> StoredResource {
        StoredResource {
            resource_type: "things".into(),
            id: id.into(),
            data: serde_json::json!({"id": id}),
        }
    }

    async fn recv(rx: &mut broadcast::Receiver<StateChanged>) -> StateChanged {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("a change within 2s")
            .expect("open feed")
    }

    #[tokio::test]
    async fn local_and_remote_state_changes_are_merged_and_marked() {
        let hub = MemoryBusHub::new();
        let a = BusStateStore::new(
            Arc::new(MemoryStore::new(Duration::from_secs(60))),
            hub.join("a".into()),
        );
        let b = BusStateStore::new(
            Arc::new(MemoryStore::new(Duration::from_secs(60))),
            hub.join("b".into()),
        );
        let mut on_a = a.subscribe();
        let mut on_b = b.subscribe();

        a.upsert(&key(), resource("1")).await.expect("upsert");
        let local = recv(&mut on_a).await;
        assert_eq!(local.origin, ChangeOrigin::Local);
        let remote = recv(&mut on_b).await;
        assert_eq!(remote.origin, ChangeOrigin::Remote);
        assert_eq!(
            (remote.session, remote.resource_type, remote.id, remote.kind),
            (
                key(),
                "things".to_owned(),
                "1".to_owned(),
                ChangeKind::Upserted
            )
        );

        b.upsert(&key(), resource("2")).await.expect("upsert");
        b.delete(&key(), "things", "2").await.expect("delete");
        let mut seen = Vec::new();
        while seen.len() < 2 {
            let c = recv(&mut on_a).await;
            assert_eq!(c.origin, ChangeOrigin::Remote);
            seen.push(c.kind);
        }
        assert_eq!(seen, vec![ChangeKind::Upserted, ChangeKind::Deleted]);
    }

    #[tokio::test]
    async fn local_bus_announces_nothing() {
        let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(60)));
        let wrapped = BusStateStore::new(store, LocalBus::new("solo".into()));
        let mut rx = wrapped.subscribe();
        wrapped.upsert(&key(), resource("1")).await.expect("upsert");
        assert_eq!(recv(&mut rx).await.origin, ChangeOrigin::Local);
    }
}
