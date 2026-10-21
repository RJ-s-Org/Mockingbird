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

const TIMEOUT: Duration = Duration::from_secs(5);
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
/// lacks; returns their names. Never fails: a missing, slow or broken shell
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
                if std::env::var_os(&name).is_none() {
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
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", &script])
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
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    let out = reader.join().ok()?;
    Some(parse(&String::from_utf8_lossy(&out)))
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
}
