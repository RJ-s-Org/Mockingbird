//! Deterministic replays without an LLM: `serve --record <dir>` writes
//! every LLM answer to `<dir>/answers.jsonl`, `serve --replay <dir>`
//! answers from that file and never calls an LLM (a request it holds no
//! answer for falls back to the faker), and `mockingbird snapshot
//! export|import` copy contracts and session state between a running
//! server and the same directory (`snapshot export --include` adds the
//! mocks' scenarios, chaos and limits; see [`SnapshotPart`]).
//!
//! Answers are matched on the prompt, not on the HTTP request: the prompt
//! already carries everything that shapes an answer (the operation, its
//! schema, the request and the session's resources), and recording at the
//! provider covers every protocol and the response cache alike. An exact
//! prompt match is tried first, then any answer recorded for the same
//! operation and status (the same system message). Repeated prompts are
//! answered in recorded order, the last answer repeating.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use clap::{Args, Subcommand};
use futures::StreamExt;
use mb_llm::{
    Capabilities, ChatRequest, ChatResponse, LlmError, MeteredStream, Provider, ResponseFormat,
    Role, StreamUsage, TextStream, Usage,
};
use mb_state::{SessionKey, StateStore, StoredResource};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::client::{self, ClientError};

/// The recorded answers in a snapshot directory.
pub const ANSWERS_FILE: &str = "answers.jsonl";
/// What `snapshot export` wrote: its version and workspace.
pub const META_FILE: &str = "snapshot.json";
/// Prometheus counter of answers served or recorded, by `mock` and
/// `result` (`exact`, `operation`, `miss`, `recorded`).
pub const ANSWERS_METRIC: &str = "mb_snapshot_answers_total";
/// Why a replayed call fell back to the faker; it ends up as the request
/// log's `fallback_reason`.
pub const MISS_REASON: &str = "no answer recorded in the snapshot for this prompt (replay)";

const FORMAT_VERSION: u32 = 1;

/// One line of [`ANSWERS_FILE`].
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Answer {
    v: u32,
    /// The whole prompt's digest.
    key: String,
    /// The system message's digest: the operation, its status and schema.
    operation: String,
    content: String,
}

/// The recorded answers, each held once and shared by its two lists.
#[derive(Default)]
struct Answers {
    exact: HashMap<String, Vec<Arc<str>>>,
    operation: HashMap<String, Vec<Arc<str>>>,
    /// `(prompt digest, answer)` of every answer, for the duplicate check.
    recorded: HashSet<(String, Arc<str>)>,
    /// How many times each list was served, by `exact:`/`operation:` key.
    served: HashMap<String, usize>,
}

impl Answers {
    fn insert(&mut self, a: Answer) -> bool {
        let content: Arc<str> = Arc::from(a.content);
        if !self.recorded.insert((a.key.clone(), Arc::clone(&content))) {
            return false;
        }
        self.exact
            .entry(a.key)
            .or_default()
            .push(Arc::clone(&content));
        self.operation.entry(a.operation).or_default().push(content);
        true
    }

    /// The next answer of `list`, the last one repeating.
    fn next(&mut self, kind: &str, key: &str) -> Option<String> {
        let list = match kind {
            "exact" => self.exact.get(key)?,
            _ => self.operation.get(key)?,
        };
        let served = self.served.entry(format!("{kind}:{key}")).or_default();
        let answer = list
            .get(*served)
            .or_else(|| list.last())
            .map(|a| a.to_string());
        *served += 1;
        answer
    }
}

/// How a server uses a snapshot directory.
pub struct Snapshot {
    dir: PathBuf,
    replay: bool,
    answers: Mutex<Answers>,
    /// Open for appending with `--record`.
    writer: Option<Mutex<std::fs::File>>,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("dir", &self.dir)
            .field("replay", &self.replay)
            .field("record", &self.writer.is_some())
            .finish_non_exhaustive()
    }
}

/// What [`Snapshot::open_with`] opens a snapshot directory for
/// (`--replay`, `--record`, or both).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SnapshotMode {
    /// Answer from the recorded answers.
    pub replay: bool,
    /// Append new answers to [`ANSWERS_FILE`].
    pub record: bool,
}

impl SnapshotMode {
    /// `--replay` alone.
    pub const REPLAY: SnapshotMode = SnapshotMode {
        replay: true,
        record: false,
    };
    /// `--record` alone.
    pub const RECORD: SnapshotMode = SnapshotMode {
        replay: false,
        record: true,
    };
}

/// Where a replayed answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hit {
    /// The same prompt was recorded.
    Exact,
    /// Only the same operation was recorded.
    Operation,
}

