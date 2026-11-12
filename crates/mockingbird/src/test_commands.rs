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
//!   only fails) and end-to-end scripts (`test:e2e`, `e2e`, ...: `npm run
//!   <name>`), with `pnpm` or `yarn` when their lock file is there;
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
//! A command found in a subfolder runs from the root as `cd <dir> && …`.

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

/// One test command of the app.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct TestCommand {
    /// The command, to run from the project root.
    pub command: String,
    /// The folder it belongs to, relative to the root (`.` for the root).
    pub dir: String,
    /// What it was found from (`package.json scripts.test`, `spec/`, ...).
    pub source: String,
}

impl TestCommand {
    /// `` `cmd` (source) ``.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("`{}` ({})", self.command, self.source)
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
    for (dir, rel) in &dirs {
        if out.len() >= MAX_COMMANDS {
            break;
        }
        for (command, source) in in_dir(dir, &mut budget) {
            if out.len() >= MAX_COMMANDS {
                break;
            }
            let command = if rel == "." {
                command
            } else {
                format!("cd {} && {command}", shell_word(rel))
            };
            let source = if rel == "." {
                source
            } else {
                format!("{rel}/{source}")
            };
            out.push(TestCommand {
                command,
                dir: rel.clone(),
                source,
            });
        }
    }
    out
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

/// The test commands of one folder: `(command, source)`.
fn in_dir(dir: &Path, budget: &mut usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    out.extend(node(dir));
    out.extend(make(dir));
    if let Some(source) = pytest(dir) {
        out.push(("pytest".into(), source));
    }
    if let Some(found) = rspec(dir) {
        out.push(found);
    }
    if dir.join("go.mod").is_file() && has_go_test(dir, budget) {
        out.push(("go test ./...".into(), "go.mod, *_test.go".into()));
    }
    if dir.join("Cargo.toml").is_file() {
        out.push(("cargo test".into(), "Cargo.toml".into()));
    }
    out
}

/// npm's placeholder `test` script, which only fails.
fn placeholder(script: &str) -> bool {
    script.contains("no test specified")
}

/// Whether a script name runs end-to-end or other test suites:
/// `test:e2e`, `e2e`, `test-integration`, `cypress:run`, ...
fn test_like(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower != "test"
        && (lower.starts_with("test:")
            || lower.starts_with("test-")
            || lower.starts_with("test_")
            || lower.split([':', '-', '_']).any(|part| part == "e2e"))
        && !lower.contains("watch")
        && !lower.contains("update")
        && !lower.contains("debug")
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
        targets.into_iter().find(|t| {
            t.starts_with("test-") || t.starts_with("test_") || *t == "e2e" || *t == "check"
        })?
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

/// Characters of the [`step`] line, at most (about).
const STEP_CHARS: usize = 300;

/// The one-line `next_steps` entry naming `commands` and how to run them
/// against the mocks; `None` when there are none.
#[must_use]
pub fn step(commands: &[TestCommand]) -> Option<String> {
    if commands.is_empty() {
        return None;
    }
    let tail = " Run each with verify_run {command} (CLI `mockingbird verify --run \"<cmd>\"`) before handing back.";
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

    fn commands(root: &Path) -> Vec<String> {
        detect(root).into_iter().map(|c| c.command).collect()
    }

    #[test]
    fn package_json_test_and_e2e_scripts() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts": {"test": "jest", "test:e2e": "playwright test", "test:watch": "jest --watch", "build": "tsc", "e2e": "cypress run"}}"#,
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
        assert_eq!(cmds, ["cd api && pytest", "cd web && npm test"]);
        assert_eq!(found[1].source, "web/package.json scripts.test");
        assert_eq!(found[1].dir, "web");
        for i in 0..12 {
            write(dir.path(), &format!("p{i:02}/Cargo.toml"), "");
        }
        assert_eq!(detect(dir.path()).len(), MAX_COMMANDS);
    }

    #[test]
    fn the_step_line_names_the_commands_briefly() {
        assert_eq!(step(&[]), None);
        let c = |command: &str, source: &str| TestCommand {
            command: command.into(),
            dir: ".".into(),
            source: source.into(),
        };
        let line = step(&[
            c("npm test", "package.json scripts.test"),
            c("bundle exec rspec", "spec/"),
        ])
        .unwrap();
        assert_eq!(
            line,
            "The app's tests: `npm test` (package.json scripts.test), `bundle exec rspec` (spec/). Run each with verify_run {command} (CLI `mockingbird verify --run \"<cmd>\"`) before handing back."
        );
        let many: Vec<TestCommand> = (0..8)
            .map(|i| {
                c(
                    &format!("cd sub{i} && npm run test:e2e"),
                    "sub/package.json scripts.test:e2e",
                )
            })
            .collect();
        let line = step(&many).unwrap();
        assert!(line.len() <= STEP_CHARS + 20, "{} {line}", line.len());
        assert!(line.contains(" more."), "{line}");
    }
}
