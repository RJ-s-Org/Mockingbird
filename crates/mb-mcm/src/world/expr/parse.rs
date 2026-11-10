//! The AST and a hand-written Pratt parser, with the depth and node caps
//! enforced while the tree is built, and a name-resolution pass.

use regex::Regex;
use serde_json::Value;

use super::lex::{Tok, Token, lex};
use super::{Fail, MAX_DEPTH, MAX_NODES, REQUEST_PARTS, compile_regex};

#[derive(Debug)]
pub(super) struct Node {
    pub kind: Kind,
    /// Byte offset of the token the node is reported at (an operator, a member
    /// name, a function name, or the start of a literal or name).
    pub pos: usize,
    height: usize,
}

/// A string literal argument naming a resource type.
#[derive(Debug)]
pub(super) struct TypeArg {
    pub name: String,
    pub pos: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BinOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompOp {
    All,
    Exists,
    Map,
    Filter,
}

impl CompOp {
    pub fn name(self) -> &'static str {
        match self {
            CompOp::All => "all",
            CompOp::Exists => "exists",
            CompOp::Map => "map",
            CompOp::Filter => "filter",
        }
    }
}

/// The plain functions: callable as `f(a, b)` or, with the first argument as the
/// receiver, as `a.f(b)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Func {
    Size,
    StartsWith,
    EndsWith,
    Contains,
    Matches,
    Lower,
    Upper,
    Slug,
    String,
    Int,
    Double,
    Md5,
    Sha1,
    Sha256,
    Seq,
    Mint,
}

impl Func {
    const ALL: [Func; 16] = [
        Func::Size,
        Func::StartsWith,
        Func::EndsWith,
        Func::Contains,
        Func::Matches,
        Func::Lower,
        Func::Upper,
        Func::Slug,
        Func::String,
        Func::Int,
        Func::Double,
        Func::Md5,
        Func::Sha1,
        Func::Sha256,
        Func::Seq,
        Func::Mint,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Func::Size => "size",
            Func::StartsWith => "startsWith",
            Func::EndsWith => "endsWith",
            Func::Contains => "contains",
            Func::Matches => "matches",
            Func::Lower => "lower",
            Func::Upper => "upper",
            Func::Slug => "slug",
            Func::String => "string",
            Func::Int => "int",
            Func::Double => "double",
            Func::Md5 => "md5",
            Func::Sha1 => "sha1",
            Func::Sha256 => "sha256",
            Func::Seq => "seq",
            Func::Mint => "mint",
        }
    }

    pub fn arity(self) -> usize {
        match self {
            Func::StartsWith | Func::EndsWith | Func::Contains | Func::Matches => 2,
            _ => 1,
        }
    }

    /// Session functions are global calls only.
    fn receiver_ok(self) -> bool {
        !matches!(self, Func::Seq | Func::Mint)
    }

    fn by_name(name: &str) -> Option<Func> {
        Func::ALL.into_iter().find(|f| f.name() == name)
    }
}

