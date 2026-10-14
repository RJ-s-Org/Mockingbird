//! Mockingbird Contract Model: protocol-neutral description of an API contract.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct Bundle {
    /// Slug derived from the title, e.g. `orders-api`.
    pub id: String,
    pub title: String,
    /// The contract's `info.version`.
    pub version: String,
    /// Hex sha256 of the source bytes.
    pub source_digest: String,
    pub operations: Vec<Operation>,
    /// JSON Schema 2020-12 definitions.
    pub defs: Map<String, Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Protocol {
    Http,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Binding {
    /// `path` uses `{param}` placeholders.
    Http { method: HttpMethod, path: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Query,
    Command(Effect),
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Create,
    Update,
    Delete,
    Other,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct Operation {
    /// `operationId` or a `{method}_{path}` slug.
    pub id: String,
    pub protocol: Protocol,
    pub binding: Binding,
    pub kind: OperationKind,
    pub summary: Option<String>,
    pub input: InputSchemas,
    pub outputs: Vec<OutputVariant>,
    /// e.g. `orders`.
    pub resource_type: Option<String>,
    pub identity: Option<Identity>,
    /// Description text and tags.
    pub semantic_hints: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct InputSchemas {
    pub path: Option<Value>,
    pub query: Option<Value>,
    pub headers: Option<Value>,
    pub body: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusSelector {
    Code(u16),
    Default,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct OutputVariant {
    pub status: StatusSelector,
    pub content_type: Option<String>,
    pub schema: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct Identity {
    pub path_param: Option<String>,
    pub field: String,
}

impl Bundle {
    pub fn operation(&self, id: &str) -> Option<&Operation> {
        self.operations.iter().find(|o| o.id == id)
    }

    /// Clone `schema` with this bundle's definitions attached as `$defs`, so
    /// it can be validated or faked standalone.
    pub fn with_defs(&self, schema: &Value) -> Value {
        let mut out = schema.clone();
        match out.as_object_mut() {
            Some(obj) => {
                obj.insert("$defs".into(), Value::Object(self.defs.clone()));
                out
            }
            // Boolean schemas (`true`/`false`) cannot carry `$defs`; wrap them.
            None => {
                let mut obj = Map::new();
                obj.insert("allOf".into(), Value::Array(vec![out]));
                obj.insert("$defs".into(), Value::Object(self.defs.clone()));
                Value::Object(obj)
            }
        }
    }
}

impl Operation {
    /// Lowest `Code` in 200..=299, else `Default`.
    pub fn success_output(&self) -> Option<&OutputVariant> {
        self.outputs
            .iter()
            .filter_map(|o| match o.status {
                StatusSelector::Code(c) if (200..300).contains(&c) => Some((c, o)),
                _ => None,
            })
            .min_by_key(|(c, _)| *c)
            .map(|(_, o)| o)
            .or_else(|| self.default_output())
    }

    /// Exact `Code` match, else `Default`.
    pub fn output_for_status(&self, code: u16) -> Option<&OutputVariant> {
        self.outputs
            .iter()
            .find(|o| o.status == StatusSelector::Code(code))
            .or_else(|| self.default_output())
    }

    /// First exact match from `preferred`, else any 4xx `Code`.
    pub fn error_output(&self, preferred: &[u16]) -> Option<&OutputVariant> {
        preferred
            .iter()
            .find_map(|c| {
                self.outputs
                    .iter()
                    .find(|o| o.status == StatusSelector::Code(*c))
            })
            .or_else(|| {
                self.outputs.iter().find(
                    |o| matches!(o.status, StatusSelector::Code(c) if (400..500).contains(&c)),
                )
            })
    }

    fn default_output(&self) -> Option<&OutputVariant> {
        self.outputs
            .iter()
            .find(|o| o.status == StatusSelector::Default)
    }
}
