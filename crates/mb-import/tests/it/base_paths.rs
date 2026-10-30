#![cfg(test)]
//! `base_paths`: the Swagger `basePath` and OpenAPI `servers[].url` path
//! prefixes a contract's HTTP operations are also served under.

use mb_import::base_paths;
use serde_json::{Value, json};

use crate::support::fixture;

fn swagger(base_path: Option<&str>) -> Vec<u8> {
    let mut doc = json!({
        "swagger": "2.0",
        "info": {"title": "Orders", "version": "1"},
        "paths": {"/orders": {"get": {"responses": {"200": {"description": "ok"}}}}}
    });
    if let Some(b) = base_path {
        doc["basePath"] = json!(b);
    }
    serde_json::to_vec(&doc).unwrap()
}

fn openapi(servers: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "openapi": "3.0.3",
        "info": {"title": "Orders", "version": "1"},
        "servers": servers,
        "paths": {"/orders": {"get": {"responses": {"200": {"description": "ok"}}}}}
    }))
    .unwrap()
}

#[test]
fn swagger_base_path() {
    assert_eq!(base_paths(&swagger(Some("/v1"))), ["/v1"]);
    assert_eq!(base_paths(&swagger(Some("/api/v1/"))), ["/api/v1"]);
    assert!(base_paths(&swagger(Some("/"))).is_empty());
    assert!(base_paths(&swagger(Some(""))).is_empty());
    assert!(base_paths(&swagger(None)).is_empty());
    // Not a path (basePath must start with `/`).
    assert!(base_paths(&swagger(Some("v1"))).is_empty());
}

#[test]
fn swagger_base_path_in_yaml() {
    let yaml = b"swagger: '2.0'\ninfo: {title: Orders, version: '1'}\nbasePath: /v2\npaths: {}\n";
    assert_eq!(base_paths(yaml), ["/v2"]);
}

#[test]
fn openapi_server_paths_in_order_without_duplicates() {
    let doc = openapi(json!([
        {"url": "https://api.example.com/v2"},
        {"url": "/v2/"},
        {"url": "https://sandbox.example.com:8443/beta?x=1#top"},
        {"url": "https://api.example.com"},
        {"url": "https://api.example.com/"},
        {"url": "/"}
    ]));
    assert_eq!(base_paths(&doc), ["/v2", "/beta"]);
}

#[test]
fn openapi_server_variables_use_their_defaults() {
    let doc = openapi(json!([
        {
            "url": "https://{region}.example.com/{version}/api",
            "variables": {
                "region": {"default": "eu"},
                "version": {"default": "v3", "enum": ["v3", "v4"]}
            }
        },
        // A variable without a default, or not declared, skips the server.
        {"url": "/{tenant}/api", "variables": {"tenant": {"enum": ["a"]}}},
        {"url": "/{undeclared}"}
    ]));
    assert_eq!(base_paths(&doc), ["/v3/api"]);
}

#[test]
fn openapi_relative_urls_without_a_leading_slash_are_ignored() {
    let doc = openapi(json!([{"url": "v1"}, {"url": "./v1"}, {"url": "."}]));
    assert!(base_paths(&doc).is_empty());
}

#[test]
fn no_base_paths_without_servers_or_for_other_formats() {
    assert!(base_paths(&fixture("openapi/orders-3.0.yaml")).is_empty());
    assert!(base_paths(&fixture("swagger/orders-2.0.json")).is_empty());
    // AsyncAPI servers are brokers, not HTTP prefixes.
    assert!(base_paths(&fixture("asyncapi/orders-amqp-2.6.yaml")).is_empty());
    assert!(base_paths(b"type Query { hello: String }").is_empty());
    assert!(base_paths(b"not a contract").is_empty());
}
