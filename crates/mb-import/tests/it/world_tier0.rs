//! The tier-0 world description: what the heuristics already put in a bundle,
//! restated as a [`World`](mb_mcm::world::World), validating cleanly against
//! the bundle it came from.
#![cfg(test)]

use std::path::{Path, PathBuf};

use mb_import::world::tier0;
use mb_import::{SourceFile, compile_files};
use mb_mcm::world::{OpKind, Pointer, validate};
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
                assert!(
                    fields.contains(&member.member.as_str()),
                    "{at}: {}",
                    member.member
                );
                let agrees = b.operations.iter().all(|o| {
                    o.resource_type.as_deref() != Some(t)
                        || o.identity.as_ref().is_none_or(|i| i.field == member.member)
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
