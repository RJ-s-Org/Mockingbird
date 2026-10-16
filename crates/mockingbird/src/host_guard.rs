//! DNS-rebinding protection for the admin port.
//!
//! A browser page can reach the admin port through a DNS name that rebinds
//! to its address; such a request carries the attacker's host name in
//! `Host`. The guard answers 421 unless `Host` is one the admin port is
//! meant to be reached at ([`HostPolicy`]). The ops endpoints
//! ([`EXEMPT_PATHS`]) are exempt, so probes that send a pod IP as `Host`
//! keep working; they expose nothing a rebinding page could abuse.

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use mb_admin::Problem;

/// Paths the guard never checks.
pub const EXEMPT_PATHS: [&str; 3] = ["/healthz", "/readyz", "/metrics"];

/// Which `Host` values the admin port answers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostPolicy {
    /// `localhost`, `127.0.0.1` or `[::1]`, with any port (`auth.mode:
    /// none` on a loopback bind).
    Loopback,
    /// Exactly this `host[:port]` (lowercased; compared caselessly): the
    /// host of `auth.external_url` in `oidc` mode.
    Exact(String),
}

impl HostPolicy {
    /// Whether `host` (a `Host` header value) is allowed.
    pub fn allows(&self, host: &str) -> bool {
        match self {
            HostPolicy::Loopback => is_loopback_host(host),
            HostPolicy::Exact(expected) => host.eq_ignore_ascii_case(expected),
        }
    }

    fn detail(&self) -> String {
        match self {
            HostPolicy::Loopback => {
                "the admin port is bound to a loopback address and only answers to \
                 Host localhost, 127.0.0.1 or [::1]"
                    .to_string()
            }
            HostPolicy::Exact(host) => {
                format!("the admin port only answers to Host {host} (auth.external_url)")
            }
        }
    }
}

/// Middleware: 421 problem unless the request's `Host` (or, without one,
/// its URI authority) is allowed by the policy. A request with neither is
/// let through (only non-browser clients send none), as is any request
/// for one of the [`EXEMPT_PATHS`].
pub async fn host_guard(State(policy): State<HostPolicy>, req: Request, next: Next) -> Response {
    if EXEMPT_PATHS.contains(&req.uri().path()) {
        return next.run(req).await;
    }
    let host = match req.headers().get(header::HOST) {
        Some(value) => Some(value.to_str().unwrap_or_default().to_string()),
        None => req.uri().authority().map(|a| a.as_str().to_string()),
    };
    match host {
        Some(host) if !policy.allows(&host) => Problem::titled(
            StatusCode::MISDIRECTED_REQUEST,
            "Misdirected Request",
            policy.detail(),
        )
        .into_response(),
        _ => next.run(req).await,
    }
}

/// Whether a `Host` value is `localhost`, `127.0.0.1` or `[::1]`, with an
/// optional numeric port.
pub fn is_loopback_host(host: &str) -> bool {
    let (name, port) = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((addr, "")) => (addr, None),
            Some((addr, tail)) => match tail.strip_prefix(':') {
                Some(port) => (addr, Some(port)),
                None => return false,
            },
            None => return false,
        }
    } else {
        match host.split_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (host, None),
        }
    };
    let port_ok = port.is_none_or(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    port_ok && (name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "::1")
}

#[cfg(test)]
mod tests {
    use super::{HostPolicy, is_loopback_host};

    #[test]
    fn exact_hosts() {
        let policy = HostPolicy::Exact("mb.example.com:9090".into());
        assert!(policy.allows("mb.example.com:9090"));
        assert!(policy.allows("MB.example.COM:9090"));
        assert!(!policy.allows("mb.example.com"));
        assert!(!policy.allows("localhost:9090"));
        assert!(!policy.allows("evil"));
    }

    #[test]
    fn loopback_hosts() {
        for ok in [
            "localhost",
            "LOCALHOST:1",
            "127.0.0.1",
            "127.0.0.1:9090",
            "[::1]",
            "[::1]:9090",
        ] {
            assert!(is_loopback_host(ok), "{ok}");
        }
        for bad in [
            "evil.example",
            "localhost.evil.example",
            "127.0.0.1.nip.io",
            "127.0.0.1:",
            "127.0.0.1:x",
            "[::1",
            "[::1]x",
            "[::2]",
            "::1",
            "",
        ] {
            assert!(!is_loopback_host(bad), "{bad}");
        }
    }
}
