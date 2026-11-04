//! World descriptions of contract versions: the effective description and
//! carrying a description forward to a new version.
//!
//! The store keeps only the authored overlay, as numbered revisions per
//! contract version. [`effective_world`] computes the description a version
//! has: its tier-0 description ([`tier0`]) overlaid with the newest
//! revision's entries that validate against the version's bundle, the
//! author of each overlaid entry, what was refused, and the gaps.
//! [`carry_forward`] starts a new version from an earlier one's newest
//! overlay, keeping each entry's author; [`carry_forward_into`] finds that
//! earlier version and is called right after a contract version is added
//! that serves or may serve (uploaded, reconciled from the config file, or
//! a draft when it is approved).

use std::collections::{BTreeMap, BTreeSet};

use futures::{StreamExt, TryStreamExt, stream};

use mb_import::world::tier0;
use mb_mcm::Bundle;
use mb_mcm::world::{Author, OpDesc, Refusal, RefusalCode, TypeDesc, World, overlay, validate};
use mb_protocols::world_gaps::{Gap, gaps};
use mb_store::{ContractStatus, ControlStore, NewWorldRevision, StoreError, WorldRevision};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// How many earlier revisions [`effective_world`] reads, at most, to find
/// which revision last set an entry. An entry unchanged for longer is
/// attributed to the oldest revision read.
const MAX_ATTRIBUTION_REVISIONS: i64 = 64;

/// How many earlier contract versions [`carry_forward_into`] looks through,
/// newest first, for one with a revision.
const MAX_CARRY_VERSIONS: usize = 32;

/// How many of those versions [`carry_forward_into`] reads at once.
const CARRY_LOOKAHEAD: usize = 4;

/// The world description a contract version has.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct EffectiveWorld {
    /// Tier 0 overlaid with the newest revision's entries that validate.
    pub world: World,
    /// The author of each entry the overlay sets, keyed by its pointer at
    /// the granularity of `/types/{t}/{field}` and `/operations/{op}/{field}`
    /// (RFC 6901 escaped): the author of the newest revision that set it to
    /// its current value. Every entry not listed is `heuristic`.
    pub authors: BTreeMap<String, Author>,
    /// The newest revision's number; `0` when there is none.
    pub revision: i64,
    /// The newest revision's entries that did not validate, which fall back
    /// to tier 0.
    pub refused: Vec<Refusal>,
    /// What the description leaves open, including each refused entry.
    pub gaps: Vec<Gap>,
}

/// The effective world description of `mock_id`'s contract `version`, whose
/// bundle is `bundle`.
///
/// The newest revision's document is validated against `bundle` before it
/// is laid over tier 0, so a stale or refused entry falls back to tier 0's
/// value instead of removing it. A document that is not a world description
/// at all is refused whole (an empty path) and tier 0 applies. With no
/// revision it is tier 0 at revision `0`. It fails only when the store does.
pub async fn effective_world(
    store: &dyn ControlStore,
    mock_id: Uuid,
    version: i64,
    bundle: &Bundle,
) -> Result<EffectiveWorld, StoreError> {
    let base = tier0(bundle);
    let Some(latest) = store.latest_world_revision(mock_id, version).await? else {
        let gaps = gaps(bundle, &base, &[]);
        return Ok(EffectiveWorld {
            world: base,
            authors: BTreeMap::new(),
            revision: 0,
            refused: Vec::new(),
            gaps,
        });
    };
    let (applied, refused) = match serde_json::from_value::<World>(latest.document.clone()) {
        Ok(doc) => {
            let v = validate(&doc, bundle);
            (v.applied, v.refused)
        }
        Err(e) => (
            World::default(),
            vec![Refusal {
                path: String::new(),
                code: RefusalCode::Invalid,
                message: format!(
                    "revision {} is not a world description: {e}",
                    latest.revision
                ),
            }],
        ),
    };
    let authors = attribute(store, &latest, &applied)
        .await?
        .into_iter()
        .map(|(pointer, origin)| (pointer, origin.author))
        .collect();
    let world = overlay(&base, &applied);
    let gaps = gaps(bundle, &world, &refused);
    Ok(EffectiveWorld {
        world,
        authors,
        revision: latest.revision,
        refused,
        gaps,
    })
}

