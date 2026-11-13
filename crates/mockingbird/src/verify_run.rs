//! `mockingbird verify --run "<cmd>"` (MCP `verify_run`): runs the app's
//! own command (its tests, an end-to-end suite) against the mocks, the way
//! `mockingbird run` does ([`crate::run::open`]), and reports what in the
//! requests it made points at the mock.
//!
//! The window: before the command starts, the newest request-log id of
//! every mock (in every workspace the caller sees) and the proxy's
//! unmapped-host counts are read; after it ends (and the request log has
//! settled), only entries with a larger id and the growth of those counts
//! are examined. A server `verify` starts for the run has an empty log, so
//! everything it logs counts.
//!
//! Reported, each request once, each failure with a `next_step`:
//!
//! - `unmatched_route`: a request reached a mock but matched no operation
//!   of its contract (404 or 405);
//! - `error_status`: a mock answered 5xx, rejected the request in
//!   validation, or (when the command failed) answered another 4xx, with
//!   the operation, the status, the log entry's source and the problem's
//!   detail; while the command passes, such another 4xx is taken as
//!   expected and listed under `notes`;
//! - `fallback`: an answer fell back to the faker because the LLM call (or
//!   a replay) failed, with the reason;
//! - `blocked_host`: the proxy blocked a host no mock claims
//!   ([`crate::report::blocked_host_steps`]);
//! - `unclaimed_hosts`: hosts the command reached that a bundled
//!   description or catalog example serves but no mock lists
//!   ([`crate::hosts`]); the project's others are a note;
//! - `check`: an assertion given beside `--run` (`--called`, ...) that the
//!   window fails;
//! - `serve_failed`: the app's own server, given with `--serve` (MCP
//!   `serve`), exited or did not become ready, so the command was not run;
//! - `missing_dependencies`: the command or the serve command failed and
//!   its output shows a dependency that is not installed
//!   ([`crate::missing_deps`]); the step names the project's install
//!   command. Like `serve_failed`, it does not point at the mock.
//!
//! With `--serve "<cmd>"`, the app's server runs under the same environment
//! for the command's whole run: it is started after the window opens (its
//! requests count), waited for ([`ServeOptions`]), and its process group is
//! stopped once the command ends (TERM, then KILL after [`SERVE_GRACE`]),
//! whatever ends it: an exit, the timeout, Ctrl-C, an error or the MCP
//! client going away.
//!
//! Costs are capped: the command's time ([`Options::timeout`], then its
//! process group is stopped), the output kept ([`TAIL_LINES`] lines of
//! [`LINE_CHARS`] characters), the log entries examined
//! ([`MAX_REQUESTS`]), the problem bodies fetched ([`MAX_DETAILS`]) and
//! the failures listed ([`MAX_FAILURES`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use reqwest::Method;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncReadExt as _;

use crate::client::{self, ClientError};
use crate::report::{self, HostReport};

/// The default limit on the command's run time (`verify --run`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// The default limit on the command's run time for the MCP tool.
pub const MCP_DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// The longest run time the MCP tool accepts.
pub const MCP_MAX_TIMEOUT: Duration = Duration::from_secs(1800);

/// Lines of the command's output kept (its last ones).
pub const TAIL_LINES: usize = 60;

/// Characters kept of one output line.
pub const LINE_CHARS: usize = 400;

/// Request-log entries examined at most, newest first.
pub const MAX_REQUESTS: usize = 5_000;

/// Recorded requests read in full for the detail of a 4xx or 5xx.
pub const MAX_DETAILS: usize = 20;

/// Failures listed at most; the rest are counted.
pub const MAX_FAILURES: usize = 50;

/// How long the request log is given to settle after the command ends.
const SETTLE: Duration = Duration::from_secs(3);

/// How long the output readers are given after the command ends (a
/// process it left running may hold its pipes open).
const DRAIN: Duration = Duration::from_secs(2);

/// How long a serve command is given to end after TERM before its process
/// group is killed.
pub const SERVE_GRACE: Duration = Duration::from_secs(5);

