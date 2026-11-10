//! Validating a world description against the bundle it describes.
//!
//! [`validate`] checks every entry of a [`World`] against a [`Bundle`]: that
//! operations and types exist, that members are properties of the type's
//! record schema, that parameters, headers and statuses are declared, and that
//! pointers land inside the request or answer schema. An entry that doesn't
//! resolve is dropped from [`Validation::applied`] and listed once in
//! [`Validation::refused`], with a JSON pointer into the description naming
//! it; the rest applies. [`check_size`] bounds a document before it is parsed.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    AnswerDesc, ChangeDesc, ConcurrencyDesc, DerivedDesc, Extra, FieldsDesc, FilterDesc,
    FilterGrammar, HeaderRef, IdentityDesc, IdentityHeader, InstructionDesc, InstructionsDesc,
    LifecycleDesc, LongRunning, MAX_BYTES, MAX_DEPTH, OpDesc, OpKind, OrderDesc, PagingDesc,
    ParamValue, ParentDesc, Pointer, QueueDesc, QueueEffect, RecordsAt, RequeueDesc, ScopeDesc,
    ScopeKind, SoftDelete, TargetDesc, TypeDesc, UniqueDesc, VERSION, World,
};
use crate::{Binding, Bundle, Operation, RecordAt, StatusSelector};

/// What [`validate`] kept of a description and what it refused.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Validation {
    /// The description with every refused entry (and every unknown key) left
    /// out.
    pub applied: World,
    /// The refused entries, each once, in document order.
    pub refused: Vec<Refusal>,
}

/// One refused entry of a description.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// A JSON pointer into the description naming the entry, e.g.
    /// `/operations/MoveFolder/target/path_param`. Empty for the whole
    /// document (a [`RefusalCode::TooLarge`] refusal).
    pub path: String,
    /// Why it was refused.
    pub code: RefusalCode,
    /// What is wrong, for a person or an assistant to fix.
    pub message: String,
}

/// Why an entry was refused.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// A key the description format doesn't define at that place.
    UnknownKey,
    /// It names an operation, type, member, parameter, header, status or
    /// schema location the contract doesn't have.
    Unresolved,
    /// It doesn't apply to the operation's protocol, e.g. a header on a
    /// GraphQL operation.
    NotApplicable,
    /// It is malformed: a pointer that doesn't parse, a pattern without
    /// exactly one `{id}`, a state not among the states, a document nested
    /// too deep.
    Invalid,
    /// The whole document is over [`MAX_BYTES`] ([`check_size`]); its path
    /// is empty.
    TooLarge,
}

impl RefusalCode {
    /// The serialized name, e.g. `unknown_key`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownKey => "unknown_key",
            Self::Unresolved => "unresolved",
            Self::NotApplicable => "not_applicable",
            Self::Invalid => "invalid",
            Self::TooLarge => "too_large",
        }
    }
}

impl fmt::Display for RefusalCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}: {}", self.code, self.message)
        } else {
            write!(f, "{} at {}: {}", self.code, self.path, self.message)
        }
    }
}

/// Checks a description document's raw bytes before it is parsed: at most
/// [`MAX_BYTES`] long, and arrays and objects nested at most [`MAX_DEPTH`]
/// deep. A too-large document is refused as [`RefusalCode::TooLarge`] with an
/// empty path; a too-deep one as [`RefusalCode::Invalid`], naming the path of
/// the first array or object past the limit. It does not check that the bytes
/// are JSON.
pub fn check_size(bytes: &[u8]) -> Result<(), Refusal> {
    if bytes.len() > MAX_BYTES {
        return Err(Refusal {
            path: String::new(),
            code: RefusalCode::TooLarge,
            message: format!(
                "the description is {} bytes; at most {MAX_BYTES} are accepted",
                bytes.len()
            ),
        });
    }
    check_depth(bytes)
}

/// Where a scan of a JSON document is, one frame per open array or object.
enum Frame {
    Object { key: Option<String>, want_key: bool },
    Array { index: usize },
}

fn check_depth(bytes: &[u8]) -> Result<(), Refusal> {
    let mut stack: Vec<Frame> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                let end = i.min(bytes.len().saturating_sub(1));
                if let Some(Frame::Object { key, want_key }) = stack.last_mut()
                    && *want_key
                {
                    let raw = bytes.get(start..=end).unwrap_or_default();
                    let text = String::from_utf8_lossy(raw);
                    *key = Some(
                        serde_json::from_str::<String>(&text)
                            .unwrap_or_else(|_| text.trim_matches('"').to_owned()),
                    );
                    *want_key = false;
                }
            }
            open @ (b'{' | b'[') => {
                if stack.len() >= MAX_DEPTH {
                    let mut path = Path::root();
                    for frame in &stack {
                        path = match frame {
                            Frame::Object { key, .. } => path.join(key.as_deref().unwrap_or("")),
                            Frame::Array { index } => path.join(index),
                        };
                    }
                    return Err(Refusal {
                        path: path.0,
                        code: RefusalCode::Invalid,
                        message: format!("arrays and objects nest more than {MAX_DEPTH} deep here"),
                    });
                }
                stack.push(if open == b'{' {
                    Frame::Object {
                        key: None,
                        want_key: true,
                    }
                } else {
                    Frame::Array { index: 0 }
                });
            }
            b'}' | b']' => {
                stack.pop();
            }
            b',' => match stack.last_mut() {
                Some(Frame::Array { index }) => *index += 1,
                Some(Frame::Object { want_key, .. }) => *want_key = true,
                None => {}
            },
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

/// A JSON pointer being built, token by token.
#[derive(Clone, Debug)]
struct Path(String);

impl Path {
    fn root() -> Self {
        Path(String::new())
    }

    fn join(&self, token: impl fmt::Display) -> Path {
        let token = token.to_string().replace('~', "~0").replace('/', "~1");
        Path(format!("{}/{token}", self.0))
    }
}

/// Validates `world` against `bundle`, keeping the entries that resolve and
/// refusing the rest by path. It never fails: a description that resolves
/// nowhere is an empty [`Validation::applied`] with every entry refused.
///
/// The record schema of a type, which its members must be properties of, is
/// the success answer of the type's read operation (else its first
/// operation with an identity, else its first operation), taken at that
/// operation's `answer.record` (or the item of `answer.records`, or the
/// bundle's own record location) and through
/// `$ref`s into the bundle's definitions; a type that is only a definition's
/// name uses that definition. A member is a dotted path through nested
/// properties, looking through `allOf`, `anyOf` and `oneOf`.
pub fn validate(world: &World, bundle: &Bundle) -> Validation {
    let mut cx = Ctx::new(world, bundle);
    let root = Path::root();
    cx.unknown(&root, &world.extra);
    if world.version != VERSION {
        cx.refuse(
            &root.join("version"),
            RefusalCode::Invalid,
            format!(
                "version {} is not {VERSION}, the only format version",
                world.version
            ),
        );
    }
    let mut applied = World::default();

    let types = root.join("types");
    for (name, t) in &world.types {
        let path = types.join(name);
        if !cx.types.contains(name.as_str()) {
            cx.refuse(
                &path,
                RefusalCode::Unresolved,
                format!("no operation has resource type `{name}` and no definition is named so"),
            );
            continue;
        }
        let desc = cx.type_desc(name, t, &path);
        applied.types.insert(name.clone(), desc);
    }
    // Derived members name child types by their parent, so they are checked
    // once every type's parent is.
    let names: Vec<String> = applied.types.keys().cloned().collect();
    for name in names {
        let Some(t) = world.types.get(&name) else {
            continue;
        };
        let derived = cx.derived(
            &name,
            &t.derived,
            &applied,
            &types.join(&name).join("derived"),
        );
        if let Some(desc) = applied.types.get_mut(&name) {
            desc.derived = derived;
        }
    }

    let operations = root.join("operations");
    for (id, d) in &world.operations {
        let path = operations.join(id);
        let Some(op) = bundle.operation(id) else {
            cx.refuse(
                &path,
                RefusalCode::Unresolved,
                format!("the contract has no operation `{id}`"),
            );
            continue;
        };
        let desc = cx.op_desc(op, d, &applied, &path);
        applied.operations.insert(id.clone(), desc);
    }

    Validation {
        applied,
        refused: cx.refused,
    }
}

/// How far `$ref`s, `allOf`s and nested lookups are followed.
const MAX_HOPS: usize = 16;

struct Ctx<'a> {
    bundle: &'a Bundle,
    world: &'a World,
    types: BTreeSet<&'a str>,
    refused: Vec<Refusal>,
}

impl<'a> Ctx<'a> {
    fn new(world: &'a World, bundle: &'a Bundle) -> Self {
        let types = bundle
            .operations
            .iter()
            .filter_map(|o| o.resource_type.as_deref())
            .chain(bundle.defs.keys().map(String::as_str))
            .collect();
        Self {
            bundle,
            world,
            types,
            refused: Vec::new(),
        }
    }

    fn refuse(&mut self, path: &Path, code: RefusalCode, message: impl Into<String>) {
        self.refused.push(Refusal {
            path: path.0.clone(),
            code,
            message: message.into(),
        });
    }

    fn unknown(&mut self, path: &Path, extra: &Extra) {
        for key in extra.keys() {
            self.refuse(
                &path.join(key),
                RefusalCode::UnknownKey,
                format!("`{key}` is not a key of a world description here"),
            );
        }
    }

    // ---- what the bundle and the description say about operations ----

    /// The operation's type: the description's, when it names a known type,
    /// else the contract's.
    fn op_type(&self, op: &'a Operation) -> Option<&'a str> {
        self.world
            .operations
            .get(&op.id)
            .and_then(|d| d.r#type.as_deref())
            .filter(|t| self.types.contains(t))
            .or(op.resource_type.as_deref())
    }

    /// The operation's kind: the description's, else the contract's.
    fn op_kind(&self, op: &Operation) -> Option<OpKind> {
        self.world
            .operations
            .get(&op.id)
            .and_then(|d| d.kind)
            .or_else(|| OpKind::of(op))
    }

    fn ops_of(&self, t: &str) -> Vec<&'a Operation> {
        self.bundle
            .operations
            .iter()
            .filter(|o| self.op_type(o) == Some(t))
            .collect()
    }

    fn writes_of(&self, t: &str) -> Vec<&'a Operation> {
        self.ops_of(t)
            .into_iter()
            .filter(|o| {
                matches!(
                    self.op_kind(o),
                    Some(
                        OpKind::Create
                            | OpKind::Update
                            | OpKind::Replace
                            | OpKind::Delete
                            | OpKind::Action
                    )
                )
            })
            .collect()
    }

