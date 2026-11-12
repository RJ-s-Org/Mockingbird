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
//! [`WorldView`], narrowed by `?operation=` and `?type=` ([`WorldFilter`]).
//! The same path takes `PUT` ([`put_world`], a whole
//! overlay) and `PATCH` ([`patch_world`], a JSON merge patch of the newest
//! overlay), each adding a revision that holds only the entries that
//! validate; `…/world/revisions[/{r}]` lists and reads revisions,
//! `…/world/revisions/{r}/restore` adds one holding an earlier overlay, and
//! `…/world/diff` compares two. A write with `If-Match: <revision>` applies
//! only on top of that revision (else a 412 `revision_conflict`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex, PoisonError};

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::{StreamExt, TryStreamExt, stream};
use mb_auth::Permission;

use mb_mcm::Bundle;
use mb_mcm::world::{
    Author, ChangeDesc, OpDesc, OpKind, Pointer, Refusal, RefusalCode, TypeDesc, Validation, World,
    check_size, effective_validated, tier0, validate, written_path_targets,
};
use mb_protocols::world_gaps::{Gap, GapCode, gaps};
use mb_store::{
    ContractStatus, ContractVersion, ControlStore, Mock, NewWorldRevision, StoreError, UnknownIds,
    WorldRevision, WorldRevisionInfo,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::AdminState;
use crate::access::WsAccess;
use crate::audit::snapshot;
use crate::problem::{
    ApiResult, Json, PROBLEM_CONTENT_TYPE, Path, Problem, Query, RequireJson, media_type,
    unsupported_media_type,
};

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
    /// to tier 0, then the entries of the mock's `world:` file refused when
    /// the config was last applied to this version (see
    /// [`record_file_refusals`]).
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
        let refused = with_file_refusals(Vec::new(), mock_id, version);
        let gaps = gaps(bundle, &base, &refused);
        return Ok(EffectiveWorld {
            world: base,
            authors: BTreeMap::new(),
            revision: 0,
            refused,
            gaps,
        });
    };
    let (world, Validation { applied, refused }) =
        match serde_json::from_value::<World>(latest.document.clone()) {
            Ok(doc) => effective_validated(bundle, &base, &doc),
            Err(e) => (
                base,
                Validation {
                    applied: World::default(),
                    refused: vec![Refusal {
                        path: String::new(),
                        code: RefusalCode::Invalid,
                        message: format!(
                            "revision {} is not a world description: {e}",
                            latest.revision
                        ),
                    }],
                },
            ),
        };
    let authors = attribute(store, &latest, &serde_json::to_value(&applied)?)
        .await?
        .into_iter()
        .map(|(pointer, origin)| (pointer, origin.author))
        .collect();
    let refused = with_file_refusals(refused, mock_id, version);
    let gaps = gaps(bundle, &world, &refused);
    Ok(EffectiveWorld {
        world,
        authors,
        revision: latest.revision,
        refused,
        gaps,
    })
}

/// The entries of each config-file mock's `world:` file refused when the
/// config was last applied, by mock id: the contract version they were
/// validated against, and the refusals. Only the node that applies the
/// config (the leader) holds them.
static FILE_REFUSALS: LazyLock<Mutex<FileRefusals>> = LazyLock::new(Mutex::default);

/// Per mock id: the contract version, and its `world:` file's refusals.
type FileRefusals = HashMap<Uuid, (i64, Vec<Refusal>)>;

/// Records the entries of `mock_id`'s `world:` file that `version` refused
/// as the config was applied, replacing those of the previous load; an
/// empty `refused` clears them. [`effective_world`] of that version lists
/// them under `refused` (and its gaps), since the stored revision holds only
/// the entries that validate.
pub fn record_file_refusals(mock_id: Uuid, version: i64, refused: Vec<Refusal>) {
    let mut all = FILE_REFUSALS.lock().unwrap_or_else(PoisonError::into_inner);
    if refused.is_empty() {
        all.remove(&mock_id);
    } else {
        all.insert(mock_id, (version, refused));
    }
}

/// `refused` followed by the recorded file refusals of `mock_id`'s
/// `version` whose pointer it does not already name.
fn with_file_refusals(mut refused: Vec<Refusal>, mock_id: Uuid, version: i64) -> Vec<Refusal> {
    let all = FILE_REFUSALS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((v, file)) = all.get(&mock_id)
        && *v == version
    {
        for r in file {
            if !refused.iter().any(|e| e.path == r.path) {
                refused.push(r.clone());
            }
        }
    }
    refused
}

/// The world description a runtime of `mock_id`'s contract `version` serves,
/// with its revision: what [`effective_world`] answers as `(world, revision)`,
/// over `base` (`tier0(bundle)`, computed once by the caller), and the operations
/// whose target path parameter the revision writes ([`written_path_targets`]).
///
/// It reads only the newest revision, once, and none of the earlier ones:
/// the runtime needs neither the authors nor the gaps. A newest revision
/// whose document is not a world description serves `base` at that
/// revision, as [`effective_world`] does (the refusal is logged). With no
/// revision it is `base` at revision `0`.
///
/// # Errors
///
/// Only when the store read fails.
pub async fn runtime_world(
    store: &dyn ControlStore,
    mock_id: Uuid,
    version: i64,
    bundle: &Bundle,
    base: &World,
) -> Result<(World, i64, BTreeSet<String>), StoreError> {
    let Some(latest) = store.latest_world_revision(mock_id, version).await? else {
        return Ok((base.clone(), 0, BTreeSet::new()));
    };
    match serde_json::from_value::<World>(latest.document) {
        Ok(doc) => {
            let (world, v) = effective_validated(bundle, base, &doc);
            let written = written_path_targets(&v.applied, &world);
            Ok((world, latest.revision, written))
        }
        Err(error) => {
            tracing::warn!(
                mock = %mock_id,
                revision = latest.revision,
                %error,
                "the newest world revision is not a world description; serving tier 0"
            );
            Ok((base.clone(), latest.revision, BTreeSet::new()))
        }
    }
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
///
/// The replay is not atomic: each revision is written expecting the one
/// before it as the newest, and a concurrent write to `to` (a `PUT` or
/// `PATCH` of `…/world`, a config-file reconcile) that lands between two of
/// them ends the carry there, leaving that write on top. When it lands
/// before the first, nothing is carried and `Ok(None)` is returned. When it
/// lands later, `to` keeps the groups already written, oldest authors
/// first, and the concurrent revision after them; the later authors'
/// entries are not carried (a `PATCH` merged onto the partial overlay
/// lacks them, a `PUT` replaced it anyway), and the returned revision is
/// the last one carried, which is no longer the newest.
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
            // Someone else wrote to `to` first: theirs stands. Past the
            // first group this is a partial carry (see `carry_forward`):
            // the groups written so far stay under theirs, the rest are
            // dropped, and `added` is our last revision, not the newest.
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
    /// A type that operations name has no identity their calls can use, so the model answers them.
    NoIdentity,
    /// An operation on a type has no `target`.
    NoTarget,
    /// A write answers an object in which no member fits the type.
    NoRecord,
    /// An HTTP route of the contract that is not served.
    SkippedRoute,
    /// An entry of the description refused at validation.
    Refused,
    /// An action the description gives nothing to change (advisory).
    NoEffect,
    /// A list query parameter the executor reads nothing for.
    UndescribedParam,
    /// A type whose create can answer 409 with no `unique` (advisory).
    NoUnique,
    /// A deleted type whose record has a deleted member, with no `lifecycle.soft_delete` (advisory).
    NoSoftDelete,
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
            GapCode::NoEffect => Self::NoEffect,
            GapCode::UndescribedParam => Self::UndescribedParam,
            GapCode::NoUnique => Self::NoUnique,
            GapCode::NoSoftDelete => Self::NoSoftDelete,
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
    /// Whether it is a check, not something missing: behaviour the contract hints at,
    /// to describe only when the docs state it. Absent when false.
    #[schema(default = false)]
    pub advisory: Option<bool>,
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
    /// The newest revision's entries that did not validate, and the
    /// entries of a config-file mock's `world:` file refused when the config
    /// was last loaded; they fall back to tier 0.
    #[schema(value_type = Vec<RefusalSchema>)]
    pub refused: Vec<Refusal>,
    /// What the description leaves open, including each refused entry.
    #[schema(value_type = Vec<GapSchema>)]
    pub gaps: Vec<Gap>,
    /// The `operation` and `type` filters that name nothing in this version,
    /// one line per kind. Absent when there is none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// With `called`: how many gaps were left out because they name neither
    /// an operation of the request log nor a type those operations touch
    /// (nor skip a route of one).
    /// Absent without `called`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gaps_left_out: Option<usize>,
    /// How a read by path of an id the session does not hold is answered, per type.
    pub unknown_ids: UnknownIdsView,
}