/// The default limit on the time a serve command takes to become ready.
pub const DEFAULT_SERVE_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest readiness wait accepted (`--serve-timeout`, MCP
/// `serve_timeout_secs`).
pub const MAX_SERVE_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a serve command with no readiness signal (no port, no line,
/// none detected) must keep running before the command starts.
pub const SERVE_SETTLE: Duration = Duration::from_secs(3);

/// Lines of the serve command's output kept (its last ones).
pub const SERVE_TAIL_LINES: usize = 30;

/// The app's own server, run while the command runs (`--serve`).
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// The command, run with `sh -c` (`cmd /C` on Windows) from
    /// [`Options::cwd`], in a process group of its own.
    pub command: String,
    /// Ready once something accepts TCP connections on this localhost port.
    pub port: Option<u16>,
    /// Ready once a line of its output matches this regular expression
    /// (ECMA 262, as a JSON Schema `pattern`; found anywhere in the line).
    pub ready: Option<String>,
    /// How long it may take to become ready.
    pub timeout: Duration,
}

/// What to run and against which server.
#[derive(Debug, Clone)]
pub struct Options {
    /// The command, run with `sh -c` (`cmd /C` on Windows) from
    /// [`Options::cwd`].
    pub command: String,
    /// Where the command (and the serve command) run; `None` is the
    /// project root of the config.
    pub cwd: Option<PathBuf>,
    /// The app's own server, run while the command runs.
    pub serve: Option<ServeOptions>,
    /// The config; `None` is `./mockingbird.yaml`.
    pub config: Option<PathBuf>,
    /// Faker only, for a server this run starts.
    pub offline: bool,
    /// Where a server this run starts keeps its pid and log.
    pub state_dir: PathBuf,
    /// How long the command may run before it is stopped.
    pub timeout: Duration,
    /// The Admin API bearer token.
    pub token: Option<String>,
    /// Echo the command's output on stderr as it comes, and pass Ctrl-C
    /// on to it (the CLI; not the MCP server, whose stdio is the protocol).
    pub interactive: bool,
}

/// The command's run.
#[derive(Serialize, Debug, Default)]
pub struct CommandRun {
    pub command: String,
    /// Where it ran: the caller's folder (`--run`), the MCP tool's `cwd`,
    /// else the project root.
    pub cwd: String,
    /// Whether it was not started: the serve command failed or was
    /// interrupted first.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub not_run: bool,
    /// Its exit code (`128 + n` when signal `n` ended it, 127 when it could
    /// not start); `null` when it was stopped at the timeout or interrupted.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Whether it was stopped early: Ctrl-C or SIGTERM to the CLI, or the
    /// MCP client going away.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
    pub duration_ms: u64,
    /// The last lines of its stdout and stderr, interleaved as they came.
    pub output_tail: String,
    /// Whether earlier output (or the end of long lines) was left out.
    pub output_truncated: bool,
}

/// The serve command's run.
#[derive(Serialize, Debug, Default)]
pub struct ServeRun {
    pub command: String,
    /// Where it ran (as the command).
    pub cwd: String,
    /// What it was waited for: `port 4242`, `port 4242 (detected)`, `a line
    /// matching …`, both, or `running for 3s`.
    pub ready_by: String,
    pub ready: bool,
    /// How long it took to become ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_ms: Option<u64>,
    /// Its exit code when it ended on its own (before it was ready, or while
    /// the command ran).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether its process group was stopped (always, once it started).
    pub stopped: bool,
    /// The last lines of its stdout and stderr ([`SERVE_TAIL_LINES`]).
    pub output_tail: String,
    pub output_truncated: bool,
}

/// The requests examined.
#[derive(Serialize, Debug, Default)]
pub struct Window {
    /// Request-log entries made while the command ran (and examined).
    pub requests: u64,
    /// Whether [`MAX_REQUESTS`] stopped the reading (older ones in the
    /// window were not examined).
    pub truncated: bool,
    /// Whether this run started the server (and stopped it again).
    pub server_started: bool,
    /// The server's Admin API.
    pub admin: String,
}

/// One failure that points at the mock.
#[derive(Serialize, Debug, Clone)]
pub struct RunFailure {
    /// `unmatched_route`, `error_status`, `fallback`, `blocked_host`,
    /// `unclaimed_hosts`, `check`, `serve_failed` or
    /// `missing_dependencies`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mock: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The log entries' `source` (`rejected`, `state`, `llm`, ...); absent
    /// for a problem the mock answered itself (a scenario, a forced status,
    /// a world rule).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Requests it covers.
    pub count: u64,
    pub detail: String,
    /// A request-log id to read it from (`get_request`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

/// `verify --run`'s result.
#[derive(Serialize, Debug, Default)]
pub struct Outcome {
    /// The command exited 0 and nothing points at the mock.
    pub ok: bool,
    pub command: CommandRun,
    /// The app's server run beside the command (`--serve`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serve: Option<ServeRun>,
    pub window: Window,
    pub failures: Vec<RunFailure>,
    /// Failures past [`MAX_FAILURES`], not listed.
    #[serde(skip_serializing_if = "is_zero")]
    pub more_failures: usize,
    /// Hosts the proxy passed to the real API while the command ran
    /// (`proxy.unmapped: pass`), and anything that could not be checked.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// A mock in a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct MockRef {
    workspace: String,
    name: String,
}

/// Runs `opts.command` against the mocks and reports the window. `checks`
/// are assertions given beside `--run` (their failures become `check`
/// failures; the ones `--run` always makes are not repeated).
///
/// # Errors
///
/// Fails when the config does not load, the server cannot be started or
/// reached, or its request log cannot be read; a command that fails is a
/// result, not an error.
pub async fn run(opts: &Options, checks: Option<&report::VerifyArgs>) -> anyhow::Result<Outcome> {
    if let Some(pattern) = opts.serve.as_ref().and_then(|s| s.ready.as_deref()) {
        line_matcher(pattern, ready_arg(opts))?;
    }
    // The proxy `open` may run in this process counts into this process's
    // recorder.
    let local = crate::telemetry::install_prometheus().ok();
    let args = crate::run::RunArgs {
        config: opts.config.clone(),
        offline: opts.offline,
        record: None,
        replay: None,
        wait: Duration::from_secs(60).into(),
        keep: false,
        state_dir: opts.state_dir.clone(),
        command: Vec::new(),
    };
    let session = crate::run::open(&args).await?;
    let result = with_session(opts, checks, &session, local.as_ref()).await;
    session.close(false).await;
    result
}

async fn with_session(
    opts: &Options,
    checks: Option<&report::VerifyArgs>,
    session: &crate::run::Session,
    local: Option<&metrics_exporter_prometheus::PrometheusHandle>,
) -> anyhow::Result<Outcome> {
    let admin = session.admin_url.as_str();
    let token = opts.token.as_deref();
    let metrics = |own: bool| async move {
        let mut text = report::fetch_metrics(admin, token)
            .await
            .unwrap_or_default();
        if own && let Some(handle) = local {
            text.push('\n');
            text.push_str(&handle.render());
        }
        text
    };
    let own = session.runs_own_proxy();
    let mocks = list_mocks(admin, token).await?;
    let baseline = newest_ids(admin, token, &mocks).await;
    let metrics_before = metrics(own).await;

    let root = crate::hosts::project_root(&session.config);
    let cwd = opts.cwd.clone().unwrap_or_else(|| root.clone());
    let env = crate::proxy::env::proxy_env(&session.proxy)?;
    let mut serve_run = None;
    let mut serve_failure = None;
    let mut served = None;
    let mut serve_interrupted = false;
    if let Some(serve) = &opts.serve {
        let mut run = ServeRun {
            command: serve.command.clone(),
            cwd: cwd.display().to_string(),
            ..ServeRun::default()
        };
        let mut own = crate::test_commands::config_ports(&session.config);
        own.push((session.proxy.port, "proxy port (proxy.port)"));
        if let Ok(url) = reqwest::Url::parse(admin)
            && let Some(port) = url.port_or_known_default()
        {
            own.push((port, "Admin API port (server.admin_port)"));
        }
        let port = match (serve.port, &serve.ready) {
            (None, None) => detected_port(&root, &serve.command, &own)
                .await
                .map(|p| (p, true)),
            (port, _) => port.map(|p| (p, false)),
        };
        match start_serve(opts, serve, port, &own, &cwd, &env.vars, &mut run).await {
            ServeStart::Ready(s) => served = Some(s),
            ServeStart::Failed(f) => serve_failure = Some(f),
            ServeStart::Interrupted => serve_interrupted = true,
        }
        serve_run = Some(run);
    }
    let command = if serve_failure.is_some() || serve_interrupted {
        CommandRun {
            command: opts.command.clone(),
            cwd: cwd.display().to_string(),
            not_run: true,
            interrupted: serve_interrupted,
            ..CommandRun::default()
        }
    } else {
        // An error here drops `served`, which kills its process group.
        execute(opts, &cwd, &env.vars).await?
    };
    if let (Some(s), Some(run)) = (served, serve_run.as_mut()) {
        s.stop(run).await;
    }

    settle(admin, token).await;
    // Mocks created while the command ran have no baseline: all theirs count.
    let mocks = list_mocks(admin, token).await.unwrap_or(mocks);
    let read = window_entries(admin, token, &mocks, &baseline).await?;
    let entries = read.entries;
    let metrics_after = metrics(own).await;
    let hosts = host_delta(
        &report::unmapped_hosts(&metrics_before),
        &report::unmapped_hosts(&metrics_after),
    );

    let mut outcome = Outcome {
        window: Window {
            requests: entries.len() as u64,
            truncated: read.truncated,
            server_started: session.started_server(),
            admin: admin.to_owned(),
        },
        ..Outcome::default()
    };
    for m in &baseline.unread {
        outcome.notes.push(format!(
            "the request log of {} could not be read before the command ran; its requests were not examined",
            m.name
        ));
    }
    for m in &read.retention {
        outcome.notes.push(format!(
            "the request log of {} no longer reaches back to the command's start (observability.request_log_retention): its oldest requests in the window were not examined",
            m.name
        ));
    }
    let lost = dropped(&metrics_before);
    for (mock, n) in dropped(&metrics_after) {
        let grown = n.saturating_sub(lost.get(&mock).copied().unwrap_or(0));
        if grown > 0 {
            outcome.notes.push(format!(
                "the request log of {mock} dropped {grown} records while the command ran (its write queue was full): they were not examined"
            ));
        }
    }
    let command_ok = command.exit_code == Some(0);
    let (mut failures, expected) = request_failures(&entries, command_ok);
    outcome.notes.extend(expected);
    outcome
        .notes
        .extend(world_review_notes(admin, token, &entries).await);
    let serve_failed = serve_failure.is_some();
    if let Some(f) = serve_failure {
        failures.insert(0, f);
    } else if let Some(s) = serve_run.as_ref().filter(|s| s.ready) {
        if s.ready_by.starts_with("running for") {
            outcome.notes.push(format!(
                "the serve command was taken as ready because it still ran after {}s (no port or ready line was given or detected); pass --serve-port (MCP serve_port) or --serve-ready (serve_ready) to wait for it properly",
                SERVE_SETTLE.as_secs()
            ));
        }
        if let Some(code) = s.exit_code {
            outcome.notes.push(format!(
                "the serve command exited {code} while the command ran (serve.output_tail says why)"
            ));
        }
    }
    add_details(admin, token, &mut failures).await;
    let missing = missing_dependencies(
        &root,
        &cwd,
        serve_run.as_ref().map(|s| (s, serve_failed)),
        &command,
    );
    // The serve command failed on a missing dependency: its failure defers
    // to that one's step.
    if missing.iter().any(|f| f.detail.ends_with(SERVE_OUTPUT))
        && let Some(f) = failures.iter_mut().find(|f| f.kind == "serve_failed")
    {
        f.next_step = Some(
            "Install the missing dependency first (the missing_dependencies failure says how), then run verify --run again.".into(),
        );
    }
    for (i, f) in missing.into_iter().enumerate() {
        failures.insert(i, f);
    }

    let mut claimed = HashSet::new();
    for ws in mocks
        .iter()
        .map(|m| m.workspace.as_str())
        .collect::<HashSet<_>>()
    {
        claimed.extend(report::claimed_hosts(admin, token, ws).await);
    }
    let mut blocked = HashSet::new();
    for h in hosts.iter().filter(|h| h.mode == "block") {
        let steps = report::blocked_host_steps_during(
            std::slice::from_ref(h),
            &claimed,
            "while the command ran",
        );
        let Some(step) = steps.into_iter().next() else {
            continue;
        };
        blocked.insert(h.host.clone());
        failures.push(RunFailure::plain(
            "blocked_host",
            h.requests,
            format!("the proxy blocked {}", describe_host(h)),
            Some(step),
        ));
    }
    for h in hosts.iter().filter(|h| h.mode == "pass") {
        outcome.notes.push(format!(
            "{} was passed to the real host {} while the command ran (proxy.unmapped: pass)",
            h.host,
            times(h.requests)
        ));
    }
    // Hosts the command reached (blocked ones already have a failure).
    let reached: HashSet<&str> = hosts
        .iter()
        .map(|h| h.host.as_str())
        .filter(|h| !blocked.contains(*h))
        .collect();
    match crate::hosts::project_hosts_async(session.config.clone(), claimed).await {
        Ok(scan) => {
            let called: Vec<String> = scan
                .unclaimed
                .iter()
                .filter(|h| h.fails_verify() && reached.contains(h.host.as_str()))
                .map(|h| h.host.clone())
                .collect();
            if !called.is_empty() {
                failures.push(RunFailure::plain(
                    "unclaimed_hosts",
                    called.len() as u64,
                    format!(
                        "the command called {} that a bundled description or catalog example serves and no mock lists: {}",
                        plural(called.len(), "host"),
                        name_some(&called)
                    ),
                    scan.summary_without(&blocked),
                ));
            } else if scan
                .unclaimed
                .iter()
                .any(crate::hosts::UnclaimedHost::fails_verify)
                && let Some(summary) = scan.summary_without(&blocked)
            {
                // Not called in this run: worth knowing, not a failure.
                outcome.notes.push(summary);
            }
        }
        Err(error) => outcome
            .notes
            .push(format!("the project host scan did not run: {error:#}")),
    }
    if let Some(args) = checks {
        let window = window_report(&entries, hosts, &metrics_before, &metrics_after);
        for f in report::check(&window, args)? {
            if ALWAYS.contains(&f.check.as_str()) {
                continue;
            }
            failures.push(RunFailure::plain(
                "check",
                1,
                format!("{}: {}", f.check, f.detail),
                None,
            ));
        }
    }
    if failures.len() > MAX_FAILURES {
        outcome.more_failures = failures.len() - MAX_FAILURES;
        failures.truncate(MAX_FAILURES);
    }
    outcome.ok = command_ok && failures.is_empty();
    outcome.failures = failures;
    outcome.command = command;
    outcome.serve = serve_run;
    Ok(outcome)
}

impl RunFailure {
    /// A failure tied to no mock or request.
    fn plain(kind: &str, count: u64, detail: String, next_step: Option<String>) -> Self {
        Self {
            kind: kind.into(),
            workspace: None,
            mock: None,
            operation: None,
            status: None,
            source: None,
            count,
            detail,
            request_id: None,
            next_step,
        }
    }
}

/// Failure kinds that do not point at the mock: the app's own server or
/// dependencies.
const NOT_THE_MOCK: &[&str] = &["serve_failed", "missing_dependencies"];

/// A `missing_dependencies` failure for each failed command (the serve
/// command first, when it failed to start or exited non-zero beside a
/// failing command: `serve` is its run and whether it failed to start)
/// whose output shows a dependency that is not installed, with the
/// project's install command ([`crate::missing_deps`]). The same
/// dependency is named once.
fn missing_dependencies(
    root: &std::path::Path,
    cwd: &std::path::Path,
    serve: Option<(&ServeRun, bool)>,
    command: &CommandRun,
) -> Vec<RunFailure> {
    use crate::missing_deps;
    let command_failed = !command.not_run
        && !command.timed_out
        && !command.interrupted
        && command.exit_code.is_some_and(|c| c != 0);
    let mut sources = Vec::new();
    // A serve command that exited while passing tests ran is only a note.
    if let Some((run, failed)) = serve
        && (failed || (command_failed && run.exit_code.is_some_and(|c| c != 0)))
    {
        sources.push((
            "The app's server",
            SERVE_OUTPUT,
            run.command.as_str(),
            run.output_tail.as_str(),
        ));
    }
    if command_failed {
        sources.push((
            "The tests",
            "the command's output",
            command.command.as_str(),
            command.output_tail.as_str(),
        ));
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (who, place, line, tail) in sources {
        let dir = missing_deps::command_dir(cwd, line);
        let Some(missing) = missing_deps::find(tail, &[&dir, root]) else {
            continue;
        };
        let install = missing
            .ecosystem
            .filter(|_| !missing.unknown_command())
            .and_then(|e| missing_deps::install(&dir, root, e));
        if !seen.insert((missing.name.clone(), install.clone())) {
            continue;
        }
        out.push(RunFailure::plain(
            "missing_dependencies",
            1,
            format!("{} in {place}", missing.describe()),
            Some(missing_deps::step(who, &missing, install.as_ref())),
        ));
    }
    out
}

/// Where a `missing_dependencies` failure found in the serve command's
/// output says it was found.
const SERVE_OUTPUT: &str = "the serve command's output";

/// The `verify` checks `--run` always makes, which are not repeated as
/// `check` failures.
const ALWAYS: &[&str] = &[
    "no-fallbacks",
    "no-blocked",
    "no-unmatched",
    "no-errors",
    "no-unclaimed-hosts",
];

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

fn times(n: u64) -> String {
    match n {
        1 => "once".into(),
        n => format!("{n} times"),
    }
}

/// Where the request logs stood before a command `mockingbird run` runs, to say after
/// it ([`world_review_after`]) which called operations leave world gaps open.
#[derive(Debug)]
pub struct ReviewWindow {
    mocks: Vec<MockRef>,
    baseline: Baseline,
}

/// The request logs of the server at `admin` before a command runs; `None` when its
/// mocks cannot be listed.
pub async fn world_review_before(admin: &str) -> Option<ReviewWindow> {
    let mocks = list_mocks(admin, None).await.ok()?;
    let baseline = newest_ids(admin, None, &mocks).await;
    Some(ReviewWindow { mocks, baseline })
}

/// The `verify --run` world notes for the requests logged since `window` was taken.
pub async fn world_review_after(admin: &str, window: &ReviewWindow) -> Vec<String> {
    settle(admin, None).await;
    match window_entries(admin, None, &window.mocks, &window.baseline).await {
        Ok(read) => world_review_notes(admin, None, &read.entries).await,
        Err(_) => Vec::new(),
    }
}

/// One note per mock the command called whose world, for the operations its log shows
/// called, leaves gaps open (not advisory ones): what they are about, and how to review
/// them. The gaps are the ones `get_world {mock, called: true}` lists, so the note goes
/// once that answer lists none. A mock whose world cannot be read is noted so.
async fn world_review_notes(
    admin: &str,
    token: Option<&str>,
    entries: &[(MockRef, Value)],
) -> Vec<String> {
    let called: BTreeSet<&MockRef> = entries
        .iter()
        .filter(|(_, e)| e["operation_id"].is_string())
        .map(|(m, _)| m)
        .collect();
    let mut notes = Vec::new();
    for m in called {
        let name = &m.name;
        let Ok(view) = get(
            admin,
            token,
            Some(&m.workspace),
            &["mocks", name, "contracts", "active", "world"],
            &[("called", "true".to_string())],
        )
        .await
        else {
            notes.push(format!(
                "the world of {name} could not be read, so whether the called operations leave gaps open was not checked"
            ));
            continue;
        };
        let mut gaps = 0;
        let mut about = Vec::new();
        for gap in view["gaps"].as_array().into_iter().flatten() {
            if gap["advisory"].as_bool() == Some(true) {
                continue;
            }
            gaps += 1;
            let what = match (gap["operation"].as_str(), gap["type"].as_str()) {
                (Some(op), _) => op.to_owned(),
                (None, Some(t)) => format!("type {t}"),
                (None, None) => continue,
            };
            if !about.contains(&what) {
                about.push(what);
            }
        }
        if gaps == 0 {
            continue;
        }
        let on = match about.is_empty() {
            true => String::new(),
            false => format!(" (on {})", name_some(&about)),
        };
        notes.push(format!(
            "World not reviewed for the called operations of {name}: {} open{on}. Read get_world {{mock: {name:?}, called: true}} (CLI: mockingbird world show {name} --called) and work through them as the mockingbird-world skill says before handing the mock back.",
            plural(gaps, "gap"),
        ));
    }
    notes
}

/// The first three of `items`, and how many more.
fn name_some(items: &[String]) -> String {
    let mut s = items.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > 3 {
        let _ = write!(s, ", {} more", items.len() - 3);
    }
    s
}

fn describe_host(h: &HostReport) -> String {
    if h.sibling_of.is_empty() {
        format!("{} {}", h.host, times(h.requests))
    } else {
        format!(
            "{} {} (same domain as mocked {})",
            h.host,
            times(h.requests),
            h.sibling_of.join(", ")
        )
    }
}

/// Reads `segments` of workspace `ws` (or none) from the Admin API.
async fn get(
    admin: &str,
    token: Option<&str>,
    ws: Option<&str>,
    segments: &[&str],
    query: &[(&str, String)],
) -> anyhow::Result<Value> {
    client::api(admin, token, ws, Method::GET, segments, query, None)
        .await
        .map(Option::unwrap_or_default)
        .map_err(|e| match e {
            ClientError::Api { status, body } => anyhow!("Admin API {status}: {body}"),
            ClientError::Usage(m) => anyhow!(m),
            ClientError::Incomplete { message, .. } => anyhow!(message),
            ClientError::Other(e) => e,
        })
}

/// Every mock in every workspace the caller sees (the default workspace
/// alone when workspaces cannot be listed).
async fn list_mocks(admin: &str, token: Option<&str>) -> anyhow::Result<Vec<MockRef>> {
    let mut workspaces: Vec<String> = get(admin, token, None, &["workspaces"], &[])
        .await
        .ok()
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|w| w["name"].as_str().map(str::to_owned))
        .collect();
    if workspaces.is_empty() {
        workspaces.push(mb_store::DEFAULT_WORKSPACE.to_owned());
    }
    let mut out = Vec::new();
    for ws in workspaces {
        let mocks = get(admin, token, Some(&ws), &["mocks"], &[]).await?;
        out.extend(
            mocks
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["name"].as_str())
                .map(|name| MockRef {
                    workspace: ws.clone(),
                    name: name.to_owned(),
                }),
        );
    }
    out.sort();
    Ok(out)
}

/// Where each mock's request log stood: its newest id (`None` for an empty
/// log), and the mocks whose log could not be read.
#[derive(Debug, Default, PartialEq, Eq)]
struct Baseline {
    newest: HashMap<MockRef, Option<i64>>,
    unread: Vec<MockRef>,
}

/// The newest request-log id of each mock.
async fn newest_ids(admin: &str, token: Option<&str>, mocks: &[MockRef]) -> Baseline {
    let mut out = Baseline::default();
    for m in mocks {
        let page = get(
            admin,
            token,
            Some(&m.workspace),
            &["mocks", &m.name, "requests"],
            &[("fields", "summary".into()), ("limit", "1".into())],
        )
        .await;
        match page {
            Ok(page) => {
                out.newest.insert(m.clone(), page[0]["id"].as_i64());
            }
            Err(_) => out.unread.push(m.clone()),
        }
    }
    out
}

/// Waits until the request log stops growing (the recorder writes off the
/// request path): two reads in a row agree, or [`SETTLE`] passed.
async fn settle(admin: &str, token: Option<&str>) {
    let deadline = Instant::now() + SETTLE;
    let mut last: Option<Baseline> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mocks = list_mocks(admin, token).await.unwrap_or_default();
        let now = newest_ids(admin, token, &mocks).await;
        if last.as_ref() == Some(&now) || Instant::now() >= deadline {
            return;
        }
        last = Some(now);
    }
}