    /// The schema of one record of type `t`.
    fn record_schema(&self, t: &str) -> Option<&'a Value> {
        let ops = self.ops_of(t);
        let source = ops
            .iter()
            .find(|o| self.op_kind(o) == Some(OpKind::Read))
            .or_else(|| ops.iter().find(|o| o.identity.is_some()))
            .or(ops.first());
        source
            .and_then(|op| self.answer_record(op))
            .or_else(|| self.bundle.defs.get(t))
    }

    /// The schema of the record an operation's success answer holds.
    fn answer_record(&self, op: &'a Operation) -> Option<&'a Value> {
        let b = self.bundle;
        let answer = success_schema(op)?;
        let desc = self
            .world
            .operations
            .get(&op.id)
            .and_then(|d| d.answer.as_ref());
        let at = |p: &Option<Pointer>, root_ok: bool| {
            p.as_ref()
                .filter(|p| {
                    let syntax = if root_ok {
                        Pointer::check_record
                    } else {
                        Pointer::check
                    };
                    syntax(p.as_str()).is_ok()
                })
                .and_then(|p| at_pointer(b, answer, &p.tokens()))
        };
        let record = match desc {
            Some(d) if at(&d.record, true).is_some() => at(&d.record, true),
            // Only a `records` pointer `check_pointer` would accept: one on
            // an array whose items resolve. A refused one leaves the lookup
            // to the bundle's own record location.
            Some(d)
                if at(&d.records, false)
                    .filter(|s| is_array(b, s))
                    .and_then(|s| items(b, s))
                    .is_some() =>
            {
                at(&d.records, false).and_then(|s| items(b, s))
            }
            _ => match op.identity.as_ref().and_then(|i| i.record.as_ref()) {
                None => Some(answer),
                Some(RecordAt::Member(m)) => property(b, answer, m),
                Some(RecordAt::Item) => items(b, answer),
                Some(RecordAt::MemberItem(m)) => property(b, answer, m).and_then(|s| items(b, s)),
                Some(RecordAt::Pointer(tokens)) => at_pointer(b, answer, tokens),
            },
        }?;
        if is_array(b, record) {
            items(b, record)
        } else {
            Some(record)
        }
    }

    // ---- checks; each refuses what fails and says whether it passed ----

    fn check_member(
        &mut self,
        record: Option<&'a Value>,
        t: Option<&str>,
        member: &str,
        path: &Path,
    ) -> bool {
        let Some(t) = t else {
            self.refuse(
                path,
                RefusalCode::Unresolved,
                format!("`{member}` names a member, but the operation has no resource type"),
            );
            return false;
        };
        let Some(record) = record else {
            self.refuse(
                path,
                RefusalCode::Unresolved,
                format!("type `{t}` has no record schema to find `{member}` in"),
            );
            return false;
        };
        if dotted_member(self.bundle, record, member).is_some() {
            return true;
        }
        self.refuse(
            path,
            RefusalCode::Unresolved,
            format!("`{member}` is not a member of a `{t}` record"),
        );
        false
    }

    fn check_pattern(&mut self, pattern: &str, path: &Path) -> bool {
        let n = pattern.matches("{id}").count();
        if n == 1 {
            return true;
        }
        self.refuse(
            path,
            RefusalCode::Invalid,
            format!("pattern `{pattern}` holds {n} `{{id}}`; it must hold exactly one"),
        );
        false
    }

    fn check_state(&mut self, states: &[String], state: &str, path: &Path) -> bool {
        if states.is_empty() || states.iter().any(|s| s == state) {
            return true;
        }
        self.refuse(
            path,
            RefusalCode::Invalid,
            format!("`{state}` is not one of the lifecycle's states {states:?}"),
        );
        false
    }

    /// Whether filter `f` sets one of `kind` and `grammar`, a `member` only with a
    /// `kind` and a `member_prefix` only with a `grammar`; refuses it otherwise.
    fn check_filter_shape(&mut self, f: &FilterDesc, path: &Path) -> bool {
        let problem = match (f.kind, f.grammar) {
            (None, None) => Some("a filter sets `kind` or `grammar`"),
            (Some(_), Some(_)) => Some("a filter sets `kind` or `grammar`, not both"),
            (_, Some(_)) if f.member.is_some() => Some(
                "a `grammar` filter's expression names its members: set `member_prefix`, not `member`",
            ),
            (Some(_), _) if f.member_prefix.is_some() => {
                Some("`member_prefix` applies to a `grammar` filter only")
            }
            _ => None,
        };
        match problem {
            Some(message) => {
                self.refuse(path, RefusalCode::Invalid, message.to_string());
                false
            }
            None => true,
        }
    }

    fn check_param(&mut self, op: &Operation, param: &str, path: &Path) -> bool {
        if declares_param(self.bundle, op, param) {
            return true;
        }
        self.refuse(
            path,
            RefusalCode::Unresolved,
            format!("operation `{}` declares no parameter `{param}`", op.id),
        );
        false
    }

    fn check_path_param(&mut self, op: &Operation, param: &str, path: &Path) -> bool {
        if !is_http(op) {
            self.refuse(
                path,
                RefusalCode::NotApplicable,
                format!(
                    "operation `{}` is not an HTTP operation and has no path",
                    op.id
                ),
            );
            return false;
        }
        if declares_path_param(self.bundle, op, param) {
            return true;
        }
        self.refuse(
            path,
            RefusalCode::Unresolved,
            format!("operation `{}` declares no path parameter `{param}`", op.id),
        );
        false
    }

    fn check_header(&mut self, op: &Operation, header: &str, path: &Path) -> bool {
        if !is_http(op) {
            self.refuse(
                path,
                RefusalCode::NotApplicable,
                format!(
                    "operation `{}` is not an HTTP operation and has no headers",
                    op.id
                ),
            );
            return false;
        }
        if declares_header(self.bundle, op, header) {
            return true;
        }
        self.refuse(
            path,
            RefusalCode::Unresolved,
            format!("operation `{}` declares no header `{header}`", op.id),
        );
        false
    }

    /// An identity header applies to HTTP operations only. A header name
    /// must be declared, ignoring case, on one of the operation's 2xx
    /// responses; `false` (none) needs no declaration.
    fn check_identity_header(&mut self, op: &Operation, h: &IdentityHeader, path: &Path) -> bool {
        if !is_http(op) {
            self.refuse(
                path,
                RefusalCode::NotApplicable,
                format!(
                    "operation `{}` is not an HTTP operation and has no headers",
                    op.id
                ),
            );
            return false;
        }
        let Some(name) = h.name() else {
            return true;
        };
        if declares_success_header(op, name) {
            return true;
        }
        self.refuse(
            path,
            RefusalCode::Unresolved,
            format!(
                "operation `{}` declares no 2xx response header `{name}`",
                op.id
            ),
        );
        false
    }

    fn check_status(&mut self, op: &Operation, code: u16, path: &Path) -> bool {
        if !is_http(op) {
            self.refuse(
                path,
                RefusalCode::NotApplicable,
                format!(
                    "operation `{}` is not an HTTP operation and has no statuses",
                    op.id
                ),
            );
            return false;
        }
        if declares_status(op, code) {
            return true;
        }
        self.refuse(
            path,
            RefusalCode::Unresolved,
            format!("operation `{}` declares no {code} response", op.id),
        );
        false
    }

    /// A pointer into `schema`, the operation's `what` schema; `records`
    /// pointers must land on an array. With `root_ok` (a `record` pointer) the
    /// root pointer `""` is accepted, and says the whole `what` is the record,
    /// so its schema must be an object's.
    fn check_pointer(
        &mut self,
        schema: Option<&'a Value>,
        pointer: &Pointer,
        (array, root_ok): (bool, bool),
        what: &str,
        path: &Path,
    ) -> bool {
        let syntax = if root_ok {
            Pointer::check_record
        } else {
            Pointer::check
        };
        if let Err(e) = syntax(pointer.as_str()) {
            self.refuse(path, RefusalCode::Invalid, format!("`{pointer}`: {e}"));
            return false;
        }
        let Some(schema) = schema else {
            self.refuse(
                path,
                RefusalCode::Unresolved,
                format!("the operation declares no {what} schema for `{pointer}` to point into"),
            );
            return false;
        };
        let Some(target) = at_pointer(self.bundle, schema, &pointer.tokens()) else {
            self.refuse(
                path,
                RefusalCode::Unresolved,
                format!("`{pointer}` is not inside the {what} schema"),
            );
            return false;
        };
        if root_ok && pointer.is_root() && !is_object(self.bundle, target) {
            self.refuse(
                path,
                RefusalCode::Unresolved,
                format!("the {what} is not an object, so it is not itself the record (`\"\"`)"),
            );
            return false;
        }
        if array && !is_array(self.bundle, target) {
            self.refuse(
                path,
                RefusalCode::Invalid,
                format!("`{pointer}` does not point to an array in the {what} schema"),
            );
            return false;
        }
        true
    }

    /// An update's `request.instructions`: its array is in a request body the
    /// operation declares (any of its media types), its kind member is named, and
    /// each kind's changes name members of the record (a braced step, a value of
    /// the instruction, stands for any key of a map) and pointers that parse. An
    /// operation of another kind is refused it whole.
    fn check_instructions(
        &mut self,
        op: &'a Operation,
        (record, ty): (Option<&'a Value>, Option<&str>),
        i: &InstructionsDesc,
        path: &Path,
    ) -> Option<InstructionsDesc> {
        self.unknown(path, &i.extra);
        if self.op_kind(op) != Some(OpKind::Update) {
            self.refuse(
                path,
                RefusalCode::NotApplicable,
                format!(
                    "operation `{}` is not an update, so it applies no instructions",
                    op.id
                ),
            );
            return None;
        }
        let bodies: Vec<&'a Value> = op
            .input
            .body
            .iter()
            .chain(op.input.bodies.iter().map(|v| &v.schema))
            .collect();
        let holding = bodies.iter().copied().find(|schema| {
            at_pointer(self.bundle, schema, &i.at.tokens())
                .is_some_and(|s| is_array(self.bundle, s))
        });
        let at = self.check_pointer(
            holding.or(bodies.first().copied()),
            &i.at,
            (true, false),
            "request body",
            &path.join("at"),
        );
        if i.kind.is_empty() {
            self.refuse(
                &path.join("kind"),
                RefusalCode::Invalid,
                "`kind` names the member of an instruction that says its kind",
            );
        }
        if !at || i.kind.is_empty() {
            return None;
        }
        let kinds_path = path.join("kinds");
        let mut kinds = BTreeMap::new();
        for (kind, d) in &i.kinds {
            let kp = kinds_path.join(kind);
            self.unknown(&kp, &d.extra);
            let mut changes = BTreeMap::new();
            for (member, change) in &d.changes {
                let mp = kp.join("changes").join(member);
                if !self.check_templated_member(record, ty, member, &mp) {
                    continue;
                }
                if let Some((ptr, request)) = change.pointer() {
                    let key = match (change, request) {
                        (_, true) => "from_request",
                        (super::InstructionChange::FromInstruction(_), _) => "from_instruction",
                        (super::InstructionChange::Append(_), _) => "append",
                        _ => "remove",
                    };
                    let ok = match request {
                        true => {
                            let schema = bodies.iter().copied().find(|schema| {
                                at_pointer(self.bundle, schema, &ptr.tokens()).is_some()
                            });
                            self.check_pointer(
                                schema.or(bodies.first().copied()),
                                ptr,
                                (false, false),
                                "request body",
                                &mp.join(key),
                            )
                        }
                        // An instruction's schema is often free-form: only the
                        // syntax is checked.
                        false => match Pointer::check(ptr.as_str()) {
                            Ok(()) => true,
                            Err(e) => {
                                self.refuse(
                                    &mp.join(key),
                                    RefusalCode::Invalid,
                                    format!("`{ptr}`: {e}"),
                                );
                                false
                            }
                        },
                    };
                    if !ok {
                        continue;
                    }
                }
                changes.insert(member.clone(), change.clone());
            }
            if !changes.is_empty() || d.changes.is_empty() {
                kinds.insert(
                    kind.clone(),
                    InstructionDesc {
                        changes,
                        extra: Extra::new(),
                    },
                );
            }
        }
        Some(InstructionsDesc {
            at: i.at.clone(),
            kind: i.kind.clone(),
            kinds,
            extra: Extra::new(),
        })
    }

    /// [`Self::check_member`] for a member whose steps may be braced (`{key}`):
    /// such a step is any key of the map it is in, so it walks into the map's
    /// `additionalProperties`, and a map that does not say what it holds ends the
    /// check there.
    fn check_templated_member(
        &mut self,
        record: Option<&'a Value>,
        t: Option<&str>,
        member: &str,
        path: &Path,
    ) -> bool {
        if !member.contains(['{', '}']) {
            return self.check_member(record, t, member, path);
        }
        let braced = |step: &str| {
            step.strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
                .filter(|inner| !inner.is_empty() && !inner.contains(['{', '}']))
                .is_some()
        };
        let steps: Vec<&str> = member.split('.').collect();
        if steps
            .iter()
            .any(|step| step.contains(['{', '}']) && !braced(step))
        {
            self.refuse(
                path,
                RefusalCode::Invalid,
                format!("`{member}`: a braced step is a whole step, `{{name}}`"),
            );
            return false;
        }
        let (Some(t), Some(record)) = (t, record) else {
            return self.check_member(record, t, member, path);
        };
        let mut at = Some(record);
        for step in steps {
            let Some(schema) = at else {
                break;
            };
            at = match braced(step) {
                true => match self.bundle.resolve(schema).get("additionalProperties") {
                    Some(inner) if inner.is_object() => Some(inner),
                    // A map that does not say what it holds.
                    _ => return true,
                },
                false => match property(self.bundle, schema, step) {
                    Some(found) => Some(found),
                    None => {
                        self.refuse(
                            path,
                            RefusalCode::Unresolved,
                            format!("`{member}` is not a member of a `{t}` record"),
                        );
                        return false;
                    }
                },
            };
        }
        true
    }

    fn check_type_path_param(&mut self, t: &str, param: &str, path: &Path) -> bool {
        let http: Vec<_> = self.ops_of(t).into_iter().filter(|o| is_http(o)).collect();
        if http.is_empty() {
            self.refuse(
                path,
                RefusalCode::NotApplicable,
                format!("type `{t}` has no HTTP operation and so no path"),
            );
            return false;
        }
        if http
            .iter()
            .any(|o| declares_path_param(self.bundle, o, param))
        {
            return true;
        }
        self.refuse(
            path,
            RefusalCode::Unresolved,
            format!("no operation of `{t}` declares a path parameter `{param}`"),
        );
        false
    }

    fn check_type_header(&mut self, t: &str, header: &str, path: &Path) -> bool {
        if !self.ops_of(t).iter().any(|o| is_http(o)) {
            self.refuse(
                path,
                RefusalCode::NotApplicable,
                format!("type `{t}` has no HTTP operation and so no headers"),
            );
            return false;
        }
        if self
            .writes_of(t)
            .iter()
            .any(|o| is_http(o) && declares_header(self.bundle, o, header))
        {
            return true;
        }
        self.refuse(
            path,
            RefusalCode::Unresolved,
            format!("no write of `{t}` declares a header `{header}`"),
        );
        false
    }

    fn check_type_status(&mut self, t: &str, code: u16, path: &Path) -> bool {
        if !self.ops_of(t).iter().any(|o| is_http(o)) {
            self.refuse(
                path,
                RefusalCode::NotApplicable,
                format!("type `{t}` has no HTTP operation and so no statuses"),
            );
            return false;
        }
        if self
            .writes_of(t)
            .iter()
            .any(|o| is_http(o) && declares_status(o, code))
        {
            return true;
        }
        self.refuse(
            path,
            RefusalCode::Unresolved,
            format!("no write of `{t}` declares a {code} response"),
        );
        false
    }

    // ---- types ----

    fn type_desc(&mut self, name: &str, t: &TypeDesc, path: &Path) -> TypeDesc {
        self.unknown(path, &t.extra);
        let record = self.record_schema(name);
        let ty = Some(name);
        let mut out = TypeDesc::default();

        if let Some(id) = &t.identity {
            let p = path.join("identity");
            self.unknown(&p, &id.extra);
            // An absent member keeps the lower tier's (the effective world
            // refuses an identity left with none). A refused member drops
            // the identity's pattern with it: the pattern says how that
            // member's value embeds the id.
            let member_ok = id
                .member
                .as_ref()
                .is_none_or(|m| self.check_member(record, ty, m, &p.join("member")));
            let alternates = id.alternates.as_ref().and_then(|alts| {
                let ap = p.join("alternates");
                let kept: Vec<String> = alts
                    .iter()
                    .enumerate()
                    .filter(|(i, m)| self.check_member(record, ty, m, &ap.join(i)))
                    .map(|(_, m)| m.clone())
                    .collect();
                // Every alternate refused: dropped, never read as `[]`
                // (which says the type has none).
                (!kept.is_empty() || alts.is_empty()).then_some(kept)
            });
            if member_ok {
                out.identity = unless_emptied(
                    id,
                    IdentityDesc {
                        member: id.member.clone(),
                        pattern: id
                            .pattern
                            .clone()
                            .filter(|pat| self.check_pattern(pat, &p.join("pattern"))),
                        alternates,
                        extra: Extra::new(),
                    },
                );
            } else if alternates.is_some() {
                out.identity = Some(IdentityDesc {
                    alternates,
                    ..IdentityDesc::default()
                });
            }
        }

        if let Some(parent) = &t.parent {
            let p = path.join("parent");
            self.unknown(&p, &parent.extra);
            if self.types.contains(parent.r#type.as_str()) {
                out.parent = Some(ParentDesc {
                    r#type: parent.r#type.clone(),
                    member: parent
                        .member
                        .clone()
                        .filter(|m| self.check_member(record, ty, m, &p.join("member"))),
                    path_param: parent
                        .path_param
                        .clone()
                        .filter(|q| self.check_type_path_param(name, q, &p.join("path_param"))),
                    pattern: parent
                        .pattern
                        .clone()
                        .filter(|pat| self.check_pattern(pat, &p.join("pattern"))),
                    embedded: parent.embedded.clone().filter(|m| {
                        let held_in = self.record_schema(&parent.r#type);
                        self.check_member(held_in, Some(&parent.r#type), m, &p.join("embedded"))
                    }),
                    extra: Extra::new(),
                });
            } else {
                self.refuse(
                    &p.join("type"),
                    RefusalCode::Unresolved,
                    format!(
                        "no operation has resource type `{}` and no definition is named so",
                        parent.r#type
                    ),
                );
            }
        }

        if let Some(l) = &t.lifecycle {
            let p = path.join("lifecycle");
            self.unknown(&p, &l.extra);
            if self.check_member(record, ty, &l.member, &p.join("member")) {
                let initial = l
                    .initial
                    .clone()
                    .filter(|s| self.check_state(&l.states, s, &p.join("initial")));
                let soft_delete = l
                    .soft_delete
                    .as_ref()
                    .and_then(|sd| self.soft_delete(name, &l.states, sd, &p.join("soft_delete")));
                out.lifecycle = Some(LifecycleDesc {
                    member: l.member.clone(),
                    states: l.states.clone(),
                    initial,
                    soft_delete,
                    extra: Extra::new(),
                });
            }
        }

        for (i, u) in t.unique.iter().enumerate() {
            let p = path.join("unique").join(i);
            self.unknown(&p, &u.extra);
            if u.members.is_empty() {
                self.refuse(
                    &p.join("members"),
                    RefusalCode::Invalid,
                    "a unique set names at least one member",
                );
                continue;
            }
            let mut ok = true;
            for (j, m) in u.members.iter().enumerate() {
                ok &= self.check_member(record, ty, m, &p.join("members").join(j));
            }
            // A scope that no longer resolves refuses the whole set: kept
            // without it, the members would be unique across every record.
            if let Some(scope) = &u.scope {
                ok &= self.check_member(record, ty, scope, &p.join("scope"));
            }
            if ok {
                out.unique.push(UniqueDesc {
                    members: u.members.clone(),
                    scope: u.scope.clone(),
                    conflict: u
                        .conflict
                        .filter(|c| self.check_type_status(name, *c, &p.join("conflict"))),
                    extra: Extra::new(),
                });
            }
        }

        if let Some(c) = &t.concurrency {
            let p = path.join("concurrency");
            self.unknown(&p, &c.extra);
            out.concurrency = unless_emptied(
                c,
                ConcurrencyDesc {
                    member: c
                        .member
                        .clone()
                        .filter(|m| self.check_member(record, ty, m, &p.join("member"))),
                    header: c
                        .header
                        .clone()
                        .filter(|h| self.check_type_header(name, h, &p.join("header"))),
                    conflict: c
                        .conflict
                        .filter(|s| self.check_type_status(name, *s, &p.join("conflict"))),
                    extra: Extra::new(),
                },
            );
        }
        out
    }

    fn soft_delete(
        &mut self,
        t: &str,
        states: &[String],
        sd: &SoftDelete,
        path: &Path,
    ) -> Option<SoftDelete> {
        self.unknown(path, &sd.extra);
        if !self.check_state(states, &sd.state, &path.join("state")) {
            return None;
        }
        let listed_with = sd.listed_with.as_ref().and_then(|lw| {
            let p = path.join("listed_with");
            self.unknown(&p, &lw.extra);
            let lists: Vec<_> = self
                .ops_of(t)
                .into_iter()
                .filter(|o| self.op_kind(o) == Some(OpKind::List))
                .collect();
            if lists
                .iter()
                .any(|o| declares_param(self.bundle, o, &lw.param))
            {
                Some(ParamValue {
                    param: lw.param.clone(),
                    value: lw.value.clone(),
                    extra: Extra::new(),
                })
            } else {
                self.refuse(
                    &p.join("param"),
                    RefusalCode::Unresolved,
                    format!("no list of `{t}` declares a parameter `{}`", lw.param),
                );
                None
            }
        });
        let unless = sd.unless.as_ref().and_then(|u| {
            let p = path.join("unless");
            self.unknown(&p, &u.extra);
            let deletes: Vec<_> = self
                .ops_of(t)
                .into_iter()
                .filter(|o| self.op_kind(o) == Some(OpKind::Delete))
                .collect();
            // The delete reads it from its query only.
            if deletes
                .iter()
                .any(|o| has_property(self.bundle, o.input.query.as_ref(), |k| k == u.param))
            {
                Some(ParamValue {
                    param: u.param.clone(),
                    value: u.value.clone(),
                    extra: Extra::new(),
                })
            } else {
                self.refuse(
                    &p.join("param"),
                    RefusalCode::Unresolved,
                    format!(
                        "no delete of `{t}` declares a query parameter `{}`",
                        u.param
                    ),
                );
                None
            }
        });
        Some(SoftDelete {
            state: sd.state.clone(),
            listed_with,
            unless,
            extra: Extra::new(),
        })
    }

    fn derived(
        &mut self,
        name: &str,
        derived: &BTreeMap<String, DerivedDesc>,
        applied: &World,
        path: &Path,
    ) -> BTreeMap<String, DerivedDesc> {
        let record = self.record_schema(name);
        let mut out = BTreeMap::new();
        for (member, d) in derived {
            let p = path.join(member);
            let (of, extra) = match d {
                DerivedDesc::Count { of, extra } | DerivedDesc::Sum { of, extra, .. } => {
                    (of, extra)
                }
            };
            self.unknown(&p, extra);
            if !self.check_member(record, Some(name), member, &p) {
                continue;
            }
            let child_of_this = applied
                .types
                .get(of)
                .and_then(|t| t.parent.as_ref())
                .is_some_and(|parent| parent.r#type == name);
            if !child_of_this {
                self.refuse(
                    &p.join("of"),
                    RefusalCode::Unresolved,
                    format!("type `{of}` is not described with `{name}` as its parent"),
                );
                continue;
            }
            match d {
                DerivedDesc::Count { of, .. } => {
                    out.insert(
                        member.clone(),
                        DerivedDesc::Count {
                            of: of.clone(),
                            extra: Extra::new(),
                        },
                    );
                }
                DerivedDesc::Sum {
                    of,
                    member: summed,
                    r#where,
                    ..
                } => {
                    let child = self.record_schema(of);
                    if !self.check_member(child, Some(of), summed, &p.join("member")) {
                        continue;
                    }
                    let r#where = r#where.as_ref().map(|w| {
                        let wp = p.join("where");
                        w.iter()
                            .filter(|(k, _)| self.check_member(child, Some(of), k, &wp.join(k)))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    });
                    out.insert(
                        member.clone(),
                        DerivedDesc::Sum {
                            of: of.clone(),
                            member: summed.clone(),
                            r#where,
                            extra: Extra::new(),
                        },
                    );
                }
            }
        }
        out
    }

    // ---- operations ----

    fn op_desc(&mut self, op: &'a Operation, d: &OpDesc, applied: &World, path: &Path) -> OpDesc {
        self.unknown(path, &d.extra);
        let mut out = OpDesc {
            kind: d.kind,
            ..OpDesc::default()
        };
        if let Some(t) = &d.r#type {
            if self.types.contains(t.as_str()) {
                out.r#type = Some(t.clone());
            } else {
                self.refuse(
                    &path.join("type"),
                    RefusalCode::Unresolved,
                    format!("no operation has resource type `{t}` and no definition is named so"),
                );
            }
        }
        let t: Option<String> = out.r#type.clone().or_else(|| op.resource_type.clone());
        let ty = t.as_deref();
        let record = ty.and_then(|t| self.record_schema(t));
        let body = op.input.body.as_ref();
        let answer = success_schema(op);

        if let Some(target) = &d.target {
            let p = path.join("target");
            self.unknown(&p, &target.extra);
            out.target = unless_emptied(
                target,
                TargetDesc {
                    path_param: target
                        .path_param
                        .clone()
                        .filter(|q| self.check_path_param(op, q, &p.join("path_param"))),
                    member: target.member.clone().filter(|m| {
                        let found = body
                            .and_then(|b| dotted_member(self.bundle, b, m))
                            .is_some();
                        if !found {
                            self.refuse(
                                &p.join("member"),
                                RefusalCode::Unresolved,
                                format!(
                                    "`{m}` is not a member of operation `{}`'s request body",
                                    op.id
                                ),
                            );
                        }
                        found
                    }),
                    extra: Extra::new(),
                },
            );
        }

        if let Some(r) = &d.request {
            let p = path.join("request");
            self.unknown(&p, &r.extra);
            out.request = unless_emptied(
                r,
                RecordsAt {
                    record: r.record.clone().filter(|ptr| {
                        self.check_pointer(
                            body,
                            ptr,
                            (false, true),
                            "request body",
                            &p.join("record"),
                        )
                    }),
                    records: r.records.clone().filter(|ptr| {
                        self.check_pointer(
                            body,
                            ptr,
                            (true, false),
                            "request body",
                            &p.join("records"),
                        )
                    }),
                    instructions: r.instructions.as_ref().and_then(|i| {
                        self.check_instructions(op, (record, ty), i, &p.join("instructions"))
                    }),
                    extra: Extra::new(),
                },
            );
        }

        if let Some(a) = &d.answer {
            let p = path.join("answer");
            self.unknown(&p, &a.extra);
            let long_running = a.long_running.as_ref().and_then(|lr| {
                let lp = p.join("long_running");
                self.unknown(&lp, &lr.extra);
                let mut ok = true;
                for (key, member) in [("done", &lr.done), ("result", &lr.result)] {
                    if answer
                        .and_then(|s| dotted_member(self.bundle, s, member))
                        .is_none()
                    {
                        self.refuse(
                            &lp.join(key),
                            RefusalCode::Unresolved,
                            format!(
                                "`{member}` is not a member of operation `{}`'s answer",
                                op.id
                            ),
                        );
                        ok = false;
                    }
                }
                ok.then(|| LongRunning {
                    done: lr.done.clone(),
                    result: lr.result.clone(),
                    extra: Extra::new(),
                })
            });
            out.answer = unless_emptied(
                a,
                AnswerDesc {
                    record: a.record.clone().filter(|ptr| {
                        self.check_pointer(answer, ptr, (false, true), "answer", &p.join("record"))
                    }),
                    records: a.records.clone().filter(|ptr| {
                        self.check_pointer(answer, ptr, (true, false), "answer", &p.join("records"))
                    }),
                    long_running,
                    identity_header: a
                        .identity_header
                        .clone()
                        .filter(|h| self.check_identity_header(op, h, &p.join("identity_header"))),
                    extra: Extra::new(),
                },
            );
        }

        if let Some(transition) = &d.transition {
            let p = path.join("transition");
            let lifecycle = ty
                .and_then(|t| applied.types.get(t))
                .and_then(|t| t.lifecycle.as_ref());
            let mut kept = BTreeMap::new();
            for (member, value) in transition {
                let mp = p.join(member);
                let ok = match lifecycle {
                    Some(l) if l.member != *member => {
                        self.refuse(
                            &mp,
                            RefusalCode::Unresolved,
                            format!(
                                "`{member}` is not `{}`, the lifecycle member of `{}`",
                                l.member,
                                ty.unwrap_or_default()
                            ),
                        );
                        false
                    }
                    Some(l) => match value.as_str() {
                        Some(state) => self.check_state(&l.states, state, &mp),
                        None if l.states.is_empty() => true,
                        None => {
                            self.refuse(
                                &mp,
                                RefusalCode::Invalid,
                                format!("a transition of `{member}` sets one of its states"),
                            );
                            false
                        }
                    },
                    None => {
                        self.refuse(
                            &mp,
                            RefusalCode::Unresolved,
                            match ty {
                                Some(t) => format!(
                                    "type `{t}` has no lifecycle, so `{member}` has no states to set"
                                ),
                                None => format!(
                                    "operation `{}` has no resource type with a lifecycle",
                                    op.id
                                ),
                            },
                        );
                        false
                    }
                };
                if ok {
                    kept.insert(member.clone(), value.clone());
                }
            }
            if !kept.is_empty() || transition.is_empty() {
                out.transition = Some(kept);
            }
        }

        let changes = path.join("changes");
        for (member, change) in &d.changes {
            let p = changes.join(member);
            if !self.check_member(record, ty, member, &p) {
                continue;
            }
            if let ChangeDesc::FromRequest(ptr) = change
                && !self.check_pointer(
                    body,
                    ptr,
                    (false, false),
                    "request body",
                    &p.join("from_request"),
                )
            {
                continue;
            }
            out.changes.insert(member.clone(), change.clone());
        }

        if let Some(s) = &d.scope {
            let p = path.join("scope");
            self.unknown(&p, &s.extra);
            let param = self.check_param(op, &s.param, &p.join("param"));
            let member = self.check_member(record, ty, &s.member, &p.join("member"));
            let recursive = match &s.recursive {
                Some(_) if s.kind != Some(ScopeKind::Prefix) => {
                    self.refuse(
                        &p.join("recursive"),
                        RefusalCode::Invalid,
                        "`recursive` applies to a `prefix` scope only".to_string(),
                    );
                    false
                }
                Some(q) => self.check_param(op, q, &p.join("recursive")),
                None => true,
            };
            if param && member && recursive {
                out.scope = Some(ScopeDesc {
                    param: s.param.clone(),
                    member: s.member.clone(),
                    kind: s.kind,
                    recursive: s.recursive.clone(),
                    extra: Extra::new(),
                });
            }
        }

        for (i, f) in d.filters.iter().enumerate() {
            let p = path.join("filters").join(i);
            self.unknown(&p, &f.extra);
            if !self.check_filter_shape(f, &p) {
                continue;
            }
            if f.grammar == Some(FilterGrammar::DeepObject) && f.param.starts_with('/') {
                self.refuse(
                    &p.join("param"),
                    RefusalCode::Invalid,
                    "a `deep_object` filter reads bracketed query keys: its `param` is a query parameter, not a body pointer".to_string(),
                );
                continue;
            }
            let param = match f.grammar {
                // A deepObject root may be declared whole (`filter`) or as its
                // bracketed keys (`filter[status]`).
                Some(FilterGrammar::DeepObject)
                    if has_property(self.bundle, op.input.query.as_ref(), |k| {
                        k.strip_prefix(f.param.as_str())
                            .is_some_and(|rest| rest.starts_with('['))
                    }) =>
                {
                    true
                }
                _ => self.check_param(op, &f.param, &p.join("param")),
            };
            let member = f
                .member
                .as_ref()
                .is_none_or(|m| self.check_member(record, ty, m, &p.join("member")));
            let prefix = f
                .member_prefix
                .as_ref()
                .is_none_or(|m| self.check_member(record, ty, m, &p.join("member_prefix")));
            if param && member && prefix {
                out.filters.push(FilterDesc {
                    param: f.param.clone(),
                    member: f.member.clone(),
                    kind: f.kind,
                    grammar: f.grammar,
                    member_prefix: f.member_prefix.clone(),
                    extra: Extra::new(),
                });
            }
        }

        if let Some(o) = &d.order {
            let p = path.join("order");
            self.unknown(&p, &o.extra);
            if self.check_member(record, ty, &o.member, &p.join("member")) {
                out.order = Some(OrderDesc {
                    member: o.member.clone(),
                    descending: o.descending,
                    extra: Extra::new(),
                });
            }
        }

        if let Some(pg) = &d.paging {
            let p = path.join("paging");
            self.unknown(&p, &pg.extra);
            out.paging = Some(PagingDesc {
                style: pg.style,
                request: pg
                    .request
                    .clone()
                    .filter(|q| self.check_param(op, q, &p.join("request"))),
                response: pg.response.clone().filter(|m| {
                    let found = answer
                        .and_then(|s| dotted_member(self.bundle, s, m))
                        .is_some();
                    if !found {
                        self.refuse(
                            &p.join("response"),
                            RefusalCode::Unresolved,
                            format!("`{m}` is not a member of operation `{}`'s answer", op.id),
                        );
                    }
                    found
                }),
                response_header: pg
                    .response_header
                    .clone()
                    .filter(|h| self.check_header(op, h, &p.join("response_header"))),
                size: pg
                    .size
                    .clone()
                    .filter(|q| self.check_param(op, q, &p.join("size"))),
                extra: Extra::new(),
            });
        }

        // A parameter the list reads cannot also be one that asks nothing, nor can one
        // named for an item member (which filters it by name).
        let read: Vec<&String> = d
            .filters
            .iter()
            .map(|f| &f.param)
            .chain(
                d.scope
                    .iter()
                    .flat_map(|s| std::iter::once(&s.param).chain(&s.recursive)),
            )
            .chain(
                d.paging
                    .iter()
                    .flat_map(|p| p.request.iter().chain(&p.size)),
            )
            .collect();
        out.ignores = d
            .ignores
            .iter()
            .enumerate()
            .filter(|(i, q)| {
                let p = path.join("ignores").join(*i);
                let problem = if read.contains(q) {
                    Some(format!("`{q}` is a parameter the list reads"))
                } else if !q.starts_with('/')
                    && record.is_some_and(|r| dotted_member(self.bundle, r, q).is_some())
                {
                    Some(format!("`{q}` names a member of the list's items"))
                } else {
                    None
                };
                match problem {
                    Some(message) => {
                        self.refuse(&p, RefusalCode::Invalid, message);
                        false
                    }
                    None => self.check_param(op, q, &p),
                }
            })
            .map(|(_, q)| q.clone())
            .collect();

        if let Some(h) = &d.idempotency {
            let p = path.join("idempotency");
            self.unknown(&p, &h.extra);
            if self.check_header(op, &h.header, &p.join("header")) {
                out.idempotency = Some(HeaderRef {
                    header: h.header.clone(),
                    extra: Extra::new(),
                });
            }
        }

        out.not_found = d
            .not_found
            .filter(|c| self.check_status(op, *c, &path.join("not_found")));

        if let Some(f) = &d.fields {
            out.fields = self.fields(op, f, (record, ty), &path.join("fields"));
        }
        if let Some(q) = &d.queue {
            let named = out.target.is_some();
            out.queue = self.queue(op, (d, named), q, &path.join("queue"));
        }
        out
    }

    /// A read's or list's `fields`: its parameter declared, a separator that is not
    /// empty, and the members it always or by default answers members of the record
    /// (each refused alone). An operation of another kind is refused it whole.
    fn fields(
        &mut self,
        op: &'a Operation,
        f: &FieldsDesc,
        (record, ty): (Option<&'a Value>, Option<&str>),
        path: &Path,
    ) -> Option<FieldsDesc> {
        self.unknown(path, &f.extra);
        if !matches!(self.op_kind(op), Some(OpKind::Read | OpKind::List)) {
            self.refuse(
                path,
                RefusalCode::Invalid,
                "`fields` applies to a read or a list".to_string(),
            );
            return None;
        }
        // Read from the query only: a body member or path parameter is no fieldset.
        if !has_property(self.bundle, op.input.query.as_ref(), |k| k == f.param) {
            self.refuse(
                &path.join("param"),
                RefusalCode::Unresolved,
                format!(
                    "operation `{}` declares no query parameter `{}`",
                    op.id, f.param
                ),
            );
            return None;
        }
        if f.separator.as_deref() == Some("") {
            self.refuse(
                &path.join("separator"),
                RefusalCode::Invalid,
                "a separator may not be empty".to_string(),
            );
            return None;
        }
        let mut members = |list: &[String], key: &str| -> Vec<String> {
            list.iter()
                .enumerate()
                .filter(|(i, m)| self.check_member(record, ty, m, &path.join(key).join(*i)))
                .map(|(_, m)| m.clone())
                .collect()
        };
        let always = members(&f.always, "always");
        let default = f.default.as_ref().map(|d| members(d, "default"));
        Some(FieldsDesc {
            param: f.param.clone(),
            separator: f.separator.clone(),
            always,
            default,
            extra: Extra::new(),
        })
    }

    /// An action's `queue`: `count` and `requeue.at` pointers into its request body,
    /// and those and `redelivered` only with `take`; a target named (by the description
    /// or the contract's identity), and no `transition` or `changes` beside it. An
    /// operation of another kind is refused it whole.
    fn queue(
        &mut self,
        op: &'a Operation,
        (d, named_target): (&OpDesc, bool),
        q: &QueueDesc,
        path: &Path,
    ) -> Option<QueueDesc> {
        self.unknown(path, &q.extra);
        if self.op_kind(op) != Some(OpKind::Action) {
            self.refuse(
                path,
                RefusalCode::Invalid,
                "`queue` applies to an action".to_string(),
            );
            return None;
        }
        let takes = q.effect == QueueEffect::Take;
        if !takes && (q.count.is_some() || q.requeue.is_some() || q.redelivered.is_some()) {
            self.refuse(
                path,
                RefusalCode::Invalid,
                "`count`, `requeue` and `redelivered` apply to a `take` only".to_string(),
            );
            return None;
        }
        if d.transition.is_some() || !d.changes.is_empty() {
            self.refuse(
                path,
                RefusalCode::Invalid,
                "a `queue` sets no members of its target: it is not with `transition` or `changes`"
                    .to_string(),
            );
            return None;
        }
        if !named_target && op.identity.is_none() {
            self.refuse(
                path,
                RefusalCode::Unresolved,
                "a `queue` acts on its target: give the operation a `target`".to_string(),
            );
            return None;
        }
        let body = op.input.body.as_ref();
        let count = q.count.clone().filter(|ptr| {
            self.check_pointer(
                body,
                ptr,
                (false, false),
                "request body",
                &path.join("count"),
            )
        });
        let requeue = q.requeue.as_ref().and_then(|r| {
            let p = path.join("requeue");
            self.unknown(&p, &r.extra);
            self.check_pointer(body, &r.at, (false, false), "request body", &p.join("at"))
                .then(|| RequeueDesc {
                    at: r.at.clone(),
                    value: r.value.clone(),
                    extra: Extra::new(),
                })
        });
        let redelivered = q.redelivered.clone().filter(|m| {
            let ok = !m.is_empty();
            if !ok {
                self.refuse(
                    &path.join("redelivered"),
                    RefusalCode::Invalid,
                    "a member may not be empty".to_string(),
                );
            }
            ok
        });
        Some(QueueDesc {
            effect: q.effect,
            count,
            requeue,
            redelivered,
            extra: Extra::new(),
        })
    }
}

/// `applied`, unless filtering left it empty although `authored` was not: a
/// sub-object whose every entry was refused (unknown keys included) is
/// dropped (as `transition` is), so it cannot stand over a lower tier's value
/// in an overlay. One authored empty (`{}`) is kept.
fn unless_emptied<T: Default + PartialEq>(authored: &T, applied: T) -> Option<T> {
    let empty = T::default();
    (applied != empty || *authored == empty).then_some(applied)
}

// ---- schema lookups ----

fn success_schema(op: &Operation) -> Option<&Value> {
    op.success_output().and_then(|o| o.schema.as_ref())
}

fn is_http(op: &Operation) -> bool {
    matches!(op.binding, Binding::Http { .. })
}

/// The `allOf`, `anyOf` and `oneOf` branches of a schema.
fn branches(schema: &Value) -> impl Iterator<Item = &Value> {
    ["allOf", "anyOf", "oneOf"]
        .into_iter()
        .filter_map(|k| schema.get(k).and_then(Value::as_array))
        .flatten()
}

/// Schemas a lookup has already searched, by address: a composition that
/// refers back to itself (`A = {anyOf: [{$ref: A}, ...]}`) is searched once,
/// so a lookup costs at most one visit per schema of the bundle.
type Seen = HashSet<*const Value>;

/// Resolve `schema`'s `$ref`s and mark it searched; `None` when it was already
/// searched or the walk is too deep.
fn visit<'a>(b: &'a Bundle, schema: &'a Value, seen: &mut Seen, depth: usize) -> Option<&'a Value> {
    if depth > MAX_HOPS {
        return None;
    }
    let schema = b.resolve(schema);
    seen.insert(std::ptr::from_ref(schema)).then_some(schema)
}

/// The schema of property `name` of `schema`, through `$ref`s and branches.
/// When several branches declare it, the first that constrains it wins over
/// one that allows anything (an envelope's `result: {}` beside the branch
/// that says what `result` is).
fn property<'a>(b: &'a Bundle, schema: &'a Value, name: &str) -> Option<&'a Value> {
    fn walk<'a>(
        b: &'a Bundle,
        schema: &'a Value,
        name: &str,
        seen: &mut Seen,
        depth: usize,
    ) -> Option<&'a Value> {
        let schema = visit(b, schema, seen, depth)?;
        let mut found = schema.get("properties").and_then(|p| p.get(name));
        for branch in branches(schema) {
            if found.is_some_and(|p| !allows_anything(p)) {
                break;
            }
            if let Some(p) = walk(b, branch, name, seen, depth + 1)
                && found.is_none_or(allows_anything)
            {
                found = Some(p);
            }
        }
        found
    }
    walk(b, schema, name, &mut Seen::new(), 0)
}

/// Whether a schema is `{}` or `true`.
fn allows_anything(schema: &Value) -> bool {
    schema.as_object().is_some_and(serde_json::Map::is_empty) || schema == &Value::Bool(true)
}

/// The item schema of an array `schema`, through `$ref`s and branches.
fn items<'a>(b: &'a Bundle, schema: &'a Value) -> Option<&'a Value> {
    fn walk<'a>(
        b: &'a Bundle,
        schema: &'a Value,
        seen: &mut Seen,
        depth: usize,
    ) -> Option<&'a Value> {
        let schema = visit(b, schema, seen, depth)?;
        if let Some(i) = schema
            .get("items")
            .filter(|i| i.is_object() || i.is_boolean())
        {
            return Some(i);
        }
        branches(schema).find_map(|s| walk(b, s, seen, depth + 1))
    }
    walk(b, schema, &mut Seen::new(), 0)
}

