//! SSRF-guarded URL fetch.
//!
//! Every hop (the first request and each redirect) is validated, resolved
//! exactly once and checked against the blocked address ranges before any
//! connection is made; the resolved addresses are then pinned into the HTTP
//! client so the name cannot be re-resolved to a different address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION, USER_AGENT};
use url::{Host, Url};

/// Largest accepted response body.
pub const MAX_FETCH_BYTES: usize = 10 * 1024 * 1024;
/// Time limit of the whole fetch, redirects included.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Most redirects followed; each hop is re-validated.
pub const MAX_REDIRECTS: usize = 5;

const ACCEPT_VALUE: &str =
    "text/html, text/markdown, application/pdf, application/json;q=0.9, */*;q=0.5";
const USER_AGENT_VALUE: &str = concat!("mockingbird-import/", env!("CARGO_PKG_VERSION"));

const BLOCKED_V4: [Ipv4Net; 14] = [
    Ipv4Net::new_assert(Ipv4Addr::new(0, 0, 0, 0), 8),
    Ipv4Net::new_assert(Ipv4Addr::new(10, 0, 0, 0), 8),
    Ipv4Net::new_assert(Ipv4Addr::new(100, 64, 0, 0), 10),
    Ipv4Net::new_assert(Ipv4Addr::new(127, 0, 0, 0), 8),
    Ipv4Net::new_assert(Ipv4Addr::new(169, 254, 0, 0), 16),
    Ipv4Net::new_assert(Ipv4Addr::new(172, 16, 0, 0), 12),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 0, 0, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 0, 2, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 168, 0, 0), 16),
    Ipv4Net::new_assert(Ipv4Addr::new(198, 18, 0, 0), 15),
    Ipv4Net::new_assert(Ipv4Addr::new(198, 51, 100, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(203, 0, 113, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(224, 0, 0, 0), 4),
    Ipv4Net::new_assert(Ipv4Addr::new(240, 0, 0, 0), 4),
];

const BLOCKED_V6: [Ipv6Net; 7] = [
    Ipv6Net::new_assert(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 128),
    Ipv6Net::new_assert(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1), 128),
    Ipv6Net::new_assert(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    Ipv6Net::new_assert(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    Ipv6Net::new_assert(Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),
    Ipv6Net::new_assert(Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
    Ipv6Net::new_assert(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
];

/// IPv4-mapped addresses: the embedded IPv4 address is checked.
const V4_MAPPED: Ipv6Net = Ipv6Net::new_assert(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96);
/// NAT64 well-known prefix: the embedded IPv4 address is checked.
const NAT64: Ipv6Net = Ipv6Net::new_assert(Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96);

/// One `import.url_allowlist` entry.
#[derive(Clone, Debug, PartialEq)]
pub enum AllowEntry {
    /// An exact host name (lower-case).
    Host(String),
    /// `*.suffix`: any subdomain of the suffix (lower-case, without `*.`).
    Suffix(String),
    /// An address range; a bare address is a /32 or /128.
    Net(IpNet),
}

impl FromStr for AllowEntry {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let entry = s.trim();
        if entry.is_empty() {
            return Err("an empty allowlist entry".into());
        }
        if entry.contains('/') {
            return entry
                .parse::<IpNet>()
                .map(AllowEntry::Net)
                .map_err(|_| format!("{entry:?} is not a valid CIDR range"));
        }
        if let Ok(ip) = entry.parse::<IpAddr>() {
            return Ok(AllowEntry::Net(IpNet::from(ip)));
        }
        if let Some(suffix) = entry.strip_prefix("*.") {
            return valid_host_name(suffix)
                .map(AllowEntry::Suffix)
                .ok_or_else(|| format!("{entry:?} is not a valid host suffix"));
        }
        valid_host_name(entry)
            .map(AllowEntry::Host)
            .ok_or_else(|| format!("{entry:?} is not a valid host name"))
    }
}

/// The lower-cased name when every label is non-empty and made of ASCII
/// letters, digits, `-` and `_`.
fn valid_host_name(name: &str) -> Option<String> {
    let name = name.to_ascii_lowercase();
    let ok = name.split('.').all(|label| {
        !label.is_empty()
            && label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    });
    ok.then_some(name)
}

/// Limits and exemptions of a fetch.
#[derive(Clone, Debug)]
pub struct FetchPolicy {
    pub allowlist: Vec<AllowEntry>,
    pub max_bytes: usize,
    pub timeout: Duration,
    pub max_redirects: usize,
    /// PEM certificates trusted in addition to the platform's roots
    /// (`outbound.ca_file`). The fetch never uses a proxy.
    pub extra_roots_pem: Vec<u8>,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self {
            allowlist: Vec::new(),
            max_bytes: MAX_FETCH_BYTES,
            timeout: FETCH_TIMEOUT,
            max_redirects: MAX_REDIRECTS,
            extra_roots_pem: Vec::new(),
        }
    }
}

impl FetchPolicy {
    /// The default policy with the given allowlist entries.
    pub fn from_allowlist(entries: &[String]) -> Result<Self, String> {
        let allowlist = entries
            .iter()
            .map(|e| e.parse::<AllowEntry>())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            allowlist,
            ..Self::default()
        })
    }

    /// Whether a `Host`/`Suffix` entry names this host, which exempts every
    /// address it resolves to.
    pub fn host_allowed(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.allowlist.iter().any(|entry| match entry {
            AllowEntry::Host(name) => *name == host,
            AllowEntry::Suffix(suffix) => host
                .strip_suffix(suffix.as_str())
                .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.')),
            AllowEntry::Net(_) => false,
        })
    }

    /// Whether a `Net` entry contains this address.
    fn ip_allowed(&self, ip: IpAddr) -> bool {
        let embedded = embedded_v4(ip).map(IpAddr::V4);
        self.allowlist.iter().any(|entry| match entry {
            AllowEntry::Net(net) => {
                net.contains(&ip) || embedded.is_some_and(|v4| net.contains(&v4))
            }
            _ => false,
        })
    }
}

