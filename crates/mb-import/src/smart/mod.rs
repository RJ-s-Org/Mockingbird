//! Smart import: turns API documentation (Markdown, HTML, PDF, plain text),
//! HAR recordings and Postman collections into reviewable draft contracts.

pub mod assemble;
pub mod chunk;
pub mod fetch;
pub mod har;
pub mod infer;
pub mod llm;
pub mod merge;
pub mod model;
pub mod paths;
pub mod postman;
pub mod redact;
pub mod text;

use async_trait::async_trait;
use mb_mcm::Bundle;
use serde::Serialize;
use serde_json::Value;

use assemble::{assemble_openapi, compile_draft};
use chunk::{CHUNK_OVERLAP_TOKENS, CHUNK_TOKENS, chunk};
use fetch::{FetchPolicy, Fetched, Resolver, SystemResolver, fetch_with};
pub use llm::ExtractLlm;
use llm::{TokenBudget, extract_chunk, summarize_operations};
use merge::merge_operations;
pub use model::ImportKind;
use model::{DraftMeta, DraftOperation, DraftWarning, OperationMeta};

/// Largest accepted input (a file, decoded base64 or text).
pub const MAX_IMPORT_BYTES: usize = 20 * 1024 * 1024;
/// Most examples kept per request body or response while importing.
pub const MAX_EXAMPLES: usize = 5;
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

impl From<fetch::FetchError> for SmartError {
    fn from(e: fetch::FetchError) -> Self {
        SmartError::Fetch(e.to_string())
    }
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

/// Tokens reserved for the prompt around a chunk when checking the budget.
const PROMPT_OVERHEAD_TOKENS: u64 = 1500;
/// Most warning messages quoted in a [`SmartError::NoOperations`].
const NO_OPERATIONS_REASONS: usize = 3;
/// Title of a draft whose source names none.
const DEFAULT_TITLE: &str = "Imported API";
/// Source name used in prompts when the input has none.
const UNNAMED_SOURCE: &str = "document";

/// Limits of one import.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Tokens the import's LLM calls may spend.
    pub max_tokens: u64,
    pub fetch: FetchPolicy,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            max_tokens: DEFAULT_IMPORT_TOKENS,
            fetch: FetchPolicy::default(),
        }
    }
}

/// What to import.
#[derive(Clone, Debug)]
pub enum ImportInput {
    /// Uploaded bytes of a known kind.
    Bytes {
        kind: ImportKind,
        name: Option<String>,
        bytes: Vec<u8>,
    },
    /// A URL to fetch.
    Url(String),
}

/// The phase an import is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Fetching,
    Extracting,
    Analyzing,
    Summarizing,
    Assembling,
}

impl Phase {
    /// The wire string of the phase.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fetching => "fetching",
            Self::Extracting => "extracting",
            Self::Analyzing => "analyzing",
            Self::Summarizing => "summarizing",
            Self::Assembling => "assembling",
        }
    }
}

/// Receives the progress of an import and may cancel it.
#[async_trait]
pub trait ImportObserver: Send + Sync {
    /// false = canceled
    async fn progress(&self, phase: Phase, done: u32, total: u32) -> bool;
}

/// An observer that never cancels.
pub struct NoopObserver;

#[async_trait]
impl ImportObserver for NoopObserver {
    async fn progress(&self, _phase: Phase, _done: u32, _total: u32) -> bool {
        true
    }
}

/// A compiled draft and its review metadata.
#[derive(Clone, Debug)]
pub struct DraftResult {
    pub document: Value,
    /// Pretty JSON of `document`.
    pub source: Vec<u8>,
    pub bundle: Bundle,
    pub meta: DraftMeta,
}

/// Reports progress; errs with [`SmartError::Canceled`] when the observer
/// cancels.
async fn report(
    observer: &dyn ImportObserver,
    phase: Phase,
    done: usize,
    total: usize,
) -> Result<(), SmartError> {
    let done = u32::try_from(done).unwrap_or(u32::MAX);
    let total = u32::try_from(total).unwrap_or(u32::MAX);
    if observer.progress(phase, done, total).await {
        Ok(())
    } else {
        Err(SmartError::Canceled)
    }
}

