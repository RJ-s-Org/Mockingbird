//! Bundling of multi-file OpenAPI, Swagger and AsyncAPI documents: `$ref`s
//! naming other uploaded files are resolved and the root document is turned
//! into one self-contained document (see
//! `docs/refinement/design-external-refs.md`). Nothing is fetched here:
//! in an upload, refs to URLs are left as they are; for a contract fetched
//! from a URL (the smart importer), refs resolve against the URL of the
//! document holding them, and the caller (`smart::remote_refs`) fetches the
//! documents they name between bundling passes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::openapi3::{escape, invalid};
use crate::schema::NAME_MAPS;
use crate::{ImportError, SourceFile, SourceFormat, json_len, parse_document};

/// Most bytes of content (compact JSON) the bundler places, counting a
/// target every time it is inlined.
const MAX_BUNDLED_BYTES: usize = 32 * 1024 * 1024;
/// Most nested external refs followed.
const MAX_DEPTH: usize = 64;

/// A ref target: a file path and a JSON pointer in it.
type Key = (String, String);

/// Where the definitions of a format live: their JSON pointer and the ref
/// prefix naming one.
fn defs_location(format: SourceFormat) -> Option<(&'static str, &'static str)> {
    match format {
        SourceFormat::OpenApi3 | SourceFormat::AsyncApi2 | SourceFormat::AsyncApi3 => {
            Some(("/components/schemas", "#/components/schemas/"))
        }
        SourceFormat::Swagger2 => Some(("/definitions", "#/definitions/")),
        _ => None,
    }
}

/// Whether a ref names a URL (it has a URI scheme, or is network-path
/// relative), which is never fetched.
fn is_url(r: &str) -> bool {
    if r.starts_with("//") {
        return true;
    }
    let Some((scheme, _)) = r.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    // A one-letter "scheme" is a Windows drive letter, which `resolve_path`
    // refuses as an absolute path.
    scheme.len() > 1
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Decodes `%XX` escapes; an invalid escape is kept as written.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

/// The directory part of `path` (empty for a top-level file).
fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Resolves the file part of a ref against the directory of `from`, folding
/// `.` and `..`; a path that is absolute, has a backslash or climbs above the
/// upload root is refused.
fn resolve_path(r: &str, rel: &str, from: &str) -> Result<String, ImportError> {
    let refuse = |why: &str| {
        invalid(
            r,
            format!(
                "external $ref {r} (in {from}) {why}; refs must name a file in the upload, relative to the file holding them"
            ),
        )
    };
    if rel.starts_with('/') || rel.contains('\\') || rel.split_once(':').is_some() {
        return Err(refuse("is not a relative path"));
    }
    let mut parts: Vec<&str> = dir_of(from).split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(refuse("climbs above the upload root"));
                }
            }
            seg => parts.push(seg),
        }
    }
    Ok(parts.join("/"))
}

/// Keeps only `[A-Za-z0-9._-]` of a definition name.
fn sanitize(name: &str) -> String {
    let out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() { "schema".into() } else { out }
}

/// The preferred definition name of a target: the last pointer segment, or
/// the file stem.
fn preferred_name(key: &Key) -> String {
    let (path, pointer) = key;
    let last = pointer
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.replace("~1", "/").replace("~0", "~"));
    let name = last.unwrap_or_else(|| {
        let file = path.rsplit('/').next().unwrap_or(path);
        file.split_once('.')
            .map_or(file, |(stem, _)| stem)
            .to_owned()
    });
    sanitize(&name)
}

/// What a JSON value is in the document.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pos {
    /// Part of the API description (paths, operations, components).
    Doc,
    /// A schema.
    Schema,
    /// An object mapping names to schemas (`properties`, the definitions
    /// section): its keys are names, not keywords.
    Names,
}

impl Pos {
    /// Whether the member `key` of an object at this position holds a
    /// literal (or an annotation) that is never followed.
    fn is_literal(self, key: &str) -> bool {
        match self {
            Pos::Names => false,
            // `value` is the literal of an Example object.
            Pos::Doc => key.starts_with("x-") || matches!(key, "example" | "value"),
            Pos::Schema => {
                key.starts_with("x-")
                    || matches!(key, "example" | "examples" | "enum" | "const" | "default")
            }
        }
    }

    /// The position of the member `key` of an object at this position.
    fn child(self, key: &str) -> Pos {
        match self {
            Pos::Names => Pos::Schema,
            Pos::Schema if NAME_MAPS.contains(&key) => Pos::Names,
            Pos::Schema => Pos::Schema,
            Pos::Doc if matches!(key, "schema" | "payload") => Pos::Schema,
            Pos::Doc => Pos::Doc,
        }
    }
}

