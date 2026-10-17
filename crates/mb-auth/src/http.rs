//! The HTTP client used for OIDC discovery, token exchange, userinfo and
//! JWKS fetches: our reqwest 0.13 client behind openidconnect's async
//! client interface.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use openidconnect::{AsyncHttpClient, HttpRequest, HttpResponse};

/// Timeout for every request to the identity provider.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A failed request to the identity provider.
#[derive(thiserror::Error, Debug)]
#[error("{0}")]
pub struct HttpError(pub String);

/// A reqwest client that never follows redirects (an OAuth endpoint that
/// redirects is misconfigured or hostile) and gives up after 10 s.
pub fn http_client() -> reqwest::Client {
    http_client_with(&mb_llm::HttpOptions::default()).unwrap_or_else(|e| {
        // Without a CA bundle or proxy, building only fails when the TLS
        // backend cannot initialise, and the default client then fails
        // the same way on first use.
        tracing::error!(error = %e, "building the identity-provider HTTP client failed");
        reqwest::Client::new()
    })
}

/// [`http_client`] with the outbound CA roots and proxy of `opts`; an
/// error when they (or the client) cannot be built, so a bad CA bundle or
/// proxy fails startup instead of being silently dropped.
pub fn http_client_with(opts: &mb_llm::HttpOptions) -> Result<reqwest::Client, HttpError> {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT);
    opts.apply(builder)
        .map_err(|e| HttpError(e.to_string()))?
        .build()
        .map_err(|e| HttpError(format!("cannot build the HTTP client: {e}")))
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

/// [`send`] as an openidconnect client. A named type (rather than a
/// closure) keeps the futures that use it provably `Send`, so they can run
/// in axum handlers.
pub(crate) struct OidcHttp<'a>(pub(crate) &'a reqwest::Client);

impl<'c> AsyncHttpClient<'c> for OidcHttp<'_> {
    type Error = HttpError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, HttpError>> + Send + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(send(self.0, request))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn invalid_outbound_options_are_an_error() {
        let bad_ca = mb_llm::HttpOptions {
            extra_roots_pem: b"not a certificate".to_vec(),
            ..Default::default()
        };
        let err = http_client_with(&bad_ca).unwrap_err();
        assert!(err.0.contains("invalid CA bundle"), "{err}");
        let bad_proxy = mb_llm::HttpOptions {
            proxy: Some("not a url".into()),
            ..Default::default()
        };
        let err = http_client_with(&bad_proxy).unwrap_err();
        assert!(err.0.contains("invalid proxy"), "{err}");
        assert!(http_client_with(&Default::default()).is_ok());
    }
}
