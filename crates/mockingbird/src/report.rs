//! `mockingbird report` summarises what a running server answered (per
//! mock and operation: request counts, response sources including
//! fallbacks, error statuses; proxied hosts no mock claims; snapshot
//! replay hits) as JSON, Markdown or JUnit XML, and `mockingbird verify`
//! checks assertions against the same summary, exiting non-zero when one
//! fails. Both read the request log and `/metrics` over the Admin API, so
//! they see what the request log retains (`observability.
//! request_log_retention` per mock).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, anyhow, bail};
use clap::{Args, ValueEnum};
use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};

use crate::client::{self, ClientError};

#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Json,
    Markdown,
    Junit,
    /// A few lines: the totals and each failed check (`verify` only;
    /// `report` prints JSON instead).
    Text,
}

#[derive(Args, Debug)]
pub struct ReportArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Json)]
    pub format: Format,
    /// Write the report to this file instead of stdout.
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Fail when an answer fell back to the faker because the LLM call (or
    /// a snapshot replay) failed. Faker answers of an offline mock are not
    /// fallbacks.
    #[arg(long)]
    pub no_fallbacks: bool,
    /// Fail when the proxy saw a host no mock claims (passed through or
    /// blocked).
    #[arg(long)]
    pub no_unmapped: bool,
    /// Fail when the proxy blocked a host no mock claims.
    #[arg(long)]
    pub no_blocked: bool,
    /// Fail when a request reached a mock but matched no operation of its
    /// contract (404 or 405: a route the contract lacks).
    #[arg(long)]
    pub no_unmatched: bool,
    /// Fail when the project can call a host no mock lists that an API
    /// description bundled in the project (its dependencies included) or a
    /// catalog example serves (`mockingbird hosts`); hosts only named by a
    /// URL in the app's files are listed, not failed on. Scans the project
    /// of `--config`.
    #[arg(long)]
    pub no_unclaimed_hosts: bool,
    /// The project's config: the project `--no-unclaimed-hosts` scans
    /// (default `./mockingbird.yaml`), and, without `--admin-url`, the
    /// server whose admin listener it describes.
    #[arg(short = 'c', long)]
    pub config: Option<PathBuf>,
    /// Fail on 5xx responses.
    #[arg(long)]
    pub no_errors: bool,
    /// Fail on 4xx responses (for example requests the contract rejects).
    #[arg(long)]
    pub no_client_errors: bool,
    /// Fail when a replay served an answer recorded for another request of
    /// the same operation instead of an exact match.
    #[arg(long)]
    pub exact_replay: bool,
    /// `OPERATION[=N]` or `MOCK/OPERATION[=N]`: the operation (its contract
    /// operation id) was called at least N times (default 1). Repeatable.
    #[arg(long, value_name = "OP[=N]")]
    pub called: Vec<String>,
    /// How to print the result: `text` names the failed checks in a few
    /// lines; `json` carries the whole report.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
    /// Write the result to this file instead of stdout.
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,
    /// Run this command (with `sh -c`, from the project root of `-c`)
    /// against the mocks as `mockingbird run` does, starting the server
    /// when none answers, and check only the requests it made: unmatched
    /// routes, 4xx/5xx answers, fallbacks, blocked hosts and the project's
    /// unclaimed hosts, each with a next step. Exits 1 when the command
    /// fails or any of those is found.
    #[arg(long, value_name = "COMMAND")]
    pub run: Option<String>,
    /// How long the `--run` command may take before it and what it
    /// started are stopped (and it fails) [default: 10m].
    #[arg(long, requires = "run")]
    pub timeout: Option<humantime::Duration>,
    /// Faker only, no LLM, for a server `--run` starts.
    #[arg(long, requires = "run")]
    pub offline: bool,
    /// Where a server `--run` starts keeps its pid and log [default:
    /// $MB_STATE_DIR, else .mockingbird].
    #[arg(long, requires = "run")]
    pub state_dir: Option<PathBuf>,
}

/// Requests of one operation (or of a whole mock).
#[derive(Serialize, Debug, Default, Clone)]
pub struct Counts {
    pub requests: u64,
    /// By response source: `llm`, `patched`, `repaired`, `cache`, `state`, `faker`
    /// (an offline mock), `fallback` (the LLM call failed), `rejected` (the
    /// request failed validation) or `none`.
    pub sources: BTreeMap<String, u64>,
    pub fallbacks: u64,
    pub client_errors: u64,
    pub errors: u64,
}

impl Counts {
    fn add(&mut self, source: &str, status: u64) {
        self.requests += 1;
        *self.sources.entry(source.to_string()).or_default() += 1;
        if source == "fallback" {
            self.fallbacks += 1;
        }
        match status {
            400..=499 => self.client_errors += 1,
            500.. => self.errors += 1,
            _ => {}
        }
    }

    pub(crate) fn merge(&mut self, other: &Counts) {
        self.requests += other.requests;
        for (k, v) in &other.sources {
            *self.sources.entry(k.clone()).or_default() += v;
        }
        self.fallbacks += other.fallbacks;
        self.client_errors += other.client_errors;
        self.errors += other.errors;
    }
}

#[derive(Serialize, Debug, Default)]
pub struct MockReport {
    pub mock: String,
    #[serde(flatten)]
    pub counts: Counts,
    /// By operation id (`-` for requests that matched none).
    pub operations: BTreeMap<String, Counts>,
    /// Distinct fallback reasons with their counts.
    pub fallback_reasons: BTreeMap<String, u64>,
    /// Requests that matched no operation of the contract (`METHOD path`,
    /// answered 404 or 405) with their counts.
    pub unmatched: BTreeMap<String, u64>,
}

#[derive(Serialize, Debug)]
pub struct HostReport {
    pub host: String,
    /// `pass` (forwarded upstream) or `block`.
    pub mode: String,
    pub requests: u64,
    /// For a host blocked because it shares a registrable domain with
    /// mocked hosts (`proxy.vendor_subdomains`): those hosts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sibling_of: Vec<String>,
}

