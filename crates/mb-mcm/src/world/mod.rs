//! World descriptions: how a compiled contract behaves as a world.
//!
//! A [`World`] is a small structured statement about a [`Bundle`](crate::Bundle):
//! which member names a record, where an answer holds it, how lists page,
//! which states a record moves through. The heuristics write a baseline one
//! (tier 0); a coding assistant, Mockingbird's LLM or a person overlay it
//! entry by entry ([`overlay`]), each entry tagged with its [`Author`].
//!
//! Every key is optional, and unknown keys are kept in the `extra` map of the
//! object they appear in instead of being dropped, so a validator can refuse
//! them by name. The published JSON Schema is [`SCHEMA`].
//!
//! ```
//! use mb_mcm::world::{OpKind, World};
//!
//! let world: World = serde_json::from_str(
//!     r#"{"version":1,"operations":{"MoveFolder":{"kind":"action","target":{"path_param":"name"}}}}"#,
//! )?;
//! assert_eq!(world.operations["MoveFolder"].kind, Some(OpKind::Action));
//! # Ok::<(), serde_json::Error>(())
//! ```

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod apply;
mod compiled;
mod effective;
pub mod expr;
mod tier0;
mod validate;

pub use apply::{apply, record_at};
pub use compiled::{
    Expressions, OpExpressions, RefuseEntry, SetEntry, SetValue, TextValue, changes_as_set,
    pointer_expression,
};
pub use effective::{effective, effective_validated};
pub use tier0::{record_pointer, tier0};
pub use validate::{Refusal, RefusalCode, Validation, check_size, validate};

/// The published JSON Schema (draft 2020-12) of a world description.
pub const SCHEMA: &str = include_str!("../../schemas/world.v1.json");

/// The largest world description document accepted, in bytes (256 KiB).
pub const MAX_BYTES: usize = 262_144;

/// The deepest nesting of arrays and objects accepted in a world description.
pub const MAX_DEPTH: usize = 32;

/// The only world description format version.
pub const VERSION: u32 = 1;

/// Keys an object did not define, kept so they can be refused by name.
pub type Extra = BTreeMap<String, Value>;

