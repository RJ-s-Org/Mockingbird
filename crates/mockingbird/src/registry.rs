//! The set of mounted mocks, hot-swapped as the control store changes, and
//! the data-plane dispatcher that routes requests to them by path prefix.
//!
//! Each mock is mounted by its contract's protocol family: REST, GraphQL
//! and SOAP mocks are dispatched by route prefix on the data plane; gRPC
//! mocks hold no prefix and are published to the shared [`GrpcServer`].

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Router;
use axum::extract::Request;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use futures::StreamExt as _;
use mb_mcm::{Channel, ChannelBinding, Direction, Protocol};
use mb_protocols::events::{ChannelMode, ChannelRuntime, ChannelSetup, EventSetup};
use mb_protocols::grpc::{GrpcError, GrpcMock, GrpcServer};
use mb_protocols::http_rest::{self, problem};
use mb_protocols::{MountPrefix, StateHealth, graphql, soap};
use mb_store::{ControlChange, DEFAULT_WORKSPACE, Mock};
use tokio::sync::{Mutex, broadcast};
use tower::ServiceExt;

use crate::budget::StoreTokenBudget;
use crate::ha::HubEvent;
use crate::runtime::{ProfileUse, RuntimeDeps, ServeMode, build_runtime_tracked};

/// How a mock is named in logs and `/readyz`: `name` in the default
/// workspace, `workspace/name` elsewhere.
pub fn mock_label(workspace: &str, name: &str) -> String {
    if workspace == DEFAULT_WORKSPACE {
        name.to_string()
    } else {
        format!("{workspace}/{name}")
    }
}

/// `(workspace, name)`: identifies a mock across workspaces.
type MockKey = (String, String);

/// Mocks [`MockRegistry::reload_all`] builds at once.
const RELOAD_CONCURRENCY: usize = 8;

/// One mounted mock.
#[derive(Clone)]
struct Mounted {
    workspace: String,
    name: String,
    served: Served,
    /// The runtime's state store circuit breaker.
    state_health: Arc<StateHealth>,
    /// The channels of a mock whose contract has some. Dropping the last
    /// handle (when the mock is replaced or unmounted) stops its tasks.
    events: Option<Arc<ChannelRuntime>>,
    /// The contract's channels, for the startup log.
    channels: Vec<Channel>,
    /// The mock's daily token budget, when it has one.
    budget: Option<Arc<StoreTokenBudget>>,
    /// The LLM profiles its runtime generates with.
    profiles: ProfileUse,
    /// The mock names no LLM profile: it uses the default one.
    uses_default: bool,
}

/// How a mounted mock is reached.
#[derive(Clone)]
enum Served {
    /// By the data-plane dispatcher, below a route prefix, for the hosts
    /// it lists (any host when empty).
    Http {
        prefix: String,
        hosts: Vec<String>,
        /// Applied to the path below `prefix` before the contract sees it.
        rewrite: mb_store::PathRewrite,
        /// The path a request for the mount point itself is sent to: a
        /// GraphQL mock at `/graphql` answers `POST /graphql`, as well as
        /// `/graphql/graphql`.
        root: Option<String>,
        router: Router,
    },
    /// By the shared gRPC server; the mock's route prefix is ignored.
    Grpc(Arc<GrpcMock>),
}

impl Mounted {
    /// Whether a change of profile `name` in `workspace` may change this
    /// mock: it generates with a profile of that name, loaded from its own
    /// workspace or, for a default or policy-picked profile, `default`.
    fn uses_profile(&self, workspace: &str, name: &str) -> bool {
        (workspace == self.workspace || workspace == DEFAULT_WORKSPACE) && self.profiles.uses(name)
    }

    fn is(&self, workspace: &str, name: &str) -> bool {
        self.workspace == workspace && self.name == name
    }

    fn key(&self) -> (&str, &str) {
        (&self.workspace, &self.name)
    }

    fn label(&self) -> String {
        mock_label(&self.workspace, &self.name)
    }

    /// The data-plane route prefix; `None` for a gRPC mock.
    fn prefix(&self) -> Option<&str> {
        match &self.served {
            Served::Http { prefix, .. } => Some(prefix),
            Served::Grpc(_) => None,
        }
    }

    /// The hosts the mock answers for; empty for any host or a gRPC mock.
    fn hosts(&self) -> &[String] {
        match &self.served {
            Served::Http { hosts, .. } => hosts,
            Served::Grpc(_) => &[],
        }
    }

    /// Whether this mock and `other` claim the same data-plane route.
    fn collides(&self, other: &Mounted) -> bool {
        match (self.prefix(), other.prefix()) {
            (Some(a), Some(b)) => mb_store::routes_collide(a, self.hosts(), b, other.hosts()),
            _ => false,
        }
    }
}