#[derive(Debug)]
pub(super) enum Kind {
    Lit(Value),
    Ident(String),
    Member(Box<Node>, String),
    Index(Box<Node>, Box<Node>),
    List(Vec<Node>),
    Map(Vec<(Node, Node)>),
    Not(Box<Node>),
    Neg(Box<Node>),
    Bin(BinOp, Box<Node>, Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Cond(Box<Node>, Box<Node>, Box<Node>),
    /// A plain function call; a receiver call has its receiver as `args[0]`.
    /// `re` holds the compiled pattern of `matches` when it is a literal.
    Call {
        func: Func,
        args: Vec<Node>,
        re: Option<Box<Regex>>,
    },
    /// `has(operand.field)`.
    Has(Box<Node>, String),
    /// `range.all(var, body)` and the other receiver macros.
    Comp {
        op: CompOp,
        var: String,
        range: Box<Node>,
        body: Box<Node>,
    },
    /// `held(type, id)`.
    Held(TypeArg, Box<Node>),
    /// `count(type, cond)`, `it` bound to each held record of the type.
    Count(TypeArg, Box<Node>),
    /// `exists(type, cond)`, `it` bound to each held record of the type.
    ExistsType(TypeArg, Box<Node>),
    /// `sum(list, expr)`, `it` bound to each item.
    Sum(Box<Node>, Box<Node>),
}

impl Node {
    pub(super) fn children(&self) -> Vec<&Node> {
        match &self.kind {
            Kind::Lit(_) | Kind::Ident(_) => Vec::new(),
            Kind::Member(a, _) | Kind::Not(a) | Kind::Neg(a) | Kind::Has(a, _) => vec![a],
            Kind::Held(_, a) | Kind::Count(_, a) | Kind::ExistsType(_, a) => vec![a],
            Kind::Index(a, b)
            | Kind::Bin(_, a, b)
            | Kind::And(a, b)
            | Kind::Or(a, b)
            | Kind::Sum(a, b) => vec![a, b],
            Kind::Comp { range, body, .. } => vec![range, body],
            Kind::Cond(a, b, c) => vec![a, b, c],
            Kind::List(xs) | Kind::Call { args: xs, .. } => xs.iter().collect(),
            Kind::Map(kvs) => kvs.iter().flat_map(|(k, v)| [k, v]).collect(),
        }
    }
}

/// Parses `src` into a tree, then resolves its names.
pub(super) fn parse(src: &str) -> Result<Node, Fail> {
    let toks = lex(src)?;
    let mut p = Parser {
        toks,
        i: 0,
        nodes: 0,
        depth: 0,
    };
    let root = p.expr()?;
    if !matches!(p.peek(), Tok::Eof) {
        return Err(Fail::new(
            p.pos(),
            format!("unexpected {}", describe(p.peek())),
        ));
    }
    resolve(&root, &mut Vec::new())?;
    Ok(root)
}

struct Parser {
    toks: Vec<Token>,
    i: usize,
    nodes: usize,
    /// Current nesting of `expr` calls (parentheses, arguments, brackets).
    depth: usize,
}

const EOF: Tok = Tok::Eof;

impl Parser {
    fn peek(&self) -> &Tok {
        self.toks.get(self.i).map_or(&EOF, |t| &t.tok)
    }

    fn pos(&self) -> usize {
        self.toks
            .get(self.i)
            .or(self.toks.last())
            .map_or(0, |t| t.pos)
    }

    fn next(&mut self) -> Tok {
        let t = self.peek().clone();
        if self.i < self.toks.len() {
            self.i += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Tok, what: &str) -> Result<(), Fail> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(Fail::new(
                self.pos(),
                format!("expected {what}, found {}", describe(self.peek())),
            ))
        }
    }

