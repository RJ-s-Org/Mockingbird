//! Smart import: turns API documentation (Markdown, HTML, PDF, plain text),
//! HAR recordings and Postman collections into reviewable draft contracts.

pub mod chunk;
pub mod model;
pub mod text;

pub use model::ImportKind;
use serde_json::Value;

/// Largest accepted input (a file, decoded base64 or text).
pub const MAX_IMPORT_BYTES: usize = 20 * 1024 * 1024;
/// Default token budget of one import job.
pub const DEFAULT_IMPORT_TOKENS: u64 = 200_000;

#[derive(thiserror::Error, Debug)]
pub enum SmartError {
    #[error("cannot read the input: {0}")]
    Unreadable(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("fetch failed: {0}")]
    Fetch(String),
    #[error("document imports need an LLM profile; the mock has none or the server runs offline")]
    LlmRequired,
    #[error("no operations were found{0}")]
    NoOperations(String),
    #[error("the import was canceled")]
    Canceled,
    #[error("the draft does not compile: {0}")]
    Compile(String),
}

/// Kind of an uploaded file from its name and bytes; None when unknown.
///
/// Content wins over the name: a PDF signature, then a HAR or Postman JSON
/// shape, then the extension, then an HTML prologue.
pub fn detect_kind(file_name: Option<&str>, bytes: &[u8]) -> Option<ImportKind> {
    if bytes.starts_with(b"%PDF-") {
        return Some(ImportKind::Pdf);
    }
    if let Some(kind) = json_kind(bytes) {
        return Some(kind);
    }
    if let Some(kind) = file_name.and_then(extension_kind) {
        return Some(kind);
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let head: String = text
        .trim_start()
        .chars()
        .take(16)
        .collect::<String>()
        .to_ascii_lowercase();
    if head.starts_with("<!doctype html") || head.starts_with("<html") {
        return Some(ImportKind::Html);
    }
    None
}

/// HAR (`/log/entries` array) or Postman (`/info/schema` on
/// schema.getpostman.com, or `/info/_postman_id` with `/item`).
fn json_kind(bytes: &[u8]) -> Option<ImportKind> {
    let first = bytes.iter().find(|b| !b.is_ascii_whitespace())?;
    if *first != b'{' {
        return None;
    }
    let doc: Value = serde_json::from_slice(bytes).ok()?;
    if doc.pointer("/log/entries").is_some_and(Value::is_array) {
        return Some(ImportKind::Har);
    }
    let postman_schema = doc
        .pointer("/info/schema")
        .and_then(Value::as_str)
        .is_some_and(|s| s.contains("schema.getpostman.com"));
    if postman_schema || (doc.pointer("/info/_postman_id").is_some() && doc.get("item").is_some()) {
        return Some(ImportKind::Postman);
    }
    None
}

fn extension_kind(name: &str) -> Option<ImportKind> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" => Some(ImportKind::Markdown),
        "html" | "htm" => Some(ImportKind::Html),
        "txt" => Some(ImportKind::Text),
        _ => None,
    }
}
