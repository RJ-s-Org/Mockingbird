//! The in-process import worker pool: claims queued jobs from the store,
//! runs the smart import pipeline and stores each result as a draft
//! contract version.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use mb_import::smart::{
    ImportInput, ImportKind, ImportObserver, ImportOptions, Phase, SmartError, run_import,
};
use mb_store::{
    ContractStatus, ControlStore, ImportJob, ImportOutcome, ImportProgress, NewAuditEvent,
    NewContractVersion, StoreError,
};
use serde_json::json;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::AdminState;

/// How often worker 0 fails stale jobs and prunes old ones, at most.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);
/// The error of a job whose pipeline panicked.
const UNEXPECTED_ERROR: &str = "the import failed unexpectedly";
/// The error of a job whose mock disappeared.
const MOCK_DELETED: &str = "the mock was deleted";
/// The actor of `import.complete` when the job records no creator.
const SYSTEM_ACTOR: &str = "system";

/// The running worker tasks; dropping this aborts them and the import
/// pipelines they are running.
pub struct ImportWorkers {
    handles: Vec<JoinHandle<()>>,
}

impl Drop for ImportWorkers {
    fn drop(&mut self) {
        for h in &self.handles {
            h.abort();
        }
    }
}

/// A spawned task that is aborted when this handle is dropped, so it never
/// outlives the task awaiting it.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts `st.imports.settings.workers` worker tasks.
pub fn spawn_import_workers(st: &AdminState) -> ImportWorkers {
    let handles = (0..st.imports.settings.workers)
        .map(|n| tokio::spawn(work(st.clone(), n)))
        .collect();
    ImportWorkers { handles }
}

async fn work(st: AdminState, n: usize) {
    let settings = st.imports.settings.clone();
    let mut last_maintenance: Option<Instant> = None;
    loop {
        if n == 0 && last_maintenance.is_none_or(|t| t.elapsed() >= MAINTENANCE_INTERVAL) {
            maintain(&st).await;
            last_maintenance = Some(Instant::now());
        }
        match run_next_import(&st).await {
            Ok(true) => {}
            Ok(false) => {
                tokio::select! {
                    () = st.imports.wake.notified() => {}
                    () = tokio::time::sleep(settings.poll_interval) => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, worker = n, "import worker could not claim a job");
                tokio::time::sleep(settings.poll_interval).await;
            }
        }
    }
}

/// Fails stale running jobs and prunes old finished ones.
async fn maintain(st: &AdminState) {
    let settings = &st.imports.settings;
    let now = Utc::now();
    let ago = |d: Duration| {
        chrono::TimeDelta::from_std(d)
            .ok()
            .and_then(|d| now.checked_sub_signed(d))
    };
    if let Some(cutoff) = ago(settings.stale_after) {
        match st.store.fail_stale_import_jobs(cutoff, now).await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(jobs = n, "failed stale import jobs"),
            Err(e) => tracing::warn!(error = %e, "could not fail stale import jobs"),
        }
    }
    if let Some(cutoff) = ago(settings.retention)
        && let Err(e) = st.store.prune_import_jobs(cutoff).await
    {
        tracing::warn!(error = %e, "could not prune import jobs");
    }
}

/// Records progress in the store; a job no longer running cancels.
struct StoreObserver {
    store: Arc<dyn ControlStore>,
    job: Uuid,
}

#[async_trait]
impl ImportObserver for StoreObserver {
    async fn progress(&self, phase: Phase, done: u32, total: u32) -> bool {
        let p = ImportProgress {
            phase: phase.as_str().to_string(),
            done,
            total,
        };
        match self
            .store
            .update_import_progress(self.job, &p, Utc::now())
            .await
        {
            Ok(running) => running,
            Err(e) => {
                // A transient store error must not cancel the import.
                tracing::warn!(error = %e, job = %self.job, "could not record import progress");
                true
            }
        }
    }
}

fn count(outcome: &'static str) {
    metrics::counter!("mb_import_jobs_total", "outcome" => outcome).increment(1);
}

async fn fail(st: &AdminState, job: &ImportJob, error: String) -> Result<(), StoreError> {
    tracing::info!(job = %job.id, mock = %job.mock, %error, "import failed");
    st.store
        .finish_import_job(job.id, ImportOutcome::Failed { error }, Utc::now())
        .await?;
    count("failed");
    Ok(())
}

