//! OpenAPI 3.x documents embedded in documentation pages: their operations
//! are taken as written (the formal import path) instead of asking the LLM
//! to re-derive them from the page's prose.
//!
//! Where the documents are found:
//!
//! 1. HTML: the text of `script[type="application/json"]`,
//!    `script#__NEXT_DATA__` and `script#ssr-props` (ReadMe pages, e.g.
//!    developer.themoviedb.org, ship the page's definition there as
//!    `document.api.schema`, in a `type="application/x-ssr-props"` script),
//!    and the value of any `data-initial-props` attribute, each read as JSON.
//! 2. Code blocks (Markdown fences, HTML `<pre>`) that parse as JSON or YAML.
//! 3. In every value read, depth first (at most [`MAX_WALK_DEPTH`] deep and
//!    [`MAX_WALK_NODES`] nodes): an object with an `openapi` string starting
//!    `3.` and an object `paths` is a document; a long string that looks
//!    like stringified JSON naming `"openapi"` is parsed and walked too
//!    (ReadMe's `oasDefinition`). Documents with the same paths count once.
//!
//! A Swagger 2.0 block is not converted: its text stays for the LLM, with
//! an `embedded_swagger2` warning.

use std::collections::{BTreeSet, HashSet};

use scraper::{Html, Selector};
use serde_json::{Map, Value, json};

use super::ImportKind;
use super::assemble::assign_id_parts;
use super::model::{DraftWarning, OperationMeta, Provenance};
use super::paths::{normalize_path, path_key};
use super::text::{BlockKind, TextDoc, is_endpoint_line};

/// Deepest value walked for embedded documents.
const MAX_WALK_DEPTH: usize = 64;
/// Most values walked for embedded documents, per page.
const MAX_WALK_NODES: usize = 200_000;
/// Shortest string parsed as stringified JSON.
const MIN_STRINGIFIED: usize = 50;
/// Longest provenance excerpt of a block operation, in chars.
const MAX_EXCERPT: usize = 300;
/// Scripts whose text may hold page data.
const SCRIPT_SELECTOR: &str =
    r#"script[type="application/json"], script#__NEXT_DATA__, script#ssr-props"#;
const PROPS_ATTRIBUTE: &str = "data-initial-props";
/// Provenance location of a block operation.
pub const EMBEDDED_LOCATION: &str = "embedded OpenAPI";
const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// One operation of an embedded document.
#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddedOperation {
    /// Upper case.
    pub method: String,
    /// The block's path template.
    pub path: String,
    /// The OpenAPI operation object, path-level parameters merged in.
    pub operation: Value,
    /// At most 300 chars of the block, for provenance.
    pub excerpt: String,
}

impl EmbeddedOperation {
    /// `"GET /pets"`.
    pub fn target(&self) -> String {
        format!("{} {}", self.method, self.path)
    }

    /// The review metadata of the operation: certain, from the block.
    pub fn meta(&self) -> OperationMeta {
        OperationMeta {
            confidence: 1.0,
            provenance: vec![Provenance {
                chunk: None,
                location: EMBEDDED_LOCATION.into(),
                excerpt: self.excerpt.clone(),
            }],
            edited: false,
        }
    }
}

/// What the embedded documents of a page hold.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Embedded {
    pub operations: Vec<EmbeddedOperation>,
    /// `components` of the blocks merged (first definition of a name wins).
    pub components: Map<String, Value>,
    pub servers: Vec<String>,
    pub warnings: Vec<DraftWarning>,
}

/// What walking values found.
#[derive(Default)]
struct Found {
    documents: Vec<Value>,
    swagger2: bool,
    nodes: usize,
}

