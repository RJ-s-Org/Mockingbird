//! Compiles API descriptions (OpenAPI 3.x, Swagger 2.0, GraphQL SDL and
//! introspection, protobuf sources and descriptor sets, WSDL 1.1 with XSD)
//! into the Mockingbird Contract Model.

mod archive;
mod graphql;
mod heuristics;
mod openapi3;
mod proto;
mod schema;
mod swagger2;
mod wsdl;

pub use archive::{
    MAX_ARCHIVE_BYTES, MAX_ARCHIVE_FILES, MAX_ARCHIVE_UNPACKED_BYTES, pack_dir, read_source,
    unpack_archive,
};
use mb_mcm::Bundle;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    OpenApi3,
    Swagger2,
    GraphqlSdl,
    GraphqlIntrospection,
    Proto,
    FileDescriptorSet,
    Wsdl11,
    Archive,
}

/// One uploaded file of a multi-file contract.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceFile {
    /// `/`-separated path relative to the upload root.
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(thiserror::Error, Debug)]
pub enum ImportError {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("unsupported format: {0}")]
    Unsupported(String),
    #[error("{pointer}: {message}")]
    Invalid { pointer: String, message: String },
}

/// A version field as text. Unquoted YAML versions (`swagger: 2.0`) parse as numbers.
fn version_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Detect the contract format from `"openapi": "3.x"` or `"swagger": "2.0"`.
pub fn detect_format(doc: &Value) -> Result<SourceFormat, ImportError> {
    if let Some(v) = version_string(doc.get("openapi")) {
        if v.starts_with("3.") {
            return Ok(SourceFormat::OpenApi3);
        }
        return Err(ImportError::Unsupported(format!("openapi {v}")));
    }
    if let Some(v) = version_string(doc.get("swagger")) {
        if v == "2.0" {
            return Ok(SourceFormat::Swagger2);
        }
        return Err(ImportError::Unsupported(format!("swagger {v}")));
    }
    Err(ImportError::Unsupported(
        "document has neither an `openapi` nor a `swagger` version field".into(),
    ))
}

/// Parse JSON, falling back to YAML.
pub fn parse_document(bytes: &[u8]) -> Result<Value, ImportError> {
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        return Ok(v);
    }
    serde_saphyr::from_slice::<Value>(bytes).map_err(|e| ImportError::Parse(e.to_string()))
}

/// Markers of GraphQL SDL text.
const SDL_MARKERS: [&str; 4] = [
    "type Query",
    "schema {",
    "type Mutation",
    "type Subscription",
];

/// The `syntax` statement of a `.proto` source.
const PROTO_SYNTAX: &str = "syntax = \"proto";

/// Whether a JSON document is a GraphQL introspection result
/// (`/__schema` or `/data/__schema`).
fn is_introspection(doc: &Value) -> bool {
    doc.get("__schema").is_some() || doc.pointer("/data/__schema").is_some()
}

/// Detect the source format of uploaded bytes.
pub fn sniff(bytes: &[u8]) -> Result<SourceFormat, ImportError> {
    if archive::is_archive(bytes) {
        return Ok(SourceFormat::Archive);
    }
    if let Ok(doc) = serde_json::from_slice::<Value>(bytes) {
        if is_introspection(&doc) {
            return Ok(SourceFormat::GraphqlIntrospection);
        }
        return detect_format(&doc);
    }
    if wsdl::looks_like_xml(bytes) {
        return wsdl::sniff(bytes);
    }
    let yaml = serde_saphyr::from_slice::<Value>(bytes);
    if let Ok(doc) = &yaml
        && let Ok(format) = detect_format(doc)
    {
        return Ok(format);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        if text.contains(PROTO_SYNTAX) {
            return Ok(SourceFormat::Proto);
        }
        if SDL_MARKERS.iter().any(|m| text.contains(m)) {
            return Ok(SourceFormat::GraphqlSdl);
        }
        // A `.proto` without a syntax statement.
        if text.contains("service ") && text.contains("message ") {
            return Ok(SourceFormat::Proto);
        }
    }
    if proto::is_descriptor_set(bytes) {
        return Ok(SourceFormat::FileDescriptorSet);
    }
    match yaml {
        Ok(doc) => detect_format(&doc),
        Err(e) => Err(ImportError::Parse(e.to_string())),
    }
}

