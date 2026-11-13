//! The project host scan: the vendor hosts an app can call, found in its
//! project directory before it ever runs, and the ones no mock lists.
//!
//! Two signals, from the project root ([`project_root`]):
//!
//! - API descriptions anywhere in the project, installed dependencies
//!   included (`node_modules`, `vendor`, `.venv`, ...): JSON or YAML files
//!   that are OpenAPI 3 (`servers[].url`, at the top, per path or per
//!   operation) or Swagger 2 (`host`). Each host is recorded with the file,
//!   its title and the operations served there.
//! - URL literals (`https://host`) in the app's own files (not its
//!   dependencies or tests), kept only when the host shares a registrable
//!   domain with a host of such a description or a mocked host, or is a
//!   catalog example's host itself, so documentation links stay out.
//!
//! Each unclaimed host carries its [`Evidence`]: a host only named by a
//! URL literal is listed but is the weaker tier, which `verify
//! --no-unclaimed-hosts` does not fail on.
//!
//! The walk skips version control, build outputs (outside dependencies),
//! the mock's own folder and contract files, and symlinks, and stops at
//! [`Limits`] (files, bytes per file, bytes in all, wall-clock time),
//! saying so in [`ScanStats::stopped`]. A scan is cached per project root
//! while the modification times near the root are unchanged (one the time
//! budget stopped, for a minute).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::bail;
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::proxy::siblings::registrable_domain;

#[derive(Args, Debug)]
pub struct HostsArgs {
    /// The project's config: the scan starts at its project root, and a
    /// host counts as claimed when a mock of it lists the host.
    #[arg(short = 'c', long, default_value = crate::client::DEFAULT_CONFIG)]
    pub config: PathBuf,
}

/// What one scan may cost; past a limit it stops and says which.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Directory entries looked at.
    pub max_files: usize,
    /// Bytes read from one file: a larger API description is skipped, a
    /// larger source file is read up to this size.
    pub max_file_bytes: u64,
    /// Bytes read in all.
    pub max_total_bytes: u64,
    /// Wall-clock time.
    pub budget: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 200_000,
            max_file_bytes: 4 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            budget: Duration::from_secs(2),
        }
    }
}

/// Bytes read from the start of a dependency's JSON or YAML file to decide
/// whether it may be an API description.
const HEAD: u64 = 8 * 1024;

/// Files whose presence marks a project root (besides `.git`).
const MANIFESTS: &[&str] = &[
    "package.json",
    "deno.json",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "Pipfile",
    "Gemfile",
    "go.mod",
    "Cargo.toml",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "build.sbt",
    "composer.json",
    "mix.exs",
    "Package.swift",
    "pubspec.yaml",
];

/// Directories never walked: version control, caches, editor and CI
/// settings.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".cache",
    ".gradle",
    "__pycache__",
    ".mockingbird",
    ".idea",
    ".vscode",
    ".github",
    ".gitlab",
    ".circleci",
    ".husky",
];

/// The app's build outputs, skipped outside dependencies only: packages
/// ship their API descriptions in `dist/` and `build/`.
const BUILD_DIRS: &[&str] = &[
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    "coverage",
];

/// The app's test folders: walked for API descriptions, not for URL
/// literals (tests name hosts the app does not call).
const TEST_DIRS: &[&str] = &["test", "tests", "__tests__", "spec", "specs", "e2e"];

/// Lock files: their URLs are package registries, not hosts the app calls.
/// Package manifests ([`MANIFESTS`]) are left out of the URL search alike
/// (their repository and homepage links).
const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "composer.lock",
    "Gemfile.lock",
    "Cargo.lock",
    "poetry.lock",
    "Pipfile.lock",
];

/// Directories of installed dependencies: walked after the app's own
/// files, for API descriptions only.
const DEPENDENCY_DIRS: &[&str] = &[
    "node_modules",
    "bower_components",
    "jspm_packages",
    "vendor",
    ".venv",
    "venv",
    "site-packages",
    ".tox",
    ".gems",
    ".bundle",
    "Pods",
    ".yarn",
];

/// Directories inside dependencies that hold their tests and samples,
/// whose API descriptions are not ones the app calls.
const DEPENDENCY_SKIP_DIRS: &[&str] = &[
    "test",
    "tests",
    "__tests__",
    "fixtures",
    "__fixtures__",
    "example",
    "examples",
];

/// Extensions of files that may be an API description.
const SPEC_EXTENSIONS: &[&str] = &["json", "yaml", "yml"];

/// Extensions of the app's own files searched for URL literals (code and
/// configuration, not documentation).
const SOURCE_EXTENSIONS: &[&str] = &[
    "js",
    "mjs",
    "cjs",
    "jsx",
    "ts",
    "mts",
    "cts",
    "tsx",
    "vue",
    "svelte",
    "py",
    "rb",
    "go",
    "rs",
    "java",
    "kt",
    "kts",
    "scala",
    "groovy",
    "php",
    "cs",
    "fs",
    "swift",
    "m",
    "dart",
    "ex",
    "exs",
    "erl",
    "clj",
    "lua",
    "pl",
    "sh",
    "json",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "properties",
    "xml",
    "env",
];

/// Source files named per occurrence of a host.
const MAX_SOURCE_REFS: usize = 3;

/// Distinct hosts kept from URL literals.
const MAX_LITERAL_HOSTS: usize = 5_000;

/// `next_steps` naming hosts (one per registrable domain); the rest are
/// counted in one more step.
const MAX_STEPS: usize = 8;

/// Characters of one domain's step (about; the host labels are cut to fit).
const MAX_STEP_CHARS: usize = 500;

/// Domains named in the summary line; the rest are counted.
const SUMMARY_DOMAINS: usize = 4;

/// Directory entries the cache fingerprint looks at; past it the scan is
/// not cached.
const MAX_FINGERPRINT_ENTRIES: usize = 20_000;

/// An API description that serves a host.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SpecRef {
    /// Relative to the project root, with `/` separators.
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Operations it serves on this host.
    pub operations: usize,
}

/// A catalog example whose mock lists a host.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ExampleRef {
    pub example: String,
    pub mock: String,
    pub contract: String,
}

/// Why a host is in the list, strongest first.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Evidence {
    /// An API description bundled in the project serves it.
    Spec,
    /// A catalog example's mock lists it (and the app's files name it).
    Catalog,
    /// Only a URL literal in the app's files names it, beside a described
    /// or mocked host of its domain: listed, but `verify
    /// --no-unclaimed-hosts` does not fail on it.
    Literal,
}

/// What a scan cost and whether it saw the whole project.
#[derive(Serialize, Debug, Clone, Default)]
pub struct ScanStats {
    /// Directory entries looked at.
    pub files_visited: usize,
    /// Files opened and read (in part or whole).
    pub files_read: usize,
    pub bytes_read: u64,
    pub elapsed_ms: u64,
    /// API descriptions found.
    pub specs: usize,
    /// API descriptions skipped as larger than the per-file limit.
    #[serde(skip_serializing_if = "is_zero")]
    pub skipped_large: usize,
    /// False when a limit stopped the scan.
    pub complete: bool,
    /// Which limits stopped it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stopped: Vec<String>,
    /// Answered from the cache of an earlier scan.
    pub cached: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// A scan's findings before they are matched against a config.
#[derive(Debug, Default, Clone)]
struct RawScan {
    /// Host to the API descriptions serving it.
    specs: BTreeMap<String, Vec<SpecRef>>,
    /// Host to the app's files with a URL literal naming it.
    literals: BTreeMap<String, Vec<String>>,
    stats: ScanStats,
    /// The time budget stopped it (such a scan is cached for a minute only).
    timed_out: bool,
}

/// A host the project can call that no mock lists.
#[derive(Serialize, Debug, Clone)]
pub struct UnclaimedHost {
    pub host: String,
    /// Its registrable domain (eTLD+1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    pub evidence: Evidence,
    /// The API descriptions in the project that serve it. Several (versions
    /// of one product) are all needed, not alternatives.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub specs: Vec<SpecRef>,
    /// The app's files with a URL literal naming it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// Catalog examples whose mocks list it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<ExampleRef>,
}

impl UnclaimedHost {
    /// Whether `verify --no-unclaimed-hosts` fails on it: a bundled
    /// description or the catalog serves it.
    #[must_use]
    pub fn fails_verify(&self) -> bool {
        self.evidence != Evidence::Literal
    }
}

/// The scan of a config's project: the hosts no mock lists and what to do
/// about them.
#[derive(Serialize, Debug)]
pub struct Answer {
    pub project_root: String,
    pub config: String,
    pub scan: ScanStats,
    /// One line for the whole list; absent when it is empty
    /// and the scan complete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub unclaimed: Vec<UnclaimedHost>,
    /// Hosts the scan found that a mock lists.
    pub claimed: Vec<String>,
    /// One per registrable domain, with what to import.
    pub next_steps: Vec<String>,
    /// The claimed hosts, for [`Answer::summary_without`].
    #[serde(skip)]
    claimed_all: BTreeSet<String>,
}

impl Answer {
    /// The summary line without the hosts in `exclude` (say, hosts a
    /// blocked-host step already names); `None` when nothing is left and
    /// the scan was complete.
    #[must_use]
    pub fn summary_without(&self, exclude: &HashSet<String>) -> Option<String> {
        let left: Vec<UnclaimedHost> = self
            .unclaimed
            .iter()
            .filter(|h| !exclude.contains(&h.host))
            .cloned()
            .collect();
        summary(&left, &self.claimed_all, &self.scan)
    }
}

/// Whether `dir` is the home folder or a filesystem root, which are never
/// scanned as a project.
fn too_broad(dir: &Path) -> bool {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    dir.parent().is_none() || home.as_deref() == Some(dir)
}

/// The project root of `config`: the nearest ancestor of its directory
/// (the directory included) holding `.git` or a package manifest
/// (`package.json`, `go.mod`, ...), not climbing to the home directory or
/// the filesystem root; else the config's directory.
pub fn project_root(config: &Path) -> PathBuf {
    let absolute = std::path::absolute(config).unwrap_or_else(|_| config.to_path_buf());
    let dir = absolute
        .canonicalize()
        .unwrap_or(absolute)
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    for candidate in dir.ancestors() {
        let is_top = too_broad(candidate);
        if is_top && candidate != dir {
            break;
        }
        if candidate.join(".git").exists() || MANIFESTS.iter().any(|m| candidate.join(m).is_file())
        {
            return candidate.to_path_buf();
        }
        if is_top {
            break;
        }
    }
    dir
}

/// What a scan leaves out besides the fixed lists: the config's folder
/// (when it is not the root) and the files its mocks name (contracts and
/// world descriptions).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct Skip {
    dir: Option<PathBuf>,
    files: BTreeSet<PathBuf>,
}