    /// Builds a node, enforcing the node and depth caps.
    fn mk(&mut self, kind: Kind, pos: usize) -> Result<Node, Fail> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(Fail::new(
                pos,
                format!("expression too large (more than {MAX_NODES} nodes)"),
            ));
        }
        let mut node = Node {
            kind,
            pos,
            height: 1,
        };
        node.height = 1 + node.children().iter().map(|c| c.height).max().unwrap_or(0);
        if node.height > MAX_DEPTH {
            return Err(Fail::new(
                pos,
                format!("expression nested too deeply (more than {MAX_DEPTH} levels)"),
            ));
        }
        Ok(node)
    }

    /// expr := or ('?' expr ':' expr)?
    fn expr(&mut self) -> Result<Node, Fail> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Fail::new(
                self.pos(),
                format!("expression nested too deeply (more than {MAX_DEPTH} levels)"),
            ));
        }
        let r = self.ternary();
        self.depth -= 1;
        r
    }

    fn ternary(&mut self) -> Result<Node, Fail> {
        let cond = self.binary(1)?;
        let pos = self.pos();
        if !self.eat(&Tok::Question) {
            return Ok(cond);
        }
        let then = self.expr()?;
        self.expect(&Tok::Colon, "':' in a conditional")?;
        let other = self.expr()?;
        self.mk(
            Kind::Cond(Box::new(cond), Box::new(then), Box::new(other)),
            pos,
        )
    }

    /// Precedence climbing over the binary operators (all left-associative).
    fn binary(&mut self, min: u8) -> Result<Node, Fail> {
        let mut lhs = self.unary()?;
        loop {
            let Some(prec) = precedence(self.peek()) else {
                return Ok(lhs);
            };
            if prec < min {
                return Ok(lhs);
            }
            let pos = self.pos();
            let op = self.next();
            let rhs = self.binary(prec + 1)?;
            let (a, b) = (Box::new(lhs), Box::new(rhs));
            let kind = match op {
                Tok::Or => Kind::Or(a, b),
                Tok::And => Kind::And(a, b),
                Tok::Eq => Kind::Bin(BinOp::Eq, a, b),
                Tok::Ne => Kind::Bin(BinOp::Ne, a, b),
                Tok::Lt => Kind::Bin(BinOp::Lt, a, b),
                Tok::Le => Kind::Bin(BinOp::Le, a, b),
                Tok::Gt => Kind::Bin(BinOp::Gt, a, b),
                Tok::Ge => Kind::Bin(BinOp::Ge, a, b),
                Tok::In => Kind::Bin(BinOp::In, a, b),
                Tok::Plus => Kind::Bin(BinOp::Add, a, b),
                Tok::Minus => Kind::Bin(BinOp::Sub, a, b),
                Tok::Star => Kind::Bin(BinOp::Mul, a, b),
                Tok::Slash => Kind::Bin(BinOp::Div, a, b),
                _ => Kind::Bin(BinOp::Rem, a, b),
            };
            lhs = self.mk(kind, pos)?;
        }
    }

    /// Prefix operators, collected in a loop (not recursion), then a postfix chain.
    fn unary(&mut self) -> Result<Node, Fail> {
        let mut prefix = Vec::new();
        loop {
            let pos = self.pos();
            match self.peek() {
                Tok::Not | Tok::Minus => {
                    if prefix.len() >= MAX_DEPTH {
                        return Err(Fail::new(
                            pos,
                            format!("expression nested too deeply (more than {MAX_DEPTH} levels)"),
                        ));
                    }
                    let t = self.next();
                    prefix.push((t, pos));
                }
                _ => break,
            }
        }
        // `-` directly before an integer literal makes a negative literal, so the
        // smallest i64 can be written.
        let mut node = match (prefix.last(), self.peek()) {
            (Some((Tok::Minus, pos)), Tok::Int(n)) => {
                let (pos, n) = (*pos, *n);
                let lit = if n == 1u64 << 63 {
                    i64::MIN
                } else {
                    i64::try_from(n)
                        .map(|v| -v)
                        .map_err(|_| Fail::new(pos, "integer literal out of range"))?
                };
                self.next();
                prefix.pop();
                let lit = self.mk(Kind::Lit(Value::from(lit)), pos)?;
                self.postfix(lit)?
            }
            _ => {
                let p = self.primary()?;
                self.postfix(p)?
            }
        };
        while let Some((t, pos)) = prefix.pop() {
            let kind = if t == Tok::Not {
                Kind::Not(Box::new(node))
            } else {
                Kind::Neg(Box::new(node))
            };
            node = self.mk(kind, pos)?;
        }
        Ok(node)
    }

    fn postfix(&mut self, mut node: Node) -> Result<Node, Fail> {
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.next();
                    let pos = self.pos();
                    let Tok::Ident(name) = self.next() else {
                        return Err(Fail::new(pos, "expected a member name after '.'"));
                    };
                    if matches!(self.peek(), Tok::LParen) {
                        self.next();
                        let args = self.args(&Tok::RParen)?;
                        node = self.receiver_call(node, name, pos, args)?;
                    } else {
                        node = self.mk(Kind::Member(Box::new(node), name), pos)?;
                    }
                }
                Tok::LBrack => {
                    let pos = self.pos();
                    self.next();
                    let idx = self.expr()?;
                    self.expect(&Tok::RBrack, "']'")?;
                    node = self.mk(Kind::Index(Box::new(node), Box::new(idx)), pos)?;
                }
                _ => return Ok(node),
            }
        }
    }

    /// Comma-separated expressions up to `close` (consumed); a trailing comma is allowed.
    fn args(&mut self, close: &Tok) -> Result<Vec<Node>, Fail> {
        let mut out = Vec::new();
        loop {
            if self.eat(close) {
                return Ok(out);
            }
            out.push(self.expr()?);
            if !self.eat(&Tok::Comma) {
                let what = if *close == Tok::RParen { "')'" } else { "']'" };
                self.expect(close, what)?;
                return Ok(out);
            }
        }
    }

    fn primary(&mut self) -> Result<Node, Fail> {
        let pos = self.pos();
        match self.next() {
            Tok::Int(n) => {
                let v =
                    i64::try_from(n).map_err(|_| Fail::new(pos, "integer literal out of range"))?;
                self.mk(Kind::Lit(Value::from(v)), pos)
            }
            Tok::Double(d) => {
                let n = serde_json::Number::from_f64(d)
                    .ok_or_else(|| Fail::new(pos, "double literal out of range"))?;
                self.mk(Kind::Lit(Value::Number(n)), pos)
            }
            Tok::Str(s) => self.mk(Kind::Lit(Value::String(s)), pos),
            Tok::True => self.mk(Kind::Lit(Value::Bool(true)), pos),
            Tok::False => self.mk(Kind::Lit(Value::Bool(false)), pos),
            Tok::Null => self.mk(Kind::Lit(Value::Null), pos),
            Tok::Ident(name) => {
                if self.eat(&Tok::LParen) {
                    let args = self.args(&Tok::RParen)?;
                    self.global_call(name, pos, args)
                } else {
                    self.mk(Kind::Ident(name), pos)
                }
            }
            Tok::LParen => {
                let e = self.expr()?;
                self.expect(&Tok::RParen, "')'")?;
                Ok(e)
            }
            Tok::LBrack => {
                let items = self.args(&Tok::RBrack)?;
                self.mk(Kind::List(items), pos)
            }
            Tok::LBrace => {
                let mut kvs = Vec::new();
                loop {
                    if self.eat(&Tok::RBrace) {
                        break;
                    }
                    let k = self.expr()?;
                    self.expect(&Tok::Colon, "':' after a map key")?;
                    let v = self.expr()?;
                    kvs.push((k, v));
                    if !self.eat(&Tok::Comma) {
                        self.expect(&Tok::RBrace, "'}'")?;
                        break;
                    }
                }
                self.mk(Kind::Map(kvs), pos)
            }
            t => Err(Fail::new(pos, format!("unexpected {}", describe(&t)))),
        }
    }

    fn global_call(&mut self, name: String, pos: usize, args: Vec<Node>) -> Result<Node, Fail> {
        let arity = |n: usize| -> Result<(), Fail> {
            if args.len() == n {
                Ok(())
            } else {
                Err(Fail::new(
                    pos,
                    format!(
                        "{name}() takes {n} argument{}, got {}",
                        if n == 1 { "" } else { "s" },
                        args.len()
                    ),
                ))
            }
        };
        match name.as_str() {
            "has" => {
                arity(1)?;
                let mut args = args.into_iter();
                let Some(Node {
                    kind: Kind::Member(operand, field),
                    ..
                }) = args.next()
                else {
                    return Err(Fail::new(
                        pos,
                        "has() takes a member access, such as has(a.b)",
                    ));
                };
                self.mk(Kind::Has(operand, field), pos)
            }
            "held" | "count" | "exists" => {
                arity(2)?;
                let mut args = args.into_iter();
                let (Some(ty), Some(rest)) = (args.next(), args.next()) else {
                    return Err(Fail::new(pos, format!("{name}() takes 2 arguments")));
                };
                let ty = type_arg(&name, ty)?;
                let rest = Box::new(rest);
                let kind = match name.as_str() {
                    "held" => Kind::Held(ty, rest),
                    "count" => Kind::Count(ty, rest),
                    _ => Kind::ExistsType(ty, rest),
                };
                self.mk(kind, pos)
            }
            "sum" => {
                arity(2)?;
                let mut args = args.into_iter();
                let (Some(list), Some(e)) = (args.next(), args.next()) else {
                    return Err(Fail::new(pos, "sum() takes 2 arguments"));
                };
                self.mk(Kind::Sum(Box::new(list), Box::new(e)), pos)
            }
            "all" | "map" | "filter" => Err(Fail::new(
                pos,
                format!("{name}() is a list macro: write list.{name}(x, ...)"),
            )),
            _ => {
                let func = Func::by_name(&name)
                    .ok_or_else(|| Fail::new(pos, format!("unknown function {name}()")))?;
                arity(func.arity())?;
                self.call(func, args, pos)
            }
        }
    }

    fn receiver_call(
        &mut self,
        recv: Node,
        name: String,
        pos: usize,
        args: Vec<Node>,
    ) -> Result<Node, Fail> {
        let op = match name.as_str() {
            "all" => Some(CompOp::All),
            "exists" => Some(CompOp::Exists),
            "map" => Some(CompOp::Map),
            "filter" => Some(CompOp::Filter),
            _ => None,
        };
        if let Some(op) = op {
            let mut it = args.into_iter();
            let (Some(var), Some(body), None) = (it.next(), it.next(), it.next()) else {
                return Err(Fail::new(
                    pos,
                    format!("{name}() takes 2 arguments: {name}(x, ...)"),
                ));
            };
            let Kind::Ident(var) = var.kind else {
                return Err(Fail::new(
                    var.pos,
                    format!("the first argument of {name}() must be a variable name"),
                ));
            };
            return self.mk(
                Kind::Comp {
                    op,
                    var,
                    range: Box::new(recv),
                    body: Box::new(body),
                },
                pos,
            );
        }
        let Some(func) = Func::by_name(&name).filter(|f| f.receiver_ok()) else {
            let msg = match name.as_str() {
                "has" | "held" | "count" | "sum" | "seq" | "mint" => {
                    format!("{name}() is a global function, not a method")
                }
                _ => format!("unknown function {name}()"),
            };
            return Err(Fail::new(pos, msg));
        };
        let mut all = Vec::with_capacity(args.len() + 1);
        all.push(recv);
        all.extend(args);
        if all.len() != func.arity() {
            let want = func.arity() - 1;
            return Err(Fail::new(
                pos,
                format!(
                    "{name}() as a method takes {want} argument{}, got {}",
                    if want == 1 { "" } else { "s" },
                    all.len() - 1
                ),
            ));
        }
        self.call(func, all, pos)
    }

    fn call(&mut self, func: Func, args: Vec<Node>, pos: usize) -> Result<Node, Fail> {
        let mut re = None;
        if func == Func::Matches
            && let Some(Node {
                kind: Kind::Lit(Value::String(p)),
                pos,
                ..
            }) = args.get(1)
        {
            re = Some(Box::new(compile_regex(p).map_err(|m| Fail::new(*pos, m))?));
        }
        self.mk(Kind::Call { func, args, re }, pos)
    }
}

