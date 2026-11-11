//! The effective world description: a document's entries that validate,
//! overlaid on a base (tier 0).
//!
//! This is the one path from a stored revision to the description anything
//! reads: the Admin API's effective view and the runtime both call it, so
//! they never disagree on what a revision means.

use std::collections::BTreeSet;

use crate::Bundle;

use super::{OpKind, Refusal, RefusalCode, TypeDesc, Validation, World, overlay, validate};

/// The effective world of `doc` over `base` (`tier0(bundle)`, passed in so a
/// caller computes it once): `doc`'s entries that validate against `bundle`,
/// overlaid on `base`, and the entries refused, which fall back to `base`'s.
#[must_use]
pub fn effective(bundle: &Bundle, base: &World, doc: &World) -> (World, Vec<Refusal>) {
    let (world, v) = effective_validated(bundle, base, doc);
    (world, v.refused)
}

/// [`effective`], with the whole [`Validation`] of `doc`: for a caller that
/// also needs the entries that applied (the Admin API attributes each to its
/// author).
///
/// An identity left with no `member` after the overlay (the revision set
/// `pattern` or `alternates` on a type `base` names no member for) is dropped
/// from the effective world and from `applied`, and refused `invalid` at
/// `/types/<t>/identity/member`; the rest of the revision applies. So an
/// effective world never holds a member-less identity.
///
/// A revision that makes an operation a create without naming a target keeps
/// `base`'s target except a path parameter the route does not name the new
/// record by: a `POST`'s, which names the collection or parent it posts to.
/// A `PUT` whose last path segment is that parameter keeps it, its target URI
/// being the record (see [`Operation::route_names_record`]).
///
/// [`Operation::route_names_record`]: crate::Operation::route_names_record
#[must_use]
pub fn effective_validated(bundle: &Bundle, base: &World, doc: &World) -> (World, Validation) {
    let mut v = validate(doc, bundle);
    let mut world = overlay(base, &v.applied);
    drop_parent_targets(bundle, base, &v.applied, &mut world);
    let memberless: Vec<String> = world
        .types
        .iter()
        .filter(|(_, t)| t.identity.as_ref().is_some_and(|i| i.member.is_none()))
        .map(|(name, _)| name.clone())
        .collect();
    for name in memberless {
        drop_identity(&mut world, &name);
        drop_identity(&mut v.applied, &name);
        let path = format!(
            "/types/{}/identity/member",
            name.replace('~', "~0").replace('/', "~1")
        );
        // A member the validator refused is already listed there.
        if !v.refused.iter().any(|r| r.path == path) {
            v.refused.push(Refusal {
                path,
                code: RefusalCode::Invalid,
                message: format!("tier 0 names no member for type `{name}`; set identity.member"),
            });
        }
    }
    (world, v)
}

/// The operations whose effective target is a path parameter the description writes,
/// not tier 0's guess: each operation of `applied` (a revision's entries that apply,
/// as [`effective_validated`] answers them) whose target names the path parameter
/// `effective`, the effective world, keeps.
#[must_use]
pub fn written_path_targets(applied: &World, effective: &World) -> BTreeSet<String> {
    applied
        .operations
        .iter()
        .filter_map(|(id, desc)| {
            let param = desc.target.as_ref()?.path_param.as_deref()?;
            let kept = effective
                .operations
                .get(id)?
                .target
                .as_ref()?
                .path_param
                .as_deref();
            (kept == Some(param)).then(|| id.clone())
        })
        .collect()
}

/// Drops from `world` the path parameter of each operation `applied` makes a
/// create without a target, where `base` took it for another kind's target
/// and the route does not name the new record by it.
fn drop_parent_targets(bundle: &Bundle, base: &World, applied: &World, world: &mut World) {
    for (id, top) in &applied.operations {
        let made_create = top.kind == Some(OpKind::Create)
            && base.operations.get(id).and_then(|b| b.kind) != Some(OpKind::Create);
        if !made_create || top.target.is_some() {
            continue;
        }
        let Some(desc) = world.operations.get_mut(id) else {
            continue;
        };
        let Some(target) = desc.target.as_mut() else {
            continue;
        };
        let names_record = |p: &str| {
            bundle
                .operation(id)
                .is_some_and(|op| op.route_names_record(p))
        };
        if target
            .path_param
            .as_deref()
            .is_some_and(|p| !names_record(p))
        {
            target.path_param = None;
            if *target == super::TargetDesc::default() {
                desc.target = None;
            }
        }
    }
}

/// Removes type `name`'s identity from `world`, and the type with it when
/// that leaves it empty.
fn drop_identity(world: &mut World, name: &str) {
    if let Some(t) = world.types.get_mut(name) {
        t.identity = None;
        if *t == TypeDesc::default() {
            world.types.remove(name);
        }
    }
}
