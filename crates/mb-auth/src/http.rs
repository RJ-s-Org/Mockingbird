//! The HTTP client used for OIDC discovery, token exchange, userinfo and
//! JWKS fetches: our reqwest 0.13 client behind openidconnect's async
//! client interface.

use std::time::Duration;

use openidconnect::{HttpRequest, HttpResponse};

/// Timeout for every request to the identity provider.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A failed request to the identity provider.
#[derive(thiserror::Error, Debug)]
#[error("{0}")]
pub struct HttpError(pub String);

/// A reqwest client that never follows redirects (an OAuth endpoint that
/// redirects is misconfigured or hostile) and gives up after 10 s.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
        .unwrap_or_else(|e| {
            // Only fails when the TLS backend cannot initialise, in which
            // case the default client fails the same way on first use.
            tracing::error!(error = %e, "building the identity-provider HTTP client failed");
            reqwest::Client::new()
        })
}

/// Sends an openidconnect request through `client`.
pub async fn send(client: &reqwest::Client, req: HttpRequest) -> Result<HttpResponse, HttpError> {
    let (parts, body) = req.into_parts();
    let resp = client
        .request(parts.method, parts.uri.to_string())
        .headers(parts.headers)
        .body(body)
        .send()
        .await
        .map_err(|e| HttpError(format!("request failed: {e}")))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .bytes()
        .await
        .map_err(|e| HttpError(format!("reading response failed: {e}")))?;
    let mut out = HttpResponse::new(body.to_vec());
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    Ok(out)
}
