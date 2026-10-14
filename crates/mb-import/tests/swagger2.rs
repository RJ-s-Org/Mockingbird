#![allow(clippy::unwrap_used)]

use mb_import::compile;
use mb_mcm::Bundle;
use serde_json::json;

fn read(dir: &str, name: &str) -> Vec<u8> {
    let path = format!("{}/../../fixtures/{dir}/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(path).unwrap()
}

fn orders20() -> Bundle {
    compile(&read("swagger", "orders-2.0.json")).unwrap()
}

#[test]
fn compiles_orders_20_like_30() {
    let a = compile(&read("openapi", "orders-3.0.yaml")).unwrap();
    let b = orders20();
    assert_eq!(a.operations.len(), b.operations.len());
    for (x, y) in a.operations.iter().zip(&b.operations) {
        assert_eq!(x.id, y.id);
        assert_eq!(x.kind, y.kind);
        assert_eq!(x.identity, y.identity);
    }
    assert_eq!(
        a.defs.keys().collect::<Vec<_>>(),
        b.defs.keys().collect::<Vec<_>>()
    );
    assert_eq!(a.operations, b.operations);
    assert_eq!(a.defs, b.defs);
}

#[test]
fn swagger_body_param_becomes_input_body() {
    let b = orders20();
    assert_eq!(
        b.operation("createOrder").unwrap().input.body,
        Some(json!({"$ref": "#/$defs/NewOrder"}))
    );
}

#[test]
fn swagger_x_nullable() {
    let b = orders20();
    assert_eq!(
        b.defs["Order"]["properties"]["note"]["type"],
        json!(["string", "null"])
    );
}

#[test]
fn swagger_form_data_becomes_form_body() {
    let doc = br#"{"swagger":"2.0","info":{"title":"T","version":"1"},
      "paths":{"/login":{"post":{"operationId":"login",
        "consumes":["application/x-www-form-urlencoded"],
        "parameters":[
          {"name":"user","in":"formData","type":"string","required":true},
          {"name":"age","in":"formData","type":"integer"}],
        "responses":{"200":{"description":"ok"}}}}}}"#;
    let b = compile(doc).unwrap();
    let body = b.operation("login").unwrap().input.body.clone().unwrap();
    assert_eq!(body["type"], json!("object"));
    assert_eq!(body["required"], json!(["user"]));
    assert_eq!(body["properties"]["age"], json!({"type": "integer"}));
    assert_eq!(
        body["x-content-type"],
        json!("application/x-www-form-urlencoded")
    );
}

#[test]
fn swagger_produces_controls_response_schema() {
    let doc = br#"{"swagger":"2.0","info":{"title":"T","version":"1"},
      "produces":["application/json"],
      "paths":{"/a":{"get":{"operationId":"a","produces":["text/plain"],
        "responses":{"200":{"description":"ok","schema":{"type":"string"}}}}},
        "/b":{"get":{"operationId":"b",
        "responses":{"200":{"description":"ok","schema":{"type":"string"}}}}}}}"#;
    let b = compile(doc).unwrap();
    let a = b.operation("a").unwrap().success_output().unwrap();
    assert_eq!(a.content_type.as_deref(), Some("text/plain"));
    assert_eq!(a.schema, None);
    let o = b.operation("b").unwrap().success_output().unwrap();
    assert_eq!(o.content_type.as_deref(), Some("application/json"));
    assert_eq!(o.schema, Some(json!({"type": "string"})));
}

#[test]
fn unquoted_numeric_versions_are_detected() {
    let y = b"swagger: 2.0\ninfo: {title: T, version: '1'}\npaths: {}\n";
    assert!(compile(y).is_ok());
    let y = b"openapi: 3.1\ninfo: {title: T, version: '1'}\npaths: {}\n";
    assert!(compile(y).is_ok());
}
