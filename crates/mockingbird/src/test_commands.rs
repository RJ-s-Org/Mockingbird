//! The app's own test commands, found from its project root
//! ([`crate::hosts::project_root`]) so an agent can run them against the
//! mocks (`mockingbird verify --run "<cmd>"`, MCP `verify_run`).
//!
//! Detection is cheap and bounded: it reads a few manifest and helper
//! files in the root and in folders up to [`MAX_DEPTH`] below it (skipping
//! dependencies, build outputs and hidden folders), charges every
//! directory entry looked at and every file read to one budget of
//! [`MAX_ENTRIES`], and lists at most [`MAX_COMMANDS`] commands:
//!
//! - `package.json`: `scripts.test` (`npm test`; not npm's placeholder that
//!   only fails) and other test scripts (`test:e2e`, `e2e`, ...: `npm run
//!   <name>`; not lint, type check or watch scripts), with `pnpm` or `yarn`
//!   when their lock file is there;
//! - a Makefile's `test` target (`make test`), else its first `test-`,
//!   `test_` or `e2e` target;
//! - pytest: `pytest.ini`, `[tool.pytest…]` in `pyproject.toml`,
//!   `[tool:pytest]` in `setup.cfg`, `conftest.py`, or a `tests/` folder
//!   with a `test_*.py` file;
//! - RSpec: a `spec/` folder with `.rspec` or a Gemfile naming rspec
//!   (`bundle exec rspec` with a Gemfile, else `rspec`);
//! - Go: `go.mod` with a `*_test.go` file below it (`go test ./...`);
//! - Cargo: `Cargo.toml` (`cargo test`).
//!
//! Every command reads `cd <absolute dir> && …`, so it runs from any
//! folder; a subfolder's `cargo test` or pytest is left out when a folder
//! above has one (a Cargo workspace, a pytest rootdir).
//!
//! Tests that need the app's own server running get a [`ServePair`] (at
//! most [`MAX_PAIRS`]), to run with `verify --run <command> --serve
//! <serve>`, from what lies next to them:
//!
//! - Playwright's `webServer` when it sets `reuseExistingServer: true`
//!   (otherwise Playwright starts the server itself);
//! - a local base URL with a port (an identifier such as `SERVER_URL`,
//!   `BASE_URL`, `baseURL` or `APP_URL`, not `DATABASE_URL`, on a line
//!   that is no comment, naming `http://localhost:<port>`; not one of the
//!   config's own ports) in a test helper, paired with the command of that
//!   helper's runner (`spec_helper.rb`, `rails_helper.rb`: RSpec;
//!   `conftest.py`: pytest; `playwright.config.*`: Playwright;
//!   `cypress.config.*`: Cypress; jest setup files: Jest; `.env.test`: the
//!   folder's first command), and with the server to start: one per
//!   `<x>/server` sample that the runner's test files name (each file goes
//!   to the sample with the longest such name; the files are named on the
//!   test command, only the `_server_` ones when there are some, the others
//!   listed in `other_tests`), else the one `<x>/server` or `server/`
//!   folder, else a `start`, `dev` or `serve` script. In a `server` folder
//!   with a folder per language, the one with a start command (a
//!   `package.json` script; `app.py`, `server.py` or `main.py`; `server.rb`
//!   or `app.rb`; a Go `main`) whose dependencies are installed comes
//!   first, then the test runner's language, then Node, Python, Ruby and
//!   Go, then the shortest name.

use std::path::Path;

use serde::Serialize;

/// Commands listed at most.
pub const MAX_COMMANDS: usize = 8;

/// Folder levels below the root searched for subprojects.
pub const MAX_DEPTH: usize = 2;

/// Directory entries looked at in all (subproject search and the Go test
/// file search).
pub const MAX_ENTRIES: usize = 4_000;

/// Bytes read from one manifest.
const MAX_FILE_BYTES: u64 = 512 * 1024;

/// Folders never searched for subprojects.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "vendor",
    "target",
    "dist",
    "build",
    "out",
    "venv",
    "env",
    "site-packages",
    "__pycache__",
    "coverage",
    "tmp",
    "log",
    "logs",
];

/// Test/serve pairs listed at most.
pub const MAX_PAIRS: usize = 8;

/// One test command of the app.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct TestCommand {
    /// The command, `cd <absolute dir> && …`.
    pub command: String,
    /// The folder it belongs to, relative to the root (`.` for the root).
    pub dir: String,
    /// What it was found from (`package.json scripts.test`, `spec/`, ...).
    pub source: String,
    /// The command without its `cd`.
    #[serde(skip)]
    bare: String,
    /// The command from the project root (`cd <dir> && …` in a subfolder).
    #[serde(skip)]
    short: String,
    #[serde(skip)]
    runner: Option<Ecosystem>,
    /// The `package.json` script it runs (Node), to tell its runner.
    #[serde(skip)]
    script: String,
}

/// A test command that needs the app's own server, and that server.
#[derive(Serialize, Debug, Clone, Eq)]
pub struct ServePair {
    /// The test command, `cd <absolute dir> && …`.
    pub command: String,
    /// The app's server, `cd <absolute dir> && …`.
    pub serve: String,
    /// The port it listens on, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serve_port: Option<u16>,
    /// What the pair was found from.
    pub source: String,
    /// The sample's other test files, which `command` leaves out (they
    /// need the same server; `x_e2e_spec.rb` beside `x_server_spec.rb`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other_tests: Vec<String>,
    /// `command` and `serve` from the project root.
    #[serde(skip)]
    short: (String, String),
}

impl PartialEq for ServePair {
    /// The forms from the project root follow from the others.
    fn eq(&self, other: &Self) -> bool {
        (
            &self.command,
            &self.serve,
            self.serve_port,
            &self.source,
            &self.other_tests,
        ) == (
            &other.command,
            &other.serve,
            other.serve_port,
            &other.source,
            &other.other_tests,
        )
    }
}

impl ServePair {
    /// Whether `serve` is this pair's serve command (in either form).
    #[must_use]
    pub fn serves(&self, serve: &str) -> bool {
        self.serve == serve || self.short.1 == serve
    }
}

/// The app's test commands and the pairs of those that need its server.
#[derive(Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Detected {
    pub commands: Vec<TestCommand>,
    pub pairs: Vec<ServePair>,
}

/// The test commands of the project at `root` and their serve pairs,
/// within one [`MAX_ENTRIES`] budget; a base URL on a port in `avoid` (the
/// config's server, admin, gRPC or proxy port) is not the app's. Answered
/// from a cache while the project's file tree looks unchanged
/// (the host scan's fingerprint) and for at most [`CACHE_TTL`].
#[must_use]
pub fn detect_all(root: &Path, avoid: &[u16]) -> Detected {
    type Cache = std::collections::HashMap<
        (std::path::PathBuf, Vec<u16>),
        (std::time::SystemTime, std::time::Instant, Detected),
    >;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Cache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut avoid = avoid.to_vec();
    avoid.sort_unstable();
    let key = (root.to_path_buf(), avoid);
    let print = crate::hosts::tree_fingerprint(root);
    if let (Ok(cache), Some(print)) = (cache.lock(), print)
        && let Some((at, stored, found)) = cache.get(&key)
        && *at == print
        && stored.elapsed() < CACHE_TTL
    {
        return found.clone();
    }
    let mut budget = MAX_ENTRIES;
    let commands = detect_within(root, &mut budget);
    let pairs = pairs(root, &commands, &key.1, &mut budget);
    let found = Detected { commands, pairs };
    if let (Ok(mut cache), Some(print)) = (cache.lock(), print) {
        if cache.len() >= 16 {
            cache.clear();
        }
        cache.insert(key, (print, std::time::Instant::now(), found.clone()));
    }
    found
}

