//! `/api/v1/workspaces/{ws}/mocks/{mock}/contracts…`: upload, list, inspect and activate
//! contract versions.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use mb_auth::Permission;
use mb_import::SourceFormat;
use mb_mcm::{Binding, Bundle, HttpMethod, Protocol};
use mb_store::{ContractStatus, ContractVersion, Mock, NewContractVersion};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::access::WsAccess;
use crate::audit::snapshot;
use crate::mocks::MockPath;
use crate::problem::{
    ApiResult, Json, Path, Problem, Query, RequireJson, media_type, unsupported_media_type,
};
use crate::{AdminState, VersionStatus, ensure_api_managed};

/// Largest accepted contract upload (an archive may be this large too).
pub const MAX_CONTRACT_BYTES: usize = 20 * 1024 * 1024;

/// Media types a contract upload may declare. None is CORS-safelisted (in
/// particular not `text/plain`), so a cross-site page cannot upload without
/// a preflight.
pub const CONTRACT_MEDIA_TYPES: [&str; 16] = [
    "application/json",
    "application/yaml",
    "application/x-yaml",
    "text/yaml",
    "application/vnd.oai.openapi",
    "application/vnd.oai.openapi+json",
    "application/vnd.oai.openapi+yaml",
    "application/graphql",
    "text/x-protobuf",
    "application/x-protobuf",
    "application/xml",
    "text/xml",
    "application/wsdl+xml",
    "application/zip",
    "application/gzip",
    "application/octet-stream",
];

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct UploadParams {
    /// Activate the new version. The first version of a mock (or any upload
    /// while no version is active) is activated regardless.
    pub activate: Option<bool>,
}

/// Path of one contract version.
#[derive(Deserialize, Debug)]
pub struct VersionPath {
    pub mock: String,
    pub version: i64,
}

/// The outcome of a contract upload.
#[derive(Serialize, ToSchema, Debug)]
pub struct UploadResult {
    pub version: i64,
    pub status: VersionStatus,
    /// Number of operations in the compiled contract.
    pub operations: usize,
}

/// A contract version without its source or compiled bundle.
#[derive(Serialize, ToSchema, Debug)]
pub struct ContractVersionSummary {
    pub version: i64,
    pub status: VersionStatus,
    /// Hex sha256 of the source.
    pub source_digest: String,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Number of operations in the compiled contract.
    pub operations: usize,
}

/// One operation of a compiled contract.
#[derive(Serialize, ToSchema, Debug, PartialEq)]
pub struct OperationSummary {
    pub id: String,
    /// Upper-case HTTP method (empty for non-HTTP operations).
    pub method: String,
    /// Path template with `{param}` placeholders (empty for non-HTTP operations).
    pub path: String,
    /// `query`, `create`, `update`, `delete`, `other`, `stream-server`,
    /// `stream-client` or `stream-bidi`.
    pub kind: String,
    /// `http`, `graphql`, `grpc` or `soap`.
    pub protocol: String,
    /// `GET /orders/{id}`, `query user`, `/pkg.Service/Method` or
    /// `Service/Port Operation`.
    pub target: String,
}

/// A contract version's metadata and operations.
#[derive(Serialize, ToSchema, Debug)]
pub struct ContractVersionDetail {
    pub version: i64,
    pub status: VersionStatus,
    pub source_digest: String,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    /// The contract's `info.title`.
    pub title: String,
    /// The contract's `info.version`.
    pub api_version: String,
    pub operations: Vec<OperationSummary>,
}

impl From<&ContractVersion> for ContractVersionSummary {
    fn from(v: &ContractVersion) -> Self {
        Self {
            version: v.version,
            status: VersionStatus::from(&v.status),
            source_digest: v.source_digest.clone(),
            created_by: v.created_by.clone(),
            created_at: v.created_at,
            operations: v.bundle.operations.len(),
        }
    }
}

fn operations(bundle: &Bundle) -> Vec<OperationSummary> {
    bundle
        .operations
        .iter()
        .map(|op| {
            let (method, path) = match &op.binding {
                Binding::Http { method, path } => (method_name(*method), path.clone()),
                _ => ("", String::new()),
            };
            OperationSummary {
                id: op.id.clone(),
                method: method.into(),
                path,
                kind: op.kind.label().into(),
                protocol: protocol_name(op.protocol),
                target: op.target(),
            }
        })
        .collect()
}

/// The protocol's serde name (`http`, `graphql`, `grpc`, `soap`).
fn protocol_name(p: Protocol) -> String {
    serde_json::to_value(p)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn method_name(m: HttpMethod) -> &'static str {
    match m {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Head => "HEAD",
        HttpMethod::Options => "OPTIONS",
    }
}

