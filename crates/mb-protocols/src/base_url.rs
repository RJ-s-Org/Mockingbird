//! The base URL a client reached a mock at, for addresses the mock writes into its own
//! answers: a WSDL's endpoints, a list's next page.

use axum::http::{HeaderMap, header};

use crate::MockRuntime;

/// The base URL a client reached the mock at: scheme, host and mount
/// prefix.
///
/// The scheme is `https` when Mockingbird terminates TLS; else, when
/// forwarded headers are trusted, the first `X-Forwarded-Proto` value or
/// the first RFC 7239 `Forwarded: proto=`; else `http`. The host is, when
/// forwarded headers are trusted, the first `X-Forwarded-Host` value or
/// the first `Forwarded: host=`; else `Host`. Unusable values (a scheme
/// other than `http`/`https`, a host with `/`, whitespace or other
/// characters a host cannot carry) are ignored.
pub(crate) fn of(rt: &MockRuntime, headers: &HeaderMap, prefix: &str) -> String {
    let trusted = rt.protocols.trust_forwarded_headers;
    let scheme = if rt.protocols.tls_terminated {
        "https".to_string()
    } else {
        trusted
            .then(|| {
                forwarded_value(headers, "x-forwarded-proto")
                    .or_else(|| first_forwarded(headers, "proto"))
            })
            .flatten()
            .map(|p| p.to_ascii_lowercase())
            .filter(|p| p == "http" || p == "https")
            .unwrap_or_else(|| "http".into())
    };
    let forwarded_host = trusted
        .then(|| {
            forwarded_value(headers, "x-forwarded-host")
                .or_else(|| first_forwarded(headers, "host"))
        })
        .flatten()
        .filter(|h| is_host(h));
    let host = forwarded_host
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .filter(|h| is_host(h))
        })
        .unwrap_or_else(|| "localhost".into());
    format!("{scheme}://{host}{prefix}")
}

/// The first comma-separated value of header `name` (`X-Forwarded-*`),
/// trimmed; `None` when absent or empty.
fn forwarded_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?;
    let first = value.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_string())
}

/// Parameter `param` (`proto` or `host`) of the first element of the RFC
/// 7239 `Forwarded` header that sets it, unquoted.
fn first_forwarded(headers: &HeaderMap, param: &str) -> Option<String> {
    let value = headers.get(header::FORWARDED)?.to_str().ok()?;
    value
        .split(',')
        .flat_map(|element| element.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            if !name.trim().eq_ignore_ascii_case(param) {
                return None;
            }
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            (!value.is_empty()).then(|| value.to_string())
        })
}

/// Whether `host` can stand as a URL authority: a hostname or IP literal
/// with an optional port, and nothing else (no `/`, whitespace, `@`, …).
fn is_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']')
        })
}
