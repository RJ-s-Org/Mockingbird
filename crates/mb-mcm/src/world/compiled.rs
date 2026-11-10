//! The expressions of an effective world, compiled once: each operation's `set`
//! (with its `changes` rewritten as `set` entries) and `refuse`.
//!
//! A runtime compiles its effective world's expressions once, when it is built, and
//! evaluates the [`Program`]s per call. An entry that does not compile is left out
//! (validation refuses it before it reaches an effective world).
//!
//! `changes` is read as `set`: `{from_request: /a/0/b}` is the expression
//! `request.body.a[0].b` ([`pointer_expression`]) and `{value: v}` the literal `v`.
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
        out
    }

    /// Every program of the operation.
    fn programs(&self) -> impl Iterator<Item = &Program> {
        let set = self.set.iter().filter_map(|e| match &e.value {
            SetValue::Expr(p) => Some(p),
            SetValue::Literal(_) => None,
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

/// `changes` as `set` entries, by member: `from_request` as the expression reading
/// the request body's member at its pointer ([`pointer_expression`]), `value` as
/// that literal. An entry whose expression does not compile is left out.
#[must_use]
pub fn changes_as_set(changes: &BTreeMap<String, ChangeDesc>) -> Vec<(&str, SetValue)> {
    changes
        .iter()
        .filter_map(|(member, change)| {
            let value = match change {
                ChangeDesc::Value(v) => SetValue::Literal(v.clone()),
                ChangeDesc::FromRequest(p) => {
                    SetValue::Expr(Program::compile(&pointer_expression(p)).ok()?)
                }
            };
            Some((member.as_str(), value))
        })
        .collect()
}

/// The expression reading the request body at `pointer`: each token a member
/// (`.name`, or `['na-me']` when it is no identifier), a token of digits an index
/// (`[0]`). `""` is `request.body`.
#[must_use]
pub fn pointer_expression(pointer: &Pointer) -> String {
    let mut out = String::from("request.body");
    for token in pointer.tokens() {
        if !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit()) && token.len() < 19 {
            out.push('[');
            out.push_str(&token);
            out.push(']');
        } else if identifier(&token) {
            out.push('.');
            out.push_str(&token);
        } else {
            out.push_str("['");
            for c in token.chars() {
                match c {
                    '\'' => out.push_str("\\'"),
                    '\\' => out.push_str("\\\\"),
                    c if c.is_control() => out.push_str(&format!("\\U{:08x}", u32::from(c))),
                    c => out.push(c),
                }
            }
            out.push_str("']");
        }
    }
    out
}

/// Words CEL reserves, which a member written `.name` cannot be.
const RESERVED: [&str; 21] = [
    "as",
    "break",
    "const",
    "continue",
    "else",
    "false",
    "for",
    "function",
    "if",
    "import",
    "in",
    "let",
    "loop",
    "package",
    "namespace",
    "null",
    "return",
    "true",
    "var",
    "void",
    "while",
];

fn identifier(token: &str) -> bool {
    let mut chars = token.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !RESERVED.contains(&token)
}

/// `token` as one step of a JSON pointer.
fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::expr::{Bindings, Host};
    use serde_json::json;

    struct NoSession;
    impl Host for NoSession {
        fn held(&mut self, _: &str, _: &Value) -> Option<Value> {
            None
        }
        fn all(&mut self, _: &str) -> Vec<Value> {
            Vec::new()
        }
        fn seq(&mut self, _: &str) -> i64 {
            1
        }
        fn mint(&mut self, _: &str) -> Option<String> {
            None
        }
    }

    fn read(pointer: &str, body: Value) -> Value {
        let p = Pointer::parse(pointer).unwrap();
        let source = pointer_expression(&p);
        let program = Program::compile(&source).unwrap_or_else(|e| panic!("{source}: {e}"));
        let request = json!({"body": body});
        let b = Bindings {
            request: &request,
            record: &Value::Null,
            now: &Value::Null,
            it: None,
        };
        program.eval(&b, &mut NoSession).unwrap()
    }

    #[test]
    fn a_pointer_reads_as_its_member_path() {
        let p = |s: &str| pointer_expression(&Pointer::parse(s).unwrap());
        assert_eq!(p("/a/b"), "request.body.a.b");
        assert_eq!(p("/items/0/name"), "request.body.items[0].name");
        assert_eq!(p("/x-y/in"), "request.body['x-y']['in']");
        assert_eq!(p("/a~1b/it's"), "request.body['a/b']['it\\'s']");
        let body = json!({"a/b": {"it's": 1}, "items": [{"name": "n"}], "x-y": {"in": true}});
        assert_eq!(read("/a~1b/it's", body.clone()), json!(1));
        assert_eq!(read("/items/0/name", body.clone()), json!("n"));
        assert_eq!(read("/x-y/in", body.clone()), json!(true));
        assert_eq!(read("/missing/deep", body), Value::Null);
    }

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
        assert_eq!(op.refuse.len(), 1);
        assert!(matches!(op.refuse[0].code, Some(TextValue::Text(ref t)) if t == "taken"));
        assert!(matches!(op.refuse[0].message, Some(TextValue::Expr(_))));
        assert!(!op.reads_session());
        assert!(compiled.op("other").is_none());
    }
}
