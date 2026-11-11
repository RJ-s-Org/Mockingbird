//! The static walk: what a compiled expression references, for validation
//! against the contract.

use serde_json::Value;

use super::parse::{Kind, Node};
use super::{FunctionRef, MemberRef, Refs, Root, Segment, TypeRef, position};

/// What a name is known to point to: a root and a path under it, or nothing
/// statically known (a macro variable over a computed list).
type Origin = Option<(Root, Vec<Segment>)>;

pub(super) fn refs(src: &str, root: &Node) -> Refs {
    let mut w = Walker {
        src,
        scope: Vec::new(),
        refs: Refs::default(),
    };
    w.walk(root);
    w.refs
}

struct Walker<'a> {
    src: &'a str,
    scope: Vec<(&'a str, Origin)>,
    refs: Refs,
}

impl<'a> Walker<'a> {
    fn function(&mut self, name: &'static str, pos: usize) {
        self.refs.functions.push(FunctionRef {
            name,
            pos: position(self.src, pos),
        });
    }

    fn ty(&mut self, name: &str, function: &'static str, pos: usize) {
        self.refs.types.push(TypeRef {
            name: name.to_string(),
            function,
            pos: position(self.src, pos),
        });
    }

    fn with<T>(&mut self, var: &'a str, origin: Origin, f: impl FnOnce(&mut Self) -> T) -> T {
        self.scope.push((var, origin));
        let r = f(self);
        self.scope.pop();
        r
    }

    /// The root and path a member/index chain reaches, when it is static.
    fn path_of(&self, n: &Node) -> Origin {
        match &n.kind {
            Kind::Ident(name) => {
                if let Some((_, o)) = self.scope.iter().rev().find(|(v, _)| v == name) {
                    return o.clone();
                }
                match name.as_str() {
                    "record" => Some((Root::Record, Vec::new())),
                    "it" => Some((Root::It, Vec::new())),
                    "value" => Some((Root::Value, Vec::new())),
                    _ => None,
                }
            }
            Kind::Member(base, field) => {
                if is_name(base, "request") && !self.in_scope("request") {
                    let part = match field.as_str() {
                        "body" => Root::Body,
                        "query" => Root::Query,
                        "path" => Root::Path,
                        "headers" => Root::Headers,
                        _ => return None,
                    };
                    return Some((part, Vec::new()));
                }
                let (root, mut path) = self.path_of(base)?;
                path.push(Segment::Member(field.clone()));
                Some((root, path))
            }
            Kind::Index(base, idx) => {
                let (root, mut path) = self.path_of(base)?;
                path.push(match &idx.kind {
                    Kind::Lit(Value::String(s)) => Segment::Member(s.clone()),
                    _ => Segment::Item,
                });
                Some((root, path))
            }
            Kind::Held(ty, _) => Some((Root::Type(ty.name.clone()), Vec::new())),
            _ => None,
        }
    }

    fn in_scope(&self, name: &str) -> bool {
        self.scope.iter().any(|(v, _)| *v == name)
    }

    fn member(&mut self, origin: (Root, Vec<Segment>), pos: usize) {
        let (root, path) = origin;
        self.refs.members.push(MemberRef {
            root,
            path,
            pos: position(self.src, pos),
        });
    }

    fn walk(&mut self, n: &'a Node) {
        match &n.kind {
            Kind::Lit(_) => {}
            Kind::Ident(_) => {
                if let Some(o) = self.path_of(n) {
                    self.member(o, n.pos);
                }
            }
            Kind::Member(..) | Kind::Index(..) => match self.path_of(n) {
                Some(o) => {
                    self.member(o, n.pos);
                    self.chain_children(n);
                }
                None => n.children().into_iter().for_each(|c| self.walk(c)),
            },
            Kind::Has(operand, field) => {
                self.function("has", n.pos);
                match self.path_of(operand) {
                    Some((root, mut path)) => {
                        path.push(Segment::Member(field.clone()));
                        self.member((root, path), n.pos);
                        self.chain_children(operand);
                    }
                    None => self.walk(operand),
                }
            }
            Kind::Call { func, args, .. } => {
                self.function(func.name(), n.pos);
                args.iter().for_each(|a| self.walk(a));
            }
            Kind::Held(ty, id) => {
                self.function("held", n.pos);
                self.ty(&ty.name, "held", ty.pos);
                self.walk(id);
            }
            Kind::Count(ty, cond) | Kind::ExistsType(ty, cond) => {
                let name = if matches!(n.kind, Kind::Count(..)) {
                    "count"
                } else {
                    "exists"
                };
                self.function(name, n.pos);
                self.ty(&ty.name, name, ty.pos);
                let origin = Some((Root::Type(ty.name.clone()), Vec::new()));
                self.with("it", origin, |w| w.walk(cond));
            }
            Kind::Children(_, r#where) => {
                self.function("children", n.pos);
                if let Some(w) = r#where {
                    self.walk(w);
                }
            }
            Kind::Sum(list, e) => {
                self.function("sum", n.pos);
                self.walk(list);
                let origin = self.item_of(list);
                self.with("it", origin, |w| w.walk(e));
            }
            Kind::Comp {
                op,
                var,
                range,
                body,
            } => {
                self.function(op.name(), n.pos);
                self.walk(range);
                let origin = self.item_of(range);
                self.with(var, origin, |w| w.walk(body));
            }
            _ => n.children().into_iter().for_each(|c| self.walk(c)),
        }
    }

    fn item_of(&self, list: &Node) -> Origin {
        let (root, mut path) = self.path_of(list)?;
        path.push(Segment::Item);
        Some((root, path))
    }

    /// Walks what a reported chain contains that is not part of its path: index
    /// expressions and `held`'s arguments.
    fn chain_children(&mut self, n: &'a Node) {
        match &n.kind {
            Kind::Member(base, _) => self.chain_children(base),
            Kind::Index(base, idx) => {
                self.walk(idx);
                self.chain_children(base);
            }
            Kind::Held(..) => self.walk(n),
            _ => {}
        }
    }
}

fn is_name(n: &Node, name: &str) -> bool {
    matches!(&n.kind, Kind::Ident(s) if s == name)
}

/// Calls `found` with each string literal a map literal under `n` gives `key`, and
/// its offset.
pub(super) fn map_literals(n: &Node, key: &str, found: &mut dyn FnMut(&str, usize)) {
    if let Kind::Map(entries) = &n.kind {
        for (k, v) in entries {
            if let (Kind::Lit(Value::String(k)), Kind::Lit(Value::String(text))) =
                (&k.kind, &v.kind)
                && k == key
            {
                found(text, v.pos);
            }
        }
    }
    for c in n.children() {
        map_literals(c, key, found);
    }
}