/// Whether `schema` describes an array, through `$ref`s and branches.
fn is_array(b: &Bundle, schema: &Value) -> bool {
    fn walk(b: &Bundle, schema: &Value, seen: &mut Seen, depth: usize) -> bool {
        let Some(schema) = visit(b, schema, seen, depth) else {
            return false;
        };
        let typed = match schema.get("type") {
            Some(Value::String(t)) => t == "array",
            Some(Value::Array(ts)) => ts.iter().any(|t| t == "array"),
            _ => false,
        };
        typed
            || schema.get("items").is_some()
            || branches(schema).any(|s| walk(b, s, seen, depth + 1))
    }
    walk(b, schema, &mut Seen::new(), 0)
}

/// Whether `schema` describes an object, through `$ref`s and branches: typed
/// `object`, or declaring `properties` or `additionalProperties`.
fn is_object(b: &Bundle, schema: &Value) -> bool {
    fn walk(b: &Bundle, schema: &Value, seen: &mut Seen, depth: usize) -> bool {
        let Some(schema) = visit(b, schema, seen, depth) else {
            return false;
        };
        let typed = match schema.get("type") {
            Some(Value::String(t)) => t == "object",
            Some(Value::Array(ts)) => ts.iter().any(|t| t == "object"),
            _ => false,
        };
        typed
            || schema.get("properties").is_some()
            || schema.get("additionalProperties").is_some()
            || branches(schema).any(|s| walk(b, s, seen, depth + 1))
    }
    walk(b, schema, &mut Seen::new(), 0)
}