fn type_arg(func: &str, n: Node) -> Result<TypeArg, Fail> {
    match n.kind {
        Kind::Lit(Value::String(name)) if !name.is_empty() => Ok(TypeArg { name, pos: n.pos }),
        _ => Err(Fail::new(
            n.pos,
            format!(
                "the first argument of {func}() must be a type name in quotes, such as {func}('Order', ...)"
            ),
        )),
    }
}

fn precedence(t: &Tok) -> Option<u8> {
    Some(match t {
        Tok::Or => 1,
        Tok::And => 2,
        Tok::Eq | Tok::Ne | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge | Tok::In => 3,
        Tok::Plus | Tok::Minus => 4,
        Tok::Star | Tok::Slash | Tok::Percent => 5,
        _ => return None,
    })
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Eof => "end of expression".into(),
        Tok::Int(n) => format!("number {n}"),
        Tok::Double(d) => format!("number {d}"),
        Tok::Str(_) => "string".into(),
        Tok::Ident(s) => format!("name '{s}'"),
        Tok::True => "'true'".into(),
        Tok::False => "'false'".into(),
        Tok::Null => "'null'".into(),
        Tok::In => "'in'".into(),
        Tok::LParen => "'('".into(),
        Tok::RParen => "')'".into(),
        Tok::LBrack => "'['".into(),
        Tok::RBrack => "']'".into(),
        Tok::LBrace => "'{'".into(),
        Tok::RBrace => "'}'".into(),
        Tok::Dot => "'.'".into(),
        Tok::Comma => "','".into(),
        Tok::Colon => "':'".into(),
        Tok::Question => "'?'".into(),
        Tok::And => "'&&'".into(),
        Tok::Or => "'||'".into(),
        Tok::Not => "'!'".into(),
        Tok::Eq => "'=='".into(),
        Tok::Ne => "'!='".into(),
        Tok::Lt => "'<'".into(),
        Tok::Le => "'<='".into(),
        Tok::Gt => "'>'".into(),
        Tok::Ge => "'>='".into(),
        Tok::Plus => "'+'".into(),
        Tok::Minus => "'-'".into(),
        Tok::Star => "'*'".into(),
        Tok::Slash => "'/'".into(),
        Tok::Percent => "'%'".into(),
    }
}

