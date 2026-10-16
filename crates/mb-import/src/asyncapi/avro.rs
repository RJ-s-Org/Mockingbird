//! Avro schema (JSON form) -> JSON Schema 2020-12. Named types (records,
//! enums, fixed) are added to the definitions under their full name;
//! references to them by name become `$ref: "#/$defs/<fullname>"`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::ImportError;
use crate::openapi3::invalid;

/// Converts `avro` into JSON Schema, adding its named types to `defs`.
/// `at` points at the payload.
pub(crate) fn avro_to_json_schema(
    avro: &Value,
    defs: &mut BTreeMap<String, Value>,
    at: &str,
) -> Result<Value, ImportError> {
    Converter {
        defs,
        local: BTreeSet::new(),
        at,
    }
    .convert(avro, None)
}

struct Converter<'d, 'a> {
    defs: &'d mut BTreeMap<String, Value>,
    /// Named types defined by this conversion so far.
    local: BTreeSet<String>,
    at: &'a str,
}

fn primitive(name: &str) -> Option<Value> {
    Some(match name {
        "null" => json!({"type": "null"}),
        "boolean" => json!({"type": "boolean"}),
        "int" => json!({"type": "integer", "format": "int32"}),
        "long" => json!({"type": "integer", "format": "int64"}),
        "float" | "double" => json!({"type": "number"}),
        "bytes" | "string" => json!({"type": "string"}),
        _ => return None,
    })
}

fn logical(name: &str) -> Option<Value> {
    Some(match name {
        "date" => json!({"type": "string", "format": "date"}),
        "timestamp-millis"
        | "timestamp-micros"
        | "timestamp-nanos"
        | "local-timestamp-millis"
        | "local-timestamp-micros"
        | "local-timestamp-nanos" => json!({"type": "string", "format": "date-time"}),
        "uuid" => json!({"type": "string", "format": "uuid"}),
        "decimal" => json!({"type": "number"}),
        "time-millis" | "time-micros" => json!({"type": "string", "format": "time"}),
        _ => return None,
    })
}

fn def_ref(fullname: &str) -> Value {
    json!({ "$ref": format!("#/$defs/{fullname}") })
}

/// Whether a field type is a union with a `null` branch.
fn is_nullable_union(t: &Value) -> bool {
    t.as_array().is_some_and(|members| {
        members
            .iter()
            .any(|m| m.as_str().or_else(|| m.get("type").and_then(Value::as_str)) == Some("null"))
    })
}

fn with_doc(mut schema: Value, doc: Option<&Value>) -> Value {
    if let (Some(doc), Value::Object(obj)) = (doc.and_then(Value::as_str), &mut schema) {
        obj.insert("description".into(), json!(doc));
    }
    schema
}

impl Converter<'_, '_> {
    fn convert(&mut self, t: &Value, ns: Option<&str>) -> Result<Value, ImportError> {
        match t {
            Value::String(name) => self.named_or_primitive(name, ns),
            Value::Array(members) => {
                let any_of = members
                    .iter()
                    .map(|m| self.convert(m, ns))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(json!({ "anyOf": any_of }))
            }
            Value::Object(obj) => {
                let base = match obj.get("type") {
                    Some(Value::String(ty)) => match ty.as_str() {
                        "record" | "error" => self.record(obj, ns)?,
                        "enum" => self.enumeration(obj, ns)?,
                        "fixed" => self.fixed(obj, ns)?,
                        "array" => {
                            let items = self.child(obj, "items", ns)?;
                            json!({"type": "array", "items": items})
                        }
                        "map" => {
                            let values = self.child(obj, "values", ns)?;
                            json!({"type": "object", "additionalProperties": values})
                        }
                        other => self.named_or_primitive(other, ns)?,
                    },
                    Some(inner @ (Value::Object(_) | Value::Array(_))) => {
                        self.convert(inner, ns)?
                    }
                    _ => return Err(invalid(self.at, "Avro schema without a type")),
                };
                Ok(obj
                    .get("logicalType")
                    .and_then(Value::as_str)
                    .and_then(logical)
                    .unwrap_or(base))
            }
            _ => Err(invalid(self.at, format!("invalid Avro schema {t}"))),
        }
    }

