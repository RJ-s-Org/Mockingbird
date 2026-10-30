//! `Debug` output of the public types: present, and free of the secrets the
//! values carry.

use std::sync::Arc;

use mb_engine::{
    Engine, EngineConfig, EngineGuards, Faker, FakerHints, GenerationLimiter, SchemaValidator,
    TokenBudget,
};
use serde_json::{Map, json};

const SECRET: &str = "s3cr3t-hunter2";

struct Budget;

impl TokenBudget for Budget {
    fn exhausted(&self) -> bool {
        false
    }
    fn spend(&self, _tokens: u64) {}
}

#[test]
fn faker_debug_names_hints_without_their_values() {
    let mut echoes = Map::new();
    echoes.insert("api_key".into(), json!(SECRET));
    let faker = Faker::new(4).with_hints(FakerHints { echoes });
    let text = format!("{faker:?}");
    assert!(text.contains("api_key"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
}

#[test]
fn engine_and_guards_implement_debug() {
    let guards = EngineGuards {
        limiter: Some(GenerationLimiter::new_shared(2)),
        budget: Some(Arc::new(Budget)),
        scrubber: None,
        max_response_bytes: Some(1024),
    };
    let engine = Engine::new(None, EngineConfig::default()).with_guards(guards);
    let text = format!("{engine:?}");
    assert!(text.starts_with("Engine"), "{text}");
    assert!(text.contains("has_provider: false"), "{text}");
    assert!(text.contains("max_response_bytes: Some(1024)"), "{text}");
    let validator = SchemaValidator::new(&json!({"type": "string"})).unwrap();
    assert!(format!("{validator:?}").starts_with("SchemaValidator"));
}
