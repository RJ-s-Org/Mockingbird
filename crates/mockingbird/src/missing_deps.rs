//! Missing dependencies in a failed command's output (`verify --run`), and
//! the project's own install command for them.
//!
//! [`find`] reads the last lines of a command that failed for the error a
//! runtime prints when a dependency is not installed (a gem, a Node
//! module, a Python module, a Go package, a Java class, or the test
//! runner itself not on `PATH`); [`install`] picks the install command from
//! the project files of the folder the command ran in (after its `cd <dir>
//! &&`, [`command_dir`]), else of the nearest folder above it up to the
//! project root.

use std::path::{Path, PathBuf};

use crate::test_commands::shell_word;

/// The runtime a dependency belongs to, which decides the project files
/// looked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ecosystem {
    Ruby,
    Node,
    Python,
    Go,
    Java,
}

/// Every ecosystem, in the order an unattributed failure (a command not
/// found) looks for their project files in one folder.
const ALL: [Ecosystem; 5] = [
    Ecosystem::Ruby,
    Ecosystem::Node,
    Ecosystem::Python,
    Ecosystem::Go,
    Ecosystem::Java,
];

/// A missing dependency the output names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    /// `gem`, `library`, `module`, `package`, `class` or `command`.
    pub what: &'static str,
    /// Its name, when the line gives one.
    pub name: Option<String>,
    /// The error that showed it (`Bundler::GemNotFound`, `Cannot find
    /// module`, ...).
    pub marker: &'static str,
    /// Its runtime; `None` for a command no runtime claims.
    pub ecosystem: Option<Ecosystem>,
    /// A missing command that is a toolchain (`node`, `bundle`, `mvn`, ...),
    /// which the project's install command does not provide.
    pub toolchain: bool,
}

impl Missing {
    /// `gem `rest-client` (Bundler::GemNotFound)`.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.name {
            Some(name) => format!("missing {} `{name}` ({})", self.what, self.marker),
            None => format!("a missing {} ({})", self.what, self.marker),
        }
    }
}

/// Lines of output examined at most (a tail is already short).
const MAX_LINES: usize = 200;

/// The first missing dependency `tail` names: the runtimes' own errors
/// first, then a command not found.
#[must_use]
pub fn find(tail: &str) -> Option<Missing> {
    let lines: Vec<&str> = tail.lines().take(MAX_LINES).collect();
    lines
        .iter()
        .find_map(|l| runtime_error(l))
        .or_else(|| lines.iter().find_map(|l| command_not_found(l)))
}

/// The text after `marker` in `line`.
fn after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    line.find(marker).map(|i| &line[i + marker.len()..])
}

/// The name at the start of `rest`: what a leading quote encloses, else
/// its first word (without trailing punctuation).
fn name_at(rest: &str) -> Option<String> {
    let rest = rest.trim_start();
    let mut chars = rest.chars();
    let first = chars.next()?;
    let name = if let Some(close) = match first {
        '\'' | '"' | '`' => Some(first),
        '‘' => Some('’'),
        _ => None,
    } {
        let inner = &rest[first.len_utf8()..];
        &inner[..inner.find(close)?]
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == ';' || c == ',' || c == '(')
            .unwrap_or(rest.len());
        rest[..end].trim_end_matches(['.', ':', ')'])
    };
    let name = name.trim();
    (!name.is_empty() && name.len() <= 200).then(|| name.to_owned())
}

/// Whether `name` is a path to a file of the project (`./lib/x`, `/app/x`,
/// `C:\x`), not a dependency.
fn is_path(name: &str) -> bool {
    name.starts_with(['.', '/', '\\', '~']) || name.get(1..3) == Some(":\\")
}

fn missing(
    what: &'static str,
    name: Option<String>,
    marker: &'static str,
    ecosystem: Ecosystem,
) -> Missing {
    Missing {
        what,
        name,
        marker,
        ecosystem: Some(ecosystem),
        toolchain: false,
    }
}