/// The pointer of each entry `doc` sets, with its value: each field of each
/// type and operation.
fn entries(doc: &Value) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for section in ["types", "operations"] {
        let Some(items) = doc.get(section).and_then(Value::as_object) else {
            continue;
        };
        for (name, desc) in items {
            let Some(fields) = desc.as_object() else {
                continue;
            };
            for (field, value) in fields {
                out.insert(
                    format!("/{section}/{}/{}", escape(name), escape(field)),
                    value.clone(),
                );
            }
        }
    }
    out
}

/// An RFC 6901 reference token for `s`.
fn escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// The revision that set an entry to its current value.
#[derive(Debug, Clone)]
struct Origin {
    author: Author,
    revision: i64,
    created_by: Option<String>,
}

impl Origin {
    fn of(r: &WorldRevision) -> Self {
        Self {
            author: r.author,
            revision: r.revision,
            created_by: r.created_by.clone(),
        }
    }
}

/// The origin of each entry of `applied` (the validated part of `latest`):
/// walking back from `latest`, the newest revision in the run of revisions
/// that hold the entry's current value.
async fn attribute(
    store: &dyn ControlStore,
    latest: &WorldRevision,
    applied: &World,
) -> Result<BTreeMap<String, Origin>, StoreError> {
    let applied = serde_json::to_value(applied)?;
    let current = entries(&latest.document);
    // The raw values, so an entry the validator trimmed compares as written.
    let mut pending: BTreeMap<String, Value> = entries(&applied)
        .into_keys()
        .filter_map(|p| current.get(&p).cloned().map(|v| (p, v)))
        .collect();
    let mut origins = BTreeMap::new();
    let mut origin = Origin::of(latest);
    let oldest = (latest.revision - MAX_ATTRIBUTION_REVISIONS).max(1);
    let mut rev = latest.revision - 1;
    while !pending.is_empty() && rev >= oldest {
        let prev = match store
            .get_world_revision(latest.mock_id, latest.version, rev)
            .await
        {
            Ok(prev) => prev,
            Err(StoreError::NotFound(_)) => break,
            Err(e) => return Err(e),
        };
        let before = entries(&prev.document);
        pending.retain(|pointer, value| {
            if before.get(pointer) == Some(value) {
                true
            } else {
                origins.insert(pointer.clone(), origin.clone());
                false
            }
        });
        origin = Origin::of(&prev);
        rev -= 1;
    }
    for pointer in pending.into_keys() {
        origins.insert(pointer, origin.clone());
    }
    Ok(origins)
}

/// `doc` (a serialized [`World`]) with only the entries whose pointers are
/// in `keep`, dropping types and operations left with none.
fn retain_entries(doc: &Value, keep: &BTreeSet<&str>) -> Value {
    let mut out = doc.clone();
    for section in ["types", "operations"] {
        let Some(items) = out.get_mut(section).and_then(Value::as_object_mut) else {
            continue;
        };
        items.retain(|name, desc| {
            let Some(fields) = desc.as_object_mut() else {
                return false;
            };
            fields.retain(|field, _| {
                keep.contains(format!("/{section}/{}/{}", escape(name), escape(field)).as_str())
            });
            !fields.is_empty()
        });
        if items.is_empty()
            && let Some(map) = out.as_object_mut()
        {
            map.remove(section);
        }
    }
    out
}

/// Starts contract version `to` of `mock_id` from version `from`'s newest
/// overlay: the entries of it that validate against `to_bundle` become the
/// first revisions of `to`, so that each entry keeps its author. The
/// entries are replayed oldest first, one revision for each author (and
/// `created_by`) that set one, each revision holding its entries and all
/// earlier ones; the last holds them all. It returns that last revision.
/// The entries that don't validate are dropped; what they described shows
/// in `to`'s gaps.
///
/// It does nothing (`Ok(None)`) when `from` has no revision, when no entry
/// validates, when the revision is not a world description, or when `to`
/// already has a revision.
pub async fn carry_forward(
    store: &dyn ControlStore,
    mock_id: Uuid,
    from: i64,
    to: i64,
    to_bundle: &Bundle,
) -> Result<Option<WorldRevision>, StoreError> {
    let Some(latest) = store.latest_world_revision(mock_id, from).await? else {
        return Ok(None);
    };
    carry(store, latest, to, to_bundle).await
}

