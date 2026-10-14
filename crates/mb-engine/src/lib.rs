//! Mock response engine: schema validation and schema-driven faking.

pub mod faker;
pub mod validate;

pub use faker::Faker;
pub use validate::SchemaValidator;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("invalid schema: {0}")]
    Schema(String),
}
