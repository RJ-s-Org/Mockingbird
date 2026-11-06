//! The tier-0 world description: what the heuristics already put in a bundle,
//! restated as a [`World`](mb_mcm::world::World), validating cleanly against
//! the bundle it came from.
#![cfg(test)]

use std::path::{Path, PathBuf};

use mb_import::world::tier0;
use mb_import::{SourceFile, compile_files};
use mb_mcm::world::{OpKind, Pointer, World, overlay, validate};
use mb_mcm::{Bundle, RecordAt};

/// Every contract file under `examples/apis` and `scripts/llm-bench/contracts`:
/// the files that are not mock configs, examples, READMEs or caches.
fn repo_contracts() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let contract = matches!(ext, "yaml" | "yml" | "json" | "graphql" | "proto" | "wsdl")
                && name != "mockingbird.yaml"
                && name != "example.json";
            if contract {
                out.push(path);
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    walk(&root.join("examples/apis"), &mut out);
    walk(&root.join("scripts/llm-bench/contracts"), &mut out);
    out.sort();
    out
}

fn compile_path(path: &Path) -> Bundle {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let bytes = std::fs::read(path).unwrap();
    compile_files(&[SourceFile { path: name, bytes }])
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn bundles() -> Vec<(PathBuf, Bundle)> {
    let paths = repo_contracts();
    assert!(paths.len() > 40, "found only {} contracts", paths.len());
    paths
        .into_iter()
        .map(|p| {
            let b = compile_path(&p);
            (p, b)
        })
        .collect()
}

#[test]
fn tier0_validates_cleanly_on_every_repo_contract() {
    let mut failures = Vec::new();
    for (path, b) in bundles() {
        let world = tier0(&b);
        let v = validate(&world, &b);
        for r in &v.refused {
            failures.push(format!("{}: {r}", path.display()));
        }
        if v.refused.is_empty() {
            assert_eq!(v.applied, world, "{}", path.display());
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn tier0_matches_bundle_identity() {
    let (mut described, mut agreeing) = (0, 0);
    for (path, b) in bundles() {
        let world = tier0(&b);
        for op in &b.operations {
            let Some(identity) = &op.identity else {
                continue;
            };
            let desc = &world.operations[&op.id];
            let at = format!("{}: {}", path.display(), op.id);
            assert_eq!(
                desc.target.as_ref().and_then(|t| t.path_param.as_deref()),
                identity.path_param.as_deref(),
                "{at}"
            );
            let t = op.resource_type.as_deref().unwrap();
            assert_eq!(desc.r#type.as_deref(), Some(t), "{at}");
            // Operations of one type can name different id fields (an index's
            // `uid` read by one, `indexUid` by another); the type's identity is
            // one of them, its read's when that one is a member of the record.
            let fields: Vec<&str> = b
                .operations
                .iter()
                .filter(|o| o.resource_type.as_deref() == Some(t))
                .filter_map(|o| o.identity.as_ref().map(|i| i.field.as_str()))
                .collect();
            if let Some(member) = world.types.get(t).and_then(|d| d.identity.as_ref()) {
                let member = member.member.as_deref().unwrap_or_default();
                assert!(fields.contains(&member), "{at}: {member}");
                let agrees = b.operations.iter().all(|o| {
                    o.resource_type.as_deref() != Some(t)
                        || o.identity.as_ref().is_none_or(|i| i.field == member)
                });
                if agrees {
                    agreeing += 1;
                }
                described += 1;
            }
            let record = desc.answer.as_ref().and_then(|a| a.record.clone());
            let expected = identity.record.as_ref().map(|r| {
                Pointer::parse(&match r {
                    RecordAt::Member(m) => format!("/{m}"),
                    RecordAt::Item => "/0".to_owned(),
                    RecordAt::MemberItem(m) => format!("/{m}/0"),
                    RecordAt::Pointer(_) => panic!("{at}: the heuristics wrote a deep pointer"),
                })
                .unwrap()
            });
            assert_eq!(record, expected, "{at}");
        }
    }
    // Nearly every type the heuristics identify gets its id field, and
    // nearly always the one every operation of it names.
    assert!(described > 300, "{described}");
    assert!(
        agreeing * 100 >= described * 95,
        "{agreeing} of {described}"
    );
}

/// Tier 0 restates the bundle, so writing it into the bundle changes
/// nothing: no answer, status or stored record moves under a tier-0 world.
#[test]
fn apply_of_tier0_is_identity() {
    for (path, b) in bundles() {
        let base = tier0(&b);
        assert!(
            mb_mcm::world::apply(&b, &base, &base) == b,
            "{}",
            path.display()
        );
    }
}

/// A description with no entries, laid over tier 0, writes nothing.
#[test]
fn apply_writes_nothing_for_a_world_with_no_entries() {
    for (path, b) in bundles() {
        let base = tier0(&b);
        let world = overlay(&base, &World::default());
        assert!(
            mb_mcm::world::apply(&b, &base, &world) == b,
            "{}",
            path.display()
        );
        assert!(
            mb_mcm::world::apply(&b, &base, &World::default()) == b,
            "{}",
            path.display()
        );
    }
}

#[test]
fn tier0_maps_operation_kinds() {
    let b = mb_import::compile(
        br##"
openapi: 3.0.0
info: {title: Kinds, version: "1"}
paths:
  /things:
    get:
      operationId: listThings
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Thing"}}}}}}
    post:
      operationId: createThing
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
  /things/{thingId}:
    parameters: [{name: thingId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getThing
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
    delete:
      operationId: deleteThing
      responses: {"204": {description: gone}}
components:
  schemas:
    Thing: {type: object, properties: {id: {type: string}, name: {type: string}}}
"##,
    )
    .unwrap();
    let world = tier0(&b);
    let kind = |id: &str| world.operations[id].kind;
    assert_eq!(kind("listThings"), Some(OpKind::List));
    assert_eq!(kind("createThing"), Some(OpKind::Create));
    assert_eq!(kind("getThing"), Some(OpKind::Read));
    assert_eq!(kind("deleteThing"), Some(OpKind::Delete));
    assert!(validate(&world, &b).refused.is_empty());
}

#[test]
fn tier0_keeps_a_literal_dotted_identity_member() {
    let mut b = mb_import::compile(
        br##"
openapi: 3.0.0
info: {title: OData, version: "1"}
paths:
  /docs/{docId}:
    parameters: [{name: docId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getDoc
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}}
components:
  schemas:
    Doc: {type: object, properties: {"odata.id": {type: string}, name: {type: string}}}
"##,
    )
    .unwrap();
    // Whatever the heuristics chose, the record's id is the member named
    // `odata.id` itself, which `member_at` reads before any dotted walk.
    let op = b.operations.iter_mut().find(|o| o.id == "getDoc").unwrap();
    let t = op.resource_type.get_or_insert_with(|| "doc".into()).clone();
    op.identity = Some(mb_mcm::Identity {
        path_param: Some("docId".into()),
        field: "odata.id".into(),
        input_field: None,
        path_field: None,
        record: None,
    });
    let world = tier0(&b);
    let identity = world.types.get(&t).and_then(|d| d.identity.as_ref());
    assert_eq!(identity.and_then(|i| i.member.as_deref()), Some("odata.id"));
    assert!(validate(&world, &b).refused.is_empty());
}
