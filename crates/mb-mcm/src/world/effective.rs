//! The effective world description: a document's entries that validate,
//! overlaid on a base (tier 0).
//!
//! This is the one path from a stored revision to the description anything
//! reads: the Admin API's effective view and the runtime both call it, so
//! they never disagree on what a revision means.

use crate::Bundle;

use super::{Refusal, Validation, World, overlay, validate};

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
#[must_use]
pub fn effective_validated(bundle: &Bundle, base: &World, doc: &World) -> (World, Validation) {
    let v = validate(doc, bundle);
    (overlay(base, &v.applied), v)
}