/// The window's request-log entries.
#[derive(Default)]
struct Entries {
    /// Newest first per mock, at most [`MAX_REQUESTS`].
    entries: Vec<(MockRef, Value)>,
    /// Whether the cap stopped the reading.
    truncated: bool,
    /// Mocks whose log no longer reaches back to the window's start
    /// (`observability.request_log_retention`).
    retention: Vec<MockRef>,
}

/// The request-log entries (summaries) newer than `baseline`. A mock whose
/// baseline could not be read is not read; a mock with no baseline (created
/// while the command ran) is read whole.
async fn window_entries(
    admin: &str,
    token: Option<&str>,
    mocks: &[MockRef],
    baseline: &Baseline,
) -> anyhow::Result<Entries> {
    const PAGE: usize = 500;
    let mut out = Entries::default();
    for m in mocks {
        if baseline.unread.contains(m) {
            continue;
        }
        let floor = baseline.newest.get(m).copied().flatten();
        let mut before: Option<i64> = None;
        loop {
            let left = MAX_REQUESTS - out.entries.len();
            if left == 0 {
                out.truncated = true;
                return Ok(out);
            }
            let mut query = vec![
                ("fields", "summary".to_string()),
                ("limit", PAGE.min(left).to_string()),
            ];
            if let Some(id) = before {
                query.push(("before", id.to_string()));
            }
            let page = get(
                admin,
                token,
                Some(&m.workspace),
                &["mocks", &m.name, "requests"],
                &query,
            )
            .await
            .map_err(|e| e.context(format!("reading the request log of {}", m.name)))?;
            let entries = page.as_array().cloned().unwrap_or_default();
            let short = entries.len() < PAGE.min(left);
            let mut reached = false;
            for e in entries {
                let Some(id) = e["id"].as_i64() else {
                    continue;
                };
                if floor.is_some_and(|floor| id <= floor) {
                    reached = true;
                    break;
                }
                before = Some(id);
                out.entries.push((m.clone(), e));
            }
            if reached {
                break;
            }
            if short || before.is_none() {
                // The log ended before the baseline entry: retention dropped
                // the window's oldest requests (and the baseline with them).
                if floor.is_some() {
                    out.retention.push(m.clone());
                }
                break;
            }
            if out.entries.len() >= MAX_REQUESTS {
                // The cap is reached with this mock's window not read to
                // its start: older entries may be left.
                out.truncated = true;
                return Ok(out);
            }
        }
    }
    Ok(out)
}

