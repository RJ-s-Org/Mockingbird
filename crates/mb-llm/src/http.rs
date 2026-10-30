//! Outbound HTTP settings shared by every client that leaves the process
//! (LLM providers, OIDC, the audit webhook): extra CA roots and a proxy.

use std::time::Duration;

use crate::LlmError;

/// Extra trust roots and the proxy of an outbound HTTP client.
///
/// `Debug` shows the proxy without the user and password a proxy URL may
/// carry, and the size of the extra roots rather than their bytes.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct HttpOptions {
    /// PEM certificates trusted in addition to the platform's roots.
    pub extra_roots_pem: Vec<u8>,
    /// `http(s)://` proxy for every request; `None` keeps reqwest's
    /// default (the `HTTP(S)_PROXY`/`NO_PROXY` environment).
    pub proxy: Option<String>,
    /// Hosts, addresses or `.domain` suffixes reached without the proxy.
    pub no_proxy: Vec<String>,
}

impl std::fmt::Debug for HttpOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpOptions")
            .field("extra_roots_pem_bytes", &self.extra_roots_pem.len())
            .field("proxy", &self.proxy.as_deref().map(redact_userinfo))
            .field("no_proxy", &self.no_proxy)
            .finish()
    }
}

/// `url` with the user and password of its authority (`user:pass@`)
/// replaced by `<redacted>`; other URLs, and text that is not a URL, as
/// they are.
pub(crate) fn redact_userinfo(url: &str) -> std::borrow::Cow<'_, str> {
    let start = url.find("://").map_or(0, |i| i + 3);
    let authority_end = url[start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |i| start + i);
    match url[start..authority_end].rfind('@') {
        Some(at) => format!("{}<redacted>{}", &url[..start], &url[start + at..]).into(),
        None => url.into(),
    }
}

impl HttpOptions {
    /// Applies the extra roots (each PEM certificate added to the default
    /// ones) and the proxy (for every scheme, minus [`Self::no_proxy`]).
    ///
    /// # Errors
    ///
    /// [`LlmError::Transport`] when the extra roots hold no PEM certificate
    /// or cannot be parsed, or the proxy is not a URL.
    pub fn apply(
        &self,
        mut builder: reqwest::ClientBuilder,
    ) -> Result<reqwest::ClientBuilder, LlmError> {
        if !self.extra_roots_pem.is_empty() {
            let certs = reqwest::Certificate::from_pem_bundle(&self.extra_roots_pem)
                .map_err(|e| LlmError::Transport(format!("invalid CA bundle: {e}")))?;
            if certs.is_empty() {
                return Err(LlmError::Transport(
                    "invalid CA bundle: no PEM certificate".into(),
                ));
            }
            builder = builder.tls_certs_merge(certs);
        }
        if let Some(url) = &self.proxy {
            let mut proxy = reqwest::Proxy::all(url).map_err(|e| {
                LlmError::Transport(format!("invalid proxy {:?}: {e}", redact_userinfo(url)))
            })?;
            if !self.no_proxy.is_empty() {
                proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&self.no_proxy.join(",")));
            }
            builder = builder.proxy(proxy);
        }
        Ok(builder)
    }
}

/// A client with `opts` applied and, when given, a whole-request
/// `timeout`.
///
/// # Errors
///
/// As [`HttpOptions::apply`], and [`LlmError::Transport`] when the client
/// cannot be built.
pub fn http_client(
    opts: &HttpOptions,
    timeout: Option<Duration>,
) -> Result<reqwest::Client, LlmError> {
    let mut builder = opts.apply(reqwest::Client::builder())?;
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    builder
        .build()
        .map_err(|e| LlmError::Transport(e.to_string()))
}
