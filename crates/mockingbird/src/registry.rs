//! The set of mounted mocks, hot-swapped as the control store changes, and
//! the data-plane dispatcher that routes requests to them by path prefix.
//!
//! Each mock is mounted by its contract's protocol family: REST, GraphQL
//! and SOAP mocks are dispatched by route prefix on the data plane; gRPC
//! mocks hold no prefix and are published to the shared [`GrpcServer`].

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Router;
use axum::extract::Request;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use mb_mcm::{Channel, ChannelBinding, Direction, Protocol};
use mb_protocols::events::{ChannelRuntime, ChannelSetup, EventSetup};
use mb_protocols::grpc::{GrpcError, GrpcMock, GrpcServer};
use mb_protocols::http_rest::{self, problem};
use mb_protocols::{MountPrefix, StateHealth, graphql, soap};
use mb_store::{ControlChange, ControlStore, DEFAULT_WORKSPACE, Mock};
use tokio::sync::{Mutex, broadcast};
use tower::ServiceExt;

use crate::runtime::{RuntimeDeps, build_runtime};

/// One mounted mock.
#[derive(Clone)]
struct Mounted {
    name: String,
    served: Served,
    /// The runtime's state store circuit breaker.
    state_health: Arc<StateHealth>,
    /// The channels of a mock whose contract has some. Dropping the last
    /// handle (when the mock is replaced or unmounted) stops its tasks.
    events: Option<Arc<ChannelRuntime>>,
    /// The contract's channels, for the startup log.
    channels: Vec<Channel>,
}

/// How a mounted mock is reached.
#[derive(Clone)]
enum Served {
    /// By the data-plane dispatcher, below a route prefix.
    Http { prefix: String, router: Router },
    /// By the shared gRPC server; the mock's route prefix is ignored.
    Grpc(Arc<GrpcMock>),
}

impl Mounted {
    /// The data-plane route prefix; `None` for a gRPC mock.
    fn prefix(&self) -> Option<&str> {
        match &self.served {
            Served::Http { prefix, .. } => Some(prefix),
            Served::Grpc(_) => None,
        }
    }
}

/// Mounted mocks, sorted by prefix length, longest first.
///
/// A mock whose rebuild fails keeps its previous version mounted and is
/// listed in [`MockRegistry::failed_mocks`] until a rebuild succeeds; it is
/// unmounted only when it is deleted or has no active contract.
///
/// After every change the gRPC mocks are published to [`MockRegistry::grpc`].
/// A gRPC mock serving a service that a mock sorting first by name already
/// serves is skipped (and listed as failed) until that service is free.
pub struct MockRegistry {
    mounted: ArcSwap<Vec<Mounted>>,
    /// Mock name → why its last rebuild failed.
    failed: std::sync::Mutex<BTreeMap<String, String>>,
    /// Mock name → the service collision that kept it off the gRPC server.
    grpc_skipped: std::sync::Mutex<BTreeMap<String, String>>,
    /// The h2c server every gRPC mock is published to.
    grpc: Arc<GrpcServer>,
    /// Serialises reloads so concurrent updates cannot drop each other's work.
    reload: Mutex<()>,
}

