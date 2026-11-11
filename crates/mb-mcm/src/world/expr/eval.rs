//! The tree-walking evaluator over `serde_json::Value`, with a step budget.

use std::cell::Cell;
use std::cmp::Ordering;

use serde_json::{Map, Value};

use super::parse::{BinOp, CompOp, Func, Kind, Node};
use super::{Bindings, Fail, Host, funcs};

/// Bytes of data copied or built per extra step, so that the step budget also
/// bounds memory.
const BYTES_PER_STEP: u64 = 1024;

/// The steps a `matches` whose pattern is computed costs for compiling it.
const REGEX_COMPILE_STEPS: u64 = 50;

static NULL: Value = Value::Null;

/// A number as the evaluator sees it: a number whose value is integral and fits
/// `i64` is an int, however it is spelled (`2` or `2.0`); any other a double.
#[derive(Debug, Clone, Copy)]
pub(super) enum Num {
    I(i64),
    D(f64),
}

impl Num {
    pub fn of(v: &Value) -> Option<Num> {
        let Value::Number(n) = v else { return None };
        match n.as_i64() {
            Some(i) => Some(Num::I(i)),
            None => n.as_f64().map(Num::from_f64),
        }
    }

    /// An integral `d` in range is an int (the cast is then exact).
    fn from_f64(d: f64) -> Num {
        if d.fract() == 0.0
            && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&d)
        {
            Num::I(d as i64)
        } else {
            Num::D(d)
        }
    }

    fn f(self) -> f64 {
        match self {
            Num::I(i) => i as f64,
            Num::D(d) => d,
        }
    }
}

/// A computed double as a JSON value: an int when it is integral and in range;
/// NaN and infinities are errors.
pub(super) fn double(d: f64, pos: usize) -> Result<Value, Fail> {
    if !d.is_finite() {
        return Err(Fail::new(pos, "the result is not a finite number"));
    }
    match Num::from_f64(d) {
        Num::I(i) => Ok(Value::from(i)),
        Num::D(d) => serde_json::Number::from_f64(d)
            .map(Value::Number)
            .ok_or_else(|| Fail::new(pos, "the result is not a finite number")),
    }
}

pub(super) fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => match Num::of(v) {
            Some(Num::I(_)) => "int",
            _ => "double",
        },
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "map",
    }
}

pub(super) struct Eval<'p, 'b, 'h> {
    pub bindings: &'b Bindings<'b>,
    pub host: &'h mut dyn Host,
    /// Steps taken; a `Cell` so that a value borrowed from `self` can be
    /// charged for before it is cloned.
    pub steps: Cell<u64>,
    pub max_steps: u64,
    /// Macro variables in scope, innermost last.
    pub scope: Vec<(&'p str, Value)>,
}

/// One step of a member/index chain, with its index already evaluated.
enum Key<'p> {
    Name(&'p str),
    Value(Value),
}

impl<'p> Eval<'p, '_, '_> {
    fn step(&self, pos: usize) -> Result<(), Fail> {
        self.steps_n(1, pos)
    }

    fn steps_n(&self, n: u64, pos: usize) -> Result<(), Fail> {
        self.steps.set(self.steps.get().saturating_add(n));
        if self.steps.get() > self.max_steps {
            return Err(Fail::new(
                pos,
                format!("evaluation exceeded its budget of {} steps", self.max_steps),
            ));
        }
        Ok(())
    }

    /// Charges for a value copied or built: one step per [`BYTES_PER_STEP`].
    fn charge(&self, v: &Value, pos: usize) -> Result<(), Fail> {
        match weight(v) / BYTES_PER_STEP {
            0 => Ok(()),
            n => self.steps_n(n, pos),
        }
    }

    /// Clones a value (which may borrow from `self`), charging for its size
    /// before the copy is made.
    fn copy_ref(&self, v: &Value, pos: usize) -> Result<Value, Fail> {
        self.charge(v, pos)?;
        Ok(v.clone())
    }