/// A runtime's missing-dependency error on `line`.
fn runtime_error(line: &str) -> Option<Missing> {
    use Ecosystem::{Go, Java, Node, Python, Ruby};
    // Ruby: Bundler, then require.
    if let Some(rest) = after(line, "Could not find gem ") {
        let marker = if line.contains("Bundler::GemNotFound") {
            "Bundler::GemNotFound"
        } else {
            "Could not find gem"
        };
        return Some(missing("gem", name_at(rest), marker, Ruby));
    }
    if let Some(rest) = after(line, "Could not find ")
        && (line.contains("in locally installed gems") || line.contains("in any of the sources"))
    {
        return Some(missing("gem", name_at(rest), "Bundler::GemNotFound", Ruby));
    }
    if line.contains("Bundler::GemNotFound") {
        return Some(missing("gem", None, "Bundler::GemNotFound", Ruby));
    }
    if let Some(rest) = after(line, "cannot load such file -- ") {
        let name = rest.split(" (").next().unwrap_or(rest).trim();
        if !name.is_empty() && !is_path(name) {
            return Some(missing("library", Some(name.to_owned()), "LoadError", Ruby));
        }
        return None;
    }
    // Node: CommonJS, then ES modules.
    for (marker, label) in [
        ("Cannot find module ", "Cannot find module"),
        ("Cannot find package ", "ERR_MODULE_NOT_FOUND"),
    ] {
        if let Some(rest) = after(line, marker) {
            let name = name_at(rest)?;
            if is_path(&name) {
                return None;
            }
            return Some(missing("module", Some(node_package(&name)), label, Node));
        }
    }
    // Python.
    if let Some(rest) = after(line, "No module named ") {
        let name = name_at(rest)?;
        let top = name.split('.').next().unwrap_or(&name).to_owned();
        let marker = if line.contains("ModuleNotFoundError") {
            "ModuleNotFoundError"
        } else {
            "No module named"
        };
        return Some(missing("module", Some(top), marker, Python));
    }
    // Go.
    for (marker, label) in [
        ("cannot find package ", "cannot find package"),
        (
            "no required module provides package ",
            "no required module provides package",
        ),
        (
            "missing go.sum entry for module providing package ",
            "missing go.sum entry",
        ),
    ] {
        if let Some(rest) = after(line, marker) {
            return Some(missing("package", name_at(rest), label, Go));
        }
    }
    // Java.
    for marker in ["ClassNotFoundException", "NoClassDefFoundError"] {
        if let Some(rest) = after(line, marker) {
            let name = rest.strip_prefix(':').and_then(name_at);
            return Some(missing("class", name, marker, Java));
        }
    }
    None
}

/// The package a Node module specifier belongs to: `@scope/name` or `name`.
fn node_package(spec: &str) -> String {
    let mut parts = spec.split('/');
    match (parts.next(), parts.next()) {
        (Some(scope), Some(name)) if scope.starts_with('@') => format!("{scope}/{name}"),
        (Some(name), _) => name.to_owned(),
        _ => spec.to_owned(),
    }
}

/// A shell's "command not found" on `line`: `sh: rspec: command not found`
/// (bash), `zsh: command not found: rspec`, `sh: 1: rspec: not found`
/// (dash), `'rspec' is not recognized as an internal or external command`
/// (cmd.exe).
fn command_not_found(line: &str) -> Option<Missing> {
    let line = line.trim();
    let name = if let Some(rest) = after(line, "command not found: ") {
        name_at(rest)
    } else if let Some(head) = line.strip_suffix(": command not found") {
        head.rsplit(": ").next().map(str::trim).map(str::to_owned)
    } else if let Some(head) = line.strip_suffix(": not found") {
        // dash: `sh: 1: rspec: not found`; only a shell's own line.
        let parts: Vec<&str> = head.split(": ").collect();
        (parts.len() >= 3 && parts[0].ends_with("sh"))
            .then(|| parts[parts.len() - 1].trim().to_owned())
    } else if let Some(i) = line.find(" is not recognized as an internal or external command") {
        name_at(&line[..i])
    } else {
        None
    }?;
    // `./node_modules/.bin/jest` is `jest`.
    let name = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&name)
        .trim_matches(['\'', '"'])
        .to_owned();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let (ecosystem, toolchain) = command_ecosystem(&name);
    Some(Missing {
        what: "command",
        name: Some(name),
        marker: "command not found",
        ecosystem,
        toolchain,
    })
}