    fn child(
        &mut self,
        obj: &Map<String, Value>,
        key: &str,
        ns: Option<&str>,
    ) -> Result<Value, ImportError> {
        let t = obj
            .get(key)
            .ok_or_else(|| invalid(self.at, format!("Avro schema without `{key}`")))?;
        self.convert(t, ns)
    }

    fn named_or_primitive(&self, name: &str, ns: Option<&str>) -> Result<Value, ImportError> {
        if let Some(p) = primitive(name) {
            return Ok(p);
        }
        let qualified = match ns {
            Some(ns) if !ns.is_empty() && !name.contains('.') => Some(format!("{ns}.{name}")),
            _ => None,
        };
        for candidate in qualified.iter().map(String::as_str).chain([name]) {
            if self.local.contains(candidate) || self.defs.contains_key(candidate) {
                return Ok(def_ref(candidate));
            }
        }
        Err(invalid(self.at, format!("unknown Avro type {name:?}")))
    }

    /// The full name of a named type and the namespace its children
    /// inherit.
    fn full_name(
        &self,
        obj: &Map<String, Value>,
        ns: Option<&str>,
    ) -> Result<(String, Option<String>), ImportError> {
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(self.at, "Avro named type without a name"))?;
        if let Some((space, _)) = name.rsplit_once('.') {
            return Ok((name.to_owned(), Some(space.to_owned())));
        }
        let space = obj
            .get("namespace")
            .and_then(Value::as_str)
            .or(ns)
            .filter(|s| !s.is_empty());
        Ok(match space {
            Some(space) => (format!("{space}.{name}"), Some(space.to_owned())),
            None => (name.to_owned(), None),
        })
    }

    /// Reserves `fullname` so the type's own body may reference it.
    fn reserve(&mut self, fullname: &str) -> Result<(), ImportError> {
        if !self.local.insert(fullname.to_owned()) {
            return Err(invalid(
                self.at,
                format!("Avro type {fullname:?} is defined twice"),
            ));
        }
        Ok(())
    }

    /// Stores a named type's schema in the definitions.
    fn define(&mut self, fullname: String, schema: Value) -> Result<Value, ImportError> {
        if let Some(existing) = self.defs.get(&fullname)
            && *existing != schema
        {
            return Err(invalid(
                self.at,
                format!("Avro type {fullname:?} collides with a schema of the same name"),
            ));
        }
        self.defs.insert(fullname, schema.clone());
        Ok(schema)
    }

    fn record(&mut self, obj: &Map<String, Value>, ns: Option<&str>) -> Result<Value, ImportError> {
        let (fullname, space) = self.full_name(obj, ns)?;
        self.reserve(&fullname)?;
        let mut properties = Map::new();
        let mut required = Vec::new();
        let fields = obj
            .get("fields")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid(self.at, format!("Avro record {fullname:?} without fields")))?;
        for field in fields {
            let name = field
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid(self.at, format!("a field of {fullname:?} has no name")))?;
            let ty = field
                .get("type")
                .ok_or_else(|| invalid(self.at, format!("field {fullname}.{name} has no type")))?;
            let schema = with_doc(self.convert(ty, space.as_deref())?, field.get("doc"));
            if field.get("default").is_none() && !is_nullable_union(ty) {
                required.push(json!(name));
            }
            properties.insert(name.to_owned(), schema);
        }
        let mut schema = json!({
            "type": "object",
            "properties": properties,
            "additionalProperties": false,
        });
        if !required.is_empty() {
            schema["required"] = Value::Array(required);
        }
        self.define(fullname, with_doc(schema, obj.get("doc")))
    }

    fn enumeration(
        &mut self,
        obj: &Map<String, Value>,
        ns: Option<&str>,
    ) -> Result<Value, ImportError> {
        let (fullname, _) = self.full_name(obj, ns)?;
        self.reserve(&fullname)?;
        let symbols = obj
            .get("symbols")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| invalid(self.at, format!("Avro enum {fullname:?} without symbols")))?;
        let schema = json!({"type": "string", "enum": symbols});
        self.define(fullname, with_doc(schema, obj.get("doc")))
    }

    fn fixed(&mut self, obj: &Map<String, Value>, ns: Option<&str>) -> Result<Value, ImportError> {
        let (fullname, _) = self.full_name(obj, ns)?;
        self.reserve(&fullname)?;
        let schema = with_doc(json!({"type": "string"}), obj.get("doc"));
        self.define(fullname, schema)
    }
}
