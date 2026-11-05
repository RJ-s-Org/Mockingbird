//! World descriptions of contract versions: the effective description and
//! carrying a description forward to a new version.
//!
//! The store keeps only the authored overlay, as numbered revisions per
//! contract version. [`effective_world`] computes the description a version
//! has: its tier-0 description ([`tier0`]) overlaid with the newest
//! revision's entries that validate against the version's bundle, the
//! author of each overlaid entry, what was refused, and the gaps.
//! [`carry_forward`] starts a new version from an earlier one's newest
//! overlay, keeping each entry's author, and the new version's effective
//! world reports the entries that no longer validate; [`carry_forward_into`]
//! finds that earlier version and is called right after a contract version
//! is added that serves or may serve (uploaded, reconciled from the config
//! file, or a draft when it is approved).
//!
//! `GET /api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/world`
//! ([`get_world`]) answers a version's effective description as a
//! [`WorldView`].

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::State;
use futures::{StreamExt, TryStreamExt, stream};
use mb_auth::Permission;

use mb_import::world::tier0;
use mb_mcm::Bundle;
use mb_mcm::world::{Author, Refusal, RefusalCode, World, overlay, validate};
use mb_protocols::world_gaps::{Gap, GapCode, gaps};
use mb_store::{
    ContractStatus, ContractVersion, ControlStore, Mock, NewWorldRevision, StoreError,
    WorldRevision,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::AdminState;
use crate::access::WsAccess;
use crate::problem::{ApiResult, Json, Path, Problem};

/// How many earlier revisions [`effective_world`] reads, at most, to find
/// which revision last set an entry. An entry unchanged for longer is
/// attributed to the oldest revision read.
const MAX_ATTRIBUTION_REVISIONS: i64 = 64;

/// How many other contract versions [`carry_forward_into`] looks through,
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
/// at all is refused whole (an empty path) and tier 0 applies, without
/// falling back to an earlier revision. With no revision it is tier 0 at
/// revision `0`. It fails only when the store does.
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
    let authors = attribute(store, &latest, &serde_json::to_value(&applied)?)
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

/// The origin of each entry of `doc` (a part of `latest`'s document, such as
/// its validated entries): walking back from `latest`, the newest revision
/// in the run of revisions that hold the entry's current value.
///
/// It reads the earlier revisions one at a time and stops as soon as every
/// entry is attributed, at most [`MAX_ATTRIBUTION_REVISIONS`] of them; this
/// is an admin read, never on the request path.
async fn attribute(
    store: &dyn ControlStore,
    latest: &WorldRevision,
    doc: &Value,
) -> Result<BTreeMap<String, Origin>, StoreError> {
    let current = entries(&latest.document);
    // The raw values, so an entry the validator trimmed compares as written.
    let mut pending: BTreeMap<String, Value> = entries(doc)
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
/// overlay: its entries become the first revisions of `to`, so that each
/// entry keeps its author. The entries are replayed oldest first, one
/// revision for each author (and `created_by`) that set one, each revision
/// holding its entries and all earlier ones; the last holds them all. It
/// returns that last revision.
///
/// Only the entries that validate against `to_bundle` apply in `to`: the
/// whole overlay is carried, so [`effective_world`] of `to` refuses the
/// stale ones (an entry on a member `to` no longer has is `unresolved`),
/// lists each as a gap, and lays tier 0 under them. The author sees what
/// the new contract dropped, and an entry comes back if a later contract
/// restores what it names.
///
/// It does nothing (`Ok(None)`) when `from` has no revision, when the
/// revision sets no entry or is not a world description, or when `to`
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
    let document = &latest.document;
    if entries(document).is_empty() {
        return Ok(None);
    }
    let stale = validate(&doc, to_bundle).refused.len();
    if stale > 0 {
        tracing::debug!(
            mock = %latest.mock_id,
            from = latest.version,
            to,
            stale,
            "carrying world entries that no longer validate; they show as refused"
        );
    }
    let origins = attribute(store, &latest, document).await?;
    // The entries of each author, in the order their oldest entry was set.
    let mut groups: Vec<(Origin, Vec<String>)> = Vec::new();
    for pointer in entries(document).into_keys() {
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
                    document: retain_entries(document, &keep),
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

/// [`carry_forward`] into the contract version `to` from the newest other
/// version that has a revision, of the 32 newest. Versions that are not
/// drafts come first: a draft's description is used only when no other
/// version has one, so an abandoned draft is not carried on. Later versions
/// count too, so a draft approved after a newer upload was revised starts
/// from that upload's description; when `to` was just added there are none.
pub async fn carry_forward_into(
    store: &dyn ControlStore,
    mock_id: Uuid,
    to: i64,
    to_bundle: &Bundle,
) -> Result<Option<WorldRevision>, StoreError> {
    let mut others: Vec<(bool, i64)> = store
        .list_contract_version_infos(mock_id)
        .await?
        .into_iter()
        .filter(|v| v.version != to)
        .map(|v| (v.status == ContractStatus::Draft, v.version))
        .collect();
    // Not drafts first, newest first within each.
    others.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let found = stream::iter(others.into_iter().take(MAX_CARRY_VERSIONS))
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

/// The path segment naming the active contract version.
pub const ACTIVE_VERSION: &str = "active";

/// Path of one contract version's world description: a version number or
/// [`ACTIVE_VERSION`].
#[derive(Deserialize, Debug)]
pub struct WorldPath {
    pub mock: String,
    pub version: String,
}

/// The OpenAPI schema of [`Author`] (which lives in `mb-mcm`).
#[derive(ToSchema, Debug)]
#[schema(as = WorldAuthor, rename_all = "snake_case")]
pub enum AuthorSchema {
    /// The importer's heuristics (tier 0).
    Heuristic,
    /// The user's coding assistant.
    Agent,
    /// Mockingbird's configured LLM.
    Llm,
    /// A person.
    User,
}

/// Keeps the schema in step with [`Author`].
impl From<Author> for AuthorSchema {
    fn from(a: Author) -> Self {
        match a {
            Author::Heuristic => Self::Heuristic,
            Author::Agent => Self::Agent,
            Author::Llm => Self::Llm,
            Author::User => Self::User,
        }
    }
}

/// The OpenAPI schema of [`RefusalCode`] (which lives in `mb-mcm`).
#[derive(ToSchema, Debug)]
#[schema(as = WorldRefusalCode, rename_all = "snake_case")]
pub enum RefusalCodeSchema {
    /// A key the description format doesn't define at that place.
    UnknownKey,
    /// It names something the contract doesn't have.
    Unresolved,
    /// It doesn't apply to the operation's protocol.
    NotApplicable,
    /// It is malformed.
    Invalid,
    /// The whole document is too large.
    TooLarge,
}

/// Keeps the schema in step with [`RefusalCode`].
impl From<RefusalCode> for RefusalCodeSchema {
    fn from(c: RefusalCode) -> Self {
        match c {
            RefusalCode::UnknownKey => Self::UnknownKey,
            RefusalCode::Unresolved => Self::Unresolved,
            RefusalCode::NotApplicable => Self::NotApplicable,
            RefusalCode::Invalid => Self::Invalid,
            RefusalCode::TooLarge => Self::TooLarge,
        }
    }
}

/// The OpenAPI schema of [`Refusal`] (which lives in `mb-mcm`): one entry
/// of a description that did not validate.
#[derive(ToSchema, Debug)]
#[schema(as = WorldRefusal)]
pub struct RefusalSchema {
    /// A JSON pointer into the description naming the entry; empty for the
    /// whole document.
    pub path: String,
    pub code: RefusalCodeSchema,
    /// What is wrong, for a person or an assistant to fix.
    pub message: String,
}

/// The OpenAPI schema of [`GapCode`] (which lives in `mb-protocols`).
#[derive(ToSchema, Debug)]
#[schema(as = WorldGapCode, rename_all = "snake_case")]
pub enum GapCodeSchema {
    /// A type that operations write to has no `identity`.
    NoIdentity,
    /// An operation on a type has no `target`.
    NoTarget,
    /// A write answers an object in which no member fits the type.
    NoRecord,
    /// An HTTP route of the contract that is not served.
    SkippedRoute,
    /// An entry of the description refused at validation.
    Refused,
}

/// Keeps the schema in step with [`GapCode`].
impl From<GapCode> for GapCodeSchema {
    fn from(c: GapCode) -> Self {
        match c {
            GapCode::NoIdentity => Self::NoIdentity,
            GapCode::NoTarget => Self::NoTarget,
            GapCode::NoRecord => Self::NoRecord,
            GapCode::SkippedRoute => Self::SkippedRoute,
            GapCode::Refused => Self::Refused,
        }
    }
}

/// The OpenAPI schema of [`Gap`] (which lives in `mb-protocols`): something
/// the description leaves open.
#[derive(ToSchema, Debug)]
#[schema(as = WorldGap)]
pub struct GapSchema {
    pub code: GapCodeSchema,
    /// The operation it is about, when it is about one.
    pub operation: Option<String>,
    /// The resource type it is about, when it is about one.
    pub r#type: Option<String>,
    /// What is missing, for a person or an assistant to fix.
    pub message: String,
}

/// A contract version's effective world description.
#[derive(Serialize, ToSchema, Debug)]
pub struct WorldView {
    /// The contract version it describes (the active one's number for
    /// `active`).
    pub version: i64,
    /// The description: tier 0 overlaid with the newest revision's entries
    /// that validate. Its format is `mockingbird world schema`.
    #[schema(value_type = Object)]
    pub world: World,
    /// The author of each entry the overlay sets, keyed by JSON pointer
    /// (`/types/{t}/{field}`, `/operations/{op}/{field}`); every entry not
    /// listed is `heuristic`.
    #[schema(value_type = BTreeMap<String, AuthorSchema>)]
    pub authors: BTreeMap<String, Author>,
    /// The newest revision's number; `0` when there is none.
    pub revision: i64,
    /// The newest revision's entries that did not validate; they fall back
    /// to tier 0.
    #[schema(value_type = Vec<RefusalSchema>)]
    pub refused: Vec<Refusal>,
    /// What the description leaves open, including each refused entry.
    #[schema(value_type = Vec<GapSchema>)]
    pub gaps: Vec<Gap>,
}

impl WorldView {
    fn new(version: i64, eff: EffectiveWorld) -> Self {
        Self {
            version,
            world: eff.world,
            authors: eff.authors,
            revision: eff.revision,
            refused: eff.refused,
            gaps: eff.gaps,
        }
    }
}

/// Contract version `segment` of `mock`: a number, or [`ACTIVE_VERSION`].
/// A 404 problem when the mock has no such version (or no active one), a
/// 400 when `segment` is neither.
pub(crate) async fn resolve_version(
    st: &AdminState,
    mock: &Mock,
    segment: &str,
) -> ApiResult<ContractVersion> {
    if segment == ACTIVE_VERSION {
        return st.store.active_contract(mock.id).await?.ok_or_else(|| {
            Problem::not_found(format!(
                "mock {:?} has no active contract version",
                mock.name
            ))
        });
    }
    let number: i64 = segment.parse().map_err(|_| {
        Problem::bad_request(format!(
            "contract version {segment:?} is not a number or {ACTIVE_VERSION:?}"
        ))
    })?;
    crate::contracts::version(st, mock, number).await
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/world",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = String, Path, description = "Contract version number, or `active`"),
    ),
    responses(
        (status = 200, description = "The version's effective world description with each overlaid entry's author, the refused entries and the gaps. A version with no revision answers tier 0 at revision 0.", body = WorldView),
        (status = 400, description = "The version is neither a number nor `active`", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such mock or version, or no active version", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_world(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(WorldPath { mock, version }): Path<WorldPath>,
) -> ApiResult<Json<WorldView>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = resolve_version(&st, &m, &version).await?;
    let eff = effective_world(st.store.as_ref(), m.id, v.version, &v.bundle).await?;
    Ok(Json(WorldView::new(v.version, eff)))
}