/// The runtime a command belongs to, and whether it is a toolchain (which
/// installing the project's dependencies does not provide).
fn command_ecosystem(name: &str) -> (Option<Ecosystem>, bool) {
    use Ecosystem::{Go, Java, Node, Python, Ruby};
    const TOOLCHAINS: &[(&str, Ecosystem)] = &[
        ("ruby", Ruby),
        ("bundle", Ruby),
        ("bundler", Ruby),
        ("gem", Ruby),
        ("node", Node),
        ("npm", Node),
        ("npx", Node),
        ("pnpm", Node),
        ("yarn", Node),
        ("python", Python),
        ("python3", Python),
        ("pip", Python),
        ("pip3", Python),
        ("poetry", Python),
        ("uv", Python),
        ("go", Go),
        ("java", Java),
        ("mvn", Java),
        ("gradle", Java),
    ];
    const TOOLS: &[(&str, Ecosystem)] = &[
        ("rspec", Ruby),
        ("rake", Ruby),
        ("rails", Ruby),
        ("cucumber", Ruby),
        ("jest", Node),
        ("mocha", Node),
        ("vitest", Node),
        ("playwright", Node),
        ("cypress", Node),
        ("tsc", Node),
        ("ts-node", Node),
        ("tsx", Node),
        ("nodemon", Node),
        ("next", Node),
        ("vite", Node),
        ("ava", Node),
        ("pytest", Python),
        ("py.test", Python),
        ("tox", Python),
        ("nox", Python),
        ("behave", Python),
        ("flask", Python),
        ("uvicorn", Python),
        ("gunicorn", Python),
    ];
    if let Some((_, e)) = TOOLCHAINS.iter().find(|(n, _)| *n == name) {
        return (Some(*e), true);
    }
    (
        TOOLS.iter().find(|(n, _)| *n == name).map(|(_, e)| *e),
        false,
    )
}

/// The folder `command` works in when run from `cwd`: `cwd`, moved by a
/// leading `cd <dir> &&` (or `;`), as many as it chains.
#[must_use]
pub fn command_dir(cwd: &Path, command: &str) -> PathBuf {
    let mut dir = cwd.to_path_buf();
    let mut rest = command.trim();
    while let Some(after_cd) = rest.strip_prefix("cd ") {
        let end = after_cd
            .find("&&")
            .or_else(|| after_cd.find(';'))
            .unwrap_or(after_cd.len());
        let word = after_cd[..end].trim();
        let word = word
            .strip_prefix('\'')
            .and_then(|w| w.strip_suffix('\''))
            .or_else(|| word.strip_prefix('"').and_then(|w| w.strip_suffix('"')))
            .unwrap_or(word);
        // A variable, `~` or `-` is the shell's to resolve.
        if word.is_empty() || word.starts_with(['~', '$', '-']) || word.contains('$') {
            break;
        }
        dir = dir.join(word);
        rest = after_cd[end..].trim_start_matches(['&', ';']).trim_start();
    }
    dir
}

/// The project's install command, and the folder it runs in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Install {
    pub dir: PathBuf,
    /// `bundle install`, `npm ci`, ...
    pub command: String,
}

impl Install {
    /// `cd <dir> && <command>`, ready to paste.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "cd {} && {}",
            shell_word(&self.dir.display().to_string()),
            self.command
        )
    }
}

/// The install command for `ecosystem` (any, Ruby, Node, Python, Go then
/// Java, when `None`) from the project files of `dir`, else of the nearest folder above
/// it up to `root` (only `dir` itself when it lies outside `root`).
#[must_use]
pub fn install(dir: &Path, root: &Path, ecosystem: Option<Ecosystem>) -> Option<Install> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let wanted: &[Ecosystem] = match &ecosystem {
        Some(e) => std::slice::from_ref(e),
        None => &ALL,
    };
    for folder in dir.ancestors() {
        if folder != dir && !folder.starts_with(&root) {
            break;
        }
        if let Some(command) = wanted.iter().find_map(|e| install_in(folder, *e)) {
            return Some(Install {
                dir: folder.to_path_buf(),
                command,
            });
        }
        if folder == root {
            break;
        }
    }
    None
}

/// Requirements files listed in one `pip install` at most.
const MAX_REQUIREMENTS: usize = 4;