/// Whether `doc` holds a ref that names another document (any ref not
/// starting with `#`).
#[cfg(feature = "smart")]
pub(crate) fn has_external_refs(doc: &Value) -> bool {
    let mut found = false;
    visit_refs(doc, Pos::Doc, &mut |r| found |= !r.starts_with('#'));
    found
}

/// Whether `doc` holds a ref that names another file (not a URL).
pub(crate) fn has_file_refs(doc: &Value) -> bool {
    let mut found = false;
    visit_refs(doc, Pos::Doc, &mut |r| {
        found |= !r.starts_with('#') && !is_url(r);
    });
    found
}

/// Calls `f` with every ref string of `value` outside literal members.
fn visit_refs(value: &Value, pos: Pos, f: &mut impl FnMut(&str)) {
    match value {
        Value::Object(m) => {
            for (k, v) in m {
                if k == "$ref"
                    && let Value::String(r) = v
                {
                    f(r);
                } else if !pos.is_literal(k) {
                    visit_refs(v, pos.child(k), f);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| visit_refs(v, pos, f)),
        _ => {}
    }
}

/// The uploaded files that `doc` (the file at `path`) references directly.
pub(crate) fn referenced_files(path: &str, doc: &Value) -> HashSet<String> {
    let mut out = HashSet::new();
    visit_refs(doc, Pos::Doc, &mut |r| {
        if r.starts_with('#') || is_url(r) {
            return;
        }
        let rel = r.split_once('#').map_or(r, |(p, _)| p);
        if let Ok(p) = resolve_path(r, &percent_decode(rel), path) {
            out.insert(p);
        }
    });
    out
}

/// Bundles `doc`, the root document at `root` of format `format`, against
/// `files`. `None` when the format has no definitions section.
pub(crate) fn bundle(
    root: &str,
    doc: &Value,
    format: SourceFormat,
    files: &[SourceFile],
) -> Result<Option<Value>, ImportError> {
    let files = files.iter().map(|f| (f.path.as_str(), f)).collect();
    match run(root, doc, format, files, HashMap::new(), Locator::Upload)? {
        Pass::Bundled(out) => Ok(out),
        #[cfg(feature = "smart")]
        Pass::Missing(_) => Ok(None),
    }
}

/// The outcome of a bundling pass.
pub(crate) enum Pass {
    /// The bundled document (`None` when the format has no definitions
    /// section).
    Bundled(Option<Value>),
    /// The documents the refs of a fetched contract name that are not
    /// fetched yet, by URL, each with the first ref naming it and the
    /// (redacted) URL of the document holding that ref.
    #[cfg(feature = "smart")]
    Missing(std::collections::BTreeMap<String, (String, String)>),
}

/// One bundling pass over `doc`, a contract fetched from the URL `root`,
/// against the documents fetched so far (`fetched`, by URL; `aliases` maps a
/// requested URL to the URL it was served from). Refs resolve against the
/// URL of the document holding them; a ref of another scheme than that
/// document's is an error. While a referenced document is missing, the pass
/// walks on with an empty placeholder in its place and returns every
/// missing document instead of a bundle.
#[cfg(feature = "smart")]
pub(crate) fn bundle_fetched(
    root: &str,
    doc: &Value,
    format: SourceFormat,
    fetched: HashMap<String, Arc<Value>>,
    aliases: &HashMap<String, String>,
) -> Result<Pass, ImportError> {
    let locator = Locator::Urls {
        aliases: aliases.clone(),
        missing: std::collections::BTreeMap::new(),
    };
    run(root, doc, format, HashMap::new(), fetched, locator)
}

fn run<'a>(
    root: &str,
    doc: &Value,
    format: SourceFormat,
    files: HashMap<&'a str, &'a SourceFile>,
    parsed: HashMap<String, Arc<Value>>,
    locator: Locator,
) -> Result<Pass, ImportError> {
    let Some((defs_pointer, defs_prefix)) = defs_location(format) else {
        return Ok(Pass::Bundled(None));
    };
    let root_defs = doc.pointer(defs_pointer).and_then(Value::as_object);
    let mut b = Bundler {
        files,
        parsed,
        locator,
        root: root.to_owned(),
        defs_pointer,
        defs_prefix,
        hoisted: HashMap::new(),
        aliases: HashSet::new(),
        taken: root_defs
            .map(|d| d.keys().cloned().collect())
            .unwrap_or_default(),
        new_defs: Vec::new(),
        stack: Vec::new(),
        budget: MAX_BUNDLED_BYTES,
    };
    // A root definition that is only an external ref takes the target's
    // content under its own name.
    if let Some(defs) = root_defs {
        for (name, v) in defs {
            if let Some(Value::String(r)) = v.get("$ref")
                && v.as_object().is_some_and(|o| o.len() == 1)
                && let Some(key) = b.key_of(r, root)?
                && key.0 != b.root
                && !b.hoisted.contains_key(&key)
            {
                b.hoisted.insert(key.clone(), name.clone());
                b.aliases.insert(name.clone());
            }
        }
    }
    let mut out = b.walk(doc, root, Pos::Doc, Some(""))?;
    #[cfg(feature = "smart")]
    if let Locator::Urls { missing, .. } = &mut b.locator
        && !missing.is_empty()
    {
        return Ok(Pass::Missing(std::mem::take(missing)));
    }
    if !b.new_defs.is_empty() {
        let section = defs_pointer
            .split('/')
            .skip(1)
            .try_fold(&mut out, |cur, seg| {
                let obj = cur.as_object_mut()?;
                Some(
                    obj.entry(seg.to_owned())
                        .or_insert_with(|| Value::Object(Map::new())),
                )
            })
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid(defs_pointer, "the definitions section is not an object"))?;
        for (name, def) in b.new_defs {
            section.insert(name, def);
        }
    }
    Ok(Pass::Bundled(Some(out)))
}

