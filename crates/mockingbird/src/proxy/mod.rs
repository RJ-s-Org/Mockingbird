//! `mockingbird proxy`: an HTTP proxy that intercepts HTTPS to hosts a mock
//! claims (TLS terminated with a leaf from the local CA, [`ca`]) and serves
//! them from the data plane, passes other hosts through or blocks them,
//! and answers placeholder images for `proxy.images` hosts ([`images`]).

pub mod ca;
pub mod env;
pub mod forward;
pub mod images;
mod server;
mod standalone;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::file_config::Unmapped;
use crate::registry::MockRegistry;

pub use server::serve;
pub use standalone::{
    CaCommand, ProxyArgs, ProxyCommand, ProxyEnvArgs, ProxyRunArgs, StandaloneOptions, ca_command,
    run, run_standalone,
};

/// The proxy's port unless `proxy.port` or `--port` says otherwise.
pub const DEFAULT_PROXY_PORT: u16 = 8888;

/// Which hosts the data plane serves (a mock lists them in `hosts`).
pub trait HostClaims: Send + Sync + 'static {
    /// Whether `host` (normalized, see
    /// [`normalize_host`](crate::registry::normalize_host)) is served.
    fn claims(&self, host: &str) -> bool;
}

impl HostClaims for MockRegistry {
    fn claims(&self, host: &str) -> bool {
        self.claims_host(host)
    }
}

/// What the proxy serves with.
pub struct ProxyState {
    pub(crate) claims: Arc<dyn HostClaims>,
    /// Serves intercepted requests to claimed hosts, `Host` intact.
    pub(crate) upstream: axum::Router,
    pub(crate) ca: Arc<ca::LocalCa>,
    pub(crate) unmapped: Unmapped,
    /// Hosts answered with placeholder images.
    pub(crate) images: Vec<String>,
    /// Forwards plain requests to unmapped hosts.
    pub(crate) client: reqwest::Client,
    seen: Mutex<HashSet<String>>,
}

impl ProxyState {
    pub fn new(
        claims: Arc<dyn HostClaims>,
        upstream: axum::Router,
        ca: Arc<ca::LocalCa>,
        unmapped: Unmapped,
        images: Vec<String>,
    ) -> anyhow::Result<Arc<ProxyState>> {
        // No fallback to a default client: it would read `HTTP_PROXY`,
        // which may point back at this proxy.
        let client = forward::forward_client()?;
        Ok(Arc::new(ProxyState {
            claims,
            upstream,
            ca,
            unmapped,
            images,
            client,
            seen: Mutex::new(HashSet::new()),
        }))
    }

    /// `true` the first time `host` is seen, so each unmapped host is
    /// logged once.
    pub fn first_sighting(&self, host: &str) -> bool {
        self.seen
            .lock()
            .map(|mut seen| seen.insert(host.to_owned()))
            .unwrap_or(false)
    }

    /// The local CA's certificate path.
    pub fn ca_cert_path(&self) -> std::path::PathBuf {
        self.ca.cert_path()
    }

    /// Whether `host` gets placeholder images.
    pub(crate) fn is_image_host(&self, host: &str) -> bool {
        self.images.iter().any(|h| h == host)
    }

    /// Whether `host` is intercepted: an image host or one a mock claims.
    pub(crate) fn is_mapped(&self, host: &str) -> bool {
        self.is_image_host(host) || self.claims.claims(host)
    }

    /// Logs `host` once as unmapped.
    pub(crate) fn note_unmapped(&self, host: &str) {
        if self.first_sighting(host) {
            tracing::info!(
                host,
                mode = self.unmapped.as_str(),
                "proxy: unmapped host {host} ({})",
                self.unmapped.as_str()
            );
        }
    }
}
