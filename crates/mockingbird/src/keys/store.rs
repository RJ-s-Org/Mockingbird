//! The private key store: one file per key in `~/.mockingbird/secrets`,
//! mode `0600` in a `0700` directory (an owner-only ACL on Windows).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};

/// `<HOME or USERPROFILE>/.mockingbird/secrets`.
///
/// # Errors
///
/// Fails when neither `HOME` nor `USERPROFILE` is set.
pub fn default_store_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| PathBuf::from(h).join(".mockingbird").join("secrets"))
        .ok_or_else(|| anyhow!("cannot find the home directory (HOME)"))
}

/// Whether `name` can name a key file: 1-64 characters of
/// `[A-Za-z0-9._-]`, not starting with a dot (so never a path).
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Writes `value` (trimmed) to `<dir>/<name>`, readable by the owner only,
/// replacing any previous key atomically; `dir` is created (or tightened)
/// to owner-only access. Returns the file's absolute path. The value is
/// never logged.
///
/// # Errors
///
/// Fails for an invalid `name` or an empty value, and when `dir` or the
/// file cannot be created, restricted or renamed into place.
pub fn write_secret(dir: &Path, name: &str, value: &str) -> anyhow::Result<PathBuf> {
    if !valid_name(name) {
        bail!("key names are 1-64 characters of [A-Za-z0-9._-], not starting with a dot");
    }
    let value = value.trim();
    if value.is_empty() {
        bail!("the key is empty");
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let dir = std::path::absolute(dir).with_context(|| format!("resolving {}", dir.display()))?;
    restrict_dir(&dir)?;
    let tmp = dir.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4().simple()));
    let written = write_private(&tmp, value).and_then(|()| {
        let file = dir.join(name);
        std::fs::rename(&tmp, &file)
            .with_context(|| format!("moving the key into {}", file.display()))?;
        restrict_file(&file)?;
        Ok(file)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(unix)]
fn write_private(path: &Path, value: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(value.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(not(unix))]
fn write_private(path: &Path, value: &str) -> anyhow::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    restrict_file(path)?;
    f.write_all(value.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("making {} private", dir.display()))
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("making {} private", path.display()))
}

/// `icacls <path> /inheritance:r /grant:r <USERNAME>:<grant>`.
#[cfg(windows)]
fn icacls(path: &Path, grant: &str) -> anyhow::Result<()> {
    let user = std::env::var("USERNAME").context("USERNAME is not set")?;
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:{grant}"))
        .stdout(std::process::Stdio::null())
        .status()
        .context("running icacls")?;
    if !status.success() {
        bail!(
            "icacls could not make {} private ({status})",
            path.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn restrict_dir(dir: &Path) -> anyhow::Result<()> {
    icacls(dir, "(OI)(CI)F")
}

#[cfg(windows)]
fn restrict_file(path: &Path) -> anyhow::Result<()> {
    icacls(path, "F")
}

#[cfg(not(any(unix, windows)))]
fn restrict_dir(_dir: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn restrict_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// The keys in `dir`: `[{name, file, private}]` sorted by name, without
/// reading any of them. `private` is false for a file others may read.
pub fn list(dir: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut keys: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            (!name.starts_with('.')).then(|| (name, e.path()))
        })
        .collect();
    keys.sort();
    keys.into_iter()
        .map(|(name, file)| {
            let private = mb_ext::check_secret_file(&file).is_ok();
            json!({ "name": name, "file": file, "private": private })
        })
        .collect()
}

/// The profile reference for a key file: `file:<absolute path>`.
pub fn file_reference(path: &Path) -> String {
    format!("file:{}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_names_are_plain() {
        let long = "x".repeat(65);
        for bad in [
            "",
            "../../.ssh/id",
            "a/b",
            "a\\b",
            ".hidden",
            "..",
            long.as_str(),
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(
            valid_name("openrouter") && valid_name("OPENROUTER_API_KEY") && valid_name("a.b-c")
        );
        let dir = tempfile::tempdir().unwrap();
        assert!(write_secret(dir.path(), "../x", "v").is_err());
        assert!(write_secret(dir.path(), "ok", "  ").is_err());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn written_secrets_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".mockingbird/secrets");
        let path = write_secret(&dir, "openrouter", "  sk-abc \n").unwrap();
        assert_eq!(path, dir.join("openrouter"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "sk-abc");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let listed = list(&dir);
        assert_eq!(
            listed,
            vec![json!({"name":"openrouter","file":path,"private":true})]
        );
        assert!(!serde_json::to_string(&listed).unwrap().contains("sk-abc"));
        write_secret(&dir, "openrouter", "sk-new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "sk-new");
        assert_eq!(list(&dir).len(), 1, "no temporary files left");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(list(&dir)[0]["private"], false);
        assert_eq!(file_reference(&path), format!("file:{}", path.display()));
    }
}
