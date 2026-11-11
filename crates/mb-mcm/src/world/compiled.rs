//! The expressions of an effective world, compiled once: each operation's `set`
//! (with its `changes` rewritten as `set` entries) and `refuse`.
//!
//! A runtime compiles its effective world's expressions once, when it is built, and
//! evaluates the [`Program`]s per call. An entry that does not compile is left out
//! (validation refuses it before it reaches an effective world).
//!
//! `changes` is read as `set`: `{from_request: /a/0/b}` reads the request body at
//! that JSON pointer, as `changes` always did, and `{value: v}` is the literal `v`.
//! A `set` entry for the same member replaces the `changes` one.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use super::expr::Program;
use super::{ChangeDesc, OpDesc, Pointer, TextDesc, World};

/// The compiled expressions of a world's operations, by operation id.
#[derive(Debug, Default)]
pub struct Expressions {
    ops: HashMap<String, OpExpressions>,
}

/// The compiled expressions of one operation.
#[derive(Debug, Default)]
pub struct OpExpressions {
    /// What the operation sets on the record it stores, in member order (a
    /// `changes` entry where no `set` entry names its member).
    pub set: Vec<SetEntry>,
    /// Its refusals, in order.
    pub refuse: Vec<RefuseEntry>,
    /// The resource types its expressions read records of (`held`, `count`,
    /// `exists`).
    pub types: Vec<String>,
    /// Whether an expression of it calls `held`.
    pub reads_held: bool,
}

/// One member an operation sets.
#[derive(Debug)]
pub struct SetEntry {
    /// The member path.
    pub member: String,
    /// Where the entry is in the world, as a JSON pointer
    /// (`/operations/<op>/set/<member>`, or `.../changes/<member>`).
    pub pointer: String,
    /// Its value.
    pub value: SetValue,
    /// Whether it is a `changes` entry, which a create does not apply (`changes`
    /// never applied to creates).
    pub from_changes: bool,
}

/// The value a [`SetEntry`] sets.
#[derive(Debug)]
pub enum SetValue {
    /// A fixed value (`changes.<member>.value`).
    Literal(Value),
    /// The request body at a JSON pointer (`changes.<member>.from_request`), read as
    /// a JSON pointer reads (RFC 6901): a token of digits names an object's member
    /// as well as an array's item, a leading zero is no index, and an explicit `null`
    /// is a value.
    Pointer(Pointer),
    /// An expression's value.
    Expr(Program),
}

/// One refusal of an operation.
#[derive(Debug)]
pub struct RefuseEntry {
    /// Where the entry is in the world (`/operations/<op>/refuse/<i>`).
    pub pointer: String,
    /// The condition.
    pub when: Program,
    /// The status it answers.
    pub status: u16,
    /// The code its body says.
    pub code: Option<TextValue>,
    /// The message its body says.
    pub message: Option<TextValue>,
}

/// A refusal's code or message.
#[derive(Debug)]
pub enum TextValue {
    /// As written.
    Text(String),
    /// An expression's value.
    Expr(Program),
}

impl Expressions {
    /// Compiles the `set`, `changes` and `refuse` entries of every operation of
    /// `world`.
    #[must_use]
    pub fn compile(world: &World) -> Self {
        let ops = world
            .operations
            .iter()
            .filter_map(|(id, d)| {
                let compiled = OpExpressions::compile(id, d);
                (!compiled.set.is_empty() || !compiled.refuse.is_empty())
                    .then(|| (id.clone(), compiled))
            })
            .collect();
        Self { ops }
    }

    /// The compiled expressions of operation `id`, when it has any.
    #[must_use]
    pub fn op(&self, id: &str) -> Option<&OpExpressions> {
        self.ops.get(id)
    }
}

