//! TLS termination (`server.tls`) with certificate reload, and the
//! outbound HTTP settings (`outbound:`) every client leaving the process
//! uses.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, anyhow, bail};
use axum_server::tls_rustls::RustlsConfig;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::task::JoinHandle;

use crate::file_config::{FileConfig, OutboundConfig, TlsConfig};

/// How often [`spawn_reloader`] checks the certificate files.
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(60);

/// Reads `server.tls`'s certificate chain and key into a rustls server
/// configuration (aws-lc-rs, ALPN `h2` then `http/1.1`). An unreadable or
/// invalid file is an error naming its key and path.
pub async fn load_rustls(cfg: &TlsConfig) -> anyhow::Result<RustlsConfig> {
    Ok(RustlsConfig::from_config(Arc::new(
        server_config(cfg).await?,
    )))
}

async fn server_config(cfg: &TlsConfig) -> anyhow::Result<ServerConfig> {
    let cert_pem = read(&cfg.cert_file, "server.tls.cert_file").await?;
    let key_pem = read(&cfg.key_file, "server.tls.key_file").await?;
    let certs = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| key_path("server.tls.cert_file", &cfg.cert_file))?;
    if certs.is_empty() {
        bail!(
            "{}: no PEM certificate",
            key_path("server.tls.cert_file", &cfg.cert_file)
        );
    }
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|e| anyhow!("no PEM private key: {e}"))
        .with_context(|| key_path("server.tls.key_file", &cfg.key_file))?;
    server_config_from(certs, key).with_context(|| {
        format!(
            "server.tls: {} does not match {}",
            cfg.cert_file.display(),
            cfg.key_file.display()
        )
    })
}

/// A rustls server configuration serving `certs` (leaf first) with `key`:
/// aws-lc-rs, safe default protocol versions, ALPN `h2` then `http/1.1`.
pub fn server_config_from(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<ServerConfig> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("configuring TLS")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the certificate does not match the key")?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

fn key_path(key: &str, path: &Path) -> String {
    format!("{key} {}", path.display())
}

async fn read(path: &Path, key: &str) -> anyhow::Result<Vec<u8>> {
    tokio::fs::read(path)
        .await
        .with_context(|| format!("cannot read {}", key_path(key, path)))
}

/// Checks the certificate files every [`RELOAD_INTERVAL`] (see
/// [`spawn_reloader_every`]).
pub fn spawn_reloader(cfg: TlsConfig, rustls: RustlsConfig) -> JoinHandle<()> {
    spawn_reloader_every(cfg, rustls, RELOAD_INTERVAL)
}

/// Every `interval`, reloads `rustls` from `cfg`'s files when either
/// file's modification time changed since the last successful load. A bad
/// file logs an error and keeps the old certificates (it is tried again
/// on the next tick).
pub fn spawn_reloader_every(
    cfg: TlsConfig,
    rustls: RustlsConfig,
    interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut loaded = mtimes(&cfg).await;
        let mut ticks = tokio::time::interval(interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticks.tick().await;
        loop {
            ticks.tick().await;
            let now = mtimes(&cfg).await;
            if now == loaded {
                continue;
            }
            match server_config(&cfg).await {
                Ok(config) => {
                    rustls.reload_from_config(Arc::new(config));
                    loaded = now;
                    tracing::info!(
                        cert_file = %cfg.cert_file.display(),
                        "reloaded the TLS certificate"
                    );
                }
                Err(e) => tracing::error!(
                    error = %format!("{e:#}"),
                    "cannot reload the TLS certificate; keeping the old one"
                ),
            }
        }
    })
}

/// The modification times of the certificate and key files (`None` for a
/// file that cannot be read).
async fn mtimes(cfg: &TlsConfig) -> (Option<SystemTime>, Option<SystemTime>) {
    let mtime = |path: &Path| {
        let path = path.to_path_buf();
        async move {
            tokio::fs::metadata(path)
                .await
                .and_then(|m| m.modified())
                .ok()
        }
    };
    (mtime(&cfg.cert_file).await, mtime(&cfg.key_file).await)
}

/// The outbound HTTP options from `outbound:`, reading `outbound.ca_file`.
/// An unreadable file or one without a valid PEM certificate is an error
/// naming `outbound.ca_file`; so is a proxy reqwest rejects.
pub fn outbound_options(cfg: &OutboundConfig) -> anyhow::Result<mb_llm::HttpOptions> {
    let extra_roots_pem = match &cfg.ca_file {
        Some(path) => {
            let pem = std::fs::read(path)
                .with_context(|| format!("cannot read {}", key_path("outbound.ca_file", path)))?;
            match reqwest::Certificate::from_pem_bundle(&pem) {
                Ok(certs) if !certs.is_empty() => pem,
                Ok(_) => bail!("{}: no PEM certificate", key_path("outbound.ca_file", path)),
                Err(e) => bail!("{}: {e}", key_path("outbound.ca_file", path)),
            }
        }
        None => Vec::new(),
    };
    let opts = mb_llm::HttpOptions {
        extra_roots_pem,
        proxy: cfg.proxy.clone(),
        no_proxy: cfg.no_proxy.clone(),
    };
    // Builds one client so every later construction is known to succeed.
    mb_llm::http_client(&opts, None)
        .map_err(|e| anyhow!("{e}"))
        .context("outbound: building the HTTP client")?;
    Ok(opts)
}

/// With `server.tls`, `auth.external_url` must be an `https://` URL.
pub fn check_external_url(cfg: &FileConfig) -> Result<(), String> {
    if cfg.server.tls.is_some() && !cfg.auth.secure_cookies() {
        return Err(format!(
            "auth.external_url {:?} must be an https:// URL when server.tls is set",
            cfg.auth.external_url
        ));
    }
    Ok(())
}

/// Stops a certificate reloader when dropped.
pub struct ReloaderGuard(pub JoinHandle<()>);

impl Drop for ReloaderGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}
