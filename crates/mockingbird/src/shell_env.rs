//! Environment variables from the user's login shell.
//!
//! Agents start `mockingbird mcp` from a desktop app or an IDE, whose
//! environment lacks what `~/.zshenv` or `~/.bashrc` exports, such as the LLM
//! key a profile references. Like VS Code, the MCP server asks the login shell
//! for its environment once at startup and takes the variables it lacks.

use std::collections::HashSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Set to skip the import.
pub const DISABLE_ENV: &str = "MB_NO_SHELL_ENV";

/// How long the login shell may take. A shell that loads version managers or a
/// framework can take several seconds, and one cut short gives no variables at all.
const TIMEOUT: Duration = Duration::from_secs(15);
const MARKER: &str = "__MOCKINGBIRD_ENV__";

/// Shell bookkeeping that describes the shell, not the user's setup.
const SKIP: &[&str] = &["_", "PWD", "OLDPWD", "SHLVL", "PS1", "PS2", "TERM"];

static IMPORTED: OnceLock<Vec<String>> = OnceLock::new();

/// The variable names the import added, or `None` when this process never
/// tried (it is not the MCP server).
pub fn imported() -> Option<&'static [String]> {
    IMPORTED.get().map(Vec::as_slice)
}

/// Reads the login shell's environment and sets every variable this process
/// lacks, or has empty where the shell's is not (an agent's `${NAME:-}`
/// passthrough of a variable it lacks); returns their names. Never fails: a missing, slow or broken shell
/// imports nothing.
///
/// # Safety
///
/// Calls [`std::env::set_var`], so no other thread may exist: call it first
/// thing in `main`, before the async runtime starts.
pub unsafe fn import() -> &'static [String] {
    let names = match std::env::var_os(DISABLE_ENV) {
        Some(_) => Vec::new(),
        None => {
            let found = login_env().unwrap_or_default();
            let mut names = Vec::new();
            for (name, value) in found {
                let lacking = match std::env::var_os(&name) {
                    None => true,
                    Some(v) => v.is_empty() && !value.is_empty(),
                };
                if lacking {
                    // SAFETY: the caller guarantees the process is still
                    // single-threaded, and `login_env` has joined its reader.
                    unsafe { std::env::set_var(&name, value) };
                    names.push(name);
                }
            }
            names.sort();
            names
        }
    };
    IMPORTED.get_or_init(|| names)
}

/// `$SHELL -l -i -c 'env -0'`, bounded by [`TIMEOUT`].
fn login_env() -> Option<Vec<(String, String)>> {
    let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty())?;
    let script = format!("printf '%s' {MARKER}; env -0; printf '%s' {MARKER}");
    let (out, timed_out) = run_login_shell(&shell, &script, TIMEOUT)?;
    if timed_out {
        // Straight to stderr: this runs before `main` installs the tracing
        // subscriber, which would drop the event.
        eprintln!("{}", timeout_warning(TIMEOUT));
    }
    Some(parse(&out))
}

/// Runs `shell -l -i -c script` and returns its stdout, and whether it was
/// killed for taking longer than `timeout`.
fn run_login_shell(
    shell: &std::ffi::OsStr,
    script: &str,
    timeout: Duration,
) -> Option<(String, bool)> {
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let start = Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    let out = reader.join().ok()?;
    Some((String::from_utf8_lossy(&out).into_owned(), timed_out))
}

fn timeout_warning(timeout: Duration) -> String {
    format!(
        "warning: the login shell did not print its environment within {} seconds: its variables (such as an LLM key a profile references) were not imported",
        timeout.as_secs()
    )
}

/// The `NAME=value` pairs between the markers, valid names only.
fn parse(out: &str) -> Vec<(String, String)> {
    let Some((_, rest)) = out.split_once(MARKER) else {
        return Vec::new();
    };
    let Some((env, _)) = rest.rsplit_once(MARKER) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    env.split('\0')
        .filter_map(|entry| entry.split_once('='))
        .filter(|(name, _)| valid_name(name) && !SKIP.contains(name))
        .filter(|(name, _)| seen.insert(name.to_string()))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_between_markers() {
        let out = format!(
            "motd noise{MARKER}A=1\0B_2=x=y\0PWD=/tmp\0bad-name=z\0\0A=dup\0{MARKER}trailing"
        );
        assert_eq!(
            parse(&out),
            vec![
                ("A".to_string(), "1".to_string()),
                ("B_2".to_string(), "x=y".to_string())
            ]
        );
    }

    #[test]
    fn no_markers_no_variables() {
        assert!(parse("A=1\0").is_empty());
        assert!(!valid_name("1A"));
        assert!(valid_name("_A1"));
    }

    #[cfg(unix)]
    #[test]
    fn a_shell_past_the_deadline_is_killed_and_reported() {
        let started = Instant::now();
        let (out, timed_out) = run_login_shell(
            std::ffi::OsStr::new("/bin/sh"),
            "printf early; exec sleep 30",
            Duration::from_millis(200),
        )
        .expect("/bin/sh runs");
        assert!(timed_out);
        assert!(parse(&out).is_empty(), "{out}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "killed, not waited out"
        );
        assert!(timeout_warning(TIMEOUT).contains("within 15 seconds"));
    }

    #[cfg(unix)]
    #[test]
    fn a_shell_within_the_deadline_is_read() {
        let script = format!("printf '%s' {MARKER}; printf 'A=1'; printf '%s' {MARKER}");
        let (out, timed_out) = run_login_shell(std::ffi::OsStr::new("/bin/sh"), &script, TIMEOUT)
            .expect("/bin/sh runs");
        assert!(!timed_out);
        assert_eq!(parse(&out), vec![("A".to_string(), "1".to_string())]);
    }
}