/// Scans the project of `config` and matches it against the config's mocks
/// (and `also_claimed`, such as the hosts a running server's mocks list).
///
/// # Errors
///
/// Fails when the config cannot be read or parsed, or its project root is
/// the home folder or a filesystem root.
pub fn project_hosts(config: &Path, also_claimed: &HashSet<String>) -> anyhow::Result<Answer> {
    let cfg = crate::file_config::load(config)?;
    let mut claimed: BTreeSet<String> = cfg
        .mocks
        .values()
        .flat_map(|m| m.hosts.iter())
        .chain(cfg.proxy.images.iter())
        .filter_map(|h| crate::registry::normalize_host(h))
        .collect();
    claimed.extend(also_claimed.iter().cloned());
    let root = project_root(config);
    if too_broad(&root) {
        bail!(
            "not scanning {} for hosts: it is the home folder or a filesystem root; keep the config in the app's project folder",
            root.display()
        );
    }
    let config_dir = std::path::absolute(config)
        .ok()
        .and_then(|c| c.canonicalize().ok())
        .and_then(|c| c.parent().map(Path::to_path_buf));
    let skip = Skip {
        dir: config_dir.filter(|d| *d != root),
        files: cfg
            .mocks
            .values()
            .flat_map(|m| std::iter::once(&m.contract).chain(m.world.iter()))
            .filter_map(|p| std::path::absolute(p).ok()?.canonicalize().ok())
            .collect(),
    };
    let raw = cached_scan(&root, &skip, Limits::default());
    let (unclaimed, found_claimed) = classify(&raw, &claimed, &crate::examples::catalog_hosts());
    let list = format!("`mockingbird hosts -c {}`", config.display());
    Ok(Answer {
        project_root: root.display().to_string(),
        config: config.display().to_string(),
        summary: summary(&unclaimed, &claimed, &raw.stats),
        next_steps: next_steps(&unclaimed, &claimed, &raw.stats, &list),
        scan: raw.stats,
        unclaimed,
        claimed: found_claimed,
        claimed_all: claimed,
    })
}

/// [`project_hosts`] off the async runtime's threads.
///
/// # Errors
///
/// As [`project_hosts`].
pub async fn project_hosts_async(
    config: PathBuf,
    also_claimed: HashSet<String>,
) -> anyhow::Result<Answer> {
    tokio::task::spawn_blocking(move || project_hosts(&config, &also_claimed)).await?
}

/// `mockingbird hosts`: the scan of a config's project as JSON.
///
/// # Errors
///
/// Fails when the config cannot be read or parsed.
pub async fn run(args: &HostsArgs) -> anyhow::Result<serde_json::Value> {
    let answer = project_hosts_async(args.config.clone(), HashSet::new()).await?;
    Ok(serde_json::to_value(answer)?)
}

/// Splits the scanned hosts into those no claimed host covers (with their
/// [`Evidence`] and the catalog examples listing each) and those a mock
/// lists. A URL literal counts when its host shares a registrable domain
/// with a described or claimed host, or is a `catalog` host itself.
fn classify(
    raw: &RawScan,
    claimed: &BTreeSet<String>,
    catalog: &[String],
) -> (Vec<UnclaimedHost>, Vec<String>) {
    let known: HashSet<&str> = raw
        .specs
        .keys()
        .map(String::as_str)
        .chain(claimed.iter().map(String::as_str))
        .filter_map(registrable_domain)
        .collect();
    let catalog: HashSet<&str> = catalog.iter().map(String::as_str).collect();
    let mut found: BTreeMap<&str, UnclaimedHost> = BTreeMap::new();
    for (host, specs) in &raw.specs {
        // A description that names the host but serves no operation there
        // (a sample, a top-level server its paths override) is left out
        // beside one that does.
        let serving: Vec<SpecRef> = specs.iter().filter(|s| s.operations > 0).cloned().collect();
        found.entry(host).or_insert_with(|| new_host(host)).specs = if serving.is_empty() {
            specs.clone()
        } else {
            serving
        };
    }
    for (host, sources) in &raw.literals {
        let in_known_domain = registrable_domain(host).is_some_and(|d| known.contains(d));
        if in_known_domain || catalog.contains(host.as_str()) {
            found
                .entry(host)
                .or_insert_with(|| new_host(host))
                .sources
                .clone_from(sources);
        }
    }
    let mut unclaimed = Vec::new();
    let mut found_claimed = Vec::new();
    for (host, mut h) in found {
        if claimed.contains(host) {
            found_claimed.push(host.to_owned());
            continue;
        }
        h.examples = crate::examples::serving(host)
            .into_iter()
            .map(|s| ExampleRef {
                example: s.example.to_owned(),
                mock: s.mock,
                contract: s.contract,
            })
            .collect();
        h.evidence = if !h.specs.is_empty() {
            Evidence::Spec
        } else if !h.examples.is_empty() {
            Evidence::Catalog
        } else {
            Evidence::Literal
        };
        unclaimed.push(h);
    }
    (unclaimed, found_claimed)
}

fn new_host(host: &str) -> UnclaimedHost {
    UnclaimedHost {
        host: host.to_owned(),
        domain: registrable_domain(host).map(str::to_owned),
        evidence: Evidence::Literal,
        specs: Vec::new(),
        sources: Vec::new(),
        examples: Vec::new(),
    }
}

/// `unclaimed` by registrable domain, in report order: domains with a
/// mocked host first, then those with a description or catalog entry,
/// then the rest, by name.
fn domains<'a>(
    unclaimed: &'a [UnclaimedHost],
    claimed: &BTreeSet<String>,
) -> Vec<(String, Vec<&'a UnclaimedHost>)> {
    let mut groups: BTreeMap<String, Vec<&UnclaimedHost>> = BTreeMap::new();
    for h in unclaimed {
        let key = h.domain.clone().unwrap_or_else(|| h.host.clone());
        groups.entry(key).or_default().push(h);
    }
    let mut ordered: Vec<(String, Vec<&UnclaimedHost>)> = groups.into_iter().collect();
    ordered.sort_by_cached_key(|(domain, hosts)| {
        (
            mocked_of(claimed, domain).is_empty(),
            hosts.iter().all(|h| h.evidence == Evidence::Literal),
            domain.clone(),
        )
    });
    ordered
}

/// The claimed hosts of `domain`.
fn mocked_of<'a>(claimed: &'a BTreeSet<String>, domain: &str) -> Vec<&'a str> {
    claimed
        .iter()
        .map(String::as_str)
        .filter(|c| registrable_domain(c) == Some(domain))
        .collect()
}

