//! Fixtures shared by the test modules.

use std::sync::Arc;

use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{LlmError, Provider, ScriptedProvider};
use mb_mcm::Bundle;
use serde_json::{Value, json};

/// The orders fixture (`fixtures/openapi/orders-3.0.yaml`), compiled.
pub fn orders() -> Bundle {
    let path = format!(
        "{}/../../fixtures/openapi/orders-3.0.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    compile(&std::fs::read(path).unwrap()).unwrap()
}

/// An `Order` that validates against the orders fixture.
pub fn valid_order() -> Value {
    json!({
        "id": "7f9c2ba4-e88f-4c1b-9f4a-3d2f0c6b1a10",
        "customerId": "c1",
        "status": "pending",
        "total": 42.5
    })
}

/// An engine whose provider replays `script`, and that provider.
pub fn scripted(script: Vec<Result<String, LlmError>>) -> (Arc<ScriptedProvider>, Engine) {
    let provider = Arc::new(ScriptedProvider::new(script));
    let dyn_provider: Arc<dyn Provider> = provider.clone();
    (
        provider,
        Engine::new(Some(dyn_provider), EngineConfig::default()),
    )
}

/// A contract with `defs` component schemas `Item0..`, each with `fields`
/// patterned string members and an optional link to the next one:
/// `listItems`, `getItem` (`/items/{id}`) and `createItem` (body and 201
/// answer `Item0`). Sized for the timing harnesses.
pub fn wide(defs: usize, fields: usize) -> Bundle {
    let mut schemas = serde_json::Map::new();
    for i in 0..defs {
        let mut properties = serde_json::Map::new();
        properties.insert("id".into(), json!({"type": "string"}));
        for j in 0..fields {
            properties.insert(
                format!("f{j}"),
                json!({"type": "string", "pattern": "^[a-z]{3}-[0-9]{4}$", "maxLength": 8}),
            );
        }
        if i + 1 < defs {
            properties.insert(
                "next".into(),
                json!({"$ref": format!("#/components/schemas/Item{}", i + 1)}),
            );
        }
        let required: Vec<String> = std::iter::once("id".to_string())
            .chain((0..fields).map(|j| format!("f{j}")))
            .collect();
        schemas.insert(
            format!("Item{i}"),
            json!({
                "type": "object",
                "required": required,
                "properties": properties,
                "additionalProperties": false
            }),
        );
    }
    let item = json!({"$ref": "#/components/schemas/Item0"});
    let ok = |schema: &Value| json!({"description": "ok", "content": {"application/json": {"schema": schema}}});
    let doc = json!({
        "openapi": "3.0.3",
        "info": {"title": "wide", "version": "1"},
        "paths": {
            "/items": {
                "get": {
                    "operationId": "listItems",
                    "responses": {"200": ok(&json!({"type": "array", "items": item}))}
                },
                "post": {
                    "operationId": "createItem",
                    "requestBody": {"required": true, "content": {"application/json": {"schema": item}}},
                    "responses": {"201": ok(&item)}
                }
            },
            "/items/{id}": {
                "get": {
                    "operationId": "getItem",
                    "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": ok(&item)}
                }
            }
        },
        "components": {"schemas": schemas}
    });
    compile(doc.to_string().as_bytes()).unwrap()
}