impl HostReport {
    /// `host (mode)`, plus the mocked hosts of its domain for a blocked
    /// vendor sibling.
    fn describe(&self) -> String {
        if self.sibling_of.is_empty() {
            format!("{} ({})", self.host, self.mode)
        } else {
            format!(
                "{} ({}, same domain as mocked {})",
                self.host,
                self.mode,
                self.sibling_of.join(", ")
            )
        }
    }
}

#[derive(Serialize, Debug, Default)]
pub struct Report {
    pub workspace: String,
    #[serde(flatten)]
    pub totals: Counts,
    pub mocks: Vec<MockReport>,
    pub unmapped_hosts: Vec<HostReport>,
    /// Snapshot answers by result (`exact`, `operation`, `miss`,
    /// `recorded`); empty without `--record` / `--replay`.
    pub snapshot: BTreeMap<String, u64>,
    /// Hosts the project can call that no mock lists (`verify
    /// --no-unclaimed-hosts` only, see [`crate::hosts`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unclaimed_hosts: Vec<String>,
    /// What to do about the hosts the proxy blocked
    /// ([`blocked_host_steps`]) and, with `--no-unclaimed-hosts`, the
    /// hosts no mock lists; omitted when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
}

/// Request-log entries fetched per page (the API's maximum).
const PAGE: usize = 500;

/// Builds the report from the server at `admin_url`.
///
/// # Errors
///
/// Fails when the Admin API cannot be reached or answers with an error.
pub async fn collect(
    admin_url: &str,
    token: Option<&str>,
    workspace: &str,
) -> anyhow::Result<Report> {
    let get = |segments: Vec<String>, query: Vec<(&'static str, String)>| async move {
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        client::api(
            admin_url,
            token,
            Some(workspace),
            Method::GET,
            &segments,
            &query,
            None,
        )
        .await
        .map(Option::unwrap_or_default)
        .map_err(|e| match e {
            ClientError::Api { status, body } => anyhow!("Admin API {status}: {body}"),
            ClientError::Usage(m) => anyhow!(m),
            ClientError::Incomplete { message, .. } => anyhow!(message),
            ClientError::Other(e) => e,
        })
    };
    let mut report = Report {
        workspace: workspace.to_string(),
        ..Report::default()
    };
    let mocks = get(vec!["mocks".into()], vec![]).await?;
    for mock in mocks.as_array().into_iter().flatten() {
        let Some(name) = mock["name"].as_str() else {
            continue;
        };
        let mut m = MockReport {
            mock: name.to_string(),
            ..MockReport::default()
        };
        let mut before: Option<i64> = None;
        loop {
            let mut query = vec![
                ("fields", "summary".to_string()),
                ("limit", PAGE.to_string()),
            ];
            if let Some(id) = before {
                query.push(("before", id.to_string()));
            }
            let page = get(vec!["mocks".into(), name.into(), "requests".into()], query)
                .await
                .with_context(|| format!("reading the request log of {name}"))?;
            let entries = page.as_array().cloned().unwrap_or_default();
            for e in &entries {
                add_entry(&mut m, e);
            }
            before = entries.last().and_then(|e| e["id"].as_i64());
            if entries.len() < PAGE || before.is_none() {
                break;
            }
        }
        report.totals.merge(&m.counts);
        report.mocks.push(m);
    }
    let metrics = fetch_metrics(admin_url, token).await?;
    report.unmapped_hosts = unmapped_hosts(&metrics);
    let claimed = claimed_hosts(admin_url, token, workspace).await;
    report.next_steps = blocked_host_steps(&report.unmapped_hosts, &claimed);
    report.snapshot = snapshot_answers(&metrics);
    Ok(report)
}

/// Snapshot answers by result, from a Prometheus text exposition.
pub(crate) fn snapshot_answers(metrics: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for (labels, value) in samples(metrics, crate::snapshot::ANSWERS_METRIC) {
        let result = labels.get("result").cloned().unwrap_or_default();
        *out.entry(result).or_default() += value;
    }
    out
}

/// The config `verify --no-unclaimed-hosts` scans the project of: `-c`,
/// else `./mockingbird.yaml`; `None` without the flag.
///
/// # Errors
///
/// A usage message when the flag is given and that config does not exist.
pub fn unclaimed_hosts_config(args: &VerifyArgs) -> Result<Option<PathBuf>, String> {
    if !args.no_unclaimed_hosts {
        return Ok(None);
    }
    match crate::proxy::env::config_or_local(args.config.as_deref()) {
        Some(config) if config.is_file() => Ok(Some(config)),
        Some(config) => Err(format!(
            "--no-unclaimed-hosts: no config at {}",
            config.display()
        )),
        None => Err("--no-unclaimed-hosts scans the project of a config: pass -c <mockingbird.yaml> (there is no ./mockingbird.yaml)".into()),
    }
}

/// Adds the project host scan of `config` ([`crate::hosts`]) to `report`:
/// the hosts no mock lists (in the config, or on the server at
/// `admin_url`) that `verify --no-unclaimed-hosts` fails on, and the
/// scan's summary line as a next step (leaving out hosts a blocked-host
/// step already names).
///
/// # Errors
///
/// Fails when the config cannot be read or parsed.
pub async fn add_unclaimed_hosts(
    report: &mut Report,
    config: &std::path::Path,
    admin_url: &str,
    token: Option<&str>,
) -> anyhow::Result<()> {
    let served = claimed_hosts(admin_url, token, &report.workspace).await;
    let answer = crate::hosts::project_hosts_async(config.to_path_buf(), served).await?;
    let blocked: std::collections::HashSet<String> = report
        .unmapped_hosts
        .iter()
        .filter(|h| h.mode == "block")
        .map(|h| h.host.clone())
        .collect();
    report.next_steps.extend(answer.summary_without(&blocked));
    report.unclaimed_hosts = answer
        .unclaimed
        .into_iter()
        .filter(crate::hosts::UnclaimedHost::fails_verify)
        .map(|h| h.host)
        .collect();
    Ok(())
}