fn default_version() -> u32 {
    VERSION
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A world description of one contract version.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct World {
    /// The format version; always [`VERSION`] (`1`).
    #[serde(default = "default_version")]
    pub version: u32,
    /// Descriptions of resource types, keyed by resource type (or `defs` key).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub types: BTreeMap<String, TypeDesc>,
    /// Descriptions of operations, keyed by operation id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operations: BTreeMap<String, OpDesc>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for World {
    fn default() -> Self {
        Self {
            version: VERSION,
            types: BTreeMap::new(),
            operations: BTreeMap::new(),
            extra: Extra::new(),
        }
    }
}

/// How records of one resource type behave.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct TypeDesc {
    /// The member that names a record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<IdentityDesc>,
    /// The type a record belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ParentDesc>,
    /// The status member and the states it moves through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<LifecycleDesc>,
    /// Member sets no two records share.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unique: Vec<UniqueDesc>,
    /// The concurrency token of a record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<ConcurrencyDesc>,
    /// Members computed from stored records, keyed by member.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub derived: BTreeMap<String, DerivedDesc>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The member that names a record.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct IdentityDesc {
    /// A dotted path to the member, e.g. `name` or `meta.id`. Absent in an
    /// overlay that sets only `pattern` or `alternates`, which keeps the
    /// lower tier's member; an effective world never holds an identity
    /// without one ([`effective`] refuses it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    /// How the id is embedded in the member's value, holding exactly one
    /// `{id}`, e.g. `folders/{id}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Other members a path value may name a record by, e.g. an alias next to
    /// the id. Absent keeps the lower tier's; `[]` says there are none, which
    /// removes a wrongly guessed alias. A set list replaces the lower tier's
    /// whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alternates: Option<Vec<String>>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The type a record belongs to, and where it names its parent.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct ParentDesc {
    /// The parent's resource type.
    pub r#type: String,
    /// The member naming the parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    /// The path parameter naming the parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_param: Option<String>,
    /// How the parent's id is embedded, holding exactly one `{id}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// The parent's (dotted) array member a record lives in, when records of the
    /// type are held inside their parent rather than on their own, e.g. an
    /// incident's `incident_updates`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded: Option<String>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The status member of a record and the states it moves through.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct LifecycleDesc {
    /// The status member.
    pub member: String,
    /// The states, when the contract lists them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<String>,
    /// The state of a created record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial: Option<String>,
    /// How a delete keeps the record in a state instead of removing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_delete: Option<SoftDelete>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A delete that moves a record to a state instead of removing it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct SoftDelete {
    /// The state of a deleted record.
    pub state: String,
    /// The list parameter and value that include deleted records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed_with: Option<ParamValue>,
    /// The delete parameter and value that remove the record instead, e.g.
    /// `{param: force, value: true}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unless: Option<ParamValue>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A parameter set to a value.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct ParamValue {
    /// The parameter.
    pub param: String,
    /// Its value.
    pub value: Value,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A set of members no two records share.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct UniqueDesc {
    /// The members, together.
    pub members: Vec<String>,
    /// The member within whose value they are unique, e.g. `parent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The declared status of a write that breaks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<u16>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The concurrency token of a record.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct ConcurrencyDesc {
    /// The body member holding the token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    /// The request header carrying it, e.g. `If-Match`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// The declared status of a write with a stale token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<u16>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A member computed exactly from stored records.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DerivedDesc {
    /// The number of child records.
    Count {
        /// The child resource type.
        of: String,
        /// Unknown keys.
        #[serde(flatten)]
        extra: Extra,
    },
    /// The sum of a member over child records.
    Sum {
        /// The child resource type.
        of: String,
        /// The summed member of the child.
        member: String,
        /// Only children whose members equal these values.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        r#where: Option<BTreeMap<String, Value>>,
        /// Unknown keys.
        #[serde(flatten)]
        extra: Extra,
    },
}

/// How one operation acts on the world.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct OpDesc {
    /// What the operation does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<OpKind>,
    /// The resource type it acts on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// Where the request names the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetDesc>,
    /// Where the request body holds the record or records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RecordsAt>,
    /// Where the answer holds the record or records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<AnswerDesc>,
    /// Members an action sets, e.g. `{state: DELETE_REQUESTED}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<BTreeMap<String, Value>>,
    /// Members of the target the operation changes, keyed by member. Read as
    /// [`OpDesc::set`] entries (see [`Expressions`]): `from_request` is the request
    /// body's member, `value` a literal.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub changes: BTreeMap<String, ChangeDesc>,
    /// Members of the record a create, update, replace or action stores, keyed by
    /// member path, each the [expression](expr) its value is.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Refusals checked in order before anything is stored or generated: the first
    /// whose `when` holds answers its status.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refuse: Vec<RefuseDesc>,
    /// The parameter that limits a list to one scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ScopeDesc>,
    /// The filters a list takes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<FilterDesc>,
    /// The order of a list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<OrderDesc>,
    /// How a list pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paging: Option<PagingDesc>,
    /// The request parameters (or body pointers) that ask nothing of a list's
    /// items, e.g. an API version or a detail level.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignores: Vec<String>,
    /// The header that makes a replay answer as the first call did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency: Option<HeaderRef>,
    /// The declared status of a missing record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_found: Option<u16>,
    /// The query parameter that names the members a read or list answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<FieldsDesc>,
    /// The messages an action puts on its target or takes from it, first in
    /// first out (a message queue's publish and get).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<QueueDesc>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The query parameter naming the members a read's record, or each of a list's
/// items, answers (a sparse fieldset: `fields=id,name`, `$select=name`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct FieldsDesc {
    /// The query parameter.
    pub param: String,
    /// What separates its members; `,` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separator: Option<String>,
    /// Members answered whatever it names, e.g. the identity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub always: Vec<String>,
    /// The members answered when the request does not send it (besides
    /// `always`); every member when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Vec<String>>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The messages an action puts on its target record, or takes from it, first in
/// first out.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct QueueDesc {
    /// Whether it puts a message on or takes messages off.
    pub effect: QueueEffect,
    /// With `take`: the request body member saying how many it takes; one when
    /// absent or not sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<Pointer>,
    /// With `take`: the request body member and value(s) that leave the messages
    /// taken on the queue, at its head, e.g. `{at: /ackmode, value: [requeue]}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requeue: Option<RequeueDesc>,
    /// With `take`: the message member set `true` on a message taken before and
    /// left on the queue, `false` on one never taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redelivered: Option<String>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// What a queue action does with its target's messages.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum QueueEffect {
    /// Puts the request's record (its body, or at `request.record`) last.
    Append,
    /// Answers the first messages, oldest first, and removes them.
    Take,
}