impl Snapshot {
    /// [`Snapshot::open_with`] with `replay` and `record` as bare flags.
    ///
    /// # Errors
    ///
    /// As [`Snapshot::open_with`].
    #[deprecated(
        since = "0.1.1",
        note = "use `Snapshot::open_with`, which names the mode"
    )]
    pub fn open(dir: &Path, replay: bool, record: bool) -> anyhow::Result<Snapshot> {
        Snapshot::open_with(dir, SnapshotMode { replay, record })
    }

    /// Opens `dir`: loads its answers for `mode.replay` and, for
    /// `mode.record`, creates it and opens [`ANSWERS_FILE`] for appending.
    ///
    /// # Errors
    ///
    /// Fails when an answers file does not parse or has an unsupported
    /// version, or (for `record`) `dir` or the file cannot be created.
    pub fn open_with(dir: &Path, mode: SnapshotMode) -> anyhow::Result<Snapshot> {
        let SnapshotMode { replay, record } = mode;
        let path = dir.join(ANSWERS_FILE);
        let mut answers = Answers::default();
        if replay || record {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    for (n, line) in text.lines().enumerate() {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let a: Answer = serde_json::from_str(line)
                            .with_context(|| format!("{}:{}", path.display(), n + 1))?;
                        if a.v != FORMAT_VERSION {
                            bail!(
                                "{}:{}: answer format v{} (this build reads v{FORMAT_VERSION})",
                                path.display(),
                                n + 1,
                                a.v
                            );
                        }
                        answers.insert(a);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !replay => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!(
                        "{} does not exist: every LLM answer falls back to the faker",
                        path.display()
                    );
                }
                Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
            }
        }
        let writer = if record {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            Some(Mutex::new(file))
        } else {
            None
        };
        Ok(Snapshot {
            dir: dir.to_path_buf(),
            replay,
            answers: Mutex::new(answers),
            writer,
        })
    }

    /// The snapshot directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether recorded answers are served (`--replay`).
    pub fn replays(&self) -> bool {
        self.replay
    }

    /// Whether new answers are appended (`--record`).
    pub fn records(&self) -> bool {
        self.writer.is_some()
    }

    fn answers(&self) -> std::sync::MutexGuard<'_, Answers> {
        self.answers.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The recorded answer for `req`, if any.
    pub fn lookup(&self, req: &ChatRequest) -> Option<(String, Hit)> {
        let (key, operation) = keys(req);
        let mut answers = self.answers();
        if let Some(a) = answers.next("exact", &key) {
            return Some((a, Hit::Exact));
        }
        answers
            .next("operation", &operation)
            .map(|a| (a, Hit::Operation))
    }

    /// Appends `content` as the answer to `req` (a duplicate is skipped).
    ///
    /// # Errors
    ///
    /// Fails when the answer cannot be serialised or appended.
    pub fn record(&self, req: &ChatRequest, content: &str) -> anyhow::Result<bool> {
        let Some(writer) = &self.writer else {
            return Ok(false);
        };
        let (key, operation) = keys(req);
        let answer = Answer {
            v: FORMAT_VERSION,
            key,
            operation,
            content: content.to_string(),
        };
        let mut line = serde_json::to_vec(&answer)?;
        line.push(b'\n');
        if !self.answers().insert(answer) {
            return Ok(false);
        }
        // One append of the whole line: the file is unbuffered, so a
        // formatted write would be several system calls.
        let mut file = writer.lock().unwrap_or_else(|p| p.into_inner());
        file.write_all(&line)
            .context("appending to the recorded answers")?;
        Ok(true)
    }

    /// Seeds the sessions under `<dir>/sessions/<mock>/<session>.json`
    /// into `workspace` (the one `snapshot export` read them from); returns
    /// how many were seeded.
    ///
    /// # Errors
    ///
    /// Fails when a session file cannot be read or parsed, or the state store
    /// rejects a write.
    pub async fn seed_sessions(
        &self,
        state: &dyn StateStore,
        workspace: &str,
    ) -> anyhow::Result<usize> {
        let mut seeded = 0;
        for (mock, session, resources) in read_sessions(&self.dir)? {
            let resources: Vec<StoredResource> = serde_json::from_value(resources)
                .with_context(|| format!("sessions/{mock}/{session}.json"))?;
            let key = SessionKey {
                workspace: workspace.to_string(),
                mock,
                session,
            };
            state
                .seed(&key, resources)
                .await
                .map_err(|e| anyhow!("seeding session {}: {e}", key.session))?;
            seeded += 1;
        }
        Ok(seeded)
    }
}

/// `(prompt digest, system message digest)` of `req`. The response format
/// is part of both: it changes what a valid answer looks like.
fn keys(req: &ChatRequest) -> (String, String) {
    let format = match &req.response_format {
        ResponseFormat::Text => json!("text"),
        ResponseFormat::JsonObject => json!("json_object"),
        ResponseFormat::JsonSchema { name, schema } => json!(["json_schema", name, schema]),
    };
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|m| json!([m.role.as_str(), m.content]))
        .collect();
    let system: Vec<&Value> = req
        .messages
        .iter()
        .zip(&messages)
        .filter(|(m, _)| m.role == Role::System)
        .map(|(_, v)| v)
        .collect();
    (
        digest(&json!([format, messages])),
        digest(&json!([format, system])),
    )
}

fn digest(v: &Value) -> String {
    Sha256::digest(v.to_string().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The snapshot `serve --record/--replay` installed in this process.
static ACTIVE: ArcSwapOption<Snapshot> = ArcSwapOption::const_empty();

/// Makes `snapshot` the one every engine built from now on uses. Process
/// wide (one `serve` per process), so the runtimes need no new dependency.
pub fn install(snapshot: Option<Arc<Snapshot>>) {
    ACTIVE.store(snapshot);
}

/// The installed snapshot.
pub fn active() -> Option<Arc<Snapshot>> {
    ACTIVE.load_full()
}

/// `inner` (the mock's provider chain, `None` when offline or with no
/// profile) wrapped for the installed snapshot: unchanged without one; a
/// replay answers from the snapshot and, without `inner`, fails over to
/// the faker on a miss.
pub fn wrap(mock: &str, inner: Option<Arc<dyn Provider>>) -> Option<Arc<dyn Provider>> {
    let Some(snapshot) = active() else {
        return inner;
    };
    if inner.is_none() && !snapshot.replays() {
        return None;
    }
    Some(Arc::new(SnapshotProvider {
        inner,
        snapshot,
        mock: mock.to_string(),
    }))
}

/// Answers from a [`Snapshot`] and records what `inner` answers.
pub struct SnapshotProvider {
    inner: Option<Arc<dyn Provider>>,
    snapshot: Arc<Snapshot>,
    mock: String,
}

impl std::fmt::Debug for SnapshotProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotProvider")
            .field("mock", &self.mock)
            .field("snapshot", &self.snapshot)
            .field("live", &self.inner.is_some())
            .finish()
    }
}