/// The proxied hosts no mock claims, from the [`UNMAPPED_METRIC`]
/// samples of a Prometheus text exposition, sorted by host and mode.
///
/// [`UNMAPPED_METRIC`]: crate::proxy::UNMAPPED_METRIC
pub(crate) fn unmapped_hosts(metrics: &str) -> Vec<HostReport> {
    // One entry per host and mode: a host's `sibling_of` label changes
    // when the mocked hosts of its domain do, which starts a new sample.
    let mut merged: BTreeMap<(String, String), HostReport> = BTreeMap::new();
    for (labels, value) in samples(metrics, crate::proxy::UNMAPPED_METRIC) {
        let host = labels.get("host").cloned().unwrap_or_default();
        let mode = labels.get("mode").cloned().unwrap_or_default();
        let entry = merged
            .entry((host.clone(), mode.clone()))
            .or_insert_with(|| HostReport {
                host,
                mode,
                requests: 0,
                sibling_of: Vec::new(),
            });
        entry.requests += value;
        for sibling in labels
            .get("sibling_of")
            .into_iter()
            .flat_map(|hosts| hosts.split(','))
            .filter(|h| !h.is_empty())
        {
            if !entry.sibling_of.iter().any(|s| s == sibling) {
                entry.sibling_of.push(sibling.to_owned());
            }
        }
    }
    let mut hosts: Vec<HostReport> = merged.into_values().collect();
    for h in &mut hosts {
        h.sibling_of.sort();
    }
    hosts
}

/// The `next_steps` for the hosts the proxy blocked and no mock claims
/// now (`claimed`, normalized; the counts run from server start, so a
/// host mocked since is left out): per host (not the
/// [`OTHER_HOSTS_LABEL`] overflow), how many calls were blocked, adding
/// it to the mock of its domain when it is a vendor's other host, the
/// catalog examples that list it ([`serving`](crate::examples::serving)),
/// and, last, how to let it through.
///
/// [`OTHER_HOSTS_LABEL`]: crate::proxy::OTHER_HOSTS_LABEL
pub fn blocked_host_steps(
    hosts: &[HostReport],
    claimed: &std::collections::HashSet<String>,
) -> Vec<String> {
    blocked_host_steps_during(hosts, claimed, "since the server started")
}

/// [`blocked_host_steps`] for counts taken over `when` (`since the server
/// started`, `while the command ran`).
pub fn blocked_host_steps_during(
    hosts: &[HostReport],
    claimed: &std::collections::HashSet<String>,
    when: &str,
) -> Vec<String> {
    hosts
        .iter()
        .filter(|h| h.mode == "block" && h.requests > 0)
        .filter(|h| h.host != crate::proxy::OTHER_HOSTS_LABEL && !h.host.is_empty())
        .filter(|h| !claimed.contains(&h.host))
        .map(|h| blocked_host_step(h, when))
        .collect()
}

fn blocked_host_step(h: &HostReport, when: &str) -> String {
    let host = &h.host;
    let times = match h.requests {
        1 => "once".to_string(),
        n => format!("{n} times"),
    };
    let mut step = format!("{host} was blocked {times} {when}: no mock serves it.");
    if !h.sibling_of.is_empty() {
        let _ = write!(
            step,
            " If it is the same API as the mock serving {}, add {host} to that mock's hosts.",
            h.sibling_of.join(", ")
        );
    }
    let otherwise = if h.sibling_of.is_empty() {
        ""
    } else {
        "Otherwise "
    };
    let examples = crate::examples::serving(host);
    if examples.is_empty() {
        let lead = if otherwise.is_empty() {
            "import"
        } else {
            "otherwise import"
        };
        let _ = write!(
            step,
            " No catalog example lists this host: {lead} its API's contract into a mock that lists {host} in hosts (import_contract)."
        );
    } else {
        let listed: Vec<String> = examples
            .iter()
            .map(|s| {
                format!(
                    "example `{}` (mock `{}`, contract `{}`)",
                    s.example, s.mock, s.contract
                )
            })
            .collect();
        let lead = if otherwise.is_empty() {
            "The"
        } else {
            "Otherwise the"
        };
        let _ = write!(
            step,
            " {lead} catalog lists it: {}. Write one with init_example (its name, a new dir; CLI `mockingbird examples init <name> --dir <dir>`) and add that mock and contract to your config.",
            listed.join("; ")
        );
    }
    let pass = if h.sibling_of.is_empty() {
        "proxy.unmapped: pass"
    } else {
        "proxy.vendor_subdomains: pass"
    };
    let _ = write!(
        step,
        " Only as a last resort, set {pass} to send it to the real host. Rerun the app's flows before handing back."
    );
    step
}

/// The hosts the mocks of the server at `admin_url` list (normalized), in
/// every workspace the caller sees, or in `workspace` alone when the
/// workspaces cannot be listed. A workspace whose mocks cannot be read
/// adds none.
pub async fn claimed_hosts(
    admin_url: &str,
    token: Option<&str>,
    workspace: &str,
) -> std::collections::HashSet<String> {
    let listed = client::api(
        admin_url,
        token,
        None,
        Method::GET,
        &["workspaces"],
        &[],
        None,
    )
    .await
    .ok()
    .flatten();
    let mut workspaces: Vec<String> = listed
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|w| w["name"].as_str().map(str::to_owned))
        .collect();
    if workspaces.is_empty() {
        workspaces.push(workspace.to_owned());
    }
    let mut hosts = std::collections::HashSet::new();
    for ws in &workspaces {
        let Ok(Some(mocks)) = client::api(
            admin_url,
            token,
            Some(ws),
            Method::GET,
            &["mocks"],
            &[],
            None,
        )
        .await
        else {
            continue;
        };
        hosts.extend(
            mocks
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|m| m["hosts"].as_array().into_iter().flatten())
                .filter_map(Value::as_str)
                .filter_map(crate::registry::normalize_host),
        );
    }
    hosts
}

