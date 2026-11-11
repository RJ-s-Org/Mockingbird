//! The expressions of an effective world, compiled once: each operation's `set`
//! (with its `changes` rewritten as `set` entries), `refuse`, `effects` and `list`
//! (`where` and `sort.by`), and each
//! type's `computed` members (with its `derived` members rewritten as `computed`
//! entries).
//!
//! A runtime compiles its effective world's expressions once, when it is built, and
//! evaluates the [`Program`]s per call. An entry that does not compile is left out
//! (validation refuses it before it reaches an effective world).
//!
//! `changes` is read as `set`: `{from_request: /a/0/b}` reads the request body at
//! that JSON pointer, as `changes` always did, and `{value: v}` is the literal `v`.
//! A `set` entry for the same member replaces the `changes` one.
//!
//! `derived` is read as `computed`, through two functions only the runtime calls
//! ([`Program::compile_internal`]): `children(type, where?)`, the held children of
//! the record being answered as `derived` has always scoped them (by the child type's
//! `parent.member`, or the collection its `parent.path_param` names, ids compared bare,
//! soft-deleted children left out, none when the listing is cut at 500), which no
//! expression over `count` can state; and `decimal_sum(list, member)`, `derived`'s
//! exact decimal sum. `{kind: count, of}` is `size(children(of))`, and `{kind: sum,
//! of, member, where?}` is `decimal_sum(children(of, where), member)`. A `computed`
//! entry for the same member replaces the `derived` one.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use super::expr::{Program, Root, Segment};
use super::{
    ChangeDesc, DerivedDesc, EffectOp, MAX_EFFECTS, OpDesc, Pointer, TextDesc, TypeDesc, World,
};

/// Child types one type's `derived` members read, at most; members over further
/// child types are not set.
pub const MAX_DERIVED_CHILD_TYPES: usize = 4;

/// The compiled expressions of a world's operations, by operation id, and of its
/// types, by type.
#[derive(Debug, Default)]
pub struct Expressions {
    ops: HashMap<String, OpExpressions>,
    types: HashMap<String, TypeExpressions>,
}

/// The compiled `computed` members of one type (its `derived` ones read as
/// `computed`), in member order.
#[derive(Debug, Default)]
pub struct TypeExpressions {
    /// The members, in member order.
    pub computed: Vec<ComputedEntry>,
    /// The child types the `derived` members read (`children()`), in the order they
    /// are first named, at most [`MAX_DERIVED_CHILD_TYPES`].
    pub children: Vec<String>,
    /// The resource types its expressions read records of (`held`, `count`,
    /// `exists`).
    pub types: Vec<String>,
    /// Whether an expression of it calls `held`.
    pub reads_held: bool,
}

/// One computed member.
#[derive(Debug)]
pub struct ComputedEntry {
    /// The member path.
    pub member: String,
    /// Where the entry is in the world (`/types/<T>/computed/<member>`, or
    /// `.../derived/<member>`).
    pub pointer: String,
    /// Its value.
    pub program: Program,
    /// Whether it is a `derived` member: set only on a record created in the session
    /// or with a child held, its decimal text written as its member declares
    /// ([`super::expr::Decimal::to_value`]), and a failure not reported.
    pub derived: bool,
}

/// One effect of an operation.
#[derive(Debug)]
pub struct EffectEntry {
    /// Where the entry is in the world (`/operations/<op>/effects/<i>`).
    pub pointer: String,
    /// What it does.
    pub op: EffectOp,
    /// The type of the record it writes.
    pub r#type: String,
    /// The id of the record an update or delete acts on.
    pub id: Option<Program>,
    /// The members it sets, in member order: each member, its pointer and value.
    pub set: Vec<(String, String, Program)>,
    /// Its condition.
    pub when: Option<Program>,
}

/// The compiled expressions of one operation.
#[derive(Debug, Default)]
pub struct OpExpressions {
    /// What the operation sets on the record it stores, in member order (a
    /// `changes` entry where no `set` entry names its member).
    pub set: Vec<SetEntry>,
    /// Its refusals, in order.
    pub refuse: Vec<RefuseEntry>,
    /// Its effects, in order.
    pub effects: Vec<EffectEntry>,
    /// A list's default item filter (`list.where`), `it` bound to each record.
    pub list_where: Option<ListWhere>,
    /// A list's sort parameter (`list.sort`).
    pub list_sort: Option<ListSort>,
    /// The resource types its expressions read records of (`held`, `count`,
    /// `exists`).
    pub types: Vec<String>,
    /// Whether an expression of it calls `held`.
    pub reads_held: bool,
    /// Whether an expression of it calls `seq`.
    pub reads_seq: bool,
}