    fn lookup(&self, name: &str, pos: usize) -> Result<&Value, Fail> {
        if let Some((_, v)) = self.scope.iter().rev().find(|(n, _)| *n == name) {
            return Ok(v);
        }
        match name {
            "request" => Ok(self.bindings.request),
            "record" => Ok(self.bindings.record),
            "now" => Ok(self.bindings.now),
            "it" => self
                .bindings
                .it
                .ok_or_else(|| Fail::new(pos, "'it' is not bound here")),
            _ => Err(Fail::new(pos, format!("unknown name '{name}'"))),
        }
    }

    pub fn eval(&mut self, n: &'p Node) -> Result<Value, Fail> {
        self.step(n.pos)?;
        let pos = n.pos;
        match &n.kind {
            Kind::Lit(v) => self.copy_ref(v, pos),
            Kind::Ident(name) => {
                let v = self.lookup(name, pos)?;
                self.copy_ref(v, pos)
            }
            Kind::Member(..) | Kind::Index(..) => self.chain(n),
            Kind::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for i in items {
                    out.push(self.eval(i)?);
                }
                Ok(Value::Array(out))
            }
            Kind::Map(kvs) => {
                let mut out = Map::new();
                for (k, v) in kvs {
                    let Value::String(key) = self.eval(k)? else {
                        return Err(Fail::new(k.pos, "map keys must be strings"));
                    };
                    let val = self.eval(v)?;
                    if out.insert(key.clone(), val).is_some() {
                        return Err(Fail::new(k.pos, format!("duplicate map key '{key}'")));
                    }
                }
                Ok(Value::Object(out))
            }
            Kind::Not(a) => match self.eval(a)? {
                Value::Bool(b) => Ok(Value::Bool(!b)),
                v => Err(Fail::new(
                    pos,
                    format!("'!' needs a bool, got {}", type_name(&v)),
                )),
            },
            Kind::Neg(a) => {
                let v = self.eval(a)?;
                match Num::of(&v) {
                    Some(Num::I(i)) => i
                        .checked_neg()
                        .map(Value::from)
                        .ok_or_else(|| Fail::new(pos, "integer overflow")),
                    Some(Num::D(d)) => double(-d, pos),
                    None => Err(Fail::new(
                        pos,
                        format!("'-' needs a number, got {}", type_name(&v)),
                    )),
                }
            }
            Kind::And(a, b) => {
                if !self.bool(a)? {
                    return Ok(Value::Bool(false));
                }
                Ok(Value::Bool(self.bool(b)?))
            }
            Kind::Or(a, b) => {
                if self.bool(a)? {
                    return Ok(Value::Bool(true));
                }
                Ok(Value::Bool(self.bool(b)?))
            }
            Kind::Cond(c, t, e) => {
                if self.bool(c)? {
                    self.eval(t)
                } else {
                    self.eval(e)
                }
            }
            Kind::Bin(op, a, b) => {
                let l = self.eval(a)?;
                let r = self.eval(b)?;
                let v = binary(*op, &l, &r, pos)?;
                if *op == BinOp::Add {
                    self.charge(&v, pos)?;
                }
                Ok(v)
            }
            Kind::Call { func, args, re } => {
                if *func == Func::Matches && re.is_none() {
                    self.steps_n(REGEX_COMPILE_STEPS, pos)?;
                }
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a)?);
                }
                let v = funcs::call(*func, &vals, re.as_deref(), self.host, pos)?;
                self.charge(&v, pos)?;
                Ok(v)
            }
            Kind::Has(operand, field) => match self.eval(operand)? {
                Value::Object(m) => Ok(Value::Bool(m.contains_key(field))),
                _ => Ok(Value::Bool(false)),
            },
            Kind::Held(ty, id) => {
                let id = self.eval(id)?;
                if id.is_null() {
                    return Ok(Value::Null);
                }
                let v = self.host.held(&ty.name, &id).unwrap_or(Value::Null);
                self.charge(&v, pos)?;
                Ok(v)
            }
            Kind::Count(ty, cond) => {
                let mut count: i64 = 0;
                for item in self.all(&ty.name, pos)? {
                    if self.iterate("it", item, cond)? {
                        count += 1;
                    }
                }
                Ok(Value::from(count))
            }
            Kind::ExistsType(ty, cond) => {
                for item in self.all(&ty.name, pos)? {
                    if self.iterate("it", item, cond)? {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            Kind::Children(ty, r#where) => {
                let r#where = match r#where {
                    Some(w) => self.eval(w)?,
                    None => Value::Null,
                };
                let items = self
                    .host
                    .children(&ty.name, &r#where)
                    .map_err(|m| Fail::new(pos, m))?;
                self.steps_n(u64::try_from(items.len()).unwrap_or(u64::MAX), pos)?;
                for item in &items {
                    self.charge(item, pos)?;
                }
                Ok(Value::Array(items))
            }
            Kind::Sum(list, e) => {
                let items = self.items(list)?;
                let mut acc = Num::I(0);
                for item in items {
                    self.step(e.pos)?;
                    self.scope.push(("it", item));
                    let v = self.eval(e);
                    self.scope.pop();
                    let v = v?;
                    let Some(x) = Num::of(&v) else {
                        return Err(Fail::new(
                            e.pos,
                            format!("sum() needs numbers, got {}", type_name(&v)),
                        ));
                    };
                    acc = add(acc, x, pos)?;
                }
                Ok(num_value(acc, pos)?)
            }
            Kind::Comp {
                op,
                var,
                range,
                body,
            } => {
                let items = self.items(range)?;
                match op {
                    CompOp::All => {
                        for item in items {
                            if !self.iterate(var, item, body)? {
                                return Ok(Value::Bool(false));
                            }
                        }
                        Ok(Value::Bool(true))
                    }
                    CompOp::Exists => {
                        for item in items {
                            if self.iterate(var, item, body)? {
                                return Ok(Value::Bool(true));
                            }
                        }
                        Ok(Value::Bool(false))
                    }
                    CompOp::Filter => {
                        let mut out = Vec::new();
                        for item in items {
                            let bound = self.copy_ref(&item, body.pos)?;
                            if self.iterate(var, bound, body)? {
                                out.push(item);
                            }
                        }
                        Ok(Value::Array(out))
                    }
                    CompOp::Map => {
                        let mut out = Vec::with_capacity(items.len());
                        for item in items {
                            self.step(body.pos)?;
                            self.scope.push((var, item));
                            let v = self.eval(body);
                            self.scope.pop();
                            out.push(v?);
                        }
                        Ok(Value::Array(out))
                    }
                }
            }
        }
    }

    /// The held records of a type, charged one step each plus their size,
    /// whether or not the caller looks at them all.
    fn all(&mut self, ty: &str, pos: usize) -> Result<Vec<Value>, Fail> {
        let items = self.host.all(ty);
        self.steps_n(u64::try_from(items.len()).unwrap_or(u64::MAX), pos)?;
        for item in &items {
            self.charge(item, pos)?;
        }
        Ok(items)
    }

    /// Evaluates a condition with `var` bound to `item`; one step per iteration.
    fn iterate(&mut self, var: &'p str, item: Value, cond: &'p Node) -> Result<bool, Fail> {
        self.step(cond.pos)?;
        self.scope.push((var, item));
        let r = self.bool(cond);
        self.scope.pop();
        r
    }

    fn bool(&mut self, n: &'p Node) -> Result<bool, Fail> {
        match self.eval(n)? {
            Value::Bool(b) => Ok(b),
            v => Err(Fail::new(
                n.pos,
                format!("expected a bool, got {}", type_name(&v)),
            )),
        }
    }

    /// The items a macro ranges over: a list's items, or a map's keys.
    fn items(&mut self, n: &'p Node) -> Result<Vec<Value>, Fail> {
        match self.eval(n)? {
            Value::Array(a) => Ok(a),
            Value::Object(m) => Ok(m.into_iter().map(|(k, _)| Value::String(k)).collect()),
            v => Err(Fail::new(
                n.pos,
                format!("expected a list, got {}", type_name(&v)),
            )),
        }
    }

    /// A member/index chain. When it starts at a name, it is walked by reference
    /// and only the value reached is cloned.
    fn chain(&mut self, n: &'p Node) -> Result<Value, Fail> {
        let mut links: Vec<(&'p Node, usize)> = Vec::new();
        let mut cur = n;
        while let Kind::Member(base, _) | Kind::Index(base, _) = &cur.kind {
            links.push((cur, cur.pos));
            cur = base;
        }
        links.reverse();
        // The base, owned unless it is a name.
        let owned = match &cur.kind {
            Kind::Ident(_) => {
                self.step(cur.pos)?;
                None
            }
            _ => Some(self.eval(cur)?),
        };
        let mut keys = Vec::with_capacity(links.len());
        for (i, (link, _)) in links.iter().enumerate() {
            if i > 0 {
                self.step(link.pos)?;
            }
            keys.push(match &link.kind {
                Kind::Member(_, name) => Key::Name(name),
                Kind::Index(_, idx) => Key::Value(self.eval(idx)?),
                _ => Key::Value(Value::Null),
            });
        }
        let mut v: &Value = match (&owned, &cur.kind) {
            (Some(v), _) => v,
            (None, Kind::Ident(name)) => self.lookup(name, cur.pos)?,
            (None, _) => &NULL,
        };
        for (key, (_, pos)) in keys.iter().zip(&links) {
            v = descend(v, key, *pos)?;
        }
        self.copy_ref(v, n.pos)
    }
}

/// Roughly how many bytes a value takes: its strings and keys, plus 8 for each
/// value. Walked without recursion, as values can nest deeply.
fn weight(v: &Value) -> u64 {
    let mut total: u64 = 0;
    let mut stack = vec![v];
    while let Some(v) = stack.pop() {
        total = total.saturating_add(8);
        match v {
            Value::String(s) => total = total.saturating_add(s.len() as u64),
            Value::Array(a) => stack.extend(a),
            Value::Object(m) => {
                for (k, x) in m {
                    total = total.saturating_add(k.len() as u64);
                    stack.push(x);
                }
            }
            _ => {}
        }
    }
    total
}

fn descend<'v>(v: &'v Value, key: &Key<'_>, pos: usize) -> Result<&'v Value, Fail> {
    match (v, key) {
        (Value::Null, _) => Ok(&NULL),
        (Value::Object(m), Key::Name(k)) => Ok(m.get(*k).unwrap_or(&NULL)),
        (Value::Object(m), Key::Value(Value::String(k))) => Ok(m.get(k).unwrap_or(&NULL)),
        (Value::Object(_), Key::Value(k)) => Err(Fail::new(
            pos,
            format!("a map index must be a string, got {}", type_name(k)),
        )),
        (Value::Array(a), Key::Value(k)) => match Num::of(k) {
            Some(Num::I(i)) => Ok(usize::try_from(i)
                .ok()
                .and_then(|i| a.get(i))
                .unwrap_or(&NULL)),
            _ => Err(Fail::new(
                pos,
                format!("a list index must be an int, got {}", type_name(k)),
            )),
        },
        (_, Key::Name(k)) => Err(Fail::new(
            pos,
            format!("no member '{k}' on a {}", type_name(v)),
        )),
        (_, Key::Value(_)) => Err(Fail::new(pos, format!("cannot index a {}", type_name(v)))),
    }
}

fn num_value(n: Num, pos: usize) -> Result<Value, Fail> {
    match n {
        Num::I(i) => Ok(Value::from(i)),
        Num::D(d) => double(d, pos),
    }
}

fn add(a: Num, b: Num, pos: usize) -> Result<Num, Fail> {
    match (a, b) {
        (Num::I(x), Num::I(y)) => x
            .checked_add(y)
            .map(Num::I)
            .ok_or_else(|| Fail::new(pos, "integer overflow")),
        _ => Ok(Num::D(a.f() + b.f())),
    }
}

fn arith(op: BinOp, a: Num, b: Num, pos: usize) -> Result<Value, Fail> {
    let overflow = || Fail::new(pos, "integer overflow");
    if let (Num::I(x), Num::I(y)) = (a, b) {
        let r = match op {
            BinOp::Add => x.checked_add(y),
            BinOp::Sub => x.checked_sub(y),
            BinOp::Mul => x.checked_mul(y),
            BinOp::Div | BinOp::Rem if y == 0 => {
                return Err(Fail::new(pos, "division by zero"));
            }
            BinOp::Div => match x.checked_rem(y) {
                // An uneven division gives a double, unlike CEL.
                Some(r) if r != 0 => return double(x as f64 / y as f64, pos),
                _ => x.checked_div(y),
            },
            _ => x.checked_rem(y),
        };
        return r.map(Value::from).ok_or_else(overflow);
    }
    let (x, y) = (a.f(), b.f());
    match op {
        BinOp::Add => double(x + y, pos),
        BinOp::Sub => double(x - y, pos),
        BinOp::Mul => double(x * y, pos),
        BinOp::Div => double(x / y, pos),
        _ => Err(Fail::new(pos, "'%' needs two ints")),
    }
}

fn binary(op: BinOp, l: &Value, r: &Value, pos: usize) -> Result<Value, Fail> {
    match op {
        BinOp::Eq => Ok(Value::Bool(equal(l, r))),
        BinOp::Ne => Ok(Value::Bool(!equal(l, r))),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let ord = compare(l, r).ok_or_else(|| {
                Fail::new(
                    pos,
                    format!("cannot compare {} with {}", type_name(l), type_name(r)),
                )
            })?;
            Ok(Value::Bool(match op {
                BinOp::Lt => ord == Ordering::Less,
                BinOp::Le => ord != Ordering::Greater,
                BinOp::Gt => ord == Ordering::Greater,
                _ => ord != Ordering::Less,
            }))
        }
        BinOp::In => match r {
            Value::Array(a) => Ok(Value::Bool(a.iter().any(|x| equal(l, x)))),
            Value::Object(m) => match l {
                Value::String(k) => Ok(Value::Bool(m.contains_key(k))),
                _ => Ok(Value::Bool(false)),
            },
            Value::Null => Ok(Value::Bool(false)),
            _ => Err(Fail::new(
                pos,
                format!("'in' needs a list or a map, got {}", type_name(r)),
            )),
        },
        BinOp::Add => match (l, r) {
            (Value::String(a), Value::String(b)) => Ok(Value::String(format!("{a}{b}"))),
            (Value::Array(a), Value::Array(b)) => {
                Ok(Value::Array(a.iter().chain(b).cloned().collect()))
            }
            _ => numeric(op, l, r, pos),
        },
        _ => numeric(op, l, r, pos),
    }
}

fn numeric(op: BinOp, l: &Value, r: &Value, pos: usize) -> Result<Value, Fail> {
    match (Num::of(l), Num::of(r)) {
        (Some(a), Some(b)) => arith(op, a, b, pos),
        _ => {
            let sym = match op {
                BinOp::Add => "+",
                BinOp::Sub => "-",
                BinOp::Mul => "*",
                BinOp::Div => "/",
                _ => "%",
            };
            Err(Fail::new(
                pos,
                format!(
                    "'{sym}' cannot combine {} and {}",
                    type_name(l),
                    type_name(r)
                ),
            ))
        }
    }
}

/// Equality: numbers compare by value across int and double; lists and maps
/// compare deeply; values of different types are unequal.
pub(super) fn equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => compare(a, b).is_some_and(|o| o == Ordering::Equal),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| equal(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| equal(v, w)))
        }
        _ => a == b,
    }
}

fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => match (Num::of(a)?, Num::of(b)?) {
            (Num::I(x), Num::I(y)) => Some(x.cmp(&y)),
            (x, y) => x.f().partial_cmp(&y.f()),
        },
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}