impl SnapshotProvider {
    fn count(&self, result: &'static str) {
        metrics::counter!(ANSWERS_METRIC, "mock" => self.mock.clone(), "result" => result)
            .increment(1);
    }

    /// The replayed answer, counted; `None` when not replaying or on a miss.
    fn replayed(&self, req: &ChatRequest) -> Option<String> {
        if !self.snapshot.replays() {
            return None;
        }
        match self.snapshot.lookup(req) {
            Some((content, hit)) => {
                self.count(match hit {
                    Hit::Exact => "exact",
                    Hit::Operation => "operation",
                });
                Some(content)
            }
            None => {
                self.count("miss");
                None
            }
        }
    }

    fn keep(&self, req: &ChatRequest, content: &str) {
        match self.snapshot.record(req, content) {
            Ok(true) => self.count("recorded"),
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "snapshot: cannot record"),
        }
    }

    fn miss() -> LlmError {
        LlmError::Http {
            status: 404,
            body: MISS_REASON.to_string(),
        }
    }
}

#[async_trait]
impl Provider for SnapshotProvider {
    fn capabilities(&self) -> Capabilities {
        match &self.inner {
            Some(p) => p.capabilities(),
            None => Capabilities {
                json_schema: true,
                json_mode: true,
            },
        }
    }

    fn total_timeout(&self, request_timeout: Duration) -> Duration {
        match &self.inner {
            Some(p) => p.total_timeout(request_timeout),
            None => request_timeout,
        }
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        if let Some(content) = self.replayed(&req) {
            return Ok(ChatResponse {
                content,
                usage: Usage::default(),
            });
        }
        let Some(inner) = &self.inner else {
            return Err(Self::miss());
        };
        let recorded = self.snapshot.records().then(|| req.clone());
        let response = inner.complete(req).await?;
        if let Some(req) = recorded {
            self.keep(&req, &response.content);
        }
        Ok(response)
    }

    async fn stream(&self, req: ChatRequest) -> Result<TextStream, LlmError> {
        self.stream_metered(req).await.map(|m| m.text)
    }

    /// A replay reports no tokens; otherwise the inner provider's usage.
    async fn stream_metered(&self, req: ChatRequest) -> Result<MeteredStream, LlmError> {
        if let Some(content) = self.replayed(&req) {
            let usage = StreamUsage::new();
            usage.set(Usage::default());
            return Ok(MeteredStream {
                text: Box::pin(futures::stream::once(std::future::ready(Ok(content)))),
                usage,
            });
        }
        let Some(inner) = &self.inner else {
            return Err(Self::miss());
        };
        if !self.snapshot.records() {
            return inner.stream_metered(req).await;
        }
        let MeteredStream {
            text: stream,
            usage,
        } = inner.stream_metered(req.clone()).await?;
        // The whole text is recorded once the stream ends without an error.
        let text = Arc::new(Mutex::new(Some(String::new())));
        let seen = Arc::clone(&text);
        let tee = stream.map(move |item| {
            let mut text = seen.lock().unwrap_or_else(|p| p.into_inner());
            match (&item, text.as_mut()) {
                (Ok(delta), Some(t)) => t.push_str(delta),
                (Err(_), _) => *text = None,
                _ => {}
            }
            item
        });
        let me = SnapshotProvider {
            inner: None,
            snapshot: Arc::clone(&self.snapshot),
            mock: self.mock.clone(),
        };
        let done = futures::stream::once(async move {
            let full = text.lock().unwrap_or_else(|p| p.into_inner()).take();
            if let Some(full) = full {
                me.keep(&req, &full);
            }
        })
        .filter_map(|()| std::future::ready(None));
        Ok(MeteredStream {
            text: Box::pin(tee.chain(done)),
            usage,
        })
    }
}

/// `<dir>/sessions/<mock>/<session>.json`, sorted: `(mock, session, json)`.
fn read_sessions(dir: &Path) -> anyhow::Result<Vec<(String, String, Value)>> {
    let root = dir.join("sessions");
    let mut out = Vec::new();
    let Ok(mocks) = std::fs::read_dir(&root) else {
        return Ok(out);
    };
    for mock in mocks {
        let mock = mock?;
        if !mock.file_type()?.is_dir() {
            continue;
        }
        let mock_name = mock.file_name().to_string_lossy().into_owned();
        for file in std::fs::read_dir(mock.path())? {
            let path = file?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(session) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let value = serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not JSON", path.display()))?;
            out.push((mock_name.clone(), session.to_string(), value));
        }
    }
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    Ok(out)
}

#[derive(Subcommand, Debug)]
pub enum SnapshotCommand {
    /// Write the running server's contracts and session state to a
    /// directory you can commit (answers are recorded there by `serve
    /// --record`).
    Export(SnapshotArgs),
    /// Create the snapshot's missing mocks from its contracts and seed its
    /// sessions into the running server.
    Import(SnapshotArgs),
}