/// A list's default item filter.
#[derive(Debug)]
pub struct ListWhere {
    /// Where it is in the world (`/operations/<op>/list/where`).
    pub pointer: String,
    /// The condition.
    pub program: Program,
}

/// A list's sort parameter.
#[derive(Debug)]
pub struct ListSort {
    /// Where `by` is in the world (`/operations/<op>/list/sort/by`).
    pub pointer: String,
    /// The request parameter.
    pub param: String,
    /// The members to order by, `[{member, desc?}]`, with `value` bound to the
    /// parameter as sent.
    pub by: Program,
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
    /// Compiles the `set`, `changes`, `refuse` and `effects` entries of every
    /// operation of `world`, and the `computed` and `derived` members of every type.
    #[must_use]
    pub fn compile(world: &World) -> Self {
        let ops = world
            .operations
            .iter()
            .filter_map(|(id, d)| {
                let compiled = OpExpressions::compile(id, d);
                (!compiled.set.is_empty()
                    || !compiled.refuse.is_empty()
                    || !compiled.effects.is_empty()
                    || compiled.list_where.is_some()
                    || compiled.list_sort.is_some())
                .then(|| (id.clone(), compiled))
            })
            .collect();
        let types = world
            .types
            .iter()
            .filter_map(|(t, d)| {
                let compiled = TypeExpressions::compile(t, d);
                (!compiled.computed.is_empty()).then(|| (t.clone(), compiled))
            })
            .collect();
        Self { ops, types }
    }

    /// The compiled expressions of operation `id`, when it has any.
    #[must_use]
    pub fn op(&self, id: &str) -> Option<&OpExpressions> {
        self.ops.get(id)
    }

    /// The compiled computed members of type `t`, when it has any.
    #[must_use]
    pub fn of_type(&self, t: &str) -> Option<&TypeExpressions> {
        self.types.get(t)
    }
}

impl TypeExpressions {
    fn compile(t: &str, d: &TypeDesc) -> Self {
        let base = format!("/types/{}", escape(t));
        let mut children: Vec<String> = Vec::new();
        for derived in d.derived.values() {
            let of = derived_child(derived);
            if !children.iter().any(|c| c == of) {
                children.push(of.to_string());
            }
        }
        children.truncate(MAX_DERIVED_CHILD_TYPES);
        let mut computed: BTreeMap<&str, ComputedEntry> = BTreeMap::new();
        for (member, derived) in &d.derived {
            if !children.iter().any(|c| c == derived_child(derived)) {
                continue;
            }
            let Ok(program) = Program::compile_internal(&derived_source(derived)) else {
                continue;
            };
            computed.insert(
                member,
                ComputedEntry {
                    member: member.clone(),
                    pointer: format!("{base}/derived/{}", escape(member)),
                    program,
                    derived: true,
                },
            );
        }
        for (member, source) in &d.computed {
            let Ok(program) = Program::compile(source) else {
                continue;
            };
            computed.insert(
                member,
                ComputedEntry {
                    member: member.clone(),
                    pointer: format!("{base}/computed/{}", escape(member)),
                    program,
                    derived: false,
                },
            );
        }
        let computed: Vec<ComputedEntry> = computed.into_values().collect();
        // Only the child types a member still reads.
        children.retain(|c| {
            computed.iter().any(|e| {
                e.derived
                    && d.derived
                        .get(&e.member)
                        .is_some_and(|derived| derived_child(derived) == c)
            })
        });
        let (types, reads_held) = reads(computed.iter().map(|e| &e.program));
        Self {
            computed,
            children,
            types,
            reads_held,
        }
    }
}

/// The child type a `derived` member is over.
fn derived_child(d: &DerivedDesc) -> &str {
    match d {
        DerivedDesc::Count { of, .. } | DerivedDesc::Sum { of, .. } => of,
    }
}

/// `derived` member `d` as the internal expression it is read as.
fn derived_source(d: &DerivedDesc) -> String {
    match d {
        DerivedDesc::Count { of, .. } => format!("size(children({}))", quoted(of)),
        DerivedDesc::Sum {
            of,
            member,
            r#where,
            ..
        } => {
            let children = match r#where {
                Some(w) => {
                    let w = serde_json::to_string(w).unwrap_or_default();
                    format!("children({}, {})", quoted(of), quoted(&w))
                }
                None => format!("children({})", quoted(of)),
            };
            format!("decimal_sum({children}, {})", quoted(member))
        }
    }
}