impl Found {
    fn walk(&mut self, value: &Value, depth: usize) {
        if depth > MAX_WALK_DEPTH || self.nodes >= MAX_WALK_NODES {
            return;
        }
        self.nodes += 1;
        match value {
            Value::Object(obj) => {
                if obj.get("paths").is_some_and(Value::is_object) {
                    let openapi = obj.get("openapi").and_then(Value::as_str);
                    if openapi.is_some_and(|v| v.starts_with("3.")) {
                        self.documents.push(value.clone());
                        return;
                    }
                    if obj.get("swagger").and_then(Value::as_str) == Some("2.0") {
                        self.swagger2 = true;
                        return;
                    }
                }
                for child in obj.values() {
                    self.walk(child, depth + 1);
                }
            }
            Value::Array(items) => {
                for child in items {
                    self.walk(child, depth + 1);
                }
            }
            Value::String(s)
                if s.len() > MIN_STRINGIFIED
                    && s.trim_start().starts_with('{')
                    && s.contains("\"openapi\"") =>
            {
                if let Ok(inner) = serde_json::from_str::<Value>(s) {
                    self.walk(&inner, depth + 1);
                }
            }
            _ => {}
        }
    }
}

/// The JSON texts of an HTML page's data scripts and props attributes.
fn html_data(bytes: &[u8]) -> Vec<String> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let page = Html::parse_document(text);
    let mut out = Vec::new();
    if let Ok(scripts) = Selector::parse(SCRIPT_SELECTOR) {
        out.extend(page.select(&scripts).map(|s| s.text().collect::<String>()));
    }
    if let Ok(props) = Selector::parse(&format!("[{PROPS_ATTRIBUTE}]")) {
        out.extend(
            page.select(&props)
                .filter_map(|e| e.value().attr(PROPS_ATTRIBUTE))
                .map(str::to_string),
        );
    }
    out
}

/// Finds OpenAPI 3.x documents in `bytes` (HTML scripts) and in `doc`'s
/// code blocks; the code blocks used are removed from `doc` so the LLM
/// never sees them.
pub fn extract_embedded(kind: ImportKind, bytes: &[u8], doc: &mut TextDoc) -> Embedded {
    let mut found = Found::default();
    if kind == ImportKind::Html {
        for data in html_data(bytes) {
            if let Ok(value) = serde_json::from_str::<Value>(&data) {
                found.walk(&value, 0);
            }
        }
    }
    doc.blocks.retain(|block| {
        if block.kind != BlockKind::Code {
            return true;
        }
        let Ok(value) = crate::parse_document(block.text.as_bytes()) else {
            return true;
        };
        let before = found.documents.len();
        found.walk(&value, 0);
        // A block that held a document is read from the document.
        found.documents.len() == before
    });

    let mut embedded = Embedded::default();
    if found.swagger2 {
        embedded.warnings.push(DraftWarning::new(
            "embedded_swagger2",
            "a Swagger 2.0 block was found; its operations are read from the text instead",
        ));
    }
    let mut seen_documents: HashSet<BTreeSet<String>> = HashSet::new();
    let mut seen_operations = HashSet::new();
    for document in &found.documents {
        let paths: BTreeSet<String> = document
            .get("paths")
            .and_then(Value::as_object)
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default();
        if !seen_documents.insert(paths) {
            continue;
        }
        add_document(&mut embedded, document, &mut seen_operations);
    }
    embedded
}