/// The ports of the config at `config` that are Mockingbird's own: the
/// data plane, the Admin API, gRPC and the proxy, each with what it is.
/// None when the config does not load.
#[must_use]
pub fn config_ports(config: &Path) -> Vec<(u16, &'static str)> {
    let Ok(cfg) = crate::file_config::load(config) else {
        return Vec::new();
    };
    vec![
        (cfg.server.port, "data-plane port (server.port)"),
        (cfg.server.admin_port, "Admin API port (server.admin_port)"),
        (cfg.server.grpc_port, "gRPC port (server.grpc_port)"),
        (cfg.proxy.port, "proxy port (proxy.port)"),
    ]
}

/// How long a detection is reused at most (an edit deep in the tree, such
/// as installing a server's dependencies, does not change the fingerprint).
pub const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

impl TestCommand {
    /// `` `cmd` (source) ``, the command as run from the project root.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("`{}` ({})", self.short, self.source)
    }
}

/// The test commands of the project at `root`, the root's first.
#[must_use]
pub fn detect(root: &Path) -> Vec<TestCommand> {
    let mut budget = MAX_ENTRIES;
    detect_within(root, &mut budget)
}

/// [`detect`], charging `budget`.
fn detect_within(root: &Path, budget: &mut usize) -> Vec<TestCommand> {
    let mut dirs = vec![(root.to_path_buf(), String::from("."))];
    let mut i = 0;
    // Breadth first, so the root and its near subprojects come first.
    while i < dirs.len() {
        let (dir, rel) = dirs[i].clone();
        i += 1;
        let depth = if rel == "." {
            0
        } else {
            rel.matches('/').count() + 1
        };
        if depth >= MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children = Vec::new();
        for entry in entries.flatten() {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                let child = if rel == "." {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                children.push((entry.path(), child));
            }
        }
        children.sort();
        dirs.extend(children);
    }
    let mut out = Vec::new();
    // Folders whose command already runs a subfolder's tests: a Cargo
    // workspace's `cargo test`, pytest from its rootdir.
    let mut covers: Vec<(String, Ecosystem)> = Vec::new();
    for (dir, rel) in &dirs {
        if out.len() >= MAX_COMMANDS {
            break;
        }
        for (command, source, eco, script) in in_dir(dir, budget) {
            if out.len() >= MAX_COMMANDS {
                break;
            }
            if eco.covers_subfolders() {
                if covers
                    .iter()
                    .any(|(above, e)| *e == eco && is_below(rel, above))
                {
                    continue;
                }
                covers.push((rel.clone(), eco));
            }
            let source = if rel == "." {
                source
            } else {
                format!("{rel}/{source}")
            };
            out.push(TestCommand {
                command: in_folder(dir, &command),
                short: from_root(root, dir, &command),
                dir: rel.clone(),
                source,
                bare: command,
                runner: Some(eco),
                script,
            });
        }
    }
    out
}

/// `cd <dir> && <command>`.
fn in_folder(dir: &Path, command: &str) -> String {
    format!("cd {} && {command}", shell_word(&dir.display().to_string()))
}

/// `command` run in `dir`, from `root`: as is in the root, `cd <relative
/// dir> && …` below it, `cd <dir> && …` elsewhere.
fn from_root(root: &Path, dir: &Path, command: &str) -> String {
    match dir.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => command.to_owned(),
        Ok(rel) => format!("cd {} && {command}", shell_word(&rel.display().to_string())),
        Err(_) => in_folder(dir, command),
    }
}

/// Whether folder `rel` lies below folder `above` (both relative to the
/// root, `.` being the root).
fn is_below(rel: &str, above: &str) -> bool {
    above == "." || rel.strip_prefix(above).is_some_and(|r| r.starts_with('/'))
}

/// The test runner a command belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ecosystem {
    Node,
    Make,
    Pytest,
    Rspec,
    Go,
    Cargo,
}

impl Ecosystem {
    /// Whether its command in a folder also runs the tests of the folders
    /// below: `cargo test` in a workspace, pytest from its rootdir.
    fn covers_subfolders(self) -> bool {
        matches!(self, Ecosystem::Cargo | Ecosystem::Pytest)
    }
}

/// `s` quoted for `sh` when it holds anything but plain path characters.
pub(crate) fn shell_word(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-:@+".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The text of the file at `path` (its first [`MAX_FILE_BYTES`]), charging
/// one entry of `budget`; `None` when it is spent or the file unreadable.
fn read(path: &Path, budget: &mut usize) -> Option<String> {
    use std::io::Read as _;
    if *budget == 0 {
        return None;
    }
    *budget -= 1;
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(MAX_FILE_BYTES).read_to_string(&mut text).ok()?;
    Some(text)
}

/// The test commands of one folder: `(command, source, runner, script)`.
fn in_dir(dir: &Path, budget: &mut usize) -> Vec<(String, String, Ecosystem, String)> {
    let mut out: Vec<(String, String, Ecosystem, String)> = node(dir, budget)
        .into_iter()
        .map(|(c, s, script)| (c, s, Ecosystem::Node, script))
        .collect();
    if let Some((c, s)) = make(dir, budget) {
        out.push((c, s, Ecosystem::Make, String::new()));
    }
    if let Some(source) = pytest(dir, budget) {
        out.push(("pytest".into(), source, Ecosystem::Pytest, String::new()));
    }
    if let Some((c, s)) = rspec(dir, budget) {
        out.push((c, s, Ecosystem::Rspec, String::new()));
    }
    if dir.join("go.mod").is_file() && has_go_test(dir, budget) {
        out.push((
            "go test ./...".into(),
            "go.mod, *_test.go".into(),
            Ecosystem::Go,
            String::new(),
        ));
    }
    if dir.join("Cargo.toml").is_file() {
        out.push((
            "cargo test".into(),
            "Cargo.toml".into(),
            Ecosystem::Cargo,
            String::new(),
        ));
    }
    out
}

/// npm's placeholder `test` script, which only fails.
fn placeholder(script: &str) -> bool {
    script.contains("no test specified")
}

/// Whether a script name (other than `test`) runs a test suite: it starts
/// with `test:`, `test-` or `test_` (`test:e2e`, `test-integration`), or
/// has an `e2e` part (`e2e`, `cy:e2e`). Watch, update, debug, lint, type
/// check and format scripts are not.
fn test_like(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower != "test"
        && (lower.starts_with("test:")
            || lower.starts_with("test-")
            || lower.starts_with("test_")
            || lower.split([':', '-', '_']).any(|part| part == "e2e"))
        && ![
            "watch", "update", "debug", "lint", "check", "types", "format", "fmt",
        ]
        .iter()
        .any(|w| lower.contains(w))
}

/// The test scripts of `package.json`: `(command, source, script)`.
fn node(dir: &Path, budget: &mut usize) -> Vec<(String, String, String)> {
    let Some(text) = read(&dir.join("package.json"), budget) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(scripts) = json["scripts"].as_object() else {
        return Vec::new();
    };
    let (tool, run) = if dir.join("pnpm-lock.yaml").is_file() {
        ("pnpm", "pnpm run")
    } else if dir.join("yarn.lock").is_file() {
        ("yarn", "yarn run")
    } else {
        ("npm", "npm run")
    };
    let mut out = Vec::new();
    if let Some(test) = scripts.get("test").and_then(|v| v.as_str())
        && !placeholder(test)
    {
        out.push((
            format!("{tool} test"),
            "package.json scripts.test".to_string(),
            test.to_owned(),
        ));
    }
    let mut extra: Vec<&String> = scripts.keys().filter(|k| test_like(k)).collect();
    extra.sort();
    for name in extra {
        out.push((
            format!("{run} {}", shell_word(name)),
            format!("package.json scripts.{name}"),
            scripts[name].as_str().unwrap_or_default().to_owned(),
        ));
    }
    out
}

fn make(dir: &Path, budget: &mut usize) -> Option<(String, String)> {
    let (name, text) = ["GNUmakefile", "makefile", "Makefile"]
        .into_iter()
        .find(|name| dir.join(name).is_file())
        .and_then(|name| read(&dir.join(name), budget).map(|t| (name, t)))?;
    let targets: Vec<&str> = text
        .lines()
        .filter(|l| !l.starts_with(['\t', ' ', '#', '.']))
        .filter_map(|l| l.split_once(':'))
        .filter(|(_, rest)| !rest.starts_with('='))
        .flat_map(|(t, _)| t.split_whitespace())
        .collect();
    let target = if targets.contains(&"test") {
        "test"
    } else {
        targets
            .into_iter()
            .find(|t| t.starts_with("test-") || t.starts_with("test_") || *t == "e2e")?
    };
    Some((format!("make {target}"), format!("{name} {target}")))
}

fn pytest(dir: &Path, budget: &mut usize) -> Option<String> {
    if dir.join("pytest.ini").is_file() {
        return Some("pytest.ini".into());
    }
    if read(&dir.join("pyproject.toml"), budget).is_some_and(|t| t.contains("[tool.pytest")) {
        return Some("pyproject.toml [tool.pytest]".into());
    }
    if read(&dir.join("setup.cfg"), budget).is_some_and(|t| t.contains("[tool:pytest]")) {
        return Some("setup.cfg [tool:pytest]".into());
    }
    if read(&dir.join("tox.ini"), budget).is_some_and(|t| t.contains("[pytest]")) {
        return Some("tox.ini [pytest]".into());
    }
    if dir.join("conftest.py").is_file() {
        return Some("conftest.py".into());
    }
    let tests = dir.join("tests");
    let has_test_file = std::fs::read_dir(&tests)
        .ok()?
        .flatten()
        .take_while(|_| spend(budget))
        .any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
        });
    has_test_file.then(|| "tests/test_*.py".into())
}

