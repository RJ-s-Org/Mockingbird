//! `vault:` secret references (HashiCorp Vault KV v2) and [`DefaultSecrets`],
//! the self-hosted [`SecretStore`] that combines them with `env:`/`file:`.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;

use crate::{EnvFileSecrets, ExtError, SecretStore};

/// How long a resolved Vault value is served from memory.
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// Timeout for one Vault request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the Vault token comes from.
enum TokenSource {
    /// `VAULT_TOKEN`, read once at construction.
    Static(String),
    /// The file named by `VAULT_TOKEN_FILE`, re-read on every cache miss so a
    /// rotated token (e.g. from Vault Agent) is picked up.
    File(PathBuf),
}

/// Resolves `vault:<mount>/<path>#<field>` references against Vault's KV v2
/// API: `GET {VAULT_ADDR}/v1/<mount>/data/<path>`, value `.data.data.<field>`
/// (which must be a string).
///
/// Configured from the environment:
///
/// - `VAULT_ADDR` (required; Vault support is off when unset);
/// - `VAULT_TOKEN`, else the file named by `VAULT_TOKEN_FILE` (re-read on
///   every cache miss);
/// - `VAULT_NAMESPACE` (optional, sent as `X-Vault-Namespace`);
/// - `VAULT_CACERT` (optional PEM bundle added to the default roots).
///
/// Requests time out after 5 s and values are cached for 5 minutes. A missing
/// secret or field is [`ExtError::SecretNotFound`]; any other failure is
/// [`ExtError::Other`] naming the reference, never the value or the token.
pub struct VaultSecrets {
    addr: reqwest::Url,
    token: TokenSource,
    namespace: Option<String>,
    client: reqwest::Client,
    cache: Mutex<HashMap<String, (String, Instant)>>,
}

impl fmt::Debug for VaultSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let token = match &self.token {
            TokenSource::Static(_) => "VAULT_TOKEN".to_string(),
            TokenSource::File(p) => format!("file {}", p.display()),
        };
        f.debug_struct("VaultSecrets")
            .field("addr", &self.addr.as_str())
            .field("token", &token)
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

/// A parsed `vault:<mount>/<path>#<field>` reference.
struct VaultRef<'a> {
    mount: &'a str,
    path: &'a str,
    field: &'a str,
}

fn parse_ref(reference: &str) -> Option<VaultRef<'_>> {
    let rest = reference.strip_prefix("vault:")?;
    let (location, field) = rest.split_once('#')?;
    let (mount, path) = location.split_once('/')?;
    let segment_ok = |s: &str| !s.is_empty() && s != "." && s != ".." && !s.contains('?');
    if field.is_empty() || !segment_ok(mount) || !path.split('/').all(segment_ok) {
        return None;
    }
    Some(VaultRef { mount, path, field })
}

