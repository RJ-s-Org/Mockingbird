use serde_json::Value;

use crate::EngineError;

const MAX_ERRORS: usize = 10;

/// JSON Schema (draft 2020-12) validator over a schema that carries its own
/// `$defs`.
pub struct SchemaValidator {
    inner: jsonschema::Validator,
}

impl SchemaValidator {
    pub fn new(schema_with_defs: &Value) -> Result<Self, EngineError> {
        let inner = jsonschema::draft202012::new(schema_with_defs)
            .map_err(|e| EngineError::Schema(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Returns up to 10 violations formatted as `"<instance path>: <message>"`.
    pub fn validate(&self, instance: &Value) -> Result<(), Vec<String>> {
        let errors: Vec<String> = self
            .inner
            .iter_errors(instance)
            .take(MAX_ERRORS)
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}
