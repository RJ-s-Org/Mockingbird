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
#[derive(Debug, Clone, Default)]
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
#[must_use = "dropping the guard stops watching"]
pub struct WatchGuard {
    _watcher: Arc<Mutex<Watcher>>,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for WatchGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchGuard")
            .field("task", &self.task)
            .finish_non_exhaustive()
    }
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Debounced = Debouncer<notify::RecommendedWatcher, RecommendedCache>;

/// One debounced batch of file system events: the paths they name, and
/// whether any asks for a rescan (events were lost, so the paths are not
/// the whole story).
struct Batch {
    paths: Vec<PathBuf>,
    rescan: bool,
}

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

    /// Whether `batch` may have changed a file of interest: it names one,
    /// or it asks for a rescan.
    fn concerns_batch(&self, batch: &Batch) -> bool {
        batch.rescan || self.concerns(&batch.paths)
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
/// environment variables applied. It also reconciles once as soon as the
/// watch is armed, so an edit made between the caller's last read of the
/// file and the start of the watch is not lost. A config that fails to
/// load or reconcile is logged and the previous state kept.
///
/// Must be called inside a Tokio runtime.
///
/// # Errors
///
/// Fails when the file watcher cannot be created or a path cannot be
/// watched.
///
/// # Panics
///
/// Panics when called outside a Tokio runtime.
pub fn spawn(
    config_path: PathBuf,
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
) -> anyhow::Result<WatchGuard> {
    spawn_with(config_path, store, state, Arc::new(file_config::apply_env))
}

/// [`spawn`] with a custom `prepare` step in place of applying the
/// environment.
///
/// # Errors
///
/// As [`spawn`].
///
/// # Panics
///
/// Panics when called outside a Tokio runtime.
pub fn spawn_with(
    config_path: PathBuf,
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
    prepare: Prepare,
) -> anyhow::Result<WatchGuard> {
    spawn_with_hook(config_path, store, state, prepare, None)
}

/// [`spawn_with`] that also runs `applied` after each successful reload.
///
/// # Errors
///
/// As [`spawn`].
///
/// # Panics
///
/// Panics when called outside a Tokio runtime.
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
///
/// # Errors
///
/// As [`spawn`].
///
/// # Panics
///
/// Panics when called outside a Tokio runtime.
pub fn spawn_with_status(
    config_path: PathBuf,
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
    prepare: Prepare,
    applied: Option<Applied>,
    status: ReloadStatus,
) -> anyhow::Result<WatchGuard> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Batch>();
    let debouncer = new_debouncer(DEBOUNCE, None, move |result: DebounceEventResult| {
        match result {
            Ok(events) => {
                // A rescan event means the OS dropped or coalesced events
                // (FSEvents does under heavy file system churn): it names
                // a directory, not the files that changed, so any of them
                // may have.
                let rescan = events.iter().any(|e| e.event.need_rescan());
                let paths = events
                    .into_iter()
                    .flat_map(|e| e.event.paths)
                    .collect::<Vec<_>>();
                if rescan || !paths.is_empty() {
                    // A closed channel means the watch is being dropped.
                    let _ = tx.send(Batch { paths, rescan });
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
        for path in watched_paths(&cfg) {
            watcher.add(&path)?;
        }
    }
    let watcher = Arc::new(Mutex::new(watcher));

    let reloader = Reloader {
        config_path,
        store,
        state,
        prepare,
        applied,
        status,
        watcher: Arc::clone(&watcher),
    };
    let task = tokio::spawn(async move {
        // The file is watched from here on, but an edit made before then
        // (after the caller last read it, e.g. during a new leader's
        // reconcile) raised no event: re-read it once.
        reloader.reload(true).await;
        while let Some(batch) = rx.recv().await {
            let relevant = match reloader.watcher.lock() {
                Ok(w) => w.concerns_batch(&batch),
                Err(_) => return,
            };
            if relevant {
                reloader.reload(false).await;
            }
        }
    });

    Ok(WatchGuard {
        _watcher: watcher,
        task,
    })
}

/// What a reload needs: the config, where it goes, and the watcher to add
/// new contract and world files to.
struct Reloader {
    config_path: PathBuf,
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
    prepare: Prepare,
    applied: Option<Applied>,
    status: ReloadStatus,
    watcher: Arc<Mutex<Watcher>>,
}

impl Reloader {
    /// Re-reads the config and reconciles it, keeping the previous state
    /// when that fails. `initial` is the check made as the watch starts,
    /// which usually finds nothing changed and then logs at debug level.
    async fn reload(&self, initial: bool) {
        let path = self.config_path.display();
        match load_and_reconcile(
            &self.config_path,
            self.store.as_ref(),
            self.state.as_ref(),
            &self.prepare,
        )
        .await
        {
            Ok((cfg, report)) => {
                self.status.succeeded();
                if let Ok(mut w) = self.watcher.lock() {
                    for path in watched_paths(&cfg) {
                        if let Err(error) = w.add(&path) {
                            tracing::warn!(file = %path.display(), error = %format!("{error:#}"), "cannot watch a mock's file");
                        }
                    }
                }
                if let Some(applied) = &self.applied {
                    applied(cfg).await;
                }
                if report.is_empty() {
                    if initial {
                        tracing::debug!(config = %path, "config unchanged since it was last applied");
                    } else {
                        tracing::info!(config = %path, "config reloaded: no changes");
                    }
                } else {
                    let changed = report.summary();
                    tracing::info!(
                        config = %path,
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
                self.status.failed(format!("{error:#}"));
                tracing::error!(
                    config = %path,
                    error = %format!("{error:#}"),
                    "config reload failed; keeping the previous state"
                );
            }
        }
    }
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

/// The files besides the config whose edits reload it: each mock's
/// `contract` and `world`.
fn watched_paths(cfg: &FileConfig) -> Vec<PathBuf> {
    cfg.mocks
        .values()
        .flat_map(|m| std::iter::once(m.contract.clone()).chain(m.world.clone()))
        .collect()
}

#[cfg(test)]
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

    /// A rescan names the directory, not the file: events were dropped,
    /// so the config may have changed and is reloaded.
    #[test]
    fn a_rescan_concerns_the_watch() {
        let dir = tempfile::tempdir().unwrap();
        let w = watcher(dir.path(), "mockingbird.yaml");
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let named = |rescan| Batch {
            paths: vec![canonical.clone()],
            rescan,
        };
        assert!(!w.concerns_batch(&named(false)));
        assert!(w.concerns_batch(&named(true)));
        assert!(w.concerns_batch(&Batch {
            paths: Vec::new(),
            rescan: true,
        }));
    }
}
