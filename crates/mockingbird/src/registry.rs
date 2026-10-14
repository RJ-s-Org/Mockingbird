//! The set of mounted mocks, hot-swapped as the control store changes, and
//! the data-plane dispatcher that routes requests to them by path prefix.

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Router;
use axum::extract::Request;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use mb_protocols::StateHealth;
use mb_protocols::http_rest::{self, problem};
use mb_store::{ControlChange, DEFAULT_WORKSPACE, Mock};
use tokio::sync::{Mutex, broadcast};
use tower::ServiceExt;

use crate::runtime::{RuntimeDeps, build_runtime};

/// One mock served by the dispatcher.
#[derive(Clone)]
struct Mounted {
    name: String,
    prefix: String,
    router: Router,
    /// The runtime's state store circuit breaker.
    state_health: Arc<StateHealth>,
}

/// Mounted mocks, sorted by prefix length, longest first.
///
/// A mock whose rebuild fails keeps its previous version mounted and is
/// listed in [`MockRegistry::failed_mocks`] until a rebuild succeeds; it is
/// unmounted only when it is deleted or has no active contract.
pub struct MockRegistry {
    mounted: ArcSwap<Vec<Mounted>>,
    /// Mock name → why its last rebuild failed.
    failed: std::sync::Mutex<BTreeMap<String, String>>,
    /// Serialises reloads so concurrent updates cannot drop each other's work.
    reload: Mutex<()>,
}

impl MockRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mounted: ArcSwap::from_pointee(Vec::new()),
            failed: std::sync::Mutex::new(BTreeMap::new()),
            reload: Mutex::new(()),
        })
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
            if let Some(holder) = next.iter().find(|m| m.prefix == mounted.prefix) {
                tracing::error!(
                    mock = %mounted.name,
                    prefix = %mounted.prefix,
                    holder = %holder.name,
                    "route prefix already mounted by another mock; mock skipped"
                );
                continue;
            }
            next.push(mounted);
        }
        self.publish(next);
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
            .map(|m| m.prefix.clone());
        let released = match (&held, &mounted) {
            (Some(old), Some(new)) => *old != new.prefix,
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
            match next.iter().position(|m| m.prefix == mounted.prefix) {
                // The holder sorts first by name, so it keeps the prefix.
                Some(i) if next[i].name < mounted.name => {
                    tracing::error!(
                        mock = %mounted.name,
                        prefix = %mounted.prefix,
                        holder = %next[i].name,
                        "route prefix already mounted by another mock; mock skipped"
                    );
                }
                Some(i) => {
                    tracing::error!(
                        mock = %next[i].name,
                        prefix = %mounted.prefix,
                        holder = %mounted.name,
                        "route prefix taken by a mock that sorts first; mock unmounted"
                    );
                    next[i] = mounted;
                }
                None => next.push(mounted),
            }
        }
        self.publish(next);
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

    /// `(name, prefix)` of every mounted mock, longest prefix first.
    pub fn mounted(&self) -> Vec<(String, String)> {
        self.mounted
            .load()
            .iter()
            .map(|m| (m.name.clone(), m.prefix.clone()))
            .collect()
    }

    /// `(name, error)` of every mock whose last rebuild failed, sorted by
    /// name. Such a mock serves its previous version, if it had one.
    pub fn failed_mocks(&self) -> Vec<(String, String)> {
        self.failed
            .lock()
            .map(|f| f.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    fn set_failed(&self, name: &str, error: Option<String>) {
        if let Ok(mut failed) = self.failed.lock() {
            match error {
                Some(error) => failed.insert(name.to_string(), error),
                None => failed.remove(name),
            };
        }
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

    fn publish(&self, mut next: Vec<Mounted>) {
        next.sort_by(|a, b| {
            b.prefix
                .len()
                .cmp(&a.prefix.len())
                .then_with(|| a.name.cmp(&b.name))
        });
        self.mounted.store(Arc::new(next));
    }

    async fn dispatch(&self, mut req: Request) -> Response {
        // One snapshot per request: a swap mid-request does not affect it.
        let snapshot = self.mounted.load_full();
        let path = req.uri().path().to_owned();
        let Some((mounted, rest)) = snapshot
            .iter()
            .find_map(|m| strip_prefix(&path, &m.prefix).map(|rest| (m, rest)))
        else {
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
        let router = mounted.router.clone();
        match router.oneshot(req).await {
            Ok(response) => response.into_response(),
            Err(never) => match never {},
        }
    }
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
    match http_rest::try_router(Arc::new(runtime)) {
        Ok(router) => Ok(Some(Mounted {
            name: mock.name.clone(),
            prefix: mock.route_prefix.clone(),
            router,
            state_health,
        })),
        Err(error) => {
            tracing::error!(mock = %mock.name, %error, "cannot route mock; keeping any previous version mounted");
            Err(format!("cannot route the contract: {error}"))
        }
    }
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
