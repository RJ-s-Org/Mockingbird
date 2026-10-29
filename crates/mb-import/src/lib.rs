//! Compiles API descriptions (OpenAPI 3.x, Swagger 2.0, GraphQL SDL and
//! introspection, protobuf sources and descriptor sets, WSDL 1.1 with XSD,
//! AsyncAPI 2.x and 3.0) into the Mockingbird Contract Model.

mod archive;
mod asyncapi;
mod graphql;
mod heuristics;
mod openapi3;
mod proto;
mod schema;
mod security;
#[cfg(feature = "smart")]
pub mod smart;
mod swagger2;
mod triggers;
mod wsdl;

pub use archive::{
    MAX_ARCHIVE_BYTES, MAX_ARCHIVE_FILES, MAX_ARCHIVE_UNPACKED_BYTES, pack_dir, read_source,
    unpack_archive,
};
use std::collections::{HashMap, HashSet};

use mb_mcm::Bundle;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use triggers::validate_triggers;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    OpenApi3,
    Swagger2,
    GraphqlSdl,
    GraphqlIntrospection,
    Proto,
    FileDescriptorSet,
    Wsdl11,
    Archive,
    /// AsyncAPI 2.0 through 2.6.
    AsyncApi2,
    /// AsyncAPI 3.0.
    AsyncApi3,
}

/// One uploaded file of a multi-file contract.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceFile {
    /// `/`-separated path relative to the upload root.
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(thiserror::Error, Debug)]
pub enum ImportError {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("unsupported format: {0}")]
    Unsupported(String),
    #[error("{pointer}: {message}")]
    Invalid { pointer: String, message: String },
}

/// A version field as text. Unquoted YAML versions (`swagger: 2.0`) parse as numbers.
fn version_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Whether `v` is an AsyncAPI 2.0.0 through 2.6.x version.
fn is_asyncapi2(v: &str) -> bool {
    let mut parts = v.split('.');
    parts.next() == Some("2")
        && parts
            .next()
            .and_then(|minor| minor.parse::<u32>().ok())
            .is_some_and(|minor| minor <= 6)
        && parts.next().is_some()
}

/// Detect the contract format from `"asyncapi": "2.x|3.0.x"`,
/// `"openapi": "3.x"` or `"swagger": "2.0"`.
pub fn detect_format(doc: &Value) -> Result<SourceFormat, ImportError> {
    if let Some(v) = version_string(doc.get("asyncapi")) {
        if is_asyncapi2(&v) {
            return Ok(SourceFormat::AsyncApi2);
        }
        if v.starts_with("3.0.") {
            return Ok(SourceFormat::AsyncApi3);
        }
        return Err(ImportError::Unsupported(format!("asyncapi {v}")));
    }
    if let Some(v) = version_string(doc.get("openapi")) {
        if v.starts_with("3.") {
            return Ok(SourceFormat::OpenApi3);
        }
        return Err(ImportError::Unsupported(format!("openapi {v}")));
    }
    if let Some(v) = version_string(doc.get("swagger")) {
        if v == "2.0" {
            return Ok(SourceFormat::Swagger2);
        }
        return Err(ImportError::Unsupported(format!("swagger {v}")));
    }
    Err(ImportError::Unsupported(
        "document has no `asyncapi`, `openapi` or `swagger` version field".into(),
    ))
}

/// Parse JSON, falling back to YAML.
pub fn parse_document(bytes: &[u8]) -> Result<Value, ImportError> {
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        return Ok(v);
    }
    serde_saphyr::from_slice::<Value>(bytes).map_err(|e| ImportError::Parse(e.to_string()))
}

/// Markers of GraphQL SDL text.
const SDL_MARKERS: [&str; 4] = [
    "type Query",
    "schema {",
    "type Mutation",
    "type Subscription",
];

/// The `syntax` statement of a `.proto` source.
const PROTO_SYNTAX: &str = "syntax = \"proto";

