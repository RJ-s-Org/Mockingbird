//! Filter expressions: a described filter's `grammar` (`scim`, `odata`, `field_value`,
//! `deep_object`) parses its parameter's value into one small [`Expr`], comparisons of
//! dotted member paths joined by `and`, `or` and `not`, which [`eval`] tests against an
//! item with the comparisons [`super`] filters use.
//!
//! - `scim` (RFC 7644 section 3.4.2.2): `userName eq "x" and not (active eq false)`,
//!   `emails.value co "@x"`, `title pr`; operators and attribute names in any case, a
//!   schema URN before an attribute dropped. A bracketed value filter
//!   (`emails[type eq "work"]`) does not parse.
//! - `odata` (`$filter`): `eq`, `ne`, `gt`, `ge`, `lt`, `le`, `in (...)`, `and`, `or`,
//!   `not`, parentheses, `'quoted'` text (`''` a quote), `contains(m,'x')`,
//!   `startswith(...)`, `endswith(...)`; `a/b` is the dotted `a.b`.
//! - `field_value`: `status:open,priority:high`, each pair an equality, all holding.
//! - `deep_object`: the parameter's bracketed keys, `filter[a][b][_gte]=v`; a last key
//!   `_eq`, `_ne` (`_neq`), `_gt`, `_lt`, `_gte`, `_lte`, `_in` or `_contains` (or the
//!   same after `$`) is the operator, none is `eq`, and every key holds.
//!
//! Anything else (another function, a property compared with a property, `_or`
//! brackets) does not parse: the caller falls back rather than guess.
//!
//! [`eval`] answers `Some(true)`, `Some(false)` or `None` (says nothing), with `and`,
//! `or` and `not` over the three (Kleene's logic). A comparison says nothing when its
//! value is not of the member's declared type ([`Expr::Unknown`], see [`retype`]), when
//! its sides do not compare (`gt` on text that is no number or date-time), or, for an
//! item the model answered, when the item does not hold the member; a stored record
//! that does not hold it does not match. An item is kept unless the whole expression
//! is `Some(false)`.

use std::cmp::Ordering;

use mb_mcm::world::{FilterGrammar, FilterKind};
use serde_json::Value;

use super::{compare, element_is, same, typed};

/// How deep parentheses, `not`s and member arrays may nest.
const MAX_DEPTH: usize = 32;

/// The most comparisons one expression may hold.
const MAX_TERMS: usize = 256;

/// A parsed filter expression.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::call) enum Expr {
    /// One member compared with one value.
    Cmp(Cmp),
    /// Every one holds.
    And(Vec<Expr>),
    /// Some one holds.
    Or(Vec<Expr>),
    /// It does not hold.
    Not(Box<Expr>),
    /// A comparison whose value is not of its member's declared type: it says nothing.
    Unknown,
}

/// A comparison of the member at a (dotted) path with a value.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::call) struct Cmp {
    pub(in crate::call) path: String,
    pub(in crate::call) op: Op,
    /// `null` for [`Op::Present`]; an array for [`Op::In`].
    pub(in crate::call) value: Value,
}

/// A comparison's operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::call) enum Op {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    Contains,
    StartsWith,
    EndsWith,
    Present,
    In,
}

impl Expr {
    fn cmp(path: impl Into<String>, op: Op, value: Value) -> Self {
        Self::Cmp(Cmp {
            path: path.into(),
            op,
            value,
        })
    }

    /// The expressions of `all` joined by `and`, one alone as itself.
    fn all(mut all: Vec<Expr>) -> Option<Self> {
        match all.len() {
            0 => None,
            1 => all.pop(),
            _ => Some(Self::And(all)),
        }
    }