fn rspec(dir: &Path, budget: &mut usize) -> Option<(String, String)> {
    if !dir.join("spec").is_dir() {
        return None;
    }
    let gemfile = read(&dir.join("Gemfile"), budget);
    let in_gemfile = gemfile.as_deref().is_some_and(|g| g.contains("rspec"));
    if !dir.join(".rspec").is_file() && !in_gemfile {
        return None;
    }
    let command = if gemfile.is_some() {
        "bundle exec rspec"
    } else {
        "rspec"
    };
    Some((command.into(), "spec/".into()))
}

/// Takes one entry from `budget`; `false` when it is spent.
fn spend(budget: &mut usize) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    true
}

/// Whether a `*_test.go` file lies below `dir` (dependencies and hidden
/// folders skipped), within the entry budget.
fn has_go_test(dir: &Path, budget: &mut usize) -> bool {
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                return false;
            }
            *budget -= 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir {
                if depth < 6 && !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push((entry.path(), depth + 1));
                }
            } else if name.ends_with("_test.go") {
                return true;
            }
        }
    }
    false
}

/// The runner a test helper belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Helper {
    Rspec,
    Pytest,
    Playwright,
    Cypress,
    Jest,
    /// `.env.test`: the folder's first command.
    Any,
}

/// Test config and helper files read for a base URL, relative to a test
/// folder, with the runner each belongs to.
const BASE_URL_FILES: &[(&str, Helper)] = &[
    ("spec/spec_helper.rb", Helper::Rspec),
    ("spec/rails_helper.rb", Helper::Rspec),
    ("conftest.py", Helper::Pytest),
    ("tests/conftest.py", Helper::Pytest),
    ("test/conftest.py", Helper::Pytest),
    ("playwright.config.ts", Helper::Playwright),
    ("playwright.config.js", Helper::Playwright),
    ("playwright.config.mjs", Helper::Playwright),
    ("playwright.config.cjs", Helper::Playwright),
    ("playwright.config.mts", Helper::Playwright),
    ("cypress.config.ts", Helper::Cypress),
    ("cypress.config.js", Helper::Cypress),
    ("cypress.config.mjs", Helper::Cypress),
    ("cypress.config.cjs", Helper::Cypress),
    ("jest.setup.js", Helper::Jest),
    ("jest.setup.ts", Helper::Jest),
    ("setupTests.js", Helper::Jest),
    ("setupTests.ts", Helper::Jest),
    ("src/setupTests.js", Helper::Jest),
    ("src/setupTests.ts", Helper::Jest),
    (".env.test", Helper::Any),
];

/// Identifiers naming the app's base URL, compared lower case and whole
/// (`DATABASE_URL` and `OPENAI_BASE_URL` are not).
const BASE_URL_KEYS: &[&str] = &[
    "server_url",
    "serverurl",
    "base_url",
    "baseurl",
    "app_url",
    "appurl",
];

/// The command of `folder` (relative `rel` to the root) that `helper`'s
/// runner runs: `(command, from root, bare, runner)`. Playwright, Cypress
/// and Jest fall back to `npx <runner>` when no script runs them.
fn runner_command(
    root: &Path,
    folder: &Path,
    commands: &[&TestCommand],
    helper: Helper,
) -> Option<(String, String, String, Option<Ecosystem>)> {
    let script_runs = |words: &[&str]| {
        commands.iter().find(|c| {
            c.runner == Some(Ecosystem::Node) && words.iter().any(|w| c.script.contains(w))
        })
    };
    let found = match helper {
        Helper::Rspec => commands.iter().find(|c| c.runner == Some(Ecosystem::Rspec)),
        Helper::Pytest => commands
            .iter()
            .find(|c| c.runner == Some(Ecosystem::Pytest)),
        Helper::Playwright => script_runs(&["playwright"]),
        Helper::Cypress => script_runs(&["cypress"]),
        Helper::Jest => script_runs(&["jest", "vitest", "react-scripts test"]),
        Helper::Any => commands.first(),
    };
    if let Some(c) = found {
        return Some((c.command.clone(), c.short.clone(), c.bare.clone(), c.runner));
    }
    let bare = match helper {
        Helper::Playwright => "npx playwright test",
        Helper::Cypress => "npx cypress run",
        Helper::Jest => "npx jest",
        _ => return None,
    };
    Some((
        in_folder(folder, bare),
        from_root(root, folder, bare),
        bare.to_owned(),
        Some(Ecosystem::Node),
    ))
}

