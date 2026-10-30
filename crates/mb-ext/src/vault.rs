//! `vault:` secret references (HashiCorp Vault KV v2) and [`DefaultSecrets`],
//! the self-hosted [`SecretStore`] that combines them with `env:`/`file:`.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;

use crate::{EnvFileSecrets, ExtError, SecretStore};

/// How long a resolved Vault value is served from memory.
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// Timeout for one Vault request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Outbound HTTP settings for the Vault client: a proxy, the hosts reached
/// without it and extra trusted CA certificates. The server passes its
/// `outbound.proxy`, `outbound.no_proxy` and `outbound.ca_file` here. The
/// default changes nothing: reqwest's own proxy handling (the
/// `HTTP(S)_PROXY`/`NO_PROXY` environment) and the default roots, plus
/// `VAULT_CACERT`.
///
/// `Debug` shows whether a proxy is set, never its URL (it may carry
/// credentials).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct VaultHttp {
    proxy: Option<String>,
    no_proxy: Vec<String>,
    extra_roots_pem: Vec<u8>,
}

impl fmt::Debug for VaultHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaultHttp")
            .field("proxy", &self.proxy.as_ref().map(|_| "<set>"))
            .field("no_proxy", &self.no_proxy)
            .field("extra_roots_pem_bytes", &self.extra_roots_pem.len())
            .finish()
    }
}

impl VaultHttp {
    /// No proxy override and no extra roots.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends every Vault request through this `http(s)://` proxy, instead
    /// of following the `HTTP(S)_PROXY` environment.
    #[must_use]
    pub fn with_proxy(mut self, url: impl Into<String>) -> Self {
        self.proxy = Some(url.into());
        self
    }

    /// Hosts, addresses or `.domain` suffixes reached without the proxy of
    /// [`VaultHttp::with_proxy`].
    #[must_use]
    pub fn with_no_proxy(mut self, hosts: Vec<String>) -> Self {
        self.no_proxy = hosts;
        self
    }

    /// PEM certificates trusted in addition to the default roots (and to
    /// `VAULT_CACERT`).
    #[must_use]
    pub fn with_extra_roots_pem(mut self, pem: Vec<u8>) -> Self {
        self.extra_roots_pem = pem;
        self
    }

    fn apply(
        &self,
        mut builder: reqwest::ClientBuilder,
    ) -> Result<reqwest::ClientBuilder, ExtError> {
        if !self.extra_roots_pem.is_empty() {
            let certs = reqwest::Certificate::from_pem_bundle(&self.extra_roots_pem)
                .ok()
                .filter(|certs| !certs.is_empty())
                .ok_or_else(|| {
                    ExtError::Other(
                        "the outbound CA bundle for Vault holds no PEM certificate".to_string(),
                    )
                })?;
            builder = builder.tls_certs_merge(certs);
        }
        if let Some(url) = &self.proxy {
            // The error never names the URL, which may carry credentials.
            let mut proxy = reqwest::Proxy::all(url).map_err(|_| {
                ExtError::Other("the outbound proxy for Vault is not a valid URL".to_string())
            })?;
            if !self.no_proxy.is_empty() {
                proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&self.no_proxy.join(",")));
            }
            builder = builder.proxy(proxy);
        }
        Ok(builder)
    }
}

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
/// A proxy and further CA roots come from a [`VaultHttp`]
/// ([`VaultSecrets::from_vars_with_http`]); without one, reqwest follows the
/// `HTTP(S)_PROXY`/`NO_PROXY` environment.
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
    /// Per reference with a read under way: the lock the misses on it queue
    /// on, so concurrent misses share one Vault read.
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
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
    ///
    /// # Errors
    ///
    /// [`ExtError::Other`], naming the variable at fault, when `VAULT_ADDR` is
    /// not an `http(s)` URL, neither `VAULT_TOKEN` nor `VAULT_TOKEN_FILE` is
    /// set, `VAULT_CACERT` cannot be read or parsed, or the HTTP client
    /// cannot be built.
    pub fn from_env() -> Result<Option<Self>, ExtError> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// Like [`VaultSecrets::from_env`], reading the variables through `var`
    /// instead of the process environment. Empty values count as unset.
    ///
    /// # Errors
    ///
    /// As [`VaultSecrets::from_env`].
    ///
    /// ```
    /// use mb_ext::VaultSecrets;
    ///
    /// // Without VAULT_ADDR, Vault support is off.
    /// assert!(VaultSecrets::from_vars(|_| None)?.is_none());
    /// # Ok::<(), mb_ext::ExtError>(())
    /// ```
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, ExtError> {
        Self::from_vars_with_http(var, &VaultHttp::default())
    }

    /// Like [`VaultSecrets::from_vars`], with the client's proxy and extra
    /// CA roots from `http`. A CA bundle without a certificate or a proxy
    /// that is not a URL is an error.
    ///
    /// # Errors
    ///
    /// As [`VaultSecrets::from_env`], and [`ExtError::Other`] when `http`
    /// holds a CA bundle without a certificate or a proxy that is not a URL.
    pub fn from_vars_with_http(
        var: impl Fn(&str) -> Option<String>,
        http: &VaultHttp,
    ) -> Result<Option<Self>, ExtError> {
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
        let mut builder = http.apply(reqwest::Client::builder().timeout(REQUEST_TIMEOUT))?;
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
            inflight: Mutex::new(HashMap::new()),
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
        // One read per reference at a time: a miss that queued behind
        // another finds the value it cached. A failure is not cached, so
        // the next one in line reads again.
        let queued = Queued::join(&self.inflight, reference);
        let _read = queued.lock.clone().lock_owned().await;
        if let Some(value) = self.cached(reference) {
            return Ok(value);
        }
        let value = self.fetch(reference, &parsed).await?;
        let now = Instant::now();
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|_, (_, expires)| now < *expires);
        cache.insert(reference.to_string(), (value.clone(), now + CACHE_TTL));
        Ok(value)
    }
}