/// How refs name other documents.
enum Locator {
    /// Uploaded files: a path relative to the file holding the ref. Refs to
    /// URLs are left as they are.
    Upload,
    /// Fetched documents, keyed by URL.
    #[cfg(feature = "smart")]
    Urls {
        /// Requested URL -> the URL it was served from.
        aliases: HashMap<String, String>,
        /// Documents named but not fetched yet, as in [`Pass::Missing`].
        missing: std::collections::BTreeMap<String, (String, String)>,
    },
}

impl Locator {
    /// `key` as error messages show it: a URL with its query values
    /// redacted.
    fn shown(&self, key: &str) -> String {
        match self {
            Locator::Upload => key.to_owned(),
            #[cfg(feature = "smart")]
            Locator::Urls { .. } => url::Url::parse(key).map_or_else(
                |_| key.to_owned(),
                |u| crate::smart::redact::redacted_url(&u),
            ),
        }
    }
}

struct Bundler<'a> {
    files: HashMap<&'a str, &'a SourceFile>,
    parsed: HashMap<String, Arc<Value>>,
    locator: Locator,
    root: String,
    defs_pointer: &'static str,
    defs_prefix: &'static str,
    /// Targets placed as definitions, by name.
    hoisted: HashMap<Key, String>,
    /// Root definitions that are filled with a target's content.
    aliases: HashSet<String>,
    /// Definition names in use.
    taken: HashSet<String>,
    /// Hoisted definitions, in the order they were met.
    new_defs: Vec<(String, Value)>,
    /// Targets being inlined or hoisted, outermost first.
    stack: Vec<Key>,
    budget: usize,
}