#[derive(Args, Debug)]
pub struct SnapshotArgs {
    /// The snapshot directory.
    #[arg(long, default_value = ".mockingbird/snapshot")]
    pub dir: PathBuf,
}

/// A mock setting `snapshot export --include` adds to a snapshot. None is
/// exported by default; `snapshot import` restores whichever the snapshot
/// holds.
#[derive(
    clap::ValueEnum,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SnapshotPart {
    /// The mock's scenarios (`scenarios/<mock>/<scenario>.json`) and its
    /// default scenario.
    Scenarios,
    /// The mock's chaos settings.
    Chaos,
    /// The mock's limits.
    Limits,
}

impl SnapshotPart {
    /// The `--include` value and the name in `snapshot.json`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SnapshotPart::Scenarios => "scenarios",
            SnapshotPart::Chaos => "chaos",
            SnapshotPart::Limits => "limits",
        }
    }
}

impl std::fmt::Display for SnapshotPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// The `--include` flag that `crate::config::command` adds to `snapshot
// export`. No doc comment: clap would make it the about text of the
// subcommand.
#[derive(Args, Debug)]
pub(crate) struct IncludeArgs {
    /// Also export these mock settings (comma-separated): `scenarios` (with
    /// the default scenario), `chaos`, `limits`. None by default; `snapshot
    /// import` restores them on the mocks it creates.
    #[arg(long, value_enum, value_delimiter = ',', value_name = "PART")]
    pub(crate) include: Vec<SnapshotPart>,
}

/// `snapshot export --include …`: an export that also writes the chosen
/// [`SnapshotPart`]s. Run by [`export_with`]; a plain `snapshot export` is
/// [`SnapshotCommand::Export`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SnapshotExportArgs {
    /// The snapshot directory.
    pub dir: PathBuf,
    /// What to add to the default export, sorted and without duplicates.
    pub include: Vec<SnapshotPart>,
}

impl SnapshotExportArgs {
    /// An export of `dir` that adds `include`.
    pub fn new(dir: impl Into<PathBuf>, include: impl IntoIterator<Item = SnapshotPart>) -> Self {
        let mut include: Vec<SnapshotPart> = include.into_iter().collect();
        include.sort_unstable();
        include.dedup();
        Self {
            dir: dir.into(),
            include,
        }
    }
}

/// The `snapshot` subcommands; JSON summaries.
///
/// # Errors
///
/// Fails when the Admin API cannot be reached or answers with an error,
/// or the snapshot directory cannot be read or written.
pub async fn run(
    admin_url: &str,
    token: Option<&str>,
    workspace: &str,
    cmd: SnapshotCommand,
) -> anyhow::Result<Value> {
    let api = Api {
        admin_url,
        token,
        workspace,
    };
    match cmd {
        SnapshotCommand::Export(args) => export(&api, &args.dir, &[]).await,
        SnapshotCommand::Import(args) => import(&api, &args.dir).await,
    }
}

/// `snapshot export --include …`: [`run`]'s export plus `args.include`,
/// which add `chaos`, `limits` and `default_scenario` to each mock file,
/// `scenarios/<mock>/<scenario>.json` files and an `include` list to
/// `snapshot.json`. Without parts it writes exactly what a plain export
/// writes.
///
/// # Errors
///
/// As [`run`].
pub async fn export_with(
    admin_url: &str,
    token: Option<&str>,
    workspace: &str,
    args: &SnapshotExportArgs,
) -> anyhow::Result<Value> {
    let api = Api {
        admin_url,
        token,
        workspace,
    };
    export(&api, &args.dir, &args.include).await
}

struct Api<'a> {
    admin_url: &'a str,
    token: Option<&'a str>,
    workspace: &'a str,
}

impl Api<'_> {
    async fn json(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<Value>,
    ) -> Result<Value, ClientError> {
        client::api(
            self.admin_url,
            self.token,
            Some(self.workspace),
            method,
            segments,
            &[],
            body,
        )
        .await
        .map(Option::unwrap_or_default)
    }

    async fn get(&self, segments: &[&str]) -> anyhow::Result<Value> {
        self.json(Method::GET, segments, None)
            .await
            .map_err(api_error)
    }

    fn url(&self, segments: &[&str]) -> anyhow::Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(self.admin_url)
            .with_context(|| format!("invalid admin URL {:?}", self.admin_url))?;
        url.path_segments_mut()
            .map_err(|()| anyhow!("invalid admin URL {:?}", self.admin_url))?
            .pop_if_empty()
            .extend(["api", "v1", "workspaces", self.workspace])
            .extend(segments);
        Ok(url)
    }

    fn request(&self, method: Method, url: reqwest::Url) -> reqwest::RequestBuilder {
        let req = reqwest::Client::new().request(method, url);
        match self.token.filter(|t| !t.is_empty()) {
            Some(token) => req.bearer_auth(token),
            None => req,
        }
    }
}

fn api_error(e: ClientError) -> anyhow::Error {
    match e {
        ClientError::Api { status, body } => anyhow!("Admin API {status}: {body}"),
        ClientError::Usage(m) => anyhow!(m),
        ClientError::Incomplete { message, .. } => anyhow!(message),
        ClientError::Other(e) => e,
    }
}

