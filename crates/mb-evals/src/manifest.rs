//! A workload manifest: an explicit list of contracts, each tagged with its
//! API type and domain, optionally naming the exact targets to call.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;

use crate::report::{EvalContract, default_targets, select_targets};

/// The manifest file: `{"entries": [...]}`.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Manifest {
    pub entries: Vec<ManifestEntry>,
}

/// One contract of a manifest.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct ManifestEntry {
    /// Unique; keys `per_contract` in the report.
    pub name: String,
    /// The contract file or multi-file directory, relative to the root the
    /// manifest is loaded against (the repository root).
    pub path: PathBuf,
    /// `rest`, `graphql`, `grpc`, `soap` or `asyncapi`.
    pub api_type: String,
    /// `payments`, `ecommerce`, `social-media`, `devtools`, `travel` or
    /// `generic`.
    pub domain: String,
    /// Operation or publish channel ids to call, exactly; absent or empty
    /// picks targets by the corpus rule, capped at `max_ops`.
    #[serde(default)]
    pub operations: Option<Vec<String>>,
    /// The cap of the corpus rule; absent uses the run's default.
    #[serde(default)]
    pub max_ops: Option<usize>,
}

/// Parses manifest JSON. Fails on malformed JSON, an empty or duplicate
/// name, or an empty `api_type`/`domain`.
pub fn parse(text: &str) -> anyhow::Result<Manifest> {
    let manifest: Manifest = serde_json::from_str(text).context("invalid manifest JSON")?;
    let mut names = BTreeSet::new();
    for entry in &manifest.entries {
        if entry.name.is_empty() {
            bail!("a manifest entry has an empty name");
        }
        if entry.api_type.is_empty() || entry.domain.is_empty() {
            bail!(
                "manifest entry {} needs an api_type and a domain",
                entry.name
            );
        }
        if !names.insert(entry.name.as_str()) {
            bail!("manifest entry {} appears twice", entry.name);
        }
    }
    Ok(manifest)
}

/// Reads the manifest at `path` and compiles each entry's contract (its
/// `path` joined to `root`), selecting its targets. Unlike the corpus, a
/// contract that cannot be read or compiled, or an unknown target id, fails
/// the whole load: a manifest is a deliberate list.
pub fn load(path: &Path, root: &Path, default_max_ops: usize) -> anyhow::Result<Vec<EvalContract>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let manifest = parse(&text).with_context(|| format!("in {}", path.display()))?;
    manifest
        .entries
        .into_iter()
        .map(|entry| compile_entry(entry, root, default_max_ops))
        .collect()
}

/// Compiles one entry against `root` and selects its targets.
pub fn compile_entry(
    entry: ManifestEntry,
    root: &Path,
    default_max_ops: usize,
) -> anyhow::Result<EvalContract> {
    let contract = root.join(&entry.path);
    let source = mb_import::read_source(&contract)
        .with_context(|| format!("{}: cannot read {}", entry.name, contract.display()))?;
    let bundle = mb_import::compile(&source)
        .with_context(|| format!("{}: cannot compile {}", entry.name, contract.display()))?;
    let targets = match entry.operations.as_deref() {
        Some(ids) if !ids.is_empty() => {
            select_targets(&bundle, ids).map_err(|e| anyhow!("{}: {e}", entry.name))?
        }
        _ => default_targets(&bundle, entry.max_ops.unwrap_or(default_max_ops)),
    };
    Ok(EvalContract {
        name: entry.name,
        bundle: Arc::new(bundle),
        api_type: Some(entry.api_type),
        domain: Some(entry.domain),
        targets,
    })
}