impl MockRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mounted: ArcSwap::from_pointee(Vec::new()),
            failed: std::sync::Mutex::new(BTreeMap::new()),
            grpc_skipped: std::sync::Mutex::new(BTreeMap::new()),
            grpc: GrpcServer::new(),
            reload: Mutex::new(()),
        })
    }

    /// The shared gRPC server the gRPC mocks are published to.
    pub fn grpc(&self) -> Arc<GrpcServer> {
        Arc::clone(&self.grpc)
    }

    /// Rebuilds every mock. A mock that fails to build is logged, recorded
    /// as failed and keeps its previous version mounted, if it had one.
    pub async fn reload_all(&self, deps: &RuntimeDeps) -> anyhow::Result<()> {
        let _guard = self.reload.lock().await;
        self.reload_all_locked(deps).await
    }

    /// [`Self::reload_all`] for a caller that already holds `self.reload`.
    async fn reload_all_locked(&self, deps: &RuntimeDeps) -> anyhow::Result<()> {
        let mut mocks = deps.store.list_mocks(DEFAULT_WORKSPACE).await?;
        mocks.sort_by(|a, b| a.name.cmp(&b.name));
        let current = self.mounted.load_full();
        let mut failed = BTreeMap::new();
        let mut next: Vec<Mounted> = Vec::new();
        for mock in mocks {
            let mounted = match build_mounted(deps, &mock).await {
                Ok(Some(mounted)) => mounted,
                Ok(None) => continue,
                Err(error) => {
                    failed.insert(mock.name.clone(), error);
                    match current.iter().find(|m| m.name == mock.name) {
                        Some(previous) => previous.clone(),
                        None => continue,
                    }
                }
            };
            if let Some(prefix) = mounted.prefix()
                && let Some(holder) = next.iter().find(|m| m.prefix() == Some(prefix))
            {
                tracing::error!(
                    mock = %mounted.name,
                    prefix = %prefix,
                    holder = %holder.name,
                    "route prefix already mounted by another mock; mock skipped"
                );
                continue;
            }
            next.push(mounted);
        }
        self.publish(next).await;
        if let Ok(mut slot) = self.failed.lock() {
            *slot = failed;
        }
        Ok(())
    }

    /// Rebuilds one mock, unmounting it when it was deleted or has no
    /// active contract. When the build fails, the mock keeps its previous
    /// version mounted and is recorded as failed.
    ///
    /// When the mock gives up a prefix it held (it is unmounted or moves to
    /// another prefix), a mock skipped earlier for colliding on that prefix
    /// may now be mountable, so every mock is rebuilt instead.
    pub async fn reload_mock(&self, deps: &RuntimeDeps, name: &str) {
        let _guard = self.reload.lock().await;
        let built = match deps.store.get_mock(DEFAULT_WORKSPACE, name).await {
            Ok(Some(mock)) => build_mounted(deps, &mock).await,
            Ok(None) => Ok(None),
            Err(error) => {
                tracing::error!(mock = %name, %error, "cannot load mock; leaving it as it was");
                return;
            }
        };
        let mounted = match built {
            Ok(mounted) => {
                self.set_failed(name, None);
                mounted
            }
            Err(error) => {
                // Whatever is mounted (the previous version, or nothing)
                // stays as it is.
                self.set_failed(name, Some(error));
                return;
            }
        };

        let held = self
            .mounted
            .load()
            .iter()
            .find(|m| m.name == name)
            .and_then(|m| m.prefix().map(str::to_owned));
        let released = match (&held, &mounted) {
            (Some(old), Some(new)) => new.prefix() != Some(old.as_str()),
            (Some(_), None) => true,
            (None, _) => false,
        };
        if released {
            match self.reload_all_locked(deps).await {
                Ok(()) => return,
                Err(error) => {
                    tracing::error!(
                        mock = %name,
                        error = %format!("{error:#}"),
                        "cannot reload mocks after a prefix was released; reloading this mock only"
                    );
                }
            }
        }

        let mut next: Vec<Mounted> = self
            .mounted
            .load()
            .iter()
            .filter(|m| m.name != name)
            .cloned()
            .collect();
        if let Some(mounted) = mounted {
            let prefix = mounted.prefix().map(str::to_owned);
            let holder = prefix
                .as_deref()
                .and_then(|p| next.iter().position(|m| m.prefix() == Some(p)));
            match (holder, prefix) {
                // The holder sorts first by name, so it keeps the prefix.
                (Some(i), Some(prefix)) if next[i].name < mounted.name => {
                    tracing::error!(
                        mock = %mounted.name,
                        prefix = %prefix,
                        holder = %next[i].name,
                        "route prefix already mounted by another mock; mock skipped"
                    );
                }
                (Some(i), Some(prefix)) => {
                    tracing::error!(
                        mock = %next[i].name,
                        prefix = %prefix,
                        holder = %mounted.name,
                        "route prefix taken by a mock that sorts first; mock unmounted"
                    );
                    next[i] = mounted;
                }
                _ => next.push(mounted),
            }
        }
        self.publish(next).await;
    }

    /// Applies control-plane changes as they are published: a mock change
    /// reloads that mock; a profile change, or missed changes, reload all.
    pub fn spawn_watcher(self: &Arc<Self>, deps: RuntimeDeps) -> tokio::task::JoinHandle<()> {
        let mut changes = deps.store.subscribe();
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(ControlChange::Mock { workspace, name }) => {
                        if workspace == DEFAULT_WORKSPACE {
                            registry.reload_mock(&deps, &name).await;
                        }
                    }
                    Ok(ControlChange::Profile { .. })
                    | Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Err(error) = registry.reload_all(&deps).await {
                            tracing::error!(%error, "cannot reload mocks");
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    /// The data-plane router: forwards each request to the mock with the
    /// longest matching prefix, with that prefix stripped.
    pub fn router(self: &Arc<Self>) -> Router {
        let registry = Arc::clone(self);
        Router::new().fallback(move |req: Request| {
            let registry = Arc::clone(&registry);
            async move { registry.dispatch(req).await }
        })
    }

    /// `(name, prefix)` of every mock mounted on the data plane, longest
    /// prefix first. gRPC mocks are listed by [`GrpcServer::services`].
    pub fn mounted(&self) -> Vec<(String, String)> {
        self.mounted
            .load()
            .iter()
            .filter_map(|m| m.prefix().map(|p| (m.name.clone(), p.to_owned())))
            .collect()
    }

    /// `(name, error)` of every mock whose last rebuild failed, or that is
    /// kept off the gRPC server by a service collision, sorted by name. A
    /// mock whose rebuild failed serves its previous version, if it had one.
    pub fn failed_mocks(&self) -> Vec<(String, String)> {
        let mut all = self.failed.lock().map(|f| f.clone()).unwrap_or_default();
        if let Ok(skipped) = self.grpc_skipped.lock() {
            for (name, error) in skipped.iter() {
                all.entry(name.clone()).or_insert_with(|| error.clone());
            }
        }
        all.into_iter().collect()
    }

    fn set_failed(&self, name: &str, error: Option<String>) {
        if let Ok(mut failed) = self.failed.lock() {
            match error {
                Some(error) => failed.insert(name.to_string(), error),
                None => failed.remove(name),
            };
        }
    }

    /// The channel runtime of mounted mock `mock`; `None` when it is not
    /// mounted or its contract has no channels.
    pub fn channel_runtime(&self, mock: &str) -> Option<Arc<ChannelRuntime>> {
        self.mounted
            .load()
            .iter()
            .find(|m| m.name == mock)
            .and_then(|m| m.events.clone())
    }

    /// `(mock, reason)` for every reason a mounted mock's channels are not
    /// all working (see [`ChannelRuntime::health`]), sorted.
    pub fn event_issues(&self) -> Vec<(String, String)> {
        let mut issues: Vec<(String, String)> = self
            .mounted
            .load()
            .iter()
            .filter_map(|m| m.events.as_ref().map(|events| (&m.name, events.health())))
            .flat_map(|(name, reasons)| reasons.into_iter().map(|r| (name.clone(), r)))
            .collect();
        issues.sort();
        issues
    }

    /// `(mock, protocol, channel id, direction, address)` of every channel
    /// of the mounted mocks with a channel runtime, by mock. WebSocket
    /// addresses are paths below the mock's route prefix.
    pub fn served_channels(&self) -> Vec<ServedChannel> {
        let mut out = Vec::new();
        for m in self.mounted.load().iter().filter(|m| m.events.is_some()) {
            for channel in &m.channels {
                let address = match (&channel.binding, m.prefix()) {
                    (ChannelBinding::WebSocket { path }, Some(prefix)) => {
                        format!("{}{path}", prefix.trim_end_matches('/'))
                    }
                    (binding, _) => binding.address().to_string(),
                };
                out.push(ServedChannel {
                    mock: m.name.clone(),
                    protocol: channel.binding.protocol(),
                    id: channel.id.clone(),
                    direction: match channel.direction {
                        Direction::Publish => "publish",
                        Direction::Subscribe => "subscribe",
                    },
                    address,
                });
            }
        }
        out.sort_by(|a, b| a.mock.cmp(&b.mock));
        out
    }

    /// Names of mounted mocks currently serving statelessly because their
    /// state store calls are failing, sorted.
    pub fn degraded_mocks(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .mounted
            .load()
            .iter()
            .filter(|m| m.state_health.is_degraded())
            .map(|m| m.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Installs `next` as the mounted set and publishes its gRPC mocks.
    /// Callers hold `self.reload`, so publishes do not interleave.
    async fn publish(&self, mut next: Vec<Mounted>) {
        let prefix_len = |m: &Mounted| m.prefix().map_or(0, str::len);
        next.sort_by(|a, b| {
            prefix_len(b)
                .cmp(&prefix_len(a))
                .then_with(|| a.name.cmp(&b.name))
        });
        let grpc: Vec<(String, Arc<GrpcMock>)> = next
            .iter()
            .filter_map(|m| match &m.served {
                Served::Grpc(mock) => Some((m.name.clone(), Arc::clone(mock))),
                Served::Http { .. } => None,
            })
            .collect();
        self.mounted.store(Arc::new(next));
        self.publish_grpc(grpc).await;
    }

    /// Publishes the gRPC mocks, in name order: a mock serving a service an
    /// earlier one already serves is skipped and recorded.
    async fn publish_grpc(&self, mut mocks: Vec<(String, Arc<GrpcMock>)>) {
        mocks.sort_by(|a, b| a.0.cmp(&b.0));
        let mut holders: BTreeMap<String, String> = BTreeMap::new();
        let mut skipped = BTreeMap::new();
        let mut published = Vec::new();
        for (name, mock) in mocks {
            let services = mock.services();
            let collision = services
                .iter()
                .find_map(|s| holders.get(s).map(|holder| (s.clone(), holder.clone())));
            if let Some((service, holder)) = collision {
                let error = GrpcError::ServiceCollision { service, holder }.to_string();
                tracing::error!(mock = %name, %error, "gRPC service already served by another mock; mock skipped");
                skipped.insert(name, error);
                continue;
            }
            for service in services {
                holders.insert(service, name.clone());
            }
            published.push(mock);
        }
        if let Err(error) = self.grpc.publish(published).await {
            tracing::error!(%error, "cannot publish the gRPC mocks; keeping the previous set");
        }
        if let Ok(mut slot) = self.grpc_skipped.lock() {
            *slot = skipped;
        }
    }

    async fn dispatch(&self, mut req: Request) -> Response {
        // One snapshot per request: a swap mid-request does not affect it.
        let snapshot = self.mounted.load_full();
        let path = req.uri().path().to_owned();
        let Some((prefix, router, rest)) = snapshot.iter().find_map(|m| match &m.served {
            Served::Http { prefix, router } => {
                strip_prefix(&path, prefix).map(|rest| (prefix, router, rest))
            }
            Served::Grpc(_) => None,
        }) else {
            return problem(
                404,
                "No mock mounted at this path",
                format!("no mock is mounted at a prefix of {path}"),
            );
        };
        match rewrite_path(req.uri(), rest) {
            Ok(uri) => *req.uri_mut() = uri,
            Err(error) => return problem(400, "Invalid request path", error.to_string()),
        }
        // Adapters that print absolute URLs (SOAP's `?wsdl`) put it back.
        let mount = prefix.trim_end_matches('/').to_owned();
        req.extensions_mut().insert(MountPrefix(mount));
        let router = router.clone();
        match router.oneshot(req).await {
            Ok(response) => response.into_response(),
            Err(never) => match never {},
        }
    }
}

/// One channel of a mounted mock, as the startup log lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct ServedChannel {
    pub mock: String,
    /// `kafka`, `amqp`, `mqtt` or `websocket`.
    pub protocol: &'static str,
    pub id: String,
    /// `publish` (the mock produces) or `subscribe` (it consumes).
    pub direction: &'static str,
    /// The broker address, or for WebSocket the path on the data plane.
    pub address: String,
}

/// The path below `prefix`, with a leading `/`, when `path` lies under
/// `prefix` at a segment boundary.
fn strip_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix == "/" {
        return Some(path);
    }
    let rest = path.strip_prefix(prefix)?;
    if rest.is_empty() {
        Some("/")
    } else if rest.starts_with('/') {
        Some(rest)
    } else {
        None
    }
}

fn rewrite_path(uri: &Uri, path: &str) -> Result<Uri, axum::http::Error> {
    let path_and_query = match uri.query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_owned(),
    };
    Uri::builder().path_and_query(path_and_query).build()
}

