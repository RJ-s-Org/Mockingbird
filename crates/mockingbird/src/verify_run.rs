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
//! Reported, each with a `next_step`:
//!
//! - `unmatched_route`: a request reached a mock but matched no operation
//!   of its contract (404 or 405);
//! - `error_status`: a mock answered 4xx or 5xx, with the operation, the
//!   status and the problem's detail;
//! - `fallback`: an answer fell back to the faker because the LLM call (or
//!   a replay) failed, with the reason;
//! - `blocked_host`: the proxy blocked a host no mock claims
//!   ([`crate::report::blocked_host_steps`]);
//! - `unclaimed_hosts`: hosts the project can call that a bundled
//!   description or catalog example serves but no mock lists
//!   ([`crate::hosts`]);
//! - `check`: an assertion given beside `--run` (`--called`, ...) that the
//!   window fails.
//!
//! Costs are capped: the command's time ([`Options::timeout`], then its
//! process group is stopped), the output kept ([`TAIL_LINES`] lines of
//! [`LINE_CHARS`] characters), the log entries examined
//! ([`MAX_REQUESTS`]), the problem bodies fetched ([`MAX_DETAILS`]) and
//! the failures listed ([`MAX_FAILURES`]).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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

/// The default limit on the command's run time.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

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

/// What to run and against which server.
#[derive(Debug, Clone)]
pub struct Options {
    /// The command, run with `sh -c` (`cmd /C` on Windows) from the
    /// project root of the config.
    pub command: String,
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
    /// Where it ran: the project root.
    pub cwd: String,
    /// Its exit code (`128 + n` when signal `n` ended it, 127 when it could
    /// not start); `null` when it was stopped at the timeout.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// The last lines of its stdout and stderr, interleaved as they came.
    pub output_tail: String,
    /// Whether earlier output (or the end of long lines) was left out.
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
    /// `unclaimed_hosts` or `check`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mock: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
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
    let env = crate::proxy::env::proxy_env(&session.proxy)?;
    let command = execute(opts, &root, &env.vars).await?;

    settle(admin, token).await;
    // Mocks created while the command ran have no baseline: all theirs count.
    let mocks = list_mocks(admin, token).await.unwrap_or(mocks);
    let (entries, truncated) = window_entries(admin, token, &mocks, &baseline).await?;
    let metrics_after = metrics(own).await;
    let hosts = host_delta(
        &report::unmapped_hosts(&metrics_before),
        &report::unmapped_hosts(&metrics_after),
    );

    let mut outcome = Outcome {
        window: Window {
            requests: entries.len() as u64,
            truncated,
            server_started: session.started_server(),
            admin: admin.to_owned(),
        },
        ..Outcome::default()
    };
    let mut failures = request_failures(&entries);
    add_details(admin, token, &mut failures).await;

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
        failures.push(RunFailure {
            kind: "blocked_host".into(),
            workspace: None,
            mock: None,
            operation: None,
            status: None,
            count: h.requests,
            detail: format!("the proxy blocked {}", describe_host(h)),
            request_id: None,
            next_step: Some(step),
        });
    }
    for h in hosts.iter().filter(|h| h.mode == "pass") {
        outcome.notes.push(format!(
            "{} was passed to the real host {} while the command ran (proxy.unmapped: pass)",
            h.host,
            times(h.requests)
        ));
    }
    match crate::hosts::project_hosts_async(session.config.clone(), claimed).await {
        Ok(scan) => {
            let hosts: Vec<String> = scan
                .unclaimed
                .iter()
                .filter(|h| h.fails_verify() && !blocked.contains(&h.host))
                .map(|h| h.host.clone())
                .collect();
            if !hosts.is_empty() {
                failures.push(RunFailure {
                    kind: "unclaimed_hosts".into(),
                    workspace: None,
                    mock: None,
                    operation: None,
                    status: None,
                    count: hosts.len() as u64,
                    detail: format!(
                        "the project can call {} with a bundled description or catalog example and no mock: {}",
                        plural(hosts.len(), "host"),
                        name_some(&hosts)
                    ),
                    request_id: None,
                    next_step: scan.summary_without(&blocked),
                });
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
            failures.push(RunFailure {
                kind: "check".into(),
                workspace: None,
                mock: None,
                operation: None,
                status: None,
                count: 1,
                detail: format!("{}: {}", f.check, f.detail),
                request_id: None,
                next_step: None,
            });
        }
    }
    if failures.len() > MAX_FAILURES {
        outcome.more_failures = failures.len() - MAX_FAILURES;
        failures.truncate(MAX_FAILURES);
    }
    outcome.ok = command.exit_code == Some(0) && failures.is_empty();
    outcome.failures = failures;
    outcome.command = command;
    Ok(outcome)
}