/// The kind of a fetched document, from its content type, else its URL's
/// last path segment and its bytes.
fn fetched_kind(fetched: &Fetched) -> Result<ImportKind, SmartError> {
    match fetched.content_type.as_deref() {
        Some("text/html") => return Ok(ImportKind::Html),
        Some("application/pdf") => return Ok(ImportKind::Pdf),
        Some("text/markdown" | "text/x-markdown") => return Ok(ImportKind::Markdown),
        Some("text/plain") => return Ok(ImportKind::Text),
        Some(ct) if ct == "application/json" || ct.ends_with("+json") => {
            return match detect_kind(None, &fetched.bytes) {
                Some(kind @ (ImportKind::Har | ImportKind::Postman)) => Ok(kind),
                _ => Err(SmartError::Unsupported(
                    "the URL returned JSON that is neither a HAR file nor a Postman collection; \
                     upload OpenAPI and other contracts to the contracts endpoint"
                        .to_string(),
                )),
            };
        }
        _ => {}
    }
    let segment = fetched
        .url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty());
    detect_kind(segment, &fetched.bytes).ok_or_else(|| {
        SmartError::Unsupported(format!(
            "the URL returned {}, which is not a supported import kind",
            fetched
                .content_type
                .as_deref()
                .unwrap_or("an unknown content type")
        ))
    })
}

/// Operations, servers, title and progress of the source-specific stage.
struct Extracted {
    title: String,
    servers: Vec<String>,
    operations: Vec<DraftOperation>,
    warnings: Vec<DraftWarning>,
    chunk_count: usize,
    chunks_processed: usize,
}

/// Reads documentation with the LLM, chunk by chunk, within the budget.
async fn extract_document(
    kind: ImportKind,
    bytes: Vec<u8>,
    source_name: Option<&str>,
    llm: &ExtractLlm,
    budget: &mut TokenBudget,
    observer: &dyn ImportObserver,
) -> Result<Extracted, SmartError> {
    report(observer, Phase::Extracting, 0, 1).await?;
    let doc = if kind == ImportKind::Pdf {
        tokio::task::spawn_blocking(move || text::extract_text(kind, &bytes))
            .await
            .map_err(|_| SmartError::Unreadable("the PDF could not be parsed".to_string()))??
    } else {
        text::extract_text(kind, &bytes)?
    };
    let chunks = chunk(&doc, CHUNK_TOKENS, CHUNK_OVERLAP_TOKENS);
    let total = chunks.len();
    let name = source_name.unwrap_or(UNNAMED_SOURCE);
    let mut operations = Vec::new();
    let mut warnings = Vec::new();
    let mut processed = 0usize;
    for c in &chunks {
        report(observer, Phase::Analyzing, processed, total).await?;
        let estimate = (c.tokens as u64)
            .saturating_add(PROMPT_OVERHEAD_TOKENS)
            .saturating_add(u64::from(llm.settings.max_tokens));
        if !budget.can_afford(estimate) {
            warnings.push(DraftWarning::new(
                "token_budget_exhausted",
                format!(
                    "stopped after {processed} of {total} chunks: the token budget of {} tokens is spent",
                    budget.limit
                ),
            ));
            break;
        }
        let result = extract_chunk(llm, c, total, name).await;
        budget.add(result.usage);
        operations.extend(result.operations);
        warnings.extend(result.warnings);
        processed += 1;
    }
    let (operations, merge_warnings) = merge_operations(operations);
    warnings.extend(merge_warnings);
    let title = doc
        .title
        .filter(|t| !t.trim().is_empty())
        .or_else(|| source_name.map(str::to_string))
        .unwrap_or_else(|| DEFAULT_TITLE.to_string());
    Ok(Extracted {
        title,
        servers: Vec::new(),
        operations,
        warnings,
        chunk_count: total,
        chunks_processed: processed,
    })
}