/// Whether a JSON document is a GraphQL introspection result
/// (`/__schema` or `/data/__schema`).
fn is_introspection(doc: &Value) -> bool {
    doc.get("__schema").is_some() || doc.pointer("/data/__schema").is_some()
}

/// Detect the source format of uploaded bytes.
pub fn sniff(bytes: &[u8]) -> Result<SourceFormat, ImportError> {
    sniff_parsed(bytes).map(|(format, _)| format)
}

/// [`sniff`], with the JSON or YAML document it parsed on the way (the
/// value [`parse_document`] returns), so [`compile`] parses only once.
fn sniff_parsed(bytes: &[u8]) -> Result<(SourceFormat, Option<Value>), ImportError> {
    if archive::is_archive(bytes) {
        return Ok((SourceFormat::Archive, None));
    }
    if let Ok(doc) = serde_json::from_slice::<Value>(bytes) {
        if is_introspection(&doc) {
            return Ok((SourceFormat::GraphqlIntrospection, Some(doc)));
        }
        return detect_format(&doc).map(|format| (format, Some(doc)));
    }
    if wsdl::looks_like_xml(bytes) {
        return wsdl::sniff(bytes).map(|format| (format, None));
    }
    let yaml = serde_saphyr::from_slice::<Value>(bytes);
    if let Ok(doc) = &yaml
        && let Ok(format) = detect_format(doc)
    {
        return Ok((format, yaml.ok()));
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        if text.contains(PROTO_SYNTAX) {
            return Ok((SourceFormat::Proto, None));
        }
        if SDL_MARKERS.iter().any(|m| text.contains(m)) {
            return Ok((SourceFormat::GraphqlSdl, None));
        }
        // A `.proto` without a syntax statement.
        if text.contains("service ") && text.contains("message ") {
            return Ok((SourceFormat::Proto, None));
        }
    }
    if proto::is_descriptor_set(bytes) {
        return Ok((SourceFormat::FileDescriptorSet, None));
    }
    match yaml {
        Ok(doc) => detect_format(&doc).map(|format| (format, Some(doc))),
        Err(e) => Err(ImportError::Parse(e.to_string())),
    }
}

/// What the deterministic compilers collect besides the bundle: the
/// warnings about what they skipped or relaxed.
pub(crate) struct Ctx {
    warnings: Vec<Warning>,
    /// Whether a `$ref` naming a missing component schema is stubbed as an
    /// open schema with a `dangling_ref` warning (contract uploads) rather
    /// than rejected (smart-import drafts, whose issues go back for repair).
    pub(crate) stub_dangling_refs: bool,
}

impl Ctx {
    /// The context of a contract upload.
    pub(crate) fn new() -> Self {
        Self {
            warnings: Vec::new(),
            stub_dangling_refs: true,
        }
    }

    /// A context that rejects dangling refs.
    #[cfg(feature = "smart")]
    pub(crate) fn strict() -> Self {
        Self {
            stub_dangling_refs: false,
            ..Self::new()
        }
    }

    /// Records a warning.
    pub(crate) fn warn(&mut self, code: &str, operation: Option<&str>, message: String) {
        self.warnings.push(Warning {
            code: code.to_owned(),
            operation: operation.map(str::to_owned),
            message,
            size: None,
        });
    }
}

/// Detect the format and dispatch to the matching compiler.
pub fn compile(bytes: &[u8]) -> Result<Bundle, ImportError> {
    compile_in(bytes, &mut Ctx::new())
}