/// The test/serve pairs of the folders of `commands` (and the root), the
/// root's first; base URLs on a port in `avoid` (Mockingbird's own) are
/// not the app's.
fn pairs(
    root: &Path,
    commands: &[TestCommand],
    avoid: &[u16],
    budget: &mut usize,
) -> Vec<ServePair> {
    let mut out: Vec<ServePair> = Vec::new();
    let mut folders: Vec<&str> = vec!["."];
    for c in commands {
        if !folders.contains(&c.dir.as_str()) {
            folders.push(&c.dir);
        }
    }
    for rel in folders {
        if out.len() >= MAX_PAIRS {
            break;
        }
        let dir = if rel == "." {
            root.to_path_buf()
        } else {
            root.join(rel)
        };
        let prefix = if rel == "." {
            String::new()
        } else {
            format!("{rel}/")
        };
        let mine: Vec<&TestCommand> = commands.iter().filter(|c| c.dir == rel).collect();
        // Playwright starts its own webServer, unless it reuses one
        // already running.
        if let Some(ws) = web_server(&dir, budget)
            && let Some((command, short, ..)) =
                runner_command(root, &dir, &mine, Helper::Playwright)
        {
            out.push(ServePair {
                command,
                serve: in_folder(&ws.dir, &ws.command),
                serve_port: ws.port,
                source: format!("{prefix}{} webServer (reuseExistingServer: true)", ws.file),
                other_tests: Vec::new(),
                short: (short, from_root(root, &ws.dir, &ws.command)),
            });
            continue;
        }
        let mut seen: Vec<Helper> = Vec::new();
        for (file, helper, port) in base_urls(&dir, avoid, budget) {
            if out.len() >= MAX_PAIRS {
                break;
            }
            // One pair per runner, from its first helper.
            if seen.contains(&helper) {
                continue;
            }
            seen.push(helper);
            let Some(test) = runner_command(root, &dir, &mine, helper) else {
                continue;
            };
            let found = format!("{prefix}{file} names http://localhost:{port}");
            serve_pairs(root, &dir, &test, port, &found, budget, &mut out);
        }
    }
    out
}

/// The pairs of the test command `test` (`(command, from root, bare,
/// runner)`, in `dir`) whose base URL names `port`: one per `<x>/server`
/// sample whose name its test files carry, else the one server next to
/// the tests.
fn serve_pairs(
    root: &Path,
    dir: &Path,
    test: &(String, String, String, Option<Ecosystem>),
    port: u16,
    found: &str,
    budget: &mut usize,
    out: &mut Vec<ServePair>,
) {
    let (command, short, bare, runner) = test;
    let servers = integration_servers(dir, *runner, budget);
    let names: Vec<String> = servers.iter().map(|s| norm(&s.name)).collect();
    let files = test_files(dir, *runner, budget);
    let mut matched = false;
    for s in &servers {
        if out.len() >= MAX_PAIRS {
            return;
        }
        let (run, left) = files_of(&files, &norm(&s.name), &names);
        if run.is_empty() {
            continue;
        }
        matched = true;
        let args = run
            .iter()
            .map(|f| shell_word(f))
            .collect::<Vec<_>>()
            .join(" ");
        let sep = if *runner == Some(Ecosystem::Node) {
            " -- "
        } else {
            " "
        };
        let test = format!("{bare}{sep}{args}");
        out.push(ServePair {
            command: in_folder(dir, &test),
            serve: in_folder(&s.dir, &s.command),
            serve_port: Some(port),
            source: format!("{found}; {}", s.source_in(root)),
            other_tests: left,
            short: (
                from_root(root, dir, &test),
                from_root(root, &s.dir, &s.command),
            ),
        });
    }
    if matched || out.len() >= MAX_PAIRS {
        return;
    }
    let single = if servers.len() == 1 {
        servers.into_iter().next()
    } else {
        None
    }
    .or_else(|| server_of(&dir.join("server"), "server", *runner, budget))
    .or_else(|| start_command(dir, budget).map(|(cmd, src, _)| Server::at(dir, "", cmd, src)))
    .or_else(|| {
        (dir != root)
            .then(|| {
                start_command(root, budget).map(|(cmd, src, _)| Server::at(root, "", cmd, src))
            })
            .flatten()
    });
    if let Some(s) = single {
        out.push(ServePair {
            command: command.clone(),
            serve: in_folder(&s.dir, &s.command),
            serve_port: Some(port),
            source: format!("{found}; {}", s.source_in(root)),
            other_tests: Vec::new(),
            short: (short.clone(), from_root(root, &s.dir, &s.command)),
        });
    }
}

/// An app server found next to the tests.
#[derive(Debug, Clone)]
struct Server {
    /// The name its tests carry (the folder above `server/`); empty when
    /// none.
    name: String,
    /// Where it runs from.
    dir: std::path::PathBuf,
    command: String,
    source: String,
}

impl Server {
    /// Its source with its folder relative to `root`.
    fn source_in(&self, root: &Path) -> String {
        match self.dir.strip_prefix(root) {
            Ok(rel) if !rel.as_os_str().is_empty() => {
                format!("{}/{}", rel.display(), self.source)
            }
            _ => self.source.clone(),
        }
    }

    fn at(dir: &Path, name: &str, command: String, source: String) -> Self {
        Self {
            name: name.into(),
            dir: dir.to_path_buf(),
            command,
            source,
        }
    }
}

/// The folders in `dir`, sorted (hidden and skipped ones left out),
/// charging `budget`.
fn subfolders(dir: &Path, budget: &mut usize) -> Vec<(String, std::path::PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, std::path::PathBuf)> = entries
        .flatten()
        .take_while(|_| spend(budget))
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .filter(|(name, _)| !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()))
        .collect();
    out.sort();
    out
}

/// The servers of the `<x>/server` folders below `dir`, by `x`.
fn integration_servers(dir: &Path, runner: Option<Ecosystem>, budget: &mut usize) -> Vec<Server> {
    subfolders(dir, budget)
        .into_iter()
        .filter(|(name, path)| name != "server" && path.join("server").is_dir())
        .filter_map(|(name, path)| server_of(&path.join("server"), &name, runner, budget))
        .collect()
}

/// The server in folder `server`, named `name`: its own start command,
/// else the one of the folder in it that has one, preferring installed
/// dependencies, then the test runner's ecosystem, then Node, Python, Ruby,
/// Go, then the shortest name.
fn server_of(
    server: &Path,
    name: &str,
    runner: Option<Ecosystem>,
    budget: &mut usize,
) -> Option<Server> {
    if !server.is_dir() {
        return None;
    }
    if let Some((cmd, src, _)) = start_command(server, budget) {
        return Some(Server::at(server, name, cmd, src));
    }
    let own = match runner {
        Some(Ecosystem::Node) => Some(0),
        Some(Ecosystem::Pytest) => Some(1),
        Some(Ecosystem::Rspec) => Some(2),
        Some(Ecosystem::Go) => Some(3),
        _ => None,
    };
    // Not installed, not the runner's language, ecosystem, name length, name.
    type Rank = (bool, bool, u8, usize, String);
    let mut best: Option<(Rank, Server)> = None;
    for (lang, path) in subfolders(server, budget) {
        let Some((cmd, src, rank)) = start_command(&path, budget) else {
            continue;
        };
        let key = (
            !installed(&path, rank),
            own != Some(rank),
            rank,
            lang.len(),
            lang.clone(),
        );
        if best.as_ref().is_none_or(|(k, _)| key < *k) {
            best = Some((key, Server::at(&path, name, cmd, src)));
        }
    }
    best.map(|(_, s)| s)
}

/// Whether the dependencies of the app in `dir` (of ecosystem `rank`, as
/// [`start_command`] gives it) are installed there.
fn installed(dir: &Path, rank: u8) -> bool {
    let any = |names: &[&str]| names.iter().any(|n| dir.join(n).is_dir());
    match rank {
        0 => any(&["node_modules"]),
        1 => any(&[".venv", "venv"]),
        2 => any(&["vendor/bundle", ".bundle"]),
        _ => false,
    }
}