/// The mock settings `snapshot import` creates a missing mock with: what
/// it is called and how requests reach it and are answered. Not its LLM
/// profile, data-plane auth or webhooks, which name things of the source
/// environment (profiles, secrets, targets) the target may not have, and
/// its scenarios, chaos and limits only when asked for ([`SnapshotPart`]).
/// Snapshots written before `path_rewrite` and `header_overrides` were
/// exported import with the defaults.
const MOCK_FIELDS: &[&str] = &[
    "name",
    "route_prefix",
    "persona",
    "consistency",
    "unknown_ids",
    "hosts",
    "path_rewrite",
    "header_overrides",
];

/// The directory of each mock's exported scenarios.
const SCENARIOS_DIR: &str = "scenarios";

/// The mock-view fields each [`SnapshotPart`] adds to a mock file.
fn part_fields(part: SnapshotPart) -> &'static [&'static str] {
    match part {
        SnapshotPart::Scenarios => &["default_scenario"],
        SnapshotPart::Chaos => &["chaos"],
        SnapshotPart::Limits => &["limits"],
    }
}

/// Whether `name` is safe as a file stem (scenario names are
/// `[A-Za-z0-9._-]`, which the Admin API enforces).
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

async fn export(api: &Api<'_>, dir: &Path, include: &[SnapshotPart]) -> anyhow::Result<Value> {
    let mocks = api.get(&["mocks"]).await?;
    let mocks = mocks.as_array().cloned().unwrap_or_default();
    std::fs::create_dir_all(dir.join("mocks"))
        .with_context(|| format!("creating {}", dir.display()))?;
    let mut summary = Vec::new();
    for mock in &mocks {
        let Some(name) = mock["name"].as_str() else {
            continue;
        };
        let fields = MOCK_FIELDS
            .iter()
            .chain(include.iter().flat_map(|p| part_fields(*p)));
        let settings: serde_json::Map<String, Value> = fields
            .filter_map(|k| {
                mock.get(*k)
                    .filter(|v| !v.is_null())
                    .map(|v| ((*k).to_string(), v.clone()))
            })
            .collect();
        let mut contract = Value::Null;
        if let Some(version) = mock["active_version"].as_i64() {
            let version = version.to_string();
            let url = api.url(&["mocks", name, "contracts", &version, "source"])?;
            let resp = api
                .request(Method::GET, url)
                .send()
                .await
                .context("downloading a contract")?
                .error_for_status()?;
            let ext = resp
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit_once('.'))
                .map(|(_, ext)| ext.trim_end_matches('"').to_string())
                .unwrap_or_else(|| "yaml".into());
            let bytes = resp.bytes().await?;
            let file = format!("{name}.{ext}");
            write(&dir.join("contracts").join(&file), &bytes)?;
            contract = json!(format!("contracts/{file}"));
        }
        let mut entry = Value::Object(settings);
        entry["contract"] = contract.clone();
        write(
            &dir.join("mocks").join(format!("{name}.json")),
            &serde_json::to_vec_pretty(&entry)?,
        )?;
        let sessions_dir = dir.join("sessions").join(name);
        // A re-export replaces the mock's sessions.
        if sessions_dir.exists() {
            std::fs::remove_dir_all(&sessions_dir)
                .with_context(|| format!("clearing {}", sessions_dir.display()))?;
        }
        let sessions = api.get(&["mocks", name, "sessions"]).await?;
        let mut exported = 0;
        for s in sessions.as_array().into_iter().flatten() {
            let Some(id) = s["session"].as_str() else {
                continue;
            };
            let resources = api.get(&["mocks", name, "sessions", id]).await?;
            write(
                &sessions_dir.join(format!("{id}.json")),
                &serde_json::to_vec_pretty(&resources)?,
            )?;
            exported += 1;
        }
        let scenarios_dir = dir.join(SCENARIOS_DIR).join(name);
        // A re-export replaces the mock's scenarios, and one without
        // `scenarios` drops them.
        if scenarios_dir.exists() {
            std::fs::remove_dir_all(&scenarios_dir)
                .with_context(|| format!("clearing {}", scenarios_dir.display()))?;
        }
        let mut entry = json!({ "mock": name, "contract": contract, "sessions": exported });
        if include.contains(&SnapshotPart::Scenarios) {
            let scenarios = api.get(&["mocks", name, "scenarios"]).await?;
            let mut written = 0;
            for scenario in scenarios.as_array().into_iter().flatten() {
                let Value::Object(mut body) = scenario.clone() else {
                    continue;
                };
                let Some(Value::String(id)) = body.remove("name") else {
                    continue;
                };
                if !plain_name(&id) {
                    bail!("mock {name} has a scenario named {id:?}, which is not a file name");
                }
                // The rest is the scenario's `PUT` body.
                body.remove("source");
                write(
                    &scenarios_dir.join(format!("{id}.json")),
                    &serde_json::to_vec_pretty(&body)?,
                )?;
                written += 1;
            }
            entry["scenarios"] = json!(written);
        }
        summary.push(entry);
    }
    let mut meta = json!({ "version": FORMAT_VERSION, "workspace": api.workspace });
    if !include.is_empty() {
        meta["include"] = json!(include);
    }
    write(&dir.join(META_FILE), &serde_json::to_vec_pretty(&meta)?)?;
    let answers = count_lines(&dir.join(ANSWERS_FILE));
    Ok(json!({ "dir": dir, "mocks": summary, "answers": answers }))
}