/// Name resolution used by [`fetch_with`].
#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>>;
}

/// The system resolver (`tokio::net::lookup_host`).
pub struct SystemResolver;

#[async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }
}

/// A fetched document.
#[derive(Debug)]
pub struct Fetched {
    /// The final URL, after redirects.
    pub url: Url,
    /// The media type, lower-case, without parameters.
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

#[derive(thiserror::Error, Debug)]
pub enum FetchError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// The host or a redirect resolves to a private or reserved address.
    /// The message names neither, so URL imports cannot probe internal
    /// names; the fetch logs both.
    #[error(
        "destination not allowed: the URL or a redirect leads to an address imports may not fetch \
         (an administrator can allow it with import.url_allowlist)"
    )]
    Blocked { host: String, ip: IpAddr },
    #[error("cannot resolve {0}")]
    Resolve(String),
    #[error("more than {0} redirects")]
    TooManyRedirects(usize),
    #[error("the response exceeds {0} bytes")]
    TooLarge(usize),
    #[error("the fetch timed out")]
    Timeout,
    #[error("the server answered HTTP {0}")]
    Http(u16),
    #[error("transport error: {0}")]
    Transport(String),
}

/// Parses a URL and accepts only http/https URLs with a host and without
/// userinfo.
pub fn validate_url(raw: &str) -> Result<Url, FetchError> {
    let url = Url::parse(raw.trim()).map_err(|e| FetchError::InvalidUrl(format!("{raw}: {e}")))?;
    check_url(url)
}

fn check_url(url: Url) -> Result<Url, FetchError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(FetchError::InvalidUrl(format!(
            "{url}: only http and https URLs can be fetched"
        )));
    }
    if url.host().is_none() {
        return Err(FetchError::InvalidUrl(format!(
            "{url}: the URL has no host"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FetchError::InvalidUrl(
            "URLs with credentials cannot be fetched".into(),
        ));
    }
    Ok(url)
}

/// The IPv4 address embedded in an IPv4-mapped or NAT64 IPv6 address.
fn embedded_v4(ip: IpAddr) -> Option<Ipv4Addr> {
    match ip {
        IpAddr::V6(v6) if V4_MAPPED.contains(&v6) || NAT64.contains(&v6) => {
            let o = v6.octets();
            Some(Ipv4Addr::new(o[12], o[13], o[14], o[15]))
        }
        _ => None,
    }
}

/// Whether the address is private, loopback, link-local, reserved,
/// documentation or multicast (IPv4-mapped and NAT64 addresses are judged by
/// their embedded IPv4 address).
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    if let Some(v4) = embedded_v4(ip) {
        return BLOCKED_V4.iter().any(|net| net.contains(&v4));
    }
    match ip {
        IpAddr::V4(v4) => BLOCKED_V4.iter().any(|net| net.contains(&v4)),
        IpAddr::V6(v6) => BLOCKED_V6.iter().any(|net| net.contains(&v6)),
    }
}

