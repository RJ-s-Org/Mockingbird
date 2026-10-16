//! Compiles API descriptions (OpenAPI 3.x, Swagger 2.0) into the Mockingbird
//! Contract Model.

mod heuristics;
mod openapi3;
mod schema;
mod security;
mod swagger2;

use mb_mcm::Bundle;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    OpenApi3,
    Swagger2,
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

/// Parse, detect the format, and dispatch to the matching compiler.
pub fn compile(bytes: &[u8]) -> Result<Bundle, ImportError> {
    let doc = parse_document(bytes)?;
    match detect_format(&doc)? {
        SourceFormat::OpenApi3 => openapi3::compile(&doc, bytes),
        SourceFormat::Swagger2 => swagger2::compile(&doc, bytes),
    }
}