    /// The expression with every comparison's path below `prefix`.
    pub(in crate::call) fn below(self, prefix: &str) -> Self {
        match self {
            Self::Cmp(c) => Self::Cmp(Cmp {
                path: format!("{prefix}.{}", c.path),
                ..c
            }),
            Self::And(all) => Self::And(all.into_iter().map(|e| e.below(prefix)).collect()),
            Self::Or(any) => Self::Or(any.into_iter().map(|e| e.below(prefix)).collect()),
            Self::Not(e) => Self::Not(Box::new(e.below(prefix))),
            Self::Unknown => Self::Unknown,
        }
    }
}

/// `text` in `grammar` (not `deep_object`, whose value is its keys: [`deep`]); `None`
/// when it does not parse.
pub(in crate::call) fn parse(grammar: FilterGrammar, text: &str) -> Option<Expr> {
    match grammar {
        FilterGrammar::Scim => Parser::new(text, Dialect::Scim)?.whole(),
        FilterGrammar::Odata => Parser::new(text, Dialect::Odata)?.whole(),
        FilterGrammar::FieldValue => field_value(text),
        FilterGrammar::DeepObject => None,
    }
}

/// `status:open,priority:high`: each pair an equality, all holding.
fn field_value(text: &str) -> Option<Expr> {
    let mut all = Vec::new();
    for pair in text.split(',') {
        let (field, value) = pair.split_once(':')?;
        let (field, value) = (field.trim(), value.trim());
        if field.is_empty() || value.is_empty() || all.len() >= MAX_TERMS {
            return None;
        }
        all.push(Expr::cmp(field, Op::Eq, Value::String(value.to_string())));
    }
    Expr::all(all)
}

/// The keys below a deepObject parameter as one expression: each `(keys, value)` of
/// `leaves` a comparison, all holding. `None` when one does not parse; also `None`
/// with no leaves, which asks nothing.
pub(in crate::call) fn deep(leaves: &[(Vec<String>, Value)]) -> Option<Expr> {
    let mut all = Vec::new();
    for (keys, value) in leaves {
        let (last, before) = keys.split_last()?;
        let (op, path) = match operator(last) {
            Some(op) => (op?, before),
            None => (Op::Eq, keys.as_slice()),
        };
        if path.is_empty()
            || path.iter().any(|k| k.is_empty() || operator(k).is_some())
            || all.len() >= MAX_TERMS
        {
            return None;
        }
        let path = path.join(".");
        let cmp = match (op, value) {
            (Op::Eq | Op::In, Value::Array(values)) => {
                Expr::cmp(path, Op::In, Value::Array(values.clone()))
            }
            (Op::In, Value::String(text)) => Expr::cmp(
                path,
                Op::In,
                Value::Array(
                    text.split(',')
                        .map(|t| Value::String(t.trim().to_string()))
                        .collect(),
                ),
            ),
            (Op::In, other) => Expr::cmp(path, Op::In, Value::Array(vec![other.clone()])),
            (_, Value::Array(_) | Value::Object(_) | Value::Null) => return None,
            (op, value) => Expr::cmp(path, op, value.clone()),
        };
        all.push(cmp);
    }
    Expr::all(all)
}