async fn version(st: &AdminState, mock: &Mock, version: i64) -> ApiResult<ContractVersion> {
    st.store
        .get_contract_version(mock.id, version)
        .await?
        .ok_or_else(|| {
            Problem::not_found(format!(
                "mock {:?} has no contract version {version}",
                mock.name
            ))
        })
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        UploadParams,
    ),
    request_body(
        description = "A contract, at most 20 MiB: OpenAPI 3.x or Swagger 2.0 (JSON or YAML), GraphQL SDL or introspection JSON, a .proto file or binary FileDescriptorSet, WSDL 1.1, or a zip or tar.gz archive of several files (at most 500 files, 64 MiB unpacked). Content-Type must be application/json, application/yaml, application/x-yaml, text/yaml, application/vnd.oai.openapi(+json|+yaml), application/graphql, text/x-protobuf, application/x-protobuf, application/xml, text/xml, application/wsdl+xml, application/zip, application/gzip or application/octet-stream.",
        content(
            (String = "application/json"),
            (String = "application/yaml"),
            (String = "application/x-yaml"),
            (String = "text/yaml"),
            (String = "application/vnd.oai.openapi"),
            (String = "application/vnd.oai.openapi+json"),
            (String = "application/vnd.oai.openapi+yaml"),
            (String = "application/graphql"),
            (String = "text/x-protobuf"),
            (Vec<u8> = "application/x-protobuf"),
            (String = "application/xml"),
            (String = "text/xml"),
            (String = "application/wsdl+xml"),
            (Vec<u8> = "application/zip"),
            (Vec<u8> = "application/gzip"),
            (Vec<u8> = "application/octet-stream"),
        )
    ),
    responses(
        (status = 201, body = UploadResult),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 413, body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "Not a contract media type", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The contract does not compile", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn upload(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
    Query(params): Query<UploadParams>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<impl IntoResponse> {
    access.require(Permission::Edit)?;
    let media = media_type(headers.get(header::CONTENT_TYPE));
    if !media
        .as_deref()
        .is_some_and(|m| CONTRACT_MEDIA_TYPES.contains(&m))
    {
        return Err(unsupported_media_type(
            "a contract upload",
            &CONTRACT_MEDIA_TYPES.join(", "),
        ));
    }
    let m = st.mock(&access.ws, &mock).await?;
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;
    let source = axum::body::to_bytes(body, MAX_CONTRACT_BYTES)
        .await
        .map_err(|e| {
            Problem::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "contract upload exceeds {MAX_CONTRACT_BYTES} bytes or could not be read: {e}"
                ),
            )
        })?
        .to_vec();
    // Compiling a large contract is CPU-bound: keep it off the runtime.
    let (source, compiled) = tokio::task::spawn_blocking(move || {
        let compiled = mb_import::compile(&source);
        (source, compiled)
    })
    .await
    .map_err(|e| {
        Problem::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("contract compilation did not finish: {e}"),
        )
    })?;
    let bundle =
        compiled.map_err(|e| Problem::unprocessable(format!("contract does not compile: {e}")))?;
    let operations = bundle.operations.len();

    let added = st
        .store
        .add_contract_version(
            m.id,
            NewContractVersion {
                source,
                bundle,
                status: ContractStatus::Approved,
                created_by: None,
            },
        )
        .await?;
    let activate =
        params.activate.unwrap_or(false) || st.store.active_contract(m.id).await?.is_none();
    let stored = if activate {
        st.store
            .activate_contract_version(m.id, added.version)
            .await?
    } else {
        added
    };
    let result = UploadResult {
        version: stored.version,
        status: VersionStatus::from(&stored.status),
        operations,
    };
    access
        .audit(
            &st.audit,
            "contract.upload",
            format!("contract:{mock}@{}", result.version),
            None,
            snapshot(&result),
        )
        .await;
    Ok((StatusCode::CREATED, Json(result)))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    responses(
        (status = 200, description = "Newest first", body = Vec<ContractVersionSummary>),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
) -> ApiResult<Json<Vec<ContractVersionSummary>>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let versions = st.store.list_contract_versions(m.id).await?;
    Ok(Json(versions.iter().map(Into::into).collect()))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = i64, Path, description = "Contract version number"),
    ),
    responses(
        (status = 200, body = ContractVersionDetail),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_one(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(VersionPath {
        mock,
        version: number,
    }): Path<VersionPath>,
) -> ApiResult<Json<ContractVersionDetail>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = version(&st, &m, number).await?;
    Ok(Json(ContractVersionDetail {
        version: v.version,
        status: VersionStatus::from(&v.status),
        operations: operations(&v.bundle),
        title: v.bundle.title,
        api_version: v.bundle.version,
        source_digest: v.source_digest,
        created_by: v.created_by,
        created_at: v.created_at,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/source",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = i64, Path, description = "Contract version number"),
    ),
    responses(
        (status = 200, description = "The uploaded source bytes, always as an application/octet-stream attachment named <mock>-v<version>.<ext>, so a browser never renders them. The sniffed type is in X-Mockingbird-Source-Type: application/zip, application/gzip, application/xml, application/json, text/plain; charset=utf-8 (GraphQL SDL or .proto), application/octet-stream (other binary) or application/yaml.", body = Vec<u8>, content_type = "application/octet-stream",
            headers(
                ("Content-Disposition" = String, description = "attachment; filename=\"<mock>-v<version>.<ext>\""),
                ("X-Mockingbird-Source-Type" = String, description = "The sniffed media type of the source"),
            )
        ),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn source(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(VersionPath {
        mock,
        version: number,
    }): Path<VersionPath>,
) -> ApiResult<Response> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = version(&st, &m, number).await?;
    // Served as an inert download: stored sources are caller-supplied, and
    // an XML or HTML-looking one rendered inline would run script with the
    // console's session (the API's nosniff and sandbox CSP apply too).
    let filename = format!(
        "{}-v{}.{}",
        download_name(&m.name),
        v.version,
        source_extension(&v.source)
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    headers.insert(
        SOURCE_TYPE_HEADER,
        HeaderValue::from_static(source_content_type(&v.source)),
    );
    Ok((headers, v.source).into_response())
}

/// Carries the sniffed media type of a downloaded contract source.
const SOURCE_TYPE_HEADER: HeaderName = HeaderName::from_static("x-mockingbird-source-type");

/// `name` reduced to `[A-Za-z0-9._-]` (mock names already are), for a
/// quoted `Content-Disposition` file name.
fn download_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The file extension of stored contract source bytes, by sniffing.
fn source_extension(source: &[u8]) -> &'static str {
    if source.starts_with(b"PK\x03\x04") {
        return "zip";
    }
    if source.starts_with(&[0x1f, 0x8b]) {
        return "tar.gz";
    }
    let text = std::str::from_utf8(source)
        .map(|t| t.trim_start_matches('\u{feff}').trim_start())
        .ok();
    let json = text.is_some_and(|t| t.starts_with('{'));
    match mb_import::sniff(source) {
        Ok(SourceFormat::OpenApi3 | SourceFormat::Swagger2) if json => "json",
        Ok(SourceFormat::OpenApi3 | SourceFormat::Swagger2) => "yaml",
        Ok(SourceFormat::GraphqlSdl) => "graphql",
        Ok(SourceFormat::GraphqlIntrospection) => "json",
        Ok(SourceFormat::Proto) => "proto",
        Ok(SourceFormat::FileDescriptorSet) => "pb",
        Ok(SourceFormat::Wsdl11) => "wsdl",
        Ok(SourceFormat::Archive) | Err(_) => match text {
            None => "bin",
            Some(t) if t.starts_with('<') => "xml",
            Some(_) if json => "json",
            Some(_) => "txt",
        },
    }
}

/// The content type of stored contract source bytes, by sniffing.
fn source_content_type(source: &[u8]) -> &'static str {
    if source.starts_with(b"PK\x03\x04") {
        return "application/zip";
    }
    if source.starts_with(&[0x1f, 0x8b]) {
        return "application/gzip";
    }
    let Ok(text) = std::str::from_utf8(source) else {
        return "application/octet-stream";
    };
    let text = text.trim_start_matches('\u{feff}').trim_start();
    if text.starts_with('<') {
        return "application/xml";
    }
    if text.starts_with('{') {
        return "application/json";
    }
    match mb_import::sniff(source) {
        Ok(SourceFormat::GraphqlSdl | SourceFormat::Proto) => "text/plain; charset=utf-8",
        _ => "application/yaml",
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/activate",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = i64, Path, description = "Contract version number"),
    ),
    responses(
        (status = 200, description = "Now the only active version; the previous one is approved", body = ContractVersionSummary),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A draft, or managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "Content-Type is not application/json (the body is ignored)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn activate(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(VersionPath {
        mock,
        version: number,
    }): Path<VersionPath>,
    _json: RequireJson,
) -> ApiResult<Json<ContractVersionSummary>> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = version(&st, &m, number).await?;
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;
    if v.status == ContractStatus::Draft {
        return Err(Problem::conflict(format!(
            "contract version {number} of mock {mock:?} is a draft and cannot be activated"
        )));
    }
    let active = st.store.activate_contract_version(m.id, number).await?;
    let view = ContractVersionSummary::from(&active);
    access
        .audit(
            &st.audit,
            "contract.activate",
            format!("contract:{mock}@{number}"),
            None,
            snapshot(&view),
        )
        .await;
    Ok(Json(view))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn fixture(path: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/../../fixtures/{path}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn source_extensions_follow_the_sniffed_format() {
        let cases = [
            ("graphql/starwars.graphql", "graphql"),
            ("proto/route_guide.proto", "proto"),
            ("wsdl/calculator.wsdl", "wsdl"),
            ("openapi/orders-3.0.yaml", "yaml"),
        ];
        for (path, ext) in cases {
            assert_eq!(source_extension(&fixture(path)), ext, "{path}");
        }
        assert_eq!(source_extension(b"PK\x03\x04rest"), "zip");
        assert_eq!(source_extension(&[0x1f, 0x8b, 0]), "tar.gz");
        assert_eq!(source_extension(b"{\"openapi\": \"3.0.3\"}"), "json");
        assert_eq!(source_extension(&[0xff, 0xfe, 0x00]), "bin");
    }

    #[test]
    fn download_names_are_header_safe() {
        assert_eq!(download_name("orders-v2"), "orders-v2");
        assert_eq!(download_name("a\"b;c d"), "a_b_c_d");
    }
}