/// The `verify` checks `--run` always makes, which are not repeated as
/// `check` failures.
const ALWAYS: &[&str] = &[
    "no-fallbacks",
    "no-blocked",
    "no-unmatched",
    "no-errors",
    "no-client-errors",
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

/// The newest request-log id of each mock that has one.
async fn newest_ids(admin: &str, token: Option<&str>, mocks: &[MockRef]) -> HashMap<MockRef, i64> {
    let mut out = HashMap::new();
    for m in mocks {
        let page = get(
            admin,
            token,
            Some(&m.workspace),
            &["mocks", &m.name, "requests"],
            &[("fields", "summary".into()), ("limit", "1".into())],
        )
        .await;
        if let Some(id) = page.ok().and_then(|p| p[0]["id"].as_i64()) {
            out.insert(m.clone(), id);
        }
    }
    out
}

/// Waits until the request log stops growing (the recorder writes off the
/// request path): two reads in a row agree, or [`SETTLE`] passed.
async fn settle(admin: &str, token: Option<&str>) {
    let deadline = Instant::now() + SETTLE;
    let mut last: Option<HashMap<MockRef, i64>> = None;
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

/// The request-log entries (summaries) newer than `baseline`, newest first
/// per mock, at most [`MAX_REQUESTS`]; whether the cap stopped the reading.
async fn window_entries(
    admin: &str,
    token: Option<&str>,
    mocks: &[MockRef],
    baseline: &HashMap<MockRef, i64>,
) -> anyhow::Result<(Vec<(MockRef, Value)>, bool)> {
    const PAGE: usize = 500;
    let mut out = Vec::new();
    for m in mocks {
        let floor = baseline.get(m).copied().unwrap_or(i64::MIN);
        let mut before: Option<i64> = None;
        loop {
            let left = MAX_REQUESTS - out.len();
            if left == 0 {
                return Ok((out, true));
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
            let n = entries.len();
            let mut done = n < PAGE.min(left);
            for e in entries {
                let Some(id) = e["id"].as_i64() else {
                    continue;
                };
                if id <= floor {
                    done = true;
                    break;
                }
                before = Some(id);
                out.push((m.clone(), e));
            }
            if done || before.is_none() {
                break;
            }
            if out.len() >= MAX_REQUESTS {
                // The cap is reached with this mock's window not read to
                // its start: older entries may be left.
                return Ok((out, true));
            }
        }
    }
    Ok((out, false))
}

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

/// The failures the logged requests show: unmatched routes, 4xx/5xx
/// answers and fallbacks, grouped (by mock, route or operation, status or
/// reason), the most frequent first within each kind.
fn request_failures(entries: &[(MockRef, Value)]) -> Vec<RunFailure> {
    // (kind, mock, operation or route, status, reason) → (count, newest id)
    type Key = (u8, MockRef, String, u16, String);
    let mut groups: BTreeMap<Key, (u64, Option<i64>)> = BTreeMap::new();
    for (m, e) in entries {
        let status = u16::try_from(e["status"].as_u64().unwrap_or(0)).unwrap_or(0);
        let op = e["operation_id"].as_str();
        let id = e["id"].as_i64();
        let mut add = |kind: u8, what: String, status: u16, reason: String| {
            let entry = groups
                .entry((kind, m.clone(), what, status, reason))
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
            add(0, route, status, String::new());
            continue;
        }
        let op = op.unwrap_or("-").to_owned();
        if status >= 400 {
            add(1, op.clone(), status, String::new());
        }
        if e["source"].as_str() == Some("fallback") {
            let reason = e["fallback_reason"].as_str().unwrap_or("unknown");
            if !reason.starts_with(mb_engine::OFFLINE_REASON) {
                add(2, op, status, reason.to_owned());
            }
        }
    }
    let mut out: Vec<(u8, u64, RunFailure)> = groups
        .into_iter()
        .map(|((kind, m, what, status, reason), (count, id))| {
            let workspace =
                (m.workspace != mb_store::DEFAULT_WORKSPACE).then(|| m.workspace.clone());
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
                    count,
                    request_id: id,
                },
                1 => RunFailure {
                    kind: "error_status".into(),
                    workspace,
                    detail: format!("{}: {what} answered {status} ({})", m.name, times(count)),
                    next_step: None,
                    mock: Some(m.name),
                    operation: Some(what),
                    status: Some(status),
                    count,
                    request_id: id,
                },
                _ => RunFailure {
                    kind: "fallback".into(),
                    workspace,
                    detail: format!(
                        "{}: {what} fell back to the faker ({}): {}",
                        m.name,
                        times(count),
                        report::shorten(&reason, 300)
                    ),
                    next_step: Some(fallback_step(&m.name, &what)),
                    mock: Some(m.name),
                    operation: Some(what),
                    status: Some(status),
                    count,
                    request_id: id,
                },
            };
            (kind, count, failure)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    out.into_iter().map(|(_, _, f)| f).collect()
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

fn error_step(mock: &str, op: &str, status: u16, id: Option<i64>) -> String {
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
    format!(
        "Mock `{mock}` answered {status} to {target}. If the app's call is right, the mock is wrong: check its world (get_world {{mock: {mock}}}){request}, then fix the world description (patch_world) or the contract, and run verify --run again."
    )
}

/// Reads the newest request of each `error_status` failure (at most
/// [`MAX_DETAILS`]) for the problem's detail, and sets its `next_step`.
async fn add_details(admin: &str, token: Option<&str>, failures: &mut [RunFailure]) {
    let mut read = 0;
    for f in failures.iter_mut().filter(|f| f.kind == "error_status") {
        let mock = f.mock.clone().unwrap_or_default();
        let op = f.operation.clone().unwrap_or_default();
        f.next_step = Some(error_step(&mock, &op, f.status.unwrap_or(0), f.request_id));
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
#[derive(Default)]
struct Tail {
    lines: VecDeque<String>,
    truncated: bool,
}

impl Tail {
    fn push(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches(['\n', '\r']);
        if text.chars().count() > LINE_CHARS {
            self.truncated = true;
        }
        self.lines.push_back(report::shorten(text, LINE_CHARS));
        if self.lines.len() > TAIL_LINES {
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

/// Reads `stream` into `tail` line by line (a line past [`LINE_CHARS`] is
/// cut), echoing each on stderr when `echo` is set.
async fn read_lines(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    tail: Arc<Mutex<Tail>>,
    echo: bool,
) {
    let mut buf = [0u8; 8192];
    let mut line: Vec<u8> = Vec::new();
    let flush = |line: &mut Vec<u8>| {
        if echo {
            eprintln!(
                "{}",
                String::from_utf8_lossy(line).trim_end_matches(['\n', '\r'])
            );
        }
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

/// Runs `opts.command` from `cwd` with `env`, within `opts.timeout`.
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
    #[cfg(windows)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(&opts.command);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(&opts.command);
        // Its own process group, so a timeout stops what it started too.
        c.process_group(0);
        c
    };
    cmd.current_dir(cwd)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            run.exit_code = Some(127);
            run.output_tail = format!("cannot run the command: {error}");
            return Ok(run);
        }
    };
    let tail = Arc::new(Mutex::new(Tail::default()));
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(tokio::spawn(read_lines(
            out,
            tail.clone(),
            opts.interactive,
        )));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(tokio::spawn(read_lines(
            err,
            tail.clone(),
            opts.interactive,
        )));
    }
    let deadline = tokio::time::sleep(opts.timeout);
    tokio::pin!(deadline);
    let interrupt = async {
        if opts.interactive {
            crate::server::shutdown_signal().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(interrupt);
    let status = tokio::select! {
        status = child.wait() => Some(status?),
        () = &mut deadline => {
            run.timed_out = true;
            None
        }
        () = &mut interrupt => None,
    };
    let status = match status {
        Some(status) => Some(status),
        None => {
            if let Some(pid) = child.id() {
                stop_group(pid, false);
            }
            match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                Ok(status) => status.ok(),
                Err(_) => {
                    if let Some(pid) = child.id() {
                        stop_group(pid, true);
                    }
                    let _ = child.kill().await;
                    child.wait().await.ok()
                }
            }
        }
    };
    run.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    for reader in readers {
        let abort = reader.abort_handle();
        if tokio::time::timeout(DRAIN, reader).await.is_err() {
            abort.abort();
        }
    }
    if !run.timed_out {
        run.exit_code = status.map(exit_code);
    }
    if let Ok(t) = tail.lock() {
        run.output_tail = t.text();
        run.output_truncated = t.truncated;
    }
    Ok(run)
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
    let ended = match (c.timed_out, c.exit_code) {
        (true, _) => format!("was stopped at the timeout after {secs:.1}s"),
        (false, Some(code)) => format!("exited {code} after {secs:.1}s"),
        (false, None) => format!("was interrupted after {secs:.1}s"),
    };
    let mut s = format!(
        "verify --run {}: `{}` {ended}; {} logged while it ran{}; {}\n",
        if o.ok { "passed" } else { "failed" },
        c.command,
        plural(
            usize::try_from(o.window.requests).unwrap_or(usize::MAX),
            "request"
        ),
        if o.window.truncated {
            format!(" (the newest {MAX_REQUESTS} examined)")
        } else {
            String::new()
        },
        match o.failures.len() + o.more_failures {
            0 => "no failures point at the mock".to_owned(),
            n => format!(
                "{} point{} at the mock",
                plural(n, "failure"),
                if n == 1 { "s" } else { "" }
            ),
        }
    );
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
        let f = request_failures(&entries);
        let kinds: Vec<&str> = f.iter().map(|f| f.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["unmatched_route", "error_status", "fallback"],
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
        assert_eq!(f[1].operation.as_deref(), Some("createOrder"));
        assert_eq!(f[1].status, Some(400));
        assert!(
            f[2].detail.contains("LLM request timed out"),
            "{}",
            f[2].detail
        );
        let step = error_step("orders", "createOrder", 400, Some(7));
        assert!(
            step.contains("get_world {mock: orders}")
                && step.contains("get_request {mock: orders, id: 7}"),
            "{step}"
        );
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
            failures: request_failures(&[(
                m("orders"),
                json!({"id": 1, "operation_id": null, "method": "GET", "path": "/nope", "status": 404}),
            )]),
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
}