/// `raw` (a `Host` value or URI host) lowercased, without a port, IPv6
/// brackets or one trailing dot; `None` when nothing is left.
pub fn normalize_host(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let host = if let Some(rest) = raw.strip_prefix('[') {
        rest.split_once(']').map_or(rest, |(h, _)| h)
    } else if raw.matches(':').count() == 1 {
        raw.split_once(':').map_or(raw, |(h, _)| h)
    } else {
        raw
    };
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// The host a request is addressed to: the URI authority (absolute-form or
/// HTTP/2 `:authority`), else the `Host` header; see [`normalize_host`].
pub fn request_host(req: &Request) -> Option<String> {
    req.uri().host().and_then(normalize_host).or_else(|| {
        req.headers()
            .get(axum::http::header::HOST)
            .and_then(|h| h.to_str().ok())
            .and_then(normalize_host)
    })
}

/// Mounted mocks from every workspace, sorted by prefix length, longest
/// first. Route prefixes and gRPC services are unique across workspaces.
///
/// A mock whose rebuild fails keeps its previous version mounted and is
/// listed in [`MockRegistry::failed_mocks`] until a rebuild succeeds; it is
/// unmounted only when it is deleted or has no active contract.
///
/// After every change the gRPC mocks are published to [`MockRegistry::grpc`].
/// A gRPC service has one holder across every workspace: a mock already
/// published keeps its services, and a mock that would serve a service
/// another mock holds is skipped (and listed as failed in `/readyz`) until
/// that service is free. Among mocks not yet published, the one sorting
/// first by (workspace, name) wins.
pub struct MockRegistry {
    mounted: ArcSwap<Vec<Mounted>>,
    /// `(workspace, name)` → why the mock's last rebuild failed.
    failed: std::sync::Mutex<BTreeMap<MockKey, String>>,
    /// `(workspace, name)` → the service collision that kept it off the
    /// gRPC server.
    grpc_skipped: std::sync::Mutex<BTreeMap<MockKey, String>>,
    /// The gRPC mocks of the last publish: they keep their services.
    grpc_published: std::sync::Mutex<BTreeSet<MockKey>>,
    /// The h2c server every gRPC mock is published to.
    grpc: Arc<GrpcServer>,
    /// `(workspace, name)` → the hosts of every stored mock that lists
    /// some, mounted or not: a host stays claimed while its mock fails to
    /// build, so the proxy fails closed instead of passing it through.
    configured_hosts: std::sync::Mutex<BTreeMap<MockKey, Vec<String>>>,
    /// Every host of the mounted mocks and of `configured_hosts`, rebuilt
    /// whenever either changes, so the proxy's per-request
    /// [`Self::claims_host`] is one lookup.
    claimed: ArcSwap<HashSet<String>>,
    /// Serialises reloads so concurrent updates cannot drop each other's work.
    reload: Mutex<()>,
}

impl std::fmt::Debug for MockRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockRegistry")
            .field("mounted", &self.mounted.load().len())
            .finish_non_exhaustive()
    }
}