/// The names an expression may use free: the bindings.
const FREE: [&str; 4] = ["request", "record", "now", "it"];

/// Checks every name is a binding or a macro variable in scope, and that
/// `request` is only reached through one of its parts.
fn resolve<'a>(n: &'a Node, scope: &mut Vec<&'a str>) -> Result<(), Fail> {
    match &n.kind {
        Kind::Ident(name) => {
            if !scope.contains(&name.as_str()) && !FREE.contains(&name.as_str()) {
                return Err(Fail::new(
                    n.pos,
                    format!("unknown name '{name}' (the bindings are request, record, now and it)"),
                ));
            }
            Ok(())
        }
        Kind::Member(base, part) if is_request(base, scope) => {
            if REQUEST_PARTS.contains(&part.as_str()) {
                Ok(())
            } else {
                Err(Fail::new(
                    n.pos,
                    format!("request has no '{part}': use request.body, .query, .path or .headers"),
                ))
            }
        }
        Kind::Comp {
            var, range, body, ..
        } => {
            if FREE[..3].contains(&var.as_str()) {
                return Err(Fail::new(
                    n.pos,
                    format!("'{var}' cannot be a macro variable"),
                ));
            }
            resolve(range, scope)?;
            scope.push(var);
            let r = resolve(body, scope);
            scope.pop();
            r
        }
        Kind::Count(_, cond) | Kind::ExistsType(_, cond) => {
            scope.push("it");
            let r = resolve(cond, scope);
            scope.pop();
            r
        }
        Kind::Sum(list, e) => {
            resolve(list, scope)?;
            scope.push("it");
            let r = resolve(e, scope);
            scope.pop();
            r
        }
        _ => n.children().into_iter().try_for_each(|c| resolve(c, scope)),
    }
}

fn is_request(n: &Node, scope: &[&str]) -> bool {
    matches!(&n.kind, Kind::Ident(s) if s == "request" && !scope.contains(&"request"))
}
