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
//!   dependencies), kept only when the host shares a registrable domain
//!   with a host of such a description, a mocked host or a catalog
//!   example's host, so documentation links stay out.
//!
//! The walk skips version control, build outputs, the mock's own folder
//! and symlinks, and stops at [`Limits`] (files, bytes per file, bytes in
//! all, wall-clock time), saying so in [`ScanStats::stopped`]. A scan is
//! cached per project root while the modification times near the root are
//! unchanged.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

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

/// Bytes read from the start of a JSON or YAML file to decide whether it
/// may be an API description.
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

/// Directories never walked: version control, build outputs, caches.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".cache",
    ".gradle",
    "coverage",
    "__pycache__",
    ".mockingbird",
    ".idea",
    ".vscode",
    // CI and repository settings, not the app.
    ".github",
    ".gitlab",
    ".circleci",
    ".husky",
];

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

/// Hosts named in one step; the rest of the domain is counted.
const MAX_HOSTS_PER_STEP: usize = 60;

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
}

/// A host the project can call that no mock lists.
#[derive(Serialize, Debug, Clone)]
pub struct UnclaimedHost {
    pub host: String,
    /// Its registrable domain (eTLD+1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// The API descriptions in the project that serve it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub specs: Vec<SpecRef>,
    /// The app's files with a URL literal naming it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// Catalog examples whose mocks list it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<ExampleRef>,
}

/// The scan of a config's project: the hosts no mock lists and what to do
/// about them.
#[derive(Serialize, Debug)]
pub struct Answer {
    pub project_root: String,
    pub config: String,
    pub scan: ScanStats,
    pub unclaimed: Vec<UnclaimedHost>,
    /// Hosts the scan found that a mock lists.
    pub claimed: Vec<String>,
    pub next_steps: Vec<String>,
}

