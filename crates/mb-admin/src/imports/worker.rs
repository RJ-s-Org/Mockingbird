//! The in-process import worker pool: claims queued jobs from the store,
//! runs the smart import pipeline and stores each result as a draft
//! contract version.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use mb_ext::{Tenancy, UsageEvent, UsageSink};
use mb_import::smart::fetch::SystemResolver;
use mb_import::smart::merge::{MergeBase, merge_drafts};
use mb_import::smart::model::{DraftMeta, DraftWarning};
use mb_import::smart::{
    DraftResult, ExtractLlm, ImportInput, ImportKind, ImportObserver, ImportOptions, Phase,
    SmartError, run_import_sources,
};
use mb_llm::{Capabilities, ChatRequest, ChatResponse, LlmError, Provider};
use mb_store::{
    ContractStatus, ControlStore, ImportJob, ImportOutcome, ImportProgress, Mock, NewAuditEvent,
    NewContractVersion, StoreError,
};
use serde_json::json;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::IMPORT_OPERATION;
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
#[must_use = "dropping ImportWorkers aborts the workers at once"]
pub struct ImportWorkers {
    handles: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for ImportWorkers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportWorkers")
            .field("workers", &self.handles.len())
            .finish()
    }
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
///
/// # Panics
///
/// When called outside a Tokio runtime.
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

/// One import job's provider: reports every call to the usage sink and its
/// tokens to `mb_import_tokens_total`, so failed and canceled jobs are
/// metered like finished ones.
struct MeteredProvider {
    inner: Arc<dyn Provider>,
    usage: Arc<dyn UsageSink>,
    tenancy: Tenancy,
    mock: String,
}