/// [`compile`], collecting the compilers' warnings in `ctx`.
fn compile_in(bytes: &[u8], ctx: &mut Ctx) -> Result<Bundle, ImportError> {
    let (format, doc) = sniff_parsed(bytes)?;
    // The document sniff parsed, or a fresh parse (not reached in practice:
    // every format below comes with its document).
    let doc = move || doc.map_or_else(|| parse_document(bytes), Ok);
    match format {
        SourceFormat::OpenApi3 => openapi3::compile(&doc()?, bytes, ctx),
        SourceFormat::Swagger2 => swagger2::compile(&doc()?, bytes, ctx),
        SourceFormat::GraphqlSdl => graphql::compile_sdl(bytes),
        SourceFormat::GraphqlIntrospection => graphql::compile_introspection(&doc()?, bytes),
        SourceFormat::Proto => proto::compile_text(bytes),
        SourceFormat::FileDescriptorSet => proto::compile_descriptor_set(bytes),
        SourceFormat::Wsdl11 => wsdl::compile_text(bytes, ctx),
        SourceFormat::AsyncApi2 => asyncapi::compile_v2(&doc()?, bytes),
        SourceFormat::AsyncApi3 => asyncapi::compile_v3(&doc()?, bytes),
        SourceFormat::Archive => {
            let mut bundle = compile_files_in(&unpack_archive(bytes)?, ctx)?;
            bundle.source_digest = proto::hex_digest(bytes);
            Ok(bundle)
        }
    }
}

/// Bytes of compact JSON the generation prompt shows of a response schema
/// (about 16k tokens); larger schemas are trimmed in the prompt.
pub const PROMPT_SCHEMA_BUDGET: usize = 64 * 1024;

/// A non-fatal finding of compilation.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Warning {
    /// `schema_too_large`, `security_scheme_unsupported`,
    /// `status_range_expanded`, `dangling_ref`, `cookie_param_ignored`,
    /// `response_header_unresolved`, `xsd_wildcard_ignored` or
    /// `wsdl_import_ignored` (see `docs/protocols.md`).
    pub code: String,
    /// The operation id the warning is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    pub message: String,
    /// Compact JSON bytes of the schema with its reachable `$defs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// [`compile`], with the warnings of the compiler (what it skipped or
/// relaxed) followed by the bundle's [`bundle_warnings`].
pub fn compile_with_warnings(bytes: &[u8]) -> Result<(Bundle, Vec<Warning>), ImportError> {
    let mut ctx = Ctx::new();
    let bundle = compile_in(bytes, &mut ctx)?;
    Ok(with_bundle_warnings(bundle, ctx))
}

/// [`compile_files`], with the warnings [`compile_with_warnings`] gives.
pub fn compile_files_with_warnings(
    files: &[SourceFile],
) -> Result<(Bundle, Vec<Warning>), ImportError> {
    let mut ctx = Ctx::new();
    let bundle = compile_files_in(files, &mut ctx)?;
    Ok(with_bundle_warnings(bundle, ctx))
}

fn with_bundle_warnings(bundle: Bundle, ctx: Ctx) -> (Bundle, Vec<Warning>) {
    let mut warnings = ctx.warnings;
    warnings.extend(bundle_warnings(&bundle));
    (bundle, warnings)
}

/// One `security_scheme_unsupported` warning per operation with security
/// requirements the mock cannot check (they are never satisfied in
/// `passthrough_validate` mode), and one `schema_too_large` warning per
/// operation whose largest response schema, with its reachable
/// definitions, exceeds [`PROMPT_SCHEMA_BUDGET`].
pub fn bundle_warnings(bundle: &Bundle) -> Vec<Warning> {
    let mut sizes = ReachableSizes::new(bundle);
    let mut warnings = Vec::new();
    for op in &bundle.operations {
        if let Some(names) = security::unsupported_schemes(&op.security) {
            let id = &op.id;
            warnings.push(Warning {
                code: "security_scheme_unsupported".into(),
                operation: Some(id.clone()),
                message: format!(
                    "operation {id}: security requirements using {names} cannot be checked by the mock (supported: http bearer and basic, apiKey in header, query or cookie, oauth2, openIdConnect); in passthrough_validate mode they are never satisfied, other alternatives still apply"
                ),
                size: None,
            });
        }
        let largest = op
            .outputs
            .iter()
            .filter_map(|o| {
                let schema = o.schema.as_ref()?;
                Some((o, sizes.of(schema)))
            })
            .max_by_key(|(_, size)| *size);
        let Some((output, size)) = largest else {
            continue;
        };
        if size <= PROMPT_SCHEMA_BUDGET {
            continue;
        }
        let status = match output.status {
            mb_mcm::StatusSelector::Code(c) => c.to_string(),
            mb_mcm::StatusSelector::Default => "default".to_string(),
        };
        let id = &op.id;
        warnings.push(Warning {
            code: "schema_too_large".into(),
            operation: Some(id.clone()),
            message: format!(
                "operation {id}: the {status} response schema is {size} bytes, over the prompt budget of {PROMPT_SCHEMA_BUDGET}; the prompt shows a trimmed schema (validation uses the full one)"
            ),
            size: Some(size as u64),
        });
    }
    warnings
}