/// The project root of `config`: the nearest ancestor of its directory
/// (the directory included) holding `.git` or a package manifest
/// (`package.json`, `go.mod`, ...), not climbing to the home directory or the filesystem
/// root; else the config's directory.
pub fn project_root(config: &Path) -> PathBuf {
    let absolute = std::path::absolute(config).unwrap_or_else(|_| config.to_path_buf());
    let dir = absolute
        .canonicalize()
        .unwrap_or(absolute)
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    for candidate in dir.ancestors() {
        let is_top = candidate.parent().is_none() || home.as_deref() == Some(candidate);
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

/// Scans the project of `config` and matches it against the config's mocks
/// (and `also_claimed`, such as the hosts a running server's mocks list).
///
/// # Errors
///
/// Fails when the config cannot be read or parsed.
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
    let config_dir = std::path::absolute(config)
        .ok()
        .and_then(|c| c.canonicalize().ok())
        .and_then(|c| c.parent().map(Path::to_path_buf));
    let skip = config_dir.filter(|d| *d != root);
    let raw = cached_scan(&root, skip.as_deref(), Limits::default());
    let list = format!(
        "`mockingbird hosts -c {}` (MCP project_hosts)",
        config.display()
    );
    let (unclaimed, found_claimed) = classify(&raw, &claimed, &crate::examples::catalog_hosts());
    let next_steps = next_steps(&unclaimed, &claimed, &raw.stats, &list);
    Ok(Answer {
        project_root: root.display().to_string(),
        config: config.display().to_string(),
        scan: raw.stats.clone(),
        unclaimed,
        claimed: found_claimed,
        next_steps,
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

/// Splits the scanned hosts into those no claimed host covers (with the
/// catalog examples listing each) and those a mock lists. URL literals
/// count only for hosts that share a registrable domain with an API
/// description's host, a claimed host or a `catalog` host.
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
        .chain(catalog.iter().map(String::as_str))
        .filter_map(registrable_domain)
        .collect();
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
        if registrable_domain(host).is_some_and(|d| known.contains(d)) {
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
        unclaimed.push(h);
    }
    (unclaimed, found_claimed)
}

fn new_host(host: &str) -> UnclaimedHost {
    UnclaimedHost {
        host: host.to_owned(),
        domain: registrable_domain(host).map(str::to_owned),
        specs: Vec::new(),
        sources: Vec::new(),
        examples: Vec::new(),
    }
}

/// The `next_steps` for `unclaimed`: one per registrable domain (domains
/// with a mocked host first, then those the app's files name), each host
/// with what to import: the API descriptions that serve it, else the
/// catalog examples that list it, else none found. At most [`MAX_STEPS`];
/// one more counts the rest and names `list`, and one says when the scan
/// stopped early.
fn next_steps(
    unclaimed: &[UnclaimedHost],
    claimed: &BTreeSet<String>,
    stats: &ScanStats,
    list: &str,
) -> Vec<String> {
    let mut groups: BTreeMap<String, Vec<&UnclaimedHost>> = BTreeMap::new();
    for h in unclaimed {
        let key = h.domain.clone().unwrap_or_else(|| h.host.clone());
        groups.entry(key).or_default().push(h);
    }
    let mocked_of = |domain: &str| -> Vec<&str> {
        claimed
            .iter()
            .map(String::as_str)
            .filter(|c| registrable_domain(c) == Some(domain))
            .collect()
    };
    let mut ordered: Vec<(String, Vec<&UnclaimedHost>)> = groups.into_iter().collect();
    ordered.sort_by_cached_key(|(domain, hosts)| {
        (
            mocked_of(domain).is_empty(),
            hosts.iter().all(|h| h.sources.is_empty()),
            domain.clone(),
        )
    });
    let mut steps = Vec::new();
    let mut rest_hosts = 0;
    let mut rest_domains = 0;
    for (i, (domain, mut hosts)) in ordered.into_iter().enumerate() {
        if i >= MAX_STEPS {
            rest_hosts += hosts.len();
            rest_domains += 1;
            continue;
        }
        // What to import first: hosts with a description, then those
        // the catalog lists, then the rest.
        let kind = |h: &UnclaimedHost| match (h.specs.is_empty(), h.examples.is_empty()) {
            (false, _) => 0,
            (true, false) => 1,
            (true, true) => 2,
        };
        hosts.sort_by(|a, b| (kind(a), &a.host).cmp(&(kind(b), &b.host)));
        steps.push(group_step(&domain, &hosts, &mocked_of(&domain), list));
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

fn group_step(domain: &str, hosts: &[&UnclaimedHost], mocked: &[&str], list: &str) -> String {
    let mut step = if let [only] = hosts {
        format!("{} has no mock, but the project can call it", only.host)
    } else {
        format!(
            "{} hosts of {domain} that the project can call have no mock",
            hosts.len()
        )
    };
    if !mocked.is_empty() {
        let _ = write!(step, " (mocked: {})", mocked.join(", "));
    }
    step.push('.');
    let paths: Vec<&str> = hosts
        .iter()
        .flat_map(|h| h.specs.iter().map(|s| s.path.as_str()))
        .collect();
    let dirs: BTreeSet<&str> = paths
        .iter()
        .map(|p| p.rsplit_once('/').map_or("", |(dir, _)| dir))
        .collect();
    let common = (paths.len() > 1 && dirs.len() == 1)
        .then(|| dirs.into_iter().next())
        .flatten()
        .filter(|d| !d.is_empty());
    if let Some(dir) = common {
        let _ = write!(step, " Bundled API descriptions in {dir}/:");
    }
    let shown: Vec<String> = hosts
        .iter()
        .take(MAX_HOSTS_PER_STEP)
        .map(|h| host_fragment(h, common))
        .collect();
    let _ = write!(step, " {}", shown.join("; "));
    if hosts.len() > MAX_HOSTS_PER_STEP {
        let _ = write!(
            step,
            "; and {} more hosts of {domain} ({list})",
            hosts.len() - MAX_HOSTS_PER_STEP
        );
    }
    step.push_str(". Import each one the app's flows use into a mock that lists the host in hosts, or name in your hand-back why it is left out.");
    step
}

/// One host of a step: what to import for it.
fn host_fragment(h: &UnclaimedHost, common: Option<&str>) -> String {
    let mut s = h.host.clone();
    if !h.sources.is_empty() {
        let _ = write!(s, " (named in {})", h.sources.join(", "));
    }
    if !h.specs.is_empty() {
        let specs: Vec<String> = h
            .specs
            .iter()
            .map(|spec| {
                let path = match common {
                    Some(dir) => spec
                        .path
                        .strip_prefix(dir)
                        .and_then(|p| p.strip_prefix('/'))
                        .unwrap_or(&spec.path),
                    None => &spec.path,
                };
                format!(
                    "{path} ({} operation{})",
                    spec.operations,
                    plural(spec.operations)
                )
            })
            .collect();
        let _ = write!(s, ": import_contract {}", specs.join(" or "));
    } else if !h.examples.is_empty() {
        let examples: Vec<String> = h
            .examples
            .iter()
            .map(|e| {
                format!(
                    "example `{}` (mock `{}`, contract `{}`)",
                    e.example, e.mock, e.contract
                )
            })
            .collect();
        let _ = write!(
            s,
            ": the catalog lists it, {} (init_example)",
            examples.join(" or ")
        );
    } else {
        s.push_str(": no contract found: import one, or say why the app doesn't need this host");
    }
    s
}

/// Scans cached per (root, skipped folder) with their [`fingerprint`].
type Cache = HashMap<(PathBuf, Option<PathBuf>), (Option<SystemTime>, Arc<RawScan>)>;

/// Projects kept in the cache.
const CACHE_ENTRIES: usize = 16;

/// [`scan`], from the cache while the root's [`fingerprint`] holds.
fn cached_scan(root: &Path, skip: Option<&Path>, limits: Limits) -> RawScan {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (root.to_path_buf(), skip.map(Path::to_path_buf));
    let print = fingerprint(root, skip);
    if let Ok(cache) = cache.lock()
        && let Some((at, scan)) = cache.get(&key)
        && print.is_some()
        && *at == print
    {
        let mut scan = (**scan).clone();
        scan.stats.cached = true;
        return scan;
    }
    let scan = scan(root, skip, limits);
    if let Ok(mut cache) = cache.lock() {
        if cache.len() >= CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(key, (print, Arc::new(scan.clone())));
    }
    scan
}

/// The newest modification time of the root and the entries one and two
/// levels below it (not the skipped folders): adding or removing a file
/// there, or installing a dependency, changes it; an edit deeper down does
/// not.
fn fingerprint(root: &Path, skip: Option<&Path>) -> Option<SystemTime> {
    let mtime = |p: &Path| std::fs::symlink_metadata(p).and_then(|m| m.modified()).ok();
    let mut newest = mtime(root)?;
    let mut bump = |t: Option<SystemTime>| {
        if let Some(t) = t
            && t > newest
        {
            newest = t;
        }
    };
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) || Some(path.as_path()) == skip {
            continue;
        }
        bump(mtime(&path));
        if entry.file_type().is_ok_and(|t| t.is_dir())
            && let Ok(children) = std::fs::read_dir(&path)
        {
            for child in children.flatten() {
                bump(mtime(&child.path()));
            }
        }
    }
    Some(newest)
}

/// Walks `root` (not `skip`, symlinks or [`SKIP_DIRS`]): the app's own
/// files first, then its dependency folders.
fn scan(root: &Path, skip: Option<&Path>, limits: Limits) -> RawScan {
    let mut walker = Walker {
        root,
        skip,
        limits,
        start: Instant::now(),
        raw: RawScan::default(),
        stopped: None,
    };
    let mut deps = Vec::new();
    walker.walk(root.to_path_buf(), false, &mut deps);
    for dir in deps {
        walker.walk(dir, true, &mut Vec::new());
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

struct Walker<'a> {
    root: &'a Path,
    skip: Option<&'a Path>,
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

    /// Walks `dir`; outside dependencies (`in_deps` false) a dependency
    /// folder is queued in `deps` instead of entered.
    fn walk(&mut self, dir: PathBuf, in_deps: bool, deps: &mut Vec<PathBuf>) {
        let mut stack = vec![dir];
        while let Some(dir) = stack.pop() {
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
                let path = entry.path();
                if kind.is_dir() {
                    if SKIP_DIRS.contains(&name.as_str()) || Some(path.as_path()) == self.skip {
                        continue;
                    }
                    if !in_deps && DEPENDENCY_DIRS.contains(&name.as_str()) {
                        deps.push(path);
                    } else if !(in_deps && DEPENDENCY_SKIP_DIRS.contains(&name.as_str())) {
                        stack.push(path);
                    }
                } else if kind.is_file() {
                    files.push((path, name));
                }
            }
            for (path, name) in files.into_iter().rev() {
                if self.stop() {
                    return;
                }
                self.file(&path, &name, in_deps);
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

    fn file(&mut self, path: &Path, name: &str, in_deps: bool) {
        let ext = extension(name);
        let spec_ext = SPEC_EXTENSIONS.contains(&ext.as_str());
        let source = !in_deps
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
        if source {
            let Some(bytes) = self.read(path, self.limits.max_file_bytes) else {
                return;
            };
            if bytes.iter().take(HEAD as usize).any(|b| *b == 0) {
                return;
            }
            if spec_ext && sniff(&bytes, &ext) {
                if len > self.limits.max_file_bytes {
                    self.raw.stats.skipped_large += 1;
                } else {
                    self.spec(&bytes, &ext, &rel);
                }
            }
            self.literals(&bytes, &rel);
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

    fn spec(&mut self, bytes: &[u8], ext: &str, rel: &str) {
        let Some((title, hosts)) = parse_spec(bytes, ext) else {
            return;
        };
        for (host, operations) in hosts {
            self.raw.specs.entry(host).or_default().push(SpecRef {
                path: rel.to_owned(),
                title: title.clone(),
                operations,
            });
        }
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

/// Whether the start of a JSON or YAML file may be an API description: a
/// top-level key OpenAPI or Swagger documents have (`openapi`, `swagger`,
/// `paths`, `components`, `servers`, `basePath`; JSON writers may put
/// `openapi` after a large `components`). The parse decides.
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

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SpecDoc {
    openapi: Option<serde::de::IgnoredAny>,
    swagger: Option<serde::de::IgnoredAny>,
    info: Option<Info>,
    servers: Option<Vec<Server>>,
    host: Option<String>,
    paths: Option<BTreeMap<String, PathItem>>,
}

#[derive(Deserialize, Default)]
struct Info {
    title: Option<String>,
}

#[derive(Deserialize, Default)]
struct Server {
    url: Option<String>,
    variables: Option<BTreeMap<String, Variable>>,
}

#[derive(Deserialize, Default)]
struct Variable {
    default: Option<String>,
}

#[derive(Deserialize, Default)]
struct PathItem {
    servers: Option<Vec<Server>>,
    get: Option<Operation>,
    put: Option<Operation>,
    post: Option<Operation>,
    delete: Option<Operation>,
    options: Option<Operation>,
    head: Option<Operation>,
    patch: Option<Operation>,
    trace: Option<Operation>,
}

#[derive(Deserialize, Default)]
struct Operation {
    servers: Option<Vec<Server>>,
}

impl PathItem {
    fn operations(&self) -> impl Iterator<Item = &Operation> {
        [
            &self.get,
            &self.put,
            &self.post,
            &self.delete,
            &self.options,
            &self.head,
            &self.patch,
            &self.trace,
        ]
        .into_iter()
        .flatten()
    }
}

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
    let mut top = server_hosts(doc.servers.as_deref());
    if doc.swagger.is_some()
        && let Some(host) = doc
            .host
            .as_deref()
            .and_then(|h| url_host(&format!("https://{h}")))
    {
        top.push(host);
    }
    let mut hosts: BTreeMap<String, usize> = top.iter().map(|h| (h.clone(), 0)).collect();
    for item in doc.paths.iter().flat_map(BTreeMap::values) {
        let path_hosts = item
            .servers
            .as_deref()
            .map(|s| server_hosts(Some(s)))
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| top.clone());
        for host in &path_hosts {
            hosts.entry(host.clone()).or_insert(0);
        }
        for op in item.operations() {
            let op_hosts = op
                .servers
                .as_deref()
                .map(|s| server_hosts(Some(s)))
                .filter(|h| !h.is_empty());
            for host in op_hosts.as_ref().unwrap_or(&path_hosts) {
                *hosts.entry(host.clone()).or_insert(0) += 1;
            }
        }
    }
    let title = doc.info.and_then(|i| i.title).filter(|t| !t.is_empty());
    Some((title, hosts))
}

/// The usable hosts of OpenAPI `servers`, with each `{variable}` replaced
/// by its default.
fn server_hosts(servers: Option<&[Server]>) -> Vec<String> {
    let mut hosts: Vec<String> = servers
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let mut url = s.url.clone()?;
            for (name, var) in s.variables.iter().flatten() {
                if let Some(default) = &var.default {
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

    #[test]
    fn bundled_openapi_and_swagger_descriptions_are_found_and_named() {
        let dir = project();
        let a = answer(dir.path());
        let checks = unclaimed(&a, "checks.vendor-one.com").expect("OpenAPI 3 host");
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
            "checks.vendor-one.com has no mock, but the project can call it (mocked: api.vendor-one.com). checks.vendor-one.com: import_contract node_modules/@vendor-one/sdk/specs/vendor_one_checks.json (3 operations). Import each one the app's flows use into a mock that lists the host in hosts, or name in your hand-back why it is left out."
        );
        assert!(
            a.next_steps[1]
                .contains("import_contract vendor/ledger/api/ledger.yaml (2 operations)"),
            "{:?}",
            a.next_steps
        );
        assert_eq!(
            a.project_root,
            dir.path().canonicalize().unwrap().display().to_string()
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
        assert_eq!(a.claimed, ["checks.vendor-one.com", "ledger.vendor-two.io"]);
        // A host a running server's mock lists counts as well.
        write(dir.path(), "mocks/mockingbird.yaml", "mocks: {}\n");
        let served: HashSet<String> = ["ledger.vendor-two.io".to_owned()].into();
        let a = project_hosts(&dir.path().join("mocks/mockingbird.yaml"), &served).unwrap();
        assert!(unclaimed(&a, "ledger.vendor-two.io").is_none());
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
        // Dependencies are not read for literals.
        write(
            dir.path(),
            "node_modules/x/index.js",
            "'https://deps.vendor-one.com'",
        );
        let a = answer(dir.path());
        let billing = unclaimed(&a, "billing.vendor-one.com").expect("kept");
        assert_eq!(billing.sources, ["src/client.js"]);
        assert!(billing.specs.is_empty());
        assert!(unclaimed(&a, "docs.unrelated-site.org").is_none());
        assert!(unclaimed(&a, "deps.vendor-one.com").is_none());
        assert!(a.unclaimed.iter().all(|h| h.host != "localhost"));
        let step = &a.next_steps[0];
        assert!(
            step.starts_with("2 hosts of vendor-one.com that the project can call have no mock (mocked: api.vendor-one.com). checks.vendor-one.com: import_contract node_modules/@vendor-one/sdk/specs/vendor_one_checks.json (3 operations); billing.vendor-one.com (named in src/client.js): no contract found: import one, or say why the app doesn't need this host. Import each"),
            "{step}"
        );
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
        let tiny = Limits {
            max_files: 2,
            ..Limits::default()
        };
        let raw = scan(&root, None, tiny);
        assert!(!raw.stats.complete);
        assert_eq!(raw.stats.stopped, ["2 files visited"]);
        let steps = next_steps(&[], &BTreeSet::new(), &raw.stats, "`list`");
        assert_eq!(steps.len(), 1);
        assert!(
            steps[0].starts_with("The host scan stopped early (2 files visited)"),
            "{}",
            steps[0]
        );

        let raw = scan(
            &root,
            None,
            Limits {
                max_total_bytes: 10,
                ..Limits::default()
            },
        );
        assert_eq!(raw.stats.stopped, ["10 bytes read in all"]);
        assert!(raw.stats.bytes_read <= 10);

        let raw = scan(
            &root,
            None,
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
            None,
            Limits {
                budget: Duration::ZERO,
                ..Limits::default()
            },
        );
        assert_eq!(raw.stats.stopped, ["time budget of 0 ms"]);
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
        // A catalog example's host, named in the app's source: known by the
        // catalog alone.
        let host = crate::examples::catalog_hosts()
            .into_iter()
            .find(|h| !crate::examples::serving(h).is_empty())
            .unwrap();
        write(
            dir.path(),
            "src/app.py",
            &format!("BASE = \"https://{host}/v1\"\n"),
        );
        let a = answer(dir.path());
        let h = unclaimed(&a, &host).expect("catalog host kept");
        assert!(!h.examples.is_empty());
        let step = a.next_steps.iter().find(|s| s.contains(&host)).unwrap();
        let serving = &crate::examples::serving(&host)[0];
        assert!(
            step.contains(&format!(
                "{host} (named in src/app.py): the catalog lists it, example `{}` (mock `{}`, contract `{}`)",
                serving.example, serving.mock, serving.contract
            )),
            "{step}"
        );
    }

    #[test]
    fn steps_are_capped_and_the_rest_counted() {
        let hosts: Vec<UnclaimedHost> = (0..12)
            .map(|i| new_host(&format!("api.vendor{i:02}.com")))
            .collect();
        let stats = ScanStats {
            complete: true,
            ..ScanStats::default()
        };
        let steps = next_steps(&hosts, &BTreeSet::new(), &stats, "`mockingbird hosts`");
        assert_eq!(steps.len(), MAX_STEPS + 1);
        assert_eq!(
            steps[MAX_STEPS],
            "And 4 more hosts in 4 more domains have no mock: `mockingbird hosts` lists them all."
        );
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
    }

    #[test]
    fn sniff_and_url_hosts() {
        assert!(sniff(b"{\n  \"components\": {}", "json"));
        assert!(!sniff(b"{\"name\": \"pkg\", \"version\": \"1\"}", "json"));
        assert!(sniff(b"# c\nopenapi: 3.1.0\n", "yaml"));
        assert!(!sniff(b"name: ci\non: push\n", "yml"));
        let hosts = url_hosts(b"a https://API.Vendor.com:443/x b http://10.0.0.1/ c ftp://f.vendor.com d https://${HOST}/");
        assert_eq!(hosts.into_iter().collect::<Vec<_>>(), ["api.vendor.com"]);
        assert_eq!(
            url_host("https://{region}.api.vendor.com/v1"),
            None,
            "an unresolved variable"
        );
        let servers = [Server {
            url: Some("https://{region}.api.vendor.com/v1".into()),
            variables: Some(
                [(
                    "region".to_owned(),
                    Variable {
                        default: Some("eu".into()),
                    },
                )]
                .into(),
            ),
        }];
        assert_eq!(server_hosts(Some(&servers)), ["eu.api.vendor.com"]);
    }
}