/// The hosts the proxy of the server at `admin_url` blocked since it
/// started and no mock claims now, and their `next_steps`
/// ([`blocked_host_steps`], [`claimed_hosts`]).
///
/// # Errors
///
/// Fails when `/metrics` cannot be read.
pub async fn blocked_steps(
    admin_url: &str,
    token: Option<&str>,
    workspace: &str,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let metrics = fetch_metrics(admin_url, token).await?;
    let claimed = claimed_hosts(admin_url, token, workspace).await;
    let hosts = unmapped_hosts(&metrics);
    let blocked = hosts
        .iter()
        .filter(|h| h.mode == "block" && h.requests > 0 && !claimed.contains(&h.host))
        .map(|h| h.host.clone())
        .collect();
    Ok((blocked, blocked_host_steps(&hosts, &claimed)))
}

pub(crate) fn add_entry(m: &mut MockReport, e: &Value) {
    let status = e["status"].as_u64().unwrap_or(0);
    let reason = e["fallback_reason"].as_str();
    let source = match e["source"].as_str() {
        Some("fallback") if reason.is_some_and(|r| r.starts_with(mb_engine::OFFLINE_REASON)) => {
            "faker"
        }
        Some(s) => s,
        None => "none",
    };
    if source == "fallback" {
        *m.fallback_reasons
            .entry(reason.unwrap_or("unknown").to_string())
            .or_default() += 1;
    }
    let op = e["operation_id"].as_str().unwrap_or("-").to_string();
    if e["operation_id"].is_null() && matches!(status, 404 | 405) {
        let route = format!(
            "{} {}",
            e["method"].as_str().unwrap_or("?"),
            e["path"].as_str().unwrap_or("?")
        );
        *m.unmatched.entry(route).or_default() += 1;
    }
    m.counts.add(source, status);
    m.operations.entry(op).or_default().add(source, status);
}

pub(crate) async fn fetch_metrics(admin_url: &str, token: Option<&str>) -> anyhow::Result<String> {
    let url = format!("{}/metrics", admin_url.trim_end_matches('/'));
    let mut req = crate::daemon::http().get(&url);
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        req = req.bearer_auth(token);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    if !resp.status().is_success() {
        bail!("{url} answered {}", resp.status());
    }
    Ok(resp.text().await?)
}

/// The samples of counter `name` in a Prometheus text exposition: labels
/// and value.
pub(crate) fn samples(text: &str, name: &str) -> Vec<(BTreeMap<String, String>, u64)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        let (labels, value) = match rest.strip_prefix('{') {
            Some(rest) => match rest.rsplit_once('}') {
                Some((labels, value)) => (labels, value),
                None => continue,
            },
            None if rest.starts_with(' ') => ("", rest),
            None => continue,
        };
        let Ok(value) = value.trim().parse::<f64>() else {
            continue;
        };
        out.push((parse_labels(labels), value as u64));
    }
    out
}

fn parse_labels(s: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut chars = s.chars().peekable();
    loop {
        let key: String = chars.by_ref().take_while(|c| *c != '=').collect();
        let key = key.trim_start_matches(',').trim().to_string();
        if key.is_empty() || chars.next() != Some('"') {
            break;
        }
        let mut value = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some('n') => value.push('\n'),
                    Some(c) => value.push(c),
                    None => break,
                },
                '"' => break,
                c => value.push(c),
            }
        }
        out.insert(key, value);
    }
    out
}

/// One failed `verify` assertion.
#[derive(Serialize, Debug)]
pub struct Failure {
    pub check: String,
    pub detail: String,
}

/// The assertions of `args` that `report` fails.
///
/// # Errors
///
/// Fails for a `--called` assertion that does not parse.
pub fn check(report: &Report, args: &VerifyArgs) -> anyhow::Result<Vec<Failure>> {
    let mut failures = Vec::new();
    let mut fail = |check: &str, detail: String| {
        failures.push(Failure {
            check: check.into(),
            detail,
        })
    };
    for m in &report.mocks {
        if args.no_fallbacks && m.counts.fallbacks > 0 {
            fail(
                "no-fallbacks",
                format!(
                    "{}: {} fallback answers ({})",
                    m.mock,
                    m.counts.fallbacks,
                    top(&m.fallback_reasons)
                ),
            );
        }
        if args.no_unmatched && !m.unmatched.is_empty() {
            let n: u64 = m.unmatched.values().sum();
            fail(
                "no-unmatched",
                format!(
                    "{}: {n} requests matched no operation ({})",
                    m.mock,
                    top(&m.unmatched)
                ),
            );
        }
        if args.no_errors && m.counts.errors > 0 {
            fail(
                "no-errors",
                format!("{}: {} 5xx responses", m.mock, m.counts.errors),
            );
        }
        if args.no_client_errors && m.counts.client_errors > 0 {
            fail(
                "no-client-errors",
                format!("{}: {} 4xx responses", m.mock, m.counts.client_errors),
            );
        }
    }
    for h in &report.unmapped_hosts {
        if args.no_unmapped || (args.no_blocked && h.mode == "block") {
            let check = if args.no_unmapped {
                "no-unmapped"
            } else {
                "no-blocked"
            };
            fail(check, format!("{}: {} requests", h.describe(), h.requests));
        }
    }
    if args.no_unclaimed_hosts && !report.unclaimed_hosts.is_empty() {
        let n = report.unclaimed_hosts.len();
        let named: Vec<&str> = report
            .unclaimed_hosts
            .iter()
            .take(TOP_ENTRIES)
            .map(String::as_str)
            .collect();
        let more = if n > TOP_ENTRIES {
            format!(", {} more", n - TOP_ENTRIES)
        } else {
            String::new()
        };
        fail(
            "no-unclaimed-hosts",
            format!(
                "{n} host{} with a bundled description or catalog example ha{} no mock ({}{more})",
                if n == 1 { "" } else { "s" },
                if n == 1 { "s" } else { "ve" },
                named.join(", ")
            ),
        );
    }
    if args.exact_replay
        && let Some(n) = report.snapshot.get("operation").filter(|n| **n > 0)
    {
        fail(
            "exact-replay",
            format!("{n} answers replayed from another request of the same operation"),
        );
    }
    for spec in &args.called {
        let (target, min) = parse_called(spec)?;
        let (mock, op) = match target.split_once('/') {
            Some((mock, op)) => (Some(mock), op),
            None => (None, target),
        };
        let count: u64 = report
            .mocks
            .iter()
            .filter(|m| mock.is_none_or(|name| m.mock == name))
            .filter_map(|m| m.operations.get(op))
            .map(|c| c.requests)
            .sum();
        if count < min {
            fail(
                "called",
                format!("{target}: called {count} times, expected at least {min}"),
            );
        }
    }
    Ok(failures)
}

