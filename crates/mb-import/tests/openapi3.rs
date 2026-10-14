#![allow(clippy::unwrap_used)]

use mb_import::{ImportError, compile};
use mb_mcm::{Bundle, Effect, Identity, OperationKind, StatusSelector};
use serde_json::json;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../../fixtures/openapi/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(path).unwrap()
}

fn orders30() -> Bundle {
    compile(&fixture("orders-3.0.yaml")).unwrap()
}

#[test]
fn compiles_orders_30() {
    let b = orders30();
    assert_eq!(b.operations.len(), 5);
    let get = b.operation("getOrder").unwrap();
    assert_eq!(get.kind, OperationKind::Query);
    assert_eq!(get.resource_type.as_deref(), Some("orders"));
    assert_eq!(
        get.identity,
        Some(Identity {
            path_param: Some("orderId".into()),
            field: "id".into()
        })
    );
    let create = b.operation("createOrder").unwrap();
    assert_eq!(create.kind, OperationKind::Command(Effect::Create));
    assert_eq!(create.identity.as_ref().unwrap().path_param, None);
    let del = b.operation("deleteOrder").unwrap();
    let out = del.success_output().unwrap();
    assert_eq!(out.status, StatusSelector::Code(204));
    assert_eq!(out.schema, None);
}

#[test]
fn normalizes_nullable_30() {
    let b = orders30();
    assert_eq!(
        b.defs["Order"]["properties"]["note"]["type"],
        json!(["string", "null"])
    );
}

#[test]
fn compiles_orders_31_equivalently() {
    let a = orders30();
    let c = compile(&fixture("orders-3.1.yaml")).unwrap();
    assert_eq!(a.operations, c.operations);
    assert_eq!(a.defs, c.defs);
}

#[test]
fn compiles_recursive_schema() {
    let b = orders30();
    assert_eq!(
        b.defs["Category"]["properties"]["parent"],
        json!({"$ref": "#/$defs/Category"})
    );
}

#[test]
fn rejects_dangling_ref() {
    let doc = r##"
openapi: 3.0.3
info: {title: T, version: "1"}
paths:
  /x:
    get:
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/Missing'
"##;
    match compile(doc.as_bytes()) {
        Err(ImportError::Invalid { pointer, .. }) => assert!(pointer.contains("Missing")),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn rejects_unknown_format() {
    let r = compile(br#"{"asyncapi":"3.0.0"}"#);
    assert!(matches!(r, Err(ImportError::Unsupported(_))));
}

#[test]
fn orders_30_snapshot() {
    let b = orders30();
    insta::assert_yaml_snapshot!(b, {".source_digest" => "[digest]"});
}

fn oas(paths: &str) -> String {
    format!("openapi: 3.0.3\ninfo: {{title: T, version: '1'}}\npaths:\n{paths}")
}

#[test]
fn rejects_duplicate_operation_ids() {
    let src = oas(r#"
  /a:
    get: {operationId: dup, responses: {"200": {description: OK}}}
    post: {operationId: dup, responses: {"200": {description: OK}}}
"#);
    match compile(src.as_bytes()) {
        Err(ImportError::Invalid { pointer, message }) => {
            assert_eq!(pointer, "/paths/~1a/post/operationId");
            assert!(message.contains("dup"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn disambiguates_generated_operation_ids() {
    let src = oas(r#"
  /a-b:
    get: {responses: {"200": {description: OK}}}
  /a_b:
    get: {responses: {"200": {description: OK}}}
  /x:
    get: {responses: {"200": {description: OK}}}
  /y:
    get: {operationId: get_x, responses: {"200": {description: OK}}}
"#);
    let b = compile(src.as_bytes()).unwrap();
    let mut ids: Vec<&str> = b.operations.iter().map(|o| o.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["get_a_b", "get_a_b_2", "get_x", "get_x_2"]);
    // The explicit operationId keeps its name.
    let y = b.operation("get_x").unwrap();
    assert!(matches!(&y.binding, mb_mcm::Binding::Http { path, .. } if path == "/y"));
}

#[test]
fn imports_request_body_required() {
    let b = orders30();
    assert!(b.operation("createOrder").unwrap().input.body_required);
    assert!(!b.operation("getOrder").unwrap().input.body_required);
    let src = oas(r#"
  /a:
    post:
      requestBody:
        content: {application/json: {schema: {type: object}}}
      responses: {"200": {description: OK}}
"#);
    let b = compile(src.as_bytes()).unwrap();
    let op = &b.operations[0];
    assert!(op.input.body.is_some());
    assert!(!op.input.body_required);
}