/// A deepObject key naming an operator (`_gte`, `$eq`): `Some(Some(op))`;
/// `Some(None)` for one it does not know (`_or`), `None` for a member's key.
fn operator(key: &str) -> Option<Option<Op>> {
    let name = key.strip_prefix('_').or_else(|| key.strip_prefix('$'))?;
    Some(match name.to_ascii_lowercase().as_str() {
        "eq" => Some(Op::Eq),
        "ne" | "neq" => Some(Op::Ne),
        "gt" => Some(Op::Gt),
        "lt" => Some(Op::Lt),
        "gte" => Some(Op::Ge),
        "lte" => Some(Op::Le),
        "in" => Some(Op::In),
        "contains" => Some(Op::Contains),
        _ => None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Scim,
    Odata,
}

impl Dialect {
    fn quote(self) -> char {
        match self {
            Self::Scim => '"',
            Self::Odata => '\'',
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Open,
    Close,
    Bracket,
    Comma,
    Quoted(String),
    Word(String),
}

/// `text` as tokens; `None` for an unterminated quote.
fn lex(text: &str, dialect: Dialect) -> Option<Vec<Tok>> {
    let quote = dialect.quote();
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '(' => out.push(Tok::Open),
            ')' => out.push(Tok::Close),
            '[' | ']' => out.push(Tok::Bracket),
            ',' => out.push(Tok::Comma),
            c if c == quote => {
                let mut s = String::new();
                loop {
                    match chars.next()? {
                        // OData doubles a quote inside text.
                        q if q == quote && dialect == Dialect::Odata => {
                            if chars.peek() == Some(&quote) {
                                chars.next();
                                s.push(quote);
                            } else {
                                break;
                            }
                        }
                        q if q == quote => break,
                        // SCIM text is a JSON string.
                        '\\' if dialect == Dialect::Scim => match chars.next()? {
                            'n' => s.push('\n'),
                            't' => s.push('\t'),
                            'r' => s.push('\r'),
                            'u' => {
                                let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                                s.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                            }
                            other => s.push(other),
                        },
                        other => s.push(other),
                    }
                }
                out.push(Tok::Quoted(s));
            }
            c => {
                let mut word = String::from(c);
                while let Some(&n) = chars.peek() {
                    if n.is_whitespace() || matches!(n, '(' | ')' | '[' | ']' | ',') || n == quote {
                        break;
                    }
                    word.push(n);
                    chars.next();
                }
                out.push(Tok::Word(word));
            }
        }
    }
    Some(out)
}

/// A recursive-descent parser of SCIM and OData filters: `or` below `and` below `not`.
struct Parser {
    toks: Vec<Tok>,
    at: usize,
    dialect: Dialect,
    depth: usize,
    terms: usize,
}

impl Parser {
    fn new(text: &str, dialect: Dialect) -> Option<Self> {
        Some(Self {
            toks: lex(text, dialect)?,
            at: 0,
            dialect,
            depth: 0,
            terms: 0,
        })
    }

    /// The whole text as one expression.
    fn whole(mut self) -> Option<Expr> {
        let e = self.or()?;
        (self.at == self.toks.len()).then_some(e)
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.at).cloned();
        self.at += 1;
        t
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        let here = self.peek() == Some(tok);
        if here {
            self.at += 1;
        }
        here
    }

    /// Whether the next token is word `kw` (in any case), taking it if so.
    fn keyword(&mut self, kw: &str) -> bool {
        let here = matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw));
        if here {
            self.at += 1;
        }
        here
    }

    fn or(&mut self) -> Option<Expr> {
        let mut any = vec![self.and()?];
        while self.keyword("or") {
            any.push(self.and()?);
        }
        Some(if any.len() == 1 {
            any.pop()?
        } else {
            Expr::Or(any)
        })
    }

    fn and(&mut self) -> Option<Expr> {
        let mut all = vec![self.unary()?];
        while self.keyword("and") {
            all.push(self.unary()?);
        }
        Expr::all(all)
    }

    fn unary(&mut self) -> Option<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return None;
        }
        let e = if self.keyword("not") {
            Expr::Not(Box::new(self.unary()?))
        } else if self.eat(&Tok::Open) {
            let e = self.or()?;
            if !self.eat(&Tok::Close) {
                return None;
            }
            e
        } else {
            self.terms += 1;
            if self.terms > MAX_TERMS {
                return None;
            }
            self.comparison()?
        };
        self.depth -= 1;
        Some(e)
    }

    fn comparison(&mut self) -> Option<Expr> {
        let Some(Tok::Word(word)) = self.next() else {
            return None;
        };
        if self.dialect == Dialect::Odata && self.peek() == Some(&Tok::Open) {
            return self.function(&word);
        }
        let path = self.path(&word)?;
        let Some(Tok::Word(op)) = self.next() else {
            // A bracketed value filter, or nothing after the attribute.
            return None;
        };
        let op = op.to_ascii_lowercase();
        let op = match (self.dialect, op.as_str()) {
            (_, "eq") => Op::Eq,
            (_, "ne") => Op::Ne,
            (_, "gt") => Op::Gt,
            (_, "ge") => Op::Ge,
            (_, "lt") => Op::Lt,
            (_, "le") => Op::Le,
            (Dialect::Scim, "co") => Op::Contains,
            (Dialect::Scim, "sw") => Op::StartsWith,
            (Dialect::Scim, "ew") => Op::EndsWith,
            (Dialect::Scim, "pr") => return Some(Expr::cmp(path, Op::Present, Value::Null)),
            (Dialect::Odata, "in") => return self.list(path),
            _ => return None,
        };
        let value = self.value()?;
        Some(Expr::cmp(path, op, value))
    }

    /// OData's `contains(m,'x')`, `startswith(...)` and `endswith(...)`, the `(` next.
    fn function(&mut self, name: &str) -> Option<Expr> {
        let op = match name.to_ascii_lowercase().as_str() {
            "contains" => Op::Contains,
            "startswith" => Op::StartsWith,
            "endswith" => Op::EndsWith,
            _ => return None,
        };
        self.eat(&Tok::Open);
        let Some(Tok::Word(member)) = self.next() else {
            return None;
        };
        let path = self.path(&member)?;
        if !self.eat(&Tok::Comma) {
            return None;
        }
        let value = self.value()?;
        if !self.eat(&Tok::Close) {
            return None;
        }
        Some(Expr::cmp(path, op, value))
    }

    /// OData's `m in ('a','b')`, after the `in`.
    fn list(&mut self, path: String) -> Option<Expr> {
        if !self.eat(&Tok::Open) {
            return None;
        }
        let mut values = vec![self.value()?];
        while self.eat(&Tok::Comma) {
            values.push(self.value()?);
        }
        if !self.eat(&Tok::Close) {
            return None;
        }
        Some(Expr::cmp(path, Op::In, Value::Array(values)))
    }

    /// Attribute `word` as a dotted member path.
    fn path(&self, word: &str) -> Option<String> {
        let reserved = ["and", "or", "not", "true", "false", "null"];
        if reserved.iter().any(|r| word.eq_ignore_ascii_case(r))
            || word.starts_with(|c: char| c.is_ascii_digit())
        {
            return None;
        }
        let path = match self.dialect {
            // `urn:...:User:name.givenName` is `name.givenName`.
            Dialect::Scim if word.to_ascii_lowercase().starts_with("urn:") => {
                word.rsplit(':').next()?.to_string()
            }
            Dialect::Scim => word.to_string(),
            Dialect::Odata => word.replace('/', "."),
        };
        let valid = path.split('.').all(|step| {
            !step.is_empty()
                && step
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '$' | '@'))
        });
        valid.then_some(path)
    }

    fn value(&mut self) -> Option<Value> {
        match self.next()? {
            Tok::Quoted(s) => Some(Value::String(s)),
            Tok::Word(w) => {
                let lower = w.to_ascii_lowercase();
                match lower.as_str() {
                    "true" => Some(Value::Bool(true)),
                    "false" => Some(Value::Bool(false)),
                    "null" => Some(Value::Null),
                    _ => {
                        if let Ok(n) = w.parse::<i64>() {
                            return Some(Value::from(n));
                        }
                        if let Some(n) =
                            w.parse::<f64>().ok().and_then(serde_json::Number::from_f64)
                        {
                            return Some(Value::Number(n));
                        }
                        // OData writes a date, a date-time or a GUID unquoted.
                        (self.dialect == Dialect::Odata
                            && w.starts_with(|c: char| c.is_ascii_digit()))
                        .then_some(Value::String(w))
                    }
                }
            }
            _ => None,
        }
    }
}

