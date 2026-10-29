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