/// How folder `dir` starts its app: `(command, source, rank)`, the rank
/// ordering ecosystems (Node, Python, Ruby, Go).
fn start_command(dir: &Path, budget: &mut usize) -> Option<(String, String, u8)> {
    if dir.join("package.json").is_file()
        && let Some(text) = read(&dir.join("package.json"), budget)
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(scripts) = json["scripts"].as_object()
    {
        let tool = if dir.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if dir.join("yarn.lock").is_file() {
            "yarn"
        } else {
            "npm"
        };
        for name in ["start", "dev", "serve"] {
            if scripts
                .get(name)
                .and_then(|v| v.as_str())
                .is_some_and(|v| !v.trim().is_empty())
            {
                let command = if name == "start" {
                    format!("{tool} start")
                } else {
                    format!("{tool} run {name}")
                };
                return Some((command, format!("package.json scripts.{name}"), 0));
            }
        }
    }
    for file in ["app.py", "server.py", "main.py"] {
        if dir.join(file).is_file() {
            return Some((format!("python3 {file}"), file.to_owned(), 1));
        }
    }
    for file in ["server.rb", "app.rb"] {
        if dir.join(file).is_file() {
            let command = if dir.join("Gemfile").is_file() {
                format!("bundle exec ruby {file}")
            } else {
                format!("ruby {file}")
            };
            return Some((command, file.to_owned(), 2));
        }
    }
    if dir.join("go.mod").is_file()
        && ["main.go", "server.go"]
            .iter()
            .any(|f| dir.join(f).is_file())
    {
        return Some(("go run .".into(), "go.mod".into(), 3));
    }
    None
}

/// The local base URLs with a port in the test config and helpers of
/// `dir`: `(file, runner, port)`, in [`BASE_URL_FILES`] order, the first of
/// each file.
fn base_urls(dir: &Path, avoid: &[u16], budget: &mut usize) -> Vec<(String, Helper, u16)> {
    let mut out = Vec::new();
    for (file, helper) in BASE_URL_FILES {
        let path = dir.join(file);
        if !path.is_file() {
            continue;
        }
        let Some(text) = read(&path, budget) else {
            continue;
        };
        if let Some(port) = text.lines().find_map(|line| base_url_port(line, avoid)) {
            out.push(((*file).to_owned(), *helper, port));
        }
    }
    out
}

/// The port of a local URL that `line` sets for a base-URL identifier;
/// `None` for a comment line or a port in `avoid`.
fn base_url_port(line: &str, avoid: &[u16]) -> Option<u16> {
    let code = line.trim_start();
    if ["#", "//", "/*", "*", "--", ";"]
        .iter()
        .any(|c| code.starts_with(c))
    {
        return None;
    }
    let named = code
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| BASE_URL_KEYS.contains(&word.to_ascii_lowercase().as_str()));
    if !named {
        return None;
    }
    local_ports(code).into_iter().find(|p| !avoid.contains(p))
}

/// The ports of the `http(s)://localhost:<port>` (or 127.0.0.1, 0.0.0.0,
/// [::1]) URLs in `text`.
fn local_ports(text: &str) -> Vec<u16> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let scheme_ok = rest[..i].ends_with("http") || rest[..i].ends_with("https");
        let after = &rest[i + 3..];
        rest = after;
        if !scheme_ok {
            continue;
        }
        for host in ["localhost:", "127.0.0.1:", "0.0.0.0:", "[::1]:"] {
            if let Some(port) = after.strip_prefix(host) {
                let digits: String = port.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(port) = digits.parse::<u16>()
                    && port > 0
                {
                    out.push(port);
                }
            }
        }
    }
    out
}

/// Playwright's `webServer` in a config of `dir`, when Playwright reuses a
/// server already running (`reuseExistingServer: true`); otherwise it
/// starts the server itself and needs no pair.
struct WebServer {
    file: String,
    command: String,
    dir: std::path::PathBuf,
    port: Option<u16>,
}

fn web_server(dir: &Path, budget: &mut usize) -> Option<WebServer> {
    for (file, helper) in BASE_URL_FILES {
        if *helper != Helper::Playwright || !dir.join(file).is_file() {
            continue;
        }
        let Some(text) = read(&dir.join(file), budget) else {
            continue;
        };
        let Some(at) = text.find("webServer") else {
            continue;
        };
        let end = (at..=text.len().min(at + 1200))
            .rev()
            .find(|i| text.is_char_boundary(*i))
            .unwrap_or(at);
        let block = &text[at..end];
        if !js_value(block, "reuseExistingServer").is_some_and(|v| v.starts_with("true")) {
            continue;
        }
        let Some(command) = js_string(block, "command") else {
            continue;
        };
        let port = js_number(block, "port")
            .or_else(|| js_string(block, "url").and_then(|u| local_ports(&u).first().copied()));
        let dir = match js_string(block, "cwd") {
            Some(cwd) => dir.join(cwd),
            None => dir.to_path_buf(),
        };
        return Some(WebServer {
            file: (*file).to_owned(),
            command,
            dir,
            port,
        });
    }
    None
}

/// What follows `key:` in `block` (`key` a whole word).
fn js_value<'a>(block: &'a str, key: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(i) = block[from..].find(key) {
        let at = from + i;
        from = at + key.len();
        let before_ok = block[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        let after = block[from..].trim_start();
        if before_ok && let Some(value) = after.strip_prefix(':') {
            return Some(value.trim_start());
        }
    }
    None
}

/// The string literal set for `key` in `block`.
fn js_string(block: &str, key: &str) -> Option<String> {
    let value = js_value(block, key)?;
    let quote = value.chars().next().filter(|c| "'\"`".contains(*c))?;
    let inner = &value[1..];
    let end = inner.find(quote)?;
    Some(inner[..end].to_owned()).filter(|s| !s.trim().is_empty())
}

/// The number set for `key` in `block`.
fn js_number(block: &str, key: &str) -> Option<u16> {
    let value = js_value(block, key)?;
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|p| *p > 0)
}

/// The test files of `runner` in `dir`: `(path relative to dir, name
/// stem)`, sorted. Only runners that take files on their command line.
fn test_files(dir: &Path, runner: Option<Ecosystem>, budget: &mut usize) -> Vec<(String, String)> {
    let folders: &[&str] = match runner {
        Some(Ecosystem::Rspec) => &["spec"],
        Some(Ecosystem::Pytest) => &["tests", "test"],
        Some(Ecosystem::Node) => &["test", "tests", "e2e", "spec", "__tests__"],
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for folder in folders {
        let Ok(entries) = std::fs::read_dir(dir.join(folder)) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .take_while(|_| spend(budget))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            if let Some(stem) = test_stem(&name, runner) {
                out.push((format!("{folder}/{name}"), stem));
            }
        }
    }
    out.sort();
    out
}

/// The stem of a test file's name (`x_spec.rb` → `x`), when it is one.
fn test_stem(name: &str, runner: Option<Ecosystem>) -> Option<String> {
    match runner {
        Some(Ecosystem::Rspec) => name.strip_suffix("_spec.rb").map(str::to_owned),
        Some(Ecosystem::Pytest) => {
            let base = name.strip_suffix(".py")?;
            base.strip_prefix("test_")
                .or_else(|| base.strip_suffix("_test"))
                .map(str::to_owned)
        }
        Some(Ecosystem::Node) => [".spec.", ".test."]
            .iter()
            .find_map(|m| name.find(m).map(|i| name[..i].to_owned())),
        _ => None,
    }
}

/// `s` lower case with `-`, `.` and spaces as `_`.
fn norm(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .map(|c| if "-. ".contains(c) { '_' } else { c })
        .collect()
}

