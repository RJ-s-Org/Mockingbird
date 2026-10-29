//! GraphQL SDL and introspection -> MCM compiler.
//!
//! Introspection results are first rendered to SDL, so both sources go
//! through the same compiler and the bundle always carries SDL.

use crate::ImportError;
use crate::heuristics::{kind_from_name, resource_from_name};
use crate::openapi3::{invalid, slug};
use async_graphql_parser::types::{
    BaseType, FieldDefinition, InputValueDefinition, Type, TypeDefinition, TypeKind,
    TypeSystemDefinition,
};
use async_graphql_parser::{Positioned, parse_schema};
use mb_mcm::{
    Binding, Bundle, Effect, GraphqlOperationType, Identity, InputSchemas, Operation,
    OperationKind, OutputVariant, Protocol, ProtocolPayload, StatusSelector,
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const BUILTIN_SCALARS: [&str; 5] = ["Int", "Float", "String", "Boolean", "ID"];
const DEFAULT_TITLE: &str = "GraphQL API";
const ROOTS: [(GraphqlOperationType, &str); 3] = [
    (GraphqlOperationType::Query, "Query"),
    (GraphqlOperationType::Mutation, "Mutation"),
    (GraphqlOperationType::Subscription, "Subscription"),
];

fn is_builtin(name: &str) -> bool {
    BUILTIN_SCALARS.contains(&name) || name.starts_with("__")
}

// ---------------------------------------------------------------------------
// Schema model
// ---------------------------------------------------------------------------

struct InputValue {
    name: String,
    description: Option<String>,
    ty: Type,
    has_default: bool,
}

struct Field {
    name: String,
    description: Option<String>,
    args: Vec<InputValue>,
    ty: Type,
}

enum Kind {
    Scalar,
    Object {
        implements: Vec<String>,
        fields: Vec<Field>,
    },
    Interface {
        implements: Vec<String>,
        fields: Vec<Field>,
    },
    Union {
        members: Vec<String>,
    },
    Enum {
        values: Vec<String>,
    },
    Input {
        fields: Vec<InputValue>,
    },
}

struct TypeDef {
    description: Option<String>,
    kind: Kind,
}

/// The parsed schema: types by name (sorted) and the root type names.
struct Schema {
    types: BTreeMap<String, TypeDef>,
    /// Query, mutation and subscription root type names.
    roots: [Option<String>; 3],
}

impl Schema {
    fn object_fields(&self, name: &str) -> Option<&[Field]> {
        match self.types.get(name).map(|t| &t.kind) {
            Some(Kind::Object { fields, .. }) => Some(fields),
            _ => None,
        }
    }
}

fn text(s: Option<Positioned<String>>) -> Option<String> {
    s.map(|d| d.node)
}

fn names<N: ToString>(list: Vec<Positioned<N>>) -> Vec<String> {
    list.into_iter().map(|n| n.node.to_string()).collect()
}

fn input_value(v: Positioned<InputValueDefinition>) -> InputValue {
    let v = v.node;
    InputValue {
        name: v.name.node.to_string(),
        description: text(v.description),
        ty: v.ty.node,
        has_default: v.default_value.is_some(),
    }
}

fn field(f: Positioned<FieldDefinition>) -> Field {
    let f = f.node;
    Field {
        name: f.name.node.to_string(),
        description: text(f.description),
        args: f.arguments.into_iter().map(input_value).collect(),
        ty: f.ty.node,
    }
}

fn convert(t: TypeDefinition) -> TypeDef {
    let kind = match t.kind {
        TypeKind::Scalar => Kind::Scalar,
        TypeKind::Object(o) => Kind::Object {
            implements: names(o.implements),
            fields: o.fields.into_iter().map(field).collect(),
        },
        TypeKind::Interface(i) => Kind::Interface {
            implements: names(i.implements),
            fields: i.fields.into_iter().map(field).collect(),
        },
        TypeKind::Union(u) => Kind::Union {
            members: names(u.members),
        },
        TypeKind::Enum(e) => Kind::Enum {
            values: e
                .values
                .into_iter()
                .map(|v| v.node.value.node.to_string())
                .collect(),
        },
        TypeKind::InputObject(i) => Kind::Input {
            fields: i.fields.into_iter().map(input_value).collect(),
        },
    };
    TypeDef {
        description: text(t.description),
        kind,
    }
}

/// Merge an `extend` definition into its base type.
fn merge(name: &str, base: &mut TypeDef, ext: TypeDef) -> Result<(), ImportError> {
    match (&mut base.kind, ext.kind) {
        (Kind::Scalar, Kind::Scalar) => {}
        (
            Kind::Object { implements, fields },
            Kind::Object {
                implements: i,
                fields: f,
            },
        )
        | (
            Kind::Interface { implements, fields },
            Kind::Interface {
                implements: i,
                fields: f,
            },
        ) => {
            implements.extend(i);
            fields.extend(f);
        }
        (Kind::Union { members }, Kind::Union { members: m }) => members.extend(m),
        (Kind::Enum { values }, Kind::Enum { values: v }) => values.extend(v),
        (Kind::Input { fields }, Kind::Input { fields: f }) => fields.extend(f),
        _ => {
            return Err(invalid(
                format!("/types/{name}"),
                "extension kind does not match the extended type",
            ));
        }
    }
    Ok(())
}

fn parse_error(e: &async_graphql_parser::Error) -> ImportError {
    if let async_graphql_parser::Error::MissingQueryRoot { .. } = e {
        return invalid("/schema", "the schema has no query type");
    }
    let message = e.to_string();
    // Pest messages are multi-line; the `= ...` line carries the reason.
    let detail = message
        .lines()
        .rev()
        .find_map(|l| l.trim_start().strip_prefix("= "))
        .unwrap_or(message.trim());
    ImportError::Parse(match e.positions().next() {
        Some(p) => format!("line {}, column {}: {detail}", p.line, p.column),
        None => detail.to_owned(),
    })
}

fn build(sdl: &str) -> Result<Schema, ImportError> {
    let doc = parse_schema(sdl).map_err(|e| parse_error(&e))?;
    let mut types: BTreeMap<String, TypeDef> = BTreeMap::new();
    let mut extensions = Vec::new();
    let mut declared_roots: Option<[Option<String>; 3]> = None;
    for def in doc.definitions {
        match def {
            TypeSystemDefinition::Schema(s) => {
                let s = s.node;
                let named =
                    [s.query, s.mutation, s.subscription].map(|r| r.map(|n| n.node.to_string()));
                match &mut declared_roots {
                    Some(roots) => {
                        if !s.extend {
                            return Err(invalid("/schema", "the schema is defined more than once"));
                        }
                        for (slot, name) in roots.iter_mut().zip(named) {
                            if name.is_some() {
                                *slot = name;
                            }
                        }
                    }
                    None => declared_roots = Some(named),
                }
            }
            TypeSystemDefinition::Type(t) => {
                let t = t.node;
                let name = t.name.node.to_string();
                if is_builtin(&name) {
                    continue;
                }
                if t.extend {
                    extensions.push((name, convert(t)));
                } else if types.insert(name.clone(), convert(t)).is_some() {
                    return Err(invalid(
                        format!("/types/{name}"),
                        "the type is defined more than once",
                    ));
                }
            }
            TypeSystemDefinition::Directive(_) => {}
        }
    }
    for (name, ext) in extensions {
        let base = types
            .get_mut(&name)
            .ok_or_else(|| invalid(format!("/types/{name}"), "extension of an undefined type"))?;
        merge(&name, base, ext)?;
    }

    let roots = declared_roots.unwrap_or_else(|| {
        ROOTS.map(|(_, default)| {
            matches!(
                types.get(default).map(|t| &t.kind),
                Some(Kind::Object { .. })
            )
            .then(|| default.to_owned())
        })
    });
    let schema = Schema { types, roots };
    validate(&schema)?;
    Ok(schema)
}

fn named_type(ty: &Type) -> &str {
    match &ty.base {
        BaseType::Named(n) => n.as_str(),
        BaseType::List(inner) => named_type(inner),
    }
}

/// The first name that occurs more than once in `names`, if any.
fn first_duplicate<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let mut seen = std::collections::BTreeSet::new();
    names.into_iter().find(|n| !seen.insert(*n))
}