async fn import(api: &Api<'_>, dir: &Path) -> anyhow::Result<Value> {
    if !dir.is_dir() {
        bail!("{} is not a snapshot directory", dir.display());
    }
    let mut created = Vec::new();
    let mut restored = 0;
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir.join("mocks")) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();
    for path in files {
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut mock: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not JSON", path.display()))?;
        let Some(name) = mock["name"].as_str().map(str::to_string) else {
            continue;
        };
        match api.json(Method::GET, &["mocks", &name], None).await {
            Ok(_) => continue,
            Err(ClientError::Api { status, .. }) if status == reqwest::StatusCode::NOT_FOUND => {}
            Err(e) => return Err(api_error(e)),
        }
        let contract = mock
            .as_object_mut()
            .and_then(|m| m.remove("contract"))
            .and_then(|c| c.as_str().map(str::to_string));
        // Set once the scenarios exist (`chaos` and `limits` are part of
        // the create body).
        let default_scenario = mock
            .as_object_mut()
            .and_then(|m| m.remove("default_scenario"))
            .filter(|d| !d.is_null());
        api.json(Method::POST, &["mocks"], Some(mock))
            .await
            .map_err(api_error)
            .with_context(|| format!("creating mock {name}"))?;
        if let Some(contract) = contract {
            let file = dir.join(&contract);
            let source = mb_import::read_source(&file)
                .with_context(|| format!("cannot read {}", file.display()))?;
            let content_type = client::contract_media_type(&file, &source);
            let mut url = api.url(&["mocks", &name, "contracts"])?;
            url.set_query(Some("activate=true"));
            let resp = api
                .request(Method::POST, url)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(source)
                .send()
                .await
                .context("uploading a contract")?;
            if !resp.status().is_success() {
                let status = resp.status();
                bail!(
                    "uploading {}: Admin API {status}: {}",
                    file.display(),
                    resp.text().await.unwrap_or_default()
                );
            }
        }
        // After the contract: a scenario is checked against it.
        for (scenario, body) in read_scenarios(dir, &name)? {
            api.json(
                Method::PUT,
                &["mocks", &name, "scenarios", &scenario],
                Some(body),
            )
            .await
            .map_err(api_error)
            .with_context(|| format!("restoring scenario {scenario} of {name}"))?;
            restored += 1;
        }
        if let Some(default_scenario) = default_scenario {
            api.json(
                Method::PATCH,
                &["mocks", &name],
                Some(json!({ "default_scenario": default_scenario })),
            )
            .await
            .map_err(api_error)
            .with_context(|| format!("setting the default scenario of {name}"))?;
        }
        created.push(name);
    }
    let mut seeded = 0;
    for (mock, session, resources) in read_sessions(dir)? {
        api.json(
            Method::PUT,
            &["mocks", &mock, "sessions", &session],
            Some(resources),
        )
        .await
        .map_err(api_error)
        .with_context(|| format!("seeding session {session} of {mock}"))?;
        seeded += 1;
    }
    let mut out = json!({ "dir": dir, "created_mocks": created, "seeded_sessions": seeded });
    if restored > 0 {
        out["restored_scenarios"] = json!(restored);
    }
    Ok(out)
}

