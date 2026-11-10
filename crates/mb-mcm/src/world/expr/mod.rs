//! World expressions: a small interpreter for a subset of CEL (the Common
//! Expression Language) over `serde_json::Value`.
//!
//! An expression is compiled once into a [`Program`] (`Send + Sync`) and
//! evaluated per request against [`Bindings`] (`request`, `record`, `now` and,
//! where a key binds one, `it`) and a [`Host`] that answers the session
//! functions (`held`, `count`, `exists`, `seq`, `mint`).
//!
//! The syntax is CEL's: literals (int, double, string, bool, null, lists,
//! maps), `.` members, `[...]` indexes, `?:`, `&&`, `||`, `!`, comparisons,
//! arithmetic, `in`, function calls `f(a)` and method calls `a.f(b)`, and the
//! macros `has(a.b)`, `list.all(x, p)`, `list.exists(x, p)`, `list.map(x, e)`,
//! `list.filter(x, p)`. Global macros bind `it` to each item:
//! `count('Type', cond)`, `exists('Type', cond)` and `sum(list, expr)`.
//!
//! Deliberate differences from CEL:
//! - a member or index of `null`, a missing key or an index past the end of a
//!   list is `null`, not an error; `has(a.b)` tests presence;
//! - a number whose value is integral and fits `i64` is an int, however it is
//!   spelled (`2` and `2.0` alike), and any other a double; int with int stays
//!   an int (overflow is an error), any double makes a double, and an int `/`
//!   that does not divide evenly gives a double; a computed result that is
//!   integral is written as an int (`1.5 + 1.5` is `3`); `%` takes ints only;
//! - ints and doubles compare with each other, and `null == null`;
//! - the type arguments of `held`, `count` and `exists` are string literals.
//!
//! Parsing caps the tree at [`MAX_DEPTH`] levels and [`MAX_NODES`] nodes, so
//! neither parsing nor evaluation can overflow the stack, and evaluation is
//! capped at a number of steps ([`MAX_STEPS`] by default).
//!
//! ```
//! use mb_mcm::world::expr::{Bindings, Host, Program};
//! use serde_json::{Value, json};
//!
//! struct NoSession;
//! impl Host for NoSession {
//!     fn held(&mut self, _: &str, _: &Value) -> Option<Value> { None }
//!     fn all(&mut self, _: &str) -> Vec<Value> { Vec::new() }
//!     fn seq(&mut self, _: &str) -> i64 { 1 }
//!     fn mint(&mut self, _: &str) -> Option<String> { None }
//! }
//!
//! let p = Program::compile("'k_' + lower(request.body.name)")?;
//! let request = json!({"body": {"name": "Ada"}});
//! let b = Bindings { request: &request, record: &Value::Null, now: &Value::Null, it: None };
//! assert_eq!(p.eval(&b, &mut NoSession)?, json!("k_ada"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt;

use serde_json::Value;

mod check;
mod eval;
mod funcs;
mod lex;
mod md5;
mod parse;
#[cfg(test)]
mod tests;

/// The deepest an expression's tree may nest.
pub const MAX_DEPTH: usize = 32;

/// The most nodes an expression's tree may have.
pub const MAX_NODES: usize = 512;

/// The default evaluation budget: every node evaluated, every macro iteration
/// and every record `count` or `exists` reads costs one step, every KiB of data
/// copied or built one more, and compiling a computed `matches` pattern 50.
pub const MAX_STEPS: u64 = 10_000;

/// The largest compiled size of a `matches` pattern, in bytes.
pub const MAX_REGEX_BYTES: usize = 1 << 20;

/// The members of `request`.
const REQUEST_PARTS: [&str; 4] = ["body", "query", "path", "headers"];

/// A place in an expression's source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Byte offset from the start of the source.
    pub offset: usize,
    /// Line, from 1.
    pub line: usize,
    /// Column in characters, from 1.
    pub column: usize,
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)
    }
}

/// An expression that does not compile: a syntax error, an unknown name or
/// function, a wrong number of arguments, or a tree over the caps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    /// Where the error is.
    pub pos: Position,
    /// What is wrong.
    pub message: String,
}

/// An evaluation that failed: a type mismatch, an overflow, the step budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalError {
    /// The node that failed.
    pub pos: Position,
    /// What went wrong.
    pub message: String,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.pos, self.message)
    }
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.pos, self.message)
    }
}

impl std::error::Error for CompileError {}
impl std::error::Error for EvalError {}

/// An internal failure: a byte offset and a message.
#[derive(Debug)]
pub(crate) struct Fail {
    pos: usize,
    msg: String,
}

impl Fail {
    fn new(pos: usize, msg: impl Into<String>) -> Self {
        Fail {
            pos,
            msg: msg.into(),
        }
    }
}

/// The line and column of a byte offset in `src`.
fn position(src: &str, offset: usize) -> Position {
    let offset = offset.min(src.len());
    let before = src.get(..offset).unwrap_or(src);
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let column = before.get(line_start..).map_or(0, |s| s.chars().count()) + 1;
    Position {
        offset,
        line,
        column,
    }
}

