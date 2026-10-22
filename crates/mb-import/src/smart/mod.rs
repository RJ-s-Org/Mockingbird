//! Smart import: turns API documentation (Markdown, HTML, PDF, plain text),
//! HAR recordings and Postman collections into reviewable draft contracts.

pub mod assemble;
pub mod chunk;
pub mod complete;
pub mod embedded;
pub mod fetch;
pub mod har;
pub mod infer;
pub mod llm;
pub mod merge;
pub mod model;
pub mod paths;
mod pdfscan;
pub mod postman;
pub mod redact;
pub mod text;

use std::collections::HashSet;

use async_trait::async_trait;
use mb_mcm::Bundle;
use serde::Serialize;
use serde_json::Value;

use assemble::{
    DraftIssue, assemble_openapi, compile_draft, implicated_operations, implicated_targets,
};
use chunk::{CHUNK_OVERLAP_TOKENS, CHUNK_TOKENS, chunk};
use embedded::{Embedded, covers_endpoints, extract_embedded, insert_embedded};
use fetch::{FetchPolicy, Fetched, Resolver, SystemResolver, fetch_with};
pub use llm::ExtractLlm;
use llm::{TokenBudget, extract_chunk, summarize_operations};
use merge::merge_operations;
pub use model::ImportKind;
use model::{DraftMeta, DraftOperation, DraftWarning, OperationMeta};
use paths::path_key;

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
/// The job error for a draft that does not compile: its first problem.
pub(crate) fn compile_error(issues: &[DraftIssue]) -> SmartError {
    let first = issues.first();
    SmartError::Compile(format!(
        "{}: {}",
        first.map_or("/", |i| i.pointer.as_str()),
        first.map_or("unknown problem", |i| i.message.as_str())
    ))
}

pub(crate) async fn report(
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
    /// Operations of OpenAPI blocks embedded in a document.
    embedded: Embedded,
}

/// The draft's title: the document's, else the source name, else
/// [`DEFAULT_TITLE`].
fn document_title(title: Option<String>, source_name: Option<&str>) -> String {
    title
        .filter(|t| !t.trim().is_empty())
        .or_else(|| source_name.map(str::to_string))
        .unwrap_or_else(|| DEFAULT_TITLE.to_string())
}

