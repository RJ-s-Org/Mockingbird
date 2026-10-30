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

impl HttpError {
    /// What failed, as `Display` prints it.
    pub fn message(&self) -> &str {
        &self.0
    }
}

/// The builder every identity-provider client starts from: no redirects
/// (an OAuth endpoint that redirects is misconfigured or hostile) and a
/// 10 s timeout.
fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
}

/// A reqwest client that never follows redirects (an OAuth endpoint that
/// redirects is misconfigured or hostile) and gives up after 10 s.
///
/// Building only fails when the TLS backend cannot initialise; the client
/// returned then still never follows redirects, but trusts no TLS roots
/// and uses no proxy, so every HTTPS request fails. Prefer
/// [`try_http_client`], which returns that failure as an error.
pub fn http_client() -> reqwest::Client {
    or_fallback(try_http_client())
}

/// [`http_client`], with a build failure returned as an error.
///
/// # Errors
///
/// An [`HttpError`] when the TLS backend cannot initialise.
pub fn try_http_client() -> Result<reqwest::Client, HttpError> {
    http_client_with(&mb_llm::HttpOptions::default())
}

fn or_fallback(built: Result<reqwest::Client, HttpError>) -> reqwest::Client {
    built.unwrap_or_else(|e| {
        tracing::error!(error = %e, "building the identity-provider HTTP client failed");
        fail_closed()
    })
}

/// A no-redirect client that trusts no TLS roots and uses no proxy, so
/// HTTPS requests fail closed. It skips the platform verifier (the step
/// that can fail on a broken TLS setup), so its build only fails when
/// reqwest cannot build any client, and `reqwest::Client::new()` in the
/// last arm then fails inside reqwest instead of returning a client.
fn fail_closed() -> reqwest::Client {
    builder()
        .tls_certs_only(Vec::new())
        .no_proxy()
        .build()
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "building the fail-closed identity-provider HTTP client failed");
            reqwest::Client::new()
        })
}

/// [`http_client`] with the outbound CA roots and proxy of `opts`; an
/// error when they (or the client) cannot be built, so a bad CA bundle or
/// proxy fails startup instead of being silently dropped.
///
/// # Errors
///
/// An [`HttpError`] for an invalid CA bundle or proxy in `opts`, or when
/// the client cannot be built.
pub fn http_client_with(opts: &mb_llm::HttpOptions) -> Result<reqwest::Client, HttpError> {
    opts.apply(builder())
        .map_err(|e| HttpError(e.to_string()))?
        .build()
        .map_err(|e| HttpError(format!("cannot build the HTTP client: {e}")))
}

/// Sends an openidconnect request through `client`.
///
/// # Errors
///
/// An [`HttpError`] when the request cannot be sent or its body cannot be
/// read; an HTTP error status is returned as a response, not an error.
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
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn invalid_outbound_options_are_an_error() {
        let bad_ca = mb_llm::HttpOptions {
            extra_roots_pem: b"not a certificate".to_vec(),
            ..Default::default()
        };
        let err = http_client_with(&bad_ca).unwrap_err();
        assert!(err.message().contains("invalid CA bundle"), "{err}");
        let bad_proxy = mb_llm::HttpOptions {
            proxy: Some("not a url".into()),
            ..Default::default()
        };
        let err = http_client_with(&bad_proxy).unwrap_err();
        assert!(err.0.contains("invalid proxy"), "{err}");
        assert!(http_client_with(&Default::default()).is_ok());
    }

    /// A local server that answers every request with `response` and
    /// counts the connections it accepted.
    async fn serve(response: String) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, hits)
    }

    /// Sends a GET through `client` to a local server that answers 302 to
    /// a second local server; returns the status and how many connections
    /// the second server accepted.
    async fn redirect_probe(client: &reqwest::Client) -> (u16, usize) {
        let ok = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
        let (target, target_hits) = serve(ok.to_string()).await;
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nlocation: http://{target}/jwks\r\n\
             content-length: 0\r\nconnection: close\r\n\r\n"
        );
        let (origin, _) = serve(redirect).await;
        let status = client
            .get(format!("http://{origin}/.well-known/openid-configuration"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        (status, target_hits.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn clients_never_follow_redirects() {
        let built = http_client_with(&Default::default()).unwrap();
        assert_eq!(redirect_probe(&built).await, (302, 0));
        assert_eq!(redirect_probe(&http_client()).await, (302, 0));
        assert_eq!(redirect_probe(&try_http_client().unwrap()).await, (302, 0));
    }
    #[tokio::test]
    async fn the_fallback_client_never_follows_redirects() {
        let fallback = or_fallback(Err(HttpError("forced build failure".into())));
        assert_eq!(redirect_probe(&fallback).await, (302, 0));
    }
}
