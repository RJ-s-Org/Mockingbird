//! Compiles API descriptions (OpenAPI 3.x, Swagger 2.0, GraphQL SDL and
//! introspection) into the Mockingbird Contract Model.

mod graphql;
mod heuristics;
mod openapi3;
mod schema;
mod swagger2;

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

/// Whether a JSON document is a GraphQL introspection result
/// (`/__schema` or `/data/__schema`).
fn is_introspection(doc: &Value) -> bool {
    doc.get("__schema").is_some() || doc.pointer("/data/__schema").is_some()
}

/// Detect the source format of uploaded bytes.
pub fn sniff(bytes: &[u8]) -> Result<SourceFormat, ImportError> {
    if let Ok(doc) = serde_json::from_slice::<Value>(bytes) {
        if is_introspection(&doc) {
            return Ok(SourceFormat::GraphqlIntrospection);
        }
        return detect_format(&doc);
    }
    let yaml = serde_saphyr::from_slice::<Value>(bytes);
    if let Ok(doc) = &yaml
        && let Ok(format) = detect_format(doc)
    {
        return Ok(format);
    }
    if let Ok(text) = std::str::from_utf8(bytes)
        && SDL_MARKERS.iter().any(|m| text.contains(m))
    {
        return Ok(SourceFormat::GraphqlSdl);
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
        format @ (SourceFormat::Proto
        | SourceFormat::FileDescriptorSet
        | SourceFormat::Wsdl11
        | SourceFormat::Archive) => Err(ImportError::Unsupported(format!("{format:?}"))),
    }
}