/// Adds `document`'s servers, components and operations to `embedded`,
/// skipping operations (by `path_key`) it already has.
fn add_document(embedded: &mut Embedded, document: &Value, seen: &mut HashSet<(String, String)>) {
    for url in document
        .get("servers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("url").and_then(Value::as_str))
    {
        if !embedded.servers.iter().any(|s| s == url) {
            embedded.servers.push(url.to_string());
        }
    }
    if let Some(components) = document.get("components").and_then(Value::as_object) {
        for (section, entries) in components {
            let Some(entries) = entries.as_object() else {
                continue;
            };
            let target = embedded
                .components
                .entry(section.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(target) = target {
                for (name, value) in entries {
                    target.entry(name.clone()).or_insert_with(|| value.clone());
                }
            }
        }
    }
    let Some(paths) = document.get("paths").and_then(Value::as_object) else {
        return;
    };
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        let shared: Vec<Value> = item
            .get("parameters")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for method in METHODS {
            let Some(Value::Object(op)) = item.get(method) else {
                continue;
            };
            let upper = method.to_ascii_uppercase();
            if !seen.insert(path_key(&upper, path)) {
                continue;
            }
            let mut operation = op.clone();
            merge_parameters(&mut operation, &shared);
            let operation = Value::Object(operation);
            let excerpt = excerpt(&upper, path, &operation);
            embedded.operations.push(EmbeddedOperation {
                method: upper,
                path: path.clone(),
                operation,
                excerpt,
            });
        }
    }
}

/// The identity of a parameter: `(in, name)`, or its `$ref`.
fn parameter_key(p: &Value) -> (String, String) {
    match p.get("$ref").and_then(Value::as_str) {
        Some(r) => ("$ref".into(), r.to_string()),
        None => (
            p.get("in")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            p.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
    }
}

/// Adds the path-level `shared` parameters the operation does not override.
fn merge_parameters(op: &mut Map<String, Value>, shared: &[Value]) {
    if shared.is_empty() {
        return;
    }
    let own: Vec<Value> = op
        .get("parameters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let keys: HashSet<(String, String)> = own.iter().map(parameter_key).collect();
    let mut merged: Vec<Value> = shared
        .iter()
        .filter(|p| !keys.contains(&parameter_key(p)))
        .cloned()
        .collect();
    merged.extend(own);
    op.insert("parameters".into(), Value::Array(merged));
}

/// `GET /path: <compact operation>`, cut to [`MAX_EXCERPT`] chars.
fn excerpt(method: &str, path: &str, operation: &Value) -> String {
    format!("{method} {path}: {operation}")
        .chars()
        .take(MAX_EXCERPT)
        .collect()
}

/// The path prefixes of the servers (`/v1` of `https://x/v1`), `/` excluded.
fn server_prefixes(servers: &[String]) -> Vec<String> {
    servers
        .iter()
        .map(|s| normalize_path(s))
        .filter(|p| p != "/")
        .collect()
}

/// Whether every endpoint line left in `doc` names an embedded operation
/// (directly or under a server's base path).
pub fn covers_endpoints(embedded: &Embedded, doc: &TextDoc) -> bool {
    let prefixes = server_prefixes(&embedded.servers);
    let mut keys: HashSet<(String, String)> = HashSet::new();
    for op in &embedded.operations {
        keys.insert(path_key(&op.method, &op.path));
        for prefix in &prefixes {
            keys.insert(path_key(&op.method, &format!("{prefix}{}", op.path)));
        }
    }
    doc.blocks
        .iter()
        .flat_map(|b| b.text.lines())
        .map(str::trim)
        .filter(|line| is_endpoint_line(line))
        .all(|line| {
            let mut tokens = line.split_whitespace();
            let (Some(method), Some(target)) = (tokens.next(), tokens.next()) else {
                return true;
            };
            keys.contains(&path_key(method, &normalize_path(target)))
        })
}

/// Inserts the operations into `document` (`paths`, `components`), ids via
/// the same conversion as the assembled operations; returns each
/// operation's final id, in order.
pub fn insert_embedded(
    document: &mut Value,
    embedded: &Embedded,
    used: &mut HashSet<String>,
) -> Vec<String> {
    let Value::Object(doc) = document else {
        return Vec::new();
    };
    let mut ids = Vec::with_capacity(embedded.operations.len());
    let paths = doc
        .entry("paths")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(paths) = paths {
        for op in &embedded.operations {
            let own = op.operation.get("operationId").and_then(Value::as_str);
            let (id, original) = assign_id_parts(&op.method, &op.path, own, used);
            let mut operation = op.operation.clone();
            if let Value::Object(o) = &mut operation {
                o.insert("operationId".into(), json!(id));
                if let Some(original) = original
                    && !o.contains_key("x-original-operation-id")
                {
                    o.insert("x-original-operation-id".into(), json!(original));
                }
            }
            let item = paths
                .entry(op.path.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(item) = item {
                item.insert(op.method.to_ascii_lowercase(), operation);
            }
            ids.push(id);
        }
    }
    if !embedded.components.is_empty() {
        let components = doc
            .entry("components")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(components) = components {
            for (section, entries) in &embedded.components {
                let target = components
                    .entry(section.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
                if let (Value::Object(target), Value::Object(entries)) = (target, entries) {
                    for (name, value) in entries {
                        target.entry(name.clone()).or_insert_with(|| value.clone());
                    }
                }
            }
        }
    }
    ids
}