/// One line for the list (about 300 characters): how many hosts have no
/// mock, per domain (with how many have a bundled description), and where
/// the detail is. `None` when the list is empty and the scan complete.
fn summary(
    unclaimed: &[UnclaimedHost],
    claimed: &BTreeSet<String>,
    stats: &ScanStats,
) -> Option<String> {
    let stopped = (!stats.complete).then(|| {
        format!(
            " The scan stopped early ({}), so the list may be incomplete.",
            stats.stopped.join("; ")
        )
    });
    if unclaimed.is_empty() {
        return stopped.map(|s| format!("No host found without a mock.{s}"));
    }
    let groups = domains(unclaimed, claimed);
    let mut parts: Vec<String> = groups
        .iter()
        .take(SUMMARY_DOMAINS)
        .map(|(domain, hosts)| {
            let described = hosts
                .iter()
                .filter(|h| h.evidence == Evidence::Spec)
                .count();
            let listed = hosts
                .iter()
                .filter(|h| h.evidence == Evidence::Catalog)
                .count();
            let mut detail = Vec::new();
            if described > 0 {
                detail.push(format!("{described} with a bundled description"));
            }
            if listed > 0 {
                detail.push(format!("{listed} in the catalog"));
            }
            if detail.is_empty() {
                format!("{domain} {}", hosts.len())
            } else {
                format!("{domain} {} ({})", hosts.len(), detail.join(", "))
            }
        })
        .collect();
    if groups.len() > SUMMARY_DOMAINS {
        parts.push(format!("{} more domains", groups.len() - SUMMARY_DOMAINS));
    }
    let n = unclaimed.len();
    Some(format!(
        "{n} host{} the project can call ha{} no mock: {}. project_hosts (`mockingbird hosts`) lists each with what to import; mock those the app's flows use, or name why a domain is left out.{}",
        plural(n),
        if n == 1 { "s" } else { "ve" },
        parts.join(", "),
        stopped.unwrap_or_default()
    ))
}

/// The `next_steps` for `unclaimed`: one per registrable domain (in
/// [`domains`] order), each about [`MAX_STEP_CHARS`] long, naming its
/// hosts by short labels under the directory of their descriptions; the
/// detail is in `unclaimed`. At most [`MAX_STEPS`]; one more counts the
/// rest and names `list`, and one says when the scan stopped early.
fn next_steps(
    unclaimed: &[UnclaimedHost],
    claimed: &BTreeSet<String>,
    stats: &ScanStats,
    list: &str,
) -> Vec<String> {
    let mut steps = Vec::new();
    let mut rest_hosts = 0;
    let mut rest_domains = 0;
    for (i, (domain, hosts)) in domains(unclaimed, claimed).into_iter().enumerate() {
        if i >= MAX_STEPS {
            rest_hosts += hosts.len();
            rest_domains += 1;
            continue;
        }
        steps.push(domain_step(&domain, &hosts, &mocked_of(claimed, &domain)));
    }
    if rest_hosts > 0 {
        steps.push(format!(
            "And {rest_hosts} more host{} in {rest_domains} more domain{} have no mock: {list} lists them all.",
            plural(rest_hosts),
            plural(rest_domains)
        ));
    }
    if !stats.complete {
        steps.push(format!(
            "The host scan stopped early ({}) after {} files and {} bytes, so hosts past that point are not listed.",
            stats.stopped.join("; "),
            stats.files_visited,
            stats.bytes_read
        ));
    }
    steps
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `host` without its `domain` suffix (`verify` for `verify.vendor.com`);
/// the domain itself stays whole.
fn label(host: &str, domain: &str) -> String {
    host.strip_suffix(domain)
        .and_then(|l| l.strip_suffix('.'))
        .filter(|l| !l.is_empty())
        .unwrap_or(host)
        .to_owned()
}

/// The longest directory every path in `paths` lies in (`""` for none).
fn common_dir<'a>(paths: impl IntoIterator<Item = &'a str>) -> String {
    let mut common: Option<Vec<&str>> = None;
    for path in paths {
        let dirs: Vec<&str> = path.split('/').collect();
        let dirs = &dirs[..dirs.len().saturating_sub(1)];
        common = Some(match common {
            None => dirs.to_vec(),
            Some(c) => c
                .iter()
                .zip(dirs)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    common.unwrap_or_default().join("/")
}

/// `items` joined by `, ` within `budget` characters, the rest counted
/// (`… and 4 more`).
fn fit(items: &[String], budget: usize) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        let more = format!(", … and {} more", items.len() - i);
        let sep = if i == 0 { "" } else { ", " };
        let last = i + 1 == items.len();
        let needed = out.len() + sep.len() + item.len() + if last { 0 } else { more.len() };
        if i > 0 && needed > budget {
            out.push_str(&more);
            return out;
        }
        out.push_str(sep);
        out.push_str(item);
    }
    out
}

/// One domain's step: its hosts by kind, each kind with what to do.
fn domain_step(domain: &str, hosts: &[&UnclaimedHost], mocked: &[&str]) -> String {
    let mut head = format!(
        "{domain}: {} host{} without a mock",
        hosts.len(),
        plural(hosts.len())
    );
    if !mocked.is_empty() {
        let labels: Vec<String> = mocked.iter().map(|m| label(m, domain)).collect();
        let _ = write!(head, " (mocked: {})", labels.join(", "));
    }
    head.push('.');
    let of = |e: Evidence| -> Vec<&UnclaimedHost> {
        hosts.iter().copied().filter(|h| h.evidence == e).collect()
    };
    let (described, listed, named) = (
        of(Evidence::Spec),
        of(Evidence::Catalog),
        of(Evidence::Literal),
    );

    // The fixed text first; the host lists share what is left.
    let mut tails: Vec<(String, Vec<String>, String)> = Vec::new();
    if !described.is_empty() {
        let mut described = described;
        // The largest APIs first: the likeliest the app's flows use.
        let ops = |h: &UnclaimedHost| h.specs.iter().map(|s| s.operations).sum::<usize>();
        described.sort_by(|a, b| ops(b).cmp(&ops(a)).then_with(|| a.host.cmp(&b.host)));
        let dir = common_dir(
            described
                .iter()
                .flat_map(|h| h.specs.iter().map(|s| s.path.as_str())),
        );
        let several = described.iter().any(|h| h.specs.len() > 1);
        let labels = described
            .iter()
            .map(|h| match h.specs.len() {
                1 => label(&h.host, domain),
                n => format!("{} ({n})", label(&h.host, domain)),
            })
            .collect();
        let place = if dir.is_empty() {
            String::new()
        } else {
            format!(" in {dir}/")
        };
        let versions = if several {
            " (n) = n versioned descriptions, all needed: one mock each listing the host, with route_prefix plus path_rewrite.add_prefix set to the same value (its version path, e.g. /v2)."
        } else {
            ""
        };
        tails.push((
            format!(" {} with a bundled description{place}: ", described.len()),
            labels,
            format!(". import_contract each.{versions}"),
        ));
    }
    if !listed.is_empty() {
        let labels = listed
            .iter()
            .map(|h| {
                let examples: Vec<&str> = h.examples.iter().map(|e| e.example.as_str()).collect();
                format!("{} (`{}`)", label(&h.host, domain), examples.join("`, `"))
            })
            .collect();
        tails.push((
            format!(" {} in the catalog: ", listed.len()),
            labels,
            ". init_example.".to_owned(),
        ));
    }
    if !named.is_empty() {
        let labels = named.iter().map(|h| label(&h.host, domain)).collect();
        tails.push((
            format!(" {} only named in the app's files: ", named.len()),
            labels,
            ".".to_owned(),
        ));
    }
    let also: Vec<String> = hosts
        .iter()
        .filter(|h| h.evidence == Evidence::Spec && !h.examples.is_empty())
        .map(|h| format!("{} (`{}`)", label(&h.host, domain), h.examples[0].example))
        .collect();
    let also = if also.is_empty() {
        String::new()
    } else {
        format!(" Also in the catalog: {}.", also.join(", "))
    };
    let fixed: usize = head.len()
        + also.len()
        + tails
            .iter()
            .map(|(lead, _, end)| lead.len() + end.len())
            .sum::<usize>();
    let budget = MAX_STEP_CHARS.saturating_sub(fixed).max(60);
    let total: usize = tails
        .iter()
        .map(|(_, labels, _)| labels.iter().map(|l| l.len() + 2).sum::<usize>())
        .sum::<usize>()
        .max(1);
    let mut step = head;
    for (lead, labels, end) in &tails {
        let own: usize = labels.iter().map(|l| l.len() + 2).sum();
        // Each list gets a share of the budget by its length.
        let share = (budget * own / total).max(30);
        let _ = write!(step, "{lead}{}{end}", fit(labels, share));
    }
    step.push_str(&also);
    step
}

/// Scans cached per (root, skips) with their [`fingerprint`] and when they
/// were stored.
type Cache = HashMap<(PathBuf, Skip), (SystemTime, Instant, Arc<RawScan>)>;

/// How long a scan the time budget stopped is reused: a project that
/// always takes longer than the budget is not rescanned on every call,
/// and one that only ran slow once gets a full scan soon.
const TIMED_OUT_TTL: Duration = Duration::from_secs(60);

/// Whether a cached scan stored at `stored` may answer at `now`: always
/// while the fingerprint holds, or for [`TIMED_OUT_TTL`] when the time
/// budget stopped it.
fn reusable(timed_out: bool, stored: Instant, now: Instant) -> bool {
    !timed_out || now.saturating_duration_since(stored) < TIMED_OUT_TTL
}

/// Projects kept in the cache.
const CACHE_ENTRIES: usize = 16;

/// [`scan`], from the cache while the root's [`fingerprint`] holds (a scan
/// the time budget stopped, for [`TIMED_OUT_TTL`] only).
fn cached_scan(root: &Path, skip: &Skip, limits: Limits) -> RawScan {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (root.to_path_buf(), skip.clone());
    let print = fingerprint(root, skip);
    if let (Ok(cache), Some(print)) = (cache.lock(), print)
        && let Some((at, stored, scan)) = cache.get(&key)
        && *at == print
        && reusable(scan.timed_out, *stored, Instant::now())
    {
        let mut scan = (**scan).clone();
        scan.stats.cached = true;
        return scan;
    }
    let scan = scan(root, skip, limits);
    if let (Ok(mut cache), Some(print)) = (cache.lock(), print) {
        if cache.len() >= CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(key, (print, Instant::now(), Arc::new(scan.clone())));
    }
    scan
}

/// [`fingerprint`] of the whole tree below `root` (nothing skipped but the
/// fixed folders), for other caches of what lies in a project
/// ([`crate::test_commands::detect_all`]).
pub(crate) fn tree_fingerprint(root: &Path) -> Option<SystemTime> {
    fingerprint(root, &Skip::default())
}

/// The newest modification time of the root and the entries one and two
/// levels below it (not the skipped folders): adding or removing a file
/// there, or installing a dependency, changes it; an edit deeper down does
/// not. `None` (not cacheable) past [`MAX_FINGERPRINT_ENTRIES`] entries.
fn fingerprint(root: &Path, skip: &Skip) -> Option<SystemTime> {
    let mtime = |p: &Path| std::fs::symlink_metadata(p).and_then(|m| m.modified()).ok();
    let mut newest = mtime(root)?;
    let mut seen = 0usize;
    let mut bump = |t: Option<SystemTime>| -> bool {
        seen += 1;
        if let Some(t) = t
            && t > newest
        {
            newest = t;
        }
        seen <= MAX_FINGERPRINT_ENTRIES
    };
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if SKIP_DIRS.contains(&name.to_string_lossy().as_ref())
            || skip.dir.as_deref() == Some(path.as_path())
        {
            continue;
        }
        if !bump(mtime(&path)) {
            return None;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir())
            && let Ok(children) = std::fs::read_dir(&path)
        {
            for child in children.flatten() {
                if !bump(mtime(&child.path())) {
                    return None;
                }
            }
        }
    }
    Some(newest)
}

