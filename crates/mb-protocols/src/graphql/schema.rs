//! Builds the dynamic schema from the bundle's SDL.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use async_graphql::dynamic::{
    Enum, EnumItem, Field, FieldFuture, InputObject, InputValue, Interface, InterfaceField, Object,
    Scalar, Schema, SchemaBuilder, Subscription, TypeRef, Union,
};
use async_graphql::parser::Positioned;
use async_graphql::parser::types::{
    BaseType, EnumValueDefinition, FieldDefinition, InputValueDefinition, Type, TypeKind,
    TypeSystemDefinition,
};
use mb_mcm::{Binding, GraphqlOperationType, ProtocolPayload};

use super::GraphqlError;
use super::resolve::{self, Shape, Shapes};
use super::subscription;
use crate::MockRuntime;

const BUILTIN_SCALARS: [&str; 5] = ["Int", "Float", "String", "Boolean", "ID"];

/// One named type, with its `extend` definitions merged in.
struct Def {
    description: Option<String>,
    kind: Kind,
}

enum Kind {
    Scalar,
    Object {
        implements: Vec<String>,
        fields: Vec<FieldDefinition>,
    },
    Interface {
        implements: Vec<String>,
        fields: Vec<FieldDefinition>,
    },
    Union {
        members: Vec<String>,
    },
    Enum {
        values: Vec<EnumValueDefinition>,
    },
    Input {
        fields: Vec<InputValueDefinition>,
    },
}

impl Kind {
    fn shape(&self) -> Option<Shape> {
        match self {
            Kind::Scalar => Some(Shape::Scalar),
            Kind::Enum { .. } => Some(Shape::Enum),
            Kind::Object { .. } => Some(Shape::Object),
            Kind::Interface { .. } | Kind::Union { .. } => Some(Shape::Abstract),
            Kind::Input { .. } => None,
        }
    }
}

fn nodes<T>(list: Vec<Positioned<T>>) -> Vec<T> {
    list.into_iter().map(|p| p.node).collect()
}

fn names<N: ToString>(list: Vec<Positioned<N>>) -> Vec<String> {
    list.into_iter().map(|n| n.node.to_string()).collect()
}

fn convert(kind: TypeKind) -> Kind {
    match kind {
        TypeKind::Scalar => Kind::Scalar,
        TypeKind::Object(o) => Kind::Object {
            implements: names(o.implements),
            fields: nodes(o.fields),
        },
        TypeKind::Interface(i) => Kind::Interface {
            implements: names(i.implements),
            fields: nodes(i.fields),
        },
        TypeKind::Union(u) => Kind::Union {
            members: names(u.members),
        },
        TypeKind::Enum(e) => Kind::Enum {
            values: nodes(e.values),
        },
        TypeKind::InputObject(i) => Kind::Input {
            fields: nodes(i.fields),
        },
    }
}

fn merge(name: &str, base: &mut Kind, ext: Kind) -> Result<(), GraphqlError> {
    match (base, ext) {
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
            return Err(GraphqlError::Schema(format!(
                "extension of {name} does not match its kind"
            )));
        }
    }
    Ok(())
}

/// The parsed SDL: types by name and the query, mutation and subscription
/// root type names.
struct Sdl {
    types: BTreeMap<String, Def>,
    roots: [Option<String>; 3],
}

fn parse(sdl: &str) -> Result<Sdl, GraphqlError> {
    let doc = async_graphql::parser::parse_schema(sdl)
        .map_err(|e| GraphqlError::Schema(e.to_string()))?;
    let mut types: BTreeMap<String, Def> = BTreeMap::new();
    let mut extensions = Vec::new();
    let mut declared: Option<[Option<String>; 3]> = None;
    for def in doc.definitions {
        match def {
            TypeSystemDefinition::Schema(s) => {
                let s = s.node;
                let named =
                    [s.query, s.mutation, s.subscription].map(|r| r.map(|n| n.node.to_string()));
                let roots = declared.get_or_insert_with(Default::default);
                for (slot, name) in roots.iter_mut().zip(named) {
                    if name.is_some() {
                        *slot = name;
                    }
                }
            }
            TypeSystemDefinition::Type(t) => {
                let t = t.node;
                let name = t.name.node.to_string();
                if BUILTIN_SCALARS.contains(&name.as_str()) || name.starts_with("__") {
                    continue;
                }
                if t.extend {
                    extensions.push((name, convert(t.kind)));
                } else {
                    let def = Def {
                        description: t.description.map(|d| d.node),
                        kind: convert(t.kind),
                    };
                    types.insert(name, def);
                }
            }
            TypeSystemDefinition::Directive(_) => {}
        }
    }
    for (name, ext) in extensions {
        let base = types
            .get_mut(&name)
            .ok_or_else(|| GraphqlError::Schema(format!("extension of undefined type {name}")))?;
        merge(&name, &mut base.kind, ext)?;
    }
    let roots = declared.unwrap_or_else(|| {
        ["Query", "Mutation", "Subscription"].map(|default| {
            matches!(
                types.get(default).map(|d| &d.kind),
                Some(Kind::Object { .. })
            )
            .then(|| default.to_string())
        })
    });
    Ok(Sdl { types, roots })
}

