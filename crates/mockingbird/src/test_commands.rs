//! The app's own test commands, found from its project root
//! ([`crate::hosts::project_root`]) so an agent can run them against the
//! mocks (`mockingbird verify --run "<cmd>"`, MCP `verify_run`).
//!
//! Detection is cheap and bounded: it reads a few manifest files in the
//! root and in folders up to [`MAX_DEPTH`] below it (skipping dependencies,
//! build outputs and hidden folders), looks at no more than
//! [`MAX_ENTRIES`] directory entries in all, and lists at most
//! [`MAX_COMMANDS`] commands:
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
//! - Playwright's `webServer` (`command`, `port` or `url`, `cwd`);
//! - else a local base URL (`SERVER_URL`, `BASE_URL`, `baseURL`, `APP_URL`,
//!   ... set to `http://localhost:<port>`) in the test config or helpers
//!   (`spec/spec_helper.rb`, `conftest.py`, `playwright.config.*`,
//!   `cypress.config.*`, jest setup files, `.env.test`), whose port the
//!   pair waits for, with the server to start: one per `<x>/server` folder
//!   whose name the test files carry (`spec/x_server_spec.rb` with
//!   `x/server/node`; the files are named in the test command), else the
//!   one `server/` folder, else a `start`, `dev` or `serve` script. In a
//!   `server` folder with a folder per language, the one with a start
//!   command (a `package.json` script; `app.py`, `server.py` or `main.py`;
//!   `server.rb` or `app.rb`; a Go `main`) and the shortest name is used.

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
    /// `command` and `serve` from the project root.
    #[serde(skip)]
    short: (String, String),
}