/// `e` with each text value typed as its member declares (`ty` of a path): a text
/// that is not one of the declared type makes its comparison [`Expr::Unknown`].
pub(in crate::call) fn retype(e: Expr, ty: &dyn Fn(&str) -> Option<String>) -> Expr {
    match e {
        Expr::Cmp(c) => {
            if matches!(
                c.op,
                Op::Present | Op::Contains | Op::StartsWith | Op::EndsWith
            ) {
                return Expr::Cmp(c);
            }
            let declared = ty(&c.path);
            let one = |v: &Value| match (declared.as_deref(), v) {
                (_, Value::String(text)) => typed(declared.as_deref(), FilterKind::Eq, text),
                (Some("integer" | "number"), Value::Bool(_))
                | (Some("boolean"), Value::Number(_)) => None,
                _ => Some(v.clone()),
            };
            let value = match &c.value {
                Value::Array(values) if c.op == Op::In => {
                    let kept: Vec<Value> = values.iter().filter_map(one).collect();
                    (!kept.is_empty()).then_some(Value::Array(kept))
                }
                Value::Null => Some(Value::Null),
                v => one(v),
            };
            match value {
                Some(value) => Expr::Cmp(Cmp { value, ..c }),
                None => Expr::Unknown,
            }
        }
        Expr::And(all) => Expr::And(all.into_iter().map(|e| retype(e, ty)).collect()),
        Expr::Or(any) => Expr::Or(any.into_iter().map(|e| retype(e, ty)).collect()),
        Expr::Not(e) => Expr::Not(Box::new(retype(*e, ty))),
        Expr::Unknown => Expr::Unknown,
    }
}