/// How many request-log records of each mock the recorder dropped (its
/// queue was full), from a Prometheus text exposition.
fn dropped(metrics: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for (labels, value) in report::samples(metrics, DROPPED_METRIC) {
        *out.entry(labels.get("mock").cloned().unwrap_or_default())
            .or_default() += value;
    }
    out
}

/// The recorder's counter of request-log records it dropped.
const DROPPED_METRIC: &str = "mb_request_log_dropped_total";

/// The growth of the proxy's unmapped-host counts from `before` to
/// `after`, per host and mode.
fn host_delta(before: &[HostReport], after: &[HostReport]) -> Vec<HostReport> {
    let base: HashMap<(&str, &str), u64> = before
        .iter()
        .map(|h| ((h.host.as_str(), h.mode.as_str()), h.requests))
        .collect();
    after
        .iter()
        .filter_map(|h| {
            let grown = h.requests.saturating_sub(
                base.get(&(h.host.as_str(), h.mode.as_str()))
                    .copied()
                    .unwrap_or(0),
            );
            (grown > 0).then(|| HostReport {
                host: h.host.clone(),
                mode: h.mode.clone(),
                requests: grown,
                sibling_of: h.sibling_of.clone(),
            })
        })
        .collect()
}

/// The window as a [`report::Report`], for the assertions given beside
/// `--run`.
fn window_report(
    entries: &[(MockRef, Value)],
    hosts: Vec<HostReport>,
    metrics_before: &str,
    metrics_after: &str,
) -> report::Report {
    let mut by_mock: BTreeMap<&MockRef, report::MockReport> = BTreeMap::new();
    for (m, e) in entries {
        let r = by_mock.entry(m).or_insert_with(|| report::MockReport {
            mock: m.name.clone(),
            ..report::MockReport::default()
        });
        report::add_entry(r, e);
    }
    let mut out = report::Report {
        unmapped_hosts: hosts,
        ..report::Report::default()
    };
    for r in by_mock.into_values() {
        out.totals.merge(&r.counts);
        out.mocks.push(r);
    }
    let before = report::snapshot_answers(metrics_before);
    for (result, n) in report::snapshot_answers(metrics_after) {
        let grown = n.saturating_sub(before.get(&result).copied().unwrap_or(0));
        if grown > 0 {
            out.snapshot.insert(result, grown);
        }
    }
    out
}

/// Where a 4xx/5xx answer came from, from its log entry's `source`: a
/// problem the mock answered itself (a scenario, a forced status, a world
/// rule) has none.
fn source_label(source: Option<&str>) -> String {
    match source {
        None => "no source: a problem the mock answered itself (a scenario, a forced status or a world rule)".into(),
        Some("rejected") => "source rejected: the request failed the contract's validation".into(),
        Some("state") => "source state: the session's records or a world rule (refuse, an unknown id)".into(),
        Some(s @ ("llm" | "patched" | "repaired")) => format!("source {s}: generated"),
        Some(s) => format!("source {s}"),
    }
}

/// Whether a 4xx/5xx answer is a failure whatever the command's outcome:
/// a 5xx, or a request the contract's validation rejected.
fn always_fails(status: u16, source: Option<&str>) -> bool {
    status >= 500 || source == Some("rejected")
}

