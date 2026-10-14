//! The set of mounted mocks, hot-swapped as the control store changes, and
//! the data-plane dispatcher that routes requests to them by path prefix.

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Router;
use axum::extract::Request;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
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
}

/// Mounted mocks, sorted by prefix length, longest first.
pub struct MockRegistry {
    mounted: ArcSwap<Vec<Mounted>>,
    /// Serialises reloads so concurrent updates cannot drop each other's work.
    reload: Mutex<()>,
}

impl MockRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mounted: ArcSwap::from_pointee(Vec::new()),
            reload: Mutex::new(()),
        })
    }

    /// Rebuilds every mock. A mock that fails to build is logged and skipped.
    pub async fn reload_all(&self, deps: &RuntimeDeps) -> anyhow::Result<()> {
        let _guard = self.reload.lock().await;
        self.reload_all_locked(deps).await
    }

    /// [`Self::reload_all`] for a caller that already holds `self.reload`.
    async fn reload_all_locked(&self, deps: &RuntimeDeps) -> anyhow::Result<()> {
        let mut mocks = deps.store.list_mocks(DEFAULT_WORKSPACE).await?;
        mocks.sort_by(|a, b| a.name.cmp(&b.name));
        let mut next: Vec<Mounted> = Vec::new();
        for mock in mocks {
            let Some(mounted) = build_mounted(deps, &mock).await else {
                continue;
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
        Ok(())
    }

    /// Rebuilds one mock, unmounting it when it was deleted, has no active
    /// contract or fails to build.
    ///
    /// When the mock gives up a prefix it held (it is unmounted or moves to
    /// another prefix), a mock skipped earlier for colliding on that prefix
    /// may now be mountable, so every mock is rebuilt instead.
    pub async fn reload_mock(&self, deps: &RuntimeDeps, name: &str) {
        let _guard = self.reload.lock().await;
        let mounted = match deps.store.get_mock(DEFAULT_WORKSPACE, name).await {
            Ok(Some(mock)) => build_mounted(deps, &mock).await,
            Ok(None) => None,
            Err(error) => {
                tracing::error!(mock = %name, %error, "cannot load mock; leaving it as it was");
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

async fn build_mounted(deps: &RuntimeDeps, mock: &Mock) -> Option<Mounted> {
    let runtime = match build_runtime(deps, mock).await {
        Ok(Some(rt)) => rt,
        Ok(None) => return None,
        Err(error) => {
            tracing::error!(mock = %mock.name, error = %format!("{error:#}"), "cannot build mock; skipped");
            return None;
        }
    };
    match http_rest::try_router(Arc::new(runtime)) {
        Ok(router) => Some(Mounted {
            name: mock.name.clone(),
            prefix: mock.route_prefix.clone(),
            router,
        }),
        Err(error) => {
            tracing::error!(mock = %mock.name, %error, "cannot route mock; skipped");
            None
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
