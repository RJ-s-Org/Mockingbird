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

fn swagger(paths: serde_json::Value) -> Vec<u8> {
    json!({"swagger": "2.0", "info": {"title": "T", "version": "1"}, "paths": paths})
        .to_string()
        .into_bytes()
}

#[test]
fn rejects_duplicate_operation_ids() {
    let ok = json!({"200": {"description": "OK"}});
    let src = swagger(json!({"/a": {
        "get": {"operationId": "dup", "responses": ok},
        "post": {"operationId": "dup", "responses": ok}
    }}));
    match compile(&src) {
        Err(mb_import::ImportError::Invalid { pointer, .. }) => {
            assert_eq!(pointer, "/paths/~1a/post/operationId");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn disambiguates_generated_operation_ids() {
    let ok = json!({"200": {"description": "OK"}});
    let src = swagger(json!({
        "/a-b": {"get": {"responses": ok}},
        "/a_b": {"get": {"responses": ok}}
    }));
    let b = compile(&src).unwrap();
    let mut ids: Vec<&str> = b.operations.iter().map(|o| o.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["get_a_b", "get_a_b_2"]);
}

#[test]
fn imports_body_and_form_required() {
    let b = orders20();
    assert!(b.operation("createOrder").unwrap().input.body_required);
    let ok = json!({"200": {"description": "OK"}});
    let src = swagger(json!({
        "/opt": {"post": {"parameters": [
            {"name": "body", "in": "body", "schema": {"type": "object"}}
        ], "responses": ok}},
        "/form": {"post": {"consumes": ["application/x-www-form-urlencoded"], "parameters": [
            {"name": "name", "in": "formData", "type": "string", "required": true}
        ], "responses": ok}},
        "/upload": {"post": {"consumes": ["multipart/form-data"], "parameters": [
            {"name": "file", "in": "formData", "type": "file"}
        ], "responses": ok}}
    }));
    let b = compile(&src).unwrap();
    let opt = b.operation("post_opt").unwrap();
    assert!(opt.input.body.is_some() && !opt.input.body_required);
    let form = b.operation("post_form").unwrap();
    assert!(form.input.body_required);
    assert_eq!(
        form.input.body.as_ref().unwrap()["x-content-type"],
        "application/x-www-form-urlencoded"
    );
    let upload = b.operation("post_upload").unwrap();
    assert_eq!(
        upload.input.body.as_ref().unwrap()["x-content-type"],
        "multipart/form-data"
    );
}