pub(super) fn type_ref(ty: &Type) -> TypeRef {
    let inner = match &ty.base {
        BaseType::Named(n) => TypeRef::Named(n.to_string().into()),
        BaseType::List(item) => TypeRef::List(Box::new(type_ref(item))),
    };
    if ty.nullable {
        inner
    } else {
        TypeRef::NonNull(Box::new(inner))
    }
}

/// `ty` without its outer non-null.
fn nullable(ty: &Type) -> Type {
    Type {
        base: ty.base.clone(),
        nullable: true,
    }
}

/// How the schema is registered where async-graphql 7 would reject a valid
/// interface implementation.
///
/// async-graphql checks field covariance the wrong way round (it accepts
/// `String` for an interface's `String!` and rejects `String!` for
/// `String`) and compares named types by equality. After validating with
/// the spec's rules ([`valid_implementation`]):
/// - a field narrowing only nullability (`String!` for `String`) is
///   registered with its interfaces' type (nullable wherever one of them
///   has it nullable), so every query still validates;
/// - a field narrowing a named type (`User` for an interface's `Node`)
///   keeps its declared type and the interface is registered without that
///   field, so queries on the object type keep working.
#[derive(Default)]
struct Adjusted {
    /// Registered field types, by `(type name, field name)`.
    types: HashMap<(String, String), Type>,
    /// Interface fields left out, as `(interface name, field name)`.
    dropped: HashSet<(String, String)>,
}

impl Adjusted {
    fn field_type(&self, ty: &str, field: &str) -> Option<&Type> {
        self.types.get(&(ty.to_string(), field.to_string()))
    }

    fn is_dropped(&self, ty: &str, field: &str) -> bool {
        self.dropped.contains(&(ty.to_string(), field.to_string()))
    }
}