/// Entries named in a failure's detail; the rest are counted.
const TOP_ENTRIES: usize = 3;

/// Characters of one entry (a fallback reason) kept in a failure's detail.
const ENTRY_CHARS: usize = 120;

/// The most frequent entries of `counts` as `N× entry`, each shortened, and
/// how many more there are; `--format json` has them all in full.
fn top(counts: &BTreeMap<String, u64>) -> String {
    let mut entries: Vec<(&String, &u64)> = counts.iter().collect();
    entries.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let mut parts: Vec<String> = entries
        .iter()
        .take(TOP_ENTRIES)
        .map(|(entry, n)| format!("{n}× {}", shorten(entry, ENTRY_CHARS)))
        .collect();
    if entries.len() > TOP_ENTRIES {
        parts.push(format!("{} more", entries.len() - TOP_ENTRIES));
    }
    parts.join("; ")
}

/// `s` cut to `max` characters, with `…` when cut.
pub(crate) fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

/// `OP[=N]` → `(OP, N)`, N defaulting to 1.
fn parse_called(spec: &str) -> anyhow::Result<(&str, u64)> {
    match spec.split_once('=') {
        Some((op, n)) => Ok((
            op,
            n.trim()
                .parse()
                .with_context(|| format!("--called {spec}: N must be a whole number"))?,
        )),
        None => Ok((spec, 1)),
    }
}

/// The report (and any failures) in `format`.
///
/// # Errors
///
/// Fails when the report cannot be serialised.
pub fn render(
    report: &Report,
    failures: Option<&[Failure]>,
    format: Format,
) -> anyhow::Result<String> {
    Ok(match format {
        Format::Json => {
            let mut v = serde_json::to_value(report)?;
            if let Some(failures) = failures {
                v = json!({
                    "ok": failures.is_empty(),
                    "failures": failures,
                    "report": v,
                });
            }
            serde_json::to_string_pretty(&v)?
        }
        Format::Markdown => markdown(report, failures),
        Format::Junit => junit(report, failures),
        Format::Text => match failures {
            Some(failures) => text(report, failures),
            None => serde_json::to_string_pretty(report)?,
        },
    })
}

fn totals(report: &Report) -> String {
    let t = &report.totals;
    let unmatched: u64 = report.mocks.iter().flat_map(|m| m.unmatched.values()).sum();
    format!(
        "{} requests, {} fallbacks, {} unmatched, {} 4xx, {} 5xx, {} unmapped hosts",
        t.requests,
        t.fallbacks,
        unmatched,
        t.client_errors,
        t.errors,
        report.unmapped_hosts.len()
    )
}

/// `verify`'s default output: the totals and one line per failed check.
fn text(report: &Report, failures: &[Failure]) -> String {
    if failures.is_empty() {
        return format!("verify passed ({})\n", totals(report));
    }
    let mut s = format!(
        "verify failed: {} check{} ({})\n",
        failures.len(),
        if failures.len() == 1 { "" } else { "s" },
        totals(report)
    );
    for f in failures {
        let _ = writeln!(s, "  {}: {}", f.check, f.detail);
    }
    for step in &report.next_steps {
        let _ = writeln!(s, "Next: {step}");
    }
    s.push_str("Details: mockingbird verify --format json (or mockingbird report)\n");
    s
}