/// The request body member, and the value or values of it, that leave taken
/// messages on their queue.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct RequeueDesc {
    /// The request body member.
    pub at: Pointer,
    /// Its value, or an array of the values that requeue.
    pub value: Value,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// What an operation does.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// Stores a new record.
    Create,
    /// Answers one record.
    Read,
    /// Answers many records.
    List,
    /// Changes some members of a record.
    Update,
    /// Replaces a record whole.
    Replace,
    /// Removes a record.
    Delete,
    /// Anything else, e.g. a custom `:move` method.
    Action,
}

impl OpKind {
    /// The kind the contract gives `op`: a query with an identity reads one
    /// record, one without lists; a create, update or delete command is the
    /// same; any other command is an action. A streaming operation has none.
    #[must_use]
    pub fn of(op: &crate::Operation) -> Option<OpKind> {
        use crate::{Effect, OperationKind};
        match op.kind {
            OperationKind::Query if op.identity.is_some() => Some(OpKind::Read),
            OperationKind::Query => Some(OpKind::List),
            OperationKind::Command(Effect::Create) => Some(OpKind::Create),
            OperationKind::Command(Effect::Update) => Some(OpKind::Update),
            OperationKind::Command(Effect::Delete) => Some(OpKind::Delete),
            OperationKind::Command(Effect::Other) => Some(OpKind::Action),
            OperationKind::StreamServer
            | OperationKind::StreamClient
            | OperationKind::StreamBidi => None,
        }
    }
}

/// Where the request names the record an operation acts on.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct TargetDesc {
    /// The path parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_param: Option<String>,
    /// The request body member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Where a request body holds the record or records.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct RecordsAt {
    /// The one record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<Pointer>,
    /// An array of records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub records: Option<Pointer>,
    /// The array of instructions an update's request sends instead of a record
    /// (a semantic patch), and what each kind of instruction changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<InstructionsDesc>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The instructions an update's request body sends, each naming its kind, and the
/// members of the target each kind changes, e.g. `{at: /instructions, kind: kind,
/// kinds: {addTags: {changes: {tags: {append: /values}}}}}`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct InstructionsDesc {
    /// The request body's array of instructions.
    pub at: Pointer,
    /// The member of an instruction that names its kind.
    pub kind: String,
    /// What each kind of instruction does, keyed by kind.
    #[serde(default)]
    pub kinds: BTreeMap<String, InstructionDesc>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// What one kind of instruction does to the target.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct InstructionDesc {
    /// The members it changes, keyed by member. A member's steps may name a
    /// member of the instruction (else of the request body) in braces, whose
    /// value is the step: `environments.{environmentKey}.on`.
    #[serde(default)]
    pub changes: BTreeMap<String, InstructionChange>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// How an instruction changes one member of the target.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InstructionChange {
    /// Set to the instruction's value at this pointer.
    FromInstruction(Pointer),
    /// Set to the request body's value at this pointer.
    FromRequest(Pointer),
    /// Set to this fixed value.
    Value(Value),
    /// Add the instruction's value at this pointer (each item of an array) to the
    /// array member, once.
    Append(Pointer),
    /// Remove the instruction's value at this pointer (each item of an array) from
    /// the array member.
    Remove(Pointer),
}

impl InstructionChange {
    /// The pointer it reads, and whether into the request body (else the
    /// instruction); `None` for a fixed value.
    #[must_use]
    pub fn pointer(&self) -> Option<(&Pointer, bool)> {
        match self {
            Self::FromRequest(p) => Some((p, true)),
            Self::FromInstruction(p) | Self::Append(p) | Self::Remove(p) => Some((p, false)),
            Self::Value(_) => None,
        }
    }
}

/// Where an answer holds the record or records.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct AnswerDesc {
    /// The one record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<Pointer>,
    /// An array of records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub records: Option<Pointer>,
    /// The answer is a long-running operation wrapping the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_running: Option<LongRunning>,
    /// The response header a create names the new record's id in, or `false`
    /// for none. Absent keeps the lower tier's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_header: Option<IdentityHeader>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The response header a create names its id in: a header name, or `false`
/// in JSON for none (which turns off a header a lower tier guessed). `true`
/// is not a value of this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityHeader {
    /// This response header holds the id, e.g. `Resource-ID` or `Location`.
    Named(String),
    /// The create names its id in no header.
    Off,
}

impl IdentityHeader {
    /// The header name, `None` for [`IdentityHeader::Off`].
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Named(h) => Some(h),
            Self::Off => None,
        }
    }
}

impl Serialize for IdentityHeader {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Named(h) => s.serialize_str(h),
            Self::Off => s.serialize_bool(false),
        }
    }
}

