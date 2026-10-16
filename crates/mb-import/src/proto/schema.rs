//! Protobuf messages and enums as JSON Schema, matching prost-reflect's
//! default JSON serialization (proto3 JSON mapping, field JSON names).

use super::comment;
use prost_reflect::{EnumDescriptor, FieldDescriptor, Kind, MessageDescriptor};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const DURATION_PATTERN: &str = r"^-?[0-9]+(\.[0-9]+)?s$";

pub(crate) fn def_ref(full_name: &str) -> Value {
    json!({ "$ref": format!("#/$defs/{full_name}") })
}

fn int64() -> Value {
    json!({"type": "string", "format": "int64", "pattern": "^-?[0-9]+$"})
}

fn uint64() -> Value {
    json!({"type": "string", "format": "uint64", "pattern": "^[0-9]+$"})
}

fn scalar(kind: &Kind) -> Option<Value> {
    Some(match kind {
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => {
            json!({"type": "integer", "minimum": i32::MIN, "maximum": i32::MAX})
        }
        Kind::Uint32 | Kind::Fixed32 => {
            json!({"type": "integer", "minimum": 0, "maximum": u32::MAX})
        }
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => int64(),
        Kind::Uint64 | Kind::Fixed64 => uint64(),
        Kind::Float | Kind::Double => json!({"type": "number"}),
        Kind::Bool => json!({"type": "boolean"}),
        Kind::String => json!({"type": "string"}),
        Kind::Bytes => json!({"type": "string", "format": "byte", "contentEncoding": "base64"}),
        Kind::Message(_) | Kind::Enum(_) => return None,
    })
}

/// The JSON Schema of a well-known type, which prost-reflect serializes
/// in its special JSON form; `None` for any other type.
pub(crate) fn well_known(full_name: &str) -> Option<Value> {
    let wrapped = |inner: Value| json!({"anyOf": [inner, {"type": "null"}]});
    Some(match full_name {
        "google.protobuf.Timestamp" => json!({"type": "string", "format": "date-time"}),
        "google.protobuf.Duration" => json!({
            "type": "string",
            "format": "proto-duration",
            "pattern": DURATION_PATTERN
        }),
        "google.protobuf.Empty" => json!({"type": "object", "additionalProperties": false}),
        "google.protobuf.Struct" => json!({"type": "object"}),
        "google.protobuf.Value" => json!({}),
        "google.protobuf.ListValue" => json!({"type": "array"}),
        "google.protobuf.NullValue" => json!({"type": "null"}),
        "google.protobuf.FieldMask" => json!({"type": "string"}),
        "google.protobuf.Any" => json!({
            "type": "object",
            "properties": {"@type": {"type": "string"}},
            "required": ["@type"]
        }),
        "google.protobuf.DoubleValue" | "google.protobuf.FloatValue" => {
            wrapped(json!({"type": "number"}))
        }
        "google.protobuf.Int64Value" => wrapped(int64()),
        "google.protobuf.UInt64Value" => wrapped(uint64()),
        "google.protobuf.Int32Value" => wrapped(scalar(&Kind::Int32)?),
        "google.protobuf.UInt32Value" => wrapped(scalar(&Kind::Uint32)?),
        "google.protobuf.BoolValue" => wrapped(json!({"type": "boolean"})),
        "google.protobuf.StringValue" => wrapped(json!({"type": "string"})),
        "google.protobuf.BytesValue" => wrapped(scalar(&Kind::Bytes)?),
        _ => return None,
    })
}

/// A message or enum type that may need a `$defs` entry.
#[derive(Clone)]
pub(crate) enum Named {
    Message(MessageDescriptor),
    Enum(EnumDescriptor),
}

impl Named {
    pub(crate) fn full_name(&self) -> &str {
        match self {
            Named::Message(m) => m.full_name(),
            Named::Enum(e) => e.full_name(),
        }
    }
}

/// Collects `$defs` entries, keyed (and so sorted) by full name.
#[derive(Default)]
pub(crate) struct Defs {
    defs: BTreeMap<String, Value>,
    pending: Vec<Named>,
}

impl Defs {
    /// Queue a type for a def, unless it is a map entry (always inline).
    /// Well-known types queued here get a def holding their mapped schema.
    pub(crate) fn add(&mut self, named: Named) {
        if matches!(&named, Named::Message(m) if m.is_map_entry()) {
            return;
        }
        self.pending.push(named);
    }

    /// Build every queued def and the defs of the types they reference.
    pub(crate) fn finish(mut self) -> Map<String, Value> {
        while let Some(named) = self.pending.pop() {
            let name = named.full_name().to_owned();
            if self.defs.contains_key(&name) {
                continue;
            }
            let schema = match well_known(&name) {
                Some(schema) => schema,
                None => match &named {
                    Named::Message(m) => self.message(m),
                    Named::Enum(e) => enum_schema(e),
                },
            };
            self.defs.insert(name, schema);
        }
        self.defs.into_iter().collect()
    }

    fn message(&mut self, msg: &MessageDescriptor) -> Value {
        let mut properties = Map::new();
        for field in msg.fields() {
            let schema = self.field(&field);
            properties.insert(
                field.json_name().to_owned(),
                with_description(schema, comment(&field.parent_file(), field.path())),
            );
        }
        let mut oneofs = Map::new();
        for oneof in msg.oneofs().filter(|o| !o.is_synthetic()) {
            let names: Vec<Value> = oneof.fields().map(|f| json!(f.json_name())).collect();
            oneofs.insert(oneof.name().to_owned(), Value::Array(names));
        }
        let mut obj = Map::new();
        obj.insert("type".into(), json!("object"));
        obj.insert("properties".into(), Value::Object(properties));
        obj.insert("additionalProperties".into(), json!(false));
        if !oneofs.is_empty() {
            obj.insert("x-oneof".into(), Value::Object(oneofs));
        }
        with_description(Value::Object(obj), comment(&msg.parent_file(), msg.path()))
    }

    /// The schema of one field: lists and maps around the value schema.
    fn field(&mut self, field: &FieldDescriptor) -> Value {
        if field.is_map()
            && let Kind::Message(entry) = field.kind()
        {
            let value = self.value(&entry.map_entry_value_field().kind());
            return json!({"type": "object", "additionalProperties": value});
        }
        let value = self.value(&field.kind());
        if field.is_list() {
            json!({"type": "array", "items": value})
        } else {
            value
        }
    }

    /// The schema of a singular value of `kind`: a scalar, an inline
    /// well-known type, or a `$ref` to a queued def.
    fn value(&mut self, kind: &Kind) -> Value {
        if let Some(schema) = scalar(kind) {
            return schema;
        }
        let named = match kind {
            Kind::Message(m) => Named::Message(m.clone()),
            Kind::Enum(e) => Named::Enum(e.clone()),
            _ => return json!({}),
        };
        if let Some(schema) = well_known(named.full_name()) {
            return schema;
        }
        let schema = def_ref(named.full_name());
        self.add(named);
        schema
    }
}

fn enum_schema(e: &EnumDescriptor) -> Value {
    let values: Vec<Value> = e.values().map(|v| json!(v.name())).collect();
    with_description(
        json!({"type": "string", "enum": values}),
        comment(&e.parent_file(), e.path()),
    )
}

fn with_description(mut schema: Value, description: Option<String>) -> Value {
    if let (Some(d), Value::Object(obj)) = (description, &mut schema) {
        obj.insert("description".into(), json!(d));
    }
    schema
}