/// How a mock answers a read by path of an id the session does not hold.
#[derive(Serialize, ToSchema, Debug)]
pub struct UnknownIdsView {
    /// The mock's `unknown_ids`: `inferred` (unset: decided per type), `generate` or
    /// `404` (decided for every type).
    #[schema(value_type = String, example = "inferred")]
    pub setting: UnknownIds,
    /// Per resource type with a read by path parameter: its answer and why.
    pub types: BTreeMap<String, UnknownIdAnswer>,
}

/// One type's answer to a read of an id the session does not hold.
#[derive(Serialize, ToSchema, Debug, PartialEq)]
pub struct UnknownIdAnswer {
    /// `404` (the read's declared 404) or `generate`. An inferred `404` applies once
    /// the session holds a record of the type it created or listed; before that the
    /// read generates.
    pub answer: String,
    /// Why: the mock's setting, or whether an operation of kind create makes the type.
    pub why: String,
}

impl UnknownIdsView {
    /// The answer of each type `world` reads by a path parameter under `setting`. Left
    /// unset, a type an operation of kind create makes answers 404 once the session
    /// holds one of its records; any other generates (reference data the app reads).
    fn new(world: &World, setting: UnknownIds) -> Self {
        let mut creates: BTreeMap<&str, &str> = BTreeMap::new();
        let mut read: BTreeSet<&str> = BTreeSet::new();
        for (id, op) in &world.operations {
            let Some(t) = op.r#type.as_deref() else {
                continue;
            };
            match op.kind {
                Some(OpKind::Create) => {
                    creates.entry(t).or_insert(id);
                }
                Some(OpKind::Read)
                    if op.target.as_ref().is_some_and(|t| t.path_param.is_some()) =>
                {
                    read.insert(t);
                }
                _ => {}
            }
        }
        let types = read
            .into_iter()
            .map(|t| {
                let (answer, why) = match (setting, creates.get(t)) {
                    (UnknownIds::NotFound, _) => ("404", "the mock sets unknown_ids: 404".to_owned()),
                    (UnknownIds::Generate, _) => {
                        ("generate", "the mock sets unknown_ids: generate".to_owned())
                    }
                    (UnknownIds::Inferred, Some(create)) => (
                        "404",
                        format!(
                            "creatable ({create}): 404 once the session holds a record of it it created or listed; generated before that"
                        ),
                    ),
                    (UnknownIds::Inferred, None) => (
                        "generate",
                        "not creatable: no operation of kind create makes it, so its ids are reference data the app reads".to_owned(),
                    ),
                };
                (
                    t.to_owned(),
                    UnknownIdAnswer {
                        answer: answer.to_owned(),
                        why,
                    },
                )
            })
            .collect();
        Self { setting, types }
    }
}

impl WorldView {
    fn new(version: i64, eff: EffectiveWorld, unknown_ids: UnknownIds) -> Self {
        let unknown_ids = UnknownIdsView::new(&eff.world, unknown_ids);
        Self {
            version,
            world: eff.world,
            authors: eff.authors,
            revision: eff.revision,
            refused: eff.refused,
            gaps: eff.gaps,
            warnings: Vec::new(),
            gaps_left_out: None,
            unknown_ids,
        }
    }

