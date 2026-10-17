//! Secret resolution: the `SecretStore` trait is part of the semver-stable
//! surface `mockingbird-cloud` builds against.
//!
//! Only references are ever stored; implementations must never log the
//! resolved value.

use async_trait::async_trait;

use crate::ExtError;

/// Resolves a secret reference (e.g. `env:NAME`) to its value.
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// The secret value `reference` points to.
    async fn resolve(&self, reference: &str) -> Result<String, ExtError>;
}

/// Self-hosted default: `env:NAME` reads an environment variable and
/// `file:/path` reads a file, trimming surrounding whitespace.
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
