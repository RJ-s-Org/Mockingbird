//! The draft model shared by every smart importer: operations with their
//! confidence and provenance, warnings, and the metadata stored with a
//! draft contract version.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Most provenance entries kept per operation.
pub const MAX_PROVENANCE: usize = 3;
/// Most characters of a provenance excerpt.
pub const EXCERPT_CHARS: usize = 300;

/// The kind of an import source.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ImportKind {
    Markdown,
    Html,
    Pdf,
    Text,
    Har,
    Postman,
}

impl ImportKind {
    /// The wire string of the kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Html => "html",
            Self::Pdf => "pdf",
            Self::Text => "text",
            Self::Har => "har",
            Self::Postman => "postman",
        }
    }

    /// The kind with the wire string `s`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "markdown" => Self::Markdown,
            "html" => Self::Html,
            "pdf" => Self::Pdf,
            "text" => Self::Text,
            "har" => Self::Har,
            "postman" => Self::Postman,
            _ => return None,
        })
    }

    /// Whether the kind is documentation that an LLM must read.
    pub fn needs_llm(self) -> bool {
        matches!(self, Self::Markdown | Self::Html | Self::Pdf | Self::Text)
    }
}

/// Where a parameter is sent.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ParamLocation {
    Path,
    Query,
    Header,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct DraftParam {
    pub name: String,
    #[serde(rename = "in")]
    pub location: ParamLocation,
    pub required: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub schema: Option<Value>,
    #[serde(default)]
    pub examples: Vec<Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct DraftBody {
    pub content_type: String,
    #[serde(default)]
    pub schema: Option<Value>,
    #[serde(default)]
    pub examples: Vec<Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct DraftResponse {
    pub status: u16,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub schema: Option<Value>,
    #[serde(default)]
    pub examples: Vec<Value>,
}

/// Where in the source an operation was found.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct Provenance {
    #[serde(default)]
    pub chunk: Option<u32>,
    pub location: String,
    pub excerpt: String,
}

/// One operation proposed by an importer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct DraftOperation {
    /// Upper-case HTTP method.
    pub method: String,
    /// OpenAPI path template, e.g. `/pets/{petId}`.
    pub path: String,
    #[serde(default)]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub parameters: Vec<DraftParam>,
    #[serde(default)]
    pub request: Option<DraftBody>,
    #[serde(default)]
    pub responses: Vec<DraftResponse>,
    pub confidence: f32,
    #[serde(default)]
    pub provenance: Vec<Provenance>,
}

impl DraftOperation {
    /// `"GET /pets/{petId}"`.
    pub fn target(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct DraftWarning {
    pub code: String,
    pub message: String,
    /// The target (`"GET /pets"`) of the operation the warning is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
}

impl DraftWarning {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            operation: None,
        }
    }

    pub fn for_operation(mut self, target: String) -> Self {
        self.operation = Some(target);
        self
    }
}

/// Review metadata of one operation of a draft.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct OperationMeta {
    pub confidence: f32,
    #[serde(default)]
    pub provenance: Vec<Provenance>,
    pub edited: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// The metadata stored with a draft contract version.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct DraftMeta {
    pub source_kind: ImportKind,
    #[serde(default)]
    pub source_name: Option<String>,
    #[serde(default)]
    pub source_url: Option<String>,
    pub chunk_count: u32,
    pub chunks_processed: u32,
    #[serde(default)]
    pub warnings: Vec<DraftWarning>,
    /// Keyed by operation id.
    #[serde(default)]
    pub operations: BTreeMap<String, OperationMeta>,
    #[serde(default)]
    pub usage: TokenUsage,
    pub edited: bool,
}

/// What the deterministic HAR/Postman importers return.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct RecordedImport {
    pub title: String,
    #[serde(default)]
    pub servers: Vec<String>,
    #[serde(default)]
    pub operations: Vec<DraftOperation>,
    #[serde(default)]
    pub warnings: Vec<DraftWarning>,
}