/// Imports with the system resolver; see [`run_import_with`].
pub async fn run_import(
    input: ImportInput,
    llm: Option<&ExtractLlm>,
    opts: &ImportOptions,
    observer: &dyn ImportObserver,
) -> Result<DraftResult, SmartError> {
    run_import_with(input, llm, opts, observer, &SystemResolver).await
}

/// Turns the input into a compiled draft: fetch (for a URL), extract
/// operations (LLM for documentation, deterministic for HAR/Postman, with
/// LLM summaries when available), assemble OpenAPI 3.1 and compile it.
pub async fn run_import_with(
    input: ImportInput,
    llm: Option<&ExtractLlm>,
    opts: &ImportOptions,
    observer: &dyn ImportObserver,
    resolver: &dyn Resolver,
) -> Result<DraftResult, SmartError> {
    let (kind, bytes, source_name, source_url) = match input {
        ImportInput::Bytes { kind, name, bytes } => (kind, bytes, name, None),
        ImportInput::Url(url) => {
            report(observer, Phase::Fetching, 0, 1).await?;
            let fetched = fetch_with(&url, &opts.fetch, resolver).await?;
            let kind = fetched_kind(&fetched)?;
            let final_url = fetched.url.to_string();
            (kind, fetched.bytes, Some(url), Some(final_url))
        }
    };
    if bytes.len() > MAX_IMPORT_BYTES {
        return Err(SmartError::Unsupported(format!(
            "the input is larger than {} MiB",
            MAX_IMPORT_BYTES / (1024 * 1024)
        )));
    }

    let mut budget = TokenBudget::new(opts.max_tokens);
    let extracted = if kind.needs_llm() {
        let llm = llm.ok_or(SmartError::LlmRequired)?;
        extract_document(
            kind,
            bytes,
            source_name.as_deref(),
            llm,
            &mut budget,
            observer,
        )
        .await?
    } else {
        let mut recorded = match kind {
            ImportKind::Har => har::import_har(&bytes)?,
            _ => postman::import_postman(&bytes)?,
        };
        if let Some(llm) = llm {
            report(observer, Phase::Summarizing, 0, 1).await?;
            let warnings = summarize_operations(llm, &mut recorded.operations, &mut budget).await;
            recorded.warnings.extend(warnings);
        }
        Extracted {
            title: recorded.title,
            servers: recorded.servers,
            operations: recorded.operations,
            warnings: recorded.warnings,
            chunk_count: 0,
            chunks_processed: 0,
        }
    };

    if extracted.operations.is_empty() {
        let reasons: Vec<&str> = extracted
            .warnings
            .iter()
            .take(NO_OPERATIONS_REASONS)
            .map(|w| w.message.as_str())
            .collect();
        let text = if reasons.is_empty() {
            String::new()
        } else {
            format!(": {}", reasons.join("; "))
        };
        return Err(SmartError::NoOperations(text));
    }

    report(observer, Phase::Assembling, 0, 1).await?;
    let (document, ids, assembly_warnings) =
        assemble_openapi(&extracted.title, &extracted.servers, &extracted.operations);
    let (source, bundle) = compile_draft(&document).map_err(|issues| {
        let first = issues.first();
        SmartError::Compile(format!(
            "{}: {}",
            first.map_or("/", |i| i.pointer.as_str()),
            first.map_or("unknown problem", |i| i.message.as_str())
        ))
    })?;
    let operations = ids
        .into_iter()
        .zip(&extracted.operations)
        .map(|(id, op)| {
            (
                id,
                OperationMeta {
                    confidence: op.confidence,
                    provenance: op.provenance.clone(),
                    edited: false,
                },
            )
        })
        .collect();
    let mut warnings = extracted.warnings;
    warnings.extend(assembly_warnings);
    let meta = DraftMeta {
        source_kind: kind,
        source_name,
        source_url,
        chunk_count: u32::try_from(extracted.chunk_count).unwrap_or(u32::MAX),
        chunks_processed: u32::try_from(extracted.chunks_processed).unwrap_or(u32::MAX),
        warnings,
        operations,
        usage: budget.used,
        edited: false,
    };
    Ok(DraftResult {
        document,
        source,
        bundle,
        meta,
    })
}
