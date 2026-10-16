//! Protobuf (`.proto` sources and binary `FileDescriptorSet`) -> MCM compiler.
//!
//! Both sources are compiled with protox, so the bundle always carries a
//! descriptor set that includes every import.

mod schema;

use crate::heuristics::{kind_from_name, resource_from_name};
use crate::openapi3::{invalid, slug};
use crate::{ImportError, SourceFile};
use mb_mcm::{
    Binding, Bundle, Identity, InputSchemas, Operation, OperationKind, OutputVariant, Protocol,
    ProtocolPayload, StatusSelector,
};
use prost::Message as _;
use prost_reflect::{DescriptorPool, FileDescriptor, MethodDescriptor, ServiceDescriptor};
use protox::Compiler;
use protox::file::{
    ChainFileResolver, DescriptorSetFileResolver, File, FileResolver, GoogleFileResolver,
};
use schema::{Defs, Named, def_ref, well_known};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// The file name given to a single uploaded `.proto`.
const MAIN_FILE: &str = "main.proto";
const DEFAULT_TITLE: &str = "Protobuf API";
const WELL_KNOWN_DIR: &str = "google/protobuf/";

/// The leading comment of the descriptor at `path` in `file`, with the
/// comment markers' single leading spaces removed.
pub(crate) fn comment(file: &FileDescriptor, path: &[i32]) -> Option<String> {
    let info = file.file_descriptor_proto().source_code_info.as_ref()?;
    let text = info
        .location
        .iter()
        .find(|l| l.path == path)?
        .leading_comments
        .as_deref()?;
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.strip_prefix(' ').unwrap_or(l).trim_end())
        .collect();
    let joined = lines.join("\n");
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

/// Uploaded `.proto` sources by file name.
struct Uploads(BTreeMap<String, String>);

impl FileResolver for Uploads {
    fn open_file(&self, name: &str) -> Result<File, protox::Error> {
        match self.0.get(name) {
            Some(source) => File::from_source(name, source),
            None => Err(protox::Error::file_not_found(name)),
        }
    }
}

/// Records the last file name no resolver could find, so an unresolved
/// import can be named in the error.
struct Recording {
    inner: ChainFileResolver,
    missing: Rc<RefCell<Option<String>>>,
}

impl FileResolver for Recording {
    fn open_file(&self, name: &str) -> Result<File, protox::Error> {
        let result = self.inner.open_file(name);
        if let Err(e) = &result
            && e.is_file_not_found()
        {
            *self.missing.borrow_mut() = Some(name.to_owned());
        }
        result
    }
}

fn compile_error(e: &protox::Error, missing: Option<String>) -> ImportError {
    let file = e.file().unwrap_or(MAIN_FILE);
    if e.is_file_not_found()
        && let Some(name) = missing
    {
        return invalid(
            file,
            format!("import \"{name}\" not found; upload a .zip or .tar.gz containing it"),
        );
    }
    // The Debug form is `<file>:<line>:<col>: <message>` when a span is known.
    let text = format!("{e:?}");
    ImportError::Parse(if text.starts_with(file) {
        text
    } else {
        format!("{file}: {text}")
    })
}

/// The compiled descriptors and which files are sources (not imports).
struct Compiled {
    pool: DescriptorPool,
    sources: BTreeSet<String>,
    file_descriptor_set: Vec<u8>,
}