/// Walks `root` (not `skip`, symlinks or [`SKIP_DIRS`]): the app's own
/// files first, then its dependency folders.
fn scan(root: &Path, skip: &Skip, limits: Limits) -> RawScan {
    let mut walker = Walker {
        root,
        skip,
        limits,
        start: Instant::now(),
        raw: RawScan::default(),
        stopped: None,
    };
    let mut deps = Vec::new();
    walker.walk(root.to_path_buf(), Place::App, &mut deps);
    for dir in deps {
        walker.walk(dir, Place::Dependency, &mut Vec::new());
    }
    let mut raw = walker.raw;
    raw.stats.elapsed_ms = u64::try_from(walker.start.elapsed().as_millis()).unwrap_or(u64::MAX);
    raw.stats.complete = walker.stopped.is_none();
    raw.stats.stopped = walker.stopped.into_iter().collect();
    raw.stats.specs = {
        let files: HashSet<&str> = raw
            .specs
            .values()
            .flatten()
            .map(|s| s.path.as_str())
            .collect();
        files.len()
    };
    raw
}

/// Where a folder lies: the app's own files, its tests, or a dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    App,
    AppTests,
    Dependency,
}

struct Walker<'a> {
    root: &'a Path,
    skip: &'a Skip,
    limits: Limits,
    start: Instant,
    raw: RawScan,
    /// The limit that stopped the walk.
    stopped: Option<String>,
}

impl Walker<'_> {
    /// Whether a limit stopped the walk (checking the clock and the bytes).
    fn stop(&mut self) -> bool {
        if self.stopped.is_none() {
            if self.start.elapsed() >= self.limits.budget {
                self.raw.timed_out = true;
                self.stopped = Some(format!(
                    "time budget of {} ms",
                    self.limits.budget.as_millis()
                ));
            } else if self.raw.stats.bytes_read >= self.limits.max_total_bytes {
                self.stopped = Some(format!("{} bytes read in all", self.limits.max_total_bytes));
            } else if self.raw.stats.files_visited >= self.limits.max_files {
                self.stopped = Some(format!("{} files visited", self.limits.max_files));
            }
        }
        self.stopped.is_some()
    }

    /// Walks `dir`; in the app's files a dependency folder is queued in
    /// `deps` instead of entered.
    fn walk(&mut self, dir: PathBuf, place: Place, deps: &mut Vec<PathBuf>) {
        let mut stack = vec![(dir, place)];
        while let Some((dir, place)) = stack.pop() {
            if self.stop() {
                return;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut entries: Vec<std::fs::DirEntry> = entries.flatten().collect();
            // Popped from the end: reverse order keeps the walk alphabetical.
            entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
            let mut files = Vec::new();
            for entry in entries {
                if self.stop() {
                    return;
                }
                self.raw.stats.files_visited += 1;
                // `file_type` does not follow symlinks: they are skipped, so
                // the walk never leaves the root.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                let name = name.as_str();
                let path = entry.path();
                if kind.is_dir() {
                    let in_deps = place == Place::Dependency;
                    if SKIP_DIRS.contains(&name)
                        || self.skip.dir.as_deref() == Some(path.as_path())
                        || (!in_deps && BUILD_DIRS.contains(&name))
                        || (in_deps && DEPENDENCY_SKIP_DIRS.contains(&name))
                    {
                        continue;
                    }
                    if !in_deps && DEPENDENCY_DIRS.contains(&name) {
                        deps.push(path);
                    } else if place == Place::App && TEST_DIRS.contains(&name) {
                        stack.push((path, Place::AppTests));
                    } else {
                        stack.push((path, place));
                    }
                } else if kind.is_file() && !self.skip.files.contains(&path) {
                    files.push((path, name.to_owned()));
                }
            }
            for (path, name) in files.into_iter().rev() {
                if self.stop() {
                    return;
                }
                self.file(&path, &name, place);
            }
        }
    }

    /// Reads up to `max` bytes of `path`, within the bytes left in all.
    fn read(&mut self, path: &Path, max: u64) -> Option<Vec<u8>> {
        let left = self
            .limits
            .max_total_bytes
            .saturating_sub(self.raw.stats.bytes_read);
        let max = max.min(left);
        if max == 0 {
            return None;
        }
        let file = std::fs::File::open(path).ok()?;
        let mut bytes = Vec::new();
        file.take(max).read_to_end(&mut bytes).ok()?;
        self.raw.stats.files_read += 1;
        self.raw.stats.bytes_read += bytes.len() as u64;
        Some(bytes)
    }

    fn file(&mut self, path: &Path, name: &str, place: Place) {
        let ext = extension(name);
        let spec_ext = SPEC_EXTENSIONS.contains(&ext.as_str());
        let source = place == Place::App
            && (SOURCE_EXTENSIONS.contains(&ext.as_str()) || name.starts_with(".env"))
            && !MANIFESTS.contains(&name)
            && !LOCKFILES.contains(&name);
        if !spec_ext && !source {
            return;
        }
        let Ok(len) = std::fs::symlink_metadata(path).map(|m| m.len()) else {
            return;
        };
        let rel = relative(self.root, path);
        if place != Place::Dependency {
            // The app's own files are read whole (up to the per-file limit):
            // a description is sniffed over all of it, and a file that is
            // none is searched for URLs (outside tests).
            let Some(bytes) = self.read(path, self.limits.max_file_bytes) else {
                return;
            };
            if bytes.iter().take(HEAD as usize).any(|b| *b == 0) {
                return;
            }
            if spec_ext && sniff(&bytes, &ext) {
                if len > self.limits.max_file_bytes {
                    self.raw.stats.skipped_large += 1;
                    return;
                }
                if self.spec(&bytes, &ext, &rel) {
                    return;
                }
            }
            if source {
                self.literals(&bytes, &rel);
            }
            return;
        }
        let Some(head) = self.read(path, HEAD) else {
            return;
        };
        if head.contains(&0) || !sniff(&head, &ext) {
            return;
        }
        if len > self.limits.max_file_bytes {
            self.raw.stats.skipped_large += 1;
            return;
        }
        let bytes = if len > head.len() as u64 {
            match self.read(path, self.limits.max_file_bytes) {
                Some(bytes) => bytes,
                None => return,
            }
        } else {
            head
        };
        self.spec(&bytes, &ext, &rel);
    }

    /// Records the hosts of `bytes` when it is an API description; whether
    /// it is one.
    fn spec(&mut self, bytes: &[u8], ext: &str, rel: &str) -> bool {
        let Some((title, hosts)) = parse_spec(bytes, ext) else {
            return false;
        };
        for (host, operations) in hosts {
            self.raw.specs.entry(host).or_default().push(SpecRef {
                path: rel.to_owned(),
                title: title.clone(),
                operations,
            });
        }
        true
    }

    fn literals(&mut self, bytes: &[u8], rel: &str) {
        for host in url_hosts(bytes) {
            let full = self.raw.literals.len() >= MAX_LITERAL_HOSTS;
            if full && !self.raw.literals.contains_key(&host) {
                continue;
            }
            let files = self.raw.literals.entry(host).or_default();
            if files.len() < MAX_SOURCE_REFS && !files.iter().any(|f| f == rel) {
                files.push(rel.to_owned());
            }
        }
    }
}