/// The install command `ecosystem`'s project files in `dir` call for.
fn install_in(dir: &Path, ecosystem: Ecosystem) -> Option<String> {
    let has = |name: &str| dir.join(name).is_file();
    let command = match ecosystem {
        Ecosystem::Ruby => has("Gemfile").then_some("bundle install")?.to_owned(),
        Ecosystem::Node => if has("pnpm-lock.yaml") {
            "pnpm install --frozen-lockfile"
        } else if has("yarn.lock") {
            "yarn install --frozen-lockfile"
        } else if has("package-lock.json") || has("npm-shrinkwrap.json") {
            "npm ci"
        } else if has("package.json") {
            "npm install"
        } else {
            return None;
        }
        .to_owned(),
        Ecosystem::Python => {
            let requirements = requirements(dir);
            if !requirements.is_empty() {
                let files: Vec<String> = requirements
                    .iter()
                    .map(|f| format!("-r {}", shell_word(f)))
                    .collect();
                format!("pip install {}", files.join(" "))
            } else if has("pyproject.toml") && has("uv.lock") {
                "uv sync".to_owned()
            } else if has("pyproject.toml") && has("poetry.lock") {
                "poetry install".to_owned()
            } else if has("pyproject.toml") || has("setup.py") {
                "pip install -e .".to_owned()
            } else {
                return None;
            }
        }
        Ecosystem::Go => has("go.mod").then_some("go mod download")?.to_owned(),
        Ecosystem::Java => {
            if has("pom.xml") {
                let mvn = if has("mvnw") { "./mvnw" } else { "mvn" };
                format!("{mvn} -q -DskipTests package")
            } else if has("build.gradle") || has("build.gradle.kts") {
                let gradle = if has("gradlew") {
                    "./gradlew"
                } else {
                    "gradle"
                };
                format!("{gradle} build -x test")
            } else {
                return None;
            }
        }
    };
    Some(command)
}

/// The `requirements*.txt` files of `dir`, sorted, at most
/// [`MAX_REQUIREMENTS`].
fn requirements(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| n.starts_with("requirements") && n.ends_with(".txt"))
        .collect();
    found.sort();
    found.truncate(MAX_REQUIREMENTS);
    found
}

