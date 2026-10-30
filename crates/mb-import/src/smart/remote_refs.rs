//! The external `$ref`s of a contract fetched from a URL: resolved against
//! the URL of the document holding them, and every document they name
//! fetched through the same guarded fetcher (and [`FetchPolicy`]) as the
//! contract.
//!
//! Resolution runs in passes: each pass bundles the contract against the
//! documents fetched so far (`refs::bundle_fetched`, the bundler of
//! multi-file uploads) and names the documents still missing, which are then
//! fetched. A pass therefore reaches one document level deeper than the one
//! before. Each document is fetched once, so cyclic refs end; the bundler
//! turns cyclic schemas into local recursive refs.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::Value;
use url::Url;

use super::fetch::{FetchPolicy, Fetched, Resolver, fetch_with};
use super::redact::redacted_url;
use super::{MAX_IMPORT_BYTES, MAX_REF_DEPTH, MAX_REF_DOCUMENTS, SmartError, blocking};
use crate::refs::{self, Pass};
use crate::{SourceFormat, parse_document};

/// Referenced documents fetched at the same time.
const CONCURRENT_FETCHES: usize = 4;

/// `url` without its fragment, as documents are keyed.
fn key(url: &Url) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    url.to_string()
}

/// `doc`, the contract `root` was fetched as (from the URL `requested`),
/// with its external refs bundled in. Unchanged when it has none.
///
/// The referenced documents are fetched with `policy` (address checks,
/// allowlist, redirects, size and timeout per document) through
/// `resolver`; all of them together take at most `policy.timeout`, hold at
/// most [`MAX_IMPORT_BYTES`] with the contract, number at most
/// [`MAX_REF_DOCUMENTS`] and lie at most [`MAX_REF_DEPTH`] documents deep.
///
/// # Errors
///
/// [`SmartError::Fetch`] when a referenced document cannot be fetched or a
/// limit is passed; [`SmartError::Unreadable`] when a ref cannot be resolved
/// (another scheme, a document that is not JSON or YAML, a missing
/// pointer).
pub(crate) async fn bundle_refs(
    requested: &str,
    root: &Fetched,
    doc: Value,
    format: SourceFormat,
    policy: &FetchPolicy,
    resolver: &dyn Resolver,
) -> Result<Value, SmartError> {
    if !refs::has_external_refs(&doc) {
        return Ok(doc);
    }
    tokio::time::timeout(
        policy.timeout,
        resolve(requested, root, doc, format, policy, resolver),
    )
    .await
    .map_err(|_| {
        SmartError::Fetch(format!(
            "fetching the documents the contract's $refs name timed out after {} s",
            policy.timeout.as_secs_f32()
        ))
    })?
}

async fn resolve(
    requested: &str,
    root: &Fetched,
    doc: Value,
    format: SourceFormat,
    policy: &FetchPolicy,
    resolver: &dyn Resolver,
) -> Result<Value, SmartError> {
    let root_key = key(&root.url);
    let mut aliases = HashMap::new();
    if let Ok(url) = Url::parse(requested.trim()) {
        aliases.insert(key(&url), root_key.clone());
    }
    let doc = Arc::new(doc);
    let mut fetched: HashMap<String, Arc<Value>> = HashMap::new();
    let mut documents = 0usize;
    let mut bytes = root.bytes.len();
    for depth in 0..=MAX_REF_DEPTH {
        let pass = {
            let (root_key, doc, fetched, aliases) = (
                root_key.clone(),
                Arc::clone(&doc),
                fetched.clone(),
                aliases.clone(),
            );
            blocking(move || {
                refs::bundle_fetched(&root_key, &doc, format, fetched, &aliases)
                    .map_err(|e| SmartError::Unreadable(e.to_string()))
            })
            .await?
        };
        let missing = match pass {
            Pass::Bundled(Some(out)) => return Ok(out),
            Pass::Bundled(None) => return Ok(Arc::unwrap_or_clone(doc)),
            Pass::Missing(missing) => missing,
        };
        if depth == MAX_REF_DEPTH {
            return Err(SmartError::Fetch(format!(
                "the contract's $refs nest more than {MAX_REF_DEPTH} documents deep"
            )));
        }
        documents += missing.len();
        if documents > MAX_REF_DOCUMENTS {
            return Err(SmartError::Fetch(format!(
                "the contract's $refs name more than {MAX_REF_DOCUMENTS} documents"
            )));
        }
        let mut results = futures::stream::iter(missing)
            .map(|(url, (r, from))| async move {
                let result = fetch_with(&url, policy, resolver).await;
                (url, r, from, result)
            })
            .buffer_unordered(CONCURRENT_FETCHES);
        while let Some((url, r, from, result)) = results.next().await {
            let shown = Url::parse(&url).map_or_else(|_| url.clone(), |u| redacted_url(&u));
            let names = format!("$ref {r} (in {from}, naming {shown})");
            let page = result.map_err(|e| SmartError::Fetch(format!("{names}: {e}")))?;
            bytes = bytes.saturating_add(page.bytes.len());
            if bytes > MAX_IMPORT_BYTES {
                return Err(SmartError::Fetch(format!(
                    "the contract and the documents its $refs name exceed {} MiB",
                    MAX_IMPORT_BYTES / (1024 * 1024)
                )));
            }
            let served = key(&page.url);
            let body = page.bytes;
            let value = blocking(move || {
                parse_document(&body).map_err(|e| {
                    SmartError::Unreadable(format!(
                        "{names}: the document is not JSON or YAML: {e}"
                    ))
                })
            })
            .await?;
            aliases.insert(url, served.clone());
            fetched.entry(served).or_insert_with(|| Arc::new(value));
        }
    }
    // The last pass returns either way.
    Err(SmartError::Fetch(format!(
        "the contract's $refs nest more than {MAX_REF_DEPTH} documents deep"
    )))
}
