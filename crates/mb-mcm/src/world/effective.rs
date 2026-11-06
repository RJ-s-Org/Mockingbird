//! The effective world description: a document's entries that validate,
//! overlaid on a base (tier 0).
//!
//! This is the one path from a stored revision to the description anything
//! reads: the Admin API's effective view and the runtime both call it, so
//! they never disagree on what a revision means.

use crate::Bundle;

use super::{Refusal, RefusalCode, TypeDesc, Validation, World, overlay, validate};

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
#[must_use]
pub fn effective_validated(bundle: &Bundle, base: &World, doc: &World) -> (World, Validation) {
    let mut v = validate(doc, bundle);
    let mut world = overlay(base, &v.applied);
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