/// Compile `names` (sorted) through `resolver` with the well-known types
/// available, keeping imports and source info.
fn run(resolver: impl FileResolver + 'static, names: &[String]) -> Result<Compiled, ImportError> {
    let mut chain = ChainFileResolver::new();
    chain.add(resolver);
    chain.add(GoogleFileResolver::new());
    let missing = Rc::new(RefCell::new(None));
    let mut compiler = Compiler::with_file_resolver(Recording {
        inner: chain,
        missing: Rc::clone(&missing),
    });
    compiler.include_imports(true).include_source_info(true);
    compiler
        .open_files(names)
        .map_err(|e| compile_error(&e, missing.borrow_mut().take()))?;
    let sources = compiler
        .files()
        .filter(|f| !f.is_import() && !f.name().starts_with(WELL_KNOWN_DIR))
        .map(|f| f.name().to_owned())
        .collect();
    Ok(Compiled {
        pool: compiler.descriptor_pool(),
        sources,
        file_descriptor_set: compiler.encode_file_descriptor_set(),
    })
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Compile uploaded `.proto` files (`/`-separated relative paths).
pub(crate) fn compile_sources(
    files: &[&SourceFile],
    source_digest: String,
) -> Result<Bundle, ImportError> {
    let mut uploads = BTreeMap::new();
    for f in files {
        let text = std::str::from_utf8(&f.bytes)
            .map_err(|e| ImportError::Parse(format!("{}: not valid UTF-8: {e}", f.path)))?;
        uploads.insert(f.path.clone(), text.to_owned());
    }
    let names: Vec<String> = uploads.keys().cloned().collect();
    let compiled = run(Uploads(uploads), &names)?;
    Ok(bundle(&compiled, source_digest))
}

/// Compile a single `.proto` source, named `main.proto`.
pub(crate) fn compile_text(bytes: &[u8]) -> Result<Bundle, ImportError> {
    let file = SourceFile {
        path: MAIN_FILE.into(),
        bytes: bytes.to_vec(),
    };
    compile_sources(&[&file], hex_digest(bytes))
}

/// Decode `bytes` as a `FileDescriptorSet` with at least one file and one
/// service.
fn decode_set(bytes: &[u8]) -> Option<prost_types::FileDescriptorSet> {
    let set = prost_types::FileDescriptorSet::decode(bytes).ok()?;
    let has_service = set.file.iter().any(|f| !f.service.is_empty());
    (!set.file.is_empty() && has_service).then_some(set)
}

/// Whether `bytes` are a binary `FileDescriptorSet` declaring a service.
pub(crate) fn is_descriptor_set(bytes: &[u8]) -> bool {
    decode_set(bytes).is_some()
}

/// Compile a binary `FileDescriptorSet`; every file in it is a source.
pub(crate) fn compile_descriptor_set(bytes: &[u8]) -> Result<Bundle, ImportError> {
    let set = decode_set(bytes)
        .ok_or_else(|| ImportError::Parse("not a FileDescriptorSet declaring a service".into()))?;
    let mut names: Vec<String> = set.file.iter().map(|f| f.name().to_owned()).collect();
    names.sort_unstable();
    names.dedup();
    let compiled = run(DescriptorSetFileResolver::new(set), &names)?;
    Ok(bundle(&compiled, hex_digest(bytes)))
}

// ---------------------------------------------------------------------------
// Bundle
// ---------------------------------------------------------------------------

fn is_source(compiled: &Compiled, file: &FileDescriptor) -> bool {
    compiled.sources.contains(file.name())
}

/// The services of the source files, sorted by full name.
fn services(compiled: &Compiled) -> Vec<ServiceDescriptor> {
    let mut services: Vec<ServiceDescriptor> = compiled
        .pool
        .services()
        .filter(|s| is_source(compiled, &s.parent_file()))
        .collect();
    services.sort_by(|a, b| a.full_name().cmp(b.full_name()));
    services
}

fn kind(method: &MethodDescriptor) -> OperationKind {
    match (method.is_client_streaming(), method.is_server_streaming()) {
        (true, true) => OperationKind::StreamBidi,
        (true, false) => OperationKind::StreamClient,
        (false, true) => OperationKind::StreamServer,
        (false, false) => kind_from_name(method.name(), OperationKind::Query),
    }
}

fn identity(method: &MethodDescriptor, resource_type: Option<&str>) -> Option<Identity> {
    method.output().fields().find(|f| f.json_name() == "id")?;
    let input = method.input();
    let resource_id = resource_type.map(|r| format!("{r}Id"));
    let input_field = input
        .fields()
        .find(|f| f.json_name() == "id")
        .or_else(|| {
            input.fields().find(|f| {
                resource_id
                    .as_deref()
                    .is_some_and(|r| f.json_name().eq_ignore_ascii_case(r))
            })
        })
        .map(|f| f.json_name().to_owned());
    Some(Identity {
        path_param: None,
        field: "id".into(),
        input_field,
    })
}

fn operation(id: String, service: &ServiceDescriptor, method: &MethodDescriptor) -> Operation {
    let resource_type = resource_from_name(method.name());
    let output = method.output();
    Operation {
        id,
        protocol: Protocol::Grpc,
        binding: Binding::Grpc {
            package: service.package_name().to_owned(),
            service: service.name().to_owned(),
            method: method.name().to_owned(),
        },
        kind: kind(method),
        summary: comment(&method.parent_file(), method.path()),
        input: InputSchemas {
            path: None,
            query: None,
            headers: None,
            body: Some(def_ref(method.input().full_name())),
            body_required: true,
        },
        outputs: vec![OutputVariant {
            status: StatusSelector::Code(200),
            content_type: Some("application/grpc".into()),
            schema: Some(def_ref(output.full_name())),
        }],
        identity: identity(method, resource_type.as_deref()),
        resource_type,
        semantic_hints: comment(&output.parent_file(), output.path())
            .into_iter()
            .collect(),
        security: Vec::new(),
    }
}

fn operations(services: &[ServiceDescriptor]) -> Vec<Operation> {
    let mut short_names: BTreeMap<&str, usize> = BTreeMap::new();
    for s in services {
        *short_names.entry(s.name()).or_default() += 1;
    }
    let mut ops = Vec::new();
    for s in services {
        let prefix = if short_names.get(s.name()).copied().unwrap_or(0) > 1 {
            s.full_name()
        } else {
            s.name()
        };
        for m in s.methods() {
            ops.push(operation(format!("{prefix}.{}", m.name()), s, &m));
        }
    }
    ops
}

fn defs(compiled: &Compiled, services: &[ServiceDescriptor]) -> serde_json::Map<String, Value> {
    let mut defs = Defs::default();
    let pool = &compiled.pool;
    for m in pool.all_messages() {
        if is_source(compiled, &m.parent_file()) && well_known(m.full_name()).is_none() {
            defs.add(Named::Message(m));
        }
    }
    for e in pool.all_enums() {
        if is_source(compiled, &e.parent_file()) && well_known(e.full_name()).is_none() {
            defs.add(Named::Enum(e));
        }
    }
    for m in services.iter().flat_map(|s| s.methods()) {
        defs.add(Named::Message(m.input()));
        defs.add(Named::Message(m.output()));
    }
    defs.finish()
}

fn title(compiled: &Compiled, services: &[ServiceDescriptor]) -> String {
    if let Some(s) = services.first() {
        let package = s.package_name();
        return if package.is_empty() {
            s.name().to_owned()
        } else {
            package.to_owned()
        };
    }
    compiled
        .pool
        .files()
        .filter(|f| is_source(compiled, f))
        .map(|f| f.package_name().to_owned())
        .find(|p| !p.is_empty())
        .unwrap_or_else(|| DEFAULT_TITLE.to_owned())
}

fn bundle(compiled: &Compiled, source_digest: String) -> Bundle {
    let services = services(compiled);
    let title = title(compiled, &services);
    Bundle {
        id: slug(&title),
        title,
        version: "0".into(),
        source_digest,
        operations: operations(&services),
        defs: defs(compiled, &services),
        protocol_payload: Some(ProtocolPayload::Grpc {
            file_descriptor_set: compiled.file_descriptor_set.clone(),
        }),
        channels: Vec::new(),
    }
}

/// The digest of several uploaded files: sha256 over each file's path,
/// length and bytes, in path order.
pub(crate) fn files_digest(files: &[&SourceFile]) -> String {
    let mut sorted: Vec<&&SourceFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut buf = Vec::new();
    for f in sorted {
        buf.extend_from_slice(f.path.as_bytes());
        buf.push(0);
        buf.extend_from_slice(&(f.bytes.len() as u64).to_le_bytes());
        buf.extend_from_slice(&f.bytes);
    }
    hex_digest(&buf)
}