/// A place in the queue of misses on one reference. The last one to leave
/// (or be dropped, if its resolve is cancelled) removes the queue.
struct Queued<'a> {
    inflight: &'a Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    reference: &'a str,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl<'a> Queued<'a> {
    fn join(
        inflight: &'a Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
        reference: &'a str,
    ) -> Self {
        let mut map = inflight.lock().unwrap_or_else(|e| e.into_inner());
        let lock = map.entry(reference.to_string()).or_default().clone();
        Self {
            inflight,
            reference,
            lock,
        }
    }
}

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        // The map's handle and this one: nobody else is queued.
        if map
            .get(self.reference)
            .is_some_and(|l| Arc::ptr_eq(l, &self.lock) && Arc::strong_count(l) == 2)
        {
            map.remove(self.reference);
        }
    }
}

/// The Vault variables [`DefaultSecrets`] snapshots at construction.
const VAULT_VARS: [&str; 5] = [
    "VAULT_ADDR",
    "VAULT_TOKEN",
    "VAULT_TOKEN_FILE",
    "VAULT_NAMESPACE",
    "VAULT_CACERT",
];

/// The self-hosted [`SecretStore`]: `env:` and `file:` through
/// [`EnvFileSecrets`], and `vault:` through [`VaultSecrets`] when `VAULT_ADDR`
/// is set. A malformed `vault:` reference is [`ExtError::BadSecretRef`]; a
/// well-formed one without a usable Vault (`VAULT_ADDR` unset, or a Vault
/// configuration that cannot be used) is [`ExtError::Other`], naming the
/// reason.
///
/// Construction only snapshots the `VAULT_*` variables: the Vault client
/// (CA bundle, HTTP client) is built on the first `vault:` resolve, so
/// building a `DefaultSecrets` (and so [`crate::Extensions::default`]) does
/// no I/O and logs nothing. A Vault configuration that cannot be used is
/// logged once, on that first resolve, and Vault is disabled.
pub struct DefaultSecrets {
    files: EnvFileSecrets,
    vars: Vec<(&'static str, String)>,
    http: VaultHttp,
    /// The Vault client, or why there is none.
    vault: tokio::sync::OnceCell<Result<VaultSecrets, String>>,
}

impl fmt::Debug for DefaultSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.vars.iter().map(|(name, _)| *name).collect();
        f.debug_struct("DefaultSecrets")
            .field("vault_vars", &names)
            .field("http", &self.http)
            .field("vault", &self.vault.get())
            .finish_non_exhaustive()
    }
}