/// The schema of a member such as `meta.id`, read as [`crate::member_at`]
/// reads a record: the property of that very name (an OData `odata.id`),
/// else, for a dotted name no property has, the property each step names in
/// turn.
fn dotted_member<'a>(b: &'a Bundle, schema: &'a Value, member: &str) -> Option<&'a Value> {
    if let Some(found) = property(b, schema, member) {
        return Some(found);
    }
    if !member.contains('.') {
        return None;
    }
    member
        .split('.')
        .try_fold(schema, |s, name| property(b, s, name))
}

/// The schema at a JSON pointer's tokens: a property, or an array's items for
/// an index (or `-`).
fn at_pointer<'a>(b: &'a Bundle, schema: &'a Value, tokens: &[String]) -> Option<&'a Value> {
    tokens.iter().try_fold(schema, |s, token| {
        property(b, s, token).or_else(|| {
            (token == "-" || token.parse::<usize>().is_ok())
                .then(|| items(b, s))
                .flatten()
        })
    })
}

/// Whether `schema` (through `$ref`s and branches) has a property whose
/// name `matches`.
fn has_property(b: &Bundle, schema: Option<&Value>, matches: impl Fn(&str) -> bool) -> bool {
    fn walk(
        b: &Bundle,
        schema: &Value,
        matches: &dyn Fn(&str) -> bool,
        seen: &mut Seen,
        depth: usize,
    ) -> bool {
        let Some(schema) = visit(b, schema, seen, depth) else {
            return false;
        };
        let here = schema
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|m| m.keys().any(|k| matches(k)));
        here || branches(schema).any(|s| walk(b, s, matches, seen, depth + 1))
    }
    schema.is_some_and(|s| walk(b, s, &matches, &mut Seen::new(), 0))
}