/// The sample of `names` (normalised) a test file's stem belongs to: the
/// longest one it starts with as whole words.
fn owner<'a>(stem: &str, names: &'a [String]) -> Option<&'a String> {
    let stem = norm(stem);
    names
        .iter()
        .filter(|n| {
            stem.strip_prefix(n.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('_'))
        })
        .max_by_key(|n| n.len())
}

/// The files of sample `name` (normalised; `names` are all the samples):
/// those to run, and those left out. When some of its files also name the
/// server (`x_server_spec.rb`), only those run.
fn files_of(
    files: &[(String, String)],
    name: &str,
    names: &[String],
) -> (Vec<String>, Vec<String>) {
    let mine: Vec<(&String, bool)> = files
        .iter()
        .filter(|(_, stem)| owner(stem, names).is_some_and(|o| o == name))
        .map(|(path, stem)| {
            let rest = norm(stem)[name.len()..].to_owned();
            (path, rest.split('_').any(|w| w == "server"))
        })
        .collect();
    let server = mine.iter().any(|(_, s)| *s);
    let (run, left): (Vec<_>, Vec<_>) = mine.into_iter().partition(|(_, s)| !server || *s);
    (
        run.into_iter().map(|(p, _)| p.clone()).collect(),
        left.into_iter().map(|(p, _)| p.clone()).collect(),
    )
}

/// Characters of the [`step`] line, at most (about).
const STEP_CHARS: usize = 300;

/// The one-line `next_steps` entry naming the app's test commands (first
/// a pair that needs its server, ready to run) and how to run them against
/// the mocks; `None` when there are none. What does not fit is counted and
/// left to the answer's `app_tests`.
#[must_use]
pub fn step(found: &Detected) -> Option<String> {
    if found.pairs.is_empty() {
        return commands_step(&found.commands);
    }
    let tail = " Run each before handing back (CLI, from the project root: `mockingbird verify --run \"<cmd>\" --serve \"<cmd>\" --serve-port <port>`).";
    let head = "The app's tests need its own server running; from the project root (verify_run's default cwd): ";
    let mut used = head.len() + tail.len();
    let mut named = Vec::new();
    for p in &found.pairs {
        let d = describe_pair(p);
        if !named.is_empty() && used + d.len() + 2 > STEP_CHARS {
            break;
        }
        used += d.len() + 2;
        named.push(d);
    }
    let mut line = format!("{head}{}", named.join("; "));
    let more = found.pairs.len() - named.len();
    if more > 0 {
        line.push_str(&format!(", and {more} more (app_tests.pairs)"));
    }
    line.push('.');
    // The others: commands no pair runs a part of.
    let others: Vec<&TestCommand> = found
        .commands
        .iter()
        .filter(|c| {
            !found.pairs.iter().any(|p| {
                p.command == c.command || p.command.starts_with(&format!("{} ", c.command))
            })
        })
        .collect();
    let mut shown = Vec::new();
    let mut length = line.len() + tail.len() + " Other tests: , N more.".len();
    for c in &others {
        let d = c.describe();
        if length + d.len() + 2 > STEP_CHARS {
            break;
        }
        length += d.len() + 2;
        shown.push(d);
    }
    if !others.is_empty() {
        let left = others.len() - shown.len();
        if shown.is_empty() {
            line.push_str(&format!(" Other tests: {left} in app_tests.commands."));
        } else if left > 0 {
            line.push_str(&format!(" Other tests: {}, {left} more.", shown.join(", ")));
        } else {
            line.push_str(&format!(" Other tests: {}.", shown.join(", ")));
        }
    }
    line.push_str(tail);
    Some(line)
}

/// `verify_run {command: "…", serve: "…", serve_port: N}`.
fn describe_pair(p: &ServePair) -> String {
    let port = p
        .serve_port
        .map(|n| format!(", serve_port: {n}"))
        .unwrap_or_default();
    format!(
        "verify_run {{command: {:?}, serve: {:?}{port}}}",
        p.short.0, p.short.1
    )
}

