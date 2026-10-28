//! `--watch`: re-reads the config file and reconciles the store whenever
//! the file or one of its contracts changes.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use mb_state::StateStore;
use mb_store::ControlStore;
use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;

use crate::file_config::{self, FileConfig};
use crate::reconcile::reconcile;

/// How long the file system must be quiet before a reload.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// Prepares a freshly loaded config before it is reconciled (e.g. applies
/// environment variables and flags).
pub type Prepare = Arc<dyn Fn(&mut FileConfig) -> anyhow::Result<()> + Send + Sync>;

/// Runs after a reload has been reconciled into the store, with the config
/// that was applied, to update the settings kept outside the store (the
/// default LLM profile, request-log settings). Reloads are serialised: the
/// next one waits for the returned future.
pub type Applied =
    Arc<dyn Fn(FileConfig) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The outcome of the last config reload, shared with `/readyz`: the
/// error of the last reload when it failed, cleared by the next good one.
#[derive(Clone, Default)]
pub struct ReloadStatus(Arc<Mutex<Option<String>>>);

impl ReloadStatus {
    /// Records a failed reload.
    pub fn failed(&self, error: String) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(error);
        }
    }

    /// Records a successful reload.
    pub fn succeeded(&self) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = None;
        }
    }

    /// Why the last reload failed; `None` when it succeeded (or none ran).
    pub fn last_error(&self) -> Option<String> {
        self.0.lock().ok().and_then(|slot| slot.clone())
    }
}

/// Keeps the watch running; dropping it stops watching.
pub struct WatchGuard {
    _watcher: Arc<Mutex<Watcher>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Debounced = Debouncer<notify::RecommendedWatcher, RecommendedCache>;

/// The debouncer with the directories it watches and the files of interest.
struct Watcher {
    debouncer: Debounced,
    dirs: HashSet<PathBuf>,
    files: HashSet<PathBuf>,
}

impl Watcher {
    /// Starts watching `path` (through its directory).
    fn add(&mut self, path: &Path) -> anyhow::Result<()> {
        let Some((dir, file)) = normalize(path) else {
            return Ok(());
        };
        if self.dirs.insert(dir.clone()) {
            self.debouncer
                .watch(&dir, RecursiveMode::NonRecursive)
                .with_context(|| format!("cannot watch {}", dir.display()))?;
        }
        self.files.insert(file);
        Ok(())
    }

    fn concerns(&self, paths: &[PathBuf]) -> bool {
        paths.iter().any(|p| {
            self.files.contains(p)
                || normalize(p).is_some_and(|(dir, f)| {
                    self.files.contains(&f) || (is_atomic_swap(&f) && self.dirs.contains(&dir))
                })
        })
    }
}

/// Whether `path` is the `..data` symlink a Kubernetes ConfigMap or Secret
/// volume swaps to publish an update: the files in the volume are links
/// through it, so their own names never appear in the events.
fn is_atomic_swap(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "..data")
}

/// `(directory, file)` with the directory canonicalized, so the file
/// matches the paths the watcher reports.
fn normalize(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let absolute = std::path::absolute(path).ok()?;
    let dir = absolute.parent()?;
    let name = absolute.file_name()?;
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let file = dir.join(name);
    Some((dir, file))
}

/// Watches `config_path` and its contracts, reconciling the store after
/// each change settles (debounced by [`DEBOUNCE`]) with the `MB_*`
/// environment variables applied. A config that fails to load or
/// reconcile is logged and the previous state kept.
///
/// Must be called inside a Tokio runtime.
pub fn spawn(
    config_path: PathBuf,
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
) -> anyhow::Result<WatchGuard> {
    spawn_with(config_path, store, state, Arc::new(file_config::apply_env))
}

/// [`spawn`] with a custom `prepare` step in place of applying the
/// environment.
pub fn spawn_with(
    config_path: PathBuf,
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
    prepare: Prepare,
) -> anyhow::Result<WatchGuard> {
    spawn_with_hook(config_path, store, state, prepare, None)
}

/// [`spawn_with`] that also runs `applied` after each successful reload.
pub fn spawn_with_hook(
    config_path: PathBuf,
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
    prepare: Prepare,
    applied: Option<Applied>,
) -> anyhow::Result<WatchGuard> {
    spawn_with_status(
        config_path,
        store,
        state,
        prepare,
        applied,
        ReloadStatus::default(),
    )
}