/// `<dir>/scenarios/<mock>/<scenario>.json`, sorted: `(scenario, body)`.
fn read_scenarios(dir: &Path, mock: &str) -> anyhow::Result<Vec<(String, Value)>> {
    let root = dir.join(SCENARIOS_DIR).join(mock);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let body = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not JSON", path.display()))?;
        out.push((name.to_string(), body));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn count_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// The workspace a snapshot's sessions were exported from, else `default`.
pub fn workspace_of(dir: &Path) -> String {
    std::fs::read(dir.join(META_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|m| m["workspace"].as_str().map(str::to_string))
        .unwrap_or_else(|| mb_store::DEFAULT_WORKSPACE.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mb_llm::ChatMessage;

    fn req(system: &str, user: &str) -> ChatRequest {
        ChatRequest {
            messages: vec![
                ChatMessage {
                    role: Role::System,
                    content: system.into(),
                },
                ChatMessage {
                    role: Role::User,
                    content: user.into(),
                },
            ],
            response_format: ResponseFormat::JsonObject,
            temperature: 0.7,
            max_tokens: 100,
            timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn records_then_replays_in_order_then_by_operation() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Snapshot::open_with(dir.path(), SnapshotMode::RECORD).unwrap();
        assert!(rec.record(&req("op", "a"), "1").unwrap());
        assert!(rec.record(&req("op", "a"), "2").unwrap());
        assert!(!rec.record(&req("op", "a"), "1").unwrap(), "duplicate");
        drop(rec);

        let play = Snapshot::open_with(dir.path(), SnapshotMode::REPLAY).unwrap();
        let get = |r| play.lookup(&r).unwrap();
        assert_eq!(get(req("op", "a")), ("1".into(), Hit::Exact));
        assert_eq!(get(req("op", "a")), ("2".into(), Hit::Exact));
        assert_eq!(
            get(req("op", "a")),
            ("2".into(), Hit::Exact),
            "last repeats"
        );
        assert_eq!(get(req("op", "b")).1, Hit::Operation);
        assert!(play.lookup(&req("other", "a")).is_none());
    }

    /// MOCKINGBIRD-PERF-13 benchmark: recording `k` distinct answers to one
    /// prompt (the duplicate check scanned every earlier answer).
    #[test]
    #[ignore = "benchmark"]
    fn bench_answers_insert() {
        for k in [1_000usize, 10_000] {
            let mut answers = Answers::default();
            let content = "x".repeat(512);
            let start = std::time::Instant::now();
            for i in 0..k {
                answers.insert(Answer {
                    v: FORMAT_VERSION,
                    key: "k".into(),
                    operation: "op".into(),
                    content: format!("{content}{i}"),
                });
            }
            println!(
                "PERF-13 {k} answers to one prompt recorded in {:?}",
                start.elapsed()
            );
        }
    }

    /// MOCKINGBIRD-STUB-3: a mock's path rewrite and header-override switch
    /// survive export and import, so an imported mock routes as before.
    #[tokio::test]
    async fn export_then_import_keeps_routing_settings() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        const WS: &str = "/api/v1/workspaces/default";
        let source = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{WS}/mocks")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "name": "tmdb",
                "route_prefix": "/tmdb",
                "hosts": ["api.tmdb.test"],
                "header_overrides": false,
                "path_rewrite": { "add_prefix": "/3" },
                "llm_profile": "env-only",
                "active_version": null,
            }])))
            .mount(&source)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{WS}/mocks/tmdb/sessions")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&source)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("snap");
        let api = Api {
            admin_url: &source.uri(),
            token: None,
            workspace: "default",
        };
        export(&api, &snap, &[]).await.unwrap();

        let target = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{WS}/mocks/tmdb")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&target)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{WS}/mocks")))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
            .mount(&target)
            .await;
        let api = Api {
            admin_url: &target.uri(),
            token: None,
            workspace: "default",
        };
        let out = import(&api, &snap).await.unwrap();
        assert_eq!(out["created_mocks"], json!(["tmdb"]));
        let requests = target.received_requests().await.unwrap();
        let created = requests
            .iter()
            .find(|r| r.method.as_str() == "POST")
            .unwrap();
        let body: Value = serde_json::from_slice(&created.body).unwrap();
        assert_eq!(
            body["path_rewrite"],
            json!({ "add_prefix": "/3" }),
            "{body}"
        );
        assert_eq!(body["header_overrides"], false, "{body}");
        assert_eq!(body["hosts"], json!(["api.tmdb.test"]));
        assert!(body.get("llm_profile").is_none(), "{body}");
    }

    const WS: &str = "/api/v1/workspaces/default";

    /// A source server whose mock `pay` has chaos, limits, a default
    /// scenario and one scenario.
    async fn source_with_behaviour() -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let source = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{WS}/mocks")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "name": "pay",
                "route_prefix": "/pay",
                "persona": null,
                "llm_profile": "env-only",
                "header_overrides": true,
                "consistency": "on",
                "unknown_ids": "generate",
                "hosts": [],
                "chaos": { "error_rate": 0.25 },
                "limits": { "max_body_bytes": 1024 },
                "default_scenario": "declined",
                "active_version": null,
            }])))
            .mount(&source)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{WS}/mocks/pay/sessions")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&source)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{WS}/mocks/pay/scenarios")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "name": "declined",
                "instructions": "Decline the card.",
                "status": 402,
                "latency_ms": null,
                "set": null,
                "operations": [],
                "source": "api",
            }])))
            .mount(&source)
            .await;
        source
    }

    /// Every file under `dir`, relative path to bytes.
    fn files(dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(root, &p, out);
                } else {
                    let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                    out.insert(rel, std::fs::read(&p).unwrap());
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    /// SNAPSHOT-OPTIN: a plain export writes exactly what it wrote before
    /// the opt-in existed, byte for byte, and never reads scenarios.
    #[tokio::test]
    async fn a_default_export_is_unchanged() {
        let source = source_with_behaviour().await;
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("snap");
        let api = Api {
            admin_url: &source.uri(),
            token: None,
            workspace: "default",
        };
        let out = export(&api, &snap, &[]).await.unwrap();
        assert_eq!(
            out["mocks"],
            json!([{ "mock": "pay", "contract": null, "sessions": 0 }])
        );
        let files = files(&snap);
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            ["mocks/pay.json", "snapshot.json"]
        );
        assert_eq!(
            String::from_utf8_lossy(&files["mocks/pay.json"]),
            "{\n  \"consistency\": \"on\",\n  \"contract\": null,\n  \"header_overrides\": true,\n  \"hosts\": [],\n  \"name\": \"pay\",\n  \"route_prefix\": \"/pay\",\n  \"unknown_ids\": \"generate\"\n}"
        );
        assert_eq!(
            String::from_utf8_lossy(&files["snapshot.json"]),
            "{\n  \"version\": 1,\n  \"workspace\": \"default\"\n}"
        );
        let requests = source.received_requests().await.unwrap();
        assert!(
            requests
                .iter()
                .all(|r| !r.url.path().ends_with("/scenarios")),
            "a default export reads no scenarios"
        );
    }

    /// SNAPSHOT-OPTIN: `--include scenarios,chaos,limits` adds them to the
    /// snapshot, and importing it restores them on the created mock.
    #[tokio::test]
    async fn an_opt_in_export_restores_scenarios_chaos_and_limits() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let source = source_with_behaviour().await;
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("snap");
        let api = Api {
            admin_url: &source.uri(),
            token: None,
            workspace: "default",
        };
        let args = SnapshotExportArgs::new(
            &snap,
            [
                SnapshotPart::Limits,
                SnapshotPart::Scenarios,
                SnapshotPart::Chaos,
                SnapshotPart::Limits,
            ],
        );
        let out = export(&api, &args.dir, &args.include).await.unwrap();
        assert_eq!(out["mocks"][0]["scenarios"], 1, "{out}");
        let meta: Value = serde_json::from_slice(&files(&snap)["snapshot.json"]).unwrap();
        assert_eq!(meta["include"], json!(["scenarios", "chaos", "limits"]));
        let scenario: Value =
            serde_json::from_slice(&files(&snap)["scenarios/pay/declined.json"]).unwrap();
        assert!(scenario.get("name").is_none() && scenario.get("source").is_none());
        assert_eq!(scenario["status"], 402);

        let target = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{WS}/mocks/pay")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&target)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{WS}/mocks")))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
            .mount(&target)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("{WS}/mocks/pay/scenarios/declined")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&target)
            .await;
        Mock::given(method("PATCH"))
            .and(path(format!("{WS}/mocks/pay")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&target)
            .await;
        let api = Api {
            admin_url: &target.uri(),
            token: None,
            workspace: "default",
        };
        let out = import(&api, &snap).await.unwrap();
        assert_eq!(out["created_mocks"], json!(["pay"]));
        assert_eq!(out["restored_scenarios"], 1, "{out}");
        let requests = target.received_requests().await.unwrap();
        let order: Vec<&str> = requests
            .iter()
            .filter(|r| r.method.as_str() != "GET")
            .map(|r| r.method.as_str())
            .collect();
        assert_eq!(order, ["POST", "PUT", "PATCH"], "scenarios before default");
        let body = |m: &str| -> Value {
            let r = requests.iter().find(|r| r.method.as_str() == m).unwrap();
            serde_json::from_slice(&r.body).unwrap()
        };
        let created = body("POST");
        assert_eq!(created["chaos"], json!({ "error_rate": 0.25 }), "{created}");
        assert_eq!(created["limits"], json!({ "max_body_bytes": 1024 }));
        assert!(created.get("default_scenario").is_none(), "{created}");
        assert_eq!(body("PUT")["instructions"], "Decline the card.");
        assert_eq!(body("PATCH"), json!({ "default_scenario": "declined" }));
    }

    /// SNAPSHOT-OPTIN: a snapshot written before the opt-in (no scenarios,
    /// no `include`) still imports as before: one create, no scenario or
    /// default-scenario calls, the same summary.
    #[tokio::test]
    async fn an_old_snapshot_still_imports() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path();
        write(
            &snap.join("mocks/pay.json"),
            br#"{"name":"pay","route_prefix":"/pay","consistency":"on","unknown_ids":"generate","hosts":[],"contract":null}"#,
        )
        .unwrap();
        write(
            &snap.join("snapshot.json"),
            br#"{"version":1,"workspace":"default"}"#,
        )
        .unwrap();
        let target = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{WS}/mocks/pay")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&target)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{WS}/mocks")))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
            .mount(&target)
            .await;
        let api = Api {
            admin_url: &target.uri(),
            token: None,
            workspace: "default",
        };
        let out = import(&api, snap).await.unwrap();
        assert_eq!(
            out,
            json!({ "dir": snap, "created_mocks": ["pay"], "seeded_sessions": 0 })
        );
        let methods: Vec<String> = target
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.method.to_string())
            .collect();
        assert_eq!(methods, ["GET", "POST"]);
    }

    /// SNAPSHOT-OPTIN: `snapshot export` stays the `Cli` command; with
    /// `--include` it becomes [`WorkflowCommand::SnapshotExport`].
    #[test]
    fn include_parses_as_the_opt_in_export() {
        use crate::client::WorkflowCommand;
        use crate::config::{Command, Invocation, parse_from};
        let plain = parse_from(["mockingbird", "snapshot", "export", "--dir", "d"]).unwrap();
        assert!(
            matches!(&plain, Invocation::Cli(cli) if matches!(
                &cli.cmd,
                Command::Snapshot(SnapshotCommand::Export(a)) if a.dir == Path::new("d")
            )),
            "{plain:?}"
        );
        let opt_in = parse_from([
            "mockingbird",
            "snapshot",
            "export",
            "--dir",
            "d",
            "--include",
            "limits,scenarios",
            "--include",
            "chaos",
        ])
        .unwrap();
        let Invocation::Workflow {
            cmd: WorkflowCommand::SnapshotExport(args),
            ..
        } = opt_in
        else {
            panic!("{opt_in:?}");
        };
        assert_eq!(
            args,
            SnapshotExportArgs::new(
                "d",
                [
                    SnapshotPart::Scenarios,
                    SnapshotPart::Chaos,
                    SnapshotPart::Limits
                ]
            )
        );
        assert!(
            parse_from(["mockingbird", "snapshot", "import", "--include", "chaos"]).is_err(),
            "import restores whatever the snapshot holds"
        );
        let mut cmd = crate::config::command();
        let about = cmd
            .find_subcommand_mut("snapshot")
            .and_then(|s| s.find_subcommand_mut("export"))
            .and_then(|e| e.get_about())
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(about.starts_with("Write the running server"), "{about}");
        assert!(
            parse_from(["mockingbird", "snapshot", "export", "--include", "webhooks"]).is_err()
        );
    }

    #[tokio::test]
    async fn a_replay_without_a_provider_misses_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let provider = SnapshotProvider {
            inner: None,
            snapshot: Arc::new(Snapshot::open_with(dir.path(), SnapshotMode::REPLAY).unwrap()),
            mock: "m".into(),
        };
        let err = provider.complete(req("op", "a")).await.unwrap_err();
        assert!(err.to_string().contains(MISS_REASON), "{err}");
        assert!(!err.is_retryable());
    }
}