/// The mounted router for `mock`; `Ok(None)` when it has no active
/// contract; `Err` (logged) when it cannot be built.
async fn build_mounted(deps: &RuntimeDeps, mock: &Mock) -> Result<Option<Mounted>, String> {
    let runtime = match build_runtime(deps, mock).await {
        Ok(Some(rt)) => rt,
        Ok(None) => return Ok(None),
        Err(error) => {
            let error = format!("{error:#}");
            tracing::error!(mock = %mock.name, %error, "cannot build mock; keeping any previous version mounted");
            return Err(error);
        }
    };
    let state_health = Arc::clone(&runtime.state_health);
    let protocol = runtime.bundle.protocol();
    let channels = runtime.bundle.channels.clone();
    let runtime = Arc::new(runtime);
    let events = if channels.is_empty() {
        None
    } else {
        let setup = event_setup(deps.store.as_ref(), mock, &channels)
            .await
            .map_err(|error| {
                let error = format!("{error:#}");
                tracing::error!(mock = %mock.name, %error, "cannot load the event settings; keeping any previous version mounted");
                error
            })?;
        Some(ChannelRuntime::start(
            Arc::clone(&runtime),
            deps.events.clone(),
            setup,
        ))
    };
    let ws = events.as_ref().and_then(|events| events.ws_router());
    let http = |router: Router| Served::Http {
        prefix: mock.route_prefix.clone(),
        router: match &ws {
            Some(ws) => ws.clone().fallback_service(router),
            None => router,
        },
    };
    let served = match protocol {
        Protocol::Http => http_rest::try_router(runtime)
            .map(http)
            .map_err(|e| e.to_string()),
        Protocol::Graphql => graphql::try_router(runtime)
            .map(http)
            .map_err(|e| e.to_string()),
        Protocol::Soap => soap::try_router(runtime)
            .map(http)
            .map_err(|e| e.to_string()),
        Protocol::Grpc => GrpcMock::new(runtime)
            .map(|m| Served::Grpc(Arc::new(m)))
            .map_err(|e| e.to_string()),
        other => Err(format!("unsupported protocol {other:?}")),
    };
    match served {
        Ok(served) => Ok(Some(Mounted {
            name: mock.name.clone(),
            served,
            state_health,
            events,
            channels,
        })),
        Err(error) => {
            tracing::error!(mock = %mock.name, %error, "cannot route mock; keeping any previous version mounted");
            Err(format!("cannot route the contract: {error}"))
        }
    }
}