impl MockRegistry {
    /// An empty registry with its own gRPC server.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mounted: ArcSwap::from_pointee(Vec::new()),
            failed: std::sync::Mutex::new(BTreeMap::new()),
            grpc_skipped: std::sync::Mutex::new(BTreeMap::new()),
            grpc_published: std::sync::Mutex::new(BTreeSet::new()),
            grpc: GrpcServer::new(),
            configured_hosts: std::sync::Mutex::new(BTreeMap::new()),
            claimed: ArcSwap::from_pointee(HashSet::new()),
            reload: Mutex::new(()),
        })
    }

    /// The shared gRPC server the gRPC mocks are published to.
    pub fn grpc(&self) -> Arc<GrpcServer> {
        Arc::clone(&self.grpc)
    }

    /// Rebuilds every mock of every workspace. A mock that fails to build is logged, recorded
    /// as failed and keeps its previous version mounted, if it had one.
    ///
    /// # Errors
    ///
    /// Fails when the mocks cannot be listed from the store; a mock that fails
    /// to build is recorded, not returned.
    pub async fn reload_all(&self, deps: &RuntimeDeps) -> anyhow::Result<()> {
        let _guard = self.reload.lock().await;
        self.reload_all_locked(deps).await
    }

    /// [`Self::reload_all`] for a caller that already holds `self.reload`.
    async fn reload_all_locked(&self, deps: &RuntimeDeps) -> anyhow::Result<()> {
        let mut mocks = deps.store.list_all_mocks().await?;
        mocks.sort_by(|a, b| (&a.workspace, &a.name).cmp(&(&b.workspace, &b.name)));
        if let Ok(mut configured) = self.configured_hosts.lock() {
            *configured = mocks
                .iter()
                .filter(|m| !m.hosts.is_empty())
                .map(|m| ((m.workspace.clone(), m.name.clone()), m.hosts.clone()))
                .collect();
        }
        self.refresh_claimed();
        let current = self.mounted.load_full();
        let mut failed = BTreeMap::new();
        let mut next: Vec<Mounted> = Vec::new();
        // Built concurrently but taken in (workspace, name) order, so which
        // mock keeps a contested prefix does not depend on build times.
        let mut builds = futures::stream::iter(mocks)
            .map(|mock| {
                let key: MockKey = (mock.workspace.clone(), mock.name.clone());
                let deps = deps.clone();
                let task = tokio::spawn(async move { build_mounted(&deps, &mock).await });
                async move { (key, task.await) }
            })
            .buffered(RELOAD_CONCURRENCY);
        while let Some((key, joined)) = builds.next().await {
            let built = joined.unwrap_or_else(|error| {
                let error = format!("building the mock failed: {error}");
                tracing::error!(mock = %mock_label(&key.0, &key.1), %error, "cannot build mock; keeping any previous version mounted");
                Err(error)
            });
            let mounted = match built {
                Ok(Some(mounted)) => mounted,
                Ok(None) => continue,
                Err(error) => {
                    let previous = current.iter().find(|m| m.is(&key.0, &key.1));
                    failed.insert(key, error);
                    match previous {
                        Some(previous) => previous.clone(),
                        None => continue,
                    }
                }
            };
            if let Some(prefix) = mounted.prefix()
                && let Some(holder) = next.iter().find(|m| m.collides(&mounted))
            {
                tracing::error!(
                    mock = %mounted.label(),
                    prefix = %prefix,
                    holder = %holder.label(),
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
    pub async fn reload_mock(&self, deps: &RuntimeDeps, workspace: &str, name: &str) {
        let _guard = self.reload.lock().await;
        if let Some(built) = self.fetch_and_build(deps, workspace, name).await {
            self.apply_built(deps, workspace, name, built).await;
        }
    }

    /// Rebuilds the mocks a change of LLM profile `name` in `workspace`
    /// affects: those whose runtime generates with it (the profile they
    /// name, or the default one, or one in that chain's fallbacks, or one
    /// the provider policy picked for a call), and those whose last build
    /// failed. The rest keep their runtimes.
    pub async fn reload_profile(&self, deps: &RuntimeDeps, workspace: &str, name: &str) {
        let _guard = self.reload.lock().await;
        let profiles = BTreeSet::from([(workspace.to_owned(), name.to_owned())]);
        self.reload_keys_locked(deps, self.profile_dependents(&profiles))
            .await;
    }

    /// Rebuilds the mocks naming no LLM profile, and those whose last build
    /// failed: what a change of `llm.default_profile` affects.
    pub async fn reload_default_profile_mocks(&self, deps: &RuntimeDeps) {
        let _guard = self.reload.lock().await;
        let keys = self.dependents(|m| m.uses_default, |_| true);
        self.reload_keys_locked(deps, keys).await;
    }

    /// The mocks a change of any of `profiles` (`(workspace, name)`)
    /// affects (see [`Self::reload_profile`]).
    fn profile_dependents(&self, profiles: &BTreeSet<MockKey>) -> BTreeSet<MockKey> {
        self.dependents(
            |m| profiles.iter().any(|(ws, name)| m.uses_profile(ws, name)),
            |key| {
                profiles
                    .iter()
                    .any(|(ws, _)| *ws == key.0 || ws == DEFAULT_WORKSPACE)
            },
        )
    }

    /// The mounted mocks matching `mounted`, with the failed mocks matching
    /// `failed`.
    fn dependents(
        &self,
        mounted: impl Fn(&Mounted) -> bool,
        failed: impl Fn(&MockKey) -> bool,
    ) -> BTreeSet<MockKey> {
        let mut keys: BTreeSet<MockKey> = self
            .mounted
            .load()
            .iter()
            .filter(|m| mounted(m))
            .map(|m| (m.workspace.clone(), m.name.clone()))
            .collect();
        if let Ok(slot) = self.failed.lock() {
            keys.extend(slot.keys().filter(|k| failed(k)).cloned());
        }
        keys
    }

    /// [`Self::reload_mock`] for each of `keys`, built concurrently and
    /// applied in `(workspace, name)` order, for a caller holding
    /// `self.reload`. Stops once applying one reloaded every mock.
    async fn reload_keys_locked(&self, deps: &RuntimeDeps, keys: BTreeSet<MockKey>) {
        let builds: Vec<_> = futures::stream::iter(keys)
            .map(|(workspace, name)| async move {
                let built = self.fetch_and_build(deps, &workspace, &name).await;
                (workspace, name, built)
            })
            .buffered(RELOAD_CONCURRENCY)
            .collect()
            .await;
        for (workspace, name, built) in builds {
            if let Some(built) = built
                && self.apply_built(deps, &workspace, &name, built).await
            {
                return;
            }
        }
    }

    /// Loads mock `name` and builds it: `Ok(None)` when it was deleted or
    /// has no active contract; `None` (logged) when it cannot be loaded.
    async fn fetch_and_build(
        &self,
        deps: &RuntimeDeps,
        workspace: &str,
        name: &str,
    ) -> Option<Result<Option<Mounted>, String>> {
        match deps.store.get_mock(workspace, name).await {
            Ok(Some(mock)) => {
                self.set_configured_hosts(workspace, name, &mock.hosts);
                Some(build_mounted(deps, &mock).await)
            }
            Ok(None) => {
                self.set_configured_hosts(workspace, name, &[]);
                Some(Ok(None))
            }
            Err(error) => {
                let label = mock_label(workspace, name);
                tracing::error!(mock = %label, %error, "cannot load mock; leaving it as it was");
                None
            }
        }
    }

    /// Mounts what [`Self::fetch_and_build`] built for mock `name`, for a
    /// caller holding `self.reload`. Returns whether every mock was
    /// reloaded instead (the mock released a prefix).
    async fn apply_built(
        &self,
        deps: &RuntimeDeps,
        workspace: &str,
        name: &str,
        built: Result<Option<Mounted>, String>,
    ) -> bool {
        let label = mock_label(workspace, name);
        let mounted = match built {
            Ok(mounted) => {
                self.set_failed(workspace, name, None);
                mounted
            }
            Err(error) => {
                // Whatever is mounted (the previous version, or nothing)
                // stays as it is.
                self.set_failed(workspace, name, Some(error));
                return false;
            }
        };

        let held = self
            .mounted
            .load()
            .iter()
            .find(|m| m.is(workspace, name))
            .and_then(|m| m.prefix().map(|p| (p.to_owned(), m.hosts().to_vec())));
        let released = match (&held, &mounted) {
            (Some((old, old_hosts)), Some(new)) => {
                new.prefix() != Some(old.as_str()) || new.hosts() != old_hosts.as_slice()
            }
            (Some(_), None) => true,
            (None, _) => false,
        };
        if released {
            match self.reload_all_locked(deps).await {
                Ok(()) => return true,
                Err(error) => {
                    tracing::error!(
                        mock = %label,
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
            .filter(|m| !m.is(workspace, name))
            .cloned()
            .collect();
        if let Some(mounted) = mounted {
            let prefix = mounted.prefix().unwrap_or_default().to_owned();
            let holders: Vec<usize> = (0..next.len())
                .filter(|&i| next[i].collides(&mounted))
                .collect();
            // A holder that sorts first by (workspace, name) keeps the route.
            if let Some(&i) = holders.iter().find(|&&i| next[i].key() < mounted.key()) {
                tracing::error!(
                    mock = %mounted.label(),
                    prefix = %prefix,
                    holder = %next[i].label(),
                    "route prefix already mounted by another mock; mock skipped"
                );
            } else {
                for &i in holders.iter().rev() {
                    let displaced = next.remove(i);
                    tracing::error!(
                        mock = %displaced.label(),
                        prefix = %prefix,
                        holder = %mounted.label(),
                        "route prefix taken by a mock that sorts first; mock unmounted"
                    );
                }
                next.push(mounted);
            }
        }
        self.publish(next).await;
        false
    }

    /// Applies control-plane changes as the HA hub delivers them (see
    /// [`Ha::changes`](crate::ha::Ha::changes)): a mock change (in any
    /// workspace, from any node) reloads that mock; a profile change
    /// reloads the mocks using that profile (see [`Self::reload_profile`]);
    /// a workspace change, a resync request, or missed changes, reload all.
    ///
    /// Changes queued while a reload runs are applied together: each mock
    /// is rebuilt once however many changes name it (one store write
    /// publishes one change, so a reconcile publishes several per mock),
    /// and a reload of all covers every mock change with it. A rebuild
    /// reads the latest stored state, so nothing is lost.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    pub fn spawn_watcher(
        self: &Arc<Self>,
        deps: RuntimeDeps,
        mut changes: broadcast::Receiver<HubEvent>,
    ) -> tokio::task::JoinHandle<()> {
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let mut batch = ChangeBatch::default();
                match changes.recv().await {
                    Ok(event) => batch.add(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => batch.all = true,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
                batch.drain(&mut changes);
                if batch.all {
                    if let Err(error) = registry.reload_all(&deps).await {
                        tracing::error!(%error, "cannot reload mocks");
                    }
                } else {
                    registry
                        .reload_batch(&deps, batch.mocks, &batch.profiles)
                        .await;
                }
                if batch.closed {
                    break;
                }
            }
        })
    }

    /// Rebuilds `mocks` and the mocks using any of `profiles`, each once.
    async fn reload_batch(
        &self,
        deps: &RuntimeDeps,
        mut mocks: BTreeSet<MockKey>,
        profiles: &BTreeSet<MockKey>,
    ) {
        let _guard = self.reload.lock().await;
        if !profiles.is_empty() {
            mocks.extend(self.profile_dependents(profiles));
        }
        self.reload_keys_locked(deps, mocks).await;
    }

    /// The data-plane router: forwards each request to the mock with the
    /// longest matching prefix among those answering for the request's
    /// host (see [`request_host`]), with that prefix stripped. At equal
    /// prefix length a mock listing the host wins over one with no hosts.
    pub fn router(self: &Arc<Self>) -> Router {
        let registry = Arc::clone(self);
        Router::new().fallback(move |req: Request| {
            let registry = Arc::clone(&registry);
            async move { registry.dispatch(req).await }
        })
    }

    /// `(workspace, name, prefix)` of every mock mounted on the data plane,
    /// longest prefix first. gRPC mocks are listed by
    /// [`GrpcServer::services`].
    pub fn mounted(&self) -> Vec<(String, String, String)> {
        self.mounted
            .load()
            .iter()
            .filter_map(|m| {
                m.prefix()
                    .map(|p| (m.workspace.clone(), m.name.clone(), p.to_owned()))
            })
            .collect()
    }

    /// Whether a mock lists `host` (already normalized) in its hosts,
    /// whether or not it is mounted (see [`Self::claimed_hosts`]).
    pub fn claims_host(&self, host: &str) -> bool {
        self.claimed.load().contains(host)
    }

    /// Every host a mock lists, sorted and deduplicated: the mounted mocks'
    /// and those of stored mocks that failed to build, have no active
    /// contract or lost a route collision, so their hosts fail closed.
    pub fn claimed_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self.claimed.load().iter().cloned().collect();
        hosts.sort();
        hosts
    }

    /// Recomputes [`Self::claimed`] from the mounted mocks and the
    /// configured hosts; called after either changes.
    fn refresh_claimed(&self) {
        let mut hosts: HashSet<String> = self
            .mounted
            .load()
            .iter()
            .flat_map(|m| m.hosts().iter().cloned())
            .collect();
        if let Ok(configured) = self.configured_hosts.lock() {
            hosts.extend(configured.values().flatten().cloned());
        }
        self.claimed.store(Arc::new(hosts));
    }

    fn set_configured_hosts(&self, workspace: &str, name: &str, hosts: &[String]) {
        if let Ok(mut configured) = self.configured_hosts.lock() {
            let key = (workspace.to_string(), name.to_string());
            if hosts.is_empty() {
                configured.remove(&key);
            } else {
                configured.insert(key, hosts.to_vec());
            }
        }
        self.refresh_claimed();
    }

    /// The 503 for a request to `host` that a stored mock lists but that no
    /// mounted mock serves; `None` when no such mock exists.
    fn unmounted_host(&self, host: &str, mounted: &[Mounted]) -> Option<Response> {
        let configured = self.configured_hosts.lock().ok()?;
        let (ws, name) = configured
            .iter()
            .find(|((ws, name), hosts)| {
                hosts.iter().any(|h| h == host) && !mounted.iter().any(|m| m.is(ws, name))
            })
            .map(|(key, _)| key.clone())?;
        let label = mock_label(&ws, &name);
        let why = self
            .failed
            .lock()
            .ok()
            .and_then(|f| f.get(&(ws, name)).cloned())
            .unwrap_or_else(|| "it has no active contract, or another mock holds its route".into());
        Some(problem(
            503,
            "Mock not mounted",
            format!("mock {label} answers for host {host} but is not mounted: {why}"),
        ))
    }

    /// `(workspace, name)` of every mounted mock, gRPC mocks included.
    pub fn mounted_mocks(&self) -> Vec<(String, String)> {
        self.mounted
            .load()
            .iter()
            .map(|m| (m.workspace.clone(), m.name.clone()))
            .collect()
    }

    /// `(label, error)` of every mock whose last rebuild failed, or that is
    /// kept off the gRPC server by a service collision, sorted by workspace
    /// and name; see [`mock_label`]. A mock whose rebuild failed serves its
    /// previous version, if it had one.
    pub fn failed_mocks(&self) -> Vec<(String, String)> {
        let mut all = self.failed.lock().map(|f| f.clone()).unwrap_or_default();
        if let Ok(skipped) = self.grpc_skipped.lock() {
            for (key, error) in skipped.iter() {
                all.entry(key.clone()).or_insert_with(|| error.clone());
            }
        }
        all.into_iter()
            .map(|((ws, name), error)| (mock_label(&ws, &name), error))
            .collect()
    }

    /// One `/readyz` reason per mock in [`Self::failed_mocks`], sorted:
    /// `mock <label>: stale, serving the previous version: <error>` when
    /// its rebuild failed and its previous version is still mounted, else
    /// `mock <label>: failed: <error>` (gRPC service collisions included).
    pub fn mock_issues(&self) -> Vec<String> {
        let failed = self.failed.lock().map(|f| f.clone()).unwrap_or_default();
        let mounted = self.mounted.load();
        let mut issues: Vec<String> = failed
            .iter()
            .map(|((ws, name), error)| {
                let label = mock_label(ws, name);
                if mounted.iter().any(|m| m.is(ws, name)) {
                    format!("mock {label}: stale, serving the previous version: {error}")
                } else {
                    format!("mock {label}: failed: {error}")
                }
            })
            .collect();
        if let Ok(skipped) = self.grpc_skipped.lock() {
            for ((ws, name), error) in skipped.iter() {
                if !failed.contains_key(&(ws.clone(), name.clone())) {
                    issues.push(format!("mock {}: failed: {error}", mock_label(ws, name)));
                }
            }
        }
        issues.sort();
        issues
    }

    fn set_failed(&self, workspace: &str, name: &str, error: Option<String>) {
        if let Ok(mut failed) = self.failed.lock() {
            let key = (workspace.to_string(), name.to_string());
            match error {
                Some(error) => failed.insert(key, error),
                None => failed.remove(&key),
            };
        }
    }

    /// The channel runtime of mounted mock `name` in `workspace`; `None`
    /// when it is not mounted or its contract has no channels.
    pub fn channel_runtime(&self, workspace: &str, name: &str) -> Option<Arc<ChannelRuntime>> {
        self.mounted
            .load()
            .iter()
            .find(|m| m.is(workspace, name))
            .and_then(|m| m.events.clone())
    }

    /// `(label, reason)` for every reason a mounted mock's channels are not
    /// all working (see [`ChannelRuntime::health`]), sorted; see
    /// [`mock_label`].
    pub fn event_issues(&self) -> Vec<(String, String)> {
        let mut issues: Vec<(String, String)> = self
            .mounted
            .load()
            .iter()
            .filter_map(|m| m.events.as_ref().map(|events| (m.label(), events.health())))
            .flat_map(|(label, reasons)| reasons.into_iter().map(move |r| (label.clone(), r)))
            .collect();
        issues.sort();
        issues
    }

    /// `(mock label, protocol, channel id, direction, address)` of every channel
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
                    mock: m.label(),
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

    /// Labels (see [`mock_label`]) of mounted mocks currently serving
    /// statelessly because their state store calls are failing, sorted.
    pub fn degraded_mocks(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .mounted
            .load()
            .iter()
            .filter(|m| m.state_health.is_degraded())
            .map(Mounted::label)
            .collect();
        labels.sort();
        labels
    }

    /// One `/readyz` reason per mounted mock whose daily token budget is
    /// exhausted (it is served by the faker until UTC midnight), sorted:
    /// `mock <label>: daily token budget exhausted`.
    pub fn budget_issues(&self) -> Vec<String> {
        let mut issues: Vec<String> = self
            .mounted
            .load()
            .iter()
            .filter(|m| m.budget.as_ref().is_some_and(|b| b.is_exhausted()))
            .map(|m| format!("mock {}: daily token budget exhausted", m.label()))
            .collect();
        issues.sort();
        issues
    }

    /// Installs `next` as the mounted set and publishes its gRPC mocks.
    /// Callers hold `self.reload`, so publishes do not interleave.
    async fn publish(&self, mut next: Vec<Mounted>) {
        let prefix_len = |m: &Mounted| m.prefix().map_or(0, str::len);
        // Longest prefix first; at equal length host-specific mocks first.
        next.sort_by(|a, b| {
            prefix_len(b)
                .cmp(&prefix_len(a))
                .then_with(|| a.hosts().is_empty().cmp(&b.hosts().is_empty()))
                .then_with(|| a.key().cmp(&b.key()))
        });
        let grpc: Vec<(MockKey, Arc<GrpcMock>)> = next
            .iter()
            .filter_map(|m| match &m.served {
                Served::Grpc(mock) => {
                    Some(((m.workspace.clone(), m.name.clone()), Arc::clone(mock)))
                }
                Served::Http { .. } => None,
            })
            .collect();
        self.mounted.store(Arc::new(next));
        self.refresh_claimed();
        self.publish_grpc(grpc).await;
    }

    /// Publishes the gRPC mocks of every workspace: the mocks of the last
    /// publish first, then the others, each group in (workspace, name)
    /// order. A mock serving a service an earlier one already serves (in
    /// any workspace; the gRPC listener is shared) is skipped and recorded,
    /// so a service stays with the mock that held it.
    async fn publish_grpc(&self, mut mocks: Vec<(MockKey, Arc<GrpcMock>)>) {
        let previous = self
            .grpc_published
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default();
        mocks.sort_by(|a, b| {
            (!previous.contains(&a.0), &a.0).cmp(&(!previous.contains(&b.0), &b.0))
        });
        let mut holders: BTreeMap<String, String> = BTreeMap::new();
        let mut skipped = BTreeMap::new();
        let mut published = Vec::new();
        for (key, mock) in mocks {
            let label = mock_label(&key.0, &key.1);
            let services = mock.services();
            let collision = services
                .iter()
                .find_map(|s| holders.get(s).map(|holder| (s.clone(), holder.clone())));
            if let Some((service, holder)) = collision {
                let error = GrpcError::ServiceCollision { service, holder }.to_string();
                tracing::error!(mock = %label, %error, "gRPC service already served by another mock; mock skipped");
                skipped.insert(key, error);
                continue;
            }
            for service in services {
                holders.insert(service, label.clone());
            }
            published.push((key, mock));
        }
        let keys: BTreeSet<MockKey> = published.iter().map(|(k, _)| k.clone()).collect();
        let mocks = published.into_iter().map(|(_, m)| m).collect();
        match self.grpc.publish(mocks).await {
            Ok(()) => {
                if let Ok(mut slot) = self.grpc_published.lock() {
                    *slot = keys;
                }
            }
            Err(error) => {
                tracing::error!(%error, "cannot publish the gRPC mocks; keeping the previous set");
            }
        }
        if let Ok(mut slot) = self.grpc_skipped.lock() {
            *slot = skipped;
        }
    }

    async fn dispatch(&self, mut req: Request) -> Response {
        // One snapshot per request: a swap mid-request does not affect it.
        let snapshot = self.mounted.load_full();
        let path = req.uri().path().to_owned();
        let host = request_host(&req);
        let Some((prefix, router, rest)) = snapshot.iter().find_map(|m| match &m.served {
            Served::Http {
                prefix,
                hosts,
                rewrite,
                root,
                router,
            } if hosts.is_empty() || host.as_ref().is_some_and(|h| hosts.contains(h)) => {
                strip_prefix(&path, prefix).map(|rest| match root {
                    Some(root) if prefix != "/" && rest == "/" => (prefix, router, root.to_owned()),
                    _ => (prefix, router, rewrite.apply(rest)),
                })
            }
            _ => None,
        }) else {
            if let Some(resp) = host
                .as_deref()
                .and_then(|h| self.unmounted_host(h, &snapshot))
            {
                return resp;
            }
            let detail = match &host {
                Some(host) => format!("no mock is mounted at a prefix of {path} for host {host}"),
                None => format!("no mock is mounted at a prefix of {path}"),
            };
            return problem(404, "No mock mounted at this path", detail);
        };
        match rewrite_path(req.uri(), &rest) {
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

/// The hub events the watcher applies in one turn, deduplicated.
#[derive(Debug, Default, PartialEq)]
struct ChangeBatch {
    /// Reload every mock: a workspace change, a resync request, or missed
    /// changes.
    all: bool,
    /// The mocks to reload when not `all`.
    mocks: BTreeSet<MockKey>,
    /// The changed profiles, `(workspace, name)`, whose mocks to reload
    /// when not `all`.
    profiles: BTreeSet<(String, String)>,
    /// The hub closed: stop once the batch is applied.
    closed: bool,
}

impl ChangeBatch {
    fn add(&mut self, event: HubEvent) {
        match event {
            HubEvent::Change(ControlChange::Mock { workspace, name }) => {
                self.mocks.insert((workspace, name));
            }
            HubEvent::Change(ControlChange::Profile { workspace, name }) => {
                self.profiles.insert((workspace, name));
            }
            HubEvent::Change(ControlChange::Workspace { .. }) | HubEvent::Resync => {
                self.all = true;
            }
        }
    }

    /// Adds every event already queued on `changes`, without waiting.
    fn drain(&mut self, changes: &mut broadcast::Receiver<HubEvent>) {
        use broadcast::error::TryRecvError;
        loop {
            match changes.try_recv() {
                Ok(event) => self.add(event),
                Err(TryRecvError::Lagged(_)) => self.all = true,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Closed) => {
                    self.closed = true;
                    return;
                }
            }
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
    let label = mock_label(&mock.workspace, &mock.name);
    let (runtime, budget, profiles) = match build_runtime_tracked(deps, mock).await {
        Ok(Some(built)) => built,
        Ok(None) => return Ok(None),
        Err(error) => {
            let error = format!("{error:#}");
            tracing::error!(mock = %label, %error, "cannot build mock; keeping any previous version mounted");
            return Err(error);
        }
    };
    let state_health = Arc::clone(&runtime.state_health);
    let protocol = runtime.bundle().protocol();
    let channels = runtime.bundle().channels.clone();
    let runtime = Arc::new(runtime);
    let events = if channels.is_empty() {
        None
    } else {
        let setup = event_setup(deps, mock, &channels)
            .await
            .map_err(|error| {
                let error = format!("{error:#}");
                tracing::error!(mock = %label, %error, "cannot load the event settings; keeping any previous version mounted");
                error
            })?;
        Some(ChannelRuntime::start(
            Arc::clone(&runtime),
            deps.events.clone(),
            setup,
        ))
    };
    let ws = events.as_ref().and_then(|events| events.ws_router());
    let root = (protocol == Protocol::Graphql).then(|| runtime.protocols.graphql_path.clone());
    let http = |router: Router| Served::Http {
        prefix: mock.route_prefix.clone(),
        hosts: mock.hosts.clone(),
        rewrite: mock.behavior.path_rewrite.clone(),
        root: root.clone(),
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
            workspace: mock.workspace.clone(),
            name: mock.name.clone(),
            served,
            state_health,
            events,
            channels,
            budget,
            profiles,
            uses_default: mock.llm_profile.is_none(),
        })),
        Err(error) => {
            tracing::error!(mock = %label, %error, "cannot route mock; keeping any previous version mounted");
            Err(format!("cannot route the contract: {error}"))
        }
    }
}

/// The stored event settings of `mock`: its broker and, by channel id,
/// each stored channel's `enabled` and triggers (the contract's when the
/// stored row keeps them), in the channel mode `deps.mode` calls for.
async fn event_setup(
    deps: &RuntimeDeps,
    mock: &Mock,
    channels: &[Channel],
) -> anyhow::Result<EventSetup> {
    use anyhow::Context as _;
    let store = deps.store.as_ref();
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
        mode: match deps.mode {
            ServeMode::Full => ChannelMode::Full,
            ServeMode::ControlOnly => ChannelMode::PublishOnly,
        },
    })
}

#[cfg(test)]
mod tests {
    use mb_store::ControlChange;
    use tokio::sync::broadcast;

    use super::{ChangeBatch, HubEvent, MockRegistry, normalize_host, strip_prefix};

    #[test]
    fn hosts_are_normalized() {
        assert_eq!(
            normalize_host("API.Example.TEST:443").as_deref(),
            Some("api.example.test")
        );
        assert_eq!(
            normalize_host("api.example.test.").as_deref(),
            Some("api.example.test")
        );
        assert_eq!(normalize_host("[::1]:8080").as_deref(), Some("::1"));
        assert_eq!(normalize_host("127.0.0.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(normalize_host(""), None);
    }

    #[test]
    fn prefixes_match_at_segment_boundaries() {
        assert_eq!(strip_prefix("/a/b", "/"), Some("/a/b"));
        assert_eq!(strip_prefix("/a", "/a"), Some("/"));
        assert_eq!(strip_prefix("/a/", "/a"), Some("/"));
        assert_eq!(strip_prefix("/a/b", "/a"), Some("/b"));
        assert_eq!(strip_prefix("/ab", "/a"), None);
        assert_eq!(strip_prefix("/b", "/a"), None);
    }

    fn mock(name: &str) -> HubEvent {
        HubEvent::Change(ControlChange::Mock {
            workspace: "default".into(),
            name: name.into(),
        })
    }

    #[test]
    fn queued_changes_are_batched_per_mock() {
        let (tx, mut rx) = broadcast::channel(64);
        for _ in 0..20 {
            tx.send(mock("a")).unwrap();
        }
        tx.send(mock("b")).unwrap();
        let mut batch = ChangeBatch::default();
        batch.drain(&mut rx);
        let keys: Vec<(String, String)> = batch.mocks.iter().cloned().collect();
        assert_eq!(
            keys,
            [
                ("default".to_owned(), "a".to_owned()),
                ("default".to_owned(), "b".to_owned())
            ]
        );
        assert!(!batch.all && !batch.closed);

        tx.send(mock("c")).unwrap();
        tx.send(HubEvent::Change(ControlChange::Profile {
            workspace: "default".into(),
            name: "p".into(),
        }))
        .unwrap();
        let mut batch = ChangeBatch::default();
        batch.drain(&mut rx);
        assert!(!batch.all);
        assert_eq!(batch.mocks.len(), 1);
        assert_eq!(
            batch.profiles.into_iter().collect::<Vec<_>>(),
            [("default".to_owned(), "p".to_owned())]
        );

        tx.send(HubEvent::Change(ControlChange::Workspace {
            name: "w".into(),
        }))
        .unwrap();
        let mut batch = ChangeBatch::default();
        batch.drain(&mut rx);
        assert!(batch.all);

        drop(tx);
        let mut batch = ChangeBatch::default();
        batch.drain(&mut rx);
        assert_eq!(
            batch,
            ChangeBatch {
                closed: true,
                ..ChangeBatch::default()
            }
        );
    }

    #[test]
    fn missed_changes_reload_all() {
        let (tx, mut rx) = broadcast::channel(2);
        for _ in 0..5 {
            tx.send(mock("a")).unwrap();
        }
        let mut batch = ChangeBatch::default();
        batch.drain(&mut rx);
        assert!(batch.all);
    }

    #[test]
    fn configured_hosts_are_claimed_as_soon_as_they_are_set() {
        let registry = MockRegistry::new();
        assert!(!registry.claims_host("api.test"));
        registry.set_configured_hosts("default", "m", &["api.test".into(), "b.test".into()]);
        assert!(registry.claims_host("api.test"));
        assert_eq!(registry.claimed_hosts(), ["api.test", "b.test"]);
        registry.set_configured_hosts("default", "m", &[]);
        assert!(!registry.claims_host("api.test"));
        assert!(registry.claimed_hosts().is_empty());
    }

    /// MOCKINGBIRD-PERF-2 benchmark: `claims_host` with `h` configured
    /// hosts (10 per mock).
    #[test]
    #[ignore = "benchmark"]
    fn bench_claims_host() {
        for h in [1_000usize, 10_000] {
            let registry = super::MockRegistry::new();
            for m in 0..h / 10 {
                let hosts: Vec<String> = (0..10).map(|i| format!("h{m}-{i}.test")).collect();
                registry.set_configured_hosts("default", &format!("m{m}"), &hosts);
            }
            let lookups = 1_000;
            let start = std::time::Instant::now();
            let mut hits = 0;
            for i in 0..lookups {
                hits += usize::from(registry.claims_host(&format!("h{}-0.test", i % (h / 10))));
            }
            assert_eq!(hits, lookups);
            println!(
                "PERF-2 claims_host with {h} hosts: {:?} per lookup",
                start.elapsed() / u32::try_from(lookups).unwrap()
            );
        }
    }
}