/// Reject a name defined more than once within one type (after `extend`
/// blocks are merged): SDL forbids it, and async-graphql's dynamic schema
/// panics on a duplicate field at serve time.
fn unique<'a>(
    names: impl IntoIterator<Item = &'a str>,
    at: impl Fn(&str) -> String,
    what: &str,
) -> Result<(), ImportError> {
    match first_duplicate(names) {
        Some(n) => Err(invalid(
            at(n),
            format!("the {what} is defined more than once"),
        )),
        None => Ok(()),
    }
}

fn validate_unique(name: &str, kind: &Kind) -> Result<(), ImportError> {
    match kind {
        Kind::Object { implements, fields } | Kind::Interface { implements, fields } => {
            unique(
                implements.iter().map(String::as_str),
                |i| format!("/types/{name}/interfaces/{i}"),
                "interface",
            )?;
            unique(
                fields.iter().map(|f| f.name.as_str()),
                |f| format!("/types/{name}/fields/{f}"),
                "field",
            )?;
            for f in fields {
                unique(
                    f.args.iter().map(|a| a.name.as_str()),
                    |a| format!("/types/{name}/fields/{}/arguments/{a}", f.name),
                    "argument",
                )?;
            }
            Ok(())
        }
        Kind::Input { fields } => unique(
            fields.iter().map(|f| f.name.as_str()),
            |f| format!("/types/{name}/fields/{f}"),
            "input field",
        ),
        Kind::Enum { values } => unique(
            values.iter().map(String::as_str),
            |v| format!("/types/{name}/values/{v}"),
            "enum value",
        ),
        Kind::Union { members } => unique(
            members.iter().map(String::as_str),
            |m| format!("/types/{name}/members/{m}"),
            "union member",
        ),
        Kind::Scalar => Ok(()),
    }
}