/// Claims and runs one queued job to completion; Ok(false) when none was queued.
pub async fn run_next_import(st: &AdminState) -> Result<bool, StoreError> {
    let Some(job) = st.store.claim_import_job(Utc::now()).await? else {
        return Ok(false);
    };
    run_job(st, &job).await?;
    Ok(true)
}

async fn run_job(st: &AdminState, job: &ImportJob) -> Result<(), StoreError> {
    let Some(input) = st.store.import_job_input(job.id).await? else {
        // Canceled between claiming and reading the input.
        count("canceled");
        return Ok(());
    };
    let mock = match st.store.get_mock(&job.workspace, &job.mock).await? {
        Some(m) if m.id == job.mock_id => m,
        _ => return fail(st, job, MOCK_DELETED.to_string()).await,
    };
    let llm = match st.llm.for_import(&mock).await {
        Ok(llm) => llm,
        Err(e) => return fail(st, job, e).await,
    };
    let input = if job.kind == "url" {
        match String::from_utf8(input) {
            Ok(url) => ImportInput::Url(url),
            Err(_) => return fail(st, job, "the job's URL is not UTF-8".to_string()).await,
        }
    } else {
        match ImportKind::parse(&job.kind) {
            Some(kind) => ImportInput::Bytes {
                kind,
                name: job.source_name.clone(),
                bytes: input,
            },
            None => {
                return fail(st, job, format!("unknown import kind {:?}", job.kind)).await;
            }
        }
    };
    let opts = ImportOptions {
        max_tokens: st.imports.settings.max_tokens,
        fetch: st.imports.settings.fetch.clone(),
    };
    let observer = StoreObserver {
        store: st.store.clone(),
        job: job.id,
    };
    // A panic in the pipeline becomes a JoinError instead of killing the
    // worker; aborting the worker (shutdown) aborts the pipeline with it.
    let mut run = AbortOnDrop(tokio::spawn(async move {
        run_import(input, llm.as_ref(), &opts, &observer).await
    }));
    let draft = match (&mut run.0).await {
        Err(e) => {
            tracing::error!(job = %job.id, error = %e, "import pipeline panicked");
            return fail(st, job, UNEXPECTED_ERROR.to_string()).await;
        }
        Ok(Err(SmartError::Canceled)) => {
            count("canceled");
            return Ok(());
        }
        Ok(Err(e)) => return fail(st, job, e.to_string()).await,
        Ok(Ok(draft)) => draft,
    };

    let meta = match serde_json::to_value(&draft.meta) {
        Ok(meta) => meta,
        Err(e) => return fail(st, job, format!("the draft metadata is invalid: {e}")).await,
    };
    let operations = draft.bundle.operations.len();
    let warnings = draft.meta.warnings.len();
    let tokens = draft
        .meta
        .usage
        .prompt_tokens
        .saturating_add(draft.meta.usage.completion_tokens);
    // The draft is stored only if the job is still running, in the same
    // transaction that marks it done: a cancel or stale sweep that won the
    // race leaves no orphaned draft and no audit event.
    let completed = st
        .store
        .complete_import_job(
            job.id,
            mock.id,
            NewContractVersion {
                source: draft.source,
                bundle: draft.bundle,
                status: ContractStatus::Draft,
                created_by: job.created_by.clone(),
            },
            meta,
            Utc::now(),
        )
        .await;
    let version = match completed {
        Ok(Some(v)) => v.version,
        Ok(None) => {
            count("canceled");
            return Ok(());
        }
        Err(e) => return fail(st, job, format!("the draft could not be stored: {e}")).await,
    };
    count("done");
    metrics::counter!("mb_import_tokens_total").increment(tokens);
    st.audit
        .record(NewAuditEvent {
            workspace: Some(job.workspace.clone()),
            actor_subject: job
                .created_by
                .clone()
                .unwrap_or_else(|| SYSTEM_ACTOR.to_string()),
            actor_kind: SYSTEM_ACTOR.to_string(),
            action: "import.complete".to_string(),
            target: format!("contract:{}@{version}", mock.name),
            before: None,
            after: Some(json!({
                "job": job.id,
                "operations": operations,
                "warnings": warnings,
            })),
            request_id: None,
            at: Utc::now(),
        })
        .await;
    Ok(())
}
