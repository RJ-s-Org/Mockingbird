//! Per-mock data-plane authentication (spec §11.2).

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::HeaderMap;
use http::header::{AUTHORIZATION, COOKIE};
use mb_mcm::{ApiKeyLocation, SecurityRequirement, SecurityScheme};

/// The query parameter that may carry a data-plane API key.
pub const API_KEY_QUERY_PARAM: &str = "api_key";

/// How a mock authenticates data-plane callers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DataPlaneAuthMode {
    /// Every caller is served.
    #[default]
    None,
    /// A Mockingbird API key with scope `data-plane`, in [`DataPlaneAuth::header`]
    /// or the `api_key` query parameter.
    ApiKey,
    /// Credentials in the form the contract declares, not verified against
    /// any issuer.
    PassthroughValidate,
}

/// A mock's data-plane auth settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataPlaneAuth {
    pub mode: DataPlaneAuthMode,
    /// The header carrying the key in [`DataPlaneAuthMode::ApiKey`] mode.
    pub header: String,
}

impl Default for DataPlaneAuth {
    fn default() -> Self {
        Self {
            mode: DataPlaneAuthMode::None,
            header: "x-api-key".into(),
        }
    }
}

/// Verifies data-plane API keys (implemented over the control store by the
/// binary, so this crate stays store-agnostic).
#[async_trait]
pub trait DataPlaneKeys: Send + Sync {
    /// Whether `key` is a usable data-plane key for `workspace`.
    async fn verify(&self, workspace: &str, key: &str) -> bool;
}

/// Whether the request presents credentials in a form one of `security`'s
/// requirements declares (any requirement; every scheme within it). No
/// requirements, or an empty one, is always satisfied. Credentials are only
/// checked for shape, never verified; OAuth scopes are not checked.
pub fn passthrough_satisfied(
    security: &[SecurityRequirement],
    headers: &HeaderMap,
    query: &[(String, String)],
) -> bool {
    security.is_empty()
        || security.iter().any(|req| {
            req.schemes
                .iter()
                .all(|scheme| scheme_satisfied(scheme, headers, query))
        })
}

/// The `WWW-Authenticate` challenge for an unsatisfied operation: the first
/// requirement's first scheme.
#[cfg(feature = "http-rest")]
pub(crate) fn passthrough_challenge(security: &[SecurityRequirement]) -> String {
    let first = security.iter().find_map(|r| r.schemes.first());
    match first {
        None
        | Some(SecurityScheme::Bearer)
        | Some(SecurityScheme::OAuth2 { .. })
        | Some(SecurityScheme::OpenIdConnect { .. }) => "Bearer".into(),
        Some(SecurityScheme::Basic) => r#"Basic realm="mockingbird""#.into(),
        Some(SecurityScheme::ApiKey { location, name }) => {
            let location = match location {
                ApiKeyLocation::Header => "header",
                ApiKeyLocation::Query => "query",
                ApiKeyLocation::Cookie => "cookie",
            };
            format!(r#"ApiKey realm="mockingbird", {location}="{name}""#)
        }
    }
}

/// The `WWW-Authenticate` challenge for `api_key` mode.
#[cfg(feature = "http-rest")]
pub(crate) fn api_key_challenge(header: &str) -> String {
    format!(r#"ApiKey realm="mockingbird", header="{header}""#)
}

fn scheme_satisfied(
    scheme: &SecurityScheme,
    headers: &HeaderMap,
    query: &[(String, String)],
) -> bool {
    match scheme {
        SecurityScheme::Bearer
        | SecurityScheme::OAuth2 { .. }
        | SecurityScheme::OpenIdConnect { .. } => {
            authorization(headers, "bearer").is_some_and(|token| !token.is_empty())
        }
        SecurityScheme::Basic => authorization(headers, "basic").is_some_and(is_basic_credential),
        SecurityScheme::ApiKey { location, name } => match location {
            ApiKeyLocation::Header => headers
                .get_all(name.to_ascii_lowercase().as_str())
                .iter()
                .any(|v| v.to_str().is_ok_and(|s| !s.trim().is_empty())),
            ApiKeyLocation::Query => query.iter().any(|(k, v)| k == name && !v.is_empty()),
            ApiKeyLocation::Cookie => headers
                .get_all(COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(';'))
                .filter_map(|pair| pair.trim().split_once('='))
                .any(|(k, v)| k.trim() == name && !v.trim().is_empty()),
        },
    }
}

/// The credentials of an `Authorization: <scheme> <credentials>` header
/// whose scheme matches `scheme` (case-insensitively), trimmed.
fn authorization<'a>(headers: &'a HeaderMap, scheme: &str) -> Option<&'a str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    let (name, rest) = value.split_once(' ').unwrap_or((value, ""));
    name.eq_ignore_ascii_case(scheme).then(|| rest.trim())
}

/// Standard base64 decoding to UTF-8 that contains `:`.
fn is_basic_credential(encoded: &str) -> bool {
    STANDARD
        .decode(encoded)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|s| s.contains(':'))
}