impl DefaultSecrets {
    /// Reads the Vault configuration from the process environment. A Vault
    /// configuration that cannot be used is logged (on the first `vault:`
    /// resolve) and Vault is disabled.
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// Like [`DefaultSecrets::from_env`], reading the variables through `var`
    /// instead of the process environment.
    ///
    /// ```
    /// use mb_ext::{DefaultSecrets, Extensions};
    ///
    /// // A fixed Vault configuration, whatever the process environment holds.
    /// let vars = [("VAULT_ADDR", "https://vault.example:8200"), ("VAULT_TOKEN_FILE", "/run/vault/token")];
    /// let secrets = DefaultSecrets::from_vars(|name| {
    ///     vars.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string())
    /// });
    /// let extensions = Extensions::default().with_secrets(std::sync::Arc::new(secrets));
    /// # let _ = extensions;
    /// ```
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Self {
        let vars = VAULT_VARS
            .iter()
            .filter_map(|name| var(name).map(|value| (*name, value)))
            .collect();
        Self {
            files: EnvFileSecrets,
            vars,
            http: VaultHttp::default(),
            vault: tokio::sync::OnceCell::new(),
        }
    }

    /// Builds the Vault client with the proxy and extra CA roots of `http`
    /// (see [`VaultSecrets::from_vars_with_http`]). An unusable setting is
    /// logged on the first `vault:` resolve and Vault is disabled, like any
    /// other unusable Vault configuration.
    #[must_use]
    pub fn with_http(mut self, http: VaultHttp) -> Self {
        self.http = http;
        self
    }

    /// The Vault client, built (off the async workers) on first use, or
    /// why Vault is unavailable.
    async fn vault(&self) -> Result<&VaultSecrets, &str> {
        self.vault
            .get_or_init(|| async {
                let vars = self.vars.clone();
                let lookup = move |name: &str| {
                    vars.iter()
                        .find(|(n, _)| *n == name)
                        .map(|(_, v)| v.clone())
                };
                let http = self.http.clone();
                let built = tokio::task::spawn_blocking(move || {
                    VaultSecrets::from_vars_with_http(lookup, &http)
                })
                .await
                .unwrap_or_else(|e| {
                    Err(ExtError::Other(format!("building the Vault client: {e}")))
                });
                match built {
                    Ok(Some(vault)) => Ok(vault),
                    Ok(None) => Err("VAULT_ADDR is not set".to_string()),
                    Err(e) => {
                        tracing::warn!(error = %e, "Vault secret references disabled");
                        Err(format!("Vault is disabled: {e}"))
                    }
                }
            })
            .await
            .as_ref()
            .map_err(String::as_str)
    }
}

#[async_trait]
impl SecretStore for DefaultSecrets {
    async fn resolve(&self, reference: &str) -> Result<String, ExtError> {
        if reference.starts_with("vault:") {
            if parse_ref(reference).is_none() {
                return Err(ExtError::BadSecretRef(reference.to_string()));
            }
            return match self.vault().await {
                Ok(vault) => vault.resolve(reference).await,
                Err(reason) => Err(ExtError::Other(format!(
                    "cannot resolve {reference}: {reason}"
                ))),
            };
        }
        self.files.resolve(reference).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{CACHE_TTL, VaultSecrets};
    use crate::SecretStore;

    /// Expired values leave the cache when a new one is stored, and no
    /// queue outlives its resolve.
    #[tokio::test]
    async fn expired_values_are_evicted_and_queues_removed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "data": { "k": "v" } }
            })))
            .mount(&server)
            .await;
        let addr = server.uri();
        let vault = VaultSecrets::from_vars(|name| match name {
            "VAULT_ADDR" => Some(addr.clone()),
            "VAULT_TOKEN" => Some("t".to_string()),
            _ => None,
        })
        .expect("config")
        .expect("VAULT_ADDR is set");
        let cached = |vault: &VaultSecrets| {
            let mut keys: Vec<String> =
                vault.cache.lock().expect("cache").keys().cloned().collect();
            keys.sort();
            keys
        };

        for reference in ["vault:secret/a#k", "vault:secret/b#k"] {
            assert_eq!(vault.resolve(reference).await.ok().as_deref(), Some("v"));
        }
        assert_eq!(cached(&vault), ["vault:secret/a#k", "vault:secret/b#k"]);
        assert!(vault.inflight.lock().expect("inflight").is_empty());

        tokio::time::pause();
        tokio::time::advance(CACHE_TTL + Duration::from_secs(1)).await;
        tokio::time::resume();
        assert_eq!(
            vault.resolve("vault:secret/c#k").await.ok().as_deref(),
            Some("v")
        );
        assert_eq!(cached(&vault), ["vault:secret/c#k"]);
        assert!(vault.inflight.lock().expect("inflight").is_empty());
    }
}