/// What `e` says of `item`: `None` when it says nothing. `answered`: the item is the
/// model's (it says nothing of a member it does not hold), else a stored record (one
/// not holding the member does not match). `fold`: member names and text compare in
/// any case.
pub(in crate::call) fn eval(e: &Expr, item: &Value, answered: bool, fold: bool) -> Option<bool> {
    match e {
        Expr::Cmp(c) => eval_cmp(c, item, answered, fold),
        Expr::And(all) => {
            let mut unknown = false;
            for e in all {
                match eval(e, item, answered, fold) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            (!unknown).then_some(true)
        }
        Expr::Or(any) => {
            let mut unknown = false;
            for e in any {
                match eval(e, item, answered, fold) {
                    Some(true) => return Some(true),
                    None => unknown = true,
                    Some(false) => {}
                }
            }
            (!unknown).then_some(false)
        }
        Expr::Not(e) => eval(e, item, answered, fold).map(|b| !b),
        Expr::Unknown => None,
    }
}

fn eval_cmp(c: &Cmp, item: &Value, answered: bool, fold: bool) -> Option<bool> {
    let mut held = Vec::new();
    resolve(item, &c.path, fold, &mut held);
    if held.is_empty() {
        return if answered { None } else { Some(false) };
    }
    // A member's array is any of its items, but for `contains`, which reads arrays.
    let mut each: Vec<&Value> = Vec::new();
    for h in &held {
        match h {
            Value::Array(items) if c.op != Op::Contains => each.extend(items.iter()),
            other => each.push(other),
        }
    }
    let eq = |h: &Value, w: &Value| match (fold, h, w) {
        (true, Value::String(a), Value::String(b)) => a.to_lowercase() == b.to_lowercase(),
        _ => same(h, w),
    };
    let text = |h: &Value, w: &Value, test: fn(&str, &str) -> bool| match (h, w) {
        (Value::String(a), Value::String(b)) if fold => test(&a.to_lowercase(), &b.to_lowercase()),
        (Value::String(a), Value::String(b)) => test(a, b),
        _ => false,
    };
    let w = &c.value;
    match c.op {
        Op::Present => Some(each.iter().any(|h| match h {
            Value::String(s) => !s.is_empty(),
            Value::Object(m) => !m.is_empty(),
            _ => true,
        })),
        Op::Eq => Some(each.iter().any(|h| eq(h, w))),
        Op::Ne => Some(!each.iter().any(|h| eq(h, w))),
        Op::In => {
            let values = w.as_array()?;
            Some(each.iter().any(|h| values.iter().any(|v| eq(h, v))))
        }
        Op::Contains => Some(each.iter().any(|h| contains(h, w, fold))),
        Op::StartsWith => Some(each.iter().any(|h| text(h, w, |a, b| a.starts_with(b)))),
        Op::EndsWith => Some(each.iter().any(|h| text(h, w, |a, b| a.ends_with(b)))),
        Op::Gt | Op::Lt | Op::Ge | Op::Le => {
            let wanted = |o: Ordering| match c.op {
                Op::Gt => o == Ordering::Greater,
                Op::Lt => o == Ordering::Less,
                Op::Ge => o != Ordering::Less,
                _ => o != Ordering::Greater,
            };
            let orders: Vec<Ordering> = each.iter().filter_map(|h| compare(h, w)).collect();
            // Sides that do not compare say nothing, as a described bound's do.
            (!orders.is_empty()).then(|| orders.into_iter().any(wanted))
        }
    }
}

/// Whether `held` contains `wanted`: an array holding an item equal to it (or an
/// object item whose one string member is), or text holding it, in any case when
/// `fold`.
pub(super) fn contains(held: &Value, wanted: &Value, fold: bool) -> bool {
    match held {
        Value::Array(elements) => elements.iter().any(|e| {
            element_is(e, wanted)
                || (fold
                    && matches!((e, wanted), (Value::String(a), Value::String(b))
                        if a.to_lowercase() == b.to_lowercase()))
        }),
        Value::String(s) => wanted.as_str().is_some_and(|w| {
            if fold {
                s.to_lowercase().contains(&w.to_lowercase())
            } else {
                s.contains(w)
            }
        }),
        _ => false,
    }
}

/// The non-`null` values at dotted `path` of `item`, each array met on the way any of
/// its items; a member name in any case when `fold`, its exact name first.
fn resolve<'a>(item: &'a Value, path: &str, fold: bool, out: &mut Vec<&'a Value>) {
    if let Some(whole) = item.get(path).filter(|v| !v.is_null()) {
        out.push(whole);
        return;
    }
    let steps: Vec<&str> = path.split('.').collect();
    walk(item, &steps, fold, out, 0);
}

