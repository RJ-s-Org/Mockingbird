//! Per-mock data-plane authentication (spec §11.2), shared by every
//! protocol adapter through the [`call`](crate::call) pipeline.

use std::collections::HashSet;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::HeaderMap;
use http::header::{AUTHORIZATION, COOKIE};
use mb_mcm::{ApiKeyLocation, Bundle, SecurityRequirement, SecurityScheme};

use crate::MockRuntime;

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

/// The request headers and query parameters that carry a mock's
/// credentials beyond the always-sensitive headers: the key header and the
/// `api_key` parameter in `api_key` mode, every API key the contract
/// declares in `passthrough_validate` mode (the whole `Cookie` header is
/// never forwarded anyway). They never reach the engine or the record.
#[derive(Debug, Default)]
pub(crate) struct CredentialNames {
    /// The lowercased data-plane key header in `api_key` mode.
    pub key_header: Option<String>,
    /// Lowercased header names.
    pub headers: HashSet<String>,
    pub query: HashSet<String>,
}

impl CredentialNames {
    pub(crate) fn new(auth: &DataPlaneAuth, bundle: &Bundle) -> Self {
        let mut out = Self::default();
        match auth.mode {
            DataPlaneAuthMode::None => {}
            DataPlaneAuthMode::ApiKey => {
                let header = auth.header.to_ascii_lowercase();
                out.headers.insert(header.clone());
                out.key_header = Some(header);
                out.query.insert(API_KEY_QUERY_PARAM.to_string());
            }
            DataPlaneAuthMode::PassthroughValidate => {
                let declared = bundle
                    .operations
                    .iter()
                    .flat_map(|op| &op.security)
                    .flat_map(|req| &req.schemes);
                for scheme in declared {
                    match scheme {
                        SecurityScheme::ApiKey {
                            location: ApiKeyLocation::Header,
                            name,
                        } => {
                            out.headers.insert(name.to_ascii_lowercase());
                        }
                        SecurityScheme::ApiKey {
                            location: ApiKeyLocation::Query,
                            name,
                        } => {
                            out.query.insert(name.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
        out
    }
}

/// A call refused by data-plane auth: answered `401` (gRPC
/// `UNAUTHENTICATED`, a SOAP client fault, a GraphQL `UNAUTHENTICATED`
/// error) with this `WWW-Authenticate` challenge where the protocol has
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unauthorized {
    pub challenge: String,
    pub detail: String,
}

/// The `api_key` mode check: `Ok` unless the mock is in `api_key` mode and
/// the call presents no valid data-plane key of the mock's workspace, in
/// the key header or the `api_key` query parameter.
pub(crate) async fn verify_key(
    rt: &MockRuntime,
    headers: &HeaderMap,
    query: &[(String, String)],
) -> Result<(), Unauthorized> {
    if rt.data_plane_auth.mode != DataPlaneAuthMode::ApiKey {
        return Ok(());
    }
    let from_header = rt
        .credential_names()
        .key_header
        .as_deref()
        .and_then(|h| headers.get(h))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty());
    let key = from_header.or_else(|| {
        query
            .iter()
            .find(|(k, v)| k == API_KEY_QUERY_PARAM && !v.is_empty())
            .map(|(_, v)| v.as_str())
    });
    let valid = match (key, &rt.data_plane_keys) {
        (Some(key), Some(keys)) => keys.verify(&rt.workspace, key).await,
        _ => false,
    };
    if valid {
        return Ok(());
    }
    Err(Unauthorized {
        challenge: api_key_challenge(&rt.data_plane_auth.header),
        detail: "a valid data-plane API key is required".into(),
    })
}

/// The data-plane auth check of one call of operation `op_id` (spec
/// §11.2). `key_verified` skips the `api_key` check an adapter already
/// made for the whole request.
pub(crate) async fn authenticate(
    rt: &MockRuntime,
    op_id: &str,
    security: &[SecurityRequirement],
    headers: &HeaderMap,
    query: &[(String, String)],
    key_verified: bool,
) -> Result<(), Unauthorized> {
    match rt.data_plane_auth.mode {
        DataPlaneAuthMode::None => Ok(()),
        DataPlaneAuthMode::ApiKey if key_verified => Ok(()),
        DataPlaneAuthMode::ApiKey => verify_key(rt, headers, query).await,
        DataPlaneAuthMode::PassthroughValidate => {
            if passthrough_satisfied(security, headers, query) {
                return Ok(());
            }
            Err(Unauthorized {
                challenge: passthrough_challenge(security),
                detail: format!("operation {op_id} requires the credentials its contract declares"),
            })
        }
    }
}

/// The `WWW-Authenticate` challenge for an unsatisfied operation: the first
/// requirement's first scheme.
fn passthrough_challenge(security: &[SecurityRequirement]) -> String {
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
fn api_key_challenge(header: &str) -> String {
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