/// At most this many files [`calls_in_source`] reads.
const MAX_CALL_FILES: usize = 20_000;
/// The extensions of the code files [`calls_in_source`] reads: a document, a page, a
/// stylesheet or data that names a field is not a call.
const CODE_EXTENSIONS: &[&str] = &[
    "js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "vue", "svelte", "py", "rb", "go",
    "java", "kt", "kts", "scala", "rs", "php", "cs", "fs", "swift", "dart", "ex", "exs", "clj",
    "cljs", "cljc", "elm", "astro", "graphql", "gql",
];
/// Files larger than this are not read by [`calls_in_source`] (a schema, a bundle).
const MAX_CALL_FILE_BYTES: u64 = 512 * 1024;

/// Which of `names` the app's own files under `root` call as a field with arguments
/// (`user(login: $login)`): the name, not part of a longer identifier, then `(`. The
/// walk reads code files only ([`CODE_EXTENSIONS`]) and leaves out [`SKIP_DIRS`],
/// dependencies and build output (`node_modules`, `vendor`, `target`, `dist`,
/// `build`), hidden directories, `skip` (the mocks' config folder and contracts, which
/// declare every field; an entry holding all of `root` is ignored) and files over
/// [`MAX_CALL_FILE_BYTES`], and stops after [`MAX_CALL_FILES`] files or once every
/// name is found.
pub(crate) fn calls_in_source(
    root: &Path,
    skip: &[PathBuf],
    names: &BTreeSet<String>,
) -> BTreeSet<String> {
    const BUILD_DIRS: &[&str] = &[
        "node_modules",
        "vendor",
        "target",
        "dist",
        "build",
        "coverage",
    ];
    // A skipped folder that holds the whole project (a config at its root, a relative
    // empty parent) would leave nothing to read.
    // Root and skips compared canonical (`/tmp` is `/private/tmp` on macOS; a
    // contract may be written with `..`).
    let root = &root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let skip: Vec<PathBuf> = skip
        .iter()
        .filter(|s| !s.as_os_str().is_empty())
        .map(|s| {
            std::path::absolute(s)
                .and_then(|a| a.canonicalize())
                .unwrap_or_else(|_| s.clone())
        })
        .filter(|s| !root.starts_with(s))
        .collect();
    let mut found = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    let mut files = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if skip.iter().any(|s| path.starts_with(s)) {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() {
                if !name.starts_with('.')
                    && !SKIP_DIRS.contains(&name.as_ref())
                    && !BUILD_DIRS.contains(&name.as_ref())
                {
                    stack.push(path);
                }
                continue;
            }
            if !kind.is_file()
                || !CODE_EXTENSIONS.contains(&extension(&name).as_str())
                || entry
                    .metadata()
                    .map_or(true, |m| m.len() > MAX_CALL_FILE_BYTES)
            {
                continue;
            }
            files += 1;
            if files > MAX_CALL_FILES {
                return found;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            for n in names {
                if !found.contains(n) && calls(&text, n) {
                    found.insert(n.clone());
                }
            }
            if found.len() == names.len() {
                return found;
            }
        }
    }
    found
}

/// Whether `text` has `name(`: `name` not inside a longer identifier, then optional
/// whitespace and `(`.
fn calls(text: &str, name: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(ident)
            && text[at + name.len()..].trim_start().starts_with('(')
    })
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether `head` (the start of a JSON or YAML file) may be an API
/// description. JSON: it starts with `{` and names one of the keys OpenAPI
/// or Swagger documents have at the top (`openapi`, `swagger`, `paths`,
/// `components`, `servers`, `basePath`) anywhere in it, since writers may
/// put `openapi` after a large `components`. YAML: a line starts with one
/// of those keys. The parse decides.
fn sniff(head: &[u8], ext: &str) -> bool {
    const KEYS: &[&str] = &[
        "openapi",
        "swagger",
        "paths",
        "components",
        "servers",
        "basePath",
    ];
    let text = String::from_utf8_lossy(head);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    if ext == "json" || text.starts_with('{') {
        return text.starts_with('{') && KEYS.iter().any(|k| text.contains(&format!("\"{k}\"")));
    }
    text.lines().any(|line| {
        KEYS.iter().any(|k| {
            line.strip_prefix(k)
                .or_else(|| {
                    line.strip_prefix('"')
                        .and_then(|l| l.strip_prefix(k))
                        .and_then(|l| l.strip_prefix('"'))
                })
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
        })
    })
}

/// A value that may have another shape than expected: an odd member (a
/// scalar where an object belongs) is ignored instead of failing the whole
/// document.
#[derive(Deserialize)]
#[serde(untagged)]
enum Lenient<T> {
    Valid(T),
    Other(serde::de::IgnoredAny),
}

impl<T> Lenient<T> {
    fn get(&self) -> Option<&T> {
        match self {
            Lenient::Valid(v) => Some(v),
            Lenient::Other(_) => None,
        }
    }
}

/// `Option<Lenient<T>>` as an `Option<&T>`.
fn ok<T>(v: Option<&Lenient<T>>) -> Option<&T> {
    v.and_then(Lenient::get)
}

type Servers = Lenient<Vec<Lenient<Server>>>;

#[derive(Deserialize)]
struct SpecDoc {
    openapi: Option<serde::de::IgnoredAny>,
    swagger: Option<serde::de::IgnoredAny>,
    info: Option<Lenient<Info>>,
    servers: Option<Servers>,
    host: Option<Lenient<String>>,
    paths: Option<Paths>,
}

#[derive(Deserialize)]
struct Info {
    title: Option<Lenient<String>>,
}

#[derive(Deserialize)]
struct Server {
    url: Option<Lenient<String>>,
    variables: Option<Lenient<BTreeMap<String, Lenient<Variable>>>>,
}

#[derive(Deserialize)]
struct Variable {
    default: Option<Lenient<String>>,
}

/// The HTTP methods of an OpenAPI path item.
const METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// A value read from a map, any other value (a scalar, a list) giving
/// `Default`: [`Paths`], [`PathItem`] and [`OpSlot`] skip what they do not
/// read without buffering it, unlike [`Lenient`], which suits small values.
trait FromMap: Default {
    fn from_map<'de, A: serde::de::MapAccess<'de>>(map: A) -> Result<Self, A::Error>;
}

struct OrDefault<T>(std::marker::PhantomData<T>);

impl<'de, T: FromMap> serde::de::Visitor<'de> for OrDefault<T> {
    type Value = T;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any value")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
        T::from_map(map)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<T, A::Error> {
        while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
        Ok(T::default())
    }

    fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<T, D::Error> {
        d.deserialize_any(self)
    }

    fn visit_bool<E>(self, _: bool) -> Result<T, E> {
        Ok(T::default())
    }

    fn visit_i64<E>(self, _: i64) -> Result<T, E> {
        Ok(T::default())
    }

    fn visit_u64<E>(self, _: u64) -> Result<T, E> {
        Ok(T::default())
    }

    fn visit_f64<E>(self, _: f64) -> Result<T, E> {
        Ok(T::default())
    }

    fn visit_str<E>(self, _: &str) -> Result<T, E> {
        Ok(T::default())
    }

    fn visit_bytes<E>(self, _: &[u8]) -> Result<T, E> {
        Ok(T::default())
    }

    fn visit_none<E>(self) -> Result<T, E> {
        Ok(T::default())
    }

    fn visit_unit<E>(self) -> Result<T, E> {
        Ok(T::default())
    }
}

/// `Deserialize` through [`OrDefault`].
macro_rules! from_map_deserialize {
    ($t:ty) => {
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                d.deserialize_any(OrDefault::<$t>(std::marker::PhantomData))
            }
        }
    };
}

/// The path items of `paths`.
#[derive(Default)]
struct Paths(Vec<PathItem>);

impl FromMap for Paths {
    fn from_map<'de, A: serde::de::MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut items = Vec::new();
        while map.next_key::<serde::de::IgnoredAny>()?.is_some() {
            items.push(map.next_value::<PathItem>()?);
        }
        Ok(Paths(items))
    }
}

#[derive(Default)]
struct PathItem {
    servers: Option<Servers>,
    operations: Vec<Operation>,
}

impl FromMap for PathItem {
    fn from_map<'de, A: serde::de::MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut item = PathItem::default();
        while let Some(key) = map.next_key::<String>()? {
            if key == "servers" {
                item.servers = Some(map.next_value()?);
            } else if METHODS.contains(&key.as_str()) {
                if let OpSlot(Some(op)) = map.next_value()? {
                    item.operations.push(op);
                }
            } else {
                map.next_value::<serde::de::IgnoredAny>()?;
            }
        }
        Ok(item)
    }
}