fn declares_path_param(b: &Bundle, op: &Operation, name: &str) -> bool {
    has_property(b, op.input.path.as_ref(), |k| k == name)
}

/// A declared path, query or top-level body parameter `name` (RPC-style and
/// non-HTTP operations take their parameters in the body), or, for a pointer
/// (`/a/b`), a member of the request body.
fn declares_param(b: &Bundle, op: &Operation, name: &str) -> bool {
    if name.starts_with('/') {
        let Ok(pointer) = Pointer::parse(name) else {
            return false;
        };
        return op
            .input
            .body
            .as_ref()
            .is_some_and(|body| at_pointer(b, body, &pointer.tokens()).is_some());
    }
    declares_path_param(b, op, name)
        || has_property(b, op.input.query.as_ref(), |k| k == name)
        || op
            .input
            .body
            .as_ref()
            .is_some_and(|body| property(b, body, name).is_some())
}

/// A declared request header or a header of any declared response, ignoring
/// case.
fn declares_header(b: &Bundle, op: &Operation, name: &str) -> bool {
    has_property(b, op.input.headers.as_ref(), |k| {
        k.eq_ignore_ascii_case(name)
    }) || op
        .outputs
        .iter()
        .any(|o| o.headers.keys().any(|k| k.eq_ignore_ascii_case(name)))
}

/// A header of a declared 2xx response, ignoring case.
fn declares_success_header(op: &Operation, name: &str) -> bool {
    op.outputs
        .iter()
        .filter(|o| matches!(o.status, StatusSelector::Code(c) if (200..300).contains(&c)))
        .any(|o| o.headers.keys().any(|k| k.eq_ignore_ascii_case(name)))
}