/// The failures the logged requests show, each request counted once:
/// unmatched routes, then fallbacks, then 4xx/5xx answers, grouped (by
/// mock, route or operation, status and source or reason), the most
/// frequent first within each kind. A 4xx that matched an operation, is not
/// a validation rejection and came while the command passed is taken as
/// expected: it is returned as a note.
fn request_failures(
    entries: &[(MockRef, Value)],
    command_ok: bool,
) -> (Vec<RunFailure>, Vec<String>) {
    // (kind, mock, operation or route, status, source or reason) → (count, newest id)
    type Key = (u8, MockRef, String, u16, Option<String>);
    let mut groups: BTreeMap<Key, (u64, Option<i64>)> = BTreeMap::new();
    for (m, e) in entries {
        let status = u16::try_from(e["status"].as_u64().unwrap_or(0)).unwrap_or(0);
        let op = e["operation_id"].as_str();
        let source = e["source"].as_str();
        let id = e["id"].as_i64();
        let mut add = |kind: u8, what: String, status: u16, extra: Option<String>| {
            let entry = groups
                .entry((kind, m.clone(), what, status, extra))
                .or_insert((0, None));
            entry.0 += 1;
            // Entries come newest first: keep the first id seen.
            if entry.1.is_none() {
                entry.1 = id;
            }
        };
        if op.is_none() && matches!(status, 404 | 405) {
            let route = format!(
                "{} {}",
                e["method"].as_str().unwrap_or("?"),
                e["path"].as_str().unwrap_or("?")
            );
            add(0, route, status, None);
            continue;
        }
        let op = op.unwrap_or("-").to_owned();
        let reason = e["fallback_reason"].as_str().unwrap_or("unknown");
        if source == Some("fallback") && !reason.starts_with(mb_engine::OFFLINE_REASON) {
            add(1, op, status, Some(reason.to_owned()));
        } else if status >= 400 {
            let expected = command_ok && !always_fails(status, source);
            add(
                if expected { 3 } else { 2 },
                op,
                status,
                source.map(str::to_owned),
            );
        }
    }
    let mut out: Vec<(u8, u64, RunFailure)> = Vec::new();
    let mut notes = Vec::new();
    for ((kind, m, what, status, extra), (count, id)) in groups {
        let workspace = (m.workspace != mb_store::DEFAULT_WORKSPACE).then(|| m.workspace.clone());
        let failure = match kind {
            0 => RunFailure {
                kind: "unmatched_route".into(),
                workspace,
                detail: format!(
                    "{}: {what} matched no operation of its contract (answered {status}, {})",
                    m.name,
                    times(count)
                ),
                next_step: Some(unmatched_step(&m.name, &what)),
                mock: Some(m.name),
                operation: None,
                status: Some(status),
                source: None,
                count,
                request_id: id,
            },
            1 => RunFailure {
                kind: "fallback".into(),
                workspace,
                detail: format!(
                    "{}: {what} fell back to the faker ({}): {}",
                    m.name,
                    times(count),
                    report::shorten(extra.as_deref().unwrap_or("unknown"), 300)
                ),
                next_step: Some(fallback_step(&m.name, &what)),
                mock: Some(m.name),
                operation: Some(what),
                status: Some(status),
                source: Some("fallback".into()),
                count,
                request_id: id,
            },
            2 => RunFailure {
                kind: "error_status".into(),
                workspace,
                detail: format!(
                    "{}: {what} answered {status} ({}; {})",
                    m.name,
                    times(count),
                    source_label(extra.as_deref())
                ),
                next_step: None,
                mock: Some(m.name),
                operation: Some(what),
                status: Some(status),
                source: extra,
                count,
                request_id: id,
            },
            _ => {
                notes.push(format!(
                    "{}: {what} answered {status} ({}; {}); the command passed, so it is taken as a status the test expected",
                    m.name,
                    times(count),
                    source_label(extra.as_deref())
                ));
                continue;
            }
        };
        out.push((kind, count, failure));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    (out.into_iter().map(|(_, _, f)| f).collect(), notes)
}

fn unmatched_step(mock: &str, route: &str) -> String {
    format!(
        "Mock `{mock}` has no operation for {route}: its contract lacks the route the app calls. Import the API's fuller contract (import_contract; or init_example with full: true where list_examples shows full_spec) or add the operation (edit_contract), then run verify --run again."
    )
}

fn fallback_step(mock: &str, op: &str) -> String {
    format!(
        "Mock `{mock}` answered {op} with faker filler, not a world answer: fix what the reason names (the LLM profile or its key with test_llm, the world description with get_world and patch_world, or the contract), then run verify --run again."
    )
}

fn error_step(mock: &str, op: &str, status: u16, source: Option<&str>, id: Option<i64>) -> String {
    let request = match id {
        Some(id) => format!(
            " and this request (get_request {{mock: {mock}, id: {id}}}; CLI `mockingbird api GET /mocks/{mock}/requests/{id}`)"
        ),
        None => String::new(),
    };
    let target = if op == "-" {
        "a request that matched no operation".to_owned()
    } else {
        format!("operation {op}")
    };
    // A 4xx the failing command may have expected (a test of a missing
    // record or a refusal).
    let expected = if always_fails(status, source) {
        ""
    } else {
        "If the test expected this status, ignore it. "
    };
    format!(
        "Mock `{mock}` answered {status} to {target}. {expected}If the app's call is right, the mock is wrong: check its world (get_world {{mock: {mock}}}){request}, then fix the world description (patch_world) or the contract, and run verify --run again."
    )
}

/// Reads the newest request of each `error_status` failure (at most
/// [`MAX_DETAILS`]) for the problem's detail, and sets its `next_step`.
async fn add_details(admin: &str, token: Option<&str>, failures: &mut [RunFailure]) {
    let mut read = 0;
    for f in failures.iter_mut().filter(|f| f.kind == "error_status") {
        let mock = f.mock.clone().unwrap_or_default();
        let op = f.operation.clone().unwrap_or_default();
        f.next_step = Some(error_step(
            &mock,
            &op,
            f.status.unwrap_or(0),
            f.source.as_deref(),
            f.request_id,
        ));
        let Some(id) = f.request_id else {
            continue;
        };
        if read >= MAX_DETAILS {
            continue;
        }
        read += 1;
        let ws = f
            .workspace
            .clone()
            .unwrap_or_else(|| mb_store::DEFAULT_WORKSPACE.to_owned());
        let id = id.to_string();
        let Ok(entry) = get(
            admin,
            token,
            Some(&ws),
            &["mocks", &mock, "requests", &id],
            &[],
        )
        .await
        else {
            continue;
        };
        if let Some(detail) = problem_detail(&entry["response"]) {
            let _ = write!(f.detail, ": {detail}");
        }
    }
}

/// The detail of a recorded response body: a problem's `detail` (with its
/// `title`), a common error field, else the body itself, shortened.
fn problem_detail(body: &Value) -> Option<String> {
    if body.is_null() {
        return None;
    }
    let text = |v: &Value| v.as_str().map(str::to_owned);
    let found = text(&body["detail"])
        .map(|d| match text(&body["title"]) {
            Some(t) if !d.contains(&t) => format!("{t}: {d}"),
            _ => d,
        })
        .or_else(|| text(&body["title"]))
        .or_else(|| text(&body["message"]))
        .or_else(|| text(&body["error"]["message"]))
        .or_else(|| text(&body["error_description"]))
        .or_else(|| text(&body["error"]))
        .unwrap_or_else(|| body.to_string());
    Some(report::shorten(&found, 300))
}

/// The last lines of a command's output.
struct Tail {
    lines: VecDeque<String>,
    max: usize,
    truncated: bool,
    /// The serve command's readiness line, and whether a line matched it.
    ready: Option<jsonschema::Validator>,
    matched: bool,
}

impl Default for Tail {
    fn default() -> Self {
        Self::new(TAIL_LINES)
    }
}

impl Tail {
    fn new(max: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            max,
            truncated: false,
            ready: None,
            matched: false,
        }
    }

    fn push(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let text = strip_ansi(&text);
        let text = text.trim_end_matches(['\n', '\r']);
        if !self.matched
            && let Some(ready) = &self.ready
        {
            self.matched = ready.is_valid(&Value::String(text.to_owned()));
        }
        if text.chars().count() > LINE_CHARS {
            self.truncated = true;
        }
        self.lines.push_back(report::shorten(text, LINE_CHARS));
        if self.lines.len() > self.max {
            self.lines.pop_front();
            self.truncated = true;
        }
    }

    fn text(&self) -> String {
        self.lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A matcher for the lines of `pattern` (`--serve-ready`): an ECMA 262
/// regular expression, found anywhere in the line (a JSON Schema
/// `pattern`).
fn line_matcher(pattern: &str, arg: &str) -> anyhow::Result<jsonschema::Validator> {
    jsonschema::validator_for(&serde_json::json!({"type": "string", "pattern": pattern}))
        .map_err(|e| anyhow!("{arg} {pattern:?} is not a valid regular expression: {e}"))
}

/// `text` without ANSI escape sequences (colours, cursor moves), as a
/// terminal would show it.
fn strip_ansi(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\u{1b}') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: parameters, then one final byte in @..~.
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ESC \.
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Reads `stream` into `tail` line by line (a line past [`LINE_CHARS`] is
/// cut there), echoing its bytes as they come on stderr when `echo` is set.
async fn read_lines(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    tail: Arc<Mutex<Tail>>,
    echo: bool,
) {
    use std::io::Write as _;
    let mut buf = [0u8; 8192];
    let mut line: Vec<u8> = Vec::new();
    let flush = |line: &mut Vec<u8>| {
        if let Ok(mut t) = tail.lock() {
            t.push(line);
        }
        line.clear();
    };
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if echo {
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(&buf[..n]);
            let _ = err.flush();
        }
        for &b in &buf[..n] {
            if b == b'\n' {
                flush(&mut line);
            } else if line.len() < LINE_CHARS * 4 {
                line.push(b);
            }
        }
    }
    if !line.is_empty() {
        flush(&mut line);
    }
}

/// Stops every command `verify --run` is running (the MCP server calls it when its
/// client goes away): each is ended as if interrupted, and still reported.
pub fn cancel_running() {
    let _ = cancel_channel().send(true);
}

fn cancel_channel() -> &'static tokio::sync::watch::Sender<bool> {
    static CANCEL: std::sync::OnceLock<tokio::sync::watch::Sender<bool>> =
        std::sync::OnceLock::new();
    CANCEL.get_or_init(|| tokio::sync::watch::channel(false).0)
}

/// Resolves once the run is asked to stop: Ctrl-C or SIGTERM to the CLI
/// (with `interactive`), or [`cancel_running`].
async fn stop_requested(interactive: bool) {
    let mut cancel = cancel_channel().subscribe();
    let cancelled = async move {
        // A closed channel never cancels.
        if cancel.wait_for(|c| *c).await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    let interrupt = async {
        if interactive {
            crate::server::shutdown_signal().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        () = cancelled => {}
        () = interrupt => {}
    }
}

/// `command` run by the shell from `cwd` with `env`, no stdin, its output
/// piped, in a process group of its own (so what it starts can be stopped
/// with it).
fn shell(
    command: &str,
    cwd: &std::path::Path,
    env: &BTreeMap<String, String>,
) -> tokio::process::Command {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("cmd");
        // Passed as written: cmd.exe does its own parsing of the line.
        c.arg("/C").raw_arg(command);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c.process_group(0);
        c
    };
    cmd.current_dir(cwd)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// Starts reading `child`'s stdout and stderr into `tail`.
fn read_output(
    child: &mut tokio::process::Child,
    tail: &Arc<Mutex<Tail>>,
    echo: bool,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(tokio::spawn(read_lines(out, tail.clone(), echo)));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(tokio::spawn(read_lines(err, tail.clone(), echo)));
    }
    readers
}

/// Waits up to [`DRAIN`] for each reader; returns whether one was still
/// held (a process left running holds the pipes).
async fn drain(readers: Vec<tokio::task::JoinHandle<()>>) -> bool {
    let mut held = false;
    for reader in readers {
        let abort = reader.abort_handle();
        if tokio::time::timeout(DRAIN, reader).await.is_err() {
            abort.abort();
            held = true;
        }
    }
    held
}

/// Runs `opts.command` from `cwd` with `env`, within `opts.timeout`. When
/// the command ends, what it left running in its process group is stopped
/// too.
async fn execute(
    opts: &Options,
    cwd: &std::path::Path,
    env: &BTreeMap<String, String>,
) -> anyhow::Result<CommandRun> {
    let mut run = CommandRun {
        command: opts.command.clone(),
        cwd: cwd.display().to_string(),
        ..CommandRun::default()
    };
    let mut cmd = shell(&opts.command, cwd, env);
    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            run.exit_code = Some(127);
            run.output_tail = format!("cannot run the command: {error}");
            return Ok(run);
        }
    };
    let pid = child.id();
    let tail = Arc::new(Mutex::new(Tail::default()));
    let readers = read_output(&mut child, &tail, opts.interactive);
    let deadline = tokio::time::sleep(opts.timeout);
    tokio::pin!(deadline);
    let stop = stop_requested(opts.interactive);
    tokio::pin!(stop);
    let status = tokio::select! {
        status = child.wait() => Some(status?),
        () = &mut deadline => {
            run.timed_out = true;
            None
        }
        () = &mut stop => {
            run.interrupted = true;
            None
        }
    };
    let status = match status {
        Some(status) => Some(status),
        None => {
            if let Some(pid) = pid {
                stop_group(pid, false);
            }
            match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                Ok(status) => status.ok(),
                Err(_) => {
                    if let Some(pid) = pid {
                        stop_group(pid, true);
                    }
                    let _ = child.kill().await;
                    child.wait().await.ok()
                }
            }
        }
    };
    run.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    // What the command left running in its group (a server it started in
    // the background) is stopped with it.
    if let Some(pid) = pid {
        stop_group(pid, false);
    }
    // A process that ignored TERM still holds the output pipes.
    if drain(readers).await
        && let Some(pid) = pid
    {
        stop_group(pid, true);
    }
    if !run.timed_out && !run.interrupted {
        run.exit_code = status.map(exit_code);
    }
    if let Ok(t) = tail.lock() {
        run.output_tail = t.text();
        run.output_truncated = t.truncated;
    }
    Ok(run)
}