/// An operation, or `None` for a method member that is not an object.
#[derive(Default)]
struct OpSlot(Option<Operation>);

struct Operation {
    servers: Option<Servers>,
}

impl FromMap for OpSlot {
    fn from_map<'de, A: serde::de::MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut op = Operation { servers: None };
        while let Some(key) = map.next_key::<String>()? {
            if key == "servers" {
                op.servers = Some(map.next_value()?);
            } else {
                map.next_value::<serde::de::IgnoredAny>()?;
            }
        }
        Ok(OpSlot(Some(op)))
    }
}

from_map_deserialize!(Paths);
from_map_deserialize!(PathItem);
from_map_deserialize!(OpSlot);

/// An OpenAPI 3 or Swagger 2 document's title and, per host it names, the
/// operations served there; `None` when `bytes` is not one.
fn parse_spec(bytes: &[u8], ext: &str) -> Option<(Option<String>, BTreeMap<String, usize>)> {
    let doc: SpecDoc = if ext == "json" || bytes.trim_ascii_start().starts_with(b"{") {
        serde_json::from_slice(bytes).ok()?
    } else {
        serde_saphyr::from_str(std::str::from_utf8(bytes).ok()?).ok()?
    };
    if doc.openapi.is_none() && doc.swagger.is_none() {
        return None;
    }
    let mut top = server_hosts(ok(doc.servers.as_ref()));
    if doc.swagger.is_some()
        && let Some(host) = ok(doc.host.as_ref()).and_then(|h| url_host(&format!("https://{h}")))
    {
        top.push(host);
    }
    let mut hosts: BTreeMap<String, usize> = top.iter().map(|h| (h.clone(), 0)).collect();
    for item in doc.paths.iter().flat_map(|p| &p.0) {
        let path_hosts = Some(server_hosts(ok(item.servers.as_ref())))
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| top.clone());
        for host in &path_hosts {
            hosts.entry(host.clone()).or_insert(0);
        }
        for op in &item.operations {
            let op_hosts = Some(server_hosts(ok(op.servers.as_ref()))).filter(|h| !h.is_empty());
            for host in op_hosts.as_ref().unwrap_or(&path_hosts) {
                *hosts.entry(host.clone()).or_insert(0) += 1;
            }
        }
    }
    let title = ok(doc.info.as_ref())
        .and_then(|i| ok(i.title.as_ref()))
        .filter(|t| !t.is_empty())
        .cloned();
    Some((title, hosts))
}