impl PartialEq for ServePair {
    /// The forms from the project root follow from the others.
    fn eq(&self, other: &Self) -> bool {
        (&self.command, &self.serve, self.serve_port, &self.source)
            == (
                &other.command,
                &other.serve,
                other.serve_port,
                &other.source,
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

/// The test commands of the project at `root` and their serve pairs.
#[must_use]
pub fn detect_all(root: &Path) -> Detected {
    let commands = detect(root);
    let pairs = pairs(root, &commands);
    Detected { commands, pairs }
}

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
            if budget == 0 {
                break;
            }
            budget -= 1;
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
        for (command, source, eco) in in_dir(dir, &mut budget) {
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
fn shell_word(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-:@+".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn read(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(MAX_FILE_BYTES).read_to_string(&mut text).ok()?;
    Some(text)
}

/// The test commands of one folder: `(command, source, runner)`.
fn in_dir(dir: &Path, budget: &mut usize) -> Vec<(String, String, Ecosystem)> {
    let mut out: Vec<(String, String, Ecosystem)> = node(dir)
        .into_iter()
        .map(|(c, s)| (c, s, Ecosystem::Node))
        .collect();
    if let Some((c, s)) = make(dir) {
        out.push((c, s, Ecosystem::Make));
    }
    if let Some(source) = pytest(dir) {
        out.push(("pytest".into(), source, Ecosystem::Pytest));
    }
    if let Some((c, s)) = rspec(dir) {
        out.push((c, s, Ecosystem::Rspec));
    }
    if dir.join("go.mod").is_file() && has_go_test(dir, budget) {
        out.push((
            "go test ./...".into(),
            "go.mod, *_test.go".into(),
            Ecosystem::Go,
        ));
    }
    if dir.join("Cargo.toml").is_file() {
        out.push(("cargo test".into(), "Cargo.toml".into(), Ecosystem::Cargo));
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

fn node(dir: &Path) -> Vec<(String, String)> {
    let Some(text) = read(&dir.join("package.json")) else {
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
        ));
    }
    let mut extra: Vec<&String> = scripts.keys().filter(|k| test_like(k)).collect();
    extra.sort();
    for name in extra {
        out.push((
            format!("{run} {}", shell_word(name)),
            format!("package.json scripts.{name}"),
        ));
    }
    out
}

fn make(dir: &Path) -> Option<(String, String)> {
    let (name, text) = ["GNUmakefile", "makefile", "Makefile"]
        .into_iter()
        .find_map(|name| read(&dir.join(name)).map(|t| (name, t)))?;
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

fn pytest(dir: &Path) -> Option<String> {
    if dir.join("pytest.ini").is_file() {
        return Some("pytest.ini".into());
    }
    if read(&dir.join("pyproject.toml")).is_some_and(|t| t.contains("[tool.pytest")) {
        return Some("pyproject.toml [tool.pytest]".into());
    }
    if read(&dir.join("setup.cfg")).is_some_and(|t| t.contains("[tool:pytest]")) {
        return Some("setup.cfg [tool:pytest]".into());
    }
    if read(&dir.join("tox.ini")).is_some_and(|t| t.contains("[pytest]")) {
        return Some("tox.ini [pytest]".into());
    }
    if dir.join("conftest.py").is_file() {
        return Some("conftest.py".into());
    }
    let tests = dir.join("tests");
    let has_test_file = std::fs::read_dir(&tests)
        .ok()?
        .flatten()
        .take(500)
        .any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
        });
    has_test_file.then(|| "tests/test_*.py".into())
}

fn rspec(dir: &Path) -> Option<(String, String)> {
    if !dir.join("spec").is_dir() {
        return None;
    }
    let gemfile = read(&dir.join("Gemfile"));
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

/// Files read for a base URL, relative to a test command's folder.
const BASE_URL_FILES: &[&str] = &[
    "spec/spec_helper.rb",
    "spec/rails_helper.rb",
    "conftest.py",
    "tests/conftest.py",
    "test/conftest.py",
    "playwright.config.ts",
    "playwright.config.js",
    "playwright.config.mjs",
    "playwright.config.cjs",
    "cypress.config.ts",
    "cypress.config.js",
    "cypress.config.mjs",
    "cypress.config.cjs",
    "jest.setup.js",
    "jest.setup.ts",
    "setupTests.js",
    "setupTests.ts",
    "src/setupTests.js",
    "src/setupTests.ts",
    ".env.test",
];

/// Names of a setting holding the app's base URL, lower case.
const BASE_URL_KEYS: &[&str] = &[
    "server_url",
    "serverurl",
    "base_url",
    "baseurl",
    "app_url",
    "appurl",
];

/// The test/serve pairs of `commands` (the first command of each folder).
fn pairs(root: &Path, commands: &[TestCommand]) -> Vec<ServePair> {
    let mut out: Vec<ServePair> = Vec::new();
    let mut folders: Vec<&str> = Vec::new();
    let mut budget = MAX_ENTRIES;
    for c in commands {
        if out.len() >= MAX_PAIRS {
            break;
        }
        if folders.contains(&c.dir.as_str()) {
            continue;
        }
        folders.push(&c.dir);
        let dir = if c.dir == "." {
            root.to_path_buf()
        } else {
            root.join(&c.dir)
        };
        let prefix = if c.dir == "." {
            String::new()
        } else {
            format!("{}/", c.dir)
        };
        if let Some(ws) = web_server(&dir) {
            out.push(ServePair {
                command: c.command.clone(),
                serve: in_folder(&ws.dir, &ws.command),
                serve_port: ws.port,
                source: format!("{prefix}{} webServer", ws.file),
                short: (c.short.clone(), from_root(root, &ws.dir, &ws.command)),
            });
            continue;
        }
        let Some((file, port)) = base_url(&dir) else {
            continue;
        };
        let found = format!("{prefix}{file} names http://localhost:{port}");
        let servers = integration_servers(&dir, &mut budget);
        let mut matched = false;
        if !servers.is_empty() {
            let files = test_files(&dir, c.runner);
            for s in &servers {
                if out.len() >= MAX_PAIRS {
                    break;
                }
                let mine = files_of(&files, &s.name);
                if mine.is_empty() {
                    continue;
                }
                matched = true;
                let args = mine
                    .iter()
                    .map(|f| shell_word(f))
                    .collect::<Vec<_>>()
                    .join(" ");
                let sep = if c.runner == Some(Ecosystem::Node) {
                    " -- "
                } else {
                    " "
                };
                let test = format!("{}{sep}{args}", c.bare);
                out.push(ServePair {
                    command: in_folder(&dir, &test),
                    serve: in_folder(&s.dir, &s.command),
                    serve_port: Some(port),
                    source: format!("{found}; {}", s.source_in(root)),
                    short: (
                        from_root(root, &dir, &test),
                        from_root(root, &s.dir, &s.command),
                    ),
                });
            }
        }
        if matched {
            continue;
        }
        let single = if servers.len() == 1 {
            servers.into_iter().next()
        } else if servers.is_empty() {
            server_of(&dir.join("server"), "server")
                .or_else(|| start_command(&dir).map(|(cmd, src, _)| Server::at(&dir, "", cmd, src)))
                .or_else(|| {
                    (c.dir != ".")
                        .then(|| {
                            start_command(root).map(|(cmd, src, _)| Server::at(root, "", cmd, src))
                        })
                        .flatten()
                })
        } else {
            None
        };
        if let Some(s) = single {
            out.push(ServePair {
                command: c.command.clone(),
                serve: in_folder(&s.dir, &s.command),
                serve_port: Some(port),
                source: format!("{found}; {}", s.source_in(root)),
                short: (c.short.clone(), from_root(root, &s.dir, &s.command)),
            });
        }
    }
    out
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

/// The servers of the `<x>/server` folders below `dir`, by `x`.
fn integration_servers(dir: &Path, budget: &mut usize) -> Vec<Server> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.')
            || SKIP_DIRS.contains(&name.as_str())
            || name == "server"
            || !entry.file_type().is_ok_and(|t| t.is_dir())
        {
            continue;
        }
        if entry.path().join("server").is_dir() {
            names.push(name);
        }
    }
    names.sort();
    names
        .into_iter()
        .filter_map(|name| server_of(&dir.join(&name).join("server"), &name))
        .collect()
}

/// The server in folder `server` (its own start command, else the one of
/// the folder in it that has one: the shortest name of the first
/// ecosystem), named `name`.
fn server_of(server: &Path, name: &str) -> Option<Server> {
    if !server.is_dir() {
        return None;
    }
    if let Some((cmd, src, _)) = start_command(server) {
        return Some(Server::at(server, name, cmd, src));
    }
    let mut best: Option<((u8, usize, String), Server)> = None;
    let entries = std::fs::read_dir(server).ok()?;
    for entry in entries.flatten().take(200) {
        let lang = entry.file_name().to_string_lossy().into_owned();
        if lang.starts_with('.') || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some((cmd, src, rank)) = start_command(&entry.path()) else {
            continue;
        };
        let key = (rank, lang.len(), lang.clone());
        if best.as_ref().is_none_or(|(k, _)| key < *k) {
            best = Some((key, Server::at(&entry.path(), name, cmd, src)));
        }
    }
    best.map(|(_, s)| s)
}

/// How folder `dir` starts its app: `(command, source, rank)`, the rank
/// ordering ecosystems (Node, Python, Ruby, Go).
fn start_command(dir: &Path) -> Option<(String, String, u8)> {
    if let Some(text) = read(&dir.join("package.json"))
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

/// The first local base URL with a port in the test config or helpers of
/// `dir`: `(file, port)`.
fn base_url(dir: &Path) -> Option<(String, u16)> {
    for file in BASE_URL_FILES {
        let Some(text) = read(&dir.join(file)) else {
            continue;
        };
        for line in text.lines() {
            let lower = line.to_ascii_lowercase();
            if BASE_URL_KEYS.iter().any(|k| lower.contains(k))
                && let Some(port) = local_port(line)
            {
                return Some(((*file).to_owned(), port));
            }
        }
    }
    None
}

/// The port of the first `http(s)://localhost:<port>` (or 127.0.0.1,
/// 0.0.0.0, [::1]) URL in `text`.
fn local_port(text: &str) -> Option<u16> {
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
                    return Some(port);
                }
            }
        }
    }
    None
}