#[async_trait]
impl Provider for MeteredProvider {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn total_timeout(&self, request_timeout: Duration) -> Duration {
        self.inner.total_timeout(request_timeout)
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let started = Instant::now();
        let result = self.inner.complete(req).await;
        let usage = result.as_ref().map(|r| r.usage).unwrap_or_default();
        let mut event = UsageEvent::new(self.tenancy.clone(), self.mock.clone(), IMPORT_OPERATION);
        event.status = if result.is_ok() { 200 } else { 502 };
        event.source = IMPORT_OPERATION.to_string();
        event.latency = started.elapsed();
        event.prompt_tokens = usage.prompt_tokens;
        event.completion_tokens = usage.completion_tokens;
        self.usage.record(&event);
        metrics::counter!("mb_import_tokens_total").increment(
            u64::from(usage.prompt_tokens).saturating_add(u64::from(usage.completion_tokens)),
        );
        result
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

/// The sources of a job from its kind and stored input.
fn job_inputs(job: &ImportJob, input: Vec<u8>) -> Result<Vec<ImportInput>, String> {
    match job.kind.as_str() {
        "url" => String::from_utf8(input)
            .map(|url| vec![ImportInput::Url(url)])
            .map_err(|_| "the job's URL is not UTF-8".to_string()),
        super::URLS_KIND => serde_json::from_slice::<Vec<String>>(&input)
            .map(|urls| urls.into_iter().map(ImportInput::Url).collect())
            .map_err(|e| format!("the job's URLs are invalid: {e}")),
        super::DOCUMENTS_KIND => {
            #[derive(serde::Deserialize)]
            struct Stored {
                kind: String,
                name: Option<String>,
                text: String,
            }
            let documents: Vec<Stored> = serde_json::from_slice(&input)
                .map_err(|e| format!("the job's documents are invalid: {e}"))?;
            documents
                .into_iter()
                .map(|d| match ImportKind::parse(&d.kind) {
                    Some(kind) => Ok(ImportInput::Bytes {
                        kind,
                        name: d.name,
                        bytes: d.text.into_bytes(),
                    }),
                    None => Err(format!("unknown document kind {:?}", d.kind)),
                })
                .collect()
        }
        kind => match ImportKind::parse(kind) {
            Some(kind) => Ok(vec![ImportInput::Bytes {
                kind,
                name: job.source_name.clone(),
                bytes: input,
            }]),
            None => Err(format!("unknown import kind {kind:?}")),
        },
    }
}

/// `draft` merged into the mock's newest draft or active version, unless
/// the job's mode is `replace` or there is none. When the base cannot take
/// a merge (not OpenAPI 3, or the merge does not compile), a job without a
/// mode stores the draft unmerged with a `merge_skipped` warning; a job
/// with mode `merge` fails.
async fn merge_into_base(
    st: &AdminState,
    job: &ImportJob,
    mock: &Mock,
    mut draft: DraftResult,
) -> Result<DraftResult, String> {
    if job.mode.as_deref() == Some("replace") {
        return Ok(draft);
    }
    let Some(base) = st
        .store
        .latest_contract_version(mock.id, &[ContractStatus::Draft, ContractStatus::Active])
        .await
        .map_err(|e| format!("cannot read the contract versions: {e}"))?
    else {
        return Ok(draft);
    };
    let version = base.version;
    let base = MergeBase {
        version,
        source: base.source,
        meta: base
            .draft_meta
            .and_then(|m| serde_json::from_value::<DraftMeta>(m).ok()),
    };
    let incoming = draft.clone();
    let merged = tokio::task::spawn_blocking(move || merge_drafts(&base, incoming))
        .await
        .map_err(|e| {
            tracing::error!(job = %job.id, error = %e, "merging the draft panicked");
            UNEXPECTED_ERROR.to_string()
        })?;
    match merged {
        Ok(merged) => Ok(merged),
        Err(e) if job.mode.is_none() => {
            let reason = match e {
                SmartError::Unsupported(reason) => reason,
                other => other.to_string(),
            };
            draft.meta.warnings.push(DraftWarning::new(
                "merge_skipped",
                format!("not merged into version {version}: {reason}"),
            ));
            Ok(draft)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Claims and runs one queued job to completion; Ok(false) when none was queued.
///
/// # Errors
///
/// A [`StoreError`] when the queue cannot be read or the job's outcome
/// cannot be stored; a failed import itself is recorded on the job, not
/// returned.
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
    // The tenant resolved when the job was created, stored with it so any
    // node bills it; a job queued before the tenant was stored is billed to
    // the default tenant.
    let tenancy = match st.store.import_job_tenant(job.id).await? {
        Some(tenant) => Tenancy::new(tenant, &job.workspace),
        None => crate::tenancy(&job.workspace),
    };
    let llm = match st.llm.import_llm_for(&mock, &tenancy).await {
        // Every call is reported, whatever becomes of the job.
        Ok(llm) => llm.map(|llm| ExtractLlm {
            provider: Arc::new(MeteredProvider {
                inner: llm.provider,
                usage: st.ext.usage.clone(),
                tenancy,
                mock: job.mock.clone(),
            }),
            settings: llm.settings,
        }),
        Err(e) => return fail(st, job, e.to_string()).await,
    };
    let inputs = match job_inputs(job, input) {
        Ok(inputs) => inputs,
        Err(e) => return fail(st, job, e).await,
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
        run_import_sources(inputs, llm.as_ref(), &opts, &observer, &SystemResolver).await
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
    let draft = match merge_into_base(st, job, &mock, draft).await {
        Ok(draft) => draft,
        Err(e) => return fail(st, job, e).await,
    };

    let meta = match serde_json::to_value(&draft.meta) {
        Ok(meta) => meta,
        Err(e) => return fail(st, job, format!("the draft metadata is invalid: {e}")).await,
    };
    // Sizing the schemas of a large bundle is CPU-bound: off the runtime.
    let sized = tokio::task::spawn_blocking(move || {
        let compile_warnings = mb_import::bundle_warnings(&draft.bundle);
        (draft, compile_warnings)
    })
    .await;
    let (draft, compile_warnings) = match sized {
        Ok(sized) => sized,
        Err(e) => {
            tracing::error!(job = %job.id, error = %e, "sizing the draft's schemas panicked");
            return fail(st, job, UNEXPECTED_ERROR.to_string()).await;
        }
    };
    let operations = draft.bundle.operations.len();
    let warnings = draft.meta.warnings.len();
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
        Ok(Some(v)) => {
            if !compile_warnings.is_empty() {
                let stored = match serde_json::to_value(&compile_warnings) {
                    Ok(value) => {
                        st.store
                            .set_contract_version_warnings(mock.id, v.version, value)
                            .await
                    }
                    Err(e) => Err(StoreError::from(e)),
                };
                if let Err(e) = stored {
                    tracing::warn!(job = %job.id, error = %e, "cannot store the draft's compilation warnings");
                }
            }
            v.version
        }
        Ok(None) => {
            count("canceled");
            return Ok(());
        }
        Err(e) => return fail(st, job, format!("the draft could not be stored: {e}")).await,
    };
    count("done");
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