/// The usable hosts of OpenAPI `servers`, with each `{variable}` replaced
/// by its default.
fn server_hosts(servers: Option<&Vec<Lenient<Server>>>) -> Vec<String> {
    let mut hosts: Vec<String> = servers
        .into_iter()
        .flatten()
        .filter_map(Lenient::get)
        .filter_map(|s| {
            let mut url = ok(s.url.as_ref())?.clone();
            for (name, var) in ok(s.variables.as_ref()).into_iter().flatten() {
                if let Some(default) = var.get().and_then(|v| ok(v.default.as_ref())) {
                    url = url.replace(&format!("{{{name}}}"), default);
                }
            }
            url_host(&url)
        })
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// The host of an absolute `http(s)://` URL when it is [`usable_host`].
fn url_host(url: &str) -> Option<String> {
    let lower = url.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = crate::registry::normalize_host(authority)?;
    usable_host(&host).then_some(host)
}

/// The hosts of the `http://` and `https://` URL literals in `bytes` that
/// are [`usable_host`]s.
fn url_hosts(bytes: &[u8]) -> BTreeSet<String> {
    let mut hosts = BTreeSet::new();
    let mut from = 0;
    while let Some(at) = bytes[from..]
        .windows(3)
        .position(|w| w == b"://")
        .map(|p| from + p)
    {
        from = at + 3;
        let before = &bytes[..at];
        if !(before.ends_with(b"http") || before.ends_with(b"https")) {
            continue;
        }
        let len = bytes[from..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || **b == b'.' || **b == b'-')
            .count();
        let Ok(host) = std::str::from_utf8(&bytes[from..from + len]) else {
            continue;
        };
        if let Some(host) = crate::registry::normalize_host(host)
            && usable_host(&host)
        {
            hosts.insert(host);
        }
    }
    hosts
}

/// Whether `host` (normalized) may be a real API host: a name with a
/// registrable domain, no template braces, not an IP address, `localhost`
/// or a reserved example or test name (RFC 2606, RFC 6761).
fn usable_host(host: &str) -> bool {
    const RESERVED: &[&str] = &["example.com", "example.net", "example.org"];
    const RESERVED_TLDS: &[&str] = &["example", "test", "invalid", "localhost", "local"];
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return false;
    }
    let Some(domain) = registrable_domain(host) else {
        return false;
    };
    let tld = host.rsplit('.').next().unwrap_or_default();
    !RESERVED.contains(&domain) && !RESERVED_TLDS.contains(&tld)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_called_with_arguments_is_found_outside_the_skipped_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("mock")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        std::fs::write(
            root.join("src/fetch.js"),
            "query { user(login: $login) { repository (name: $repo) { id } } }",
        )
        .unwrap();
        std::fs::write(
            root.join("mock/schema.graphql"),
            "type Query { license(key: String!): License }",
        )
        .unwrap();
        std::fs::write(root.join("node_modules/x/i.js"), "enterprise(slug: s)").unwrap();
        std::fs::write(
            root.join("src/other.js"),
            "superuser(1); codeOfConduct; licenses(2)",
        )
        .unwrap();
        let names: BTreeSet<String> = [
            "user",
            "repository",
            "license",
            "enterprise",
            "codeOfConduct",
        ]
        .map(String::from)
        .into();
        let found = calls_in_source(root, &[root.join("mock")], &names);
        assert_eq!(found, ["repository", "user"].map(String::from).into());
        // A config at the project root skips its folder: the root is still read.
        let at_root = calls_in_source(root, &[root.to_path_buf(), PathBuf::new()], &names);
        assert!(at_root.contains("user"), "{at_root:?}");
        // A document naming a field is not a call.
        std::fs::write(root.join("NOTES.md"), "license(key: x)").unwrap();
        let docs = calls_in_source(root, &[root.join("mock")], &names);
        assert!(!docs.contains("license"), "{docs:?}");
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const OPENAPI: &str = r#"{
  "components": {"schemas": {}},
  "info": {"title": "Checks API", "version": "1"},
  "openapi": "3.0.1",
  "paths": {
    "/v2/Checks": {
      "servers": [{"url": "https://checks.vendor-one.com"}],
      "get": {}, "post": {}
    },
    "/v2/Checks/{Sid}": {
      "servers": [{"url": "https://checks.vendor-one.com"}],
      "get": {}
    }
  },
  "servers": [{"url": "https://checks.vendor-one.com"}]
}"#;

    const SWAGGER: &str = "swagger: '2.0'\ninfo:\n  title: Ledger API\n  version: '1'\nhost: ledger.vendor-two.io\nbasePath: /v1\npaths:\n  /entries:\n    get: {}\n    post: {}\n";

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", "{\"name\": \"app\"}");
        write(
            root,
            "node_modules/@vendor-one/sdk/specs/vendor_one_checks.json",
            OPENAPI,
        );
        write(root, "vendor/ledger/api/ledger.yaml", SWAGGER);
        write(
            root,
            "mocks/mockingbird.yaml",
            "mocks:\n  api:\n    contract: api.json\n    hosts: [api.vendor-one.com]\n",
        );
        dir
    }

    fn answer(root: &Path) -> Answer {
        project_hosts(&root.join("mocks/mockingbird.yaml"), &HashSet::new()).unwrap()
    }

    fn unclaimed<'a>(a: &'a Answer, host: &str) -> Option<&'a UnclaimedHost> {
        a.unclaimed.iter().find(|h| h.host == host)
    }

    fn complete() -> ScanStats {
        ScanStats {
            complete: true,
            ..ScanStats::default()
        }
    }

    #[test]
    fn bundled_openapi_and_swagger_descriptions_are_found_and_named() {
        let dir = project();
        let a = answer(dir.path());
        let checks = unclaimed(&a, "checks.vendor-one.com").expect("OpenAPI 3 host");
        assert_eq!(checks.evidence, Evidence::Spec);
        assert_eq!(
            checks.specs,
            [SpecRef {
                path: "node_modules/@vendor-one/sdk/specs/vendor_one_checks.json".into(),
                title: Some("Checks API".into()),
                operations: 3,
            }]
        );
        let ledger = unclaimed(&a, "ledger.vendor-two.io").expect("Swagger 2 host");
        assert_eq!(ledger.specs[0].path, "vendor/ledger/api/ledger.yaml");
        assert_eq!(ledger.specs[0].title.as_deref(), Some("Ledger API"));
        assert_eq!(ledger.specs[0].operations, 2);
        assert_eq!(a.scan.specs, 2);
        assert!(a.scan.complete);
        // The mocked domain comes first, with what to import.
        assert_eq!(
            a.next_steps[0],
            "vendor-one.com: 1 host without a mock (mocked: api). 1 with a bundled description in node_modules/@vendor-one/sdk/specs/: checks. import_contract each."
        );
        assert_eq!(
            a.next_steps[1],
            "vendor-two.io: 1 host without a mock. 1 with a bundled description in vendor/ledger/api/: ledger. import_contract each."
        );
        assert_eq!(
            a.summary.as_deref(),
            Some(
                "2 hosts the project can call have no mock: vendor-one.com 1 (1 with a bundled description), vendor-two.io 1 (1 with a bundled description). project_hosts (`mockingbird hosts`) lists each with what to import; mock those the app's flows use, or name why a domain is left out."
            )
        );
        assert_eq!(
            a.project_root,
            dir.path().canonicalize().unwrap().display().to_string()
        );
    }

    /// npm packages ship their descriptions in `dist/`: build-output names
    /// are skipped in the app only.
    #[test]
    fn a_description_in_a_dependency_build_folder_is_found() {
        let dir = project();
        write(
            dir.path(),
            "node_modules/pkg/dist/api.json",
            &OPENAPI.replace("checks.vendor-one.com", "shipped.vendor-five.com"),
        );
        let a = answer(dir.path());
        assert!(unclaimed(&a, "shipped.vendor-five.com").is_some());
    }

    #[test]
    fn several_descriptions_of_one_host_are_all_needed() {
        let dir = project();
        write(
            dir.path(),
            "node_modules/@vendor-one/sdk/specs/vendor_one_checks_v3.json",
            &OPENAPI.replace("/v2/", "/v3/"),
        );
        let a = answer(dir.path());
        assert_eq!(
            unclaimed(&a, "checks.vendor-one.com").unwrap().specs.len(),
            2
        );
        assert_eq!(
            a.next_steps[0],
            "vendor-one.com: 1 host without a mock (mocked: api). 1 with a bundled description in node_modules/@vendor-one/sdk/specs/: checks (2). import_contract each. (n) = n versioned descriptions, all needed: one mock each listing the host, with route_prefix plus path_rewrite.add_prefix set to the same value (its version path, e.g. /v2)."
        );
    }

    #[test]
    fn a_host_a_mock_claims_gets_no_step() {
        let dir = project();
        write(
            dir.path(),
            "mocks/mockingbird.yaml",
            "mocks:\n  checks:\n    contract: c.json\n    hosts: [checks.vendor-one.com]\n  ledger:\n    contract: l.yaml\n    hosts: [ledger.vendor-two.io]\n",
        );
        let a = answer(dir.path());
        assert!(a.unclaimed.is_empty(), "{:?}", a.unclaimed);
        assert!(a.next_steps.is_empty(), "{:?}", a.next_steps);
        assert!(a.summary.is_none());
        assert_eq!(a.claimed, ["checks.vendor-one.com", "ledger.vendor-two.io"]);
        // A host a running server's mock lists counts as well.
        write(dir.path(), "mocks/mockingbird.yaml", "mocks: {}\n");
        let served: HashSet<String> = ["ledger.vendor-two.io".to_owned()].into();
        let a = project_hosts(&dir.path().join("mocks/mockingbird.yaml"), &served).unwrap();
        assert!(unclaimed(&a, "ledger.vendor-two.io").is_none());
        assert!(unclaimed(&a, "checks.vendor-one.com").is_some());
    }

    /// The mocks' own contracts and world files are not the app's, wherever
    /// the config sits (here at the root).
    #[test]
    fn the_mocks_own_contracts_are_skipped_with_the_config_at_the_root() {
        let dir = project();
        write(
            dir.path(),
            "contracts/billing.json",
            &OPENAPI.replace("checks.vendor-one.com", "billing.vendor-six.com"),
        );
        write(
            dir.path(),
            "contracts/billing.world.json",
            "{\"note\": \"https://world.vendor-six.com\"}",
        );
        write(
            dir.path(),
            "mockingbird.yaml",
            "mocks:\n  billing:\n    contract: contracts/billing.json\n    world: contracts/billing.world.json\n",
        );
        let a = project_hosts(&dir.path().join("mockingbird.yaml"), &HashSet::new()).unwrap();
        assert!(
            unclaimed(&a, "billing.vendor-six.com").is_none(),
            "{:?}",
            a.unclaimed
        );
        assert!(unclaimed(&a, "world.vendor-six.com").is_none());
        assert!(unclaimed(&a, "checks.vendor-one.com").is_some());
    }

    #[test]
    fn source_literals_count_only_beside_a_known_domain() {
        let dir = project();
        write(
            dir.path(),
            "src/client.js",
            "const BASE = 'https://billing.vendor-one.com/v1';\n// docs: https://docs.unrelated-site.org/guide\nfetch(`http://localhost:3000/x`); 'https://api.example.com';\n",
        );
        // Dependencies and the app's tests are not read for literals.
        write(
            dir.path(),
            "node_modules/x/index.js",
            "'https://deps.vendor-one.com'",
        );
        write(
            dir.path(),
            "test/client.test.js",
            "'https://tests.vendor-one.com'",
        );
        let a = answer(dir.path());
        let billing = unclaimed(&a, "billing.vendor-one.com").expect("kept");
        assert_eq!(billing.sources, ["src/client.js"]);
        assert_eq!(billing.evidence, Evidence::Literal);
        assert!(!billing.fails_verify());
        assert!(
            unclaimed(&a, "checks.vendor-one.com")
                .unwrap()
                .fails_verify()
        );
        assert!(unclaimed(&a, "docs.unrelated-site.org").is_none());
        assert!(unclaimed(&a, "deps.vendor-one.com").is_none());
        assert!(unclaimed(&a, "tests.vendor-one.com").is_none());
        assert!(a.unclaimed.iter().all(|h| h.host != "localhost"));
        assert_eq!(
            a.next_steps[0],
            "vendor-one.com: 2 hosts without a mock (mocked: api). 1 with a bundled description in node_modules/@vendor-one/sdk/specs/: checks. import_contract each. 1 only named in the app's files: billing."
        );
    }

    /// A description in the app's own files is not also searched for URLs.
    #[test]
    fn a_parsed_description_is_not_searched_for_literals() {
        let dir = project();
        let own = OPENAPI
            .replace("checks.vendor-one.com", "own.vendor-one.com")
            .replace(
                "\"version\": \"1\"",
                "\"version\": \"1\", \"termsOfService\": \"https://terms.vendor-one.com\"",
            );
        write(dir.path(), "api/own.json", &own);
        let a = answer(dir.path());
        assert!(unclaimed(&a, "own.vendor-one.com").is_some());
        assert!(unclaimed(&a, "terms.vendor-one.com").is_none());
    }

    #[test]
    fn the_mock_folder_and_build_outputs_are_skipped() {
        let dir = project();
        write(
            dir.path(),
            "mocks/other.json",
            &OPENAPI.replace("checks.vendor-one.com", "mockfolder.vendor-three.com"),
        );
        write(
            dir.path(),
            "dist/bundle.json",
            &OPENAPI.replace("checks.vendor-one.com", "built.vendor-three.com"),
        );
        write(
            dir.path(),
            "node_modules/pkg/test/fixture.json",
            &OPENAPI.replace("checks.vendor-one.com", "fixture.vendor-three.com"),
        );
        let a = answer(dir.path());
        for host in [
            "mockfolder.vendor-three.com",
            "built.vendor-three.com",
            "fixture.vendor-three.com",
        ] {
            assert!(unclaimed(&a, host).is_none(), "{host}");
        }
    }

    #[test]
    fn caps_stop_the_scan_and_say_so() {
        let dir = project();
        let root = dir.path().canonicalize().unwrap();
        let none = Skip::default();
        let raw = scan(
            &root,
            &none,
            Limits {
                max_files: 2,
                ..Limits::default()
            },
        );
        assert!(!raw.stats.complete);
        assert_eq!(raw.stats.stopped, ["2 files visited"]);
        let steps = next_steps(&[], &BTreeSet::new(), &raw.stats, "`list`");
        assert_eq!(steps.len(), 1);
        assert!(
            steps[0].starts_with("The host scan stopped early (2 files visited)"),
            "{}",
            steps[0]
        );
        assert_eq!(
            summary(&[], &BTreeSet::new(), &raw.stats).as_deref(),
            Some(
                "No host found without a mock. The scan stopped early (2 files visited), so the list may be incomplete."
            )
        );

        let raw = scan(
            &root,
            &none,
            Limits {
                max_total_bytes: 10,
                ..Limits::default()
            },
        );
        assert_eq!(raw.stats.stopped, ["10 bytes read in all"]);
        assert!(raw.stats.bytes_read <= 10);

        let raw = scan(
            &root,
            &none,
            Limits {
                max_file_bytes: 64,
                ..Limits::default()
            },
        );
        assert!(raw.stats.complete);
        assert_eq!(
            raw.stats.skipped_large, 2,
            "both descriptions are over 64 bytes"
        );
        assert!(raw.specs.is_empty());

        let raw = scan(
            &root,
            &none,
            Limits {
                budget: Duration::ZERO,
                ..Limits::default()
            },
        );
        assert_eq!(raw.stats.stopped, ["time budget of 0 ms"]);
        assert!(raw.timed_out);
    }

    #[test]
    fn the_cache_is_marked_and_a_new_file_invalidates_it() {
        let dir = project();
        let root = dir.path().canonicalize().unwrap();
        let skip = Skip::default();
        let first = cached_scan(&root, &skip, Limits::default());
        assert!(!first.stats.cached);
        let again = cached_scan(&root, &skip, Limits::default());
        assert!(again.stats.cached);
        assert_eq!(again.specs, first.specs);
        // A new file one level down changes the fingerprint.
        std::thread::sleep(Duration::from_millis(20));
        write(
            &root,
            "vendor/new.json",
            &OPENAPI.replace("checks.vendor-one.com", "new.vendor-seven.com"),
        );
        let fresh = cached_scan(&root, &skip, Limits::default());
        assert!(!fresh.stats.cached);
        assert!(fresh.specs.contains_key("new.vendor-seven.com"));
    }

    #[test]
    fn a_scan_the_clock_stopped_is_cached_for_a_minute() {
        let dir = project();
        let root = dir.path().canonicalize().unwrap();
        let skip = Skip::default();
        let quick = Limits {
            budget: Duration::ZERO,
            ..Limits::default()
        };
        let first = cached_scan(&root, &skip, quick);
        assert!(first.timed_out && !first.stats.cached);
        let second = cached_scan(&root, &skip, quick);
        assert!(second.stats.cached, "reused within the minute");
        let now = Instant::now();
        let stored = now.checked_sub(TIMED_OUT_TTL).unwrap();
        assert!(!reusable(true, stored, now), "expired after the minute");
        assert!(reusable(true, now, now));
        assert!(
            reusable(false, stored, now),
            "a complete scan does not expire"
        );
        // A size-capped scan is cached.
        let small = Limits {
            max_files: 2,
            ..Limits::default()
        };
        let other = tempfile::tempdir().unwrap();
        let other = other.path().canonicalize().unwrap();
        write(&other, "a.json", "{}");
        write(&other, "b.json", "{}");
        write(&other, "c.json", "{}");
        assert!(!cached_scan(&other, &skip, small).stats.cached);
        assert!(cached_scan(&other, &skip, small).stats.cached);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_root_are_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        write(
            outside.path(),
            "spec.json",
            &OPENAPI.replace("checks.vendor-one.com", "outside.vendor-four.com"),
        );
        let dir = project();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("node_modules/linked")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("spec.json"),
            dir.path().join("src-spec.json"),
        )
        .unwrap();
        let a = answer(dir.path());
        assert!(unclaimed(&a, "outside.vendor-four.com").is_none());
        assert!(unclaimed(&a, "checks.vendor-one.com").is_some());
    }

    #[test]
    fn a_host_without_a_description_falls_back_to_the_catalog() {
        let dir = project();
        // A catalog example's host, named in the app's source: kept by the
        // exact host, not by its domain.
        let host = crate::examples::catalog_hosts()
            .into_iter()
            .find(|h| !crate::examples::serving(h).is_empty())
            .unwrap();
        let domain = registrable_domain(&host).unwrap().to_owned();
        let sibling = format!("docs-site.{domain}");
        write(
            dir.path(),
            "src/app.py",
            &format!("BASE = \"https://{host}/v1\"\nDOCS = \"https://{sibling}/x\"\n"),
        );
        let a = answer(dir.path());
        let h = unclaimed(&a, &host).expect("catalog host kept");
        assert_eq!(h.evidence, Evidence::Catalog);
        assert!(h.fails_verify());
        assert!(
            unclaimed(&a, &sibling).is_none(),
            "a doc link of the domain"
        );
        let step = a
            .next_steps
            .iter()
            .find(|s| s.starts_with(&domain))
            .unwrap();
        let serving = &crate::examples::serving(&host)[0];
        assert!(
            step.contains(&format!(
                "1 in the catalog: {} (`{}",
                label(&host, &domain),
                serving.example
            )),
            "{step}"
        );
        assert!(step.contains("init_example"), "{step}");
    }

    #[test]
    fn steps_are_capped_and_the_rest_counted() {
        let hosts: Vec<UnclaimedHost> = (0..12)
            .map(|i| new_host(&format!("api.vendor{i:02}.com")))
            .collect();
        let steps = next_steps(&hosts, &BTreeSet::new(), &complete(), "`mockingbird hosts`");
        assert_eq!(steps.len(), MAX_STEPS + 1);
        assert_eq!(
            steps[MAX_STEPS],
            "And 4 more hosts in 4 more domains have no mock: `mockingbird hosts` lists them all."
        );
        let line = summary(&hosts, &BTreeSet::new(), &complete()).unwrap();
        assert!(line.contains(", 8 more domains."), "{line}");
    }

    /// Many hosts of one domain fit one step of about 500 characters.
    #[test]
    fn a_large_domain_step_stays_short() {
        let hosts: Vec<UnclaimedHost> = (0..60)
            .map(|i| UnclaimedHost {
                evidence: Evidence::Spec,
                specs: vec![SpecRef {
                    path: format!("node_modules/sdk/specs/product_{i:02}.json"),
                    title: None,
                    operations: 60 - i,
                }],
                ..new_host(&format!("product{i:02}.vendor.com"))
            })
            .collect();
        let claimed: BTreeSet<String> = ["api.vendor.com".to_owned()].into();
        let steps = next_steps(&hosts, &claimed, &complete(), "`list`");
        assert_eq!(steps.len(), 1);
        let step = &steps[0];
        assert!(step.len() <= MAX_STEP_CHARS + 40, "{} {step}", step.len());
        assert!(
            step.starts_with("vendor.com: 60 hosts without a mock (mocked: api). 60 with a bundled description in node_modules/sdk/specs/: product00, product01, "),
            "{step}"
        );
        assert!(step.contains(" more. import_contract each."), "{step}");
    }

    #[test]
    fn a_blocked_host_is_left_out_of_the_summary() {
        let dir = project();
        let a = answer(dir.path());
        let blocked: HashSet<String> = ["checks.vendor-one.com".to_owned()].into();
        let line = a.summary_without(&blocked).unwrap();
        assert!(
            line.starts_with("1 host the project can call has no mock: vendor-two.io 1"),
            "{line}"
        );
        let all: HashSet<String> = a.unclaimed.iter().map(|h| h.host.clone()).collect();
        assert!(a.summary_without(&all).is_none());
    }

    #[test]
    fn the_root_is_the_nearest_folder_with_a_manifest_or_git() {
        let dir = project();
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(project_root(&root.join("mocks/mockingbird.yaml")), root);
        write(dir.path(), "mocks/go.mod", "module x\n");
        assert_eq!(
            project_root(&root.join("mocks/mockingbird.yaml")),
            root.join("mocks")
        );
        assert!(too_broad(Path::new("/")));
    }

    /// An odd member (a scalar where an object belongs) loses that member,
    /// not the document.
    #[test]
    fn an_odd_member_does_not_fail_the_description() {
        let doc = r#"{"openapi": "3.0.0", "info": "not an object",
          "servers": [{"url": "https://odd.vendor.com"}, 7],
          "paths": {
            "/a": {"x-flag": true, "servers": "nope", "get": {}, "post": "weird"},
            "/b": 5,
            "/c": {"get": {"servers": [{"url": 3}]}}
          }}"#;
        let (title, hosts) = parse_spec(doc.as_bytes(), "json").unwrap();
        assert_eq!(title, None);
        assert_eq!(
            hosts.into_iter().collect::<Vec<_>>(),
            [("odd.vendor.com".to_owned(), 2)]
        );
    }

    #[test]
    fn sniff_and_url_hosts() {
        assert!(sniff(b"{\n  \"components\": {}", "json"));
        assert!(!sniff(b"{\"name\": \"pkg\", \"version\": \"1\"}", "json"));
        assert!(sniff(b"# c\nopenapi: 3.1.0\n", "yaml"));
        assert!(!sniff(b"name: ci\non: push\n", "yml"));
        let hosts = url_hosts(
            b"a https://API.Vendor.com:443/x b http://10.0.0.1/ c ftp://f.vendor.com d https://${HOST}/",
        );
        assert_eq!(hosts.into_iter().collect::<Vec<_>>(), ["api.vendor.com"]);
        assert_eq!(
            url_host("https://{region}.api.vendor.com/v1"),
            None,
            "an unresolved variable"
        );
        let servers: Vec<Lenient<Server>> = serde_json::from_str(
            r#"[{"url": "https://{region}.api.vendor.com/v1", "variables": {"region": {"default": "eu"}}}]"#,
        )
        .unwrap();
        assert_eq!(server_hosts(Some(&servers)), ["eu.api.vendor.com"]);
        assert_eq!(label("verify.vendor.com", "vendor.com"), "verify");
        assert_eq!(label("vendor.com", "vendor.com"), "vendor.com");
        assert_eq!(common_dir(["a/b/c/x.json", "a/b/y.json"]), "a/b");
        assert_eq!(common_dir(["x.json"]), "");
    }
}
