//! Missing dependencies in a failed command's output (`verify --run`), and
//! the project's own install command for them.
//!
//! [`find`] reads the last lines of a command that failed for the error a
//! runtime prints when a dependency is not installed (a gem, a Node
//! module, a Python module, a Go package, a Java class, or a command not
//! on `PATH`), where the runtime prints it: at the start of a line (after
//! `E `, `Error: ` or `Error [ERR_…]: `), or ending in the exception's
//! class for Ruby, after a file position for Go, and after `Exception in
//! thread` or `Caused by:` for Java. The last such line wins, and a name
//! that is a module of the project itself is not a missing dependency.
//!
//! [`install`] picks the install command from the project files of the
//! folder the command ran in (after its `cd <dir> &&`, [`command_dir`]),
//! else of the nearest folder above it up to the project root; a workspace
//! member without a lockfile installs from the workspace above it, up to
//! the git root.

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

impl Ecosystem {
    /// Its project files, for a step that found none.
    fn manifests(self) -> &'static str {
        match self {
            Ecosystem::Ruby => "Gemfile",
            Ecosystem::Node => "package.json",
            Ecosystem::Python => "requirements.txt, pyproject.toml, setup.py or Pipfile",
            Ecosystem::Go => "go.mod",
            Ecosystem::Java => "pom.xml or build.gradle",
        }
    }
}

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
    /// ``a missing gem `rest-client` (Bundler::GemNotFound)``.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.name {
            Some(name) => format!("a missing {} `{name}` ({})", self.what, self.marker),
            None => format!("a missing {} ({})", self.what, self.marker),
        }
    }

    /// A command no runtime claims: installing the project's dependencies
    /// does not provide it.
    #[must_use]
    pub fn unknown_command(&self) -> bool {
        self.what == "command" && self.ecosystem.is_none()
    }
}

/// The last missing dependency `tail` names: a runtime's own error, else a
/// command not found. A module found in one of `dirs` (the command's
/// folder, the project root) is the project's own, not a dependency.
#[must_use]
pub fn find(tail: &str, dirs: &[&Path]) -> Option<Missing> {
    let lines: Vec<&str> = tail.lines().collect();
    for (i, line) in lines.iter().enumerate().rev() {
        let next = lines.get(i + 1).copied().unwrap_or_default();
        if let Some((missing, spec)) = runtime_error(line, next)
            && !missing.ecosystem.is_some_and(|e| is_local(e, &spec, dirs))
        {
            return Some(missing);
        }
    }
    lines.iter().rev().find_map(|l| command_not_found(l))
}

/// The text after `marker` in `line`.
fn after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    line.find(marker).map(|i| &line[i + marker.len()..])
}