fn sources(c: &Counts) -> String {
    c.sources
        .iter()
        .map(|(k, v)| format!("{k} {v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Escapes `|` and newlines for a Markdown table cell.
fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn markdown(report: &Report, failures: Option<&[Failure]>) -> String {
    let mut s = String::from("## Mockingbird\n\n");
    if let Some(failures) = failures {
        if failures.is_empty() {
            s.push_str("✅ All checks passed.\n\n");
        } else {
            s.push_str("❌ **Checks failed:**\n\n");
            for f in failures {
                let _ = writeln!(s, "- `{}`: {}", f.check, cell(&f.detail));
            }
            s.push('\n');
        }
    }
    let t = &report.totals;
    let _ = writeln!(
        s,
        "{} requests · {} fallbacks · {} 4xx · {} 5xx · {} unmapped hosts\n",
        t.requests,
        t.fallbacks,
        t.client_errors,
        t.errors,
        report.unmapped_hosts.len()
    );
    if !report.mocks.is_empty() {
        s.push_str("| Mock | Operation | Requests | Sources | Fallbacks | 4xx | 5xx |\n");
        s.push_str("|---|---|---:|---|---:|---:|---:|\n");
        for m in &report.mocks {
            for (op, c) in &m.operations {
                let _ = writeln!(
                    s,
                    "| {} | `{}` | {} | {} | {} | {} | {} |",
                    cell(&m.mock),
                    cell(op),
                    c.requests,
                    sources(c),
                    c.fallbacks,
                    c.client_errors,
                    c.errors
                );
            }
            if m.operations.is_empty() {
                let _ = writeln!(s, "| {} | – | 0 | | 0 | 0 | 0 |", cell(&m.mock));
            }
        }
        s.push('\n');
    }
    let reasons: Vec<(&str, &str, u64)> = report
        .mocks
        .iter()
        .flat_map(|m| {
            m.fallback_reasons
                .iter()
                .map(|(r, n)| (m.mock.as_str(), r.as_str(), *n))
        })
        .collect();
    if !reasons.is_empty() {
        s.push_str("**Fallback reasons**\n\n");
        for (mock, reason, n) in reasons {
            let _ = writeln!(s, "- {mock}: {n}× {}", cell(&shorten(reason, 300)));
        }
        s.push('\n');
    }
    let unmatched: Vec<(&str, &str, u64)> = report
        .mocks
        .iter()
        .flat_map(|m| {
            m.unmatched
                .iter()
                .map(|(r, n)| (m.mock.as_str(), r.as_str(), *n))
        })
        .collect();
    if !unmatched.is_empty() {
        s.push_str("**Unmatched routes** (no contract operation)\n\n| Mock | Route | Requests |\n|---|---|---:|\n");
        for (mock, route, n) in unmatched {
            let _ = writeln!(s, "| {} | `{}` | {n} |", cell(mock), cell(route));
        }
        s.push('\n');
    }
    if !report.unmapped_hosts.is_empty() {
        s.push_str("**Unmapped hosts** (no mock claims them)\n\n| Host | Mode | Requests |\n|---|---|---:|\n");
        for h in &report.unmapped_hosts {
            let mode = if h.sibling_of.is_empty() {
                h.mode.clone()
            } else {
                format!("{} (same domain as {})", h.mode, h.sibling_of.join(", "))
            };
            let _ = writeln!(
                s,
                "| {} | {} | {} |",
                cell(&h.host),
                cell(&mode),
                h.requests
            );
        }
        s.push('\n');
    }
    if !report.next_steps.is_empty() {
        s.push_str("**Next steps**\n\n");
        for step in &report.next_steps {
            let _ = writeln!(s, "- {}", cell(step));
        }
        s.push('\n');
    }
    if !report.snapshot.is_empty() {
        let parts: Vec<String> = report
            .snapshot
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        let _ = writeln!(s, "Snapshot answers: {}\n", parts.join(", "));
    }
    s
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// One test case per operation (failing on fallbacks or 5xx), per unmapped
/// host (failing when blocked) and, for `verify`, per failed check.
fn junit(report: &Report, failures: Option<&[Failure]>) -> String {
    let mut cases = String::new();
    let mut tests = 0;
    let mut failed = 0;
    let mut case = |class: &str, name: &str, failure: Option<String>| {
        tests += 1;
        match failure {
            Some(message) => {
                failed += 1;
                let _ = writeln!(
                    cases,
                    "    <testcase classname=\"{}\" name=\"{}\"><failure message=\"{}\"/></testcase>",
                    xml(class),
                    xml(name),
                    xml(&message)
                );
            }
            None => {
                let _ = writeln!(
                    cases,
                    "    <testcase classname=\"{}\" name=\"{}\"/>",
                    xml(class),
                    xml(name)
                );
            }
        }
    };
    for m in &report.mocks {
        for (op, c) in &m.operations {
            let failure = (c.fallbacks > 0 || c.errors > 0).then(|| {
                format!(
                    "{} requests: {} fallbacks, {} 5xx",
                    c.requests, c.fallbacks, c.errors
                )
            });
            case(&format!("mockingbird.{}", m.mock), op, failure);
        }
    }
    for h in &report.unmapped_hosts {
        let failure = (h.mode == "block").then(|| {
            if h.sibling_of.is_empty() {
                format!("{} requests blocked: no mock claims the host", h.requests)
            } else {
                format!(
                    "{} requests blocked: no mock claims the host, which shares its domain with mocked {}",
                    h.requests,
                    h.sibling_of.join(", ")
                )
            }
        });
        case("mockingbird.proxy", &h.host, failure);
    }
    for f in failures.into_iter().flatten() {
        case("mockingbird.verify", &f.check, Some(f.detail.clone()));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n  <testsuite name=\"mockingbird\" tests=\"{tests}\" failures=\"{failed}\">\n{cases}  </testsuite>\n</testsuites>\n"
    )
}

/// Writes `text` to `output`, else stdout.
///
/// # Errors
///
/// Fails when `output` or stdout cannot be written.
pub fn emit(text: &str, output: Option<&std::path::Path>) -> anyhow::Result<()> {
    match output {
        Some(path) => {
            std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
        }
        None => {
            println!("{}", text.trim_end());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        let mut m = MockReport {
            mock: "orders".into(),
            ..MockReport::default()
        };
        for e in [
            json!({"operation_id": "getOrder", "status": 200, "source": "llm"}),
            json!({"operation_id": "getOrder", "status": 200, "source": "fallback", "fallback_reason": "LLM request timed out"}),
            json!({"operation_id": "listOrders", "status": 200, "source": "fallback", "fallback_reason": mb_engine::OFFLINE_REASON}),
            json!({"operation_id": null, "method": "POST", "path": "/accounts/balance/get", "status": 404, "source": null}),
        ] {
            add_entry(&mut m, &e);
        }
        let mut r = Report::default();
        r.totals.merge(&m.counts);
        r.mocks.push(m);
        r.unmapped_hosts.push(HostReport {
            host: "api.example.com".into(),
            mode: "block".into(),
            requests: 2,
            sibling_of: Vec::new(),
        });
        r
    }

    fn verify(args: &[&str]) -> VerifyArgs {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            v: VerifyArgs,
        }
        <Cli as clap::Parser>::parse_from(std::iter::once("verify").chain(args.iter().copied())).v
    }

    fn blocked(host: &str, requests: u64, sibling_of: &[&str]) -> HostReport {
        HostReport {
            host: host.into(),
            mode: "block".into(),
            requests,
            sibling_of: sibling_of.iter().map(|h| (*h).to_owned()).collect(),
        }
    }

    fn none() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    #[test]
    fn a_blocked_host_the_catalog_serves_names_its_example_and_contract() {
        let steps = blocked_host_steps(
            &[blocked("lookups.twilio.com", 3, &["api.twilio.com"])],
            &none(),
        );
        assert_eq!(steps.len(), 1, "{steps:?}");
        let step = &steps[0];
        assert!(
            step.starts_with(
                "lookups.twilio.com was blocked 3 times since the server started: no mock serves it. If it is the same API as the mock serving api.twilio.com, add lookups.twilio.com to that mock's hosts. Otherwise the catalog lists it: example `twilio` (mock `twilio-lookups`, contract `twilio-lookups.openapi.yaml`)."
            ),
            "{step}"
        );
        assert!(step.contains("init_example"), "{step}");
        assert!(
            step.contains("Only as a last resort, set proxy.vendor_subdomains: pass"),
            "{step}"
        );
    }

    #[test]
    fn a_blocked_host_every_catalog_example_lists_names_them_all() {
        let steps = blocked_host_steps(&[blocked("api.github.com", 2, &[])], &none());
        assert!(steps[0].contains("example `github-rest`"), "{}", steps[0]);
        assert!(
            steps[0].contains("example `github-graphql`"),
            "{}",
            steps[0]
        );
        assert!(steps[0].contains(" The catalog lists it: "), "{}", steps[0]);
    }

    #[test]
    fn a_blocked_host_without_a_catalog_entry_says_how_to_mock_or_pass_it() {
        let steps = blocked_host_steps(
            &[
                blocked("verify.twilio.com", 9, &["api.twilio.com"]),
                blocked("api.example.com", 1, &[]),
                blocked(crate::proxy::OTHER_HOSTS_LABEL, 4, &[]),
                HostReport {
                    host: "passed.example.org".into(),
                    mode: "pass".into(),
                    requests: 2,
                    sibling_of: Vec::new(),
                },
            ],
            &none(),
        );
        assert_eq!(steps.len(), 2, "{steps:?}");
        assert_eq!(
            steps[0],
            "verify.twilio.com was blocked 9 times since the server started: no mock serves it. If it is the same API as the mock serving api.twilio.com, add verify.twilio.com to that mock's hosts. No catalog example lists this host: otherwise import its API's contract into a mock that lists verify.twilio.com in hosts (import_contract). Only as a last resort, set proxy.vendor_subdomains: pass to send it to the real host. Rerun the app's flows before handing back."
        );
        assert_eq!(
            steps[1],
            "api.example.com was blocked once since the server started: no mock serves it. No catalog example lists this host: import its API's contract into a mock that lists api.example.com in hosts (import_contract). Only as a last resort, set proxy.unmapped: pass to send it to the real host. Rerun the app's flows before handing back."
        );
    }

    #[test]
    fn a_blocked_host_a_mock_now_lists_gets_no_step() {
        let claimed: std::collections::HashSet<String> =
            ["verify.twilio.com".to_owned()].into_iter().collect();
        let steps = blocked_host_steps(
            &[blocked("verify.twilio.com", 9, &["api.twilio.com"])],
            &claimed,
        );
        assert!(steps.is_empty(), "{steps:?}");
    }

    #[test]
    fn unmapped_hosts_merge_samples_and_their_steps_come_from_the_metric() {
        let metrics = format!(
            "{m}{{host=\"verify.twilio.com\",mode=\"block\",sibling_of=\"api.twilio.com\"}} 9\n\
             {m}{{host=\"verify.twilio.com\",mode=\"block\",sibling_of=\"api.twilio.com,lookups.twilio.com\"}} 2\n\
             {m}{{host=\"example.org\",mode=\"pass\"}} 2\n",
            m = crate::proxy::UNMAPPED_METRIC
        );
        let hosts = unmapped_hosts(&metrics);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].host, "example.org");
        assert_eq!(hosts[1].requests, 11);
        assert_eq!(
            hosts[1].sibling_of,
            ["api.twilio.com", "lookups.twilio.com"]
        );
        let steps = blocked_host_steps(&hosts, &none());
        assert_eq!(steps.len(), 1);
        assert!(
            steps[0].starts_with("verify.twilio.com was blocked 11 times since the server started")
        );
    }

    #[test]
    fn next_steps_render_in_json_markdown_and_text() {
        let mut r = report();
        r.next_steps = vec!["mock verify.example.com".into()];
        let json: Value = serde_json::from_str(&render(&r, None, Format::Json).unwrap()).unwrap();
        assert_eq!(json["next_steps"][0], "mock verify.example.com");
        let md = render(&r, None, Format::Markdown).unwrap();
        assert!(
            md.contains("**Next steps**\n\n- mock verify.example.com\n"),
            "{md}"
        );
        let failures = check(&r, &verify(&["--no-blocked"])).unwrap();
        let text = render(&r, Some(&failures), Format::Text).unwrap();
        assert!(text.contains("\nNext: mock verify.example.com\n"), "{text}");
        let quiet = render(&report(), None, Format::Json).unwrap();
        assert!(!quiet.contains("next_steps"), "{quiet}");
    }

    #[test]
    fn counts_faker_answers_apart_from_fallbacks() {
        let r = report();
        let m = &r.mocks[0];
        assert_eq!(m.counts.requests, 4);
        assert_eq!(m.counts.fallbacks, 1);
        assert_eq!(m.counts.sources["faker"], 1);
        assert_eq!(m.counts.client_errors, 1);
        assert_eq!(m.operations["-"].requests, 1);
    }

    #[test]
    fn checks_fail_as_asked() {
        let r = report();
        assert!(check(&r, &verify(&[])).unwrap().is_empty());
        let f = check(
            &r,
            &verify(&[
                "--no-fallbacks",
                "--no-blocked",
                "--called",
                "getOrder=2",
                "--called",
                "orders/createOrder",
            ]),
        )
        .unwrap();
        let checks: Vec<&str> = f.iter().map(|f| f.check.as_str()).collect();
        assert_eq!(checks, ["no-fallbacks", "no-blocked", "called"], "{f:?}");
        assert!(f[2].detail.contains("createOrder: called 0 times"));
        assert!(check(&r, &verify(&["--called", "getOrder=x"])).is_err());
    }

    #[test]
    fn no_unclaimed_hosts_needs_a_config_that_exists() {
        assert_eq!(unclaimed_hosts_config(&verify(&[])), Ok(None));
        let err = unclaimed_hosts_config(&verify(&[
            "--no-unclaimed-hosts",
            "-c",
            "/nonexistent/mockingbird.yaml",
        ]))
        .unwrap_err();
        assert!(
            err.contains("no config at /nonexistent/mockingbird.yaml"),
            "{err}"
        );
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("mockingbird.yaml");
        std::fs::write(&config, "mocks: {}\n").unwrap();
        let path = config.display().to_string();
        assert_eq!(
            unclaimed_hosts_config(&verify(&["--no-unclaimed-hosts", "-c", &path])),
            Ok(Some(config))
        );
    }

    #[test]
    fn unclaimed_hosts_fail_no_unclaimed_hosts_and_print_next_lines() {
        let mut r = report();
        assert!(
            check(&r, &verify(&["--no-unclaimed-hosts"]))
                .unwrap()
                .is_empty()
        );
        r.unclaimed_hosts = [
            "a.vendor.com",
            "b.vendor.com",
            "c.vendor.com",
            "d.vendor.com",
        ]
        .map(String::from)
        .to_vec();
        r.next_steps = vec!["4 hosts of vendor.com that the project can call have no mock.".into()];
        assert!(check(&r, &verify(&[])).unwrap().is_empty());
        let f = check(&r, &verify(&["--no-unclaimed-hosts", "-c", "app.yaml"])).unwrap();
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].check, "no-unclaimed-hosts");
        assert_eq!(
            f[0].detail,
            "4 hosts with a bundled description or catalog example have no mock (a.vendor.com, b.vendor.com, c.vendor.com, 1 more)"
        );
        let text = render(&r, Some(&f), Format::Text).unwrap();
        assert!(
            text.contains("Next: 4 hosts of vendor.com that the project can call have no mock."),
            "{text}"
        );
    }

    #[test]
    fn unmatched_routes_fail_no_unmatched() {
        let r = report();
        assert_eq!(r.mocks[0].unmatched["POST /accounts/balance/get"], 1);
        let f = check(&r, &verify(&["--no-unmatched"])).unwrap();
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].check, "no-unmatched");
        assert!(
            f[0].detail
                .contains("1 requests matched no operation (1× POST /accounts/balance/get)"),
            "{}",
            f[0].detail
        );
        let md = render(&r, None, Format::Markdown).unwrap();
        assert!(md.contains("`POST /accounts/balance/get`"), "{md}");
    }

    #[test]
    fn text_output_is_short_with_long_reasons() {
        let mut m = MockReport {
            mock: "github".into(),
            ..MockReport::default()
        };
        for i in 0..6 {
            let reason = format!("{i}: {}", "x".repeat(10_000));
            add_entry(
                &mut m,
                &json!({"operation_id": "repo", "status": 200, "source": "fallback", "fallback_reason": reason}),
            );
        }
        let mut r = Report::default();
        r.totals.merge(&m.counts);
        r.mocks.push(m);
        let args = verify(&["--no-fallbacks"]);
        assert_eq!(args.format, Format::Text);
        let f = check(&r, &args).unwrap();
        let out = render(&r, Some(&f), args.format).unwrap();
        assert!(out.len() < 1_000, "{out}");
        assert!(
            out.starts_with("verify failed: 1 check (6 requests, 6 fallbacks"),
            "{out}"
        );
        assert!(
            out.contains("no-fallbacks: github: 6 fallback answers"),
            "{out}"
        );
        assert!(out.contains("; 3 more"), "{out}");
        assert!(out.contains("--format json"), "{out}");
        let json = render(&r, Some(&f), Format::Json).unwrap();
        assert!(json.len() > 60_000, "the JSON keeps the full reasons");
        let ok = render(&r, Some(&[]), Format::Text).unwrap();
        assert!(ok.starts_with("verify passed (6 requests"), "{ok}");
    }

    #[test]
    fn renders_markdown_and_junit() {
        let r = report();
        let md = render(&r, None, Format::Markdown).unwrap();
        assert!(md.contains("| orders | `getOrder` | 2 |"), "{md}");
        assert!(md.contains("api.example.com"));
        let xml = render(&r, None, Format::Junit).unwrap();
        assert!(xml.contains("tests=\"4\" failures=\"2\""), "{xml}");
    }

    #[test]
    fn vendor_siblings_are_listed_and_fail_no_unmapped() {
        let mut r = Report::default();
        r.unmapped_hosts.push(HostReport {
            host: "verify.twilio.com".into(),
            mode: "block".into(),
            requests: 1,
            sibling_of: vec!["api.twilio.com".into()],
        });
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(
            json["unmapped_hosts"][0],
            json!({"host": "verify.twilio.com", "mode": "block", "requests": 1, "sibling_of": ["api.twilio.com"]})
        );
        let f = check(&r, &verify(&["--no-unmapped"])).unwrap();
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].check, "no-unmapped");
        assert!(
            f[0].detail
                .contains("verify.twilio.com (block, same domain as mocked api.twilio.com)"),
            "{f:?}"
        );
        let md = render(&r, None, Format::Markdown).unwrap();
        assert!(
            md.contains("| verify.twilio.com | block (same domain as api.twilio.com) | 1 |"),
            "{md}"
        );
    }

    #[test]
    fn parses_prometheus_samples() {
        let text = "# TYPE x counter\nmb_proxy_unmapped_requests_total{host=\"a.io\",mode=\"pass\"} 3\nmb_proxy_unmapped_requests_total_other 1\n";
        let s = samples(text, "mb_proxy_unmapped_requests_total");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0["host"], "a.io");
        assert_eq!(s[0].1, 3);
    }
}
