//! Builds the dynamic schema from the bundle's SDL.

use std::collections::{BTreeMap, HashMap};
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
        builder = register(builder, rt, name, def, root_ops, &shapes);
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
                o = o.field(field(rt, f, op_id, shapes));
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
                let mut field = InterfaceField::new(f.name.node.to_string(), type_ref(&f.ty.node));
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
/// projection from the parent value.
fn field(
    rt: &Arc<MockRuntime>,
    f: &FieldDefinition,
    op_id: Option<&str>,
    shapes: &Arc<Shapes>,
) -> Field {
    let name = f.name.node.to_string();
    let ty = f.ty.node.clone();
    let shapes = Arc::clone(shapes);
    let mut out = match op_id {
        Some(op_id) => {
            let rt = Arc::clone(rt);
            let op_id = op_id.to_string();
            Field::new(name, type_ref(&ty), move |ctx| {
                let rt = Arc::clone(&rt);
                let op_id = op_id.clone();
                let ty = ty.clone();
                let shapes = Arc::clone(&shapes);
                FieldFuture::new(
                    async move { resolve::root(&rt, &op_id, &ty, &shapes, &ctx).await },
                )
            })
        }
        None => {
            let field_name = name.clone();
            Field::new(name, type_ref(&ty), move |ctx| {
                let out = resolve::nested(&field_name, &ty, &shapes, &ctx);
                FieldFuture::new(async move { out })
            })
        }
    };
    if let Some(d) = &f.description {
        out = out.description(d.node.clone());
    }
    for a in &f.arguments {
        out = out.argument(input_value(&a.node));
    }
    out
}
