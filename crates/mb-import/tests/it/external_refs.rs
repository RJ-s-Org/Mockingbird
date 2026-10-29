#![cfg(test)]
//! Multi-file OpenAPI, Swagger and AsyncAPI contracts whose `$ref`s name
//! other uploaded files (MB-IMPORT-STUB-2, docs/refinement/design-external-refs.md).

use std::io::Write;

use mb_import::{ImportError, SourceFile, compile, compile_files};
use serde_json::{Value, json};

fn file(path: &str, text: &str) -> SourceFile {
    SourceFile {
        path: path.into(),
        bytes: text.as_bytes().to_vec(),
    }
}

fn zip_of(files: &[SourceFile]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for f in files {
        zip.start_file(f.path.as_str(), zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(&f.bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn message(err: ImportError) -> String {
    err.to_string()
}

const ROOT: &str = r#"
openapi: 3.0.3
info: {title: Pets, version: "1"}
paths:
  /pets:
    $ref: ./paths/pets.yaml
  /pets/{id}:
    get:
      operationId: getPet
      parameters:
        - $ref: ./parameters.yaml#/PetId
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                $ref: ./schemas/pet.yaml
        "404":
          $ref: '#/components/responses/NotFound'
components:
  responses:
    NotFound:
      description: missing
      content:
        application/json:
          schema:
            $ref: ./schemas/common.yaml#/Error
  schemas:
    Owner:
      $ref: ./schemas/owner.yaml
"#;

const PETS_PATH: &str = r#"
get:
  operationId: listPets
  responses:
    "200":
      description: ok
      content:
        application/json:
          schema:
            type: array
            items:
              $ref: ../schemas/pet.yaml
"#;

const PARAMETERS: &str = r#"
PetId:
  name: id
  in: path
  required: true
  schema: {type: integer}
"#;

const PET: &str = r#"
type: object
required: [id, name]
properties:
  id: {type: integer}
  name: {type: string}
  owner:
    $ref: ./owner.yaml
  tag:
    $ref: '#/definitions/Tag'
definitions:
  Tag: {type: string, maxLength: 8}
"#;

const OWNER: &str = r#"
type: object
properties:
  name: {type: string}
  pets:
    type: array
    items:
      $ref: ./pet.yaml
"#;

const COMMON: &str = r#"
Error:
  type: object
  properties:
    message: {type: string}
"#;

fn split_spec() -> Vec<SourceFile> {
    vec![
        file("openapi.yaml", ROOT),
        file("paths/pets.yaml", PETS_PATH),
        file("parameters.yaml", PARAMETERS),
        file("schemas/pet.yaml", PET),
        file("schemas/owner.yaml", OWNER),
        file("schemas/common.yaml", COMMON),
        file("README.md", "# Pets"),
    ]
}

fn output_schema(b: &mb_mcm::Bundle, op: &str, status: u16) -> Value {
    let op = b.operations.iter().find(|o| o.id == op).unwrap();
    op.outputs
        .iter()
        .find(|o| o.status == mb_mcm::StatusSelector::Code(status))
        .and_then(|o| o.schema.clone())
        .unwrap()
}

#[test]
fn split_openapi_archive_compiles_into_one_contract() {
    let b = compile(&zip_of(&split_spec())).unwrap();
    let mut ids: Vec<&str> = b.operations.iter().map(|o| o.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["getPet", "listPets"]);

    // Schema refs are hoisted into named definitions; the root's
    // `Owner: {$ref: ./schemas/owner.yaml}` keeps its name, and the
    // recursive pet <-> owner refs become local ones.
    assert_eq!(
        output_schema(&b, "getPet", 200),
        json!({"$ref": "#/$defs/pet"})
    );
    assert_eq!(
        output_schema(&b, "listPets", 200),
        json!({"type": "array", "items": {"$ref": "#/$defs/pet"}})
    );
    let pet = &b.defs["pet"];
    assert_eq!(pet["properties"]["owner"], json!({"$ref": "#/$defs/Owner"}));
    assert_eq!(pet["properties"]["tag"], json!({"$ref": "#/$defs/Tag"}));
    assert_eq!(b.defs["Tag"], json!({"type": "string", "maxLength": 8}));
    assert_eq!(
        b.defs["Owner"]["properties"]["pets"]["items"],
        json!({"$ref": "#/$defs/pet"})
    );
    assert_eq!(
        output_schema(&b, "getPet", 404),
        json!({"$ref": "#/$defs/Error"})
    );

    // The inlined parameter is compiled like a local one.
    let get = b.operations.iter().find(|o| o.id == "getPet").unwrap();
    assert_eq!(
        get.input.path.as_ref().unwrap()["properties"]["id"],
        json!({"type": "integer"})
    );
}

#[test]
fn loose_files_compile_deterministically_in_any_order() {
    let files = split_spec();
    let a = compile_files(&files).unwrap();
    let mut reversed = files.clone();
    reversed.reverse();
    let b = compile_files(&reversed).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.source_digest.len(), 64);
    // Changing a referenced file changes the digest.
    let mut edited = files;
    edited[3] = file(
        "schemas/pet.yaml",
        &PET.replace("maxLength: 8", "maxLength: 9"),
    );
    let c = compile_files(&edited).unwrap();
    assert_ne!(a.source_digest, c.source_digest);
    assert_eq!(c.defs["Tag"]["maxLength"], json!(9));
}

#[test]
fn a_shared_contract_referenced_by_the_root_is_not_a_second_root() {
    let common = r#"
openapi: 3.0.3
info: {title: Common, version: "1"}
paths: {}
components:
  schemas:
    Error: {type: object, properties: {code: {type: integer}}}
"#;
    let root = r#"
openapi: 3.0.3
info: {title: Orders, version: "1"}
paths:
  /orders:
    get:
      operationId: listOrders
      responses:
        default:
          description: error
          content:
            application/json:
              schema:
                $ref: common.yaml#/components/schemas/Error
"#;
    let b = compile_files(&[file("common.yaml", common), file("orders.yaml", root)]).unwrap();
    assert_eq!(b.title, "Orders");
    assert_eq!(
        b.defs["Error"]["properties"]["code"],
        json!({"type": "integer"})
    );
}

#[test]
fn swagger2_definitions_from_another_file() {
    let root = r#"
swagger: "2.0"
info: {title: S, version: "1"}
paths:
  /a:
    get:
      operationId: getA
      responses:
        "200":
          description: ok
          schema:
            $ref: defs.json#/A
"#;
    let defs = r##"{"A": {"type": "object", "properties": {"b": {"$ref": "#/B"}}}, "B": {"type": "boolean"}}"##;
    let b = compile_files(&[file("api.yaml", root), file("defs.json", defs)]).unwrap();
    assert_eq!(output_schema(&b, "getA", 200), json!({"$ref": "#/$defs/A"}));
    assert_eq!(b.defs["A"]["properties"]["b"], json!({"$ref": "#/$defs/B"}));
    assert_eq!(b.defs["B"], json!({"type": "boolean"}));
}

#[test]
fn asyncapi_payload_from_another_file() {
    let root = r#"
asyncapi: 2.6.0
info: {title: Events, version: "1"}
servers:
  k: {url: "localhost:9092", protocol: kafka}
channels:
  user/signedup:
    subscribe:
      operationId: onSignup
      message:
        payload:
          $ref: ./schemas/user.json
"#;
    let user = r#"{"type": "object", "properties": {"email": {"type": "string"}}}"#;
    let b = compile_files(&[file("asyncapi.yaml", root), file("schemas/user.json", user)]).unwrap();
    assert_eq!(
        b.defs["user"]["properties"]["email"],
        json!({"type": "string"})
    );
}

#[test]
fn a_name_taken_by_a_root_schema_gets_a_suffix() {
    let root = r#"
openapi: 3.1.0
info: {title: T, version: "1"}
paths:
  /a:
    get:
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: ./other/pet.yaml}
components:
  schemas:
    pet: {type: string}
"#;
    let b = compile_files(&[
        file("api.yaml", root),
        file("other/pet.yaml", "type: integer"),
    ])
    .unwrap();
    assert_eq!(b.defs["pet"], json!({"type": "string"}));
    assert_eq!(b.defs["pet_2"], json!({"type": "integer"}));
}

#[test]
fn missing_file_is_named() {
    let mut files = split_spec();
    files.retain(|f| f.path != "schemas/owner.yaml");
    let err = message(compile_files(&files).unwrap_err());
    assert!(err.contains("schemas/owner.yaml"), "{err}");
    assert!(err.contains("not in the upload"), "{err}");
}

#[test]
fn missing_pointer_is_named() {
    let mut files = split_spec();
    files[5] = file("schemas/common.yaml", "Other: {type: string}");
    let err = message(compile_files(&files).unwrap_err());
    assert!(err.contains("/Error"), "{err}");
    assert!(err.contains("schemas/common.yaml"), "{err}");
}

#[test]
fn traversal_above_the_upload_root_is_refused() {
    for r in ["../../etc/passwd", "/etc/passwd", "..\\secret.yaml"] {
        let root = format!(
            "openapi: 3.0.3\ninfo: {{title: T, version: '1'}}\npaths: {{}}\ncomponents:\n  schemas:\n    A: {{$ref: '{r}'}}\n"
        );
        let err = message(
            compile_files(&[file("api/openapi.yaml", &root), file("api/x.yaml", "{}")])
                .unwrap_err(),
        );
        assert!(err.contains(r), "{r}: {err}");
        assert!(err.contains("upload"), "{r}: {err}");
    }
}

#[test]
fn urls_are_never_fetched() {
    let root = r#"
openapi: 3.0.3
info: {title: T, version: "1"}
paths:
  /a:
    get:
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "https://example.com/pet.yaml"}
"#;
    let err = message(compile_files(&[file("api.yaml", root), file("x.yaml", "{}")]).unwrap_err());
    assert!(err.contains("https://example.com/pet.yaml"), "{err}");
    assert!(err.contains("never fetched"), "{err}");
}

#[test]
fn a_non_schema_cycle_is_refused() {
    let root = r#"
openapi: 3.0.3
info: {title: T, version: "1"}
paths:
  /a: {$ref: ./a.yaml}
"#;
    let a = "$ref: ./b.yaml\n";
    let b = "$ref: ./a.yaml\n";
    let err = message(
        compile_files(&[file("api.yaml", root), file("a.yaml", a), file("b.yaml", b)]).unwrap_err(),
    );
    assert!(err.contains("cycl"), "{err}");
}

#[test]
fn exponential_inlining_is_capped() {
    // Each level is a parameter list that references the next level twice;
    // 40 levels would inline 2^40 copies.
    let mut files = vec![file(
        "api.yaml",
        "openapi: 3.0.3\ninfo: {title: T, version: '1'}\npaths:\n  /a: {$ref: ./l0.yaml}\n",
    )];
    for i in 0..40 {
        let next = i + 1;
        files.push(file(
            &format!("l{i}.yaml"),
            &format!("x-pad: {}\nsummary: {{$ref: ./l{next}.yaml}}\ndescription: {{$ref: ./l{next}.yaml}}\n", "p".repeat(64)),
        ));
    }
    files.push(file("l40.yaml", "{}"));
    let start = std::time::Instant::now();
    let err = message(compile_files(&files).unwrap_err());
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
    assert!(err.contains("32 MiB"), "{err}");
}

#[test]
fn a_single_file_with_an_external_ref_says_what_to_do() {
    let root = r#"
openapi: 3.0.3
info: {title: T, version: "1"}
paths:
  /a:
    get:
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: ./schemas/pet.yaml}
"#;
    let err = message(compile(root.as_bytes()).unwrap_err());
    assert!(err.contains("external $ref ./schemas/pet.yaml"), "{err}");
    assert!(err.contains("archive"), "{err}");
}

#[test]
fn a_single_file_without_external_refs_is_unchanged() {
    // Bundling never runs for a lone file or an upload without external refs:
    // the digest stays the digest of the contract's bytes.
    let root = "openapi: 3.0.3\ninfo: {title: T, version: '1'}\npaths: {}\n";
    let alone = compile(root.as_bytes()).unwrap();
    let with_readme = compile_files(&[file("api.yaml", root), file("README.md", "hi")]).unwrap();
    assert_eq!(alone, with_readme);
}