/// An `io::Write` that only counts the bytes written to it.
#[derive(Default)]
struct ByteCount(usize);

impl std::io::Write for ByteCount {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The length of `value` as compact JSON, without building the text.
fn json_len<T: Serialize + ?Sized>(value: &T) -> usize {
    let mut count = ByteCount::default();
    // Serializing a `Value` or a `str` into a counter cannot fail.
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

/// The definition names of the `#/$defs/<name>[/...]` references in
/// `value` (JSON-pointer unescaped), as `Bundle::with_reachable_defs`
/// follows them.
fn def_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(obj) => {
            for (k, v) in obj {
                if k == "$ref"
                    && let Some(r) = v.as_str()
                    && let Some(rest) = r.strip_prefix("#/$defs/")
                {
                    let name = rest.split('/').next().unwrap_or(rest);
                    out.push(name.replace("~1", "/").replace("~0", "~"));
                } else {
                    def_refs(v, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| def_refs(v, out)),
        _ => {}
    }
}

/// `bundle.with_reachable_defs(schema).to_string().len()` for many schemas,
/// computed from per-definition sizes and references measured once,
/// without cloning the reachable definitions or serializing them again.
struct ReachableSizes<'a> {
    bundle: &'a Bundle,
    /// Per definition, measured on first use: its member's length in the
    /// `$defs` object (`"name":` and the value) and the definitions it
    /// references.
    defs: Option<HashMap<&'a str, (usize, Vec<String>)>>,
}

impl<'a> ReachableSizes<'a> {
    fn new(bundle: &'a Bundle) -> Self {
        Self { bundle, defs: None }
    }

    fn of(&mut self, schema: &Value) -> usize {
        let bundle = self.bundle;
        let defs = self.defs.get_or_insert_with(|| {
            bundle
                .defs
                .iter()
                .map(|(name, def)| {
                    let mut refs = Vec::new();
                    def_refs(def, &mut refs);
                    (
                        name.as_str(),
                        (json_len(name.as_str()) + 1 + json_len(def), refs),
                    )
                })
                .collect()
        });
        // The `$defs` object of the reachable definitions.
        let mut roots = Vec::new();
        def_refs(schema, &mut roots);
        let mut pending: Vec<&str> = roots.iter().map(String::as_str).collect();
        let mut reachable: HashSet<&str> = HashSet::new();
        let mut defs_len = 2;
        while let Some(name) = pending.pop() {
            if reachable.contains(name) {
                continue;
            }
            if let Some((len, refs)) = defs.get(name) {
                if !reachable.is_empty() {
                    defs_len += 1;
                }
                defs_len += len;
                pending.extend(refs.iter().map(String::as_str));
                reachable.insert(name);
            }
        }
        match schema {
            // `{`, the members but `$defs` with their commas, `"$defs":`,
            // the definitions, `}`.
            Value::Object(obj) => {
                let members: usize = obj
                    .iter()
                    .filter(|(k, _)| k.as_str() != "$defs")
                    .map(|(k, v)| json_len(k.as_str()) + 1 + json_len(v) + 1)
                    .sum();
                1 + members + 8 + defs_len + 1
            }
            // `{"allOf":[<schema>],"$defs":<definitions>}`
            other => 21 + json_len(other) + defs_len,
        }
    }
}

/// Whether `path` names a GraphQL SDL file.
fn is_graphql_path(path: &str) -> bool {
    [".graphql", ".graphqls", ".gql"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// File extensions of documentation and images, never taken for a
/// contract in a multi-file upload.
const INCIDENTAL_EXTENSIONS: [&str; 14] = [
    ".md",
    ".markdown",
    ".txt",
    ".rst",
    ".adoc",
    ".html",
    ".htm",
    ".pdf",
    ".png",
    ".jpg",
    ".jpeg",
    ".gif",
    ".svg",
    ".css",
];

/// Whether `path` names documentation or an image (case-insensitive), or a
/// `LICENSE`, `README`, `CHANGELOG` or `NOTICE` file.
fn is_incidental(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    INCIDENTAL_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
        || ["license", "readme", "changelog", "notice"]
            .iter()
            .any(|stem| {
                name.strip_prefix(stem)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
            })
}

/// Compile a multi-file contract (an unpacked archive or loose files):
/// 1. any `.proto` files are compiled together (imports resolve between
///    them);
/// 2. else exactly one `.wsdl` is compiled with the other files available
///    to its `schemaLocation`s (several `.wsdl` files are invalid:
///    `wsdl:import` is not followed);
/// 3. else the `.graphql`/`.graphqls`/`.gql` files are compiled as one SDL,
///    joined by `\n` in path order;
/// 4. else the one file that is a contract ([`sniff`] recognises it, and
///    it is not documentation, an image, a `README` or a `LICENSE`) is
///    compiled on its own (it may not be another archive); none or several
///    such files are invalid, and the error names them.
pub fn compile_files(files: &[SourceFile]) -> Result<Bundle, ImportError> {
    compile_files_in(files, &mut Ctx::new())
}

fn compile_files_in(files: &[SourceFile], ctx: &mut Ctx) -> Result<Bundle, ImportError> {
    let protos: Vec<&SourceFile> = files
        .iter()
        .filter(|f| f.path.ends_with(".proto"))
        .collect();
    if !protos.is_empty() {
        return proto::compile_sources(&protos, proto::files_digest(&protos));
    }
    let wsdls: Vec<&SourceFile> = files.iter().filter(|f| f.path.ends_with(".wsdl")).collect();
    match wsdls.as_slice() {
        [] => {}
        [wsdl] => {
            let digest = match files {
                [one] => proto::hex_digest(&one.bytes),
                _ => proto::files_digest(&files.iter().collect::<Vec<_>>()),
            };
            return wsdl::compile_upload(wsdl, files, digest, ctx);
        }
        many => {
            let names: Vec<&str> = many.iter().map(|f| f.path.as_str()).collect();
            return Err(ImportError::Invalid {
                pointer: "/".into(),
                message: format!(
                    "the upload contains {} .wsdl files ({}); expected one: wsdl:import is not supported, so merge the imported definitions into the file that holds wsdl:service",
                    many.len(),
                    names.join(", ")
                ),
            });
        }
    }
    let mut graphql: Vec<&SourceFile> = files.iter().filter(|f| is_graphql_path(&f.path)).collect();
    if !graphql.is_empty() {
        graphql.sort_by(|a, b| a.path.cmp(&b.path));
        let sdl = graphql
            .iter()
            .map(|f| f.bytes.as_slice())
            .collect::<Vec<_>>()
            .join(&b'\n');
        return graphql::compile_sdl(&sdl);
    }
    // A lone file is compiled whatever its name; among several, only the
    // contracts count.
    let contracts: Vec<&SourceFile> = match files {
        [one] => vec![one],
        _ => files
            .iter()
            .filter(|f| !is_incidental(&f.path) && sniff(&f.bytes).is_ok())
            .collect(),
    };
    match contracts.as_slice() {
        [one] if archive::is_archive(&one.bytes) => Err(ImportError::Invalid {
            pointer: one.path.clone(),
            message: "nested archives are not supported".into(),
        }),
        [one] => compile_in(&one.bytes, ctx),
        [] => {
            let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
            Err(ImportError::Unsupported(format!(
                "archive contains no recognised contract (files: {})",
                names.join(", ")
            )))
        }
        many => {
            let names: Vec<&str> = many.iter().map(|f| f.path.as_str()).collect();
            Err(ImportError::Invalid {
                pointer: "/".into(),
                message: format!(
                    "the upload contains {} contracts ({}); expected one",
                    many.len(),
                    names.join(", ")
                ),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReachableSizes, compile, parse_document, sniff, sniff_parsed};
    use mb_mcm::Bundle;
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};

    /// Every file under the repository's `fixtures/`.
    fn fixture_files() -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures"),
            &mut out,
        );
        out.sort();
        out
    }

    fn assert_sizes(bundle: &Bundle, schemas: &[&Value]) {
        let mut sizes = ReachableSizes::new(bundle);
        for schema in schemas {
            assert_eq!(
                sizes.of(schema),
                bundle.with_reachable_defs(schema).to_string().len(),
                "{schema}"
            );
        }
    }

    #[test]
    fn reachable_sizes_match_the_serialized_schema() {
        let mut compiled = 0;
        for path in fixture_files() {
            let Ok(bundle) = compile(&std::fs::read(&path).unwrap()) else {
                continue;
            };
            compiled += 1;
            let schemas: Vec<&Value> = bundle
                .operations
                .iter()
                .flat_map(|op| op.outputs.iter().filter_map(|o| o.schema.as_ref()))
                .chain(bundle.defs.values())
                .collect();
            assert_sizes(&bundle, &schemas);
        }
        assert!(compiled > 10, "only {compiled} fixtures compiled");

        let mut bundle: Bundle = serde_json::from_value(json!({
            "id": "t", "title": "t", "version": "0", "source_digest": "",
            "operations": [], "defs": {}, "channels": [], "webhooks": []
        }))
        .unwrap();
        bundle.defs = serde_json::from_value(json!({
            "A": {"type": "object", "properties": {"b": {"$ref": "#/$defs/B"}}},
            "B": {"items": [{"$ref": "#/$defs/A/properties/b"}, {"$ref": "#/$defs/C"}]},
            "C": true,
            "a/b~c": {"description": "é \"quoted\" \u{1}", "$ref": "#/$defs/A"},
            "self": {"$ref": "#/$defs/self"}
        }))
        .unwrap();
        let schemas = [
            json!({"$ref": "#/$defs/A"}),
            json!({"$ref": "#/$defs/a~1b~0c"}),
            json!({"$ref": "#/$defs/missing"}),
            json!({"$ref": "#/$defs/self", "$defs": {"old": {"type": "string"}}}),
            json!({"$defs": {}}),
            json!({}),
            json!(true),
            json!(false),
            json!([{"$ref": "#/$defs/B"}]),
            json!({"x": [1, 2.5, -3, null, "s"], "y": {"$ref": "#/$defs/C"}}),
            json!({"$ref": 5, "z": {"$ref": "#/components/schemas/A"}}),
        ];
        assert_sizes(&bundle, &schemas.iter().collect::<Vec<_>>());
    }

    #[test]
    fn sniff_parsed_returns_the_parse_document_value() {
        for path in fixture_files() {
            let bytes = std::fs::read(&path).unwrap();
            let Ok((format, doc)) = sniff_parsed(&bytes) else {
                assert!(sniff(&bytes).is_err(), "{}", path.display());
                continue;
            };
            assert_eq!(sniff(&bytes).ok(), Some(format), "{}", path.display());
            if let Some(doc) = doc {
                assert_eq!(doc, parse_document(&bytes).unwrap(), "{}", path.display());
            }
        }
    }
}
