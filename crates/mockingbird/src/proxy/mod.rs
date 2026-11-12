//! `mockingbird proxy`: an HTTP proxy that intercepts HTTPS to hosts a mock
//! claims (TLS terminated with a leaf from the local CA, [`ca`]) and serves
//! them from the data plane, passes other hosts through or blocks them
//! (always blocking other hosts of a mocked vendor's domain unless
//! `proxy.vendor_subdomains` is `pass`, see [`siblings`]), and answers placeholder images for `proxy.images` hosts ([`images`]).

pub mod browser;
pub mod ca;
pub mod env;
pub mod forward;
pub mod images;
mod server;
pub mod siblings;
mod standalone;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::file_config::{Unmapped, VendorSubdomains};
use crate::registry::MockRegistry;

pub use server::{PAC_PATH, VENDOR_SUBDOMAIN_PROBLEM, pac_script, serve};
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

    /// Every served host (normalized).
    fn hosts(&self) -> Vec<String>;
}

impl HostClaims for MockRegistry {
    fn claims(&self, host: &str) -> bool {
        self.claims_host(host)
    }

    fn hosts(&self) -> Vec<String> {
        self.claimed_hosts()
    }
}

/// Prometheus counter of proxied requests to hosts no mock claims, by
/// `host` and `mode` (`pass` or `block`), plus `sibling_of` (the mocked
/// hosts of the same registrable domain, comma-separated) for a host
/// blocked as a vendor sibling; `mockingbird report` reads it.
/// Past [`MAX_UNMAPPED_HOSTS`] distinct hosts, new ones are counted under
/// [`OTHER_HOSTS_LABEL`].
pub const UNMAPPED_METRIC: &str = "mb_proxy_unmapped_requests_total";

/// Distinct unmapped hosts the proxy remembers (each logged once and
/// counted under its own `host` label in [`UNMAPPED_METRIC`]).
pub const MAX_UNMAPPED_HOSTS: usize = 500;

/// The `host` label of [`UNMAPPED_METRIC`] for unmapped hosts past
/// [`MAX_UNMAPPED_HOSTS`].
pub const OTHER_HOSTS_LABEL: &str = "other";

/// What the proxy serves with.
pub struct ProxyState {
    pub(crate) claims: Arc<dyn HostClaims>,
    /// Serves intercepted requests to claimed hosts, `Host` intact.
    pub(crate) upstream: axum::Router,
    pub(crate) ca: Arc<ca::LocalCa>,
    pub(crate) unmapped: Unmapped,
    pub(crate) vendor_subdomains: VendorSubdomains,
    /// What is answered with placeholder images.
    pub(crate) images: Vec<images::ImageRule>,
    /// Forwards plain requests to unmapped hosts.
    pub(crate) client: reqwest::Client,
    /// Unmapped hosts seen so far, at most [`MAX_UNMAPPED_HOSTS`].
    seen: Mutex<HashSet<String>>,
    /// Whether reaching [`MAX_UNMAPPED_HOSTS`] was logged.
    seen_full: AtomicBool,
}

impl std::fmt::Debug for ProxyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyState")
            .field("unmapped", &self.unmapped)
            .field("vendor_subdomains", &self.vendor_subdomains)
            .field("images", &self.images)
            .finish_non_exhaustive()
    }
}

impl ProxyState {
    /// The proxy's state: `claims` decides which hosts are intercepted and
    /// `upstream` serves them; `unmapped` and `vendor_subdomains` decide
    /// what happens to other hosts; `images` are the `proxy.images` entries.
    ///
    /// # Errors
    ///
    /// Fails when an `images` entry is invalid (see
    /// [`images::ImageRule::parse`]) or the forwarding client cannot be built.
    pub fn new(
        claims: Arc<dyn HostClaims>,
        upstream: axum::Router,
        ca: Arc<ca::LocalCa>,
        unmapped: Unmapped,
        vendor_subdomains: VendorSubdomains,
        images: Vec<String>,
    ) -> anyhow::Result<Arc<ProxyState>> {
        // No fallback to a default client: it would read `HTTP_PROXY`,
        // which may point back at this proxy.
        let client = forward::forward_client()?;
        let images = images
            .iter()
            .map(|entry| images::ImageRule::parse(entry).map_err(|e| anyhow::anyhow!(e)))
            .collect::<anyhow::Result<_>>()?;
        Ok(Arc::new(ProxyState {
            claims,
            upstream,
            ca,
            unmapped,
            vendor_subdomains,
            images,
            client,
            seen: Mutex::new(HashSet::new()),
            seen_full: AtomicBool::new(false),
        }))
    }

