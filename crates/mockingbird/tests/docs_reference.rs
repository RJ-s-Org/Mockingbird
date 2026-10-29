//! The operator documentation stays in step with the code: the full example
//! config sets every key and loads, `docs/configuration.md` documents every
//! key path, and every relative markdown link resolves.
#![cfg(test)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use mockingbird::file_config::{self, FileConfig};
use serde_json::Value;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn full_example() -> FileConfig {
    file_config::load(&repo().join("docs/examples/mockingbird.full.yaml")).unwrap()
}

/// Paths whose object keys are user-chosen names, collapsed to `<name>`.
const MAP_PATHS: &[&str] = &[
    "brokers",
    "mocks",
    "llm.profiles",
    "llm.profiles.<name>.headers",
    "mocks.<name>.scenarios",
    "mocks.<name>.scenarios.<name>.headers",
    "mocks.<name>.channels",
];

/// Paths holding free-form JSON, not config keys.
const OPAQUE_PATHS: &[&str] = &[
    "mocks.<name>.scenarios.<name>.set",
    "llm.profiles.<name>.extra_body",
];

/// Every key path in `value` below `prefix`: map entries become `<name>`
/// and array items `[]` (`pii.custom[].name`).
fn key_paths(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    if OPAQUE_PATHS.contains(&prefix) {
        return;
    }
    match value {
        Value::Object(map) => {
            let is_map = MAP_PATHS.contains(&prefix);
            for (key, child) in map {
                let segment = if is_map { "<name>" } else { key.as_str() };
                let path = if prefix.is_empty() {
                    segment.to_string()
                } else {
                    format!("{prefix}.{segment}")
                };
                out.insert(path.clone());
                key_paths(child, &path, out);
            }
        }
        Value::Array(items) => {
            let path = format!("{prefix}[]");
            for item in items {
                key_paths(item, &path, out);
            }
        }
        _ => {}
    }
}

fn paths_of(cfg: &FileConfig) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    key_paths(&serde_json::to_value(cfg).unwrap(), "", &mut out);
    out
}

#[test]
fn full_example_loads() {
    let cfg = full_example();
    assert!(!cfg.mocks.is_empty(), "no mock");
    assert!(!cfg.brokers.is_empty(), "no broker");
    assert!(!cfg.llm.profiles.is_empty(), "no LLM profile");
    assert!(
        cfg.mocks.values().any(|m| !m.scenarios.is_empty()),
        "no scenario"
    );
    cfg.validate_ranges().unwrap();

    // Every key the defaults have is set, and every `Option` is set (not
    // `null`) in at least one entry of each map.
    let full = paths_of(&cfg);
    let defaults = paths_of(&FileConfig::default());
    let missing: Vec<_> = defaults.difference(&full).collect();
    assert!(missing.is_empty(), "keys not set: {missing:?}");
    let (mut nulls, mut set) = (BTreeSet::new(), BTreeSet::new());
    scan_values(
        &serde_json::to_value(&cfg).unwrap(),
        "",
        &mut nulls,
        &mut set,
    );
    let unset: Vec<_> = nulls.difference(&set).collect();
    assert!(unset.is_empty(), "keys left unset (null): {unset:?}");
}

/// Sorts the key paths of `value` into those holding `null` somewhere and
/// those holding a value somewhere (map entries collapsed to `<name>`).
fn scan_values(
    value: &Value,
    prefix: &str,
    nulls: &mut BTreeSet<String>,
    set: &mut BTreeSet<String>,
) {
    if OPAQUE_PATHS.contains(&prefix) {
        set.insert(prefix.to_string());
        return;
    }
    match value {
        Value::Null => {
            nulls.insert(prefix.to_string());
        }
        Value::Object(map) => {
            set.insert(prefix.to_string());
            let is_map = MAP_PATHS.contains(&prefix);
            for (key, child) in map {
                let segment = if is_map { "<name>" } else { key.as_str() };
                let path = if prefix.is_empty() {
                    segment.to_string()
                } else {
                    format!("{prefix}.{segment}")
                };
                scan_values(child, &path, nulls, set);
            }
        }
        Value::Array(items) => {
            set.insert(prefix.to_string());
            for item in items {
                scan_values(item, &format!("{prefix}[]"), nulls, set);
            }
        }
        _ => {
            set.insert(prefix.to_string());
        }
    }
}

#[test]
fn every_config_key_is_documented() {
    let doc = read(&repo().join("docs/configuration.md"));
    let mut paths = paths_of(&FileConfig::default());
    paths.extend(paths_of(&full_example()));
    assert!(paths.contains("mocks.<name>.chaos.error_rate"), "{paths:?}");
    let undocumented: Vec<_> = paths
        .iter()
        .filter(|p| !doc.contains(&format!("`{p}`")))
        .collect();
    assert!(
        undocumented.is_empty(),
        "docs/configuration.md does not document: {undocumented:?}"
    );
}

/// Markdown files under `dir`, recursively.
fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            markdown_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

/// Inline link targets (`[text](target)`) outside fenced code blocks and
/// code spans.
fn link_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        // Drop code spans so `[a](b)` inside backticks is not a link.
        let mut plain = String::new();
        let mut in_code = false;
        for c in line.chars() {
            if c == '`' {
                in_code = !in_code;
            } else if !in_code {
                plain.push(c);
            }
        }
        let mut rest = plain.as_str();
        while let Some(i) = rest.find("](") {
            rest = &rest[i + 2..];
            let Some(end) = rest.find(')') else { break };
            let target = rest[..end].split_whitespace().next().unwrap_or_default();
            targets.push(target.to_string());
            rest = &rest[end..];
        }
    }
    targets
}

#[test]
fn docs_links_resolve() {
    let mut files = vec![repo().join("README.md")];
    markdown_files(&repo().join("docs"), &mut files);
    let mut broken = Vec::new();
    let mut checked = 0;
    for file in &files {
        let dir = file.parent().unwrap();
        for target in link_targets(&read(file)) {
            let target = target.trim_start_matches('<').trim_end_matches('>');
            if target.is_empty()
                || target.starts_with('#')
                || target.contains("://")
                || target.starts_with("mailto:")
            {
                continue;
            }
            let path = target.split('#').next().unwrap_or_default();
            checked += 1;
            if !dir.join(path).exists() {
                broken.push(format!("{}: {target}", file.display()));
            }
        }
    }
    assert!(checked > 10, "only {checked} relative links found");
    assert!(broken.is_empty(), "broken links: {broken:#?}");
}

/// The Kubernetes guide claims only what the chart and the release
/// workflow actually do.
#[test]
fn kubernetes_guide_matches_chart_and_release() {
    let guide = read(&repo().join("docs/deploy/kubernetes.md"));
    let release = read(&repo().join(".github/workflows/release.yml"));
    let helpers = read(&repo().join("deploy/helm/mockingbird/templates/_helpers.tpl"));
    // Binaries are signed only when the workflow signs blobs.
    if !release.contains("cosign sign-blob") {
        assert!(
            !guide.contains("the binaries are signed"),
            "the guide claims signed binaries; release.yml gives them provenance and sha256 only"
        );
        assert!(guide.contains("Binary archives are not signed"));
        assert!(release.contains("SHA256SUMS") && guide.contains("SHA256SUMS"));
    }
    // TLS in the pod needs HTTPS probes (and a way to mount the files).
    if !helpers.contains("scheme: HTTPS") {
        assert!(
            !guide.contains("end-to-end TLS"),
            "the guide offers server.tls in the pods; the chart's probes are plain HTTP"
        );
    }
}