/// Detect the format and dispatch to the matching compiler.
pub fn compile(bytes: &[u8]) -> Result<Bundle, ImportError> {
    match sniff(bytes)? {
        SourceFormat::OpenApi3 => openapi3::compile(&parse_document(bytes)?, bytes),
        SourceFormat::Swagger2 => swagger2::compile(&parse_document(bytes)?, bytes),
        SourceFormat::GraphqlSdl => graphql::compile_sdl(bytes),
        SourceFormat::GraphqlIntrospection => {
            graphql::compile_introspection(&parse_document(bytes)?, bytes)
        }
        SourceFormat::Proto => proto::compile_text(bytes),
        SourceFormat::FileDescriptorSet => proto::compile_descriptor_set(bytes),
        SourceFormat::Wsdl11 => wsdl::compile_text(bytes),
        SourceFormat::Archive => {
            let mut bundle = compile_files(&unpack_archive(bytes)?)?;
            bundle.source_digest = proto::hex_digest(bytes);
            Ok(bundle)
        }
    }
}

/// Whether `path` names a GraphQL SDL file.
fn is_graphql_path(path: &str) -> bool {
    [".graphql", ".graphqls", ".gql"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// Compile a multi-file contract (an unpacked archive or loose files):
/// 1. any `.proto` files are compiled together (imports resolve between
///    them);
/// 2. else exactly one `.wsdl` is compiled with the other files available
///    to its `schemaLocation`s (several `.wsdl` files are invalid);
/// 3. else the `.graphql`/`.graphqls`/`.gql` files are compiled as one SDL,
///    joined by `\n` in path order;
/// 4. else exactly one file is compiled on its own (it may not be another
///    archive).
pub fn compile_files(files: &[SourceFile]) -> Result<Bundle, ImportError> {
    let protos: Vec<&SourceFile> = files
        .iter()
        .filter(|f| f.path.ends_with(".proto"))
        .collect();
    if !protos.is_empty() {
        return proto::compile_sources(&protos, proto::files_digest(&protos));
    }
    let wsdls: Vec<&SourceFile> = files.iter().filter(|f| f.path.ends_with(".wsdl")).collect();
    match wsdls.as_slice() {
        [] => {}
        [wsdl] => {
            let digest = match files {
                [one] => proto::hex_digest(&one.bytes),
                _ => proto::files_digest(&files.iter().collect::<Vec<_>>()),
            };
            return wsdl::compile_upload(wsdl, files, digest);
        }
        many => {
            return Err(ImportError::Invalid {
                pointer: "/".into(),
                message: format!(
                    "the upload contains {} .wsdl files; expected one",
                    many.len()
                ),
            });
        }
    }
    let mut graphql: Vec<&SourceFile> = files.iter().filter(|f| is_graphql_path(&f.path)).collect();
    if !graphql.is_empty() {
        graphql.sort_by(|a, b| a.path.cmp(&b.path));
        let sdl = graphql
            .iter()
            .map(|f| f.bytes.as_slice())
            .collect::<Vec<_>>()
            .join(&b'\n');
        return graphql::compile_sdl(&sdl);
    }
    match files {
        [one] if archive::is_archive(&one.bytes) => Err(ImportError::Invalid {
            pointer: one.path.clone(),
            message: "nested archives are not supported".into(),
        }),
        [one] => compile(&one.bytes),
        _ => Err(ImportError::Unsupported(
            "archive contains no recognised contract".into(),
        )),
    }
}