fn validate(schema: &Schema) -> Result<(), ImportError> {
    let types = &schema.types;
    let known = |ty: &Type| {
        let n = named_type(ty);
        BUILTIN_SCALARS.contains(&n) || types.contains_key(n)
    };
    let unknown = |owner: &str, field: &str, ty: &Type| {
        invalid(
            format!("/types/{owner}/fields/{field}"),
            format!("unknown type `{}`", named_type(ty)),
        )
    };
    let is_kind =
        |name: &str, want: fn(&Kind) -> bool| types.get(name).is_some_and(|t| want(&t.kind));
    let is_object = |k: &Kind| matches!(k, Kind::Object { .. });
    let is_interface = |k: &Kind| matches!(k, Kind::Interface { .. });

    match &schema.roots[0] {
        None => return Err(invalid("/schema", "the schema has no query type")),
        Some(q) if !is_kind(q, is_object) => {
            return Err(invalid(
                "/schema",
                format!("query type `{q}` is not a defined object type"),
            ));
        }
        Some(_) => {}
    }
    for ((_, label), root) in ROOTS.iter().zip(&schema.roots).skip(1) {
        if let Some(r) = root
            && !is_kind(r, is_object)
        {
            return Err(invalid(
                "/schema",
                format!(
                    "{} type `{r}` is not a defined object type",
                    label.to_lowercase()
                ),
            ));
        }
    }

    for (name, def) in types {
        validate_unique(name, &def.kind)?;
        match &def.kind {
            Kind::Object { implements, fields } | Kind::Interface { implements, fields } => {
                if let Some(i) = implements.iter().find(|i| !is_kind(i, is_interface)) {
                    return Err(invalid(
                        format!("/types/{name}/interfaces"),
                        format!("`{i}` is not a defined interface"),
                    ));
                }
                for f in fields {
                    if !known(&f.ty) {
                        return Err(unknown(name, &f.name, &f.ty));
                    }
                    if let Some(a) = f.args.iter().find(|a| !known(&a.ty)) {
                        return Err(unknown(name, &f.name, &a.ty));
                    }
                }
            }
            Kind::Input { fields } => {
                if let Some(f) = fields.iter().find(|f| !known(&f.ty)) {
                    return Err(unknown(name, &f.name, &f.ty));
                }
            }
            Kind::Union { members } => {
                if let Some(m) = members.iter().find(|m| !is_kind(m, is_object)) {
                    return Err(invalid(
                        format!("/types/{name}/members"),
                        format!("`{m}` is not a defined object type"),
                    ));
                }
            }
            Kind::Scalar | Kind::Enum { .. } => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JSON Schema
// ---------------------------------------------------------------------------

fn def_ref(name: &str) -> Value {
    json!({ "$ref": format!("#/$defs/{name}") })
}

fn type_schema(ty: &Type) -> Value {
    let inner = match &ty.base {
        BaseType::Named(n) => named_schema(n),
        BaseType::List(item) => json!({"type": "array", "items": type_schema(item)}),
    };
    nullable(inner, ty.nullable)
}

/// The schema of a field's value in a response. Like [`type_schema`], except
/// that list items which the GraphQL adapter cannot serve as null (items of
/// enum, object, interface, union or list type: async-graphql's dynamic
/// schema rejects a null there, e.g. "invalid item for enum") are required,
/// so the faker never generates such a null and validation repairs one.
fn output_type_schema(schema: &Schema, ty: &Type) -> Value {
    let inner = match &ty.base {
        BaseType::Named(n) => named_schema(n),
        BaseType::List(item) if item.nullable && !null_item_servable(schema, item) => {
            let required = Type {
                base: item.base.clone(),
                nullable: false,
            };
            json!({"type": "array", "items": output_type_schema(schema, &required)})
        }
        BaseType::List(item) => {
            json!({"type": "array", "items": output_type_schema(schema, item)})
        }
    };
    nullable(inner, ty.nullable)
}

/// Whether a null list item of type `item` can be served: only scalars.
fn null_item_servable(schema: &Schema, item: &Type) -> bool {
    match &item.base {
        BaseType::Named(n) => match schema.types.get(n.as_str()) {
            Some(def) => matches!(def.kind, Kind::Scalar),
            // Built-in scalars are not in `types`.
            None => true,
        },
        BaseType::List(_) => false,
    }
}

fn named_schema(name: &str) -> Value {
    match name {
        "Int" => json!({"type": "integer", "minimum": i32::MIN, "maximum": i32::MAX}),
        "Float" => json!({"type": "number"}),
        "String" | "ID" => json!({"type": "string"}),
        "Boolean" => json!({"type": "boolean"}),
        other => def_ref(other),
    }
}

fn nullable(inner: Value, nullable: bool) -> Value {
    if nullable {
        json!({"anyOf": [inner, {"type": "null"}]})
    } else {
        inner
    }
}

fn with_description(mut schema: Value, description: Option<&str>) -> Value {
    if let (Some(d), Value::Object(obj)) = (description, &mut schema) {
        obj.insert("description".into(), json!(d));
    }
    schema
}

/// `{type: object, properties, required, additionalProperties: false}`;
/// `required` is omitted when empty.
fn object_schema(properties: Map<String, Value>, required: Vec<Value>) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert("type".into(), json!("object"));
    obj.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        obj.insert("required".into(), Value::Array(required));
    }
    obj.insert("additionalProperties".into(), json!(false));
    obj
}

/// Input values (arguments or input fields) as an object schema; a value
/// is required when it is non-null and has no default.
fn input_values_schema(values: &[InputValue]) -> (Map<String, Value>, bool) {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for v in values {
        properties.insert(
            v.name.clone(),
            with_description(type_schema(&v.ty), v.description.as_deref()),
        );
        if !v.ty.nullable && !v.has_default {
            required.push(json!(v.name));
        }
    }
    let any_required = !required.is_empty();
    (object_schema(properties, required), any_required)
}

fn fields_schema(schema: &Schema, fields: &[Field], typename: Option<&str>) -> Map<String, Value> {
    let mut properties = Map::new();
    let mut required = Vec::new();
    if let Some(name) = typename {
        properties.insert("__typename".into(), json!({ "const": name }));
        required.push(json!("__typename"));
    }
    for f in fields {
        properties.insert(
            f.name.clone(),
            with_description(output_type_schema(schema, &f.ty), f.description.as_deref()),
        );
        if !f.ty.nullable {
            required.push(json!(f.name));
        }
    }
    object_schema(properties, required)
}

fn one_of(mut names: Vec<&str>) -> Map<String, Value> {
    names.sort_unstable();
    names.dedup();
    let mut obj = Map::new();
    obj.insert(
        "oneOf".into(),
        Value::Array(names.into_iter().map(def_ref).collect()),
    );
    obj
}

/// The object types implementing each interface, by interface name, in
/// type-name order.
fn implementors(schema: &Schema) -> BTreeMap<&str, Vec<&str>> {
    let mut out: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (name, def) in &schema.types {
        if let Kind::Object { implements, .. } = &def.kind {
            for interface in implements {
                out.entry(interface.as_str())
                    .or_default()
                    .push(name.as_str());
            }
        }
    }
    out
}

fn def_schema(
    schema: &Schema,
    implementors: &BTreeMap<&str, Vec<&str>>,
    name: &str,
    def: &TypeDef,
) -> Value {
    let mut obj = match &def.kind {
        Kind::Scalar => {
            return json!({ "description": format!("GraphQL scalar {name}") });
        }
        Kind::Object { fields, .. } => fields_schema(schema, fields, Some(name)),
        Kind::Interface { fields, .. } => match implementors.get(name) {
            Some(names) if !names.is_empty() => one_of(names.clone()),
            // No concrete type to pick from: describe the shared fields.
            _ => fields_schema(schema, fields, None),
        },
        Kind::Union { members } => one_of(members.iter().map(String::as_str).collect()),
        Kind::Enum { values } => {
            let mut obj = Map::new();
            obj.insert("type".into(), json!("string"));
            obj.insert("enum".into(), json!(values));
            obj
        }
        Kind::Input { fields } => input_values_schema(fields).0,
    };
    if let Some(d) = &def.description {
        obj.insert("description".into(), json!(d));
    }
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

fn operation(
    schema: &Schema,
    operation_type: GraphqlOperationType,
    label: &str,
    f: &Field,
) -> Operation {
    let kind = match operation_type {
        GraphqlOperationType::Query => OperationKind::Query,
        GraphqlOperationType::Mutation => {
            kind_from_name(&f.name, OperationKind::Command(Effect::Other))
        }
        GraphqlOperationType::Subscription => OperationKind::StreamServer,
    };
    let named = named_type(&f.ty);
    let object_fields = schema.object_fields(named);
    let resource_type = match object_fields {
        Some(_) => Some(named.to_lowercase()),
        None => resource_from_name(&f.name),
    };
    let returns_one = matches!(f.ty.base, BaseType::Named(_));
    let identity = object_fields
        .filter(|fields| returns_one && fields.iter().any(|x| x.name == "id"))
        .map(|_| {
            let resource_id = resource_type.as_ref().map(|r| format!("{r}id"));
            let input_field = f
                .args
                .iter()
                .find(|a| a.name == "id")
                .or_else(|| {
                    f.args.iter().find(|a| {
                        resource_id
                            .as_deref()
                            .is_some_and(|r| a.name.eq_ignore_ascii_case(r))
                    })
                })
                .map(|a| a.name.clone());
            Identity {
                path_param: None,
                field: "id".into(),
                input_field,
                path_field: None,
            }
        });
    let (body, body_required) = input_values_schema(&f.args);
    Operation {
        id: format!("{label}.{}", f.name),
        protocol: Protocol::Graphql,
        binding: Binding::Graphql {
            operation_type,
            field: f.name.clone(),
        },
        kind,
        summary: f.description.clone(),
        input: InputSchemas {
            path: None,
            query: None,
            headers: None,
            body: Some(Value::Object(body)),
            body_required,
        },
        outputs: vec![OutputVariant {
            status: StatusSelector::Code(200),
            content_type: Some("application/json".into()),
            schema: Some(output_type_schema(schema, &f.ty)),
            headers: Default::default(),
        }],
        resource_type,
        identity,
        semantic_hints: schema
            .types
            .get(named)
            .and_then(|t| t.description.clone())
            .into_iter()
            .collect(),
        security: Vec::new(),
    }
}

fn compile_text(sdl: &str, bytes: &[u8], title: Option<String>) -> Result<Bundle, ImportError> {
    let schema = build(sdl)?;
    let implementors = implementors(&schema);
    let defs: Map<String, Value> = schema
        .types
        .iter()
        .map(|(name, def)| (name.clone(), def_schema(&schema, &implementors, name, def)))
        .collect();
    let mut operations = Vec::new();
    for ((operation_type, label), root) in ROOTS.iter().zip(&schema.roots) {
        let Some(fields) = root.as_deref().and_then(|r| schema.object_fields(r)) else {
            continue;
        };
        operations.extend(
            fields
                .iter()
                .map(|f| operation(&schema, *operation_type, label, f)),
        );
    }
    let title = title.unwrap_or_else(|| DEFAULT_TITLE.to_owned());
    let digest = Sha256::digest(bytes);
    Ok(Bundle {
        id: slug(&title),
        title,
        version: "0".into(),
        source_digest: digest.iter().map(|b| format!("{b:02x}")).collect(),
        operations,
        defs,
        protocol_payload: Some(ProtocolPayload::Graphql {
            sdl: sdl.to_owned(),
        }),
        channels: Vec::new(),
        webhooks: Vec::new(),
    })
}

/// Compile GraphQL SDL.
pub(crate) fn compile_sdl(bytes: &[u8]) -> Result<Bundle, ImportError> {
    let sdl = std::str::from_utf8(bytes)
        .map_err(|e| ImportError::Parse(format!("SDL is not UTF-8: {e}")))?;
    compile_text(sdl, bytes, None)
}

/// Compile an introspection result (`{"__schema": ..}` or
/// `{"data": {"__schema": ..}}`) by rendering it to SDL.
pub(crate) fn compile_introspection(doc: &Value, bytes: &[u8]) -> Result<Bundle, ImportError> {
    let (sdl, description) = render_introspection(doc)?;
    compile_text(&sdl, bytes, description.filter(|d| !d.trim().is_empty()))
}

// ---------------------------------------------------------------------------
// Introspection -> SDL
// ---------------------------------------------------------------------------

/// A GraphQL string literal.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// The `name` of an introspection object, checked to be a GraphQL name.
fn name_of<'a>(v: &'a Value, at: &str) -> Result<&'a str, ImportError> {
    v.get("name")
        .and_then(Value::as_str)
        .filter(|n| is_name(n))
        .ok_or_else(|| invalid(at, "missing or invalid `name`"))
}