/// Validates every interface implementation and works out its [`Adjusted`]
/// registration.
fn implementation_types(sdl: &Sdl) -> Result<Adjusted, GraphqlError> {
    let mut out = Adjusted::default();
    for (name, def) in &sdl.types {
        let (Kind::Object { implements, fields } | Kind::Interface { implements, fields }) =
            &def.kind
        else {
            continue;
        };
        for f in fields {
            let fname = f.name.node.as_str();
            let declared = &f.ty.node;
            let mut implemented = Vec::new();
            for iface in implements {
                let Some(Kind::Interface {
                    fields: ifields, ..
                }) = sdl.types.get(iface).map(|d| &d.kind)
                else {
                    continue; // async-graphql reports a non-interface
                };
                let Some(ifield) = ifields.iter().find(|i| i.name.node == fname) else {
                    continue;
                };
                let ity = &ifield.ty.node;
                if !valid_implementation(sdl, declared, ity) {
                    return Err(GraphqlError::Schema(format!(
                        "field \"{name}.{fname}\" of type {declared} does not implement \"{iface}.{fname}\" of type {ity}"
                    )));
                }
                if !library_accepts(declared, ity) {
                    implemented.push((iface, ity));
                }
            }
            if implemented.is_empty() {
                continue;
            }
            let widened = implemented
                .iter()
                .try_fold(declared.clone(), |w, (_, ity)| meet(&w, ity));
            match widened {
                Some(w) => {
                    out.types.insert((name.clone(), fname.to_string()), w);
                }
                None => {
                    for (iface, _) in implemented {
                        tracing::warn!(
                            field = %format!("{name}.{fname}"),
                            interface = %iface,
                            "covariant interface field registered on the implementing type only"
                        );
                        out.dropped.insert((iface.clone(), fname.to_string()));
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The spec's `IsValidImplementationFieldType(field, implemented)`.
fn valid_implementation(sdl: &Sdl, field: &Type, implemented: &Type) -> bool {
    if !field.nullable {
        return valid_implementation(sdl, &nullable(field), &nullable(implemented));
    }
    if !implemented.nullable {
        return false;
    }
    match (&field.base, &implemented.base) {
        (BaseType::List(f), BaseType::List(i)) => valid_implementation(sdl, f, i),
        (BaseType::Named(f), BaseType::Named(i)) => {
            f == i
                || match sdl.types.get(i.as_str()).map(|d| &d.kind) {
                    Some(Kind::Union { members }) => members.iter().any(|m| m == f.as_str()),
                    Some(Kind::Interface { .. }) => {
                        match sdl.types.get(f.as_str()).map(|d| &d.kind) {
                            Some(
                                Kind::Object { implements, .. }
                                | Kind::Interface { implements, .. },
                            ) => implements.iter().any(|m| m == i.as_str()),
                            _ => false,
                        }
                    }
                    _ => false,
                }
        }
        _ => false,
    }
}

/// async-graphql 7's own check of `field` implementing `implemented`.
fn library_accepts(field: &Type, implemented: &Type) -> bool {
    match (field.nullable, implemented.nullable) {
        (false, false) => library_accepts(&nullable(field), &nullable(implemented)),
        (true, false) => library_accepts(field, &nullable(implemented)),
        (false, true) => false,
        (true, true) => match (&field.base, &implemented.base) {
            (BaseType::Named(f), BaseType::Named(i)) => f == i,
            (BaseType::List(f), BaseType::List(i)) => library_accepts(f, i),
            _ => false,
        },
    }
}

/// The type nullable wherever `a` or `b` is, when both have the same list
/// structure and named type.
fn meet(a: &Type, b: &Type) -> Option<Type> {
    let base = match (&a.base, &b.base) {
        (BaseType::Named(x), BaseType::Named(y)) if x == y => a.base.clone(),
        (BaseType::List(x), BaseType::List(y)) => BaseType::List(Box::new(meet(x, y)?)),
        _ => return None,
    };
    Some(Type {
        base,
        nullable: a.nullable || b.nullable,
    })
}

pub(super) fn input_value(v: &InputValueDefinition) -> InputValue {
    let mut out = InputValue::new(v.name.node.to_string(), type_ref(&v.ty.node));
    if let Some(d) = &v.description {
        out = out.description(d.node.clone());
    }
    if let Some(default) = &v.default_value {
        out = out.default_value(default.node.clone());
    }
    out
}

/// A dynamic schema answering `rt.bundle`'s queries, mutations and
/// subscriptions, with
/// the depth, complexity and introspection settings of `rt.protocols`.
pub(crate) fn build_schema(rt: &Arc<MockRuntime>) -> Result<Schema, GraphqlError> {
    let Some(ProtocolPayload::Graphql { sdl }) = &rt.bundle.protocol_payload else {
        return Err(GraphqlError::MissingSdl);
    };
    let sdl = parse(sdl)?;
    let [Some(query), mutation, subscription] = &sdl.roots else {
        return Err(GraphqlError::Schema("the schema has no query type".into()));
    };
    let shapes: Arc<Shapes> = Arc::new(
        sdl.types
            .iter()
            .filter_map(|(name, def)| Some((name.clone(), def.kind.shape()?)))
            .collect(),
    );
    // Root fields → operation id; other types have none.
    let no_ops: HashMap<&str, &str> = HashMap::new();
    let mut ops_by_root: [HashMap<&str, &str>; 3] = Default::default();
    for op in &rt.bundle.operations {
        if let Binding::Graphql {
            operation_type,
            field,
        } = &op.binding
        {
            let slot = match operation_type {
                GraphqlOperationType::Query => 0,
                GraphqlOperationType::Mutation => 1,
                GraphqlOperationType::Subscription => 2,
            };
            ops_by_root[slot].insert(field.as_str(), op.id.as_str());
        }
    }

    let adjusted = implementation_types(&sdl)?;
    let mut builder = Schema::build(query, mutation.as_deref(), subscription.as_deref());
    for (name, def) in &sdl.types {
        if subscription.as_deref() == Some(name.as_str()) {
            builder = register_subscription(builder, rt, name, def, &ops_by_root[2], &shapes)?;
            continue;
        }
        let root_ops = if name == query {
            &ops_by_root[0]
        } else if mutation.as_deref() == Some(name.as_str()) {
            &ops_by_root[1]
        } else {
            &no_ops
        };
        builder = register(builder, rt, name, def, root_ops, &shapes, &adjusted);
    }
    let p = &rt.protocols;
    builder = builder
        .limit_depth(p.graphql_max_depth)
        .limit_complexity(p.graphql_max_complexity);
    if !p.graphql_introspection {
        builder = builder.disable_introspection();
    }
    builder
        .finish()
        .map_err(|e| GraphqlError::Schema(e.to_string()))
}

/// The subscription root type: one streaming field per object field.
fn register_subscription(
    builder: SchemaBuilder,
    rt: &Arc<MockRuntime>,
    name: &str,
    def: &Def,
    root_ops: &HashMap<&str, &str>,
    shapes: &Arc<Shapes>,
) -> Result<SchemaBuilder, GraphqlError> {
    let Kind::Object { fields, .. } = &def.kind else {
        return Err(GraphqlError::Schema(format!(
            "subscription type {name} is not an object type"
        )));
    };
    let mut s = Subscription::new(name);
    if let Some(d) = &def.description {
        s = s.description(d.clone());
    }
    for f in fields {
        let op_id = root_ops.get(f.name.node.as_str()).copied();
        s = s.field(subscription::field(rt, f, op_id, shapes));
    }
    Ok(builder.register(s))
}

fn register(
    builder: SchemaBuilder,
    rt: &Arc<MockRuntime>,
    name: &str,
    def: &Def,
    root_ops: &HashMap<&str, &str>,
    shapes: &Arc<Shapes>,
    adjusted: &Adjusted,
) -> SchemaBuilder {
    let description = def.description.clone();
    match &def.kind {
        Kind::Scalar => {
            let mut s = Scalar::new(name);
            if let Some(d) = description {
                s = s.description(d);
            }
            builder.register(s)
        }
        Kind::Object { implements, fields } => {
            let mut o = Object::new(name);
            if let Some(d) = description {
                o = o.description(d);
            }
            for i in implements {
                o = o.implement(i);
            }
            for f in fields {
                let op_id = root_ops.get(f.name.node.as_str()).copied();
                let as_type = adjusted.field_type(name, &f.name.node);
                o = o.field(field(rt, f, op_id, shapes, as_type));
            }
            builder.register(o)
        }
        Kind::Interface { implements, fields } => {
            let mut i = Interface::new(name);
            if let Some(d) = description {
                i = i.description(d);
            }
            for parent in implements {
                i = i.implement(parent);
            }
            for f in fields {
                if adjusted.is_dropped(name, &f.name.node) {
                    continue;
                }
                let ty = adjusted
                    .field_type(name, &f.name.node)
                    .unwrap_or(&f.ty.node);
                let mut field = InterfaceField::new(f.name.node.to_string(), type_ref(ty));
                if let Some(d) = &f.description {
                    field = field.description(d.node.clone());
                }
                for a in &f.arguments {
                    field = field.argument(input_value(&a.node));
                }
                i = i.field(field);
            }
            builder.register(i)
        }
        Kind::Union { members } => {
            let mut u = Union::new(name);
            if let Some(d) = description {
                u = u.description(d);
            }
            for m in members {
                u = u.possible_type(m);
            }
            builder.register(u)
        }
        Kind::Enum { values } => {
            let mut e = Enum::new(name);
            if let Some(d) = description {
                e = e.description(d);
            }
            for v in values {
                let mut item = EnumItem::new(v.value.node.to_string());
                if let Some(d) = &v.description {
                    item = item.description(d.node.clone());
                }
                e = e.item(item);
            }
            builder.register(e)
        }
        Kind::Input { fields } => {
            let mut o = InputObject::new(name);
            if let Some(d) = description {
                o = o.description(d);
            }
            for f in fields {
                o = o.field(input_value(f));
            }
            builder.register(o)
        }
    }
}

/// An object field: a root field when `op_id` names its operation, else a
/// projection from the parent value. `as_type` is the type it is registered
/// with when that differs from the declared one (see [`Adjusted`]).
fn field(
    rt: &Arc<MockRuntime>,
    f: &FieldDefinition,
    op_id: Option<&str>,
    shapes: &Arc<Shapes>,
    as_type: Option<&Type>,
) -> Field {
    let name = f.name.node.to_string();
    let ty = f.ty.node.clone();
    let registered = type_ref(as_type.unwrap_or(&ty));
    let shapes = Arc::clone(shapes);
    let mut out = match op_id {
        Some(op_id) => {
            let rt = Arc::clone(rt);
            let op_id = op_id.to_string();
            Field::new(name, registered, move |ctx| {
                let rt = Arc::clone(&rt);
                let op_id = op_id.clone();
                let ty = ty.clone();
                let shapes = Arc::clone(&shapes);
                FieldFuture::new(
                    async move { resolve::root(&rt, &op_id, &ty, &shapes, &ctx).await },
                )
            })
        }
        None => Field::new(name, registered, move |ctx| {
            let out = resolve::nested(&ty, &shapes, &ctx);
            FieldFuture::new(async move { out })
        }),
    };
    if let Some(d) = &f.description {
        out = out.description(d.node.clone());
    }
    for a in &f.arguments {
        out = out.argument(input_value(&a.node));
    }
    out
}