    /// `true` the first time `host` is seen, so each unmapped host is
    /// logged once. Only the first [`MAX_UNMAPPED_HOSTS`] distinct hosts
    /// are remembered; a new host past them is `false`.
    pub fn first_sighting(&self, host: &str) -> bool {
        self.sight(host).1
    }

    /// The `host` label a request to unmapped `host` is counted under in
    /// [`UNMAPPED_METRIC`]: the host itself while it is (or would be) one
    /// of the first [`MAX_UNMAPPED_HOSTS`] seen, else [`OTHER_HOSTS_LABEL`].
    pub fn unmapped_label(&self, host: &str) -> String {
        let tracked = self
            .seen
            .lock()
            .map(|seen| seen.contains(host) || seen.len() < MAX_UNMAPPED_HOSTS)
            .unwrap_or(false);
        if tracked {
            host.to_owned()
        } else {
            OTHER_HOSTS_LABEL.to_owned()
        }
    }

    /// `(tracked, first)`: whether `host` is one of the remembered hosts,
    /// and whether this call added it.
    fn sight(&self, host: &str) -> (bool, bool) {
        let Ok(mut seen) = self.seen.lock() else {
            return (false, false);
        };
        if seen.contains(host) {
            (true, false)
        } else if seen.len() < MAX_UNMAPPED_HOSTS {
            seen.insert(host.to_owned());
            (true, true)
        } else {
            (false, false)
        }
    }

    /// The local CA's certificate path.
    pub fn ca_cert_path(&self) -> std::path::PathBuf {
        self.ca.cert_path()
    }

    /// Whether a request for `path` on `host` gets a placeholder image.
    pub(crate) fn is_image(&self, host: &str, path: &str) -> bool {
        self.images.iter().any(|rule| rule.matches(host, path))
    }

    /// Whether `host` is intercepted: a `proxy.images` host or one a mock
    /// claims.
    pub(crate) fn is_mapped(&self, host: &str) -> bool {
        self.images.iter().any(|rule| rule.host == host) || self.claims.claims(host)
    }

    /// The hosts a mock claims that share unmapped `host`'s registrable
    /// domain, when `proxy.vendor_subdomains` blocks such hosts; empty
    /// otherwise (and for a host of no mocked vendor's domain).
    pub(crate) fn blocking_siblings(&self, host: &str) -> Vec<String> {
        if self.vendor_subdomains == VendorSubdomains::Pass {
            return Vec::new();
        }
        let mapped = self.claims.hosts();
        siblings::siblings_of(host, mapped.iter().map(String::as_str))
    }

    /// Logs `host` once as unmapped and counts the request in
    /// [`UNMAPPED_METRIC`]; `sibling_of` are the mocked hosts of its
    /// domain when it is blocked as a vendor sibling (mode `block`).
    pub(crate) fn note_unmapped(&self, host: &str, sibling_of: &[String]) {
        let (tracked, first) = self.sight(host);
        let label = if tracked {
            host.to_owned()
        } else {
            OTHER_HOSTS_LABEL.to_owned()
        };
        let mode = if sibling_of.is_empty() {
            self.unmapped.as_str()
        } else {
            Unmapped::Block.as_str()
        };
        if sibling_of.is_empty() {
            metrics::counter!(UNMAPPED_METRIC, "host" => label, "mode" => mode).increment(1);
        } else {
            metrics::counter!(UNMAPPED_METRIC, "host" => label, "mode" => mode, "sibling_of" => sibling_of.join(","))
                .increment(1);
        }
        if !tracked && !self.seen_full.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                limit = MAX_UNMAPPED_HOSTS,
                "proxy: more than {MAX_UNMAPPED_HOSTS} unmapped hosts; new ones are not logged and are counted as host \"{OTHER_HOSTS_LABEL}\""
            );
        }
        if first && sibling_of.is_empty() {
            tracing::info!(host, mode, "proxy: unmapped host {host} ({mode})");
        } else if first {
            let mapped = sibling_of.join(", ");
            tracing::warn!(
                host,
                mode,
                sibling_of = %mapped,
                "proxy: unmapped host {host} shares a domain with mocked {mapped}; blocked (proxy.vendor_subdomains)"
            );
        }
    }
}