/// The name at the start of `rest`: what a leading quote encloses, else
/// its first word (without trailing punctuation).
fn name_at(rest: &str) -> Option<String> {
    let rest = rest.trim_start();
    let first = rest.chars().next()?;
    let close = match first {
        '\'' | '"' | '`' => Some(first),
        '‘' => Some('’'),
        _ => None,
    };
    let name = if let Some(close) = close {
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

/// `line` without leading blanks and the prefixes runtimes and runners put
/// before an error: `E ` (pytest), `Error: `, `Error [ERR_…]: ` (Node).
fn message(line: &str) -> &str {
    let mut m = line.trim_start();
    if let Some(r) = m.strip_prefix("E ") {
        m = r.trim_start();
    }
    if let Some(r) = m.strip_prefix("Error: ") {
        m = r.trim_start();
    } else if let Some(r) = m.strip_prefix("Error [ERR_")
        && let Some(i) = r.find("]: ")
    {
        m = r[i + 3..].trim_start();
    }
    m
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

/// A runtime's missing-dependency error on `line` (`next` is the line after
/// it), and the specifier as written (to tell the project's own modules).
fn runtime_error(line: &str, next: &str) -> Option<(Missing, String)> {
    let m = message(line);
    ruby(line, m)
        .or_else(|| node(m))
        .or_else(|| python(m))
        .or_else(|| go(m))
        .or_else(|| java(m, next))
}

fn with_spec(m: Missing) -> (Missing, String) {
    let spec = m.name.clone().unwrap_or_default();
    (m, spec)
}

/// Bundler at the start of the line or raised (`… (Bundler::GemNotFound)`),
/// and a raised `LoadError`.
fn ruby(line: &str, m: &str) -> Option<(Missing, String)> {
    let raised = line.trim_end().ends_with("(Bundler::GemNotFound)");
    let marker = if raised {
        "Bundler::GemNotFound"
    } else {
        "Could not find gem"
    };
    if let Some(rest) = m.strip_prefix("Could not find gem ") {
        return Some(with_spec(missing(
            "gem",
            name_at(rest),
            marker,
            Ecosystem::Ruby,
        )));
    }
    let gem_sources =
        |l: &str| l.contains("in locally installed gems") || l.contains("in any of the sources");
    if let Some(rest) = m.strip_prefix("Could not find ")
        && gem_sources(m)
    {
        return Some(with_spec(missing(
            "gem",
            name_at(rest),
            "Bundler::GemNotFound",
            Ecosystem::Ruby,
        )));
    }
    if raised {
        let name = after(line, "Could not find gem ")
            .or_else(|| after(line, "Could not find ").filter(|_| gem_sources(line)))
            .and_then(name_at);
        return Some(with_spec(missing(
            "gem",
            name,
            "Bundler::GemNotFound",
            Ecosystem::Ruby,
        )));
    }
    let rest = m.strip_prefix("cannot load such file -- ").or_else(|| {
        line.trim_end()
            .ends_with("(LoadError)")
            .then(|| after(line, ": cannot load such file -- "))
            .flatten()
    })?;
    let name = rest.split(" (").next().unwrap_or(rest).trim();
    (!name.is_empty() && !is_path(name)).then(|| {
        with_spec(missing(
            "library",
            Some(name.to_owned()),
            "LoadError",
            Ecosystem::Ruby,
        ))
    })
}

/// `Cannot find module 'x'` (CommonJS, Jest) and `Cannot find package 'x'`
/// (ES modules).
fn node(m: &str) -> Option<(Missing, String)> {
    let (rest, marker) = m
        .strip_prefix("Cannot find module ")
        .map(|r| (r, "Cannot find module"))
        .or_else(|| {
            m.strip_prefix("Cannot find package ")
                .map(|r| (r, "ERR_MODULE_NOT_FOUND"))
        })?;
    let spec = name_at(rest)?;
    if is_path(&spec) {
        return None;
    }
    let found = missing("module", Some(node_package(&spec)), marker, Ecosystem::Node);
    Some((found, spec))
}

/// `ModuleNotFoundError: No module named 'x'`, `ImportError: …`, and
/// `/usr/bin/python3: No module named pytest` (`python -m`).
fn python(m: &str) -> Option<(Missing, String)> {
    let (rest, marker) = if let Some(r) = m.strip_prefix("ModuleNotFoundError: ") {
        (r, "ModuleNotFoundError")
    } else if let Some(r) = m.strip_prefix("ImportError: ") {
        (r, "ImportError")
    } else if let Some((head, r)) = m.split_once(": ")
        && Path::new(head)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("python"))
    {
        (r, "No module named")
    } else {
        (m, "No module named")
    };
    let name = name_at(rest.strip_prefix("No module named ")?)?;
    let top = name.split('.').next().unwrap_or(&name).to_owned();
    Some((
        missing("module", Some(top.clone()), marker, Ecosystem::Python),
        name,
    ))
}

/// Go's errors, after `go: ` or a file position (`main.go:4:2: `).
fn go(m: &str) -> Option<(Missing, String)> {
    let go_prefixed = m.starts_with("go: ");
    let msg = if let Some(r) = m.strip_prefix("go: ") {
        r
    } else if let Some(i) = m.find(".go:")
        && let Some(j) = m[i..].find(": ")
        && j > 4
        && m[i + 4..i + j]
            .split(':')
            .all(|p| p.chars().all(|c| c.is_ascii_digit()))
    {
        &m[i + j + 2..]
    } else {
        m
    };
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
        if let Some(rest) = msg.strip_prefix(marker) {
            return Some(with_spec(missing(
                "package",
                name_at(rest),
                label,
                Ecosystem::Go,
            )));
        }
    }
    (go_prefixed && msg.contains("missing go.sum entry")).then(|| {
        with_spec(missing(
            "package",
            None,
            "missing go.sum entry",
            Ecosystem::Go,
        ))
    })
}

/// An uncaught `ClassNotFoundException` or `NoClassDefFoundError`: after
/// `Exception in thread "…" `, after `Caused by: `, or at the start of a
/// line a stack frame (`at …`) follows when no logger prefix is before it.
/// A class probe a library logs is not.
fn java(m: &str, next: &str) -> Option<(Missing, String)> {
    let rest = if let Some(r) = m.strip_prefix("Exception in thread \"") {
        r.split_once("\" ")?.1
    } else if let Some(r) = m.strip_prefix("Caused by: ") {
        r
    } else if next.trim_start().starts_with("at ") {
        m
    } else {
        return None;
    };
    for (class, marker) in [
        ("java.lang.ClassNotFoundException", "ClassNotFoundException"),
        ("java.lang.NoClassDefFoundError", "NoClassDefFoundError"),
    ] {
        if let Some(r) = rest.strip_prefix(class) {
            let name = r
                .strip_prefix(':')
                .and_then(name_at)
                .map(|n| n.replace('/', "."));
            return Some(with_spec(missing("class", name, marker, Ecosystem::Java)));
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

/// Whether `spec` names a module of the project itself in one of `dirs`:
/// a Python package or module (`x/`, `x.py`, `src/x`), a Ruby file
/// (`lib/x.rb`, `x.rb`), or a Node path alias (`@/…`, `#…`, `~/…`) or a
/// file a `baseUrl` resolves (`x.js`, `x.ts`, `src/x`, ...).
fn is_local(ecosystem: Ecosystem, spec: &str, dirs: &[&Path]) -> bool {
    if spec.is_empty() {
        return false;
    }
    let exists = |p: PathBuf| p.exists();
    match ecosystem {
        Ecosystem::Python => {
            let top = spec.split('.').next().unwrap_or(spec);
            dirs.iter().any(|d| {
                exists(d.join(top))
                    || exists(d.join(format!("{top}.py")))
                    || exists(d.join("src").join(top))
            })
        }
        Ecosystem::Ruby => dirs.iter().any(|d| {
            exists(d.join("lib").join(format!("{spec}.rb"))) || exists(d.join(format!("{spec}.rb")))
        }),
        Ecosystem::Node => {
            if spec.starts_with("@/") || spec.starts_with('#') || spec.starts_with("~/") {
                return true;
            }
            dirs.iter().any(|d| {
                [d.to_path_buf(), d.join("src")].iter().any(|base| {
                    exists(base.join(spec))
                        || ["js", "ts", "mjs", "cjs", "jsx", "tsx"]
                            .iter()
                            .any(|ext| exists(base.join(format!("{spec}.{ext}"))))
                })
            })
        }
        Ecosystem::Go | Ecosystem::Java => false,
    }
}

/// A shell's "command not found" on `line`: `sh: rspec: command not found`
/// (bash), `zsh: command not found: rspec`, `sh: 1: rspec: not found`
/// (dash), `bundler: command not found: rspec`, `'rspec' is not recognized
/// as an internal or external command` (cmd.exe).
fn command_not_found(line: &str) -> Option<Missing> {
    let line = line.trim();
    let shell_like = |head: &str| {
        Path::new(head)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with("sh") || n == "bundler")
    };
    let name = if let Some((head, rest)) = line.split_once(": command not found: ") {
        shell_like(head).then(|| name_at(rest)).flatten()
    } else if let Some((head, least)) = line
        .strip_suffix(": command not found")
        .map(|h| (h, 2))
        .or_else(|| line.strip_suffix(": not found").map(|h| (h, 3)))
    {
        // bash: `sh: [line 1: ]x`; dash: `sh: 1: x`.
        let parts: Vec<&str> = head.split(": ").collect();
        (parts.len() >= least && shell_like(parts[0]))
            .then(|| parts[parts.len() - 1].trim().to_owned())
    } else if line.starts_with('\'')
        && let Some(i) = line.find(" is not recognized as an internal or external command")
    {
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
        ("bun", Node),
        ("python", Python),
        ("python3", Python),
        ("pip", Python),
        ("pip3", Python),
        ("poetry", Python),
        ("uv", Python),
        ("pipenv", Python),
        ("pdm", Python),
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

/// The shell word at the start of `s` (quotes and backslash escapes
/// resolved), and the text after it; `None` for an unterminated quote.
fn word_at(s: &str) -> Option<(String, &str)> {
    let mut word = String::new();
    let mut chars = s.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        match c {
            '\'' => {
                chars.next();
                loop {
                    match chars.next()? {
                        (_, '\'') => break,
                        (_, c) => word.push(c),
                    }
                }
            }
            '"' => {
                chars.next();
                loop {
                    match chars.next()? {
                        (_, '"') => break,
                        (_, '\\') => word.push(chars.next()?.1),
                        (_, c) => word.push(c),
                    }
                }
            }
            '\\' => {
                chars.next();
                word.push(chars.next()?.1);
            }
            c if c.is_whitespace() || ";&|<>()".contains(c) => return Some((word, &s[i..])),
            c => {
                chars.next();
                word.push(c);
            }
        }
    }
    Some((word, ""))
}

/// The folder `command` works in when run from `cwd`: `cwd`, moved by its
/// leading `cd <dir>` steps joined with `&&` or `;` (a `|| exit` after one
/// is skipped; `cd -` goes back).
#[must_use]
pub fn command_dir(cwd: &Path, command: &str) -> PathBuf {
    let mut dir = cwd.to_path_buf();
    let mut prev = cwd.to_path_buf();
    let mut rest = command.trim_start();
    while let Some(args) = rest.strip_prefix("cd")
        && args.starts_with([' ', '\t'])
    {
        let Some((word, tail)) = word_at(args.trim_start()) else {
            break;
        };
        if word == "-" {
            std::mem::swap(&mut dir, &mut prev);
        } else if word.is_empty() || word.starts_with('~') || word.contains('$') {
            // `cd` alone, `~` and variables are the shell's to resolve.
            break;
        } else {
            let next = dir.join(&word);
            prev = std::mem::replace(&mut dir, next);
        }
        let mut tail = tail.trim_start();
        if let Some(r) = tail.strip_prefix("||") {
            let r = r.trim_start();
            if !(r.starts_with("exit") || r.starts_with("return")) {
                break;
            }
            let end = ["&&", ";", "||", "|"]
                .iter()
                .filter_map(|sep| r.find(sep))
                .min()
                .unwrap_or(r.len());
            tail = r[end..].trim_start();
        }
        if let Some(r) = tail.strip_prefix("&&").or_else(|| tail.strip_prefix(';')) {
            rest = r.trim_start();
        } else {
            break;
        }
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

/// `path` with its longest existing ancestor resolved (symlinks such as
/// macOS's `/var` → `/private/var`), so two paths compare alike.
fn canonical(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(resolved) = ancestor.canonicalize() {
            return match path.strip_prefix(ancestor) {
                Ok(rel) if !rel.as_os_str().is_empty() => resolved.join(rel),
                _ => resolved,
            };
        }
    }
    path.to_path_buf()
}

/// The git root at or above `root`, else `root`.
fn repo_top(root: &Path) -> PathBuf {
    root.ancestors()
        .find(|a| a.join(".git").exists())
        .unwrap_or(root)
        .to_path_buf()
}

/// What a folder's project files call for.
enum Found {
    /// A lockfile (or a manifest that needs none): install here.
    Here(String),
    /// A manifest without a lockfile: a workspace above may hold it; else
    /// this command, here.
    Unlocked(String),
}

/// The install command for `ecosystem` from the project files of `dir`,
/// else of the nearest folder above it up to `root` (only `dir` itself
/// when it lies outside `root`). A folder with a manifest and no lockfile
/// installs from the nearest folder above it, up to the git root, that has
/// a lockfile or declares a workspace.
#[must_use]
pub fn install(dir: &Path, root: &Path, ecosystem: Ecosystem) -> Option<Install> {
    let dir = canonical(dir);
    let root = canonical(root);
    let top = repo_top(&root);
    for folder in dir.ancestors() {
        if folder != dir && !folder.starts_with(&root) {
            break;
        }
        match found_in(folder, ecosystem) {
            Some(Found::Here(command)) => {
                return Some(Install {
                    dir: folder.to_path_buf(),
                    command,
                });
            }
            Some(Found::Unlocked(command)) => {
                let workspace = folder
                    .ancestors()
                    .skip(1)
                    .take_while(|up| up.starts_with(&top))
                    .find_map(|up| workspace_in(up, ecosystem).map(|c| (up, c)));
                let (dir, command) = workspace.unwrap_or((folder, command));
                return Some(Install {
                    dir: dir.to_path_buf(),
                    command,
                });
            }
            None => {}
        }
        if folder == root {
            break;
        }
    }
    None
}

/// The largest project file read (a `package.json`, a `pyproject.toml`).
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// The text of `path`, when it is a file of at most [`MAX_FILE_BYTES`].
fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    (meta.is_file() && meta.len() <= MAX_FILE_BYTES)
        .then(|| std::fs::read_to_string(path).ok())
        .flatten()
}

/// The install a Node lockfile in `dir` calls for.
fn node_lock(dir: &Path) -> Option<String> {
    let has = |name: &str| dir.join(name).is_file();
    let command = if has("pnpm-lock.yaml") {
        "pnpm install --frozen-lockfile"
    } else if has("yarn.lock") {
        // Yarn 2+ (berry) has .yarnrc.yml and refuses --frozen-lockfile.
        if has(".yarnrc.yml") {
            "yarn install --immutable"
        } else {
            "yarn install --frozen-lockfile"
        }
    } else if has("bun.lock") || has("bun.lockb") {
        "bun install"
    } else if has("package-lock.json") || has("npm-shrinkwrap.json") {
        "npm ci"
    } else {
        return None;
    };
    Some(command.to_owned())
}

/// The install a Python lockfile (or Pipfile) in `dir` calls for.
fn python_lock(dir: &Path) -> Option<String> {
    let has = |name: &str| dir.join(name).is_file();
    let command = if has("uv.lock") {
        "uv sync"
    } else if has("poetry.lock") {
        "poetry install"
    } else if has("pdm.lock") {
        "pdm install"
    } else if has("Pipfile") {
        "pipenv install --dev"
    } else {
        return None;
    };
    Some(command.to_owned())
}

/// What `ecosystem`'s project files in `dir` call for.
fn found_in(dir: &Path, ecosystem: Ecosystem) -> Option<Found> {
    let has = |name: &str| dir.join(name).is_file();
    let found = match ecosystem {
        Ecosystem::Ruby => Found::Here(has("Gemfile").then_some("bundle install")?.to_owned()),
        Ecosystem::Node => match node_lock(dir) {
            Some(command) => Found::Here(command),
            None if has("package.json") => Found::Unlocked("npm install".to_owned()),
            None => return None,
        },
        Ecosystem::Python => {
            if let Some(command) = python_lock(dir) {
                Found::Here(command)
            } else {
                let requirements = requirements(dir);
                if !requirements.is_empty() {
                    let files: Vec<String> = requirements
                        .iter()
                        .map(|f| format!("-r {}", shell_word(f)))
                        .collect();
                    Found::Here(format!("pip install {}", files.join(" ")))
                } else if has("pyproject.toml") || has("setup.py") {
                    let extra =
                        read_small(&dir.join("pyproject.toml")).and_then(|text| test_extra(&text));
                    Found::Unlocked(match extra {
                        Some(extra) => format!("pip install -e '.[{extra}]'"),
                        None => "pip install -e .".to_owned(),
                    })
                } else {
                    return None;
                }
            }
        }
        Ecosystem::Go => Found::Here(has("go.mod").then_some("go mod tidy")?.to_owned()),
        Ecosystem::Java => {
            if has("pom.xml") {
                let mvn = if has("mvnw") { "./mvnw" } else { "mvn" };
                Found::Here(format!("{mvn} -q -DskipTests package"))
            } else if has("build.gradle") || has("build.gradle.kts") {
                let gradle = if has("gradlew") {
                    "./gradlew"
                } else {
                    "gradle"
                };
                Found::Here(format!("{gradle} build -x test"))
            } else {
                return None;
            }
        }
    };
    Some(found)
}

/// The install a workspace root (or a lockfile) in `dir` calls for, for a
/// member below it without a lockfile.
fn workspace_in(dir: &Path, ecosystem: Ecosystem) -> Option<String> {
    match ecosystem {
        Ecosystem::Node => node_lock(dir).or_else(|| {
            if dir.join("pnpm-workspace.yaml").is_file() {
                Some("pnpm install".to_owned())
            } else {
                read_small(&dir.join("package.json"))
                    .filter(|text| text.contains("\"workspaces\""))
                    .map(|_| "npm install".to_owned())
            }
        }),
        Ecosystem::Python => {
            if dir.join("uv.lock").is_file()
                || read_small(&dir.join("pyproject.toml"))
                    .is_some_and(|text| text.contains("[tool.uv.workspace]"))
            {
                // From the workspace root, every member.
                Some("uv sync --all-packages".to_owned())
            } else {
                python_lock(dir)
            }
        }
        Ecosystem::Ruby | Ecosystem::Go | Ecosystem::Java => None,
    }
}

/// Requirement files that hold test or development dependencies.
const TEST_STEMS: &[&str] = &["test", "tests", "testing", "dev", "development"];

/// Requirements files listed in one `pip install` at most.
const MAX_REQUIREMENTS: usize = 4;

/// The `.txt` file names of `dir`, sorted.
fn txt_files(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| n.ends_with(".txt"))
        .collect();
    found.sort();
    found
}

/// The requirements files of `dir` to install from: `requirements.txt` and
/// its test and development variants (`requirements-dev.txt`, ...); else
/// those of a `requirements/` folder (`test.txt`, `dev.txt`, else
/// `base.txt`); else any `requirements*.txt`. At most [`MAX_REQUIREMENTS`].
fn requirements(dir: &Path) -> Vec<String> {
    let files: Vec<String> = txt_files(dir)
        .into_iter()
        .filter(|n| n.starts_with("requirements"))
        .collect();
    let mut out: Vec<String> = Vec::new();
    if files.iter().any(|f| f == "requirements.txt") {
        out.push("requirements.txt".to_owned());
    }
    out.extend(
        files
            .iter()
            .filter(|f| {
                let stem = f
                    .trim_start_matches("requirements")
                    .trim_end_matches(".txt")
                    .trim_start_matches(['-', '_', '.']);
                TEST_STEMS.contains(&stem)
            })
            .cloned(),
    );
    if out.is_empty() {
        let folder = txt_files(&dir.join("requirements"));
        let stem = |f: &String| f.trim_end_matches(".txt").to_owned();
        out.extend(
            folder
                .iter()
                .filter(|f| TEST_STEMS.contains(&stem(f).as_str()))
                .map(|f| format!("requirements/{f}")),
        );
        if out.is_empty() {
            out.extend(
                folder
                    .iter()
                    .filter(|f| matches!(stem(f).as_str(), "base" | "common" | "requirements"))
                    .map(|f| format!("requirements/{f}")),
            );
        }
    }
    if out.is_empty() {
        out = files;
    }
    out.truncate(MAX_REQUIREMENTS);
    out
}

/// The `[project.optional-dependencies]` extra that holds the tests' (or
/// development) dependencies, if `pyproject` declares one.
fn test_extra(pyproject: &str) -> Option<String> {
    let mut in_section = false;
    let mut extras = Vec::new();
    for line in pyproject.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[project.optional-dependencies]";
            continue;
        }
        if in_section && let Some((key, _)) = line.split_once('=') {
            extras.push(key.trim().trim_matches(['"', '\'']).to_owned());
        }
    }
    ["test", "tests", "testing", "dev"]
        .iter()
        .find(|e| extras.iter().any(|x| x == *e))
        .map(|e| (*e).to_owned())
}

/// The next step for `missing`, found in the output of `who` (`The tests`,
/// `The app's server`): the project's install command when one was found,
/// run in the agent's own shell (outside `mockingbird run`, whose proxy may
/// block the package registry). A command no runtime claims gets no
/// install: it is not the project's dependency.
#[must_use]
pub fn step(who: &str, missing: &Missing, install: Option<&Install>) -> String {
    let what = missing.describe();
    let name = missing.name.as_deref().unwrap_or("it");
    if missing.unknown_command() {
        return format!(
            "{who} failed on {what}. `{name}` is not installed on this machine (or not on PATH): install it, then run verify --run again."
        );
    }
    let install_part = match (install, missing.ecosystem) {
        (Some(i), _) => format!("install the project's dependencies (`{}`)", i.line()),
        (None, Some(e)) => format!(
            "install the project's dependencies with its own install step (no {} was found from the folder it ran in up to the project root)",
            e.manifests()
        ),
        (None, None) => "install the project's dependencies".to_owned(),
    };
    let mut s = if missing.toolchain {
        format!(
            "{who} failed on {what}. `{name}` is not installed on this machine (or not on PATH): install it, then {install_part}, then run verify --run again."
        )
    } else {
        let mut first = install_part;
        if let Some(c) = first.get_mut(0..1) {
            c.make_ascii_uppercase();
        }
        format!("{who} failed on {what}. {first}, then run verify --run again.")
    };
    s.push_str(" Run the install in your own shell, not under `mockingbird run`.");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str) {
        write_text(root, rel, "");
    }

    fn write_text(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn named(tail: &str) -> (String, &'static str, Option<Ecosystem>) {
        let m = find(tail, &[]).unwrap_or_else(|| panic!("nothing found in {tail:?}"));
        (m.name.unwrap_or_default(), m.marker, m.ecosystem)
    }

    /// The install command for the missing dependency of `tail`, for
    /// `command` run from `root`.
    fn hint(root: &Path, command: &str, tail: &str) -> Option<String> {
        let m = find(tail, &[])?;
        install(&command_dir(root, command), root, m.ecosystem?).map(|i| i.command)
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
            named(
                "/gems/bundler/spec_set.rb:91:in `materialize': Could not find rack-3 in any of the sources (Bundler::GemNotFound)"
            ),
            (
                "rack-3".into(),
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
            find(
                "x.rb:1:in `require': cannot load such file -- /app/lib/thing (LoadError)",
                &[]
            ),
            None
        );
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Gemfile");
        assert_eq!(
            hint(dir.path(), "bundle exec rspec", "Could not find gem 'x'").as_deref(),
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
        // Jest indents it.
        assert_eq!(
            named("    Cannot find module '@scope/pkg/lib/x.js' from 'src/app.js'"),
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
        assert_eq!(find("Error: Cannot find module './routes'", &[]), None);
        assert_eq!(
            find("Error: Cannot find module '/app/server.js'", &[]),
            None
        );
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
        write(dir.path(), "bun.lockb");
        assert_eq!(
            hint(dir.path(), "npm test", tail).as_deref(),
            Some("bun install")
        );
        write(dir.path(), "yarn.lock");
        assert_eq!(
            hint(dir.path(), "npm test", tail).as_deref(),
            Some("yarn install --frozen-lockfile")
        );
        write(dir.path(), ".yarnrc.yml");
        assert_eq!(
            hint(dir.path(), "npm test", tail).as_deref(),
            Some("yarn install --immutable")
        );
        write(dir.path(), "pnpm-lock.yaml");
        assert_eq!(
            hint(dir.path(), "npm test", tail).as_deref(),
            Some("pnpm install --frozen-lockfile")
        );
    }

    #[test]
    fn python_errors_name_the_top_module_and_pick_the_lock_requirements_or_extras() {
        assert_eq!(
            named("ModuleNotFoundError: No module named 'requests.adapters'"),
            (
                "requests".into(),
                "ModuleNotFoundError",
                Some(Ecosystem::Python)
            )
        );
        assert_eq!(
            named("E   ModuleNotFoundError: No module named 'httpx'"),
            (
                "httpx".into(),
                "ModuleNotFoundError",
                Some(Ecosystem::Python)
            )
        );
        assert_eq!(
            named("/usr/bin/python3: No module named pytest"),
            ("pytest".into(), "No module named", Some(Ecosystem::Python))
        );
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let tail = "ModuleNotFoundError: No module named 'requests'";
        write(root, "setup.py");
        assert_eq!(
            hint(root, "pytest", tail).as_deref(),
            Some("pip install -e .")
        );
        write_text(
            root,
            "pyproject.toml",
            "[project]\nname = \"x\"\n[project.optional-dependencies]\ndocs = [\"a\"]\ntest = [\"pytest\"]\n[tool.x]\ndev = 1\n",
        );
        assert_eq!(
            hint(root, "pytest", tail).as_deref(),
            Some("pip install -e '.[test]'")
        );
        write(root, "requirements/base.txt");
        assert_eq!(
            hint(root, "pytest", tail).as_deref(),
            Some("pip install -r requirements/base.txt")
        );
        write(root, "requirements/test.txt");
        assert_eq!(
            hint(root, "pytest", tail).as_deref(),
            Some("pip install -r requirements/test.txt")
        );
        write(root, "requirements-prod.txt");
        write(root, "requirements-dev.txt");
        write(root, "requirements.txt");
        assert_eq!(
            hint(root, "pytest", tail).as_deref(),
            Some("pip install -r requirements.txt -r requirements-dev.txt")
        );
        write(root, "Pipfile");
        assert_eq!(
            hint(root, "pytest", tail).as_deref(),
            Some("pipenv install --dev")
        );
        write(root, "pdm.lock");
        assert_eq!(hint(root, "pytest", tail).as_deref(), Some("pdm install"));
        write(root, "poetry.lock");
        assert_eq!(
            hint(root, "pytest", tail).as_deref(),
            Some("poetry install")
        );
        write(root, "uv.lock");
        assert_eq!(hint(root, "pytest", tail).as_deref(), Some("uv sync"));
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
            named("main.go:5:2: missing go.sum entry for module providing package github.com/q (imported by m); to add:").1,
            "missing go.sum entry"
        );
        assert_eq!(
            named("Exception in thread \"main\" java.lang.ClassNotFoundException: org.x.Client"),
            (
                "org.x.Client".into(),
                "ClassNotFoundException",
                Some(Ecosystem::Java)
            )
        );
        assert_eq!(
            named("Caused by: java.lang.NoClassDefFoundError: org/x/Client").0,
            "org.x.Client"
        );
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "go.mod");
        assert_eq!(
            hint(
                dir.path(),
                "go test ./...",
                "go: cannot find package \"a/b\""
            )
            .as_deref(),
            Some("go mod tidy")
        );
        let java = "Caused by: java.lang.ClassNotFoundException: org.x.Client";
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
    fn log_noise_and_assertions_are_not_runtime_errors() {
        // A test's assertion that mentions the text.
        assert_eq!(
            find(
                "  expected output to include \"Cannot find module 'express'\"",
                &[]
            ),
            None
        );
        assert_eq!(
            find(
                "AssertionError: assert \"No module named 'requests'\" in err",
                &[]
            ),
            None
        );
        // A library logging the probe of an optional class.
        assert_eq!(
            find(
                "2026-11-12 DEBUG o.x.Loader - optional class absent: java.lang.ClassNotFoundException: org.opt.Codec\nTests run: 3, Failures: 1",
                &[]
            ),
            None
        );
        // An uncaught trace at the start of a line is.
        assert_eq!(
            named("java.lang.ClassNotFoundException: org.x.C\n\tat java.base/x.y(Z.java:1)").0,
            "org.x.C"
        );
        // The last error wins.
        assert_eq!(
            named("Error: Cannot find module 'a'\nError: Cannot find module 'b'").0,
            "b"
        );
    }

    #[test]
    fn the_projects_own_modules_are_not_missing_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "myapp/__init__.py");
        write(root, "settings.py");
        write(root, "src/core/__init__.py");
        write(root, "lib/helpers/format.rb");
        write(root, "src/utils/date.ts");
        let dirs = [root];
        for tail in [
            "ModuleNotFoundError: No module named 'myapp.views'",
            "ModuleNotFoundError: No module named 'settings'",
            "ModuleNotFoundError: No module named 'core'",
            "x.rb:1:in `require': cannot load such file -- helpers/format (LoadError)",
            "Error: Cannot find module 'utils/date'",
            "Error: Cannot find module '@/components/Button'",
            "Error: Cannot find module '#config'",
        ] {
            assert_eq!(find(tail, &dirs), None, "{tail}");
        }
        // A dependency is still one; an earlier one is found past a local one.
        assert_eq!(
            find("ModuleNotFoundError: No module named 'requests'", &dirs)
                .and_then(|m| m.name)
                .as_deref(),
            Some("requests")
        );
        assert_eq!(
            find(
                "ModuleNotFoundError: No module named 'requests'\nModuleNotFoundError: No module named 'myapp'",
                &dirs
            )
            .and_then(|m| m.name)
            .as_deref(),
            Some("requests")
        );
    }

    #[test]
    fn a_runner_not_found_is_a_missing_command_of_its_runtime() {
        for (line, name) in [
            ("sh: rspec: command not found", "rspec"),
            ("sh: line 1: jest: command not found", "jest"),
            ("zsh: command not found: pytest", "pytest"),
            ("bundler: command not found: rspec", "rspec"),
            ("sh: 1: ./node_modules/.bin/vitest: not found", "vitest"),
            (
                "'mocha' is not recognized as an internal or external command,",
                "mocha",
            ),
        ] {
            let m = find(line, &[]).unwrap_or_else(|| panic!("{line}"));
            assert_eq!(m.name.as_deref(), Some(name), "{line}");
            assert_eq!(m.what, "command");
            assert!(!m.toolchain);
        }
        assert_eq!(
            find("sh: rspec: command not found", &[]).unwrap().ecosystem,
            Some(Ecosystem::Ruby)
        );
        let mvn = find("sh: mvn: command not found", &[]).unwrap();
        assert!(mvn.toolchain);
        assert_eq!(mvn.ecosystem, Some(Ecosystem::Java));
        // A line that only mentions "not found" is not a shell's.
        assert_eq!(find("GET /users/7: not found", &[]), None);
        assert_eq!(find("lookup: user 7: not found", &[]), None);
        // A runtime's own error wins over a later command not found.
        assert_eq!(
            named("ModuleNotFoundError: No module named 'x'\nsh: foo: command not found").0,
            "x"
        );
    }

    #[test]
    fn cd_prefixes_are_followed() {
        let cwd = Path::new("/w");
        for (command, dir) in [
            ("npm test", "/w"),
            ("cd web && npm test", "/w/web"),
            ("cd web; npm test", "/w/web"),
            ("cd web&&npm test", "/w/web"),
            ("cd 'my app' && cd tests && pytest", "/w/my app/tests"),
            ("cd my\\ app && make", "/w/my app"),
            ("cd \"a b\" || exit 1; make", "/w/a b"),
            ("cd web || exit && npm test", "/w/web"),
            ("cd web && cd - && npm test", "/w"),
            ("cd /abs/x && make", "/abs/x"),
            ("cd $APP && make", "/w"),
            ("cd web | tee x", "/w/web"),
            ("cd web || true && make", "/w/web"),
        ] {
            assert_eq!(command_dir(cwd, command), PathBuf::from(dir), "{command}");
        }
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
        let found = install(
            &command_dir(root, "cd web && npm test"),
            root,
            Ecosystem::Node,
        )
        .unwrap();
        assert_eq!(found.command, "npm ci");
        assert!(found.dir.ends_with("web"), "{found:?}");
        assert!(found.line().ends_with("web && npm ci"), "{}", found.line());
        // The nearest requirements are above the folder it cds into.
        let found = install(
            &command_dir(root, "cd 'api' && cd tests && pytest"),
            root,
            Ecosystem::Python,
        )
        .unwrap();
        assert_eq!(found.command, "pip install -r requirements.txt");
        assert!(found.dir.ends_with("api"), "{found:?}");
        // Only the runtime's own files count.
        assert_eq!(install(&root.join("api"), root, Ecosystem::Node), None);
        // Not above the project root.
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        assert_eq!(install(&inner, &inner, Ecosystem::Ruby), None);
        // A folder that does not exist still compares with the root.
        let found = install(&root.join("api/missing"), root, Ecosystem::Python).unwrap();
        assert!(found.dir.ends_with("api"), "{found:?}");
    }

    #[test]
    fn a_workspace_member_installs_from_its_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        std::fs::create_dir(repo.join(".git")).unwrap();
        // npm: the lockfile is at the workspace root, above the project root.
        write_text(repo, "package.json", "{\"workspaces\": [\"packages/*\"]}");
        write(repo, "package-lock.json");
        write(repo, "packages/api/package.json");
        let member = repo.join("packages/api");
        let found = install(&member, &member, Ecosystem::Node).unwrap();
        assert_eq!(found.command, "npm ci");
        assert_eq!(found.dir, canonical(repo));
        // pnpm without a lockfile yet: the workspace file.
        let pnpm = tempfile::tempdir().unwrap();
        write(pnpm.path(), "pnpm-workspace.yaml");
        write(pnpm.path(), "apps/web/package.json");
        let found = install(&pnpm.path().join("apps/web"), pnpm.path(), Ecosystem::Node).unwrap();
        assert_eq!(found.command, "pnpm install");
        assert_eq!(found.dir, canonical(pnpm.path()));
        // uv: a member's pyproject, the workspace's uv.lock.
        let uv = tempfile::tempdir().unwrap();
        write_text(
            uv.path(),
            "pyproject.toml",
            "[tool.uv.workspace]\nmembers = [\"libs/*\"]\n",
        );
        write(uv.path(), "uv.lock");
        write(uv.path(), "libs/client/pyproject.toml");
        let found = install(&uv.path().join("libs/client"), uv.path(), Ecosystem::Python).unwrap();
        assert_eq!(found.command, "uv sync --all-packages");
        assert_eq!(found.dir, canonical(uv.path()));
        // A lone package.json with no workspace above installs in place.
        let lone = tempfile::tempdir().unwrap();
        write(lone.path(), "app/package.json");
        let found = install(&lone.path().join("app"), lone.path(), Ecosystem::Node).unwrap();
        assert_eq!(found.command, "npm install");
        assert!(found.dir.ends_with("app"));
    }

    #[test]
    fn clean_failures_name_no_dependency() {
        assert_eq!(
            find(
                "FAIL spec/orders_spec.rb\nExpected 200, got 500\n3 examples, 1 failure",
                &[]
            ),
            None
        );
        assert_eq!(find("", &[]), None);
    }

    #[test]
    fn the_step_names_the_dependency_and_the_install_to_run_in_the_agents_shell() {
        let m = find(
            "Could not find gem 'rest-client' in locally installed gems. (Bundler::GemNotFound)",
            &[],
        )
        .unwrap();
        let install = Install {
            dir: PathBuf::from("/abs/dir"),
            command: "bundle install".into(),
        };
        assert_eq!(
            step("The tests", &m, Some(&install)),
            "The tests failed on a missing gem `rest-client` (Bundler::GemNotFound). Install the project's dependencies (`cd /abs/dir && bundle install`), then run verify --run again. Run the install in your own shell, not under `mockingbird run`."
        );
        assert!(step("The tests", &m, None).contains("no Gemfile was found"));
        let mvn = find("sh: mvn: command not found", &[]).unwrap();
        assert!(step("The tests", &mvn, None).contains("`mvn` is not installed on this machine"),);
        // A command no runtime claims: no project install.
        let unknown = find("sh: frobnicate: command not found", &[]).unwrap();
        assert_eq!(unknown.ecosystem, None);
        assert_eq!(
            step("The tests", &unknown, None),
            "The tests failed on a missing command `frobnicate` (command not found). `frobnicate` is not installed on this machine (or not on PATH): install it, then run verify --run again."
        );
    }
}