impl VaultSecrets {
    /// Builds the client from the process environment. `Ok(None)` when
    /// `VAULT_ADDR` is unset; an error when it is set but the rest of the
    /// configuration is unusable (no token, bad URL, unreadable CA bundle).
    pub fn from_env() -> Result<Option<Self>, ExtError> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// Like [`VaultSecrets::from_env`], reading the variables through `var`
    /// instead of the process environment. Empty values count as unset.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, ExtError> {
        let var = |name: &str| var(name).filter(|v| !v.trim().is_empty());
        let Some(addr) = var("VAULT_ADDR") else {
            return Ok(None);
        };
        let addr = reqwest::Url::parse(addr.trim())
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https") && !u.cannot_be_a_base())
            .ok_or_else(|| ExtError::Other(format!("invalid VAULT_ADDR {addr:?}")))?;
        let token = match (var("VAULT_TOKEN"), var("VAULT_TOKEN_FILE")) {
            (Some(token), _) => TokenSource::Static(token.trim().to_string()),
            (None, Some(file)) => TokenSource::File(PathBuf::from(file)),
            (None, None) => {
                return Err(ExtError::Other(
                    "VAULT_ADDR is set but neither VAULT_TOKEN nor VAULT_TOKEN_FILE is".to_string(),
                ));
            }
        };
        let mut builder = reqwest::Client::builder().timeout(REQUEST_TIMEOUT);
        if let Some(ca) = var("VAULT_CACERT") {
            let pem = std::fs::read(&ca)
                .map_err(|e| ExtError::Other(format!("reading VAULT_CACERT {ca}: {e}")))?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(|e| ExtError::Other(format!("parsing VAULT_CACERT {ca}: {e}")))?;
            builder = builder.tls_certs_merge(certs);
        }
        let client = builder
            .build()
            .map_err(|e| ExtError::Other(format!("building the Vault client: {e}")))?;
        Ok(Some(Self {
            addr,
            token,
            namespace: var("VAULT_NAMESPACE"),
            client,
            cache: Mutex::new(HashMap::new()),
        }))
    }

    fn cached(&self, reference: &str) -> Option<String> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .get(reference)
            .filter(|(_, expires)| Instant::now() < *expires)
            .map(|(value, _)| value.clone())
    }

    async fn token(&self, reference: &str) -> Result<String, ExtError> {
        let token = match &self.token {
            TokenSource::Static(token) => token.clone(),
            TokenSource::File(path) => tokio::fs::read_to_string(path)
                .await
                .map_err(|e| {
                    ExtError::Other(format!(
                        "vault {reference}: reading VAULT_TOKEN_FILE {}: {e}",
                        path.display()
                    ))
                })?
                .trim()
                .to_string(),
        };
        if token.is_empty() {
            return Err(ExtError::Other(format!(
                "vault {reference}: the Vault token is empty"
            )));
        }
        Ok(token)
    }

    async fn fetch(&self, reference: &str, r: &VaultRef<'_>) -> Result<String, ExtError> {
        let other = |what: &str| ExtError::Other(format!("vault {reference}: {what}"));
        let token = self.token(reference).await?;
        let mut url = self.addr.clone();
        url.path_segments_mut()
            .map_err(|()| other("VAULT_ADDR cannot be a base URL"))?
            .pop_if_empty()
            .extend(["v1", r.mount, "data"])
            .extend(r.path.split('/'));
        let mut request = self.client.get(url).header("X-Vault-Token", token);
        if let Some(ns) = &self.namespace {
            request = request.header("X-Vault-Namespace", ns);
        }
        let response = request
            .send()
            .await
            .map_err(|e| other(&format!("request failed: {}", e.without_url())))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ExtError::SecretNotFound(reference.to_string()));
        }
        if !status.is_success() {
            return Err(other(&format!("Vault answered HTTP {}", status.as_u16())));
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|_| other("Vault answered an unreadable body"))?;
        match body.pointer("/data/data").and_then(|d| d.get(r.field)) {
            None | Some(serde_json::Value::Null) => {
                Err(ExtError::SecretNotFound(reference.to_string()))
            }
            Some(serde_json::Value::String(value)) => Ok(value.clone()),
            Some(_) => Err(other("the field is not a string")),
        }
    }
}

#[async_trait]
impl SecretStore for VaultSecrets {
    async fn resolve(&self, reference: &str) -> Result<String, ExtError> {
        let parsed =
            parse_ref(reference).ok_or_else(|| ExtError::BadSecretRef(reference.to_string()))?;
        if let Some(value) = self.cached(reference) {
            return Ok(value);
        }
        let value = self.fetch(reference, &parsed).await?;
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(
            reference.to_string(),
            (value.clone(), Instant::now() + CACHE_TTL),
        );
        Ok(value)
    }
}

/// The self-hosted [`SecretStore`]: `env:` and `file:` through
/// [`EnvFileSecrets`], and `vault:` through [`VaultSecrets`] when `VAULT_ADDR`
/// is set. Without Vault, `vault:` references are
/// [`ExtError::BadSecretRef`].
#[derive(Debug)]
pub struct DefaultSecrets {
    files: EnvFileSecrets,
    vault: Option<VaultSecrets>,
}

impl DefaultSecrets {
    /// Reads the Vault configuration from the process environment. A Vault
    /// configuration that cannot be used is logged and Vault is disabled.
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// Like [`DefaultSecrets::from_env`], reading the variables through `var`
    /// instead of the process environment.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Self {
        let vault = match VaultSecrets::from_vars(var) {
            Ok(vault) => vault,
            Err(e) => {
                tracing::warn!(error = %e, "Vault secret references disabled");
                None
            }
        };
        Self {
            files: EnvFileSecrets,
            vault,
        }
    }
}

#[async_trait]
impl SecretStore for DefaultSecrets {
    async fn resolve(&self, reference: &str) -> Result<String, ExtError> {
        if reference.starts_with("vault:") {
            return match &self.vault {
                Some(vault) => vault.resolve(reference).await,
                None => Err(ExtError::BadSecretRef(reference.to_string())),
            };
        }
        self.files.resolve(reference).await
    }
}