/// Fetches a URL with the system resolver.
pub async fn fetch(raw: &str, policy: &FetchPolicy) -> Result<Fetched, FetchError> {
    fetch_with(raw, policy, &SystemResolver).await
}

/// Fetches a URL, following up to `policy.max_redirects` redirects, each hop
/// validated, resolved once through `resolver` and checked against the
/// blocked ranges.
pub async fn fetch_with(
    raw: &str,
    policy: &FetchPolicy,
    resolver: &dyn Resolver,
) -> Result<Fetched, FetchError> {
    tokio::time::timeout(policy.timeout, fetch_hops(raw, policy, resolver))
        .await
        .map_err(|_| FetchError::Timeout)?
}

async fn fetch_hops(
    raw: &str,
    policy: &FetchPolicy,
    resolver: &dyn Resolver,
) -> Result<Fetched, FetchError> {
    let mut url = validate_url(raw)?;
    let mut redirects = 0usize;
    loop {
        let client = pinned_client(&url, policy, resolver).await?;
        let mut response = client
            .get(url.clone())
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, ACCEPT_VALUE)
            .send()
            .await
            .map_err(transport)?;
        let status = response.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308)
            && let Some(location) = response.headers().get(LOCATION)
        {
            let location = location
                .to_str()
                .map_err(|_| FetchError::InvalidUrl("a redirect Location is not text".into()))?;
            let next = url
                .join(location)
                .map_err(|e| FetchError::InvalidUrl(format!("{location}: {e}")))?;
            redirects += 1;
            if redirects > policy.max_redirects {
                return Err(FetchError::TooManyRedirects(policy.max_redirects));
            }
            url = check_url(next)?;
            continue;
        }
        if !response.status().is_success() {
            return Err(FetchError::Http(status));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty());
        if response
            .content_length()
            .is_some_and(|len| len > policy.max_bytes as u64)
        {
            return Err(FetchError::TooLarge(policy.max_bytes));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            if bytes.len() + chunk.len() > policy.max_bytes {
                return Err(FetchError::TooLarge(policy.max_bytes));
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(Fetched {
            url,
            content_type,
            bytes,
        });
    }
}

/// Checks the hop's addresses and builds a client pinned to them.
async fn pinned_client(
    url: &Url,
    policy: &FetchPolicy,
    resolver: &dyn Resolver,
) -> Result<reqwest::Client, FetchError> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if !policy.extra_roots_pem.is_empty() {
        let roots =
            reqwest::Certificate::from_pem_bundle(&policy.extra_roots_pem).map_err(transport)?;
        builder = builder.tls_certs_merge(roots);
    }
    let builder = match url.host() {
        Some(Host::Ipv4(v4)) => {
            check_addr(&v4.to_string(), IpAddr::V4(v4), false, policy)?;
            builder
        }
        Some(Host::Ipv6(v6)) => {
            check_addr(&v6.to_string(), IpAddr::V6(v6), false, policy)?;
            builder
        }
        Some(Host::Domain(host)) => {
            let port = url.port_or_known_default().unwrap_or(80);
            let addrs = resolver
                .resolve(host, port)
                .await
                .map_err(|_| FetchError::Resolve(host.to_string()))?;
            if addrs.is_empty() {
                return Err(FetchError::Resolve(host.to_string()));
            }
            let host_allowed = policy.host_allowed(host);
            for addr in &addrs {
                check_addr(host, addr.ip(), host_allowed, policy)?;
            }
            builder.resolve_to_addrs(host, &addrs)
        }
        None => {
            return Err(FetchError::InvalidUrl(format!(
                "{url}: the URL has no host"
            )));
        }
    };
    builder.build().map_err(transport)
}

fn check_addr(
    host: &str,
    ip: IpAddr,
    host_allowed: bool,
    policy: &FetchPolicy,
) -> Result<(), FetchError> {
    if host_allowed || !is_blocked_ip(ip) || policy.ip_allowed(ip) {
        Ok(())
    } else {
        tracing::warn!(%host, %ip, "import fetch blocked: private or reserved address");
        Err(FetchError::Blocked {
            host: host.to_string(),
            ip,
        })
    }
}

fn transport(err: reqwest::Error) -> FetchError {
    // The URL may carry secrets (a signed query) and is the caller's own.
    let err = err.without_url();
    let mut message = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    FetchError::Transport(message)
}