/// The next step for `missing`, found in the output of `who` (`The tests`,
/// `The app's server`): the install command when one was found, and, when
/// `registries_blocked` (`proxy.unmapped: block`), to run it outside the
/// proxy.
#[must_use]
pub fn step(
    who: &str,
    missing: &Missing,
    install: Option<&Install>,
    registries_blocked: bool,
) -> String {
    let what = missing.describe();
    let install_part = match install {
        Some(i) => format!("install the project's dependencies (`{}`)", i.line()),
        None => "install the project's dependencies with its own install step (no Gemfile, package.json, requirements.txt, pyproject.toml, go.mod, pom.xml or build.gradle was found from the folder it ran in up to the project root)".to_owned(),
    };
    let mut s = match (&missing.name, missing.toolchain) {
        (Some(name), true) => format!(
            "{who} could not start: {what}. `{name}` is not installed on this machine (or not on PATH): install it, then {install_part}, then run verify --run again."
        ),
        _ => {
            let mut first = install_part;
            if let Some(c) = first.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            format!("{who} could not start: {what}. {first}, then run verify --run again.")
        }
    };
    if registries_blocked {
        s.push_str(" Run the install in your own shell, outside the proxy (not under `mockingbird run`, without the `mockingbird proxy env` variables): this config's proxy.unmapped: block would block the package registry.");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    fn named(tail: &str) -> (String, &'static str, Option<Ecosystem>) {
        let m = find(tail).unwrap_or_else(|| panic!("nothing found in {tail:?}"));
        (m.name.unwrap_or_default(), m.marker, m.ecosystem)
    }

    /// The install line for the first missing dependency of `tail`, for
    /// `command` run from `root`.
    fn hint(root: &Path, command: &str, tail: &str) -> Option<String> {
        let m = find(tail)?;
        install(&command_dir(root, command), root, m.ecosystem).map(|i| i.command)
    }

    #[test]
    fn ruby_errors_name_the_gem_and_call_for_bundle_install() {
        assert_eq!(
            named(
                "Could not find gem 'rest-client' in locally installed gems. (Bundler::GemNotFound)"
            ),
            (
                "rest-client".into(),
                "Bundler::GemNotFound",
                Some(Ecosystem::Ruby)
            )
        );
        assert_eq!(
            named("Could not find rack-3.0.8 in locally installed gems"),
            (
                "rack-3.0.8".into(),
                "Bundler::GemNotFound",
                Some(Ecosystem::Ruby)
            )
        );
        assert_eq!(
            named("/x/app.rb:1:in `require': cannot load such file -- faraday (LoadError)"),
            ("faraday".into(), "LoadError", Some(Ecosystem::Ruby))
        );
        // The project's own file is not a dependency.
        assert_eq!(
            find("cannot load such file -- /app/lib/thing (LoadError)"),
            None
        );
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Gemfile");
        assert_eq!(
            hint(dir.path(), "bundle exec rspec", "Bundler::GemNotFound").as_deref(),
            Some("bundle install")
        );
    }

    #[test]
    fn node_errors_name_the_package_and_follow_the_lockfile() {
        assert_eq!(
            named("Error: Cannot find module 'express'"),
            (
                "express".into(),
                "Cannot find module",
                Some(Ecosystem::Node)
            )
        );
        assert_eq!(
            named("Error: Cannot find module '@scope/pkg/lib/x.js'"),
            (
                "@scope/pkg".into(),
                "Cannot find module",
                Some(Ecosystem::Node)
            )
        );
        assert_eq!(
            named(
                "Error [ERR_MODULE_NOT_FOUND]: Cannot find package 'zod' imported from /app/x.mjs"
            ),
            ("zod".into(), "ERR_MODULE_NOT_FOUND", Some(Ecosystem::Node))
        );
        assert_eq!(find("Error: Cannot find module './routes'"), None);
        assert_eq!(find("Error: Cannot find module '/app/server.js'"), None);
        let dir = tempfile::tempdir().unwrap();
        let tail = "Error: Cannot find module 'express'";
        write(dir.path(), "package.json");
        assert_eq!(
            hint(dir.path(), "npm test", tail).as_deref(),
            Some("npm install")
        );
        write(dir.path(), "package-lock.json");
        assert_eq!(
            hint(dir.path(), "npm test", tail).as_deref(),
            Some("npm ci")
        );
        write(dir.path(), "yarn.lock");
        assert_eq!(
            hint(dir.path(), "npm test", tail).as_deref(),
            Some("yarn install --frozen-lockfile")
        );
        write(dir.path(), "pnpm-lock.yaml");
        assert_eq!(
            hint(dir.path(), "npm test", tail).as_deref(),
            Some("pnpm install --frozen-lockfile")
        );
    }

    #[test]
    fn python_errors_name_the_top_module_and_pick_requirements_or_the_lock() {
        assert_eq!(
            named("ModuleNotFoundError: No module named 'requests.adapters'"),
            (
                "requests".into(),
                "ModuleNotFoundError",
                Some(Ecosystem::Python)
            )
        );
        assert_eq!(
            named("/usr/bin/python3: No module named pytest"),
            ("pytest".into(), "No module named", Some(Ecosystem::Python))
        );
        let dir = tempfile::tempdir().unwrap();
        let tail = "ModuleNotFoundError: No module named 'requests'";
        write(dir.path(), "pyproject.toml");
        assert_eq!(
            hint(dir.path(), "pytest", tail).as_deref(),
            Some("pip install -e .")
        );
        write(dir.path(), "poetry.lock");
        assert_eq!(
            hint(dir.path(), "pytest", tail).as_deref(),
            Some("poetry install")
        );
        write(dir.path(), "uv.lock");
        assert_eq!(hint(dir.path(), "pytest", tail).as_deref(), Some("uv sync"));
        write(dir.path(), "requirements.txt");
        write(dir.path(), "requirements-dev.txt");
        assert_eq!(
            hint(dir.path(), "pytest", tail).as_deref(),
            Some("pip install -r requirements-dev.txt -r requirements.txt")
        );
    }

    #[test]
    fn go_and_java_errors_call_for_their_build_tools() {
        assert_eq!(
            named("main.go:4:2: no required module provides package github.com/x/y; to add it:"),
            (
                "github.com/x/y".into(),
                "no required module provides package",
                Some(Ecosystem::Go)
            )
        );
        assert_eq!(
            named(r#"app.go:3:8: cannot find package "github.com/x/z" in any of:"#).0,
            "github.com/x/z"
        );
        assert_eq!(
            named("Exception in thread \"main\" java.lang.ClassNotFoundException: org.x.Client"),
            (
                "org.x.Client".into(),
                "ClassNotFoundException",
                Some(Ecosystem::Java)
            )
        );
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "go.mod");
        assert_eq!(
            hint(dir.path(), "go test ./...", "cannot find package \"a/b\"").as_deref(),
            Some("go mod download")
        );
        let java = "java.lang.ClassNotFoundException: org.x.Client";
        write(dir.path(), "build.gradle.kts");
        assert_eq!(
            hint(dir.path(), "x", java).as_deref(),
            Some("gradle build -x test")
        );
        write(dir.path(), "gradlew");
        assert_eq!(
            hint(dir.path(), "x", java).as_deref(),
            Some("./gradlew build -x test")
        );
        write(dir.path(), "pom.xml");
        assert_eq!(
            hint(dir.path(), "x", java).as_deref(),
            Some("mvn -q -DskipTests package")
        );
    }

    #[test]
    fn a_runner_not_found_is_a_missing_command_of_its_runtime() {
        for (line, name) in [
            ("sh: rspec: command not found", "rspec"),
            ("sh: line 1: jest: command not found", "jest"),
            ("zsh: command not found: pytest", "pytest"),
            ("sh: 1: ./node_modules/.bin/vitest: not found", "vitest"),
            (
                "'mocha' is not recognized as an internal or external command,",
                "mocha",
            ),
        ] {
            let m = find(line).unwrap_or_else(|| panic!("{line}"));
            assert_eq!(m.name.as_deref(), Some(name), "{line}");
            assert_eq!(m.what, "command");
            assert!(!m.toolchain);
        }
        assert_eq!(
            find("sh: rspec: command not found").unwrap().ecosystem,
            Some(Ecosystem::Ruby)
        );
        let mvn = find("sh: mvn: command not found").unwrap();
        assert!(mvn.toolchain);
        assert_eq!(mvn.ecosystem, Some(Ecosystem::Java));
        // A line that only mentions "not found" is not a shell's.
        assert_eq!(find("GET /users/7: not found"), None);
        // A runtime's own error wins over a later command not found.
        assert_eq!(
            named("sh: foo: command not found\nModuleNotFoundError: No module named 'x'").0,
            "x"
        );
    }

    #[test]
    fn the_install_runs_where_the_command_cds_else_in_the_nearest_folder_above() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "Gemfile");
        write(root, "web/package-lock.json");
        write(root, "web/package.json");
        write(root, "api/requirements.txt");
        std::fs::create_dir_all(root.join("api/tests")).unwrap();
        let node = find("Error: Cannot find module 'express'").unwrap();
        let found = install(
            &command_dir(root, "cd web && npm test"),
            root,
            node.ecosystem,
        )
        .unwrap();
        assert_eq!(found.command, "npm ci");
        assert!(found.dir.ends_with("web"), "{found:?}");
        assert!(found.line().ends_with("web && npm ci"), "{}", found.line());
        // A quoted folder, chained twice; the nearest requirements are above.
        let py = find("No module named 'requests'").unwrap();
        let found = install(
            &command_dir(root, "cd 'api' && cd tests && pytest"),
            root,
            py.ecosystem,
        )
        .unwrap();
        assert_eq!(found.command, "pip install -r requirements.txt");
        assert!(found.dir.ends_with("api"), "{found:?}");
        // Only the runtime's own files count: the root's Gemfile is no
        // install for a Node module.
        assert_eq!(install(&root.join("api"), root, node.ecosystem), None);
        // A command not found of no known runtime takes any project file.
        let any = find("sh: frobnicate: command not found").unwrap();
        assert_eq!(any.ecosystem, None);
        assert_eq!(
            install(root, root, any.ecosystem)
                .map(|i| i.command)
                .as_deref(),
            Some("bundle install")
        );
        // Not above the project root.
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        assert_eq!(install(&inner, &inner, Some(Ecosystem::Ruby)), None);
        // A variable folder is the shell's: the command's own folder is kept.
        assert_eq!(command_dir(root, "cd $APP && make"), root.to_path_buf());
    }

    #[test]
    fn clean_failures_name_no_dependency() {
        assert_eq!(
            find("FAIL spec/orders_spec.rb\nExpected 200, got 500\n3 examples, 1 failure"),
            None
        );
        assert_eq!(find(""), None);
    }

    #[test]
    fn the_step_names_the_dependency_the_install_and_the_proxy_when_registries_are_blocked() {
        let m = find(
            "Could not find gem 'rest-client' in locally installed gems. (Bundler::GemNotFound)",
        )
        .unwrap();
        let install = Install {
            dir: PathBuf::from("/abs/dir"),
            command: "bundle install".into(),
        };
        assert_eq!(
            step("The tests", &m, Some(&install), false),
            "The tests could not start: missing gem `rest-client` (Bundler::GemNotFound). Install the project's dependencies (`cd /abs/dir && bundle install`), then run verify --run again."
        );
        assert!(step("The tests", &m, Some(&install), true).contains("outside the proxy"));
        assert!(step("The tests", &m, None, false).contains("no Gemfile"));
        let mvn = find("sh: mvn: command not found").unwrap();
        assert!(
            step("The tests", &mvn, None, false).contains("`mvn` is not installed on this machine"),
        );
    }
}