/// The serve command, running.
struct Served {
    child: tokio::process::Child,
    pid: Option<u32>,
    tail: Arc<Mutex<Tail>>,
    readers: Vec<tokio::task::JoinHandle<()>>,
    /// Whether [`Served::stop`] ran; until then, dropping it kills its
    /// process group (an error or a panic on the way).
    stopped: bool,
}

impl Drop for Served {
    fn drop(&mut self) {
        if !self.stopped
            && let Some(pid) = self.pid
        {
            stop_group(pid, true);
        }
    }
}

impl Served {
    /// Stops its process group (TERM, then KILL after [`SERVE_GRACE`]) and
    /// fills `run` with its exit code (when it ended on its own) and output.
    async fn stop(mut self, run: &mut ServeRun) {
        let ended = self.child.try_wait().ok().flatten();
        let mut leader_ended = ended.is_some();
        if let Some(pid) = self.pid {
            // Signal the group first, then reap its leader.
            stop_group(pid, false);
            let deadline = Instant::now() + SERVE_GRACE;
            leader_ended |= tokio::time::timeout(SERVE_GRACE, self.child.wait())
                .await
                .is_ok();
            // The rest of its group (`npm start` runs node below it).
            while group_alive(pid) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            // Past the grace: what is left is killed. Without a group check
            // (Windows), a leader that did not end is.
            if group_alive(pid) || !leader_ended {
                stop_group(pid, true);
            }
        }
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await;
        }
        self.stopped = true;
        // A process that kept the output pipes past TERM (on Windows, one
        // the leader started) is killed with the group.
        if drain(std::mem::take(&mut self.readers)).await
            && let Some(pid) = self.pid
        {
            stop_group(pid, true);
        }
        run.stopped = true;
        run.exit_code = ended.map(exit_code);
        if let Ok(t) = self.tail.lock() {
            run.output_tail = t.text();
            run.output_truncated = t.truncated;
        }
    }
}

/// How starting the serve command ended.
enum ServeStart {
    Ready(Served),
    /// It exited, never became ready or could not start: it is stopped.
    Failed(RunFailure),
    /// The run was asked to stop while waiting: it is stopped.
    Interrupted,
}

/// Starts `serve` from `cwd` with `env` and waits until it is ready: `port`
/// accepts connections and/or a line matches `serve.ready`; with neither,
/// it must still run after [`SERVE_SETTLE`]. `port` is `(port, detected)`;
/// `own` are Mockingbird's own ports, named when the port is one of them.
#[allow(clippy::too_many_arguments)]
async fn start_serve(
    opts: &Options,
    serve: &ServeOptions,
    port: Option<(u16, bool)>,
    own: &[(u16, &str)],
    cwd: &std::path::Path,
    env: &BTreeMap<String, String>,
    run: &mut ServeRun,
) -> ServeStart {
    let mut by = Vec::new();
    if let Some((p, detected)) = port {
        by.push(if detected {
            format!("port {p} (detected)")
        } else {
            format!("port {p}")
        });
    }
    if let Some(pattern) = &serve.ready {
        by.push(format!("a line matching {pattern:?}"));
    }
    if by.is_empty() {
        by.push(format!("running for {}s", SERVE_SETTLE.as_secs()));
    }
    run.ready_by = by.join(" and ");
    let fail = |detail: String, next: String| {
        ServeStart::Failed(RunFailure::plain("serve_failed", 1, detail, Some(next)))
    };
    if let Some((p, _)) = port
        && let Some((_, what)) = own.iter().find(|(o, _)| *o == p)
    {
        return fail(
            format!("port {p} is Mockingbird's own {what} in this config, not the app's"),
            format!(
                "The app cannot listen on port {p}: Mockingbird's server uses it. Give the port the app's server listens on (--serve-port, MCP serve_port), or move Mockingbird's {what} in the config, then run verify --run again."
            ),
        );
    }
    if let Some((p, _)) = port
        && port_open(p).await
    {
        return fail(
            format!(
                "port {p} already accepted connections before `{}` started: another process listens there, and the tests would reach it instead",
                serve.command
            ),
            format!(
                "Stop the process that listens on port {p} (one you started; check with `lsof -i :{p}`), then run verify --run again."
            ),
        );
    }
    let mut tail = Tail::new(SERVE_TAIL_LINES);
    if let Some(pattern) = &serve.ready {
        // Checked before the run started.
        tail.ready = line_matcher(pattern, ready_arg(opts)).ok();
    }
    let tail = Arc::new(Mutex::new(tail));
    let mut cmd = shell(&serve.command, cwd, env);
    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            return fail(
                format!("cannot run the serve command `{}`: {error}", serve.command),
                "Fix the serve command (it runs with sh -c), then run verify --run again.".into(),
            );
        }
    };
    let readers = read_output(&mut child, &tail, opts.interactive);
    let mut served = Served {
        pid: child.id(),
        child,
        tail: tail.clone(),
        readers,
        stopped: false,
    };
    let deadline = started + serve.timeout;
    let stop = stop_requested(opts.interactive);
    tokio::pin!(stop);
    let failure = loop {
        tokio::select! {
            () = &mut stop => {
                served.stop(run).await;
                return ServeStart::Interrupted;
            }
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        if let Ok(Some(status)) = served.child.try_wait() {
            break Some((
                format!(
                    "the serve command `{}` exited {} before it was ready ({} waited for)",
                    serve.command,
                    exit_code(status),
                    run.ready_by
                ),
                format!(
                    "Read serve.output_tail for why it exited, fix the serve command or install what it needs (it runs from {}; `cd <dir> && …` moves it), then run verify --run again.",
                    cwd.display()
                ),
            ));
        }
        let port_ok = match port {
            Some((p, _)) => port_open(p).await,
            None => true,
        };
        let line_ok = serve.ready.is_none() || tail.lock().is_ok_and(|t| t.matched);
        let settled = port.is_some() || serve.ready.is_some() || started.elapsed() >= SERVE_SETTLE;
        if port_ok && line_ok && settled {
            break None;
        }
        if Instant::now() >= deadline {
            break Some((
                format!(
                    "the serve command `{}` was not ready within {}s ({} waited for)",
                    serve.command,
                    serve.timeout.as_secs(),
                    run.ready_by
                ),
                "Read serve.output_tail: pass the port the app listens on (--serve-port, MCP serve_port) or a line it prints once ready (--serve-ready, serve_ready), or allow it longer (--serve-timeout, serve_timeout_secs), then run verify --run again.".into(),
            ));
        }
    };
    match failure {
        None => {
            run.ready = true;
            run.ready_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
            ServeStart::Ready(served)
        }
        Some((detail, next)) => {
            served.stop(run).await;
            fail(detail, next)
        }
    }
}