fn description_prefix(v: &Value, indent: &str) -> String {
    match v.get("description").and_then(Value::as_str) {
        Some(d) => format!("{indent}{}\n", quote(d)),
        None => String::new(),
    }
}

fn type_ref(v: &Value, at: &str) -> Result<String, ImportError> {
    let of = || {
        v.get("ofType")
            .filter(|t| t.is_object())
            .ok_or_else(|| invalid(at, "wrapping type without `ofType`"))
    };
    match v.get("kind").and_then(Value::as_str) {
        Some("NON_NULL") => Ok(format!("{}!", type_ref(of()?, at)?)),
        Some("LIST") => Ok(format!("[{}]", type_ref(of()?, at)?)),
        _ => name_of(v, at).map(str::to_owned),
    }
}

fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// `"desc" name: Type = default`, as used for arguments and input fields.
fn render_input_value(v: &Value, at: &str) -> Result<String, ImportError> {
    let mut out = String::new();
    if let Some(d) = v.get("description").and_then(Value::as_str) {
        out.push_str(&quote(d));
        out.push(' ');
    }
    out.push_str(name_of(v, at)?);
    out.push_str(": ");
    out.push_str(&type_ref(v.get("type").unwrap_or(&Value::Null), at)?);
    if let Some(default) = v.get("defaultValue").and_then(Value::as_str) {
        out.push_str(" = ");
        out.push_str(default);
    }
    Ok(out)
}