fn declares_status(op: &Operation, code: u16) -> bool {
    op.outputs
        .iter()
        .any(|o| o.status == StatusSelector::Code(code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Binding, Effect, GraphqlOperationType, HttpMethod, Identity, InputSchemas, Operation,
        OperationKind, OutputVariant, Protocol, StatusSelector,
    };
    use serde_json::{Map, json};

    /// A bundle of HTTP operations `(id, method, path, success schema)`. Each
    /// `{param}` of a path is a declared string path parameter; the kind follows
    /// the method; nothing has a resource type or identity yet.
    fn bundle(ops: &[(&str, &str, &str, Value)]) -> Bundle {
        let operations = ops
            .iter()
            .map(|(id, method, path, schema)| {
                let (method, kind) = match *method {
                    "GET" => (HttpMethod::Get, OperationKind::Query),
                    "POST" => (HttpMethod::Post, OperationKind::Command(Effect::Create)),
                    "PUT" => (HttpMethod::Put, OperationKind::Command(Effect::Update)),
                    "PATCH" => (HttpMethod::Patch, OperationKind::Command(Effect::Update)),
                    "DELETE" => (HttpMethod::Delete, OperationKind::Command(Effect::Delete)),
                    other => panic!("unknown method {other}"),
                };
                let params: Map<String, Value> = path
                    .split('/')
                    .filter_map(|s| s.strip_prefix('{')?.strip_suffix('}'))
                    .map(|p| (p.to_owned(), json!({"type": "string"})))
                    .collect();
                Operation {
                    id: (*id).to_owned(),
                    protocol: Protocol::Http,
                    binding: Binding::Http {
                        method,
                        path: (*path).to_owned(),
                    },
                    kind,
                    summary: None,
                    input: InputSchemas {
                        path: (!params.is_empty())
                            .then(|| json!({"type": "object", "properties": params})),
                        query: None,
                        headers: None,
                        body: None,
                        body_required: false,
                        bodies: Vec::new(),
                    },
                    outputs: vec![OutputVariant {
                        status: StatusSelector::Code(200),
                        content_type: Some("application/json".into()),
                        schema: Some(schema.clone()),
                        headers: Map::new(),
                        description: None,
                    }],
                    resource_type: None,
                    identity: None,
                    semantic_hints: vec![],
                    security: vec![],
                }
            })
            .collect();
        Bundle {
            id: "t".into(),
            title: "t".into(),
            version: "1".into(),
            description: None,
            source_digest: String::new(),
            operations,
            defs: Map::new(),
            protocol_payload: None,
            channels: vec![],
            webhooks: vec![],
            api_key_names: None,
        }
    }

    fn op<'a>(b: &'a mut Bundle, id: &str) -> &'a mut Operation {
        b.operations.iter_mut().find(|o| o.id == id).unwrap()
    }

    fn typed(b: &mut Bundle, id: &str, t: &str, path_param: Option<&str>, field: &str) {
        let o = op(b, id);
        o.resource_type = Some(t.into());
        o.identity = path_param.map(|p| Identity {
            path_param: Some(p.into()),
            field: field.into(),
            input_field: None,
            path_field: None,
            record: None,
        });
    }

    fn declare(b: &mut Bundle, id: &str, status: u16) {
        op(b, id).outputs.push(OutputVariant {
            status: StatusSelector::Code(status),
            content_type: None,
            schema: None,
            headers: Map::new(),
            description: None,
        });
    }

    fn world(v: Value) -> World {
        serde_json::from_value(v).unwrap()
    }

    fn paths(v: &Validation) -> Vec<(&str, RefusalCode)> {
        v.refused
            .iter()
            .map(|r| (r.path.as_str(), r.code))
            .collect()
    }

    /// Folders: a read, a list with `parent`, `pageToken` and `pageSize`, a create.
    fn folders() -> Bundle {
        let folder = json!({"$ref": "#/$defs/Folder"});
        let mut b = bundle(&[
            ("GetFolder", "GET", "/v3/folders/{name}", folder.clone()),
            (
                "ListFolders",
                "GET",
                "/v3/folders",
                json!({"type": "object", "properties": {
                    "folders": {"type": "array", "items": folder.clone()},
                    "nextPageToken": {"type": "string"}
                }}),
            ),
            ("CreateFolder", "POST", "/v3/folders", folder),
        ]);
        b.defs.insert(
            "Folder".into(),
            json!({"type": "object", "properties": {
                "name": {"type": "string"},
                "parent": {"type": "string"},
                "displayName": {"type": "string"},
                "state": {"type": "string", "enum": ["ACTIVE", "DELETE_REQUESTED"]},
                "etag": {"type": "string"},
                "meta": {"allOf": [{"type": "object", "properties": {"created": {"type": "string"}}}]}
            }}),
        );
        typed(&mut b, "GetFolder", "folder", Some("name"), "name");
        typed(&mut b, "ListFolders", "folder", None, "name");
        typed(&mut b, "CreateFolder", "folder", None, "name");
        declare(&mut b, "GetFolder", 404);
        op(&mut b, "ListFolders").input.query = Some(json!({"type": "object", "properties": {
            "parent": {"type": "string"},
            "pageToken": {"type": "string"},
            "pageSize": {"type": "integer"},
            "showDeleted": {"type": "boolean"}
        }}));
        op(&mut b, "CreateFolder").input.body = Some(json!({"$ref": "#/$defs/Folder"}));
        b
    }

    /// Folders with a `PATCH` taking a JSON patch, or (as another media type) a
    /// semantic patch of instructions; a folder holds labels and a map of
    /// environments.
    fn patched_folders() -> Bundle {
        let mut b = folders();
        b.operations.extend(
            bundle(&[(
                "UpdateFolder",
                "PATCH",
                "/v3/folders/{name}",
                json!({"$ref": "#/$defs/Folder"}),
            )])
            .operations,
        );
        typed(&mut b, "UpdateFolder", "folder", Some("name"), "name");
        let update = op(&mut b, "UpdateFolder");
        update.input.body = Some(json!({"type": "array", "items": {"type": "object"}}));
        update.input.bodies = vec![crate::BodyVariant {
            media_type: "application/json; domain-model=semanticpatch".into(),
            schema: json!({"type": "object", "properties": {
                "comment": {"type": "string"},
                "instructions": {"type": "array", "items": {"type": "object"}}
            }}),
        }];
        if let Some(Value::Object(folder)) = b.defs.get_mut("Folder")
            && let Some(Value::Object(props)) = folder.get_mut("properties")
        {
            props.insert(
                "labels".into(),
                json!({"type": "array", "items": {"type": "string"}}),
            );
            props.insert(
                "envs".into(),
                json!({"type": "object", "additionalProperties": {
                    "type": "object", "properties": {"on": {"type": "boolean"}}
                }}),
            );
        }
        b
    }

    #[test]
    fn instructions_keep_the_changes_that_resolve() {
        let b = patched_folders();
        let w = world(json!({"operations": {
            "UpdateFolder": {"request": {"instructions": {
                "at": "/instructions",
                "kind": "kind",
                "kinds": {
                    "addLabels": {"changes": {"labels": {"append": "/values"}}},
                    "turnOn": {"changes": {"envs.{env}.on": {"value": true}}},
                    "rename": {"changes": {
                        "displayName": {"from_request": "/comment"},
                        "parent": {"from_request": "/nothere"}
                    }},
                    "bad": {"changes": {"nope": {"value": 1}, "envs.{env.on": {"value": 1}}}
                }
            }}},
            "GetFolder": {"request": {"instructions": {"at": "/instructions", "kind": "kind"}}}
        }}));
        let v = validate(&w, &b);
        let base = "/operations/UpdateFolder/request/instructions/kinds";
        assert_eq!(
            paths(&v),
            vec![
                (
                    "/operations/GetFolder/request/instructions",
                    RefusalCode::NotApplicable
                ),
                (
                    &*format!("{base}/bad/changes/envs.{{env.on"),
                    RefusalCode::Invalid
                ),
                (
                    &*format!("{base}/bad/changes/nope"),
                    RefusalCode::Unresolved
                ),
                (
                    &*format!("{base}/rename/changes/parent/from_request"),
                    RefusalCode::Unresolved
                ),
            ]
        );
        let applied = v.applied.operations["UpdateFolder"]
            .request
            .as_ref()
            .and_then(|r| r.instructions.as_ref())
            .unwrap();
        assert_eq!(
            applied.kinds.keys().collect::<Vec<_>>(),
            vec!["addLabels", "rename", "turnOn"]
        );
        assert_eq!(
            applied.kinds["rename"].changes.keys().collect::<Vec<_>>(),
            vec!["displayName"]
        );
        // An array the request body does not hold is refused whole.
        let w = world(
            json!({"operations": {"UpdateFolder": {"request": {"instructions": {
                "at": "/steps", "kind": "kind"
            }}}}}),
        );
        let v = validate(&w, &b);
        assert_eq!(
            paths(&v),
            vec![(
                "/operations/UpdateFolder/request/instructions/at",
                RefusalCode::Unresolved
            )]
        );
        assert!(v.applied.operations["UpdateFolder"].request.is_none());
    }

    #[test]
    fn validate_keeps_resolving_entries() {
        let b = folders();
        let w = world(json!({
            "version": 1,
            "types": {"folder": {
                "identity": {"member": "name", "pattern": "folders/{id}"},
                "parent": {"type": "folder", "member": "parent", "pattern": "folders/{id}"},
                "lifecycle": {
                    "member": "state",
                    "states": ["ACTIVE", "DELETE_REQUESTED"],
                    "initial": "ACTIVE",
                    "soft_delete": {
                        "state": "DELETE_REQUESTED",
                        "listed_with": {"param": "showDeleted", "value": true}
                    }
                },
                "unique": [{"members": ["displayName", "meta.created"], "scope": "parent"}],
                "concurrency": {"member": "etag"},
                "derived": {"displayName": {"kind": "count", "of": "folder"}}
            }},
            "operations": {
                "GetFolder": {"kind": "read", "type": "folder", "target": {"path_param": "name"}, "not_found": 404},
                "ListFolders": {
                    "kind": "list",
                    "scope": {"param": "parent", "member": "parent"},
                    "order": {"member": "displayName"},
                    "paging": {"style": "cursor", "request": "pageToken", "response": "nextPageToken", "size": "pageSize"},
                    "filters": [{"param": "showDeleted", "kind": "flag"}],
                    "answer": {"records": "/folders"}
                },
                "CreateFolder": {
                    "kind": "create",
                    "changes": {"parent": {"from_request": "/parent"}}
                }
            }
        }));
        let v = validate(&w, &b);
        assert_eq!(v.refused, vec![]);
        assert_eq!(v.applied, w);
    }

    #[test]
    fn validate_refuses_unknown_operation() {
        let b = folders();
        let w = world(json!({"operations": {
            "Nope": {"kind": "read"},
            "GetFolder": {"kind": "read"}
        }}));
        let v = validate(&w, &b);
        assert_eq!(
            paths(&v),
            vec![("/operations/Nope", RefusalCode::Unresolved)]
        );
        assert!(!v.applied.operations.contains_key("Nope"));
        assert!(v.applied.operations.contains_key("GetFolder"));

        let w = world(json!({"types": {"nope": {"identity": {"member": "id"}}}}));
        let v = validate(&w, &folders());
        assert_eq!(paths(&v), vec![("/types/nope", RefusalCode::Unresolved)]);
        assert!(v.applied.types.is_empty());
    }

    #[test]
    fn validate_refuses_member_not_in_schema() {
        let w = world(json!({"types": {"folder": {
            "identity": {"member": "nmae"},
            "lifecycle": {"member": "state"}
        }}}));
        let v = validate(&w, &folders());
        assert_eq!(
            paths(&v),
            vec![("/types/folder/identity/member", RefusalCode::Unresolved)]
        );
        let folder = &v.applied.types["folder"];
        assert!(folder.identity.is_none());
        assert!(folder.lifecycle.is_some(), "the rest still applies");

        // A dotted member walks nested properties, through `allOf`.
        let w = world(json!({"types": {"folder": {"unique": [
            {"members": ["meta.created"]},
            {"members": ["meta.updated"]}
        ]}}}));
        let v = validate(&w, &folders());
        assert_eq!(
            paths(&v),
            vec![("/types/folder/unique/1/members/0", RefusalCode::Unresolved)]
        );
        assert_eq!(v.applied.types["folder"].unique.len(), 1);
    }

    #[test]
    fn validate_reads_a_dotted_member_name_as_a_literal_property_first() {
        // As `member_at` reads a record: a property named `odata.id` is that
        // member, and a dotted walk is tried only when no property has the name.
        let mut b = bundle(&[(
            "GetDoc",
            "GET",
            "/docs/{doc_id}",
            json!({"type": "object", "properties": {
                "odata.id": {"type": "string"},
                "meta": {"type": "object", "properties": {"etag": {"type": "string"}}}
            }}),
        )]);
        typed(&mut b, "GetDoc", "doc", Some("doc_id"), "odata.id");
        let w = world(json!({"types": {"doc": {
            "identity": {"member": "odata.id"},
            "unique": [{"members": ["meta.etag"]}]
        }}}));
        let v = validate(&w, &b);
        assert_eq!(v.refused, vec![]);
        assert_eq!(v.applied, w);

        // Neither a literal property nor a nested path.
        let w = world(json!({"types": {"doc": {"identity": {"member": "odata.uid"}}}}));
        let v = validate(&w, &b);
        assert_eq!(
            paths(&v),
            vec![("/types/doc/identity/member", RefusalCode::Unresolved)]
        );
    }

    #[test]
    fn validate_resolves_member_through_answer_pointer() {
        let mut b = bundle(&[(
            "GetZone",
            "GET",
            "/zones/{zone_id}",
            json!({"type": "object", "properties": {
                "success": {"type": "boolean"},
                "result": {"type": "object", "properties": {"id": {"type": "string"}}}
            }}),
        )]);
        typed(&mut b, "GetZone", "zone", Some("zone_id"), "id");
        let w = world(json!({
            "types": {"zone": {"identity": {"member": "id"}}},
            "operations": {"GetZone": {"answer": {"record": "/result"}}}
        }));
        let v = validate(&w, &b);
        assert_eq!(v.refused, vec![]);
        assert_eq!(v.applied, w);

        // Without the pointer the record is the envelope, which has no `id`.
        let w = world(json!({"types": {"zone": {"identity": {"member": "id"}}}}));
        let v = validate(&w, &b);
        assert_eq!(
            paths(&v),
            vec![("/types/zone/identity/member", RefusalCode::Unresolved)]
        );

        // A pointer to nothing is refused by its path.
        let w = world(json!({"operations": {"GetZone": {"answer": {"record": "/data"}}}}));
        let v = validate(&w, &b);
        assert_eq!(
            paths(&v),
            vec![("/operations/GetZone/answer/record", RefusalCode::Unresolved)]
        );
        // A pointer that doesn't parse is invalid.
        let w = world(json!({"operations": {"GetZone": {"answer": {"record": "result"}}}}));
        let v = validate(&w, &b);
        assert_eq!(
            paths(&v),
            vec![("/operations/GetZone/answer/record", RefusalCode::Invalid)]
        );
    }

    #[test]
    fn validate_refuses_records_pointer_to_non_array() {
        let w = world(json!({"operations": {
            "ListFolders": {"answer": {"records": "/nextPageToken"}},
            "GetFolder": {"answer": {"records": "/name"}}
        }}));
        let v = validate(&w, &folders());
        assert_eq!(
            paths(&v),
            vec![
                ("/operations/GetFolder/answer/records", RefusalCode::Invalid),
                (
                    "/operations/ListFolders/answer/records",
                    RefusalCode::Invalid
                ),
            ]
        );
        // Its only entry refused, the answer is dropped whole.
        assert!(v.applied.operations["ListFolders"].answer.is_none());

        // An item of an array resolves through `items`.
        let w = world(json!({"operations": {"ListFolders": {"answer": {"record": "/folders/0"}}}}));
        assert_eq!(validate(&w, &folders()).refused, vec![]);
    }

    #[test]
    fn validate_refused_records_pointer_leaves_record_lookup_alone() {
        // `/name` resolves, but to a string: refused as `invalid`, it must
        // not stand in for the record, so the valid identity member applies.
        let w = world(json!({
            "types": {"folder": {"identity": {"member": "name"}}},
            "operations": {"GetFolder": {"answer": {"records": "/name"}}}
        }));
        let v = validate(&w, &folders());
        assert_eq!(
            paths(&v),
            vec![("/operations/GetFolder/answer/records", RefusalCode::Invalid)]
        );
        assert!(v.applied.types["folder"].identity.is_some());
    }

    #[test]
    fn validate_resolves_member_behind_interior_ref() {
        let mut b = folders();
        b.defs.insert(
            "Wrapped".into(),
            json!({"type": "object", "properties": {
                "name": {"type": "string"},
                "m": {"$ref": "#/$defs/Folder/properties/meta"}
            }}),
        );
        op(&mut b, "GetFolder").outputs[0].schema = Some(json!({"$ref": "#/$defs/Wrapped"}));
        let w = world(json!({"types": {"folder": {"unique": [{"members": ["m.created"]}]}}}));
        let v = validate(&w, &b);
        assert_eq!(v.refused, vec![]);
    }

    #[test]
    fn validate_prefers_the_branch_that_says_what_a_member_is() {
        let mut b = folders();
        b.defs.insert(
            "Envelope".into(),
            json!({"type": "object", "properties": {"ok": {"type": "boolean"}, "result": {}}}),
        );
        op(&mut b, "GetFolder").outputs[0].schema = Some(json!({"allOf": [
            {"$ref": "#/$defs/Envelope"},
            {"properties": {"result": {"$ref": "#/$defs/Folder"}}}
        ]}));
        let w = world(json!({
            "types": {"folder": {"identity": {"member": "name"}}},
            "operations": {"GetFolder": {"answer": {"record": "/result"}}}
        }));
        let v = validate(&w, &b);
        assert_eq!(v.refused, vec![]);
    }

    #[test]
    fn validate_takes_the_record_of_an_identified_operation_before_a_list() {
        // No read: the record is where the identified create answers it,
        // not the first operation's list envelope.
        let mut b = folders();
        b.operations.retain(|o| o.id != "GetFolder");
        op(&mut b, "CreateFolder").identity = Some(Identity {
            path_param: None,
            field: "name".into(),
            input_field: None,
            path_field: None,
            record: None,
        });
        let w = world(json!({"types": {"folder": {"identity": {"member": "name"}}}}));
        let v = validate(&w, &b);
        assert_eq!(v.refused, vec![]);
    }

    #[test]
    fn validate_refuses_header_on_graphql_operation() {
        let mut b = bundle(&[(
            "createIssue",
            "POST",
            "/graphql",
            json!({"type": "object", "properties": {"id": {"type": "string"}}}),
        )]);
        {
            let o = op(&mut b, "createIssue");
            o.protocol = Protocol::Graphql;
            o.binding = Binding::Graphql {
                operation_type: GraphqlOperationType::Mutation,
                field: "createIssue".into(),
            };
            o.resource_type = Some("issue".into());
            o.input.body =
                Some(json!({"type": "object", "properties": {"title": {"type": "string"}}}));
        }
        let w = world(json!({
            "types": {"issue": {
                "identity": {"member": "id"},
                "concurrency": {"header": "If-Match"}
            }},
            "operations": {"createIssue": {
                "kind": "create",
                "idempotency": {"header": "Idempotency-Key"},
                "target": {"path_param": "id", "member": "title"}
            }}
        }));
        let v = validate(&w, &b);
        assert_eq!(
            paths(&v),
            vec![
                (
                    "/types/issue/concurrency/header",
                    RefusalCode::NotApplicable
                ),
                (
                    "/operations/createIssue/target/path_param",
                    RefusalCode::NotApplicable
                ),
                (
                    "/operations/createIssue/idempotency/header",
                    RefusalCode::NotApplicable
                ),
            ]
        );
        let applied = &v.applied.operations["createIssue"];
        assert_eq!(applied.kind, Some(OpKind::Create));
        assert!(applied.idempotency.is_none());
        assert_eq!(
            applied.target.as_ref().unwrap().member.as_deref(),
            Some("title")
        );
        assert!(v.applied.types["issue"].identity.is_some());
    }

    #[test]
    fn validate_refuses_undeclared_conflict_status() {
        let mut b = folders();
        declare(&mut b, "CreateFolder", 412);
        op(&mut b, "CreateFolder").input.headers =
            Some(json!({"type": "object", "properties": {"if-match": {"type": "string"}}}));
        let w = world(json!({
            "types": {"folder": {
                "unique": [{"members": ["displayName"], "conflict": 409}],
                "concurrency": {"member": "etag", "header": "If-Match", "conflict": 412}
            }},
            "operations": {"GetFolder": {"not_found": 410}}
        }));
        let v = validate(&w, &b);
        assert_eq!(
            paths(&v),
            vec![
                ("/types/folder/unique/0/conflict", RefusalCode::Unresolved),
                ("/operations/GetFolder/not_found", RefusalCode::Unresolved),
            ]
        );
        let folder = &v.applied.types["folder"];
        assert_eq!(folder.unique[0].conflict, None);
        assert_eq!(folder.unique[0].members, vec!["displayName".to_owned()]);
        assert_eq!(folder.concurrency.as_ref().unwrap().conflict, Some(412));
        assert_eq!(v.applied.operations["GetFolder"].not_found, None);
    }

    #[test]
    fn validate_reports_unknown_keys() {
        let w = world(json!({
            "version": 1,
            "bogus": true,
            "types": {"folder": {
                "identty": {"member": "name"},
                "identity": {"member": "name", "patern": "x"}
            }},
            "operations": {"GetFolder": {
                "target": {"path_parm": "name"},
                "paging": {"style": "cursor", "a/b": 1}
            }}
        }));
        let v = validate(&w, &folders());
        assert_eq!(
            paths(&v),
            vec![
                ("/bogus", RefusalCode::UnknownKey),
                ("/types/folder/identty", RefusalCode::UnknownKey),
                ("/types/folder/identity/patern", RefusalCode::UnknownKey),
                (
                    "/operations/GetFolder/target/path_parm",
                    RefusalCode::UnknownKey
                ),
                ("/operations/GetFolder/paging/a~1b", RefusalCode::UnknownKey),
            ]
        );
        assert!(v.applied.extra.is_empty());
        let folder = &v.applied.types["folder"];
        assert!(folder.extra.is_empty());
        assert_eq!(
            folder.identity.as_ref().unwrap().member.as_deref(),
            Some("name")
        );
        assert!(folder.identity.as_ref().unwrap().extra.is_empty());
        let get = &v.applied.operations["GetFolder"];
        // `target` held only a typo: refused, it is dropped, not kept empty.
        assert!(get.target.is_none());
        assert!(get.paging.as_ref().unwrap().extra.is_empty());
    }

    #[test]
    fn validate_drops_sub_objects_with_only_unknown_keys() {
        let w = world(json!({
            "types": {"folder": {"concurrency": {"bogus": 1}}},
            "operations": {
                "GetFolder": {
                    "target": {"path_parm": "name"},
                    "answer": {"bogus": 2}
                },
                "CreateFolder": {"request": {"bogus": 3}}
            }
        }));
        let v = validate(&w, &folders());
        assert_eq!(
            paths(&v),
            vec![
                ("/types/folder/concurrency/bogus", RefusalCode::UnknownKey),
                (
                    "/operations/CreateFolder/request/bogus",
                    RefusalCode::UnknownKey
                ),
                (
                    "/operations/GetFolder/target/path_parm",
                    RefusalCode::UnknownKey
                ),
                (
                    "/operations/GetFolder/answer/bogus",
                    RefusalCode::UnknownKey
                ),
            ]
        );
        assert!(v.applied.types["folder"].concurrency.is_none());
        let get = &v.applied.operations["GetFolder"];
        assert_eq!(get.target, None);
        assert!(get.answer.is_none());
        assert!(v.applied.operations["CreateFolder"].request.is_none());
    }

    #[test]
    fn validate_checks_fields_queue_and_soft_delete_bypass() {
        let mut b = folders();
        b.operations.extend(
            bundle(&[(
                "DeleteFolder",
                "DELETE",
                "/v3/folders/{name}",
                json!({"type": "object"}),
            )])
            .operations,
        );
        typed(&mut b, "DeleteFolder", "folder", Some("name"), "name");
        op(&mut b, "DeleteFolder").input.query = Some(json!({"type": "object", "properties": {
            "force": {"type": "boolean"}
        }}));
        op(&mut b, "GetFolder").input.query = Some(json!({"type": "object", "properties": {
            "fields": {"type": "string"}
        }}));
        op(&mut b, "CreateFolder").input.body = Some(json!({"type": "object", "properties": {
            "count": {"type": "integer"},
            "mode": {"type": "string"}
        }}));
        let w = world(json!({
            "types": {"folder": {"lifecycle": {"member": "state", "soft_delete": {
                "state": "DELETE_REQUESTED", "unless": {"param": "force", "value": true}
            }}}},
            "operations": {
                "GetFolder": {"fields": {"param": "fields", "always": ["name", "nope"], "default": []}},
                "ListFolders": {"fields": {"param": "missing"}},
                "CreateFolder": {"kind": "action", "target": {"member": "mode"}, "queue": {
                    "effect": "take", "count": "/count", "requeue": {"at": "/nope", "value": "keep"},
                    "redelivered": "redelivered"
                }},
                "DeleteFolder": {"queue": {"effect": "append"}}
            }
        }));
        let v = validate(&w, &b);
        assert_eq!(
            paths(&v),
            vec![
                (
                    "/operations/CreateFolder/queue/requeue/at",
                    RefusalCode::Unresolved
                ),
                ("/operations/DeleteFolder/queue", RefusalCode::Invalid),
                (
                    "/operations/GetFolder/fields/always/1",
                    RefusalCode::Unresolved
                ),
                (
                    "/operations/ListFolders/fields/param",
                    RefusalCode::Unresolved
                ),
            ]
        );
        let ops = &v.applied.operations;
        let fields = ops["GetFolder"].fields.as_ref().unwrap();
        assert_eq!(fields.always, vec!["name".to_string()]);
        assert_eq!(fields.default, Some(Vec::new()));
        let queue = ops["CreateFolder"].queue.as_ref().unwrap();
        assert_eq!(queue.effect, QueueEffect::Take);
        assert!(queue.count.is_some() && queue.requeue.is_none());
        let soft = v.applied.types["folder"].lifecycle.as_ref().unwrap();
        let unless = soft.soft_delete.as_ref().unwrap().unless.as_ref().unwrap();
        assert_eq!(
            (unless.param.as_str(), &unless.value),
            ("force", &json!(true))
        );

        // A path parameter is no query parameter: neither the delete nor the read reads
        // it so. A queue with no target, or one setting members, is refused.
        let w = world(json!({
            "types": {"folder": {"lifecycle": {"member": "state", "soft_delete": {
                "state": "DELETE_REQUESTED", "unless": {"param": "name", "value": true}
            }}}},
            "operations": {
                "GetFolder": {"fields": {"param": "name"}},
                "CreateFolder": {"kind": "action", "queue": {"effect": "append"}},
                "ListFolders": {"kind": "action", "changes": {"state": {"value": "ACTIVE"}},
                                "queue": {"effect": "take"}}
            }
        }));
        let v = validate(&w, &b);
        let refused = paths(&v);
        for wanted in [
            (
                "/types/folder/lifecycle/soft_delete/unless/param",
                RefusalCode::Unresolved,
            ),
            (
                "/operations/GetFolder/fields/param",
                RefusalCode::Unresolved,
            ),
            ("/operations/CreateFolder/queue", RefusalCode::Unresolved),
            ("/operations/ListFolders/queue", RefusalCode::Invalid),
        ] {
            assert!(refused.contains(&wanted), "{wanted:?} in {refused:?}");
        }
    }

    #[test]
    fn validate_checks_grammar_filters() {
        let mut b = folders();
        op(&mut b, "ListFolders").input.query = Some(json!({"type": "object", "properties": {
            "filter": {"type": "string"},
            "where[state]": {"type": "string"}
        }}));
        let w = world(json!({
            "operations": {
                "ListFolders": {
                    "filters": [
                        {"param": "filter", "grammar": "scim"},
                        // A deepObject root declared as its bracketed keys.
                        {"param": "where", "grammar": "deep_object", "member_prefix": "meta"},
                        {"param": "filter", "grammar": "odata", "member_prefix": "nope"},
                        {"param": "missing", "grammar": "field_value"},
                        {"param": "filter"},
                        {"param": "filter", "kind": "eq", "grammar": "scim"},
                        {"param": "filter", "grammar": "scim", "member": "state"},
                        {"param": "filter", "kind": "eq", "member_prefix": "meta"}
                    ]
                }
            }
        }));
        let v = validate(&w, &b);
        let refused: Vec<(String, RefusalCode)> =
            v.refused.iter().map(|r| (r.path.clone(), r.code)).collect();
        let at = |i: usize, key: &str| format!("/operations/ListFolders/filters/{i}{key}");
        assert_eq!(
            refused,
            vec![
                (at(2, "/member_prefix"), RefusalCode::Unresolved),
                (at(3, "/param"), RefusalCode::Unresolved),
                (at(4, ""), RefusalCode::Invalid),
                (at(5, ""), RefusalCode::Invalid),
                (at(6, ""), RefusalCode::Invalid),
                (at(7, ""), RefusalCode::Invalid),
            ]
        );
        let kept = &v.applied.operations["ListFolders"].filters;
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].grammar, Some(FilterGrammar::Scim));
        assert_eq!(kept[1].member_prefix.as_deref(), Some("meta"));
    }

    #[test]
    fn validate_checks_body_params_prefix_scopes_ignores_and_embedded_parents() {
        let mut b = folders();
        op(&mut b, "ListFolders").input.body = Some(json!({"type": "object", "properties": {
            "path": {"type": "string"},
            "page": {"type": "object", "properties": {"number": {"type": "integer"}}}
        }}));
        let w = world(json!({
            "types": {
                "folder": {"parent": {"type": "folder", "member": "parent", "embedded": "nope"}}
            },
            "operations": {
                "ListFolders": {
                    "scope": {"param": "/path", "member": "parent", "kind": "prefix", "recursive": "/nope"},
                    "paging": {"style": "page", "request": "/page/number", "size": "/page/size"},
                    "ignores": ["showDeleted", "/page", "/version", "/page/number", "displayName"],
                    "filters": [{"param": "/path", "grammar": "deep_object"}]
                },
                "GetFolder": {"scope": {"param": "name", "member": "parent", "recursive": "name"}}
            }
        }));
        let v = validate(&w, &b);
        let refused: Vec<(String, RefusalCode)> =
            v.refused.iter().map(|r| (r.path.clone(), r.code)).collect();
        assert_eq!(
            refused,
            vec![
                (
                    "/types/folder/parent/embedded".to_owned(),
                    RefusalCode::Unresolved
                ),
                (
                    "/operations/GetFolder/scope/recursive".to_owned(),
                    RefusalCode::Invalid
                ),
                (
                    "/operations/ListFolders/scope/recursive".to_owned(),
                    RefusalCode::Unresolved
                ),
                (
                    "/operations/ListFolders/filters/0/param".to_owned(),
                    RefusalCode::Invalid
                ),
                (
                    "/operations/ListFolders/paging/size".to_owned(),
                    RefusalCode::Unresolved
                ),
                (
                    "/operations/ListFolders/ignores/2".to_owned(),
                    RefusalCode::Unresolved
                ),
                (
                    "/operations/ListFolders/ignores/3".to_owned(),
                    RefusalCode::Invalid
                ),
                (
                    "/operations/ListFolders/ignores/4".to_owned(),
                    RefusalCode::Invalid
                ),
            ]
        );
        let list = &v.applied.operations["ListFolders"];
        assert_eq!(
            list.paging.as_ref().and_then(|p| p.request.as_deref()),
            Some("/page/number")
        );
        assert_eq!(list.ignores, ["showDeleted", "/page"]);
        assert!(list.scope.is_none());
    }

    #[test]
    fn validate_finds_parameters_and_headers_in_composed_inputs() {
        let mut b = folders();
        b.defs.insert(
            "NamePath".into(),
            json!({"type": "object", "properties": {"name": {"type": "string"}}}),
        );
        op(&mut b, "GetFolder").input.path = Some(json!({"allOf": [{"$ref": "#/$defs/NamePath"}]}));
        op(&mut b, "ListFolders").input.query = Some(json!({"allOf": [
            {"type": "object", "properties": {"parent": {"type": "string"}}},
            {"oneOf": [{"type": "object", "properties": {"pageToken": {"type": "string"}}}]}
        ]}));
        op(&mut b, "CreateFolder").input.headers = Some(json!({"anyOf": [
            {"type": "object", "properties": {"idempotency-key": {"type": "string"}}}
        ]}));
        let w = world(json!({
            "operations": {
                "GetFolder": {"target": {"path_param": "name"}},
                "ListFolders": {
                    "scope": {"param": "parent", "member": "parent"},
                    "filters": [{"param": "pageToken", "kind": "flag"}]
                },
                "CreateFolder": {"idempotency": {"header": "Idempotency-Key"}}
            }
        }));
        let v = validate(&w, &b);
        assert_eq!(v.refused, vec![]);
        assert_eq!(v.applied, w);
    }

    #[test]
    fn validate_checks_patterns_states_transitions_and_derived() {
        let mut b = folders();
        b.operations.push({
            let mut d = bundle(&[(
                "DeleteFolder",
                "DELETE",
                "/v3/folders/{name}",
                json!({"$ref": "#/$defs/Folder"}),
            )])
            .operations
            .remove(0);
            d.resource_type = Some("folder".into());
            d
        });
        b.defs.insert(
            "Project".into(),
            json!({"type": "object", "properties": {"folder": {"type": "string"}, "cost": {"type": "number"}}}),
        );
        let w = world(json!({
            "types": {
                "folder": {
                    "identity": {"member": "name", "pattern": "folders/{id}/{id}"},
                    "lifecycle": {"member": "state", "states": ["ACTIVE"], "initial": "GONE"},
                    "derived": {
                        "displayName": {"kind": "count", "of": "Project"},
                        "etag": {"kind": "sum", "of": "Project", "member": "costs"},
                        "parent": {"kind": "count", "of": "folder"}
                    }
                },
                "Project": {"parent": {"type": "folder", "member": "folder"}}
            },
            "operations": {"DeleteFolder": {
                "kind": "action",
                "transition": {"state": "ACTIVE", "displayName": "x"}
            }}
        }));
        let v = validate(&w, &b);
        assert_eq!(
            paths(&v),
            vec![
                ("/types/folder/identity/pattern", RefusalCode::Invalid),
                ("/types/folder/lifecycle/initial", RefusalCode::Invalid),
                ("/types/folder/derived/etag/member", RefusalCode::Unresolved),
                ("/types/folder/derived/parent/of", RefusalCode::Unresolved),
                (
                    "/operations/DeleteFolder/transition/displayName",
                    RefusalCode::Unresolved
                ),
            ]
        );
        let folder = &v.applied.types["folder"];
        assert_eq!(folder.identity.as_ref().unwrap().pattern, None);
        assert_eq!(folder.lifecycle.as_ref().unwrap().initial, None);
        assert_eq!(
            folder.derived.keys().collect::<Vec<_>>(),
            vec!["displayName"]
        );
        let transition = v.applied.operations["DeleteFolder"]
            .transition
            .as_ref()
            .unwrap();
        assert_eq!(transition.keys().collect::<Vec<_>>(), vec!["state"]);
    }

    #[test]
    fn validate_cyclic_composition_returns_quickly() {
        for n in [3, 4, 6] {
            let mut b = folders();
            let branches: Vec<Value> = (0..n).map(|_| json!({"$ref": "#/$defs/A"})).collect();
            b.defs.insert("A".into(), json!({ "anyOf": branches }));
            op(&mut b, "GetFolder").outputs[0].schema = Some(json!({"$ref": "#/$defs/A"}));
            op(&mut b, "ListFolders").resource_type = None;
            op(&mut b, "CreateFolder").resource_type = None;
            let w = world(json!({
                "types": {"folder": {"identity": {"member": "x"}}},
                "operations": {"GetFolder": {"answer": {"records": "/a/0"}}}
            }));
            let start = std::time::Instant::now();
            let v = validate(&w, &b);
            assert!(
                start.elapsed() < std::time::Duration::from_secs(1),
                "{n} branches took {:?}",
                start.elapsed()
            );
            assert_eq!(
                paths(&v),
                vec![
                    ("/types/folder/identity/member", RefusalCode::Unresolved),
                    (
                        "/operations/GetFolder/answer/records",
                        RefusalCode::Unresolved
                    ),
                ]
            );
        }
    }

    #[test]
    fn validate_refuses_unique_scope_not_in_schema() {
        let w = world(json!({
            "types": {"folder": {"unique": [
                {"members": ["displayName"], "scope": "gone"},
                {"members": ["displayName"], "scope": "parent"}
            ]}}
        }));
        let v = validate(&w, &folders());
        assert_eq!(
            paths(&v),
            vec![("/types/folder/unique/0/scope", RefusalCode::Unresolved)]
        );
        let unique = &v.applied.types["folder"].unique;
        assert_eq!(unique.len(), 1);
        assert_eq!(unique[0].scope.as_deref(), Some("parent"));
    }

    #[test]
    fn validate_drops_sub_objects_left_empty() {
        let w = world(json!({
            "types": {"folder": {"concurrency": {"member": "nope"}}},
            "operations": {
                "GetFolder": {
                    "target": {"path_param": "nope"},
                    "answer": {"record": "/nope"}
                },
                "CreateFolder": {"request": {"records": "/nope"}},
                "ListFolders": {"target": {}}
            }
        }));
        let v = validate(&w, &folders());
        assert_eq!(v.refused.len(), 4, "{:?}", paths(&v));
        assert!(v.applied.types["folder"].concurrency.is_none());
        let get = &v.applied.operations["GetFolder"];
        assert!(get.target.is_none());
        assert!(get.answer.is_none());
        assert!(v.applied.operations["CreateFolder"].request.is_none());
        // An object authored empty stays: it is what the author wrote.
        assert_eq!(
            v.applied.operations["ListFolders"].target,
            Some(TargetDesc::default())
        );
    }

    #[test]
    fn validate_refuses_transition_without_lifecycle() {
        let w = world(json!({
            "operations": {"CreateFolder": {"transition": {"state": "ACTIVE"}}}
        }));
        let v = validate(&w, &folders());
        assert_eq!(
            paths(&v),
            vec![(
                "/operations/CreateFolder/transition/state",
                RefusalCode::Unresolved
            )]
        );
        assert!(v.applied.operations["CreateFolder"].transition.is_none());
    }

    #[test]
    fn validate_statuses_on_graphql_are_not_applicable() {
        let mut b = bundle(&[(
            "createIssue",
            "POST",
            "/graphql",
            json!({"type": "object", "properties": {"id": {"type": "string"}, "title": {"type": "string"}}}),
        )]);
        {
            let o = op(&mut b, "createIssue");
            o.protocol = Protocol::Graphql;
            o.binding = Binding::Graphql {
                operation_type: GraphqlOperationType::Mutation,
                field: "createIssue".into(),
            };
            o.resource_type = Some("issue".into());
        }
        let w = world(json!({
            "types": {"issue": {
                "unique": [{"members": ["title"], "conflict": 409}],
                "concurrency": {"member": "id", "conflict": 412}
            }},
            "operations": {"createIssue": {"not_found": 404}}
        }));
        let v = validate(&w, &b);
        assert_eq!(
            paths(&v),
            vec![
                ("/types/issue/unique/0/conflict", RefusalCode::NotApplicable),
                (
                    "/types/issue/concurrency/conflict",
                    RefusalCode::NotApplicable
                ),
                (
                    "/operations/createIssue/not_found",
                    RefusalCode::NotApplicable
                ),
            ]
        );
    }

    #[test]
    fn check_size_rejects_over_256k_and_depth_33() {
        let big = vec![b' '; MAX_BYTES + 1];
        let err = check_size(&big).unwrap_err();
        // Too large is its own code (an API answers 413), too deep is `invalid` (422).
        assert_eq!(err.code, RefusalCode::TooLarge);
        assert_eq!(err.code.as_str(), "too_large");
        assert_eq!(err.path, "");
        assert!(check_size(&vec![b' '; MAX_BYTES]).is_ok());

        let nested = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        assert!(check_size(nested(32).as_bytes()).is_ok());
        let err = check_size(nested(33).as_bytes()).unwrap_err();
        assert_eq!(err.code, RefusalCode::Invalid);
        assert_eq!(err.path, "/0".repeat(32));

        // Objects count too, the path names keys, and brackets in strings don't.
        let mut doc = String::from(r#"{"types":{"a/b":[1,{"x":"[[[[{{{{"#);
        doc.push_str(r#"","y":"#);
        for _ in 0..28 {
            doc.push('[');
        }
        doc.push_str(&"]".repeat(28));
        doc.push_str("}]}}");
        assert!(check_size(doc.as_bytes()).is_ok(), "depth 32");
        let deeper = doc
            .replacen(r#""y":"#, r#""y":["#, 1)
            .replacen("}]}}", "]}]}}", 1);
        let err = check_size(deeper.as_bytes()).unwrap_err();
        assert!(
            err.path.starts_with("/types/a~1b/1/y/0/0"),
            "path {}",
            err.path
        );
    }

    /// Templates: a read by `{id}`, a create answering 201 with a declared
    /// `Resource-ID` header, and widgets, which no operation identifies (so
    /// tier 0 gives the type no identity).
    fn templates() -> Bundle {
        let template = json!({"type": "object", "properties": {
            "TemplateId": {"type": "string"},
            "Alias": {"type": "string"},
            "Name": {"type": "string"}
        }});
        let widget = json!({"type": "object", "properties": {
            "uid": {"type": "string"},
            "label": {"type": "string"}
        }});
        let mut b = bundle(&[
            ("getTemplate", "GET", "/templates/{id}", template.clone()),
            ("createTemplate", "POST", "/templates", template),
            ("listWidgets", "GET", "/widgets", widget.clone()),
            ("createWidget", "POST", "/widgets", widget),
        ]);
        typed(&mut b, "getTemplate", "template", Some("id"), "TemplateId");
        typed(&mut b, "createTemplate", "template", None, "TemplateId");
        typed(&mut b, "listWidgets", "widget", None, "uid");
        typed(&mut b, "createWidget", "widget", None, "uid");
        let create = op(&mut b, "createTemplate");
        create.outputs[0].status = StatusSelector::Code(201);
        create.outputs[0]
            .headers
            .insert("Resource-ID".into(), json!({"schema": {"type": "string"}}));
        create.outputs.push(OutputVariant {
            status: StatusSelector::Code(400),
            content_type: None,
            schema: None,
            headers: [("X-Other".to_owned(), json!({}))].into_iter().collect(),
            description: None,
        });
        b
    }

    fn graphql_create() -> Bundle {
        let mut b = bundle(&[(
            "createIssue",
            "POST",
            "/graphql",
            json!({"type": "object", "properties": {"id": {"type": "string"}}}),
        )]);
        let o = op(&mut b, "createIssue");
        o.protocol = Protocol::Graphql;
        o.binding = Binding::Graphql {
            operation_type: GraphqlOperationType::Mutation,
            field: "createIssue".into(),
        };
        o.resource_type = Some("issue".into());
        b
    }

    #[test]
    fn alternates_must_be_members_of_the_record() {
        let v = validate(
            &world(json!({"types": {"template": {"identity": {
                "member": "TemplateId", "alternates": ["Alias", "Nope"]
            }}}})),
            &templates(),
        );
        let id = v.applied.types["template"].identity.as_ref().unwrap();
        assert_eq!(id.member.as_deref(), Some("TemplateId"));
        assert_eq!(id.alternates, Some(vec!["Alias".to_owned()]));
        assert_eq!(
            paths(&v),
            vec![(
                "/types/template/identity/alternates/1",
                RefusalCode::Unresolved
            )]
        );
        // Every alternate refused: the list is dropped, not kept as `[]`
        // (which would say the type has none).
        let v = validate(
            &world(json!({"types": {"template": {"identity": {"alternates": ["Nope"]}}}})),
            &templates(),
        );
        assert!(
            v.applied
                .types
                .get("template")
                .is_none_or(|t| t.identity.is_none())
        );
    }

    #[test]
    fn identity_header_must_be_a_declared_2xx_response_header() {
        let b = templates();
        let v = validate(
            &world(
                json!({"operations": {"createTemplate": {"answer": {"identity_header": "resource-id"}}}}),
            ),
            &b,
        );
        assert!(v.refused.is_empty(), "{:?}", v.refused);
        assert_eq!(
            v.applied.operations["createTemplate"]
                .answer
                .as_ref()
                .and_then(|a| a.identity_header.as_ref())
                .and_then(IdentityHeader::name),
            Some("resource-id")
        );
        // Declared, but only on a 400: not where a create names its id.
        let v = validate(
            &world(
                json!({"operations": {"createTemplate": {"answer": {"identity_header": "X-Other"}}}}),
            ),
            &b,
        );
        assert_eq!(
            paths(&v),
            vec![(
                "/operations/createTemplate/answer/identity_header",
                RefusalCode::Unresolved
            )]
        );
        assert!(
            v.applied
                .operations
                .get("createTemplate")
                .is_none_or(|o| o.answer.is_none())
        );
    }

    #[test]
    fn identity_header_is_not_applicable_off_http() {
        for header in [json!("Resource-ID"), json!(false)] {
            let v = validate(
                &world(
                    json!({"operations": {"createIssue": {"answer": {"identity_header": header}}}}),
                ),
                &graphql_create(),
            );
            assert_eq!(
                paths(&v),
                vec![(
                    "/operations/createIssue/answer/identity_header",
                    RefusalCode::NotApplicable
                )],
                "{header}"
            );
        }
    }

    #[test]
    fn identity_header_false_is_applied_without_a_header_check() {
        // `listWidgets`' answer declares no header at all.
        for id in ["createTemplate", "createWidget"] {
            let v = validate(
                &world(json!({"operations": {id: {"answer": {"identity_header": false}}}})),
                &templates(),
            );
            assert!(v.refused.is_empty(), "{:?}", v.refused);
            assert_eq!(
                v.applied.operations[id]
                    .answer
                    .as_ref()
                    .unwrap()
                    .identity_header,
                Some(IdentityHeader::Off)
            );
        }
        // `true` is not an identity header: the document does not deserialize
        // (the Admin API refuses it `invalid` at the entry).
        let doc = json!({"operations": {"createTemplate": {"answer": {"identity_header": true}}}});
        assert!(serde_json::from_value::<World>(doc).is_err());
        assert!(serde_json::from_value::<IdentityHeader>(json!(true)).is_err());
        assert_eq!(
            serde_json::to_value(IdentityHeader::Off).unwrap(),
            json!(false)
        );
        assert_eq!(
            serde_json::to_value(IdentityHeader::Named("Location".into())).unwrap(),
            json!("Location")
        );
    }

    #[test]
    fn an_identity_with_no_member_on_a_type_tier0_gave_none_is_refused() {
        let b = templates();
        let base = crate::world::tier0(&b);
        assert!(
            base.types
                .get("widget")
                .is_none_or(|t| t.identity.is_none())
        );
        let doc = world(json!({
            "types": {"widget": {"identity": {"alternates": ["label"]}}},
            "operations": {"createWidget": {"kind": "create"}}
        }));
        let (eff, refused) = crate::world::effective(&b, &base, &doc);
        assert_eq!(
            refused
                .iter()
                .map(|r| (r.path.as_str(), r.code))
                .collect::<Vec<_>>(),
            vec![("/types/widget/identity/member", RefusalCode::Invalid)]
        );
        assert!(refused[0].message.contains("set identity.member"));
        assert!(eff.types.get("widget").is_none_or(|t| t.identity.is_none()));
        // The rest of the revision applies.
        assert_eq!(eff.operations["createWidget"].kind, Some(OpKind::Create));
        // The same identity on a type tier 0 names a member for keeps it.
        let doc = world(json!({"types": {"template": {"identity": {"alternates": ["Alias"]}}}}));
        let (eff, refused) = crate::world::effective(&b, &base, &doc);
        assert!(refused.is_empty(), "{refused:?}");
        let id = eff.types["template"].identity.as_ref().unwrap();
        assert_eq!(id.member.as_deref(), Some("TemplateId"));
        assert_eq!(id.alternates, Some(vec!["Alias".to_owned()]));
    }

    #[test]
    fn empty_alternates_are_kept_as_set() {
        let b = templates();
        let base = crate::world::overlay(
            &crate::world::tier0(&b),
            &world(json!({"types": {"template": {"identity": {"alternates": ["Alias"]}}}})),
        );
        let doc = world(json!({"types": {"template": {"identity": {"alternates": []}}}}));
        let (eff, refused) = crate::world::effective(&b, &base, &doc);
        assert!(refused.is_empty(), "{refused:?}");
        let id = eff.types["template"].identity.as_ref().unwrap();
        assert_eq!(id.alternates, Some(vec![]));
        assert_eq!(id.member.as_deref(), Some("TemplateId"));
    }
}