/// The stored event settings of `mock`: its broker and, by channel id,
/// each stored channel's `enabled` and triggers (the contract's when the
/// stored row keeps them).
async fn event_setup(
    store: &dyn ControlStore,
    mock: &Mock,
    channels: &[Channel],
) -> anyhow::Result<EventSetup> {
    use anyhow::Context as _;
    let broker = store
        .get_event_settings(mock.id)
        .await
        .with_context(|| format!("loading the event settings of mock {:?}", mock.name))?
        .and_then(|s| s.broker);
    let stored = store
        .list_channel_settings(mock.id)
        .await
        .with_context(|| format!("loading the channel settings of mock {:?}", mock.name))?;
    let channels = stored
        .into_iter()
        .map(|s| {
            let triggers = s.triggers.unwrap_or_else(|| {
                channels
                    .iter()
                    .find(|c| c.id == s.channel_id)
                    .map(Channel::effective_triggers)
                    .unwrap_or_default()
            });
            let setup = ChannelSetup {
                enabled: s.enabled,
                triggers,
            };
            (s.channel_id, setup)
        })
        .collect();
    Ok(EventSetup {
        broker,
        channels,
        lease_key: mock.id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::strip_prefix;

    #[test]
    fn prefixes_match_at_segment_boundaries() {
        assert_eq!(strip_prefix("/a/b", "/"), Some("/a/b"));
        assert_eq!(strip_prefix("/a", "/a"), Some("/"));
        assert_eq!(strip_prefix("/a/", "/a"), Some("/"));
        assert_eq!(strip_prefix("/a/b", "/a"), Some("/b"));
        assert_eq!(strip_prefix("/ab", "/a"), None);
        assert_eq!(strip_prefix("/b", "/a"), None);
    }
}
