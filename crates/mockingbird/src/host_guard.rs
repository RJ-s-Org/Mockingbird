//! DNS-rebinding protection for the admin port.
//!
//! While the admin listener is bound to a loopback address, a browser page
//! can still reach it through a DNS name that rebinds to 127.0.0.1; such a
//! request carries the attacker's host name in `Host`. Requests are
//! accepted only when `Host` names the loopback interface.

use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use mb_admin::Problem;

/// Middleware: 421 problem unless the request's `Host` (or, without one,
/// its URI authority) is `localhost`, `127.0.0.1` or `[::1]`, with or
/// without a port. A request with neither is let through (only non-browser
/// clients send none).
pub async fn require_loopback_host(req: Request, next: Next) -> Response {
    let host = match req.headers().get(header::HOST) {
        Some(value) => Some(value.to_str().unwrap_or_default().to_string()),
        None => req.uri().authority().map(|a| a.as_str().to_string()),
    };
    match host {
        Some(host) if !is_loopback_host(&host) => Problem::titled(
            StatusCode::MISDIRECTED_REQUEST,
            "Misdirected Request",
            "the admin port is bound to a loopback address and only answers to \
             Host localhost, 127.0.0.1 or [::1]",
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
    use super::is_loopback_host;

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