impl<'de> Deserialize<'de> for IdentityHeader {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Named(String),
            Flag(bool),
        }
        match Raw::deserialize(d)? {
            Raw::Named(h) => Ok(Self::Named(h)),
            Raw::Flag(false) => Ok(Self::Off),
            Raw::Flag(true) => Err(serde::de::Error::custom(
                "identity_header is a header name or `false`, not `true`",
            )),
        }
    }
}

/// A long-running operation answer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct LongRunning {
    /// The member that says the operation finished.
    pub done: String,
    /// The member holding the result once it has.
    pub result: String,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Where a changed member's new value comes from.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeDesc {
    /// The request body at this pointer.
    FromRequest(Pointer),
    /// This fixed value.
    Value(Value),
}

/// A refusal an operation answers when its condition holds.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RefuseDesc {
    /// The [expression](expr) that refuses the call when it is `true`.
    pub when: String,
    /// The declared status it answers.
    pub status: u16,
    /// The error code its body says, where the error schema has a member for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<TextDesc>,
    /// The message its body says, where the error schema has a member for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<TextDesc>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A text a refusal says: as written, or an [expression](expr)'s value.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum TextDesc {
    /// This text.
    Text(String),
    /// The value of this expression.
    Expr {
        /// The expression.
        expr: String,
    },
}

/// The parameter that limits a list to the records of one scope.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct ScopeDesc {
    /// The request parameter.
    pub param: String,
    /// The record member it matches.
    pub member: String,
    /// How the member matches the parameter; `eq` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ScopeKind>,
    /// With `kind: prefix`, the request parameter whose `true` asks for every
    /// record below the path, not only those directly under it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recursive: Option<String>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// How a scope's member matches its parameter.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    /// The member holds the parameter's value.
    Eq,
    /// The member is a `/`-separated path directly under the parameter's path
    /// (`/a/b` under `/a`; any path under the root, `""` or `/`).
    Prefix,
}

/// A filter a list takes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FilterDesc {
    /// The request parameter.
    pub param: String,
    /// The record member it tests; the parameter's name when absent. Not with a
    /// `grammar`, whose expression names its members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    /// How it tests the member; one of `kind` and `grammar` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<FilterKind>,
    /// The expression grammar the parameter's value is written in: the value is
    /// parsed and its comparisons test the members it names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grammar: Option<FilterGrammar>,
    /// With a `grammar`, the (dotted) member the expression's attribute paths are
    /// below (`metadata` for `metadata[run]=x`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_prefix: Option<String>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The grammar of a filter parameter's expression.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FilterGrammar {
    /// SCIM (RFC 7644 section 3.4.2.2): `userName eq "x" and active eq true`.
    Scim,
    /// OData `$filter`: `status eq 'open' and contains(name,'x')`.
    Odata,
    /// `field:value` pairs joined by commas, each an equality, all of them holding.
    FieldValue,
    /// deepObject brackets below the parameter: `filter[a][b][_gte]=v`.
    DeepObject,
}

/// How a filter tests a member.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FilterKind {
    /// Equal to the parameter.
    Eq,
    /// Contains the parameter.
    Contains,
    /// Greater than or equal to it.
    Gte,
    /// Less than or equal to it.
    Lte,
    /// A boolean switch, e.g. `showDeleted`.
    Flag,
    /// Free-text search.
    Text,
}

/// The order of a list.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct OrderDesc {
    /// The member it sorts by.
    pub member: String,
    /// Largest first.
    #[serde(default, skip_serializing_if = "is_false")]
    pub descending: bool,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// How a list pages.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PagingDesc {
    /// The paging style.
    pub style: PagingStyle,
    /// The request parameter carrying the cursor, page or offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    /// The answer member carrying the next cursor or URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    /// The answer header carrying it, e.g. `Link`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_header: Option<String>,
    /// The request parameter setting the page size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A paging style.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PagingStyle {
    /// An opaque cursor from the previous page.
    Cursor,
    /// A page number.
    Page,
    /// A record offset.
    Offset,
    /// The URL of the next page.
    NextUrl,
    /// A cursor in an answer header.
    HeaderCursor,
}

/// A header.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct HeaderRef {
    /// The header name.
    pub header: String,
    /// Unknown keys.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Who wrote an entry. A later author overrides an earlier one entry by
/// entry, in this order.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    /// The importer's heuristics (tier 0).
    Heuristic,
    /// The user's coding assistant.
    Agent,
    /// Mockingbird's configured LLM.
    Llm,
    /// A person.
    User,
}