/// The readiness line's argument as the caller gave it.
fn ready_arg(opts: &Options) -> &'static str {
    if opts.interactive {
        "--serve-ready"
    } else {
        "serve_ready"
    }
}

/// Whether something accepts TCP connections on `port` of localhost (IPv4
/// or IPv6).
async fn port_open(port: u16) -> bool {
    for ip in [
        std::net::IpAddr::from([127, 0, 0, 1]),
        std::net::IpAddr::from(std::net::Ipv6Addr::LOCALHOST),
    ] {
        let connect = tokio::net::TcpStream::connect((ip, port));
        if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(500), connect).await {
            return true;
        }
    }
    false
}

/// The port of the pair the project's test detection proposes for the
/// serve command `serve` ([`crate::test_commands`]), if any.
async fn detected_port(root: &std::path::Path, serve: &str, own: &[(u16, &str)]) -> Option<u16> {
    let root = root.to_path_buf();
    let serve = serve.trim().to_owned();
    let avoid: Vec<u16> = own.iter().map(|(p, _)| *p).collect();
    tokio::task::spawn_blocking(move || {
        crate::test_commands::detect_all(&root, &avoid)
            .pairs
            .into_iter()
            .find(|p| p.serves(&serve))
            .and_then(|p| p.serve_port)
    })
    .await
    .ok()
    .flatten()
}

/// Whether any process of the group `pid` leads is left.
#[cfg(unix)]
fn group_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", "--", &format!("-{pid}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(unix))]
fn group_alive(_pid: u32) -> bool {
    false
}