    /// What `called` narrows to, read before any narrowing: the operations of
    /// `bundle` that `logged` (the request log's operation ids, a
    /// GraphQL entry's ids joined with `,`) names, and the resource types
    /// those operations touch (their described type, else the contract's,
    /// and the type of each of their gaps).
    fn called_scope(&self, bundle: &Bundle, logged: &BTreeSet<String>) -> Called {
        let named: BTreeSet<&str> = logged
            .iter()
            .flat_map(|id| std::iter::once(id.as_str()).chain(id.split(',')))
            .map(str::trim)
            .collect();
        let operations: BTreeSet<String> = bundle
            .operations
            .iter()
            .filter(|op| named.contains(op.id.as_str()))
            .map(|op| op.id.clone())
            .collect();
        let types = bundle
            .operations
            .iter()
            .filter(|op| operations.contains(&op.id))
            .filter_map(|op| {
                self.world
                    .operations
                    .get(&op.id)
                    .and_then(|d| d.r#type.clone())
                    .or_else(|| op.resource_type.clone())
            })
            .chain(
                self.gaps
                    .iter()
                    .filter(|g| g.operation.as_ref().is_some_and(|o| operations.contains(o)))
                    .filter_map(|g| g.r#type.clone()),
            )
            .collect();
        Called { operations, types }
    }

    /// Narrows the view to what the app called: the operations of
    /// `called`, the types they touch, the gaps naming one of those
    /// operations, naming no operation and one of those types, or skipping a
    /// route of one of those types (a skipped route is never served, so the
    /// log never names it), and the refused entries and authors about
    /// either. Gaps and refusals about the description as a whole are kept,
    /// as [`WorldView::narrow`] keeps them. The other gaps are dropped and
    /// counted in [`WorldView::gaps_left_out`]; with no called operation a
    /// warning says to run the app's flows first.
    fn keep_called(&mut self, called: &Called) {
        let Called { operations, types } = called;
        if operations.is_empty() {
            self.warnings.push(format!(
                "the request log names no operation of contract version {}: run the app's flows against the mock first",
                self.version
            ));
        }
        let before = self.gaps.len();
        self.gaps.retain(|g| match (&g.operation, &g.r#type) {
            (Some(op), ty) => {
                operations.contains(op)
                    || (g.code == GapCode::SkippedRoute
                        && ty.as_ref().is_some_and(|t| types.contains(t)))
            }
            (None, Some(t)) => types.contains(t),
            (None, None) => true,
        });
        self.gaps_left_out = Some(before - self.gaps.len());
        self.world
            .operations
            .retain(|id, _| operations.contains(id));
        self.world.types.retain(|t, _| types.contains(t));
        self.unknown_ids.types.retain(|t, _| types.contains(t));
        let keeps = |pointer: &str| keeps_pointer(pointer, operations, types).unwrap_or(true);
        self.refused.retain(|r| keeps(&r.path));
        self.authors.retain(|p, _| keeps(p));
    }

    /// Narrows the view to what `filter` names (nothing when it is empty):
    /// the operations it names and those whose resource type it names, the
    /// types it names, and the gaps, refused entries and authors about
    /// either. Gaps and refusals about the description as a whole (neither
    /// an operation nor a type) are kept. A name the version does not have
    /// is listed in [`WorldView::warnings`].
    fn narrow(&mut self, filter: &WorldFilter) {
        if filter.operations.is_empty() && filter.types.is_empty() {
            return;
        }
        let known_ops: BTreeSet<&str> = self
            .world
            .operations
            .keys()
            .map(String::as_str)
            .chain(self.gaps.iter().filter_map(|g| g.operation.as_deref()))
            .collect();
        let known_types: BTreeSet<&str> = self
            .world
            .types
            .keys()
            .map(String::as_str)
            .chain(
                self.world
                    .operations
                    .values()
                    .filter_map(|o| o.r#type.as_deref()),
            )
            .chain(self.gaps.iter().filter_map(|g| g.r#type.as_deref()))
            .collect();
        for (kind, asked, known) in [
            ("operation", &filter.operations, &known_ops),
            ("type", &filter.types, &known_types),
        ] {
            let unknown: Vec<&str> = asked
                .iter()
                .map(String::as_str)
                .filter(|n| !known.contains(n))
                .collect();
            if !unknown.is_empty() {
                self.warnings.push(format!(
                    "no {kind} {} in contract version {}",
                    unknown.join(", "),
                    self.version
                ));
            }
        }
        // The operations kept: those named, and those of a named type.
        let ops: BTreeSet<String> = self
            .world
            .operations
            .iter()
            .filter(|(_, d)| d.r#type.as_ref().is_some_and(|t| filter.types.contains(t)))
            .map(|(id, _)| id.clone())
            .chain(filter.operations.iter().cloned())
            .collect();
        let types = &filter.types;
        self.world.operations.retain(|id, _| ops.contains(id));
        self.world.types.retain(|t, _| types.contains(t));
        self.unknown_ids.types.retain(|t, _| types.contains(t));
        self.gaps.retain(|g| match (&g.operation, &g.r#type) {
            (None, None) => true,
            (op, ty) => {
                op.as_ref().is_some_and(|o| ops.contains(o))
                    || ty.as_ref().is_some_and(|t| types.contains(t))
            }
        });
        let keeps = |pointer: &str| keeps_pointer(pointer, &ops, types).unwrap_or(true);
        self.refused.retain(|r| keeps(&r.path));
        self.authors.retain(|p, _| keeps(p));
    }
}

/// Whether a narrowed view keeps the entry at `pointer`: one under
/// `/operations/{op}` when `ops` has `op`, one under `/types/{t}` when
/// `types` has `t`; `None` for a pointer about the description as a whole.
fn keeps_pointer(pointer: &str, ops: &BTreeSet<String>, types: &BTreeSet<String>) -> Option<bool> {
    let mut parts = pointer.strip_prefix('/')?.splitn(3, '/');
    let section = parts.next()?;
    let name = parts.next()?.replace("~1", "/").replace("~0", "~");
    match section {
        "operations" => Some(ops.contains(&name)),
        "types" => Some(types.contains(&name)),
        _ => None,
    }
}

/// The operations a mock's request log names and the types they touch
/// ([`WorldView::called`]).
#[derive(Debug, Default)]
struct Called {
    operations: BTreeSet<String>,
    types: BTreeSet<String>,
}

/// Which entries of a world view to answer: `?operation=<id>` and
/// `?type=<resource type>`, each repeatable, and `?called=true`. With none,
/// all of them.
#[derive(Debug, Default)]
pub struct WorldFilter {
    /// Operation ids.
    pub operations: BTreeSet<String>,
    /// Resource types.
    pub types: BTreeSet<String>,
    /// Only what the mock's request log names: the operations called and
    /// the types they touch.
    pub called: bool,
}

impl WorldFilter {
    /// The filter a query's `operation`, `type` and `called` pairs give;
    /// other keys are ignored. A 400 problem when `called` is neither `true`
    /// nor `false`.
    fn from_pairs(pairs: Vec<(String, String)>) -> ApiResult<Self> {
        let mut filter = Self::default();
        for (key, value) in pairs {
            match key.as_str() {
                "operation" => {
                    filter.operations.insert(value);
                }
                "type" => {
                    filter.types.insert(value);
                }
                "called" => {
                    filter.called = value.parse().map_err(|_| {
                        Problem::bad_request(format!("called must be true or false, not {value:?}"))
                    })?;
                }
                _ => {}
            }
        }
        Ok(filter)
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
        ("operation" = Option<Vec<String>>, Query, explode, description = "Answer only this operation (repeatable), with its gaps, refused entries and authors"),
        ("type" = Option<Vec<String>>, Query, explode, description = "Answer only this resource type (repeatable): its `types` entry, the operations of that type, and their gaps, refused entries and authors"),
        ("called" = Option<bool>, Query, description = "Answer only what the mock's request log (any session) names: the operations called, the types they touch, the gaps naming one of those operations, or naming no operation and one of those types, or skipping a route of one of those types, and their refused entries and authors; gaps about the description as a whole are kept. `gaps_left_out` counts the gaps left out. Combines with `operation` and `type`"),
    ),
    responses(
        (status = 200, description = "The version's effective world description with each overlaid entry's author, the refused entries and the gaps. A version with no revision answers tier 0 at revision 0. With `operation` or `type`, only the entries they match; a name the version does not have is listed in `warnings`. With `called`, only what the request log names, and `gaps_left_out`. `unknown_ids` gives each type read by a path parameter its answer to an id the session does not hold (`404` or `generate`) and why.", body = WorldView),
        (status = 400, description = "The version is neither a number nor `active`, or `called` is not a boolean", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such mock or version, or no active version", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_world(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(WorldPath { mock, version }): Path<WorldPath>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> ApiResult<Json<WorldView>> {
    access.require(Permission::Read)?;
    let filter = WorldFilter::from_pairs(pairs)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = resolve_version(&st, &m, &version).await?;
    let eff = effective_world(st.store.as_ref(), m.id, v.version, &v.bundle).await?;
    let mut view = WorldView::new(v.version, eff, m.behavior.unknown_ids);
    // Read from the whole view, before `narrow` drops the gaps whose types
    // it takes.
    let called = if filter.called {
        let logged = st.store.request_log_operations(m.id).await?;
        Some(view.called_scope(&v.bundle, &logged))
    } else {
        None
    };
    view.narrow(&filter);
    if let Some(called) = &called {
        view.keep_called(called);
    }
    Ok(Json(view))
}

// ---------------------------------------------------------------------------
// Authoring: put, patch, revisions, restore, diff.
// ---------------------------------------------------------------------------

/// The media type of a JSON merge patch (RFC 7396).
pub const MERGE_PATCH_CONTENT_TYPE: &str = "application/merge-patch+json";

/// How many times a write without `If-Match` is retried when another write
/// lands between reading the newest revision and adding the next one.
const WRITE_ATTEMPTS: usize = 4;

/// The `code` of the 412 answering a stale `If-Match`.
pub const REVISION_CONFLICT: &str = "revision_conflict";

/// Path of one revision of a contract version's world description.
#[derive(Deserialize, Debug)]
pub struct RevisionPath {
    pub mock: String,
    pub version: String,
    pub revision: i64,
}

/// Who writes a revision: `agent`, `llm` or `user`.
#[derive(Deserialize, Debug, Default)]
pub struct AuthorQuery {
    /// `agent`, `llm` or `user` (default `user`; `heuristic` is refused).
    pub author: Option<String>,
}

/// The two revisions to compare.
#[derive(Deserialize, Debug, Default)]
pub struct DiffQuery {
    /// The older revision; `0` is the empty overlay. Default: the one before
    /// `to`.
    pub from: Option<i64>,
    /// The newer revision; default the newest (`0` when there is none).
    pub to: Option<i64>,
}

/// The answer to a write: the revision added, what of the document was
/// applied (and stored) and what was refused.
#[derive(Serialize, ToSchema, Debug)]
pub struct WorldWriteResult {
    /// The contract version written to.
    pub version: i64,
    /// The new revision's number.
    pub revision: i64,
    /// The entries that validate: the stored overlay.
    #[schema(value_type = Object)]
    pub applied: World,
    /// The entries that did not validate; they are not stored.
    #[schema(value_type = Vec<RefusalSchema>)]
    pub refused: Vec<Refusal>,
    /// Whether the running mock serves the new revision when the write
    /// returns: the write waits for the rebuild, at most
    /// [`WORLD_MOUNT_WAIT`].
    pub runtime: WorldMount,
}

/// How long a world write waits for the running mock to serve it.
pub const WORLD_MOUNT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Where a world write stands on this node's running mock when the write
/// returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorldMount {
    /// The running mock serves the new revision: the next call uses it.
    Mounted,
    /// The mock is rebuilding (or retrying a failed load) and still serves
    /// the previous description; it applies once the rebuild lands.
    Pending,
    /// No running mock serves this version here: it is not the active
    /// version, or the mock is not mounted on this node.
    NotServed,
    /// This server does not run the mocks (a control-only node), so it
    /// cannot tell.
    Unknown,
}

/// Waits for a node's running mocks to serve a world revision
/// ([`AdminState::world_mounts`]).
#[async_trait::async_trait]
pub trait WorldMounts: Send + Sync {
    /// Waits, at most `within`, until mock `mock` of `workspace` serves
    /// contract `version` with world revision `revision` or newer.
    async fn wait(
        &self,
        workspace: &str,
        mock: &str,
        version: i64,
        revision: i64,
        within: std::time::Duration,
    ) -> WorldMount;
}

/// Where the write of `revision` on `v` stands on this node's running mock.
async fn mounted(
    st: &AdminState,
    ws: &str,
    m: &Mock,
    v: &ContractVersion,
    revision: i64,
) -> WorldMount {
    if v.status != ContractStatus::Active {
        return WorldMount::NotServed;
    }
    match &st.world_mounts {
        Some(mounts) => {
            mounts
                .wait(ws, &m.name, v.version, revision, st.world_mount_wait)
                .await
        }
        None => WorldMount::Unknown,
    }
}

/// One revision of a world description, without its document.
#[derive(Serialize, ToSchema, Debug)]
#[schema(as = WorldRevisionInfo)]
pub struct RevisionInfoView {
    pub id: Uuid,
    /// The contract version it is attached to.
    pub version: i64,
    /// 1-based, increasing per contract version.
    pub revision: i64,
    #[schema(value_type = AuthorSchema)]
    pub author: Author,
    /// The subject of the caller that wrote it, when known.
    pub created_by: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<WorldRevisionInfo> for RevisionInfoView {
    fn from(r: WorldRevisionInfo) -> Self {
        Self {
            id: r.id,
            version: r.version,
            revision: r.revision,
            author: r.author,
            created_by: r.created_by,
            created_at: r.created_at,
        }
    }
}

/// One revision of a world description with its overlay document.
#[derive(Serialize, ToSchema, Debug)]
#[schema(as = WorldRevision)]
pub struct RevisionView {
    pub id: Uuid,
    /// The contract version it is attached to.
    pub version: i64,
    /// 1-based, increasing per contract version.
    pub revision: i64,
    #[schema(value_type = AuthorSchema)]
    pub author: Author,
    /// The subject of the caller that wrote it, when known.
    pub created_by: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// The overlay as stored.
    #[schema(value_type = Object)]
    pub document: Value,
}

impl From<WorldRevision> for RevisionView {
    fn from(r: WorldRevision) -> Self {
        Self {
            id: r.id,
            version: r.version,
            revision: r.revision,
            author: r.author,
            created_by: r.created_by,
            created_at: r.created_at,
            document: r.document,
        }
    }
}

/// The leaf pointers that differ between two revisions' overlays.
#[derive(Serialize, ToSchema, Debug, PartialEq, Eq)]
pub struct WorldDiff {
    pub from: i64,
    pub to: i64,
    /// Leaves `to` has and `from` has not.
    pub added: Vec<String>,
    /// Leaves `from` has and `to` has not.
    pub removed: Vec<String>,
    /// Leaves both have, with different values.
    pub changed: Vec<String>,
}

/// The problem of a refused world write: a [`Problem`] plus a machine
/// readable `code` and, when it is about one place, the JSON `path` into
/// the document.
#[derive(Serialize, ToSchema, Debug)]
pub struct WorldProblem {
    /// Always `about:blank`.
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
    /// `too_large` (413), `invalid` (422) or `revision_conflict` (412).
    pub code: String,
    /// A JSON pointer into the document; empty for the whole document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Why a world write failed.
#[derive(Debug)]
pub enum WorldError {
    /// An ordinary problem (404, 400, 415, ...).
    Problem(Problem),
    /// A problem with a `code` and maybe a `path`.
    Coded {
        status: StatusCode,
        title: &'static str,
        code: &'static str,
        path: Option<String>,
        detail: String,
    },
}

impl WorldError {
    /// The 412 answering a write whose `If-Match` is not the newest revision.
    fn conflict(detail: impl Into<String>) -> Self {
        Self::Coded {
            status: StatusCode::PRECONDITION_FAILED,
            title: "Revision conflict",
            code: REVISION_CONFLICT,
            path: None,
            detail: detail.into(),
        }
    }

    /// The 413 or 422 of a document refused whole.
    fn refused(r: Refusal) -> Self {
        let (status, title) = match r.code {
            RefusalCode::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "World description too large"),
            _ => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid world description",
            ),
        };
        Self::Coded {
            status,
            title,
            code: r.code.as_str(),
            path: Some(r.path),
            detail: r.message,
        }
    }
}

impl From<Problem> for WorldError {
    fn from(p: Problem) -> Self {
        Self::Problem(p)
    }
}

impl From<StoreError> for WorldError {
    fn from(e: StoreError) -> Self {
        Self::Problem(e.into())
    }
}

impl IntoResponse for WorldError {
    fn into_response(self) -> Response {
        match self {
            Self::Problem(p) => p.into_response(),
            Self::Coded {
                status,
                title,
                code,
                path,
                detail,
            } => {
                let body = WorldProblem {
                    kind: "about:blank".into(),
                    title: title.into(),
                    status: status.as_u16(),
                    detail,
                    code: code.into(),
                    path,
                };
                let body = serde_json::to_vec(&body).unwrap_or_default();
                (status, [(header::CONTENT_TYPE, PROBLEM_CONTENT_TYPE)], body).into_response()
            }
        }
    }
}

type WorldResult<T> = Result<T, WorldError>;

/// The author `q` names: `agent`, `llm` or `user`, else `default`. A 400
/// for `heuristic` (only the importer writes tier 0) or anything else.
fn author_of(q: &AuthorQuery, default: Author) -> ApiResult<Author> {
    match q.author.as_deref() {
        None => Ok(default),
        Some("agent") => Ok(Author::Agent),
        Some("llm") => Ok(Author::Llm),
        Some("user") => Ok(Author::User),
        Some("heuristic") => Err(Problem::bad_request(
            "author `heuristic` is the importer's (tier 0); write as `agent`, `llm` or `user`",
        )),
        Some(other) => Err(Problem::bad_request(format!(
            "author {other:?} is not `agent`, `llm` or `user`"
        ))),
    }
}

/// The revision `If-Match` names (`3`, `"3"` or `W/"3"`; `0` for none yet),
/// `None` when absent or `*`. A 400 when it is not a revision number.
fn if_match(headers: &HeaderMap) -> ApiResult<Option<i64>> {
    let Some(value) = headers.get(header::IF_MATCH) else {
        return Ok(None);
    };
    let text = value.to_str().unwrap_or_default().trim();
    if text == "*" {
        return Ok(None);
    }
    let bare = text.strip_prefix("W/").unwrap_or(text).trim_matches('"');
    bare.parse::<i64>()
        .ok()
        .filter(|n| *n >= 0)
        .map(Some)
        .ok_or_else(|| {
            Problem::bad_request(format!(
                "If-Match {text:?} is not a world description revision number"
            ))
        })
}

/// A 415 unless the request's `Content-Type` is one of `accepted`.
fn require_media(headers: &HeaderMap, accepted: &[&str]) -> ApiResult<()> {
    match media_type(headers.get(header::CONTENT_TYPE)) {
        Some(m) if accepted.contains(&m.as_str()) => Ok(()),
        _ => Err(unsupported_media_type(
            "this endpoint",
            &accepted.join(" or "),
        )),
    }
}

/// The body, or a 413 `too_large` when it is over the route's limit.
fn body_bytes(body: Result<Bytes, BytesRejection>) -> WorldResult<Bytes> {
    body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            WorldError::refused(Refusal {
                path: String::new(),
                code: RefusalCode::TooLarge,
                message: format!(
                    "the description is over {} bytes; at most {} are accepted",
                    mb_mcm::world::MAX_BYTES,
                    mb_mcm::world::MAX_BYTES
                ),
            })
        } else {
            WorldError::Problem(Problem::new(e.status(), e.body_text()))
        }
    })
}

/// Parses JSON, a 400 problem when it is not.
fn parse_json(bytes: &[u8]) -> WorldResult<Value> {
    serde_json::from_slice(bytes).map_err(|e| {
        WorldError::Problem(Problem::titled(
            StatusCode::BAD_REQUEST,
            "Malformed JSON",
            format!("the body is not JSON: {e}"),
        ))
    })
}

/// The world description `doc` is, checked whole: at most
/// [`mb_mcm::world::MAX_BYTES`] (413) and [`mb_mcm::world::MAX_DEPTH`]
/// deep (422), of the format's shape and with pointers that parse (422
/// naming the first entry that is not). The entries are validated against
/// a bundle later, one by one.
fn check_document(doc: &Value) -> WorldResult<World> {
    check_world(doc).map_err(WorldError::refused)
}

/// [`check_document`], refusing with the [`Refusal`] itself.
fn check_world(doc: &Value) -> Result<World, Refusal> {
    let bytes = serde_json::to_vec(doc).map_err(|e| Refusal {
        path: String::new(),
        code: RefusalCode::Invalid,
        message: format!("the description does not serialize: {e}"),
    })?;
    check_size(&bytes)?;
    let world = serde_json::from_value::<World>(doc.clone()).map_err(|e| Refusal {
        path: shape_error_path(doc),
        code: RefusalCode::Invalid,
        message: format!("not a world description entry: {e}"),
    })?;
    if let Some(r) = bad_pointer(&world) {
        return Err(r);
    }
    Ok(world)
}

/// The overlay a write of `doc` against `bundle` stores, as the Admin API's
/// `PUT …/world` would: the entries that validate, without types and
/// operations left with none, and the entries refused.
///
/// For writers outside the Admin API (config-file mocks). It does what the
/// `PUT` checks do before storing: the size and depth limits, the format's
/// shape and the pointers, then [`validate`].
///
/// # Errors
///
/// The [`Refusal`] of a document refused whole: over
/// [`mb_mcm::world::MAX_BYTES`] (`too_large`), too deep, not of the format's
/// shape or with a pointer that does not parse (`invalid`, naming where).
pub fn validated_overlay(doc: &Value, bundle: &Bundle) -> Result<Validation, Refusal> {
    let world = check_world(doc)?;
    Ok(trimmed(validate(&world, bundle)))
}

/// `v` without the types and operations its `applied` has no entry for.
fn trimmed(mut v: Validation) -> Validation {
    v.applied.types.retain(|_, t| *t != TypeDesc::default());
    v.applied
        .operations
        .retain(|_, op| *op != OpDesc::default());
    v
}

/// The first request or answer pointer of `world` that does not parse.
fn bad_pointer(world: &World) -> Option<Refusal> {
    for (id, op) in &world.operations {
        let base = format!("/operations/{}", escape(id));
        // Each pointer, where it is, and whether it is a record pointer (which
        // may be the root `""`: the whole answer or request body is the record).
        let mut pointers: Vec<(String, &Pointer, bool)> = Vec::new();
        if let Some(r) = &op.request {
            pointers.extend(
                r.record
                    .iter()
                    .map(|p| (format!("{base}/request/record"), p, true)),
            );
            pointers.extend(
                r.records
                    .iter()
                    .map(|p| (format!("{base}/request/records"), p, false)),
            );
        }
        if let Some(a) = &op.answer {
            pointers.extend(
                a.record
                    .iter()
                    .map(|p| (format!("{base}/answer/record"), p, true)),
            );
            pointers.extend(
                a.records
                    .iter()
                    .map(|p| (format!("{base}/answer/records"), p, false)),
            );
        }
        for (member, change) in &op.changes {
            if let ChangeDesc::FromRequest(p) = change {
                pointers.push((
                    format!("{base}/changes/{}/from_request", escape(member)),
                    p,
                    false,
                ));
            }
        }
        for (path, pointer, record) in pointers {
            let syntax = if record {
                Pointer::check_record
            } else {
                Pointer::check
            };
            if let Err(e) = syntax(pointer.as_str()) {
                return Some(Refusal {
                    path,
                    code: RefusalCode::Invalid,
                    message: format!("`{pointer}` is not a JSON pointer: {e}"),
                });
            }
        }
    }
    None
}

/// Whether a value deserializes as one entry type.
type Parses = fn(&Value) -> bool;

/// Where `doc` stops being of the [`World`] shape, for a document that does
/// not deserialize: the type or operation that does not, and inside it the
/// deepest member whose removal makes it deserialize. Empty when the
/// document is not an object.
fn shape_error_path(doc: &Value) -> String {
    let Some(obj) = doc.as_object() else {
        return String::new();
    };
    fn parses<T: serde::de::DeserializeOwned>(v: &Value) -> bool {
        serde_json::from_value::<T>(v.clone()).is_ok()
    }
    let sections: [(&str, Parses); 2] = [
        ("types", parses::<TypeDesc>),
        ("operations", parses::<OpDesc>),
    ];
    for (section, ok) in sections {
        let Some(items) = obj.get(section) else {
            continue;
        };
        let Some(items) = items.as_object() else {
            return format!("/{section}");
        };
        for (name, desc) in items {
            if !ok(desc) {
                return format!("/{section}/{}{}", escape(name), culprit(desc, ok));
            }
        }
    }
    if obj.contains_key("version") {
        return "/version".into();
    }
    String::new()
}

/// The most trial parses [`culprit`] makes: each costs a parse of the
/// whole entry, so a bounded number keeps a hostile document cheap.
const CULPRIT_TRIALS: usize = 128;

/// Nodes with more children than this are narrowed by halves first.
const CULPRIT_SCAN: usize = 16;

/// The path, relative to `entry` (which `ok` refuses), of the deepest member
/// whose removal alone makes `ok` accept it; empty when none does.
///
/// A node with many children is narrowed by halves (the half whose removal
/// makes `ok` accept it holds the member) before its children are tried
/// one by one, and at most [`CULPRIT_TRIALS`] removals are tried in all,
/// after which the deepest path found so far is the answer.
fn culprit(entry: &Value, ok: Parses) -> String {
    let mut path: Vec<String> = Vec::new();
    let mut budget = CULPRIT_TRIALS;
    // Whether removing `children` of the node at `path` makes `ok` accept
    // `entry`; `None` once the budget is spent.
    let mut fixes = |path: &[String], children: &[String]| -> Option<bool> {
        budget = budget.checked_sub(1)?;
        let mut candidate = entry.clone();
        remove_at(&mut candidate, path, children);
        Some(ok(&candidate))
    };
    'down: loop {
        let mut node = entry;
        for token in &path {
            let next = match node {
                Value::Object(m) => m.get(token.as_str()),
                Value::Array(a) => token.parse::<usize>().ok().and_then(|i| a.get(i)),
                _ => None,
            };
            match next {
                Some(n) => node = n,
                None => break 'down,
            }
        }
        let mut children: Vec<String> = match node {
            Value::Object(m) => m.keys().cloned().collect(),
            Value::Array(a) => (0..a.len()).map(|i| i.to_string()).collect(),
            _ => break,
        };
        while children.len() > CULPRIT_SCAN {
            let (low, high) = children.split_at(children.len() / 2);
            let half = match fixes(&path, low) {
                Some(true) => low,
                Some(false) => match fixes(&path, high) {
                    Some(true) => high,
                    _ => break 'down,
                },
                None => break 'down,
            };
            children = half.to_vec();
        }
        for child in children {
            match fixes(&path, std::slice::from_ref(&child)) {
                Some(true) => {
                    path.push(child);
                    continue 'down;
                }
                Some(false) => {}
                None => break 'down,
            }
        }
        break;
    }
    path.iter().map(|t| format!("/{}", escape(t))).collect()
}

/// Removes `children` of the node at `path` in `v`.
fn remove_at(v: &mut Value, path: &[String], children: &[String]) {
    let mut node = v;
    for token in path {
        node = match node {
            Value::Object(m) => match m.get_mut(token.as_str()) {
                Some(n) => n,
                None => return,
            },
            Value::Array(a) => match token.parse::<usize>().ok().and_then(|i| a.get_mut(i)) {
                Some(n) => n,
                None => return,
            },
            _ => return,
        };
    }
    match node {
        Value::Object(m) => {
            for child in children {
                m.remove(child.as_str());
            }
        }
        Value::Array(a) => {
            let gone: BTreeSet<usize> = children.iter().filter_map(|c| c.parse().ok()).collect();
            let mut i = 0;
            a.retain(|_| {
                i += 1;
                !gone.contains(&(i - 1))
            });
        }
        _ => {}
    }
}

/// Validates `world` against `bundle` and stores what applies as the next
/// revision of the version: when `expect` is set, only if the newest
/// revision is that one (else a 412).
async fn store_validated(
    st: &AdminState,
    m: &Mock,
    v: &ContractVersion,
    world: &World,
    author: Author,
    created_by: &str,
    expect: Option<i64>,
) -> WorldResult<(WorldRevision, Vec<Refusal>, World)> {
    // A type or operation left with no entry sets nothing.
    let Validation { applied, refused } = trimmed(validate(world, &v.bundle));
    let document = serde_json::to_value(&applied).map_err(StoreError::from)?;
    let added = st
        .store
        .add_world_revision(
            m.id,
            v.version,
            NewWorldRevision {
                author,
                document,
                created_by: Some(created_by.to_string()),
                expect,
            },
        )
        .await;
    match added {
        Ok(r) => Ok((r, refused, applied)),
        Err(StoreError::Conflict(d)) => Err(WorldError::conflict(d)),
        Err(e) => Err(e.into()),
    }
}

/// The overlay to write, built from the newest revision's document (`None`
/// when there is none).
type Build<'a> = &'a (dyn Fn(Option<&WorldRevision>) -> WorldResult<Value> + Send + Sync);

/// Writes the overlay `build` makes from the newest revision as the next
/// revision, checked whole and validated, and audits it as `action`.
///
/// With `If-Match`, the newest revision must be that one, else a 412.
/// Without it the write applies on top of whatever is newest, built again
/// if another write lands in between.
#[allow(clippy::too_many_arguments)]
async fn write(
    st: &AdminState,
    access: &WsAccess,
    m: &Mock,
    v: &ContractVersion,
    author: Author,
    expect: Option<i64>,
    action: &str,
    build: Build<'_>,
) -> WorldResult<(StatusCode, Json<WorldWriteResult>)> {
    let mut attempt = 0;
    let (revision, refused, applied) = loop {
        attempt += 1;
        let latest = st.store.latest_world_revision(m.id, v.version).await?;
        let current = latest.as_ref().map_or(0, |r| r.revision);
        if let Some(n) = expect
            && n != current
        {
            return Err(WorldError::conflict(format!(
                "If-Match names revision {n}, but the newest revision of contract version {} is {current}",
                v.version
            )));
        }
        // Checking a document near the size limit is CPU-bound: keep it
        // off the runtime.
        let doc = build(latest.as_ref())?;
        let world = tokio::task::spawn_blocking(move || check_document(&doc))
            .await
            .map_err(|e| {
                Problem::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("checking the world description did not finish: {e}"),
                )
            })??;
        match store_validated(
            st,
            m,
            v,
            &world,
            author,
            &access.principal.subject,
            Some(current),
        )
        .await
        {
            Ok(done) => break done,
            Err(WorldError::Coded { code, .. })
                if code == REVISION_CONFLICT && expect.is_none() && attempt < WRITE_ATTEMPTS =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    };
    let info = RevisionInfoView::from(WorldRevisionInfo::from(&revision));
    access
        .audit(
            &st.audit,
            action,
            format!("world:{}@{}", m.name, v.version),
            None,
            snapshot(&json!({
                "revision": info.revision,
                "author": info.author,
                "refused": refused.len(),
            })),
        )
        .await;
    let runtime = mounted(st, &access.ws, m, v, revision.revision).await;
    Ok((
        StatusCode::CREATED,
        Json(WorldWriteResult {
            version: v.version,
            revision: revision.revision,
            applied,
            refused,
            runtime,
        }),
    ))
}

#[utoipa::path(
    put,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/world",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = String, Path, description = "Contract version number, or `active`"),
        ("author" = Option<String>, Query, description = "`agent`, `llm` or `user` (default `user`)"),
        ("If-Match" = Option<String>, Header, description = "The revision this write is based on (`0` for none yet); a 412 when it is not the newest"),
    ),
    request_body(content = Object, description = "An overlay world description (`mockingbird world schema`); it replaces the overlay whole", content_type = "application/json"),
    responses(
        (status = 201, description = "Added as the next revision: the entries that validate are stored and apply at once; the refused ones are listed and not stored", body = WorldWriteResult),
        (status = 400, description = "Malformed JSON, an unknown or `heuristic` author, a bad If-Match, or a version that is neither a number nor `active`", body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`revision_conflict`: If-Match is not the newest revision", body = WorldProblem, content_type = "application/problem+json"),
        (status = 413, description = "`too_large`: over 256 KiB", body = WorldProblem, content_type = "application/problem+json"),
        (status = 415, body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`invalid`: nested over 32 deep, not of the format's shape, or a pointer that does not parse; `path` names where", body = WorldProblem, content_type = "application/problem+json"),
    )
)]
pub async fn put_world(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(WorldPath { mock, version }): Path<WorldPath>,
    Query(q): Query<AuthorQuery>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> WorldResult<(StatusCode, Json<WorldWriteResult>)> {
    access.require(Permission::Edit)?;
    require_media(&headers, &["application/json"])?;
    let author = author_of(&q, Author::User)?;
    let expect = if_match(&headers)?;
    let bytes = body_bytes(body)?;
    check_size(&bytes).map_err(WorldError::refused)?;
    let doc = parse_json(&bytes)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = resolve_version(&st, &m, &version).await?;
    let build = move |_: Option<&WorldRevision>| -> WorldResult<Value> { Ok(doc.clone()) };
    write(&st, &access, &m, &v, author, expect, "world.put", &build).await
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/world",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = String, Path, description = "Contract version number, or `active`"),
        ("author" = Option<String>, Query, description = "`agent`, `llm` or `user` (default `user`)"),
        ("If-Match" = Option<String>, Header, description = "The revision this patch is based on (`0` for none yet); a 412 when it is not the newest. Without it the patch applies to the newest."),
    ),
    request_body(content = Object, description = "A JSON merge patch (RFC 7396) of the newest revision's overlay; `null` removes an entry", content_type = "application/merge-patch+json"),
    responses(
        (status = 201, description = "The merged overlay, added as the next revision: the entries that validate are stored and apply at once; the refused ones are listed and not stored", body = WorldWriteResult),
        (status = 400, description = "Malformed JSON, an unknown or `heuristic` author, a bad If-Match, or a version that is neither a number nor `active`", body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`revision_conflict`: If-Match is not the newest revision", body = WorldProblem, content_type = "application/problem+json"),
        (status = 413, description = "`too_large`: the patch or the merged overlay is over 256 KiB", body = WorldProblem, content_type = "application/problem+json"),
        (status = 415, description = "Content-Type is not application/merge-patch+json or application/json", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`invalid`: the merged overlay is nested over 32 deep, not of the format's shape, or has a pointer that does not parse; `path` names where", body = WorldProblem, content_type = "application/problem+json"),
    )
)]
pub async fn patch_world(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(WorldPath { mock, version }): Path<WorldPath>,
    Query(q): Query<AuthorQuery>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> WorldResult<(StatusCode, Json<WorldWriteResult>)> {
    access.require(Permission::Edit)?;
    require_media(&headers, &[MERGE_PATCH_CONTENT_TYPE, "application/json"])?;
    let author = author_of(&q, Author::User)?;
    let expect = if_match(&headers)?;
    let bytes = body_bytes(body)?;
    check_size(&bytes).map_err(WorldError::refused)?;
    let patch = parse_json(&bytes)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = resolve_version(&st, &m, &version).await?;
    let build = move |latest: Option<&WorldRevision>| -> WorldResult<Value> {
        let mut doc = latest.map_or_else(
            || json!({ "version": mb_mcm::world::VERSION }),
            |r| r.document.clone(),
        );
        json_patch::merge(&mut doc, &patch);
        Ok(doc)
    };
    write(&st, &access, &m, &v, author, expect, "world.patch", &build).await
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/world/revisions",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = String, Path, description = "Contract version number, or `active`"),
    ),
    responses(
        (status = 200, description = "The version's world description revisions, newest first", body = Vec<RevisionInfoView>),
        (status = 400, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_revisions(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(WorldPath { mock, version }): Path<WorldPath>,
) -> ApiResult<Json<Vec<RevisionInfoView>>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = resolve_version(&st, &m, &version).await?;
    let list = st.store.list_world_revisions(m.id, v.version).await?;
    Ok(Json(list.into_iter().map(RevisionInfoView::from).collect()))
}

/// Revision `revision` of `v`, a 404 problem when there is none.
async fn revision_of(
    st: &AdminState,
    m: &Mock,
    v: &ContractVersion,
    revision: i64,
) -> ApiResult<WorldRevision> {
    match st.store.get_world_revision(m.id, v.version, revision).await {
        Ok(r) => Ok(r),
        Err(StoreError::NotFound(_)) => Err(Problem::not_found(format!(
            "contract version {} of mock {:?} has no world description revision {revision}",
            v.version, m.name
        ))),
        Err(e) => Err(e.into()),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/world/revisions/{revision}",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = String, Path, description = "Contract version number, or `active`"),
        ("revision" = i64, Path, description = "Revision number"),
    ),
    responses(
        (status = 200, description = "The revision with its overlay document as stored", body = RevisionView),
        (status = 400, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_revision(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(RevisionPath {
        mock,
        version,
        revision,
    }): Path<RevisionPath>,
) -> ApiResult<Json<RevisionView>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = resolve_version(&st, &m, &version).await?;
    Ok(Json(revision_of(&st, &m, &v, revision).await?.into()))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/world/revisions/{revision}/restore",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = String, Path, description = "Contract version number, or `active`"),
        ("revision" = i64, Path, description = "The revision to restore"),
        ("author" = Option<String>, Query, description = "`agent`, `llm` or `user` (default: the restored revision's author, `user` for a `heuristic` one)"),
        ("If-Match" = Option<String>, Header, description = "The newest revision this restore expects; a 412 when it is not"),
    ),
    responses(
        (status = 201, description = "The revision's document, validated again, added as the next revision", body = WorldWriteResult),
        (status = 400, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`revision_conflict`", body = WorldProblem, content_type = "application/problem+json"),
        (status = 415, description = "Content-Type is not application/json", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`invalid`: the stored document is not a world description", body = WorldProblem, content_type = "application/problem+json"),
    )
)]
pub async fn restore_revision(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(RevisionPath {
        mock,
        version,
        revision,
    }): Path<RevisionPath>,
    Query(q): Query<AuthorQuery>,
    headers: HeaderMap,
    _json: RequireJson,
) -> WorldResult<(StatusCode, Json<WorldWriteResult>)> {
    access.require(Permission::Edit)?;
    let expect = if_match(&headers)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = resolve_version(&st, &m, &version).await?;
    let old = revision_of(&st, &m, &v, revision).await?;
    // Only the importer writes tier 0: a heuristic revision restored
    // through the API is the person's.
    let default = match old.author {
        Author::Heuristic => Author::User,
        other => other,
    };
    let author = author_of(&q, default)?;
    let build = move |_: Option<&WorldRevision>| -> WorldResult<Value> { Ok(old.document.clone()) };
    write(
        &st,
        &access,
        &m,
        &v,
        author,
        expect,
        "world.restore",
        &build,
    )
    .await
}

/// Each leaf of `v` (a scalar, or an empty object or array) by its JSON
/// pointer.
fn leaves(v: &Value, prefix: &str, out: &mut BTreeMap<String, Value>) {
    match v {
        Value::Object(m) if !m.is_empty() => {
            for (k, child) in m {
                leaves(child, &format!("{prefix}/{}", escape(k)), out);
            }
        }
        Value::Array(a) if !a.is_empty() => {
            for (i, child) in a.iter().enumerate() {
                leaves(child, &format!("{prefix}/{i}"), out);
            }
        }
        leaf => {
            out.insert(prefix.to_string(), leaf.clone());
        }
    }
}

/// The leaves that differ from `from` to `to`.
fn diff(from: &Value, to: &Value) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (mut a, mut b) = (BTreeMap::new(), BTreeMap::new());
    leaves(from, "", &mut a);
    leaves(to, "", &mut b);
    let added = b.keys().filter(|k| !a.contains_key(*k)).cloned().collect();
    let removed = a.keys().filter(|k| !b.contains_key(*k)).cloned().collect();
    let changed = b
        .iter()
        .filter(|(k, v)| a.get(*k).is_some_and(|old| old != *v))
        .map(|(k, _)| k.clone())
        .collect();
    (added, removed, changed)
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/world/diff",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = String, Path, description = "Contract version number, or `active`"),
        ("from" = Option<i64>, Query, description = "The older revision; `0` is the empty overlay (default: the one before `to`)"),
        ("to" = Option<i64>, Query, description = "The newer revision (default: the newest)"),
    ),
    responses(
        (status = 200, description = "The leaf pointers of the overlay documents added, removed and changed from `from` to `to`", body = WorldDiff),
        (status = 400, body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such mock, version or revision", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn diff_world(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(WorldPath { mock, version }): Path<WorldPath>,
    Query(q): Query<DiffQuery>,
) -> ApiResult<Json<WorldDiff>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = resolve_version(&st, &m, &version).await?;
    let to = match q.to {
        Some(n) => n,
        None => st
            .store
            .latest_world_revision(m.id, v.version)
            .await?
            .map_or(0, |r| r.revision),
    };
    let from = q.from.unwrap_or((to - 1).max(0));
    let mut docs = Vec::with_capacity(2);
    for n in [from, to] {
        docs.push(if n == 0 {
            json!({ "version": mb_mcm::world::VERSION })
        } else {
            revision_of(&st, &m, &v, n).await?.document
        });
    }
    let (added, removed, changed) = diff(&docs[0], &docs[1]);
    Ok(Json(WorldDiff {
        from,
        to,
        added,
        removed,
        changed,
    }))
}