impl Author {
    /// The serialized name, e.g. `heuristic`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Heuristic => "heuristic",
            Self::Agent => "agent",
            Self::Llm => "llm",
            Self::User => "user",
        }
    }
}

impl fmt::Display for Author {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An RFC 6901 JSON pointer into a request or answer, e.g. `/result/0`.
///
/// It deserializes from any string, so one document with a bad pointer still
/// loads and the validator can refuse that entry by path;
/// [`Pointer::parse`] checks the syntax.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, Default)]
#[serde(transparent)]
pub struct Pointer(String);

/// Why a string is not a [`Pointer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerError {
    /// The string is empty; the whole document is not a pointer here.
    Empty,
    /// The string does not start with `/`.
    NoLeadingSlash,
    /// A `~` is not followed by `0` or `1`, at this byte offset.
    BadEscape(usize),
}

impl fmt::Display for PointerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a JSON pointer may not be empty"),
            Self::NoLeadingSlash => f.write_str("a JSON pointer starts with `/`"),
            Self::BadEscape(at) => write!(
                f,
                "`~` at byte {at} is not followed by `0` or `1` (RFC 6901 escapes)"
            ),
        }
    }
}

impl std::error::Error for PointerError {}

impl Pointer {
    /// Parses an RFC 6901 pointer, refusing the empty string, strings not
    /// starting with `/`, and `~` escapes other than `~0` and `~1`.
    pub fn parse(s: &str) -> Result<Pointer, PointerError> {
        Self::check(s)?;
        Ok(Pointer(s.to_owned()))
    }

    /// Checks this pointer's syntax, as [`Pointer::parse`] does.
    pub fn check(s: &str) -> Result<(), PointerError> {
        if s.is_empty() {
            return Err(PointerError::Empty);
        }
        if !s.starts_with('/') {
            return Err(PointerError::NoLeadingSlash);
        }
        let bytes = s.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'~' && !matches!(bytes.get(i + 1), Some(b'0' | b'1')) {
                return Err(PointerError::BadEscape(i));
            }
        }
        Ok(())
    }

    /// Checks a record pointer's syntax (`answer.record`, `request.record`): the
    /// root pointer `""`, which says the whole answer (or request body) is the
    /// record, or any pointer [`Pointer::check`] accepts.
    pub fn check_record(s: &str) -> Result<(), PointerError> {
        if s.is_empty() {
            return Ok(());
        }
        Self::check(s)
    }

    /// Whether this is the root pointer `""`, the whole document.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The pointer text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The unescaped reference tokens, e.g. `["result", "0"]`.
    pub fn tokens(&self) -> Vec<String> {
        self.0
            .split('/')
            .skip(1)
            .map(|t| t.replace("~1", "/").replace("~0", "~"))
            .collect()
    }
}

impl fmt::Display for Pointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Overlays `top` on `base` entry by entry: each type and operation of `top`
/// is merged into `base`'s field by field, a field set in `top` replacing
/// `base`'s. A type's `identity` and an operation's `answer` merge field by
/// field too, so `answer: {identity_header: ..}` keeps the lower tier's
/// `answer.record`. An operation's `target` replaces `base`'s whole: a member
/// `top` leaves out is not inherited, so `target: {member: ..}` drops the
/// lower tier's `path_param` (a `top` that makes an operation a create
/// without naming a target keeps `base`'s here; [`effective`] then drops what
/// the route does not name the new record by). Other sub-objects, vectors and
/// maps replace whole (`identity.alternates` among them); unknown keys merge
/// key by key.
pub fn overlay(base: &World, top: &World) -> World {
    let mut out = base.clone();
    for (name, t) in &top.types {
        let merged = match out.types.remove(name) {
            Some(b) => overlay_type(b, t),
            None => t.clone(),
        };
        out.types.insert(name.clone(), merged);
    }
    for (id, op) in &top.operations {
        let merged = match out.operations.remove(id) {
            Some(b) => overlay_op(b, op),
            None => op.clone(),
        };
        out.operations.insert(id.clone(), merged);
    }
    merge_extra(&mut out.extra, &top.extra);
    out
}

fn set<T: Clone>(field: &mut Option<T>, top: &Option<T>) {
    if top.is_some() {
        field.clone_from(top);
    }
}

fn set_vec<T: Clone>(field: &mut Vec<T>, top: &[T]) {
    if !top.is_empty() {
        *field = top.to_vec();
    }
}

fn set_map<V: Clone>(field: &mut BTreeMap<String, V>, top: &BTreeMap<String, V>) {
    if !top.is_empty() {
        field.clone_from(top);
    }
}