/// `text` as an expression's string literal.
fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

/// The resource types `programs` read records of (`held`, `count`, `exists`), in the
/// order they are named, and whether one calls `held`.
fn reads<'a>(programs: impl Iterator<Item = &'a Program>) -> (Vec<String>, bool) {
    let mut types: Vec<String> = Vec::new();
    let mut held = false;
    for program in programs {
        let refs = program.check();
        for t in refs.types {
            if !types.contains(&t.name) {
                types.push(t.name);
            }
        }
        held |= refs.functions.iter().any(|f| f.name == "held");
    }
    (types, held)
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
        let effects = d
            .effects
            .iter()
            .take(MAX_EFFECTS)
            .enumerate()
            .filter_map(|(i, e)| {
                let pointer = format!("{base}/effects/{i}");
                let compile = |source: &Option<String>| -> Result<Option<Program>, ()> {
                    source
                        .as_deref()
                        .map(Program::compile)
                        .transpose()
                        .map_err(|_| ())
                };
                let set = e
                    .set
                    .iter()
                    .map(|(member, source)| {
                        let at = format!("{pointer}/set/{}", escape(member));
                        Program::compile(source)
                            .ok()
                            .map(|p| (member.clone(), at, p))
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(EffectEntry {
                    op: e.op,
                    r#type: e.r#type.clone(),
                    id: compile(&e.id).ok()?,
                    when: compile(&e.when).ok()?,
                    set,
                    pointer,
                })
            })
            .collect();
        let list = d.list.as_ref();
        let list_where = list
            .and_then(|l| l.r#where.as_deref())
            .and_then(|w| Program::compile(w).ok())
            .map(|program| ListWhere {
                pointer: format!("{base}/list/where"),
                program,
            });
        let list_sort = list.and_then(|l| l.sort.as_ref()).and_then(|s| {
            Some(ListSort {
                pointer: format!("{base}/list/sort/by"),
                param: s.param.clone(),
                by: Program::compile(&s.by).ok()?,
            })
        });
        let mut out = Self {
            set: set.into_values().collect(),
            refuse,
            effects,
            list_where,
            list_sort,
            types: Vec::new(),
            reads_held: false,
            reads_seq: false,
        };
        let (types, reads_held) = reads(out.programs());
        out.types = types;
        out.reads_held = reads_held;
        let reads_seq = out
            .programs()
            .any(|p| p.check().functions.iter().any(|f| f.name == "seq"));
        out.reads_seq = reads_seq;
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
        let effects = self.effects.iter().flat_map(|e| {
            e.id.iter()
                .chain(e.when.iter())
                .chain(e.set.iter().map(|(_, _, p)| p))
        });
        let list = self
            .list_where
            .iter()
            .map(|w| &w.program)
            .chain(self.list_sort.iter().map(|s| &s.by));
        set.chain(refuse).chain(effects).chain(list)
    }

    /// Whether a `set` or `changes` entry reads the request body's top-level member
    /// `name` (`request.body.<name>` in an expression, `from_request: /<name>`): the
    /// description says where that member lands, whatever the record calls it.
    #[must_use]
    pub fn sets_from_body(&self, name: &str) -> bool {
        self.set.iter().any(|entry| match &entry.value {
            SetValue::Literal(_) => false,
            SetValue::Pointer(p) => p.tokens().first().is_some_and(|t| t == name),
            SetValue::Expr(p) => p.check().members.iter().any(|m| {
                matches!(m.root, Root::Body)
                    && matches!(m.path.first(), Some(Segment::Member(n)) if n == name)
            }),
        })
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
    fn sets_from_body_names_the_top_level_members_set_reads() {
        let world: World = serde_json::from_value(json!({"operations": {"op": {
            "changes": {"a": {"from_request": "/FromPointer/deep"}},
            "set": {"b": "request.body.SmsUrl != null ? request.body.SmsUrl : record.b",
                    "c": "request.query.Q", "d": "record.Inner.x", "e": "'lit'"}
        }}}))
        .unwrap();
        let compiled = Expressions::compile(&world);
        let op = compiled.op("op").unwrap();
        assert!(op.sets_from_body("SmsUrl"));
        assert!(op.sets_from_body("FromPointer"));
        for name in ["deep", "Q", "Inner", "x", "b", "lit"] {
            assert!(!op.sets_from_body(name), "{name}");
        }
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
        assert!(matches!(op.set[0].value, SetValue::Pointer(ref p) if p.as_str() == "/x"));
        assert_eq!(op.refuse.len(), 1);
        assert!(matches!(op.refuse[0].code, Some(TextValue::Text(ref t)) if t == "taken"));
        assert!(matches!(op.refuse[0].message, Some(TextValue::Expr(_))));
        assert!(!op.reads_session());
        assert!(!op.reads_held);
        assert!(compiled.op("other").is_none());
    }

    #[test]
    fn derived_is_read_as_computed_and_a_computed_entry_replaces_it() {
        let world: World = serde_json::from_value(json!({"types": {"orders": {
            "derived": {
                "n": {"kind": "count", "of": "lines"},
                "paid": {"kind": "sum", "of": "lines", "member": "amount", "where": {"status": "it's \"posted\""}},
                "total": {"kind": "sum", "of": "lines", "member": "price"},
                "label": {"kind": "count", "of": "notes"}
            },
            "computed": {"label": "record.name + '!'", "other": "1 + 1"}
        }}}))
        .unwrap();
        let compiled = Expressions::compile(&world);
        let orders = compiled.of_type("orders").unwrap();
        let entries: Vec<(&str, &str, bool, &str)> = orders
            .computed
            .iter()
            .map(|e| {
                (
                    e.member.as_str(),
                    e.pointer.as_str(),
                    e.derived,
                    e.program.source(),
                )
            })
            .collect();
        assert_eq!(
            entries,
            vec![
                (
                    "label",
                    "/types/orders/computed/label",
                    false,
                    "record.name + '!'"
                ),
                (
                    "n",
                    "/types/orders/derived/n",
                    true,
                    "size(children(\"lines\"))"
                ),
                ("other", "/types/orders/computed/other", false, "1 + 1"),
                (
                    "paid",
                    "/types/orders/derived/paid",
                    true,
                    r#"decimal_sum(children("lines", "{\"status\":\"it's \\\"posted\\\"\"}"), "amount")"#
                ),
                (
                    "total",
                    "/types/orders/derived/total",
                    true,
                    r#"decimal_sum(children("lines"), "price")"#
                ),
            ]
        );
        // `notes` is read by no member left.
        assert_eq!(orders.children, vec!["lines".to_string()]);
        assert!(compiled.of_type("lines").is_none());
    }

    /// A host answering `children()` with fixed records.
    struct Kids(Vec<Value>);

    impl crate::world::expr::Host for Kids {
        fn held(&mut self, _: &str, _: &Value) -> Option<Value> {
            None
        }
        fn all(&mut self, _: &str) -> Vec<Value> {
            Vec::new()
        }
        fn seq(&mut self, _: &str) -> i64 {
            0
        }
        fn mint(&mut self, _: &str) -> Option<String> {
            None
        }
        fn children(&mut self, _: &str, r#where: &Value) -> Result<Vec<Value>, String> {
            let wanted: Option<serde_json::Map<String, Value>> = match r#where {
                Value::String(text) => serde_json::from_str(text).ok(),
                _ => None,
            };
            Ok(self
                .0
                .iter()
                .filter(|c| wanted.iter().flatten().all(|(k, v)| c.get(k) == Some(v)))
                .cloned()
                .collect())
        }
    }

    #[test]
    fn a_derived_sum_is_exact_decimal_text_and_a_count_an_integer() {
        let world: World = serde_json::from_value(json!({"types": {"orders": {"derived": {
            "n": {"kind": "count", "of": "lines"},
            "paid": {"kind": "sum", "of": "lines", "member": "amount", "where": {"status": "posted"}},
            "total": {"kind": "sum", "of": "lines", "member": "price"}
        }}}}))
        .unwrap();
        let compiled = Expressions::compile(&world);
        let orders = compiled.of_type("orders").unwrap();
        let mut host = Kids(vec![
            json!({"amount": 0.1, "price": "12.50", "status": "posted"}),
            json!({"amount": 0.2, "price": "1.25", "status": "posted"}),
            json!({"amount": 5, "price": null, "status": "pending"}),
        ]);
        let null = Value::Null;
        let b = crate::world::expr::Bindings {
            request: &null,
            record: &null,
            now: &null,
            it: None,
            value: None,
        };
        let values: Vec<Value> = orders
            .computed
            .iter()
            .map(|e| e.program.eval(&b, &mut host).unwrap())
            .collect();
        assert_eq!(values, vec![json!(3), json!("0.3"), json!("13.75")]);
    }
}