/// Playwright's `webServer` in a config of `dir`.
struct WebServer {
    file: String,
    command: String,
    dir: std::path::PathBuf,
    port: Option<u16>,
}

fn web_server(dir: &Path) -> Option<WebServer> {
    for file in [
        "playwright.config.ts",
        "playwright.config.js",
        "playwright.config.mjs",
        "playwright.config.cjs",
        "playwright.config.mts",
    ] {
        let Some(text) = read(&dir.join(file)) else {
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
        let Some(command) = js_string(block, "command") else {
            continue;
        };
        let port = js_number(block, "port")
            .or_else(|| js_string(block, "url").and_then(|u| local_port(&u)));
        let dir = match js_string(block, "cwd") {
            Some(cwd) => dir.join(cwd),
            None => dir.to_path_buf(),
        };
        return Some(WebServer {
            file: file.to_owned(),
            command,
            dir,
            port,
        });
    }
    None
}

/// Where `key:` starts in `block` (as a whole word), and what follows it.
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
fn test_files(dir: &Path, runner: Option<Ecosystem>) -> Vec<(String, String)> {
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
        for entry in entries.flatten().take(500) {
            let name = entry.file_name().to_string_lossy().into_owned();
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

/// The files whose stem starts with `name` (as whole words); when some of
/// them also name the server (`x_server_spec.rb`), only those.
fn files_of(files: &[(String, String)], name: &str) -> Vec<String> {
    let name = norm(name);
    let mine: Vec<(&String, String)> = files
        .iter()
        .filter_map(|(path, stem)| {
            let stem = norm(stem);
            let rest = stem.strip_prefix(&name)?;
            (rest.is_empty() || rest.starts_with('_')).then(|| (path, rest.to_owned()))
        })
        .collect();
    let server: Vec<String> = mine
        .iter()
        .filter(|(_, rest)| rest.split('_').any(|w| w == "server"))
        .map(|(p, _)| (*p).clone())
        .collect();
    if server.is_empty() {
        mine.into_iter().map(|(p, _)| p.clone()).collect()
    } else {
        server
    }
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
    let head = "The app's tests need its own server running: ";
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
    let mut named = Vec::new();
    let mut used = "The app's tests: ".len() + tail.len();
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
    Some(format!(
        "The app's tests: {}{more}.{tail}",
        named.join(", ")
    ))
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
            "The app's tests: `npm test` (package.json scripts.test), `bundle exec rspec` (spec/). Run each with verify_run {command} (CLI, from the project root: `mockingbird verify --run \"<cmd>\"`) before handing back."
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

    #[test]
    fn a_server_url_in_spec_helper_and_a_server_folder_make_a_pair() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, ".rspec", "--require spec_helper\n");
        write(
            root,
            "spec/spec_helper.rb",
            "SERVER_URL = ENV.fetch('SERVER_URL', 'http://localhost:4242')\n",
        );
        write(root, "spec/orders_spec.rb", "");
        write(
            root,
            "server/package.json",
            r#"{"scripts": {"start": "node server.js"}}"#,
        );
        let found = detect_all(root);
        assert_eq!(
            found.pairs,
            [ServePair {
                command: cd(root, "rspec"),
                serve: cd(&root.join("server"), "npm start"),
                serve_port: Some(4242),
                source: "spec/spec_helper.rb names http://localhost:4242; server/package.json scripts.start".into(),
                short: Default::default(),
            }]
        );
        // A URL of another host is not the app's own server.
        write(
            root,
            "spec/spec_helper.rb",
            "SERVER_URL = 'https://api.example.com:8443'\n",
        );
        assert!(detect_all(root).pairs.is_empty());
        // Without a server folder, a start script next to the tests.
        let app = tempfile::tempdir().unwrap();
        write(
            app.path(),
            "package.json",
            r#"{"scripts": {"test": "jest", "dev": "vite"}}"#,
        );
        write(app.path(), ".env.test", "APP_URL=http://127.0.0.1:5173\n");
        let found = detect_all(app.path());
        assert_eq!(found.pairs.len(), 1, "{found:?}");
        assert_eq!(found.pairs[0].serve, cd(app.path(), "npm run dev"));
        assert_eq!(found.pairs[0].serve_port, Some(5173));
    }

    #[test]
    fn playwrights_web_server_makes_a_pair() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "e2e/package.json",
            r#"{"scripts": {"test": "playwright test"}}"#,
        );
        write(
            root,
            "e2e/playwright.config.ts",
            "export default defineConfig({\n  use: { baseURL: 'http://localhost:3000' },\n  webServer: {\n    command: 'npm run start',\n    url: 'http://127.0.0.1:3000',\n    cwd: '../app',\n    reuseExistingServer: !process.env.CI,\n  },\n});\n",
        );
        let found = detect_all(root);
        assert_eq!(
            found.pairs,
            [ServePair {
                command: cd(&root.join("e2e"), "npm test"),
                serve: cd(&root.join("e2e").join("../app"), "npm run start"),
                serve_port: Some(3000),
                source: "e2e/playwright.config.ts webServer".into(),
                short: Default::default(),
            }]
        );
        let line = step(&found).unwrap();
        assert!(
            line.starts_with("The app's tests need its own server running: verify_run {command: ")
                && line.contains("serve_port: 3000}")
                && !line.contains("Other tests"),
            "{line}"
        );
    }

    #[test]
    fn integrations_pair_their_tests_with_their_servers_by_name() {
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
            "hosted_page_spec.rb",
            "unrelated_spec.rb",
        ] {
            write(root, &format!("spec/{spec}"), "");
        }
        // checkout-flow: a folder per language; the Node one starts it.
        write(root, "checkout-flow/server/README.md", "");
        write(
            root,
            "checkout-flow/server/node/package.json",
            r#"{"scripts": {"start": "node server.js"}}"#,
        );
        write(
            root,
            "checkout-flow/server/node-typescript/package.json",
            r#"{"scripts": {"start": "ts-node server.ts"}}"#,
        );
        write(root, "checkout-flow/server/python/server.py", "");
        write(root, "hosted-page/server/ruby/server.rb", "");
        write(root, "hosted-page/server/ruby/Gemfile", "");
        // No tests carry its name: no pair.
        write(root, "other-sample/server/app.py", "");
        let found = detect_all(root);
        assert_eq!(
            found.pairs,
            [
                ServePair {
                    command: cd(
                        root,
                        "bundle exec rspec spec/checkout_flow_server_spec.rb"
                    ),
                    serve: cd(&root.join("checkout-flow/server/node"), "npm start"),
                    serve_port: Some(4242),
                    source: "spec/spec_helper.rb names http://localhost:4242; checkout-flow/server/node/package.json scripts.start".into(),
                    short: Default::default(),
                },
                ServePair {
                    command: cd(root, "bundle exec rspec spec/hosted_page_spec.rb"),
                    serve: cd(
                        &root.join("hosted-page/server/ruby"),
                        "bundle exec ruby server.rb"
                    ),
                    serve_port: Some(4242),
                    source: "spec/spec_helper.rb names http://localhost:4242; hosted-page/server/ruby/server.rb".into(),
                    short: Default::default(),
                },
            ]
        );
        let line = step(&found).unwrap();
        assert!(
            line.starts_with("The app's tests need its own server running: verify_run {command: \"bundle exec rspec spec/checkout_flow_server_spec.rb\", serve: \"cd checkout-flow/server/node && npm start\", serve_port: 4242}")
                && line.contains("1 more (app_tests.pairs).")
                && !line.contains("Other tests"),
            "{line}"
        );
        // Capped.
        for i in 0..12 {
            write(root, &format!("s{i:02}/server/app.py"), "");
            write(root, &format!("spec/s{i:02}_spec.rb"), "");
        }
        assert_eq!(detect_all(root).pairs.len(), MAX_PAIRS);
    }
}