fn merge_extra(field: &mut Extra, top: &Extra) {
    for (k, v) in top {
        field.insert(k.clone(), v.clone());
    }
}

/// Merges `top` into `field` by `overlay_fields`, or takes `top` when `field`
/// has none.
fn merge_sub<T: Clone>(field: &mut Option<T>, top: &Option<T>, overlay_fields: fn(&mut T, &T)) {
    match (field.as_mut(), top) {
        (Some(b), Some(t)) => overlay_fields(b, t),
        (None, Some(_)) => field.clone_from(top),
        (_, None) => {}
    }
}

fn overlay_identity(b: &mut IdentityDesc, t: &IdentityDesc) {
    set(&mut b.member, &t.member);
    set(&mut b.pattern, &t.pattern);
    set(&mut b.alternates, &t.alternates);
    merge_extra(&mut b.extra, &t.extra);
}

fn overlay_answer(b: &mut AnswerDesc, t: &AnswerDesc) {
    set(&mut b.record, &t.record);
    set(&mut b.records, &t.records);
    set(&mut b.long_running, &t.long_running);
    set(&mut b.identity_header, &t.identity_header);
    merge_extra(&mut b.extra, &t.extra);
}

fn overlay_type(mut b: TypeDesc, t: &TypeDesc) -> TypeDesc {
    merge_sub(&mut b.identity, &t.identity, overlay_identity);
    set(&mut b.parent, &t.parent);
    set(&mut b.lifecycle, &t.lifecycle);
    set_vec(&mut b.unique, &t.unique);
    set(&mut b.concurrency, &t.concurrency);
    set_map(&mut b.derived, &t.derived);
    merge_extra(&mut b.extra, &t.extra);
    b
}