fn compile_regex(pattern: &str) -> Result<regex::Regex, String> {
    regex::RegexBuilder::new(pattern)
        .size_limit(MAX_REGEX_BYTES)
        .dfa_size_limit(MAX_REGEX_BYTES)
        .build()
        .map_err(|e| format!("invalid regular expression: {e}"))
}

/// The values an expression sees.
#[derive(Debug, Clone, Copy)]
pub struct Bindings<'a> {
    /// `{body, path, query, headers}`: the parsed request, header names lower-cased.
    pub request: &'a Value,
    /// The target record as stored before this call, or `null`.
    pub record: &'a Value,
    /// The request time.
    pub now: &'a Value,
    /// The item a key binds at the top level (another record, a list item), if any.
    pub it: Option<&'a Value>,
}

/// The session behind `held`, `count`, `exists`, `seq` and `mint`.
pub trait Host {
    /// The held record of `ty` with this id (or alternate id), if any.
    fn held(&mut self, ty: &str, id: &Value) -> Option<Value>;
    /// The held records of `ty`, for `count` and `exists`; the caller caps them
    /// (at 500, as `derived` does).
    fn all(&mut self, ty: &str) -> Vec<Value>;
    /// The next value of the per-session counter `scope`, from 1.
    fn seq(&mut self, scope: &str) -> i64;
    /// A fresh id matching `pattern`, or `None` when the pattern cannot be generated.
    fn mint(&mut self, pattern: &str) -> Option<String>;
}

/// A compiled expression.
#[derive(Debug)]
pub struct Program {
    source: String,
    root: parse::Node,
}

impl Program {
    /// Compiles `source`, refusing syntax errors, unknown names and functions,
    /// wrong numbers of arguments, invalid literal patterns and trees over the caps.
    pub fn compile(source: &str) -> Result<Program, CompileError> {
        let root = parse::parse(source).map_err(|f| CompileError {
            pos: position(source, f.pos),
            message: f.msg,
        })?;
        Ok(Program {
            source: source.to_string(),
            root,
        })
    }

    /// The source text.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// What the expression references, for validation against the contract.
    pub fn check(&self) -> Refs {
        check::refs(&self.source, &self.root)
    }

    /// Evaluates with the default budget of [`MAX_STEPS`].
    pub fn eval(&self, bindings: &Bindings<'_>, host: &mut dyn Host) -> Result<Value, EvalError> {
        self.eval_with_budget(bindings, host, MAX_STEPS)
    }

    /// Evaluates with a budget of `max_steps`.
    pub fn eval_with_budget(
        &self,
        bindings: &Bindings<'_>,
        host: &mut dyn Host,
        max_steps: u64,
    ) -> Result<Value, EvalError> {
        let mut e = eval::Eval {
            bindings,
            host,
            steps: std::cell::Cell::new(0),
            max_steps,
            scope: Vec::new(),
        };
        e.eval(&self.root).map_err(|f| EvalError {
            pos: position(&self.source, f.pos),
            message: f.msg,
        })
    }
}

/// Where a member path starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Root {
    /// `request.body`
    Body,
    /// `request.query`
    Query,
    /// `request.path`
    Path,
    /// `request.headers`
    Headers,
    /// `record`
    Record,
    /// `it` as the caller binds it
    It,
    /// A record of this type: `held('T', id)`, or `it` inside `count('T', ...)`
    /// and `exists('T', ...)`.
    Type(String),
}

/// One step of a member path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// A named member (`.name` or `['name']`).
    Member(String),
    /// An item of a list (or a computed key): `[i]`, or a macro variable.
    Item,
}

/// A member path an expression reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRef {
    /// Where the path starts.
    pub root: Root,
    /// The members under the root (empty for the root itself).
    pub path: Vec<Segment>,
    /// The last member's name (or the root's name).
    pub pos: Position,
}

impl fmt::Display for MemberRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.root {
            Root::Body => f.write_str("request.body")?,
            Root::Query => f.write_str("request.query")?,
            Root::Path => f.write_str("request.path")?,
            Root::Headers => f.write_str("request.headers")?,
            Root::Record => f.write_str("record")?,
            Root::It => f.write_str("it")?,
            Root::Type(t) => write!(f, "{t}")?,
        }
        for s in &self.path {
            match s {
                Segment::Member(m) => write!(f, ".{m}")?,
                Segment::Item => f.write_str("[]")?,
            }
        }
        Ok(())
    }
}

/// A resource type an expression names, and the function it names it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRef {
    /// The type name, as written.
    pub name: String,
    /// `held`, `count` or `exists`.
    pub function: &'static str,
    /// The type name's string literal.
    pub pos: Position,
}

/// A function or macro an expression calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionRef {
    /// The function or macro name.
    pub name: &'static str,
    /// The name in the source.
    pub pos: Position,
}

/// What [`Program::check`] finds, in the order the tree is walked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refs {
    /// The member paths read off `request.*`, `record`, `it` and typed records.
    pub members: Vec<MemberRef>,
    /// The type names passed to `held`, `count` and `exists`.
    pub types: Vec<TypeRef>,
    /// The functions and macros called.
    pub functions: Vec<FunctionRef>,
}