/// The line naming `commands` when none needs the app's server.
fn commands_step(commands: &[TestCommand]) -> Option<String> {
    if commands.is_empty() {
        return None;
    }
    let tail = " Run each with verify_run {command} (CLI, from the project root: `mockingbird verify --run \"<cmd>\"`) before handing back.";
    let head = "The app's tests, from the project root (verify_run's default cwd): ";
    let mut named = Vec::new();
    let mut used = head.len() + tail.len();
    for c in commands {
        let d = c.describe();
        // Always name the first; stop before the line runs long.
        if !named.is_empty() && used + d.len() + 2 > STEP_CHARS {
            break;
        }
        used += d.len() + 2;
        named.push(d);
    }
    let more = commands.len() - named.len();
    let more = if more > 0 {
        format!(", {more} more")
    } else {
        String::new()
    };
    Some(format!("{head}{}{more}.{tail}", named.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The commands without their `cd`, which every one carries.
    fn commands(root: &Path) -> Vec<String> {
        detect(root)
            .into_iter()
            .map(|c| {
                assert!(c.command.starts_with("cd /"), "{}", c.command);
                c.bare
            })
            .collect()
    }

    fn cd(dir: &Path, command: &str) -> String {
        in_folder(dir, command)
    }

    #[test]
    fn package_json_test_and_e2e_scripts() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts": {"test": "jest", "test:e2e": "playwright test", "test:watch": "jest --watch", "test:lint": "eslint .", "test:types": "tsc", "build": "tsc", "e2e": "cypress run"}}"#,
        );
        assert_eq!(
            commands(dir.path()),
            ["npm test", "npm run e2e", "npm run test:e2e"]
        );
        assert_eq!(detect(dir.path())[0].source, "package.json scripts.test");
        write(dir.path(), "pnpm-lock.yaml", "");
        assert_eq!(commands(dir.path())[0], "pnpm test");
    }

    #[test]
    fn npm_placeholder_test_script_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#,
        );
        assert!(detect(dir.path()).is_empty());
    }

    #[test]
    fn makefile_test_target() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Makefile",
            "VAR := 1\nbuild:\n\tcc x.c\ntest: build\n\t./run-tests\n",
        );
        assert_eq!(commands(dir.path()), ["make test"]);
        let other = tempfile::tempdir().unwrap();
        write(
            other.path(),
            "Makefile",
            "all:\n\ttrue\ntest-e2e:\n\ttrue\n",
        );
        assert_eq!(commands(other.path()), ["make test-e2e"]);
    }

    #[test]
    fn pytest_by_config_or_tests_folder() {
        for (file, text) in [
            ("pytest.ini", "[pytest]\n"),
            (
                "pyproject.toml",
                "[tool.pytest.ini_options]\naddopts = \"-q\"\n",
            ),
            ("setup.cfg", "[tool:pytest]\n"),
            ("tests/test_api.py", "def test_x(): pass\n"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), file, text);
            assert_eq!(commands(dir.path()), ["pytest"], "{file}");
        }
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname = \"x\"\n");
        write(dir.path(), "tests/helpers.py", "");
        assert!(detect(dir.path()).is_empty());
    }

    #[test]
    fn rspec_needs_spec_and_rspec_config_or_gem() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "spec/api_spec.rb", "");
        assert!(detect(dir.path()).is_empty());
        write(dir.path(), ".rspec", "--require spec_helper\n");
        assert_eq!(commands(dir.path()), ["rspec"]);
        write(dir.path(), "Gemfile", "gem 'rspec'\n");
        assert_eq!(commands(dir.path()), ["bundle exec rspec"]);
        assert_eq!(detect(dir.path())[0].source, "spec/");
    }

    #[test]
    fn go_test_needs_a_test_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "go.mod", "module x\n");
        write(dir.path(), "main.go", "package main\n");
        assert!(detect(dir.path()).is_empty());
        write(dir.path(), "internal/api/api_test.go", "package api\n");
        assert_eq!(commands(dir.path()), ["go test ./..."]);
    }

    #[test]
    fn cargo_test() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        assert_eq!(commands(dir.path()), ["cargo test"]);
    }

    #[test]
    fn subprojects_run_from_the_root_and_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "web/package.json",
            r#"{"scripts": {"test": "vitest"}}"#,
        );
        write(dir.path(), "api/pytest.ini", "[pytest]\n");
        write(
            dir.path(),
            "node_modules/x/package.json",
            r#"{"scripts": {"test": "x"}}"#,
        );
        let found = detect(dir.path());
        let cmds: Vec<&str> = found.iter().map(|c| c.command.as_str()).collect();
        assert_eq!(
            cmds,
            [
                cd(&dir.path().join("api"), "pytest"),
                cd(&dir.path().join("web"), "npm test")
            ]
        );
        assert_eq!(found[1].source, "web/package.json scripts.test");
        assert_eq!(found[1].dir, "web");
        for i in 0..12 {
            write(dir.path(), &format!("p{i:02}/Cargo.toml"), "");
        }
        assert_eq!(detect(dir.path()).len(), MAX_COMMANDS);
    }

    #[test]
    fn a_workspace_or_rootdir_covers_its_subfolders() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        write(
            dir.path(),
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\n",
        );
        write(dir.path(), "pytest.ini", "[pytest]\n");
        write(dir.path(), "svc/pytest.ini", "[pytest]\n");
        write(
            dir.path(),
            "web/package.json",
            r#"{"scripts": {"test": "vitest"}}"#,
        );
        assert_eq!(commands(dir.path()), ["pytest", "cargo test", "npm test"]);
        let make = tempfile::tempdir().unwrap();
        write(make.path(), "Makefile", "check:\n\tlint\n");
        assert!(
            detect(make.path()).is_empty(),
            "a check target is not a test"
        );
    }

    fn tc(command: &str, source: &str) -> TestCommand {
        TestCommand {
            command: command.into(),
            dir: ".".into(),
            source: source.into(),
            bare: command.into(),
            short: command.into(),
            runner: None,
            script: String::new(),
        }
    }

    fn only(commands: Vec<TestCommand>) -> Detected {
        Detected {
            commands,
            pairs: Vec::new(),
        }
    }

    #[test]
    fn the_step_line_names_the_commands_briefly() {
        assert_eq!(step(&Detected::default()), None);
        let c = tc;
        let line = step(&only(vec![
            c("npm test", "package.json scripts.test"),
            c("bundle exec rspec", "spec/"),
        ]))
        .unwrap();
        assert_eq!(
            line,
            "The app's tests, from the project root (verify_run's default cwd): `npm test` (package.json scripts.test), `bundle exec rspec` (spec/). Run each with verify_run {command} (CLI, from the project root: `mockingbird verify --run \"<cmd>\"`) before handing back."
        );
        let many: Vec<TestCommand> = (0..8)
            .map(|i| {
                c(
                    &format!("cd sub{i} && npm run test:e2e"),
                    "sub/package.json scripts.test:e2e",
                )
            })
            .collect();
        let line = step(&only(many)).unwrap();
        assert!(line.len() <= STEP_CHARS + 20, "{} {line}", line.len());
        assert!(line.contains(" more."), "{line}");
    }

    fn pair(command: String, serve: String, port: u16, source: &str) -> ServePair {
        ServePair {
            command,
            serve,
            serve_port: Some(port),
            source: source.into(),
            other_tests: Vec::new(),
            short: Default::default(),
        }
    }

    #[test]
    fn a_server_url_in_spec_helper_and_a_server_folder_make_a_pair() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, ".rspec", "--require spec_helper\n");
        write(
            root,
            "spec/spec_helper.rb",
            "# SERVER_URL = 'http://localhost:9999' (old)\nSERVER_URL = ENV.fetch('SERVER_URL', 'http://localhost:4242')\n",
        );
        write(root, "spec/orders_spec.rb", "");
        write(
            root,
            "server/package.json",
            r#"{"scripts": {"start": "node server.js"}}"#,
        );
        let found = detect_all(root, &[]);
        assert_eq!(
            found.pairs,
            [pair(
                cd(root, "rspec"),
                cd(&root.join("server"), "npm start"),
                4242,
                "spec/spec_helper.rb names http://localhost:4242; server/package.json scripts.start"
            )]
        );
        // Without a server folder, a start script next to the tests.
        let app = tempfile::tempdir().unwrap();
        write(
            app.path(),
            "package.json",
            r#"{"scripts": {"test": "jest", "dev": "vite"}}"#,
        );
        write(app.path(), ".env.test", "APP_URL=http://127.0.0.1:5173\n");
        let found = detect_all(app.path(), &[]);
        assert_eq!(found.pairs.len(), 1, "{found:?}");
        assert_eq!(found.pairs[0].serve, cd(app.path(), "npm run dev"));
        assert_eq!(found.pairs[0].serve_port, Some(5173));
    }

    #[test]
    fn base_urls_are_whole_names_outside_comments_on_ports_not_mockingbirds() {
        for (line, avoid, want) in [
            ("SERVER_URL = 'http://localhost:4242'", &[][..], Some(4242)),
            (
                "const baseURL = process.env.BASE_URL ?? 'http://127.0.0.1:3000';",
                &[],
                Some(3000),
            ),
            ("DATABASE_URL=http://localhost:5432", &[], None),
            ("OPENAI_BASE_URL = 'http://localhost:8080/v1'", &[], None),
            ("my_app_url_suffix = 'http://localhost:3000'", &[], None),
            ("# BASE_URL=http://localhost:3000", &[], None),
            ("  // baseURL: 'http://localhost:3000',", &[], None),
            ("BASE_URL = 'https://staging.example.com:8443'", &[], None),
            // The config's own ports are Mockingbird's, not the app's.
            ("BASE_URL=http://localhost:8080", &[8080], None),
            (
                "APP_URL = ENV['A'] || 'http://localhost:9090' || 'http://localhost:4000'",
                &[9090],
                Some(4000),
            ),
        ] {
            assert_eq!(base_url_port(line, avoid), want, "{line}");
        }
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".rspec", "");
        write(
            dir.path(),
            "spec/spec_helper.rb",
            "DATABASE_URL = 'http://localhost:5432'\nBASE_URL = 'http://localhost:8080'\n",
        );
        write(dir.path(), "server/app.py", "");
        assert!(detect_all(dir.path(), &[8080]).pairs.is_empty());
        assert_eq!(detect_all(dir.path(), &[]).pairs.len(), 1);
    }

    #[test]
    fn each_helper_pairs_with_its_own_runner() {
        // Jest unit tests and Playwright e2e tests in one package: the
        // Playwright config's base URL pairs with the Playwright script.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "package.json",
            r#"{"scripts": {"test": "jest", "test:e2e": "playwright test", "start": "node server.js"}}"#,
        );
        write(
            root,
            "playwright.config.ts",
            "export default defineConfig({ use: { baseURL: 'http://localhost:3000' } });\n",
        );
        let found = detect_all(root, &[]);
        assert_eq!(
            found.pairs,
            [pair(
                cd(root, "npm run test:e2e"),
                cd(root, "npm start"),
                3000,
                "playwright.config.ts names http://localhost:3000; package.json scripts.start"
            )]
        );
        let line = step(&found).unwrap();
        assert!(
            line.contains(
                "serve: \"npm start\", serve_port: 3000}. Other tests: 1 in app_tests.commands."
            ),
            "{line}"
        );
        // A jest setup with its own base URL pairs with `npm test`.
        write(
            root,
            "jest.setup.js",
            "process.env.API_BASE = 'x';\nconst SERVER_URL = 'http://localhost:4000';\n",
        );
        let found = detect_all(root, &[]);
        let commands: Vec<(&str, Option<u16>)> = found
            .pairs
            .iter()
            .map(|p| (p.command.as_str(), p.serve_port))
            .collect();
        let (e2e, unit) = (cd(root, "npm run test:e2e"), cd(root, "npm test"));
        assert_eq!(
            commands,
            [(e2e.as_str(), Some(3000)), (unit.as_str(), Some(4000))]
        );
        // A Playwright config with no script running it: `npx playwright test`.
        let bare = tempfile::tempdir().unwrap();
        write(
            bare.path(),
            "e2e/package.json",
            r#"{"scripts": {"test": "jest"}}"#,
        );
        write(
            bare.path(),
            "e2e/playwright.config.js",
            "module.exports = { use: { baseURL: 'http://localhost:3000' } };\n",
        );
        write(
            bare.path(),
            "package.json",
            r#"{"scripts": {"start": "node app.js"}}"#,
        );
        let found = detect_all(bare.path(), &[]);
        assert_eq!(found.pairs.len(), 1, "{found:?}");
        assert_eq!(
            found.pairs[0].command,
            cd(&bare.path().join("e2e"), "npx playwright test")
        );
        assert_eq!(found.pairs[0].serve, cd(bare.path(), "npm start"));
    }

    #[test]
    fn playwrights_web_server_pairs_only_when_it_reuses_a_running_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "e2e/package.json",
            r#"{"scripts": {"test": "playwright test"}}"#,
        );
        let config = |reuse: &str| {
            format!(
                "export default defineConfig({{\n  webServer: {{\n    command: 'npm run start',\n    url: 'http://127.0.0.1:3000',\n    cwd: '../app',\n    reuseExistingServer: {reuse},\n  }},\n}});\n"
            )
        };
        // Playwright starts the server itself.
        write(root, "e2e/playwright.config.ts", &config("!process.env.CI"));
        assert!(detect_all(root, &[]).pairs.is_empty());
        write(root, "e2e/playwright.config.ts", &config("true"));
        let found = detect_all(root, &[]);
        let mut want = pair(
            cd(&root.join("e2e"), "npm test"),
            cd(&root.join("e2e").join("../app"), "npm run start"),
            3000,
            "e2e/playwright.config.ts webServer (reuseExistingServer: true)",
        );
        want.serve_port = Some(3000);
        assert_eq!(found.pairs, [want]);
        let line = step(&found).unwrap();
        assert!(
            line.starts_with("The app's tests need its own server running; from the project root (verify_run's default cwd): verify_run {command: \"cd e2e && npm test\", serve: \"cd e2e/../app && npm run start\", serve_port: 3000}"),
            "{line}"
        );
    }

    #[test]
    fn samples_pair_their_tests_with_their_servers_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "Gemfile", "gem 'rspec'\n");
        write(
            root,
            "spec/spec_helper.rb",
            "BASE_URL = ENV['BASE_URL'] || \"http://localhost:4242\"\n",
        );
        for spec in [
            "checkout_flow_server_spec.rb",
            "checkout_flow_e2e_spec.rb",
            "checkout_spec.rb",
            "hosted_page_spec.rb",
            "unrelated_spec.rb",
        ] {
            write(root, &format!("spec/{spec}"), "");
        }
        // checkout-flow: a folder per language; the installed Node one
        // starts it, before the runner's own Ruby.
        write(root, "checkout-flow/server/README.md", "");
        write(
            root,
            "checkout-flow/server/node/package.json",
            r#"{"scripts": {"start": "node server.js"}}"#,
        );
        std::fs::create_dir_all(root.join("checkout-flow/server/node/node_modules")).unwrap();
        write(
            root,
            "checkout-flow/server/node-typescript/package.json",
            r#"{"scripts": {"start": "ts-node server.ts"}}"#,
        );
        write(root, "checkout-flow/server/ruby/server.rb", "");
        // `checkout` is a prefix of `checkout_flow`: its file is its own.
        write(root, "checkout/server/python/server.py", "");
        write(root, "checkout/server/ruby/app.rb", "");
        // Nothing installed: the runner's ecosystem (Ruby) first.
        write(
            root,
            "hosted-page/server/node/package.json",
            r#"{"scripts": {"start": "node server.js"}}"#,
        );
        write(root, "hosted-page/server/ruby/server.rb", "");
        write(root, "hosted-page/server/ruby/Gemfile", "");
        // No tests carry its name: no pair.
        write(root, "other-sample/server/app.py", "");
        let found = detect_all(root, &[]);
        let base = "spec/spec_helper.rb names http://localhost:4242";
        let mut flow = pair(
            cd(root, "bundle exec rspec spec/checkout_flow_server_spec.rb"),
            cd(&root.join("checkout-flow/server/node"), "npm start"),
            4242,
            &format!("{base}; checkout-flow/server/node/package.json scripts.start"),
        );
        flow.other_tests = vec!["spec/checkout_flow_e2e_spec.rb".into()];
        assert_eq!(
            found.pairs,
            [
                pair(
                    cd(root, "bundle exec rspec spec/checkout_spec.rb"),
                    cd(&root.join("checkout/server/ruby"), "ruby app.rb"),
                    4242,
                    &format!("{base}; checkout/server/ruby/app.rb"),
                ),
                flow,
                pair(
                    cd(root, "bundle exec rspec spec/hosted_page_spec.rb"),
                    cd(
                        &root.join("hosted-page/server/ruby"),
                        "bundle exec ruby server.rb"
                    ),
                    4242,
                    &format!("{base}; hosted-page/server/ruby/server.rb"),
                ),
            ]
        );
        let line = step(&found).unwrap();
        assert!(
            line.starts_with("The app's tests need its own server running; from the project root (verify_run's default cwd): verify_run {command: \"bundle exec rspec spec/checkout_spec.rb\", serve: \"cd checkout/server/ruby && ruby app.rb\", serve_port: 4242}")
                && line.contains("more (app_tests.pairs).")
                && !line.contains("Other tests"),
            "{line}"
        );
        // Capped.
        for i in 0..12 {
            write(root, &format!("s{i:02}/server/app.py"), "");
            write(root, &format!("spec/s{i:02}_spec.rb"), "");
        }
        assert_eq!(detect_all(root, &[]).pairs.len(), MAX_PAIRS);
    }

    #[test]
    fn a_top_level_server_still_serves_when_no_sample_matches() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, ".rspec", "");
        write(
            root,
            "spec/spec_helper.rb",
            "SERVER_URL = 'http://localhost:4242'\n",
        );
        write(root, "spec/api_spec.rb", "");
        write(root, "docs-a/server/app.py", "");
        write(root, "docs-b/server/app.py", "");
        write(root, "server/app.py", "");
        let found = detect_all(root, &[]);
        assert_eq!(
            found.pairs,
            [pair(
                cd(root, "rspec"),
                cd(&root.join("server"), "python3 app.py"),
                4242,
                "spec/spec_helper.rb names http://localhost:4242; server/app.py"
            )]
        );
    }
}