fn render_fields(t: &Value, at: &str) -> Result<String, ImportError> {
    let mut out = String::new();
    for (j, f) in list(t, "fields").iter().enumerate() {
        let at = format!("{at}/fields/{j}");
        out.push_str(&description_prefix(f, "  "));
        out.push_str("  ");
        out.push_str(name_of(f, &at)?);
        let args = list(f, "args")
            .iter()
            .enumerate()
            .map(|(k, a)| render_input_value(a, &format!("{at}/args/{k}")))
            .collect::<Result<Vec<_>, _>>()?;
        if !args.is_empty() {
            out.push_str(&format!("({})", args.join(", ")));
        }
        out.push_str(": ");
        out.push_str(&type_ref(f.get("type").unwrap_or(&Value::Null), &at)?);
        out.push('\n');
    }
    Ok(out)
}

fn render_implements(t: &Value, at: &str) -> Result<String, ImportError> {
    let names = list(t, "interfaces")
        .iter()
        .enumerate()
        .map(|(k, i)| name_of(i, &format!("{at}/interfaces/{k}")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(if names.is_empty() {
        String::new()
    } else {
        format!(" implements {}", names.join(" & "))
    })
}

fn block(body: &str) -> String {
    if body.is_empty() {
        String::new()
    } else {
        format!(" {{\n{body}}}")
    }
}

fn render_type(t: &Value, name: &str, at: &str) -> Result<String, ImportError> {
    let mut out = description_prefix(t, "");
    let kind = t.get("kind").and_then(Value::as_str).unwrap_or_default();
    let line = match kind {
        "SCALAR" => format!("scalar {name}"),
        "OBJECT" => format!(
            "type {name}{}{}",
            render_implements(t, at)?,
            block(&render_fields(t, at)?)
        ),
        "INTERFACE" => format!(
            "interface {name}{}{}",
            render_implements(t, at)?,
            block(&render_fields(t, at)?)
        ),
        "UNION" => {
            let members = list(t, "possibleTypes")
                .iter()
                .enumerate()
                .map(|(k, m)| name_of(m, &format!("{at}/possibleTypes/{k}")))
                .collect::<Result<Vec<_>, _>>()?;
            format!("union {name} = {}", members.join(" | "))
        }
        "ENUM" => {
            let mut body = String::new();
            for (k, v) in list(t, "enumValues").iter().enumerate() {
                body.push_str(&description_prefix(v, "  "));
                body.push_str(&format!(
                    "  {}\n",
                    name_of(v, &format!("{at}/enumValues/{k}"))?
                ));
            }
            format!("enum {name}{}", block(&body))
        }
        "INPUT_OBJECT" => {
            let mut body = String::new();
            for (k, v) in list(t, "inputFields").iter().enumerate() {
                body.push_str(&format!(
                    "  {}\n",
                    render_input_value(v, &format!("{at}/inputFields/{k}"))?
                ));
            }
            format!("input {name}{}", block(&body))
        }
        other => return Err(invalid(at, format!("unknown type kind `{other}`"))),
    };
    out.push_str(&line);
    out.push('\n');
    Ok(out)
}

/// Directive locations a query can use.
const EXECUTABLE_LOCATIONS: [&str; 8] = [
    "QUERY",
    "MUTATION",
    "SUBSCRIPTION",
    "FIELD",
    "FRAGMENT_DEFINITION",
    "FRAGMENT_SPREAD",
    "INLINE_FRAGMENT",
    "VARIABLE_DEFINITION",
];

/// Directives every GraphQL server knows.
const BUILTIN_DIRECTIVES: [&str; 5] = ["skip", "include", "deprecated", "specifiedBy", "oneOf"];

/// The custom directives of `__schema.directives` that a query can use, as
/// SDL definitions limited to their executable locations (the GraphQL
/// adapter accepts queries using them; type-system directives play no part
/// in serving).
fn render_directives(schema: &Value, base: &str) -> Result<String, ImportError> {
    let mut out = String::new();
    for (i, d) in list(schema, "directives").iter().enumerate() {
        let at = format!("{base}/directives/{i}");
        let name = name_of(d, &at)?;
        let locations: Vec<&str> = list(d, "locations")
            .iter()
            .filter_map(Value::as_str)
            .filter(|l| EXECUTABLE_LOCATIONS.contains(l))
            .collect();
        if BUILTIN_DIRECTIVES.contains(&name) || locations.is_empty() {
            continue;
        }
        let args = list(d, "args")
            .iter()
            .enumerate()
            .map(|(k, a)| render_input_value(a, &format!("{at}/args/{k}")))
            .collect::<Result<Vec<_>, _>>()?;
        out.push('\n');
        out.push_str(&description_prefix(d, ""));
        out.push_str("directive @");
        out.push_str(name);
        if !args.is_empty() {
            out.push_str(&format!("({})", args.join(", ")));
        }
        if d.get("isRepeatable").and_then(Value::as_bool) == Some(true) {
            out.push_str(" repeatable");
        }
        out.push_str(" on ");
        out.push_str(&locations.join(" | "));
        out.push('\n');
    }
    Ok(out)
}

/// Render an introspection result as SDL (types sorted by name, built-in
/// scalars and `__*` types omitted); also returns the schema description.
fn render_introspection(doc: &Value) -> Result<(String, Option<String>), ImportError> {
    let (base, schema) = match doc.get("__schema") {
        Some(s) => ("/__schema", s),
        None => (
            "/data/__schema",
            doc.pointer("/data/__schema")
                .ok_or_else(|| invalid("/__schema", "missing introspection schema"))?,
        ),
    };
    let types = schema
        .get("types")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(format!("{base}/types"), "expected an array"))?;
    let mut named = types
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let at = format!("{base}/types/{i}");
            name_of(t, &at).map(|n| (n, at, t))
        })
        .collect::<Result<Vec<_>, _>>()?;
    named.sort_by(|a, b| a.0.cmp(b.0));

    let mut roots = [None, None, None];
    for (slot, key) in roots
        .iter_mut()
        .zip(["queryType", "mutationType", "subscriptionType"])
    {
        if let Some(r) = schema.get(key).filter(|r| r.is_object()) {
            *slot = Some(name_of(r, &format!("{base}/{key}"))?);
        }
    }
    let defined = |n: &str| named.iter().any(|(t, _, _)| *t == n);
    let needs_schema_block = ROOTS
        .iter()
        .zip(&roots)
        .any(|((_, default), root)| root.map_or(defined(default), |r| r != *default));

    let mut out = String::new();
    if needs_schema_block {
        out.push_str("schema {\n");
        for ((_, label), root) in ROOTS.iter().zip(&roots) {
            if let Some(r) = root {
                out.push_str(&format!("  {}: {r}\n", label.to_lowercase()));
            }
        }
        out.push_str("}\n\n");
    }
    let rendered = named
        .iter()
        .filter(|(n, _, _)| !is_builtin(n))
        .map(|(n, at, t)| render_type(t, n, at))
        .collect::<Result<Vec<_>, _>>()?;
    out.push_str(&rendered.join("\n"));
    out.push_str(&render_directives(schema, base)?);
    let description = schema
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((out, description))
}