fn walk<'a>(at: &'a Value, steps: &[&str], fold: bool, out: &mut Vec<&'a Value>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let Some((step, rest)) = steps.split_first() else {
        if !at.is_null() {
            out.push(at);
        }
        return;
    };
    match at {
        Value::Array(items) => {
            for i in items {
                walk(i, steps, fold, out, depth + 1);
            }
        }
        Value::Object(members) => {
            let found = members.get(*step).or_else(|| {
                fold.then(|| {
                    members
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(step))
                        .map(|(_, v)| v)
                })
                .flatten()
            });
            if let Some(next) = found {
                walk(next, rest, fold, out, depth + 1);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn scim(text: &str) -> Option<Expr> {
        parse(FilterGrammar::Scim, text)
    }

    fn odata(text: &str) -> Option<Expr> {
        parse(FilterGrammar::Odata, text)
    }

    fn cmp(path: &str, op: Op, value: Value) -> Expr {
        Expr::cmp(path, op, value)
    }

    #[test]
    fn scim_parses_comparisons_joined_by_and_or_not() {
        assert_eq!(
            scim(r#"userName Eq "x" AND active eq true"#),
            Some(Expr::And(vec![
                cmp("userName", Op::Eq, json!("x")),
                cmp("active", Op::Eq, json!(true)),
            ]))
        );
        assert_eq!(
            scim(r#"emails.value co "@x" or not (title pr)"#),
            Some(Expr::Or(vec![
                cmp("emails.value", Op::Contains, json!("@x")),
                Expr::Not(Box::new(cmp("title", Op::Present, Value::Null))),
            ]))
        );
        assert_eq!(
            scim(r#"a eq 1 or b eq 2 and c eq 3"#),
            Some(Expr::Or(vec![
                cmp("a", Op::Eq, json!(1)),
                Expr::And(vec![cmp("b", Op::Eq, json!(2)), cmp("c", Op::Eq, json!(3))]),
            ]))
        );
        assert_eq!(
            scim(r#"urn:ietf:params:scim:schemas:core:2.0:User:name.givenName sw "A\"b""#),
            Some(cmp("name.givenName", Op::StartsWith, json!("A\"b")))
        );
        assert_eq!(
            scim("meta.lastModified gt \"2024-01-01T00:00:00Z\""),
            Some(cmp(
                "meta.lastModified",
                Op::Gt,
                json!("2024-01-01T00:00:00Z")
            ))
        );
    }

    #[test]
    fn scim_refuses_what_it_does_not_read() {
        for bad in [
            r#"emails[type eq "work"]"#,
            r#"userName eq"#,
            r#"userName eq "x"#,
            r#"userName like "x""#,
            r#"userName eq x"#,
            r#"(userName eq "x""#,
            r#"userName eq "x" extra"#,
            "",
        ] {
            assert_eq!(scim(bad), None, "{bad}");
        }
    }

    #[test]
    fn odata_parses_comparisons_functions_and_lists() {
        assert_eq!(
            odata("postalCode eq '98052' and (amount ge 5 or not startswith(name,'O''B'))"),
            Some(Expr::And(vec![
                cmp("postalCode", Op::Eq, json!("98052")),
                Expr::Or(vec![
                    cmp("amount", Op::Ge, json!(5)),
                    Expr::Not(Box::new(cmp("name", Op::StartsWith, json!("O'B")))),
                ]),
            ]))
        );
        assert_eq!(
            odata("contains(address/city, 'Sea')"),
            Some(cmp("address.city", Op::Contains, json!("Sea")))
        );
        assert_eq!(
            odata("status in ('open', 'held') and created lt 2024-01-01T00:00:00Z"),
            Some(Expr::And(vec![
                cmp("status", Op::In, json!(["open", "held"])),
                cmp("created", Op::Lt, json!("2024-01-01T00:00:00Z")),
            ]))
        );
        assert_eq!(
            odata("deleted ne null"),
            Some(cmp("deleted", Op::Ne, Value::Null))
        );
        for bad in [
            "tolower(name) eq 'x'",
            "a eq b",
            "name eq 'x",
            "contains(name 'x')",
            "a co 'x'",
            "a eq 'x' or",
        ] {
            assert_eq!(odata(bad), None, "{bad}");
        }
    }

    #[test]
    fn field_value_is_equalities_that_all_hold() {
        assert_eq!(
            parse(FilterGrammar::FieldValue, "status:open, priority:high"),
            Some(Expr::And(vec![
                cmp("status", Op::Eq, json!("open")),
                cmp("priority", Op::Eq, json!("high")),
            ]))
        );
        assert_eq!(
            parse(FilterGrammar::FieldValue, "at:10:30"),
            Some(cmp("at", Op::Eq, json!("10:30")))
        );
        for bad in ["status", "status:", ":open", "a:1,,b:2"] {
            assert_eq!(parse(FilterGrammar::FieldValue, bad), None, "{bad}");
        }
    }

    #[test]
    fn deep_object_keys_name_members_and_operators() {
        let leaf = |keys: &[&str], value: Value| {
            (
                keys.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
                value,
            )
        };
        assert_eq!(
            deep(&[
                leaf(&["status"], json!("open")),
                leaf(&["category", "name", "_eq"], json!("Tools")),
                leaf(&["price", "$gte"], json!("5")),
                leaf(&["id", "_in"], json!("a,b")),
                leaf(&["tag"], json!(["x", "y"])),
            ]),
            Some(Expr::And(vec![
                cmp("status", Op::Eq, json!("open")),
                cmp("category.name", Op::Eq, json!("Tools")),
                cmp("price", Op::Ge, json!("5")),
                cmp("id", Op::In, json!(["a", "b"])),
                cmp("tag", Op::In, json!(["x", "y"])),
            ]))
        );
        assert_eq!(deep(&[]), None);
        for bad in [
            leaf(&["_or", "0", "status", "_eq"], json!("x")),
            leaf(&["status", "_between"], json!("x")),
            leaf(&["_eq"], json!("x")),
        ] {
            assert_eq!(deep(std::slice::from_ref(&bad)), None, "{bad:?}");
        }
    }

    #[test]
    fn eval_is_three_valued_over_stored_and_answered_items() {
        let e = scim(r#"userName eq "ANN" and not (title pr)"#).unwrap();
        let ann = json!({"UserName": "ann"});
        assert_eq!(eval(&e, &ann, false, true), Some(true));
        // Case-sensitive, the member and the text differ.
        assert_eq!(eval(&e, &ann, false, false), Some(false));
        // A stored record lacking the member does not match; an answered item says
        // nothing.
        let e = odata("status eq 'open'").unwrap();
        assert_eq!(eval(&e, &json!({}), false, false), Some(false));
        assert_eq!(eval(&e, &json!({"status": null}), true, false), None);
        assert_eq!(
            eval(&e, &json!({"status": "done"}), true, false),
            Some(false)
        );
        // An unknown side of `or` keeps; of `and` with a false side does not.
        let e = Expr::Or(vec![Expr::Unknown, cmp("a", Op::Eq, json!(1))]);
        assert_eq!(eval(&e, &json!({"a": 2}), false, false), None);
        let e = Expr::And(vec![Expr::Unknown, cmp("a", Op::Eq, json!(1))]);
        assert_eq!(eval(&e, &json!({"a": 2}), false, false), Some(false));
        assert_eq!(
            eval(
                &Expr::Not(Box::new(Expr::Unknown)),
                &json!({}),
                false,
                false
            ),
            None
        );
    }

    #[test]
    fn eval_reads_arrays_dates_and_numbers() {
        let user = json!({"emails": [{"value": "a@x.io"}, {"value": "b@y.io"}], "n": 7,
            "at": "2024-05-01T00:00:00Z", "tags": ["red", "blue"]});
        let yes = |text: &str| eval(&scim(text).unwrap(), &user, false, true);
        assert_eq!(yes(r#"emails.value co "@Y""#), Some(true));
        assert_eq!(yes(r#"emails.value ew ".io""#), Some(true));
        assert_eq!(yes(r#"emails.value eq "c@z.io""#), Some(false));
        assert_eq!(yes(r#"tags eq "blue""#), Some(true));
        assert_eq!(yes(r#"tags co "red""#), Some(true));
        assert_eq!(yes("n gt 6 and n le 7"), Some(true));
        assert_eq!(yes("n lt 7"), Some(false));
        assert_eq!(yes(r#"at ge "2024-04-30T00:00:00Z""#), Some(true));
        // Text that is no number or date-time does not compare: it says nothing.
        assert_eq!(yes(r#"at gt "soon""#), None);
    }

    #[test]
    fn retype_makes_a_mistyped_comparison_unknown() {
        let ty = |path: &str| (path == "n").then(|| "integer".to_string());
        let e = retype(parse(FilterGrammar::FieldValue, "n:abc,m:3").unwrap(), &ty);
        assert_eq!(
            e,
            Expr::And(vec![Expr::Unknown, cmp("m", Op::Eq, json!("3"))])
        );
        let e = retype(parse(FilterGrammar::FieldValue, "n:3").unwrap(), &ty);
        assert_eq!(e, cmp("n", Op::Eq, json!(3)));
        assert_eq!(
            cmp("run", Op::Eq, json!("x")).below("meta_data"),
            cmp("meta_data.run", Op::Eq, json!("x"))
        );
    }
}
