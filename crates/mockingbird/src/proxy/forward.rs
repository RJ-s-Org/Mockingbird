//! Forwards a request to another HTTP server, streaming both bodies.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use mb_protocols::http_rest::problem;

/// Hop-by-hop headers, never forwarded.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "proxy-connection",
    "proxy-authorization",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The client forwarding uses: it ignores `HTTP(S)_PROXY` (which may point
/// at this very proxy) and does not follow redirects (the caller does).
///
/// # Errors
///
/// Fails when the HTTP client cannot be built.
pub fn forward_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

fn strip_hop_by_hop(headers: &HeaderMap) -> HeaderMap {
    // Headers named in `Connection` are hop-by-hop too.
    let named: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let n = name.as_str();
        if !HOP_BY_HOP.contains(&n) && !named.iter().any(|h| h == n) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// Sends `req` to `target`, with `Host` set to `host_header` when given
/// (else the target's), and answers the response; 502 problem+json when
/// the target cannot be reached.
pub async fn forward(
    client: &reqwest::Client,
    target: reqwest::Url,
    host_header: Option<&str>,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let mut headers = strip_hop_by_hop(&parts.headers);
    headers.remove(header::HOST);
    if let Some(host) = host_header
        && let Ok(value) = host.parse()
    {
        headers.insert(header::HOST, value);
    }
    let target_label = target.host_str().unwrap_or_default().to_owned();
    let result = client
        .request(parts.method, target)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await;
    let upstream = match result {
        Ok(upstream) => upstream,
        Err(error) => {
            tracing::debug!(host = %target_label, %error, "proxy: forwarding failed");
            return problem(
                502,
                "Upstream unreachable",
                format!("cannot reach {target_label}: {error}"),
            );
        }
    };
    let status = upstream.status();
    let headers = strip_hop_by_hop(upstream.headers());
    let mut resp = Body::from_stream(upstream.bytes_stream()).into_response();
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}
