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
    http_client_with(&mb_llm::HttpOptions::default())
}

/// [`http_client`] with the outbound CA roots and proxy of `opts`.
pub fn http_client_with(opts: &mb_llm::HttpOptions) -> reqwest::Client {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT);
    let built = opts
        .apply(builder)
        .map_err(|e| e.to_string())
        .and_then(|b| b.build().map_err(|e| e.to_string()));
    built.unwrap_or_else(|e| {
        // The options are checked at startup and the TLS backend only
        // fails to initialise when the default client fails the same way
        // on first use, so this is not expected.
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