fn overlay_op(mut b: OpDesc, t: &OpDesc) -> OpDesc {
    // A target names the record whole: a lower tier's path parameter kept next to
    // an authored member would name another record (a create under a parent keyed
    // by the parent).
    if t.target.is_some() {
        b.target.clone_from(&t.target);
    }
    set(&mut b.kind, &t.kind);
    set(&mut b.r#type, &t.r#type);
    set(&mut b.request, &t.request);
    merge_sub(&mut b.answer, &t.answer, overlay_answer);
    set(&mut b.transition, &t.transition);
    set_map(&mut b.changes, &t.changes);
    set_map(&mut b.set, &t.set);
    set_vec(&mut b.refuse, &t.refuse);
    set(&mut b.scope, &t.scope);
    set_vec(&mut b.filters, &t.filters);
    set(&mut b.order, &t.order);
    set(&mut b.paging, &t.paging);
    set_vec(&mut b.ignores, &t.ignores);
    set(&mut b.idempotency, &t.idempotency);
    set(&mut b.not_found, &t.not_found);
    set(&mut b.fields, &t.fields);
    set(&mut b.queue, &t.queue);
    merge_extra(&mut b.extra, &t.extra);
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SPEC_EXCERPT: &str = include_str!("../../tests/fixtures/world-spec-excerpt.json");

    #[test]
    fn world_roundtrips_the_spec_excerpt() {
        let world: World = serde_json::from_str(SPEC_EXCERPT).unwrap();
        let text = serde_json::to_string(&world).unwrap();
        let again: World = serde_json::from_str(&text).unwrap();
        assert_eq!(world, again);
        assert_eq!(world.operations["MoveFolder"].kind, Some(OpKind::Action));
        assert!(world.extra.is_empty());
        assert!(world.types["folder"].extra.is_empty());
        assert!(world.operations.values().all(|op| op.extra.is_empty()));
        // Serializing writes back exactly what was read: nothing is added.
        let original: serde_json::Value = serde_json::from_str(SPEC_EXCERPT).unwrap();
        assert_eq!(serde_json::to_value(&world).unwrap(), original);
    }

    #[test]
    fn unknown_keys_are_kept_in_extra() {
        let world: World =
            serde_json::from_value(json!({"version":1,"types":{"t":{"identty":{}}}})).unwrap();
        assert!(world.types["t"].extra.contains_key("identty"));
        assert!(world.types["t"].identity.is_none());

        let world: World = serde_json::from_value(json!({
            "version": 1,
            "bogus": 3,
            "operations": {"o": {"kindd": "read", "target": {"path_parm": "id"}}},
            "types": {"t": {"derived": {"n": {"kind": "count", "of": "c", "extra_key": 1}}}}
        }))
        .unwrap();
        assert!(world.extra.contains_key("bogus"));
        assert!(world.operations["o"].extra.contains_key("kindd"));
        let target = world.operations["o"].target.as_ref().unwrap();
        assert!(target.extra.contains_key("path_parm"));
        match &world.types["t"].derived["n"] {
            DerivedDesc::Count { of, extra } => {
                assert_eq!(of, "c");
                assert!(extra.contains_key("extra_key"));
            }
            other => panic!("expected a count, got {other:?}"),
        }
    }

    #[test]
    fn version_defaults_to_one() {
        let world: World = serde_json::from_value(json!({})).unwrap();
        assert_eq!(world.version, 1);
        assert_eq!(World::default().version, 1);
    }

    #[test]
    fn overlay_replaces_set_fields_only() {
        let base: World = serde_json::from_value(json!({
            "types": {
                "folder": {
                    "identity": {"member": "id"},
                    "lifecycle": {"member": "state", "states": ["ACTIVE", "GONE"]},
                    "unique": [{"members": ["a"]}, {"members": ["b"]}]
                },
                "other": {"identity": {"member": "key"}}
            },
            "operations": {
                "GetFolder": {"kind": "read", "type": "folder", "target": {"path_param": "id"}}
            }
        }))
        .unwrap();
        let top: World = serde_json::from_value(json!({
            "types": {
                "folder": {
                    "identity": {"member": "name"},
                    "unique": [{"members": ["c"]}]
                },
                "fresh": {"identity": {"member": "uid"}}
            },
            "operations": {
                "GetFolder": {"not_found": 404},
                "ListFolders": {"kind": "list"}
            }
        }))
        .unwrap();
        let out = overlay(&base, &top);

        let folder = &out.types["folder"];
        assert_eq!(
            folder.identity.as_ref().unwrap().member.as_deref(),
            Some("name")
        );
        assert_eq!(folder.lifecycle, base.types["folder"].lifecycle);
        // Vectors replace whole.
        assert_eq!(folder.unique.len(), 1);
        assert_eq!(folder.unique[0].members, vec!["c".to_owned()]);
        assert_eq!(out.types["other"], base.types["other"]);
        assert_eq!(out.types["fresh"], top.types["fresh"]);

        let get = &out.operations["GetFolder"];
        assert_eq!(get.kind, Some(OpKind::Read));
        assert_eq!(get.r#type.as_deref(), Some("folder"));
        assert_eq!(get.not_found, Some(404));
        assert_eq!(out.operations["ListFolders"].kind, Some(OpKind::List));
    }

    #[test]
    fn overlay_keeps_base_answer_members_the_top_leaves_out() {
        let base: World = serde_json::from_value(
            json!({"operations": {"createZone": {"answer": {"record": "/data"}}}}),
        )
        .unwrap();
        let top: World = serde_json::from_value(
            json!({"operations": {"createZone": {"answer": {"identity_header": "Resource-ID"}}}}),
        )
        .unwrap();
        let out = overlay(&base, &top);
        assert_eq!(
            serde_json::to_value(&out.operations["createZone"].answer).unwrap(),
            json!({"record": "/data", "identity_header": "Resource-ID"})
        );
    }

    #[test]
    fn overlay_identity_header_false_replaces_the_base_header() {
        let base: World = serde_json::from_value(
            json!({"operations": {"c": {"answer": {"identity_header": "Resource-ID"}}}}),
        )
        .unwrap();
        let off: World = serde_json::from_value(
            json!({"operations": {"c": {"answer": {"identity_header": false}}}}),
        )
        .unwrap();
        let header = |w: &World| {
            w.operations["c"]
                .answer
                .as_ref()
                .and_then(|a| a.identity_header.clone())
        };
        assert_eq!(header(&overlay(&base, &off)), Some(IdentityHeader::Off));
        let silent: World =
            serde_json::from_value(json!({"operations": {"c": {"answer": {"record": "/x"}}}}))
                .unwrap();
        assert_eq!(
            header(&overlay(&base, &silent)),
            Some(IdentityHeader::Named("Resource-ID".into()))
        );
    }

    #[test]
    fn overlay_keeps_base_identity_member_when_only_alternates_are_set() {
        // A revision that sets only alternates deserializes as a World.
        let top: World = serde_json::from_value(
            json!({"types": {"template": {"identity": {"alternates": ["Alias"]}}}}),
        )
        .unwrap();
        let base: World = serde_json::from_value(
            json!({"types": {"template": {"identity": {"member": "TemplateId", "pattern": "t/{id}"}}}}),
        )
        .unwrap();
        let out = overlay(&base, &top);
        assert_eq!(
            serde_json::to_value(&out.types["template"].identity).unwrap(),
            json!({"member": "TemplateId", "pattern": "t/{id}", "alternates": ["Alias"]})
        );
        // A set list replaces the lower tier's whole; `[]` is kept as set.
        let none: World = serde_json::from_value(
            json!({"types": {"template": {"identity": {"alternates": []}}}}),
        )
        .unwrap();
        let out = overlay(&out, &none);
        let id = out.types["template"].identity.as_ref().unwrap();
        assert_eq!(id.alternates, Some(vec![]));
        assert_eq!(id.member.as_deref(), Some("TemplateId"));
    }

    #[test]
    fn an_authored_target_replaces_the_base_target_whole() {
        // Tier 0 took the parent collection's segment for the create's target; the
        // author names the new record by a body member. Nothing of tier 0's stays.
        let base: World = serde_json::from_value(json!({"operations": {"createThing": {
            "kind": "create", "type": "things", "target": {"path_param": "parentKey"}
        }}}))
        .unwrap();
        let top: World = serde_json::from_value(json!({"operations": {"createThing": {
            "target": {"member": "key"}
        }}}))
        .unwrap();
        let op = &overlay(&base, &top).operations["createThing"];
        assert_eq!(
            op.target,
            Some(TargetDesc {
                member: Some("key".into()),
                ..TargetDesc::default()
            })
        );
        assert_eq!(op.kind, Some(OpKind::Create));
        // A top that sets no target keeps the base's.
        let none: World =
            serde_json::from_value(json!({"operations": {"createThing": {"type": "things"}}}))
                .unwrap();
        assert_eq!(
            overlay(&base, &none).operations["createThing"].target,
            base.operations["createThing"].target
        );
    }

    #[test]
    fn overlay_keeps_the_base_target_of_a_kind_changed_to_create() {
        // Whether the route names the new record is the bundle's to say: the
        // overlay keeps the base target, and `effective` decides.
        let base: World = serde_json::from_value(json!({"operations": {"o": {
            "kind": "action", "target": {"path_param": "parentKey"}
        }}}))
        .unwrap();
        for kind in ["create", "update"] {
            let top: World =
                serde_json::from_value(json!({"operations": {"o": {"kind": kind}}})).unwrap();
            assert_eq!(
                overlay(&base, &top).operations["o"].target,
                base.operations["o"].target
            );
        }
    }

    #[test]
    fn pointer_parse_rejects_bad_pointers() {
        for bad in ["", "a", "/a~2", "/a~", "Contacts/x"] {
            assert!(Pointer::parse(bad).is_err(), "{bad:?} should be refused");
        }
        for good in ["/Contacts", "/result/0", "/a~0b~1c", "/"] {
            let p = Pointer::parse(good).unwrap();
            assert_eq!(p.as_str(), good);
        }
        assert_eq!(
            Pointer::parse("/a~1b/~0/0").unwrap().tokens(),
            vec!["a/b".to_owned(), "~".to_owned(), "0".to_owned()]
        );
    }

    #[test]
    fn check_record_accepts_the_root_pointer_only_there() {
        assert_eq!(Pointer::check(""), Err(PointerError::Empty));
        assert_eq!(Pointer::check_record(""), Ok(()));
        assert_eq!(Pointer::check_record("/data/0"), Ok(()));
        assert_eq!(
            Pointer::check_record("data"),
            Err(PointerError::NoLeadingSlash)
        );
        let root: Pointer = serde_json::from_value(json!("")).unwrap();
        assert!(root.is_root());
        assert!(root.tokens().is_empty());
        assert!(!Pointer::parse("/a").unwrap().is_root());
    }

    #[test]
    fn author_and_kinds_are_snake_case() {
        assert_eq!(serde_json::to_value(Author::Llm).unwrap(), json!("llm"));
        assert_eq!(
            serde_json::to_value(PagingStyle::HeaderCursor).unwrap(),
            json!("header_cursor")
        );
        assert_eq!(
            serde_json::to_value(PagingStyle::NextUrl).unwrap(),
            json!("next_url")
        );
        assert_eq!(
            serde_json::to_value(OpKind::Replace).unwrap(),
            json!("replace")
        );
        assert_eq!(serde_json::to_value(FilterKind::Gte).unwrap(), json!("gte"));
        assert_eq!(
            serde_json::to_value(FilterGrammar::DeepObject).unwrap(),
            json!("deep_object")
        );
        assert_eq!(
            serde_json::to_value(FilterGrammar::FieldValue).unwrap(),
            json!("field_value")
        );
    }
}
