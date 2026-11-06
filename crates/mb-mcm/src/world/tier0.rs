//! The tier-0 world description of a bundle.
//!
//! [`tier0`] restates what the import heuristics already put in a [`Bundle`]
//! as a [`World`]: each operation's kind and resource type, where its request
//! names the record, which member names a record of its type, and where its
//! answer holds the record. It infers nothing new; an assistant, the LLM or
//! a person overlays it entry by entry
//! ([`overlay`](super::overlay)).

use std::collections::BTreeMap;

use super::{
    AnswerDesc, Extra, IdentityDesc, OpDesc, OpKind, Pointer, TargetDesc, TypeDesc, World, validate,
};
use crate::{Bundle, RecordAt};

/// The tier-0 world description of `bundle`: only what the bundle already
/// holds.
///
/// - an operation's `kind` from its [`OperationKind`](crate::OperationKind)
///   ([`OpKind::of`]), and its `type` from `resource_type`;
/// - from its [`Identity`](crate::Identity): `target.path_param` (the path
///   parameter) and `target.member` (the request body field non-HTTP
///   operations name the record by), the type's `identity.member` (the
///   record's id field: of the fields the type's operations name, its
///   reads' first, the first that is a member of its record), and
///   `answer.record` where the answer holds the record
///   ([`record_pointer`]).
///
/// An operation the bundle says nothing about gets no entry.
#[must_use]
pub fn tier0(bundle: &Bundle) -> World {
    let mut world = World::default();
    for op in &bundle.operations {
        let mut desc = OpDesc {
            kind: OpKind::of(op),
            r#type: op.resource_type.clone(),
            ..OpDesc::default()
        };
        if let Some(identity) = &op.identity {
            if identity.path_param.is_some() || identity.input_field.is_some() {
                desc.target = Some(TargetDesc {
                    path_param: identity.path_param.clone(),
                    member: identity.input_field.clone(),
                    extra: Extra::new(),
                });
            }
            if let Some(record) = identity.record.as_ref().and_then(record_pointer) {
                desc.answer = Some(AnswerDesc {
                    record: Some(record),
                    ..AnswerDesc::default()
                });
            }
        }
        if desc != OpDesc::default() {
            world.operations.insert(op.id.clone(), desc);
        }
    }
    for (t, member) in identities(bundle, &world) {
        world.types.insert(
            t,
            TypeDesc {
                identity: Some(IdentityDesc {
                    member,
                    ..IdentityDesc::default()
                }),
                ..TypeDesc::default()
            },
        );
    }
    world
}

/// The identity member of each type: of the id fields its operations'
/// identities name (its reads' first, then in contract order), the first
/// that is a member of the type's record. A type none of whose fields is
/// gets none.
fn identities(bundle: &Bundle, ops: &World) -> BTreeMap<String, String> {
    let mut candidates: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let reads_first = bundle
        .operations
        .iter()
        .filter(|o| OpKind::of(o) == Some(OpKind::Read))
        .chain(
            bundle
                .operations
                .iter()
                .filter(|o| OpKind::of(o) != Some(OpKind::Read)),
        );
    for op in reads_first {
        if let (Some(t), Some(identity)) = (op.resource_type.as_deref(), &op.identity) {
            let fields = candidates.entry(t).or_default();
            if !fields.contains(&identity.field.as_str()) {
                fields.push(&identity.field);
            }
        }
    }
    let mut out = BTreeMap::new();
    for (t, fields) in candidates {
        let found = fields.into_iter().find(|field| {
            // Only the type and its operations: the answer pointers of its
            // operations say where its record is.
            let operations = ops
                .operations
                .iter()
                .filter(|(_, d)| d.r#type.as_deref() == Some(t))
                .map(|(id, d)| (id.clone(), d.clone()))
                .collect();
            let identity = TypeDesc {
                identity: Some(IdentityDesc {
                    member: (*field).to_owned(),
                    ..IdentityDesc::default()
                }),
                ..TypeDesc::default()
            };
            let probe = World {
                types: BTreeMap::from([(t.to_owned(), identity)]),
                operations,
                ..World::default()
            };
            let under = format!("/types/{}/", t.replace('~', "~0").replace('/', "~1"));
            !validate(&probe, bundle)
                .refused
                .iter()
                .any(|r| r.path.starts_with(&under))
        });
        if let Some(field) = found {
            out.insert(t.to_owned(), field.to_owned());
        }
    }
    out
}

/// The answer pointer of a bundle's record location: [`RecordAt::Member`] `m`
/// → `/m`, [`RecordAt::Item`] → `/0`, [`RecordAt::MemberItem`] `m` → `/m/0`,
/// and a [`RecordAt::Pointer`] its tokens, each RFC 6901 escaped. The inverse
/// of [`record_at`](super::record_at). `None` only for a pointer with no
/// tokens, which no record location is (the answer itself is no `RecordAt`).
#[must_use]
pub fn record_pointer(at: &RecordAt) -> Option<Pointer> {
    let token = |m: &str| m.replace('~', "~0").replace('/', "~1");
    let pointer = match at {
        RecordAt::Member(m) => format!("/{}", token(m)),
        RecordAt::Item => "/0".to_owned(),
        RecordAt::MemberItem(m) => format!("/{}/0", token(m)),
        RecordAt::Pointer(tokens) => tokens.iter().map(|t| format!("/{}", token(t))).collect(),
    };
    Pointer::parse(&pointer).ok()
}