impl Bundler<'_> {
    /// The target of `r` held by `from`; `None` for a URL, or for a local ref
    /// of the root.
    fn key_of(&self, r: &str, from: &str) -> Result<Option<Key>, ImportError> {
        if from == self.root && r.starts_with('#') {
            return Ok(None);
        }
        let (rel, pointer) = r.split_once('#').unwrap_or((r, ""));
        let path = match &self.locator {
            Locator::Upload if is_url(r) => return Ok(None),
            _ if rel.is_empty() => from.to_owned(),
            Locator::Upload => resolve_path(r, &percent_decode(rel), from)?,
            #[cfg(feature = "smart")]
            Locator::Urls { aliases, .. } => {
                let url = self.join(r, rel, from)?;
                aliases.get(&url).cloned().unwrap_or(url)
            }
        };
        Ok(Some((path, percent_decode(pointer))))
    }

    /// The URL (without fragment) that `rel`, the file part of the ref `r`,
    /// names, resolved against `from`, the URL of the document holding it.
    /// Only a ref of the same scheme as `from` is accepted.
    #[cfg(feature = "smart")]
    fn join(&self, r: &str, rel: &str, from: &str) -> Result<String, ImportError> {
        let shown = self.locator.shown(from);
        let base = url::Url::parse(from)
            .map_err(|e| invalid(r, format!("external $ref {r} (in {shown}): {e}")))?;
        let mut url = base.join(rel).map_err(|e| {
            invalid(
                r,
                format!("external $ref {r} (in {shown}) is not a valid URL reference: {e}"),
            )
        })?;
        if url.scheme() != base.scheme() {
            return Err(invalid(
                r,
                format!(
                    "external $ref {r} (in {shown}) uses the {} scheme; a contract fetched over {} \
                     follows only {} refs",
                    url.scheme(),
                    base.scheme(),
                    base.scheme()
                ),
            ));
        }
        url.set_fragment(None);
        Ok(url.to_string())
    }

    /// The parsed document of an uploaded file or a fetched URL; `None`
    /// (and recorded) while a fetched contract's document is missing.
    fn document(
        &mut self,
        path: &str,
        r: &str,
        from: &str,
    ) -> Result<Option<Arc<Value>>, ImportError> {
        if let Some(doc) = self.parsed.get(path) {
            return Ok(Some(Arc::clone(doc)));
        }
        #[cfg(feature = "smart")]
        if matches!(self.locator, Locator::Urls { .. }) {
            let shown = self.locator.shown(from);
            if let Locator::Urls { missing, .. } = &mut self.locator {
                missing
                    .entry(path.to_owned())
                    .or_insert_with(|| (r.to_owned(), shown));
            }
            return Ok(None);
        }
        let file = self.files.get(path).ok_or_else(|| {
            invalid(
                r,
                format!("external $ref {r} (in {from}) names {path}, which is not in the upload"),
            )
        })?;
        let doc = parse_document(&file.bytes).map_err(|e| {
            invalid(
                path,
                format!("{path}, referenced by $ref {r} in {from}, is not JSON or YAML: {e}"),
            )
        })?;
        let doc = Arc::new(doc);
        self.parsed.insert(path.to_owned(), Arc::clone(&doc));
        Ok(Some(doc))
    }

    /// The value a ref names.
    fn target(&mut self, key: &Key, r: &str, from: &str) -> Result<Value, ImportError> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(invalid(
                r,
                format!(
                    "external $ref {r} (in {}): refs nest deeper than {MAX_DEPTH}",
                    self.locator.shown(from)
                ),
            ));
        }
        let (path, pointer) = key;
        let Some(doc) = self.document(path, r, from)? else {
            return Ok(Value::Object(Map::new()));
        };
        let value = doc.pointer(pointer).cloned().ok_or_else(|| {
            invalid(
                r,
                format!(
                    "external $ref {r} (in {}): {} has no {pointer}",
                    self.locator.shown(from),
                    self.locator.shown(path)
                ),
            )
        })?;
        let len = json_len(&value);
        self.budget = self.budget.checked_sub(len).ok_or_else(|| {
            invalid(
                r,
                format!(
                    "external $ref {r} (in {}): the bundled contract exceeds {} MiB",
                    self.locator.shown(from),
                    MAX_BUNDLED_BYTES / (1024 * 1024)
                ),
            )
        })?;
        Ok(value)
    }

    /// A fresh definition name for `key`.
    fn claim_name(&mut self, key: &Key) -> String {
        let base = preferred_name(key);
        let mut name = base.clone();
        let mut n = 2;
        while self.taken.contains(&name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        self.taken.insert(name.clone());
        name
    }

    fn local_ref(&self, name: &str) -> Value {
        Value::String(format!("{}{}", self.defs_prefix, escape(name)))
    }

    /// Bundles `value`, held by the file `file`, at position `pos`. While
    /// walking the root towards its definitions section, `at` is `value`'s
    /// pointer.
    fn walk(
        &mut self,
        value: &Value,
        file: &str,
        pos: Pos,
        at: Option<&str>,
    ) -> Result<Value, ImportError> {
        match value {
            Value::Object(obj) => {
                if let Some(Value::String(r)) = obj.get("$ref") {
                    return self.walk_ref(obj, r, file, pos);
                }
                let in_defs = at == Some(self.defs_pointer);
                let mut out = Map::new();
                for (k, v) in obj {
                    if pos.is_literal(k) {
                        out.insert(k.clone(), v.clone());
                        continue;
                    }
                    let new = if in_defs && self.aliases.contains(k) {
                        self.walk_alias(v, file)?
                    } else {
                        // The child's pointer, while it is on the way to the
                        // definitions section.
                        let child_at = at.map(|a| format!("{a}/{}", escape(k))).filter(|c| {
                            self.defs_pointer
                                .strip_prefix(c.as_str())
                                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
                        });
                        let child_pos = if child_at.as_deref() == Some(self.defs_pointer) {
                            Pos::Names
                        } else {
                            pos.child(k)
                        };
                        self.walk(v, file, child_pos, child_at.as_deref())?
                    };
                    out.insert(k.clone(), new);
                }
                Ok(Value::Object(out))
            }
            Value::Array(items) => items
                .iter()
                .map(|v| self.walk(v, file, pos, None))
                .collect::<Result<_, _>>()
                .map(Value::Array),
            other => Ok(other.clone()),
        }
    }

    /// The content of a root definition that is only an external ref.
    fn walk_alias(&mut self, v: &Value, file: &str) -> Result<Value, ImportError> {
        let r = v.get("$ref").and_then(Value::as_str).unwrap_or_default();
        let Some(key) = self.key_of(r, file)? else {
            return Ok(v.clone());
        };
        self.place(key, r, file, Pos::Schema)
    }

    /// The bundled content of the target `key` of the ref `r` in `file`.
    fn place(&mut self, key: Key, r: &str, file: &str, pos: Pos) -> Result<Value, ImportError> {
        let target = self.target(&key, r, file)?;
        let held_by = key.0.clone();
        self.stack.push(key);
        let out = self.walk(&target, &held_by, pos, None);
        self.stack.pop();
        out
    }

    /// Bundles a ref object at position `pos`.
    fn walk_ref(
        &mut self,
        obj: &Map<String, Value>,
        r: &str,
        file: &str,
        pos: Pos,
    ) -> Result<Value, ImportError> {
        // The siblings of the ref, bundled.
        let mut siblings = Map::new();
        for (k, v) in obj {
            if k == "$ref" {
                continue;
            }
            let new = if pos.is_literal(k) {
                v.clone()
            } else {
                self.walk(v, file, pos.child(k), None)?
            };
            siblings.insert(k.clone(), new);
        }
        let with_ref = |r: Value, mut siblings: Map<String, Value>| {
            siblings.insert("$ref".into(), r);
            Value::Object(siblings)
        };
        let Some(key) = self.key_of(r, file)? else {
            return Ok(with_ref(Value::String(r.to_owned()), siblings));
        };
        if key.0 == self.root {
            return Ok(with_ref(Value::String(format!("#{}", key.1)), siblings));
        }
        if let Some(name) = self.hoisted.get(&key) {
            return Ok(with_ref(self.local_ref(name), siblings));
        }
        if pos != Pos::Doc {
            let name = self.claim_name(&key);
            self.hoisted.insert(key.clone(), name.clone());
            let def = self.place(key, r, file, Pos::Schema)?;
            self.new_defs.push((name.clone(), def));
            return Ok(with_ref(self.local_ref(&name), siblings));
        }
        if self.stack.contains(&key) {
            return Err(invalid(
                r,
                format!(
                    "external $ref {r} (in {}) is cyclic outside a schema, which cannot be bundled into one document",
                    self.locator.shown(file)
                ),
            ));
        }
        match self.place(key, r, file, Pos::Doc)? {
            Value::Object(mut content) => {
                content.extend(siblings);
                Ok(Value::Object(content))
            }
            other => Ok(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_url, percent_decode, preferred_name, resolve_path};

    #[test]
    fn urls_and_paths() {
        assert!(is_url("https://x/y.yaml"));
        assert!(is_url("//x/y.yaml"));
        assert!(is_url("file:///etc/passwd"));
        assert!(!is_url("./pet.yaml#/Pet"));
        assert!(!is_url("pet.yaml"));
        assert!(!is_url("C:/pet.yaml"));
        assert_eq!(
            resolve_path("r", "../b/c.yaml", "a/x.yaml").unwrap(),
            "b/c.yaml"
        );
        assert_eq!(
            resolve_path("r", "./c.yaml", "a/x.yaml").unwrap(),
            "a/c.yaml"
        );
        assert!(resolve_path("r", "../c.yaml", "x.yaml").is_err());
        assert!(resolve_path("r", "C:/c.yaml", "x.yaml").is_err());
        assert_eq!(percent_decode("a%20b%zz"), "a b%zz");
        assert_eq!(
            preferred_name(&("s/pet.v1.yaml".into(), String::new())),
            "pet"
        );
        assert_eq!(
            preferred_name(&("s.yaml".into(), "/a~1b c".into())),
            "a_b_c"
        );
    }
}