impl OpExpressions {
    fn compile(id: &str, d: &OpDesc) -> Self {
        let base = format!("/operations/{}", escape(id));
        let mut set: BTreeMap<&str, SetEntry> = BTreeMap::new();
        for (member, value) in changes_as_set(&d.changes) {
            let pointer = format!("{base}/changes/{}", escape(member));
            set.insert(
                member,
                SetEntry {
                    member: member.to_string(),
                    pointer,
                    value,
                    from_changes: true,
                },
            );
        }
        for (member, source) in &d.set {
            let Ok(program) = Program::compile(source) else {
                continue;
            };
            set.insert(
                member,
                SetEntry {
                    member: member.clone(),
                    pointer: format!("{base}/set/{}", escape(member)),
                    value: SetValue::Expr(program),
                    from_changes: false,
                },
            );
        }
        let text = |t: &Option<TextDesc>| -> Result<Option<TextValue>, ()> {
            match t {
                None => Ok(None),
                Some(TextDesc::Text(s)) => Ok(Some(TextValue::Text(s.clone()))),
                Some(TextDesc::Expr { expr }) => Program::compile(expr)
                    .map(|p| Some(TextValue::Expr(p)))
                    .map_err(|_| ()),
            }
        };
        let refuse = d
            .refuse
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                Some(RefuseEntry {
                    pointer: format!("{base}/refuse/{i}"),
                    when: Program::compile(&r.when).ok()?,
                    status: r.status,
                    code: text(&r.code).ok()?,
                    message: text(&r.message).ok()?,
                })
            })
            .collect::<Vec<_>>();
        let mut out = Self {
            set: set.into_values().collect(),
            refuse,
            types: Vec::new(),
            reads_held: false,
        };
        let mut types: Vec<String> = Vec::new();
        for program in out.programs() {
            for t in program.check().types {
                if !types.contains(&t.name) {
                    types.push(t.name);
                }
            }
        }
        out.types = types;
        let reads_held = out
            .programs()
            .any(|p| p.check().functions.iter().any(|f| f.name == "held"));
        out.reads_held = reads_held;
        out
    }

    /// Every program of the operation.
    fn programs(&self) -> impl Iterator<Item = &Program> {
        let set = self.set.iter().filter_map(|e| match &e.value {
            SetValue::Expr(p) => Some(p),
            SetValue::Literal(_) | SetValue::Pointer(_) => None,
        });
        let refuse = self.refuse.iter().flat_map(|r| {
            let texts = [&r.code, &r.message].into_iter().filter_map(|t| match t {
                Some(TextValue::Expr(p)) => Some(p),
                _ => None,
            });
            std::iter::once(&r.when).chain(texts)
        });
        set.chain(refuse)
    }

    /// Whether an expression of the operation calls a session function (`held`,
    /// `count`, `exists`, `seq`, `mint`).
    #[must_use]
    pub fn reads_session(&self) -> bool {
        self.programs().any(|p| {
            p.check()
                .functions
                .iter()
                .any(|f| matches!(f.name, "held" | "count" | "exists" | "seq" | "mint"))
        })
    }
}

/// `changes` as `set` entries, by member: `from_request` as the request body at its
/// pointer, `value` as that literal.
#[must_use]
pub fn changes_as_set(changes: &BTreeMap<String, ChangeDesc>) -> Vec<(&str, SetValue)> {
    changes
        .iter()
        .map(|(member, change)| {
            let value = match change {
                ChangeDesc::Value(v) => SetValue::Literal(v.clone()),
                ChangeDesc::FromRequest(p) => SetValue::Pointer(p.clone()),
            };
            (member.as_str(), value)
        })
        .collect()
}

/// `token` as one step of a JSON pointer.
fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn changes_are_set_entries_a_set_entry_replaces() {
        let world: World = serde_json::from_value(json!({"operations": {"op": {
            "changes": {"a": {"from_request": "/x"}, "b": {"value": [1, {"c": null}]}},
            "set": {"b": "2", "d": "request.body.x + 1"},
            "refuse": [{"when": "request.body.x > 1", "status": 409, "code": "taken", "message": {"expr": "'x is ' + string(request.body.x)"}}]
        }}}))
        .unwrap();
        let compiled = Expressions::compile(&world);
        let op = compiled.op("op").unwrap();
        let members: Vec<(&str, &str)> = op
            .set
            .iter()
            .map(|e| (e.member.as_str(), e.pointer.as_str()))
            .collect();
        assert_eq!(
            members,
            vec![
                ("a", "/operations/op/changes/a"),
                ("b", "/operations/op/set/b"),
                ("d", "/operations/op/set/d")
            ]
        );
        assert!(matches!(op.set[0].value, SetValue::Pointer(ref p) if p.as_str() == "/x"));
        assert_eq!(op.refuse.len(), 1);
        assert!(matches!(op.refuse[0].code, Some(TextValue::Text(ref t)) if t == "taken"));
        assert!(matches!(op.refuse[0].message, Some(TextValue::Expr(_))));
        assert!(!op.reads_session());
        assert!(!op.reads_held);
        assert!(compiled.op("other").is_none());
    }
}