/// [`spawn_with_hook`] that records each reload's outcome in `status`.
pub fn spawn_with_status(
    config_path: PathBuf,
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
    prepare: Prepare,
    applied: Option<Applied>,
    status: ReloadStatus,
) -> anyhow::Result<WatchGuard> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
    let debouncer = new_debouncer(DEBOUNCE, None, move |result: DebounceEventResult| {
        match result {
            Ok(events) => {
                let paths = events
                    .into_iter()
                    .flat_map(|e| e.event.paths)
                    .collect::<Vec<_>>();
                if !paths.is_empty() {
                    // A closed channel means the watch is being dropped.
                    let _ = tx.send(paths);
                }
            }
            Err(errors) => {
                for error in errors {
                    tracing::warn!(%error, "config watch error");
                }
            }
        }
    })
    .context("starting the config file watcher")?;

    // Directories are watched rather than files, so editors that replace a
    // file by renaming a new one over it are still seen.
    let mut watcher = Watcher {
        debouncer,
        dirs: HashSet::new(),
        files: HashSet::new(),
    };
    watcher.add(&config_path)?;
    if let Ok(cfg) = file_config::load(&config_path) {
        for path in contract_paths(&cfg) {
            watcher.add(&path)?;
        }
    }
    let watcher = Arc::new(Mutex::new(watcher));

    let shared = Arc::clone(&watcher);
    let task = tokio::spawn(async move {
        while let Some(paths) = rx.recv().await {
            let relevant = match shared.lock() {
                Ok(w) => w.concerns(&paths),
                Err(_) => return,
            };
            if !relevant {
                continue;
            }
            match load_and_reconcile(&config_path, store.as_ref(), state.as_ref(), &prepare).await {
                Ok((cfg, report)) => {
                    status.succeeded();
                    if let Ok(mut w) = shared.lock() {
                        for path in contract_paths(&cfg) {
                            if let Err(error) = w.add(&path) {
                                tracing::warn!(error = %format!("{error:#}"), "cannot watch contract");
                            }
                        }
                    }
                    if let Some(applied) = &applied {
                        applied(cfg).await;
                    }
                    if report.is_empty() {
                        tracing::debug!(config = %config_path.display(), "config reloaded; no changes");
                    } else {
                        let changed = report.summary();
                        tracing::info!(
                            config = %config_path.display(),
                            added = report.created.len(),
                            removed = report.deleted.len(),
                            created = ?report.created,
                            updated = ?report.updated,
                            deleted = ?report.deleted,
                            new_versions = ?report.new_versions,
                            "config reloaded: {changed}"
                        );
                    }
                }
                Err(error) => {
                    status.failed(format!("{error:#}"));
                    tracing::error!(
                        config = %config_path.display(),
                        error = %format!("{error:#}"),
                        "config reload failed; keeping the previous state"
                    );
                }
            }
        }
    });

    Ok(WatchGuard {
        _watcher: watcher,
        task,
    })
}

async fn load_and_reconcile(
    config_path: &Path,
    store: &dyn ControlStore,
    state: &dyn StateStore,
    prepare: &Prepare,
) -> anyhow::Result<(FileConfig, crate::reconcile::ReconcileReport)> {
    let mut cfg = file_config::load(config_path)?;
    prepare(&mut cfg)?;
    let report = reconcile(store, state, &cfg).await?;
    Ok((cfg, report))
}

fn contract_paths(cfg: &FileConfig) -> Vec<PathBuf> {
    cfg.mocks.values().map(|m| m.contract.clone()).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn watcher(dir: &Path, file: &str) -> Watcher {
        let debouncer = new_debouncer(DEBOUNCE, None, |_: DebounceEventResult| {}).unwrap();
        let mut watcher = Watcher {
            debouncer,
            dirs: HashSet::new(),
            files: HashSet::new(),
        };
        watcher.add(&dir.join(file)).unwrap();
        watcher
    }

    #[test]
    fn a_configmap_swap_concerns_the_files_of_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let w = watcher(dir.path(), "mockingbird.yaml");
        assert!(w.concerns(&[dir.path().join("mockingbird.yaml")]));
        assert!(w.concerns(&[dir.path().join("..data")]));
        assert!(!w.concerns(&[dir.path().join("..data_tmp")]));
        assert!(!w.concerns(&[dir.path().join("other.yaml")]));
        assert!(!w.concerns(&[other.path().join("..data")]));
    }
}
