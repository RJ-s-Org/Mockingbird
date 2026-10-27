//! Secret resolution: the `SecretStore` trait is part of the semver-stable
//! surface `mockingbird-cloud` builds against.
//!
//! Only references are ever stored; implementations must never log the
//! resolved value.

use std::path::Path;

use async_trait::async_trait;

use crate::ExtError;

/// Resolves a secret reference (e.g. `env:NAME`) to its value.
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// The secret value `reference` points to.
    async fn resolve(&self, reference: &str) -> Result<String, ExtError>;
}

/// Whether `path` is in Mockingbird's private key store: a file whose
/// directory is `secrets` inside a `.mockingbird` directory
/// (`~/.mockingbird/secrets/<name>`).
pub fn in_key_store(path: &Path) -> bool {
    let dir = path.parent();
    dir.and_then(Path::file_name).is_some_and(|n| n == "secrets")
        && dir
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|n| n == ".mockingbird")
}

/// Refuses a key-store file ([`in_key_store`]) that group or others may
/// read (Unix modes); every other path, and every path on other platforms,
/// passes. The error never contains the file's contents.
pub fn check_secret_file(path: &Path) -> Result<(), ExtError> {
    #[cfg(unix)]
    if in_key_store(path) {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(ExtError::Other(format!(
                    "secret file {} is readable by other users (mode {mode:o}); run chmod 600 {}",
                    path.display(),
                    path.display()
                )));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Self-hosted default: `env:NAME` reads an environment variable and
/// `file:/path` reads a file, trimming surrounding whitespace. A `file:`
/// path in the key store must not be readable by others
/// ([`check_secret_file`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvFileSecrets;

#[async_trait]
impl SecretStore for EnvFileSecrets {
    async fn resolve(&self, reference: &str) -> Result<String, ExtError> {
        if let Some(name) = reference.strip_prefix("env:") {
            if name.is_empty() {
                return Err(ExtError::BadSecretRef(reference.to_string()));
            }
            return std::env::var(name).map_err(|_| ExtError::SecretNotFound(name.to_string()));
        }
        if let Some(path) = reference.strip_prefix("file:") {
            if path.is_empty() {
                return Err(ExtError::BadSecretRef(reference.to_string()));
            }
            check_secret_file(Path::new(path))?;
            return match tokio::fs::read_to_string(path).await {
                Ok(contents) => Ok(contents.trim().to_string()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(ExtError::SecretNotFound(path.to_string()))
                }
                Err(e) => Err(ExtError::Other(format!("reading secret file {path}: {e}"))),
            };
        }
        Err(ExtError::BadSecretRef(reference.to_string()))
    }
}
