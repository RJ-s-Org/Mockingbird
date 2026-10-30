//! The eval corpus: every contract under a directory that `mb_import`
//! compiles.

use std::path::Path;

use anyhow::Context;

/// One compiled contract of the corpus.
#[derive(Clone, Debug)]
pub struct CorpusEntry {
    /// `/`-separated path relative to the corpus root.
    pub name: String,
    pub bundle: mb_mcm::Bundle,
}

/// Compiles every contract under `root`, sorted by name.
///
/// Each file is compiled on its own with [`mb_import::compile`]. A
/// subdirectory that holds no contract file itself but several `.proto`
/// files, or a `.wsdl` with `.xsd` files beside it, is one multi-file
/// contract: it is packed and compiled through the archive path
/// ([`mb_import::read_source`]) and named after the directory. Hidden
/// entries are ignored. A file or directory that cannot be read or does
/// not compile is skipped with a warning ([`load_with_skipped`] names the
/// skipped contracts).
///
/// Fails only when `root` itself cannot be read.
pub fn load(root: &Path) -> anyhow::Result<Vec<CorpusEntry>> {
    load_with_skipped(root).map(|(entries, _)| entries)
}

/// [`load`], also returning the names of the contracts it skipped (they
/// could not be read or do not compile), sorted.
pub fn load_with_skipped(root: &Path) -> anyhow::Result<(Vec<CorpusEntry>, Vec<String>)> {
    std::fs::read_dir(root).with_context(|| format!("cannot read {}", root.display()))?;
    let mut out = Loaded::default();
    visit(root, "", &mut out);
    out.entries.sort_by(|a, b| a.name.cmp(&b.name));
    out.skipped.sort();
    Ok((out.entries, out.skipped))
}

/// What [`visit`] gathers.
#[derive(Default)]
struct Loaded {
    entries: Vec<CorpusEntry>,
    skipped: Vec<String>,
}

fn visit(dir: &Path, rel: &str, out: &mut Loaded) {
    let mut children: Vec<(String, std::path::PathBuf, bool)> = match std::fs::read_dir(dir) {
        Ok(read) => read
            .filter_map(|entry| match entry {
                Ok(entry) => Some(entry),
                Err(e) => {
                    tracing::warn!(dir = %dir.display(), error = %e, "skipping unreadable entry");
                    None
                }
            })
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_owned();
                if name.starts_with('.') {
                    return None;
                }
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                Some((name, entry.path(), is_dir))
            })
            .collect(),
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "skipping unreadable directory");
            return;
        }
    };
    children.sort();

    if !rel.is_empty() && is_multi_file_contract(dir) {
        compile_into(dir, rel.to_owned(), out);
        return;
    }
    for (name, path, is_dir) in children {
        let child_rel = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        if is_dir {
            visit(&path, &child_rel, out);
        } else {
            compile_into(&path, child_rel, out);
        }
    }
}

/// Whether `dir` is one contract spread over several files: it holds no
/// `.proto`/`.wsdl` file directly and its subtree holds two or more
/// `.proto` files, or it holds exactly one `.wsdl` next to `.xsd` files.
fn is_multi_file_contract(dir: &Path) -> bool {
    let direct = extensions(dir, false);
    let count = |all: &[String], ext: &str| all.iter().filter(|e| e.as_str() == ext).count();
    if count(&direct, "wsdl") == 1 && count(&direct, "xsd") > 0 {
        return true;
    }
    if count(&direct, "proto") > 0 || count(&direct, "wsdl") > 0 {
        return false;
    }
    count(&extensions(dir, true), "proto") >= 2
}

/// The lowercase extensions of the non-hidden files in `dir` (and its
/// subdirectories when `recursive`).
fn extensions(dir: &Path, recursive: bool) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() && recursive => out.extend(extensions(&path, true)),
            Ok(t) if t.is_file() => {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    out.push(ext.to_ascii_lowercase());
                }
            }
            _ => {}
        }
    }
    out
}

fn compile_into(path: &Path, name: String, out: &mut Loaded) {
    let bytes = match mb_import::read_source(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(contract = %name, error = %e, "skipping unreadable contract");
            out.skipped.push(name);
            return;
        }
    };
    match mb_import::compile(&bytes) {
        Ok(bundle) => out.entries.push(CorpusEntry { name, bundle }),
        Err(e) => {
            tracing::warn!(contract = %name, error = %e, "skipping contract that does not compile");
            out.skipped.push(name);
        }
    }
}