/// Signals the process group `pid` leads (TERM, or KILL with `force`).
#[cfg(unix)]
fn stop_group(pid: u32, force: bool) {
    let signal = if force { "-KILL" } else { "-TERM" };
    let _ = std::process::Command::new("kill")
        .args([signal, "--", &format!("-{pid}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn stop_group(pid: u32, force: bool) {
    crate::daemon::terminate(pid, force);
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

/// `verify --run`'s text output: a status line, each failure with its
/// next step, and the output's tail when the command failed.
#[must_use]
pub fn text(o: &Outcome) -> String {
    let c = &o.command;
    let secs = Duration::from_millis(c.duration_ms).as_secs_f64();
    let ended = match (c.timed_out, c.interrupted, c.exit_code) {
        _ if c.not_run && c.interrupted => {
            "was not run (interrupted while the serve command started)".to_owned()
        }
        _ if c.not_run => "was not run (the serve command failed)".to_owned(),
        (true, _, _) => format!("was stopped at the timeout after {secs:.1}s"),
        (false, true, _) => format!("was interrupted after {secs:.1}s"),
        (false, false, Some(code)) => format!("exited {code} after {secs:.1}s"),
        (false, false, None) => format!("ended without an exit code after {secs:.1}s"),
    };
    let serve_failed = o.failures.iter().any(|f| f.kind == "serve_failed");
    let missing = o.failures.iter().any(|f| f.kind == "missing_dependencies");
    let not_the_mock = o
        .failures
        .iter()
        .filter(|f| NOT_THE_MOCK.contains(&f.kind.as_str()))
        .count();
    let mut s = format!(
        "verify --run {}: `{}` {ended}; {} logged{}{}; {}\n",
        if o.ok { "passed" } else { "failed" },
        c.command,
        plural(
            usize::try_from(o.window.requests).unwrap_or(usize::MAX),
            "request"
        ),
        if c.not_run { "" } else { " while it ran" },
        if o.window.truncated {
            format!(" (the newest {MAX_REQUESTS} examined)")
        } else {
            String::new()
        },
        match (o.failures.len() + o.more_failures).saturating_sub(not_the_mock) {
            0 if missing => "the app's dependencies are missing".to_owned(),
            0 if serve_failed => "the app's server failed to start".to_owned(),
            0 => "no failures point at the mock".to_owned(),
            n => format!(
                "{} point{} at the mock",
                plural(n, "failure"),
                if n == 1 { "s" } else { "" }
            ),
        }
    );
    if let Some(serve) = &o.serve {
        let state = match serve.ready_ms {
            Some(ms) => format!(
                "ready after {:.1}s ({})",
                Duration::from_millis(ms).as_secs_f64(),
                serve.ready_by
            ),
            None => format!("not ready ({})", serve.ready_by),
        };
        let _ = writeln!(
            s,
            "Served `{}`: {state}{}",
            serve.command,
            if serve.stopped { "; stopped" } else { "" }
        );
    }
    for f in &o.failures {
        let _ = writeln!(s, "  {}: {}", f.kind, f.detail);
        if let Some(step) = &f.next_step {
            let _ = writeln!(s, "    Next: {step}");
        }
    }
    if o.more_failures > 0 {
        let _ = writeln!(
            s,
            "  … {} more (--format json lists the first {MAX_FAILURES})",
            o.more_failures
        );
    }
    for note in &o.notes {
        let _ = writeln!(s, "Note: {note}");
    }
    if c.exit_code != Some(0) && !c.output_tail.is_empty() {
        let _ = writeln!(s, "Output (last lines):");
        for line in c.output_tail.lines() {
            let _ = writeln!(s, "  | {line}");
        }
    }
    if let Some(serve) = &o.serve
        && (!serve.ready || c.exit_code != Some(0) || serve.exit_code.is_some())
        && !serve.output_tail.is_empty()
    {
        let _ = writeln!(s, "Serve output (last lines):");
        for line in serve.output_tail.lines() {
            let _ = writeln!(s, "  | {line}");
        }
    }
    if !o.ok {
        s.push_str("Details: add --format json\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(name: &str) -> MockRef {
        MockRef {
            workspace: mb_store::DEFAULT_WORKSPACE.into(),
            name: name.into(),
        }
    }

    #[test]
    fn requests_group_into_failures_with_next_steps() {
        let entries = vec![
            (
                m("orders"),
                json!({"id": 9, "operation_id": null, "method": "GET", "path": "/nope", "status": 404, "source": null}),
            ),
            (
                m("orders"),
                json!({"id": 8, "operation_id": null, "method": "GET", "path": "/nope", "status": 404, "source": null}),
            ),
            (
                m("orders"),
                json!({"id": 7, "operation_id": "createOrder", "method": "POST", "path": "/orders", "status": 400, "source": "rejected"}),
            ),
            (
                m("orders"),
                json!({"id": 6, "operation_id": "getOrder", "status": 200, "source": "fallback", "fallback_reason": "LLM request timed out"}),
            ),
            (
                m("orders"),
                json!({"id": 5, "operation_id": "listOrders", "status": 200, "source": "fallback", "fallback_reason": mb_engine::OFFLINE_REASON}),
            ),
            (
                m("orders"),
                json!({"id": 4, "operation_id": "listOrders", "status": 200, "source": "llm"}),
            ),
        ];
        let mut entries = entries;
        entries.extend([
            (
                m("orders"),
                json!({"id": 3, "operation_id": "getOrder", "status": 404, "source": "state"}),
            ),
            (
                m("orders"),
                json!({"id": 2, "operation_id": "getOrder", "status": 503, "source": null}),
            ),
            (
                m("orders"),
                json!({"id": 1, "operation_id": "updateOrder", "status": 400, "source": "fallback", "fallback_reason": "LLM request timed out"}),
            ),
        ]);
        // The command passed: the 404 a world rule answered is expected.
        let (f, notes) = request_failures(&entries, true);
        let kinds: Vec<&str> = f.iter().map(|f| f.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "unmatched_route",
                "fallback",
                "fallback",
                "error_status",
                "error_status"
            ],
            "{f:?}"
        );
        assert_eq!(f[0].count, 2);
        assert_eq!(f[0].request_id, Some(9), "the newest request");
        assert!(
            f[0].detail.contains("GET /nope matched no operation"),
            "{}",
            f[0].detail
        );
        assert!(f[0].next_step.as_deref().unwrap().contains("edit_contract"));
        assert!(
            f[1].detail.contains("LLM request timed out"),
            "{}",
            f[1].detail
        );
        // Each request once: the 400 fallback is a fallback, not also a 4xx.
        assert_eq!(f.iter().map(|f| f.count).sum::<u64>(), 6, "{f:?}");
        let rejected = f
            .iter()
            .find(|f| f.status == Some(400) && f.kind == "error_status")
            .unwrap();
        assert_eq!(rejected.source.as_deref(), Some("rejected"));
        assert!(
            rejected.detail.contains("source rejected"),
            "{}",
            rejected.detail
        );
        let five = f.iter().find(|f| f.status == Some(503)).unwrap();
        assert!(
            five.detail
                .contains("no source: a problem the mock answered itself"),
            "{}",
            five.detail
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("getOrder answered 404") && notes[0].contains("source state"),
            "{}",
            notes[0]
        );
        // The command failed: the 404 is a failure the test may have expected.
        let (f, notes) = request_failures(&entries, false);
        assert!(notes.is_empty());
        let missing = f
            .iter()
            .find(|f| f.status == Some(404) && f.kind == "error_status")
            .unwrap();
        assert_eq!(missing.operation.as_deref(), Some("getOrder"));
        let step = error_step("orders", "getOrder", 404, Some("state"), Some(3));
        assert!(
            step.contains("If the test expected this status, ignore it.")
                && step.contains("get_world {mock: orders}")
                && step.contains("get_request {mock: orders, id: 3}"),
            "{step}"
        );
        let step = error_step("orders", "createOrder", 400, Some("rejected"), Some(7));
        assert!(!step.contains("expected this status"), "{step}");
    }

    #[test]
    fn host_counts_grow_only_within_the_window() {
        let h = |host: &str, mode: &str, n: u64| HostReport {
            host: host.into(),
            mode: mode.into(),
            requests: n,
            sibling_of: Vec::new(),
        };
        let delta = host_delta(
            &[
                h("a.example.org", "block", 3),
                h("b.example.org", "pass", 1),
            ],
            &[
                h("a.example.org", "block", 3),
                h("b.example.org", "pass", 4),
                h("c.example.org", "block", 2),
            ],
        );
        let got: Vec<(&str, u64)> = delta
            .iter()
            .map(|h| (h.host.as_str(), h.requests))
            .collect();
        assert_eq!(got, [("b.example.org", 3), ("c.example.org", 2)]);
    }

    #[test]
    fn problem_details_are_read_from_common_shapes() {
        assert_eq!(
            problem_detail(&json!({"title": "Bad Request", "detail": "/name: required"})).unwrap(),
            "Bad Request: /name: required"
        );
        assert_eq!(
            problem_detail(&json!({"error": {"message": "No such order"}})).unwrap(),
            "No such order"
        );
        assert_eq!(problem_detail(&json!({"code": 1})).unwrap(), "{\"code\":1}");
        assert!(problem_detail(&Value::Null).is_none());
    }

    #[test]
    fn the_tail_keeps_the_last_short_lines() {
        let mut t = Tail::default();
        for i in 0..(TAIL_LINES + 5) {
            t.push(format!("line {i}\n").as_bytes());
        }
        t.push("x".repeat(LINE_CHARS * 2).as_bytes());
        assert!(t.truncated);
        assert_eq!(t.lines.len(), TAIL_LINES);
        assert!(t.text().starts_with("line 6\n"), "{}", t.text());
        assert!(t.lines.back().unwrap().chars().count() <= LINE_CHARS + 1);
    }

    #[test]
    fn text_names_the_command_failures_and_tail() {
        let o = Outcome {
            ok: false,
            command: CommandRun {
                command: "npm test".into(),
                exit_code: Some(1),
                duration_ms: 1500,
                output_tail: "FAIL orders.test.js".into(),
                ..CommandRun::default()
            },
            window: Window {
                requests: 3,
                ..Window::default()
            },
            failures: request_failures(
                &[(
                    m("orders"),
                    json!({"id": 1, "operation_id": null, "method": "GET", "path": "/nope", "status": 404}),
                )],
                false,
            )
            .0,
            ..Outcome::default()
        };
        let s = text(&o);
        assert!(
            s.starts_with("verify --run failed: `npm test` exited 1 after 1.5s; 3 requests logged while it ran; 1 failure points at the mock\n"),
            "{s}"
        );
        assert!(s.contains("  unmatched_route: orders: GET /nope"), "{s}");
        assert!(
            s.contains("    Next: Mock `orders` has no operation"),
            "{s}"
        );
        assert!(s.contains("  | FAIL orders.test.js"), "{s}");
    }

    #[test]
    fn the_serve_ready_pattern_is_a_regex_found_anywhere_in_a_line() {
        let error = line_matcher("(unclosed", "serve_ready")
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("serve_ready \"(unclosed\" is not a valid"),
            "{error}"
        );
        let mut t = Tail::new(SERVE_TAIL_LINES);
        t.ready = Some(line_matcher(r"^Node server listening (on|at) .*:\d+$", "x").unwrap());
        t.push(b"starting\n");
        assert!(!t.matched);
        // Colours are stripped first, and a trailing \r.
        t.push(b"\x1b[32mNode server listening at\x1b[0m http://localhost:4242\r\n");
        assert!(t.matched);
        assert_eq!(
            t.lines.back().unwrap(),
            "Node server listening at http://localhost:4242"
        );
        for i in 0..40 {
            t.push(format!("{i}\n").as_bytes());
        }
        assert_eq!(t.lines.len(), SERVE_TAIL_LINES);
        assert!(t.matched, "a match stays");
    }

    #[test]
    fn text_names_the_serve_command_and_its_output_when_it_failed() {
        let o = Outcome {
            command: CommandRun {
                command: "bundle exec rspec".into(),
                not_run: true,
                ..CommandRun::default()
            },
            serve: Some(ServeRun {
                command: "cd server && npm start".into(),
                ready_by: "port 4242".into(),
                exit_code: Some(1),
                stopped: true,
                output_tail: "Error: Cannot find module 'express'".into(),
                ..ServeRun::default()
            }),
            failures: vec![RunFailure::plain(
                "serve_failed",
                1,
                "the serve command `cd server && npm start` exited 1 before it was ready (port 4242 waited for)".into(),
                Some("Read serve.output_tail".into()),
            )],
            ..Outcome::default()
        };
        let s = text(&o);
        assert!(
            s.starts_with(
                "verify --run failed: `bundle exec rspec` was not run (the serve command failed);"
            ),
            "{s}"
        );
        assert!(
            s.contains("Served `cd server && npm start`: not ready (port 4242); stopped\n"),
            "{s}"
        );
        assert!(s.contains("  serve_failed: the serve command"), "{s}");
        assert!(
            s.contains("Serve output (last lines):\n  | Error: Cannot find module 'express'"),
            "{s}"
        );
    }

    /// A missing dependency is found in the output of whichever command
    /// failed, once, and is not counted as pointing at the mock.
    #[test]
    fn missing_dependencies_come_from_the_failed_commands_and_do_not_point_at_the_mock() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("server")).unwrap();
        std::fs::write(root.join("server/package.json"), "{}").unwrap();
        let serve = ServeRun {
            command: "cd server && npm start".into(),
            exit_code: Some(1),
            output_tail: "Error: Cannot find module 'express'".into(),
            ..ServeRun::default()
        };
        let mut command = CommandRun {
            command: "cd server && npm test".into(),
            exit_code: Some(1),
            output_tail: "Error: Cannot find module 'express'".into(),
            ..CommandRun::default()
        };
        let found = missing_dependencies(root, root, Some((&serve, true)), &command);
        // The same module and install, named once (for the server).
        assert_eq!(found.len(), 1, "{found:?}");
        let step = found[0].next_step.as_deref().unwrap();
        assert!(
            step.starts_with("The app's server failed on a missing module `express`")
                && step.contains("server && npm install`")
                && step.ends_with("not under `mockingbird run`."),
            "{step}"
        );
        // A command that passed, or was stopped at the timeout, is not read,
        // nor a serve command that exited while passing tests ran.
        command.exit_code = Some(0);
        assert!(missing_dependencies(root, root, None, &command).is_empty());
        assert!(missing_dependencies(root, root, Some((&serve, false)), &command).is_empty());
        command.exit_code = None;
        command.timed_out = true;
        assert!(missing_dependencies(root, root, None, &command).is_empty());
        // A clean failure names no dependency.
        command.timed_out = false;
        command.exit_code = Some(1);
        command.output_tail = "1 example, 1 failure".into();
        assert!(missing_dependencies(root, root, None, &command).is_empty());

        command.output_tail = "Error: Cannot find module 'express'".into();
        let o = Outcome {
            failures: missing_dependencies(root, root, None, &command),
            command,
            ..Outcome::default()
        };
        let s = text(&o);
        assert!(s.contains("; the app's dependencies are missing\n"), "{s}");
        assert!(
            s.contains("  missing_dependencies: a missing module `express`"),
            "{s}"
        );
        assert!(
            s.contains("    Next: The tests failed on a missing module"),
            "{s}"
        );
    }

    /// With the serve command failed on a missing dependency, the status
    /// line names the dependencies, and neither failure points at the mock.
    #[test]
    fn the_status_line_names_missing_dependencies_beside_a_failed_serve() {
        let o = Outcome {
            command: CommandRun {
                command: "npm test".into(),
                not_run: true,
                ..CommandRun::default()
            },
            failures: vec![
                RunFailure::plain(
                    "missing_dependencies",
                    1,
                    format!("a missing module `express` (Cannot find module) in {SERVE_OUTPUT}"),
                    Some("The app's server failed on …".into()),
                ),
                RunFailure::plain("serve_failed", 1, "exited 1".into(), None),
            ],
            ..Outcome::default()
        };
        let s = text(&o);
        assert!(
            s.starts_with(
                "verify --run failed: `npm test` was not run (the serve command failed); 0 requests logged; the app's dependencies are missing\n"
            ),
            "{s}"
        );
    }
}