/// Reads documentation: the operations of its embedded OpenAPI blocks,
/// then the rest with the LLM, chunk by chunk, within the budget. When the
/// blocks cover every endpoint the text names, no chunk is read (and no
/// LLM is needed).
async fn extract_document(
    kind: ImportKind,
    bytes: Vec<u8>,
    source_name: Option<&str>,
    llm: Option<&ExtractLlm>,
    budget: &mut TokenBudget,
    observer: &dyn ImportObserver,
) -> Result<Extracted, SmartError> {
    report(observer, Phase::Extracting, 0, 1).await?;
    let (doc, chunks, embedded, covered) = blocking(move || {
        let mut doc = text::extract_text(kind, &bytes)?;
        let embedded = extract_embedded(kind, &bytes, &mut doc);
        let covered = !embedded.operations.is_empty() && covers_endpoints(&embedded, &doc);
        let chunks = if covered {
            Vec::new()
        } else {
            chunk(&doc, CHUNK_TOKENS, CHUNK_OVERLAP_TOKENS)
        };
        Ok((doc, chunks, embedded, covered))
    })
    .await?;
    let llm = match llm {
        Some(llm) => llm,
        None if covered => {
            return Ok(Extracted {
                title: document_title(doc.title, source_name),
                servers: embedded.servers.clone(),
                operations: Vec::new(),
                warnings: embedded.warnings.clone(),
                chunk_count: 0,
                chunks_processed: 0,
                embedded,
            });
        }
        None => return Err(SmartError::LlmRequired),
    };
    let total = chunks.len();
    let name = source_name.unwrap_or(UNNAMED_SOURCE);
    let mut operations = Vec::new();
    let mut warnings = embedded.warnings.clone();
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
    let (mut operations, merge_warnings) = merge_operations(operations);
    warnings.extend(merge_warnings);
    // An operation an embedded block defines is taken from the block.
    let blocks: HashSet<(String, String)> = embedded
        .operations
        .iter()
        .map(|o| path_key(&o.method, &o.path))
        .collect();
    let replaced: HashSet<String> = operations
        .iter()
        .filter(|o| blocks.contains(&path_key(&o.method, &o.path)))
        .map(DraftOperation::target)
        .collect();
    operations.retain(|o| !blocks.contains(&path_key(&o.method, &o.path)));
    warnings.retain(|w| w.operation.as_ref().is_none_or(|t| !replaced.contains(t)));
    Ok(Extracted {
        title: document_title(doc.title, source_name),
        servers: embedded.servers.clone(),
        operations,
        warnings,
        chunk_count: total,
        chunks_processed: processed,
        embedded,
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
    run_import_sources(vec![input], llm, opts, observer, resolver).await
}

/// Most sources of one import.
pub const MAX_IMPORT_SOURCES: usize = 100;

/// Where a source's operations came from.
struct SourceInfo {
    kind: ImportKind,
    name: Option<String>,
    url: Option<String>,
}

/// The name a source is reported by: its file name, or its URL with query
/// values redacted.
fn input_name(input: &ImportInput) -> String {
    match input {
        ImportInput::Bytes { kind, name, .. } => {
            name.clone().unwrap_or_else(|| kind.as_str().to_string())
        }
        ImportInput::Url(url) => fetch::validate_url(url)
            .map(|u| redact::redacted_url(&u))
            .unwrap_or_else(|_| "an invalid URL".to_string()),
    }
}

/// Fetches (for a URL) and extracts one source within `budget`.
async fn extract_source(
    input: ImportInput,
    llm: Option<&ExtractLlm>,
    opts: &ImportOptions,
    observer: &dyn ImportObserver,
    resolver: &dyn Resolver,
    budget: &mut TokenBudget,
) -> Result<(SourceInfo, Extracted), SmartError> {
    let (kind, bytes, source_name, source_url) = match input {
        ImportInput::Bytes { kind, name, bytes } => (kind, bytes, name, None),
        ImportInput::Url(url) => {
            report(observer, Phase::Fetching, 0, 1).await?;
            let fetched = fetch_with(&url, &opts.fetch, resolver).await?;
            let kind = fetched_kind(&fetched)?;
            // The real URL is only fetched; what is stored and prompted
            // with has its query values redacted.
            let name = fetch::validate_url(&url)
                .map(|u| redact::redacted_url(&u))
                .unwrap_or_default();
            let final_url = redact::redacted_url(&fetched.url);
            (kind, fetched.bytes, Some(name), Some(final_url))
        }
    };
    if bytes.len() > MAX_IMPORT_BYTES {
        return Err(SmartError::Unsupported(format!(
            "the input is larger than {} MiB",
            MAX_IMPORT_BYTES / (1024 * 1024)
        )));
    }

    let extracted = if kind.needs_llm() {
        extract_document(kind, bytes, source_name.as_deref(), llm, budget, observer).await?
    } else {
        let mut recorded = blocking(move || match kind {
            ImportKind::Har => har::import_har(&bytes),
            _ => postman::import_postman(&bytes),
        })
        .await?;
        if let Some(llm) = llm {
            let warnings =
                summarize_operations(llm, &mut recorded.operations, budget, observer).await?;
            recorded.warnings.extend(warnings);
        }
        Extracted {
            title: recorded.title,
            servers: recorded.servers,
            operations: recorded.operations,
            warnings: recorded.warnings,
            chunk_count: 0,
            chunks_processed: 0,
            embedded: Embedded::default(),
        }
    };
    let info = SourceInfo {
        kind,
        name: source_name,
        url: source_url,
    };
    Ok((info, extracted))
}

/// The extractions of several sources as one: operations merged (an
/// embedded block's operation wins over the others), servers united,
/// warnings and chunk counts summed. The first source with operations
/// names the draft.
fn combine(parts: Vec<(SourceInfo, Extracted)>) -> Option<(SourceInfo, Extracted)> {
    if parts.len() <= 1 {
        return parts.into_iter().next();
    }
    let first = parts
        .iter()
        .position(|(_, e)| !e.operations.is_empty() || !e.embedded.operations.is_empty())
        .unwrap_or(0);
    let mut info = None;
    let mut combined = Extracted {
        title: String::new(),
        servers: Vec::new(),
        operations: Vec::new(),
        warnings: Vec::new(),
        chunk_count: 0,
        chunks_processed: 0,
        embedded: Embedded::default(),
    };
    let mut seen_blocks = HashSet::new();
    for (i, (source, e)) in parts.into_iter().enumerate() {
        if i == first {
            combined.title = e.title;
            info = Some(source);
        }
        for server in e.servers {
            if !combined.servers.contains(&server) {
                combined.servers.push(server);
            }
        }
        combined.operations.extend(e.operations);
        combined.warnings.extend(e.warnings);
        combined.chunk_count += e.chunk_count;
        combined.chunks_processed += e.chunks_processed;
        for op in e.embedded.operations {
            if seen_blocks.insert(path_key(&op.method, &op.path)) {
                combined.embedded.operations.push(op);
            }
        }
        for (section, entries) in e.embedded.components {
            let target = combined
                .embedded
                .components
                .entry(section)
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let (Value::Object(target), Value::Object(entries)) = (target, entries) {
                for (name, value) in entries {
                    target.entry(name).or_insert(value);
                }
            }
        }
        for server in e.embedded.servers {
            if !combined.embedded.servers.contains(&server) {
                combined.embedded.servers.push(server);
            }
        }
    }
    let (mut operations, merge_warnings) =
        merge_operations(std::mem::take(&mut combined.operations));
    combined.warnings.extend(merge_warnings);
    operations.retain(|o| !seen_blocks.contains(&path_key(&o.method, &o.path)));
    combined.operations = operations;
    info.map(|info| (info, combined))
}

/// Every source extracted under one token budget, operations merged
/// ([`merge_operations`]), one draft assembled. A source that cannot be
/// fetched or read is skipped with a `source_skipped` warning; when all
/// fail, the first error is returned. [`SmartError::LlmRequired`] and
/// [`SmartError::Canceled`] end the import at once.
pub async fn run_import_sources(
    inputs: Vec<ImportInput>,
    llm: Option<&ExtractLlm>,
    opts: &ImportOptions,
    observer: &dyn ImportObserver,
    resolver: &dyn Resolver,
) -> Result<DraftResult, SmartError> {
    if inputs.len() > MAX_IMPORT_SOURCES {
        return Err(SmartError::Unsupported(format!(
            "an import takes at most {MAX_IMPORT_SOURCES} sources"
        )));
    }
    let names: Vec<String> = inputs.iter().map(input_name).collect();
    let mut budget = TokenBudget::new(opts.max_tokens);
    let mut parts = Vec::with_capacity(inputs.len());
    let mut skipped = Vec::new();
    let mut first_error = None;
    for (input, name) in inputs.into_iter().zip(&names) {
        match extract_source(input, llm, opts, observer, resolver, &mut budget).await {
            Ok(part) => parts.push(part),
            Err(e @ (SmartError::LlmRequired | SmartError::Canceled)) => return Err(e),
            Err(e) => {
                skipped.push(DraftWarning::new(
                    "source_skipped",
                    format!("{name} was skipped: {e}"),
                ));
                first_error.get_or_insert(e);
            }
        }
    }
    let Some((info, mut extracted)) = combine(parts) else {
        return Err(first_error.unwrap_or_else(|| SmartError::NoOperations(String::new())));
    };
    extracted.warnings.extend(skipped);
    let SourceInfo {
        kind,
        name: source_name,
        url: source_url,
    } = info;
    let sources = if names.len() > 1 { names } else { Vec::new() };

    extracted.operations = drop_low_confidence(
        std::mem::take(&mut extracted.operations),
        &mut extracted.warnings,
    );
    if extracted.operations.is_empty() && extracted.embedded.operations.is_empty() {
        // Operations left out for low confidence say why themselves.
        let low = extracted
            .warnings
            .iter()
            .any(|w| w.code == "low_confidence");
        let reasons: Vec<&str> = extracted
            .warnings
            .iter()
            .filter(|w| !low || w.code == "low_confidence")
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
    let Assembled {
        extracted,
        document,
        ids,
        embedded_ids,
        warnings: assembly_warnings,
        source,
        bundle,
    } = blocking(move || assemble(extracted)).await?;
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
        .chain(
            embedded_ids
                .into_iter()
                .zip(&extracted.embedded.operations)
                .map(|(id, op)| (id, op.meta())),
        )
        .collect();
    let mut warnings = extracted.warnings;
    warnings.extend(assembly_warnings);
    warnings.extend(assemble::schema_warnings(&bundle));
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
        sources,
        merged_from: None,
    };
    Ok(DraftResult {
        document,
        source,
        bundle,
        meta,
    })
}

/// Operations below this confidence are left out of the draft.
pub const MIN_CONFIDENCE: f32 = 0.4;

/// `ops` without those below [`MIN_CONFIDENCE`]; each one left out gets a
/// `low_confidence` warning with the reason: the existing warnings about
/// it, else a missing response shape, else the model's own doubt.
fn drop_low_confidence(
    ops: Vec<DraftOperation>,
    warnings: &mut Vec<DraftWarning>,
) -> Vec<DraftOperation> {
    let mut kept = Vec::with_capacity(ops.len());
    let mut dropped = Vec::new();
    for op in ops {
        if op.confidence >= MIN_CONFIDENCE {
            kept.push(op);
            continue;
        }
        let target = op.target();
        let known: Vec<&str> = warnings
            .iter()
            .filter(|w| w.operation.as_deref() == Some(target.as_str()))
            .map(|w| w.message.as_str())
            .collect();
        let reason = if !known.is_empty() {
            known.join("; ")
        } else if !op
            .responses
            .iter()
            .any(|r| r.schema.is_some() || !r.examples.is_empty())
        {
            "the documentation gives no response shape".to_string()
        } else {
            "the model was unsure".to_string()
        };
        dropped.push(
            DraftWarning::new(
                "low_confidence",
                format!(
                    "{target} was left out: confidence {:.2} is below 0.40 ({reason})",
                    op.confidence
                ),
            )
            .for_operation(target),
        );
    }
    warnings.extend(dropped);
    kept
}

/// A compiled draft before its metadata is attached.
struct Assembled {
    extracted: Extracted,
    document: Value,
    ids: Vec<String>,
    /// The ids of `extracted.embedded`'s operations, in order.
    embedded_ids: Vec<String>,
    warnings: Vec<DraftWarning>,
    source: Vec<u8>,
    bundle: Bundle,
}

/// The draft document of `extracted`: its operations assembled, its
/// embedded operations inserted; with the ids of both, and warnings.
fn build_document(extracted: &Extracted) -> (Value, Vec<String>, Vec<String>, Vec<DraftWarning>) {
    let (mut document, ids, warnings) =
        assemble_openapi(&extracted.title, &extracted.servers, &extracted.operations);
    let mut used: HashSet<String> = ids.iter().cloned().collect();
    let embedded_ids = insert_embedded(&mut document, &extracted.embedded, &mut used);
    (document, ids, embedded_ids, warnings)
}

/// Assembles and compiles the extracted operations, skipping the ones the
/// compiler's problems point at (§11.3); a problem elsewhere fails.
fn assemble(mut extracted: Extracted) -> Result<Assembled, SmartError> {
    let (mut document, mut ids, mut embedded_ids, mut assembly_warnings) =
        build_document(&extracted);
    let compiled = match compile_draft(&document) {
        Ok(compiled) => compiled,
        Err(issues) => {
            // Skip the operations the problems point at, never the job
            // (§11.3); a problem elsewhere still fails it.
            let bad = implicated_operations(&issues, &extracted.operations, &ids);
            let block_targets: Vec<(&str, &str)> = extracted
                .embedded
                .operations
                .iter()
                .map(|o| (o.method.as_str(), o.path.as_str()))
                .collect();
            let bad_blocks = implicated_targets(&issues, &block_targets, &embedded_ids);
            let skipped = bad.len() + bad_blocks.len();
            let total = extracted.operations.len() + extracted.embedded.operations.len();
            if skipped == 0 || skipped == total {
                return Err(compile_error(&issues));
            }
            let problems: Vec<String> = issues
                .iter()
                .take(3)
                .map(|i| format!("{}: {}", i.pointer, i.message))
                .collect();
            let mut targets: Vec<String> = Vec::with_capacity(skipped);
            for &i in bad.iter().rev() {
                targets.push(extracted.operations.remove(i).target());
            }
            for &i in bad_blocks.iter().rev() {
                targets.push(extracted.embedded.operations.remove(i).target());
            }
            for target in targets {
                extracted.warnings.push(
                    DraftWarning::new(
                        "invalid_operation",
                        format!(
                            "{target} was skipped because the draft does not compile with it ({})",
                            problems.join("; ")
                        ),
                    )
                    .for_operation(target),
                );
            }
            (document, ids, embedded_ids, assembly_warnings) = build_document(&extracted);
            compile_draft(&document).map_err(|issues| compile_error(&issues))?
        }
    };
    let (source, bundle) = compiled;
    Ok(Assembled {
        extracted,
        document,
        ids,
        embedded_ids,
        warnings: assembly_warnings,
        source,
        bundle,
    })
}

/// Runs CPU-bound work (parsing, extraction, assembly, compilation) on the
/// blocking pool, so a large input does not stall the async runtime. A
/// panic in `f` resumes on the caller.
async fn blocking<T, F>(f: F) -> Result<T, SmartError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, SmartError> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => match e.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            // Only a runtime shutdown cancels a blocking task.
            Err(_) => Err(SmartError::Canceled),
        },
    }
}