/// [`carry_forward`] from `latest`, the source version's newest revision.
async fn carry(
    store: &dyn ControlStore,
    latest: WorldRevision,
    to: i64,
    to_bundle: &Bundle,
) -> Result<Option<WorldRevision>, StoreError> {
    let Ok(doc) = serde_json::from_value::<World>(latest.document.clone()) else {
        return Ok(None);
    };
    let mut applied = validate(&doc, to_bundle).applied;
    // A type or operation every entry of which went stale carries nothing.
    applied.types.retain(|_, t| *t != TypeDesc::default());
    applied.operations.retain(|_, op| *op != OpDesc::default());
    if applied.types.is_empty() && applied.operations.is_empty() {
        return Ok(None);
    }
    let origins = attribute(store, &latest, &applied).await?;
    let document = serde_json::to_value(&applied)?;
    // The entries of each author, in the order their oldest entry was set.
    let mut groups: Vec<(Origin, Vec<String>)> = Vec::new();
    for pointer in entries(&document).into_keys() {
        let origin = origins
            .get(&pointer)
            .cloned()
            .unwrap_or_else(|| Origin::of(&latest));
        match groups
            .iter_mut()
            .find(|(o, _)| o.author == origin.author && o.created_by == origin.created_by)
        {
            Some((o, pointers)) => {
                o.revision = o.revision.min(origin.revision);
                pointers.push(pointer);
            }
            None => groups.push((origin, vec![pointer])),
        }
    }
    groups.sort_by_key(|(o, _)| o.revision);
    let mut keep: BTreeSet<&str> = BTreeSet::new();
    let mut added = None;
    for (expect, (origin, pointers)) in (0..).zip(&groups) {
        keep.extend(pointers.iter().map(String::as_str));
        let revision = store
            .add_world_revision(
                latest.mock_id,
                to,
                NewWorldRevision {
                    author: origin.author,
                    document: retain_entries(&document, &keep),
                    created_by: origin.created_by.clone(),
                    expect: Some(expect),
                },
            )
            .await;
        match revision {
            Ok(r) => added = Some(r),
            // Someone else wrote to `to` first: theirs stands.
            Err(StoreError::Conflict(_)) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(added)
}

/// [`carry_forward`] into the contract version `to` from the newest earlier
/// version that has a revision, of the 32 newest earlier ones. Versions that
/// are not drafts come first: a draft's description is used only when no
/// other version has one, so an abandoned draft is not carried on.
pub async fn carry_forward_into(
    store: &dyn ControlStore,
    mock_id: Uuid,
    to: i64,
    to_bundle: &Bundle,
) -> Result<Option<WorldRevision>, StoreError> {
    let mut earlier: Vec<(bool, i64)> = store
        .list_contract_version_infos(mock_id)
        .await?
        .into_iter()
        .filter(|v| v.version < to)
        .map(|v| (v.status == ContractStatus::Draft, v.version))
        .collect();
    // Not drafts first, newest first within each.
    earlier.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let found = stream::iter(earlier.into_iter().take(MAX_CARRY_VERSIONS))
        .map(|(_, from)| store.latest_world_revision(mock_id, from))
        .buffered(CARRY_LOOKAHEAD)
        .try_filter_map(|r| async move { Ok(r) })
        .boxed()
        .try_next()
        .await?;
    match found {
        Some(latest) => carry(store, latest, to, to_bundle).await,
        None => Ok(None),
    }
}

/// [`carry_forward_into`], logging instead of failing: a world description
/// is optional, so the new version stands without it.
pub(crate) async fn carry_forward_logged(
    store: &dyn ControlStore,
    mock_id: Uuid,
    to: i64,
    to_bundle: &Bundle,
) {
    if let Err(e) = carry_forward_into(store, mock_id, to, to_bundle).await {
        tracing::warn!(mock = %mock_id, version = to, error = %e, "cannot carry the world description forward");
    }
}
