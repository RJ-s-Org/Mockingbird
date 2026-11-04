//! Request routing for the REST adapter.
//!
//! A route pattern is a list of `/`-separated segments. Each segment is
//! one of, in order of precedence when several routes match a request:
//!
//! 1. static text (`/orders`);
//! 2. a template: parameters with literal text around or between them in
//!    one segment (`/files/{name}.json`, `/items/{id}:archive`, OData keys
//!    `/companies({id})` and `/pairs(a={a},b={b})`). Between templates, the
//!    one with more literal text (bytes, all its pieces together) comes
//!    first; templates with as much literal text are ordered by their
//!    pieces, compared as text, so the order never depends on the contract's
//!    order;
//! 3. a parameter that is the whole segment (`/orders/{id}`);
//! 4. a catch-all, the last segment only (`/static/{*path}`), which takes
//!    the rest of the path, slashes included.
//!
//! The first segment where two matching routes differ decides between
//! them, so `/a/b/{x}` wins over `/a/{y}/c` for `/a/b/c`: a route that
//! fails further down gives way to the next candidate of that segment
//! (backtracking). A parameter never matches an empty string nor a `/`,
//! `{{` and `}}` stand for literal braces, and two parameters of one
//! segment must be separated by literal text.
//!
//! Within a template, values are split lazily: each literal between two
//! parameters is its first occurrence (after at least one byte of value)
//! that lets the rest match, so `{name}.{ext}` reads `a.tar.gz` as `a` and
//! `tar.gz`, and `{a}-{b}` reads `1-2-3` as `1` and `2-3`. A literal ending
//! the segment is matched at its end (`{name}.json` reads `a.b.json` as
//! `a.b`). Matching a segment against a template takes time linear in the
//! segment's length times the template's number of pieces.
//!
//! This module replaced `matchit`. Wherever `matchit` matched a request,
//! this router picks the same operation and parameters; it also matches
//! some requests `matchit` answered with a 404 because `matchit` did not
//! backtrack out of a template (`/files/{id}/{rev}` with
//! `/files/{name}.json/meta` routes `GET /files/f.json/7` to the first), and
//! it accepts route sets `matchit` refused: any number of parameters per
//! segment, any siblings below a template (`/c({id})/a`, `/c({id})/b({x})`)
//! and a catch-all beside deeper routes. Only routes matching exactly the
//! same requests conflict.

use std::cmp::Reverse;

use mb_mcm::{Binding, Bundle, HttpMethod};
use serde_json::{Map, Value};

/// Why one HTTP operation of a contract is not routed. The other
/// operations are routed regardless ([`RouteTable::skipped`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    /// Operation `op`'s route matches exactly the requests an earlier
    /// operation `other` of the same method already takes (`/a/{x}` and
    /// `/a/{y}`, or `/a` and `/a/`); the earlier one keeps them.
    #[error(
        "operation {op}: cannot route {method} {path}: it matches the same requests as operation {other} ({method} {other_path}), which takes them"
    )]
    Conflict {
        op: String,
        method: HttpMethod,
        path: String,
        other: String,
        other_path: String,
    },
    /// Operation `op`'s path is not a valid route pattern (an unclosed
    /// `{`, an empty parameter name, a catch-all that is not the last
    /// segment, two parameters with nothing between them...).
    #[error("operation {op}: cannot route {method} {path}: {reason}")]
    Invalid {
        op: String,
        method: HttpMethod,
        path: String,
        reason: String,
    },
}

impl RouteError {
    /// The id of the operation that is not routed.
    #[must_use]
    pub fn operation(&self) -> &str {
        match self {
            RouteError::Conflict { op, .. } | RouteError::Invalid { op, .. } => op,
        }
    }
}

/// Maps `(method, path)` to an operation id.
///
/// Built with [`RouteTable::with_base_paths`], it also routes each
/// operation under the contract's base paths (`/v1/orders` as well as
/// `/orders`). A path matched by a contract route is never taken by a
/// prefixed one.
pub struct RouteTable {
    contract: Trie,
    /// The contract routes under the base paths, consulted when no
    /// contract route matches.
    prefixed: Trie,
    skipped: Vec<RouteError>,
}

/// Shows how many routes there are and which operations were skipped.
impl std::fmt::Debug for RouteTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteTable")
            .field("routes", &self.contract.routes.len())
            .field("prefixed_routes", &self.prefixed.routes.len())
            .field("skipped", &self.skipped)
            .finish_non_exhaustive()
    }
}

const ALL_METHODS: [HttpMethod; 7] = [
    HttpMethod::Get,
    HttpMethod::Post,
    HttpMethod::Put,
    HttpMethod::Patch,
    HttpMethod::Delete,
    HttpMethod::Head,
    HttpMethod::Options,
];

/// The index of `method` in [`ALL_METHODS`].
fn slot(method: HttpMethod) -> usize {
    match method {
        HttpMethod::Get => 0,
        HttpMethod::Post => 1,
        HttpMethod::Put => 2,
        HttpMethod::Patch => 3,
        HttpMethod::Delete => 4,
        HttpMethod::Head => 5,
        HttpMethod::Options => 6,
    }
}

impl RouteTable {
    /// The routes of every HTTP operation of `bundle`. An operation whose
    /// route cannot be registered is left out and listed in
    /// [`RouteTable::skipped`]; the others are routed.
    #[must_use]
    pub fn new(bundle: &Bundle) -> Self {
        Self::with_base_paths(bundle, &[])
    }

    /// [`RouteTable::new`], with every operation also routed under each of
    /// `base_paths` (as `mb_import::base_paths` returns them: Swagger
    /// `basePath`, OpenAPI `servers[].url` paths). A trailing `/` is
    /// ignored; a base path that does not start with `/`, is the root or
    /// contains `{` or `}` is skipped. When a prefixed route conflicts with
    /// another prefixed route, the first one is kept; a contract route
    /// matching the same request always wins. Base paths never add to
    /// [`RouteTable::skipped`].
    #[must_use]
    pub fn with_base_paths(bundle: &Bundle, base_paths: &[String]) -> Self {
        let mut contract = Trie::default();
        let mut skipped = Vec::new();
        for op in &bundle.operations {
            let Binding::Http { method, path } = &op.binding else {
                continue;
            };
            if let Err(e) = contract.insert(*method, normalize(path), path, &op.id) {
                skipped.push(e.for_operation(&op.id, *method, path));
            }
        }
        let mut prefixed = Trie::default();
        for base in base_paths.iter().filter_map(|b| base_path(b)) {
            for op in &bundle.operations {
                let Binding::Http { method, path } = &op.binding else {
                    continue;
                };
                let route = match normalize(path) {
                    "/" => base.to_owned(),
                    route => format!("{base}{route}"),
                };
                if let Err(e) = prefixed.insert(*method, &route, &route, &op.id) {
                    let error = e.for_operation(&op.id, *method, &route);
                    tracing::debug!(operation = %op.id, base_path = base, %error, "prefixed route skipped");
                }
            }
        }
        Self {
            contract,
            prefixed,
            skipped,
        }
    }

    /// The contract operations that are not routed, in contract order.
    #[must_use]
    pub fn skipped(&self) -> &[RouteError] {
        &self.skipped
    }

    /// The operation id and percent-decoded path parameters for a raw
    /// (still percent-encoded) request path. One trailing `/` is ignored.
    #[must_use]
    pub fn find(&self, method: HttpMethod, path: &str) -> Option<(String, Map<String, Value>)> {
        let path = normalize(path);
        let mut values = Vec::new();
        let route = [&self.contract, &self.prefixed]
            .into_iter()
            .find_map(|trie| trie.find(slot(method), path, &mut values))?;
        let params = route
            .names
            .iter()
            .zip(values)
            .map(|(k, v)| (k.clone(), Value::String(percent_decode(v))))
            .collect();
        Some((route.op.clone(), params))
    }

    /// Methods that have an operation matching `path`, in a stable order.
    #[must_use]
    pub fn allowed(&self, path: &str) -> Vec<HttpMethod> {
        let path = normalize(path);
        let mut found = 0u8;
        let mut scratch = Vec::new();
        let left = tokens(path);
        for trie in [&self.contract, &self.prefixed] {
            trie.root
                .allowed(path, Some(0), left, &mut found, &mut scratch);
        }
        ALL_METHODS
            .into_iter()
            .filter(|m| found & (1 << slot(*m)) != 0)
            .collect()
    }
}

/// The routes of every method, as a tree of segments.
#[derive(Debug, Default)]
struct Trie {
    root: Node,
    routes: Vec<Route>,
}

#[derive(Debug)]
struct Route {
    op: String,
    /// The path as the contract wrote it, for conflict messages.
    path: String,
    /// The parameter names, in the order their values are captured.
    names: Vec<String>,
}

/// One segment position of the tree. Children are tried in precedence
/// order: static, templates (sorted by [`TemplateRank`]), parameter, then
/// the catch-alls ending here.
#[derive(Debug)]
struct Node {
    /// By method slot, the route whose last segment is this node.
    routes: [Option<usize>; 7],
    /// By method slot, the route whose catch-all follows this node.
    catch_all: [Option<usize>; 7],
    /// The method slots with a route in this node or below it.
    methods: u8,
    /// The fewest and most tokens a request may have left at this node
    /// to reach a route in it or below (`usize::MAX`: a catch-all).
    min_left: usize,
    max_left: usize,
    /// Sorted by text, for binary search.
    statics: Vec<(String, Node)>,
    templates: Vec<Template>,
    /// `templates` by leading literal, rebuilt when one is added.
    template_index: TemplateIndex,
    param: Option<Box<Node>>,
}

impl Default for Node {
    fn default() -> Self {
        Node {
            routes: [None; 7],
            catch_all: [None; 7],
            methods: 0,
            min_left: usize::MAX,
            max_left: 0,
            statics: Vec::new(),
            templates: Vec::new(),
            template_index: TemplateIndex::default(),
            param: None,
        }
    }
}

/// How many templates a node scans one by one; above that, and when a
/// token has at most this many candidate leading literals, only templates
/// whose leading literal starts the token are tried.
const TEMPLATE_SCAN: usize = 8;

/// A node's templates by leading literal (empty for a template that
/// starts with a parameter).
#[derive(Debug, Default)]
struct TemplateIndex {
    /// Sorted by literal: the indexes into `templates` (ascending, so in
    /// rank order) of the templates starting with it.
    by_prefix: Vec<(String, Vec<usize>)>,
    /// The distinct leading literal lengths, ascending.
    lens: Vec<usize>,
}

impl TemplateIndex {
    fn new(templates: &[Template]) -> Self {
        let mut by_prefix: Vec<(String, Vec<usize>)> = Vec::new();
        for (i, t) in templates.iter().enumerate() {
            let prefix = match t.rank.pieces.first() {
                Some(Piece::Lit(text)) => text.as_str(),
                _ => "",
            };
            match by_prefix.binary_search_by(|(p, _)| p.as_str().cmp(prefix)) {
                Ok(at) => by_prefix[at].1.push(i),
                Err(at) => by_prefix.insert(at, (prefix.to_owned(), vec![i])),
            }
        }
        let mut lens: Vec<usize> = by_prefix.iter().map(|(p, _)| p.len()).collect();
        lens.sort_unstable();
        lens.dedup();
        Self { by_prefix, lens }
    }
}

/// The indexes of the templates that may match a token, in rank order.
enum Candidates<'n> {
    All(std::ops::Range<usize>),
    /// Ascending index lists, merged.
    Merge {
        lists: [&'n [usize]; TEMPLATE_SCAN],
        k: usize,
    },
}

impl Iterator for Candidates<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        match self {
            Candidates::All(range) => range.next(),
            Candidates::Merge { lists, k } => {
                let (j, first) = lists[..*k]
                    .iter()
                    .enumerate()
                    .filter_map(|(j, l)| Some((j, *l.first()?)))
                    .min_by_key(|(_, first)| *first)?;
                lists[j] = &lists[j][1..];
                Some(first)
            }
        }
    }
}

#[derive(Debug)]
struct Template {
    rank: TemplateRank,
    node: Node,
}

/// A template's precedence among its siblings: lower sorts first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TemplateRank {
    /// More literal text first.
    literal: Reverse<usize>,
    /// The literal text and parameter positions, names left out.
    pieces: Vec<Piece>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Piece {
    Lit(String),
    Param,
}

/// Why a route was not inserted, before the operation is known.
#[derive(Debug)]
enum InsertError {
    Conflict { other: String, other_path: String },
    Invalid(String),
}

impl InsertError {
    fn for_operation(self, op: &str, method: HttpMethod, path: &str) -> RouteError {
        let (op, path) = (op.to_owned(), path.to_owned());
        match self {
            InsertError::Conflict { other, other_path } => RouteError::Conflict {
                op,
                method,
                path,
                other,
                other_path,
            },
            InsertError::Invalid(reason) => RouteError::Invalid {
                op,
                method,
                path,
                reason,
            },
        }
    }
}

impl Trie {
    /// Add `pattern` (normalized) for `op`; `path` is the contract's own
    /// spelling, kept for messages.
    fn insert(
        &mut self,
        method: HttpMethod,
        pattern: &str,
        path: &str,
        op: &str,
    ) -> Result<(), InsertError> {
        let segments = parse(pattern).map_err(InsertError::Invalid)?;
        let slot = slot(method);
        let bit = 1u8 << slot;
        let mut names = Vec::new();
        // A conflict means the same nodes already exist: marking them with
        // the method on the way down changes nothing then.
        let mut node = &mut self.root;
        let mut end_catch_all = false;
        let catch_all = matches!(segments.last(), Some(Segment::CatchAll(_)));
        let n = segments.len();
        for (depth, segment) in segments.iter().enumerate() {
            node.methods |= bit;
            node.widen(n - depth, catch_all);
            node = match segment {
                Segment::Static(text) => {
                    let i = match node.statics.binary_search_by(|(t, _)| t.as_str().cmp(text)) {
                        Ok(i) => i,
                        Err(i) => {
                            node.statics.insert(i, (text.clone(), Node::default()));
                            i
                        }
                    };
                    &mut node.statics[i].1
                }
                Segment::Template(parts) => {
                    names.extend(parts.iter().filter_map(|p| match p {
                        Part::Param(name) => Some(name.clone()),
                        Part::Lit(_) => None,
                    }));
                    let rank = template_rank(parts);
                    let i = match node.templates.binary_search_by(|t| t.rank.cmp(&rank)) {
                        Ok(i) => i,
                        Err(i) => {
                            node.templates.insert(
                                i,
                                Template {
                                    rank,
                                    node: Node::default(),
                                },
                            );
                            node.template_index = TemplateIndex::new(&node.templates);
                            i
                        }
                    };
                    &mut node.templates[i].node
                }
                Segment::Param(name) => {
                    names.push(name.clone());
                    node.param.get_or_insert_with(Box::default)
                }
                Segment::CatchAll(name) => {
                    names.push(name.clone());
                    end_catch_all = true;
                    break;
                }
            };
        }
        node.methods |= bit;
        if !end_catch_all {
            node.widen(0, false);
        }
        let target = if end_catch_all {
            &mut node.catch_all[slot]
        } else {
            &mut node.routes[slot]
        };
        if let Some(i) = *target {
            let other = &self.routes[i];
            return Err(InsertError::Conflict {
                other: other.op.clone(),
                other_path: other.path.clone(),
            });
        }
        *target = Some(self.routes.len());
        self.routes.push(Route {
            op: op.to_owned(),
            path: path.to_owned(),
            names,
        });
        Ok(())
    }

    /// The route of highest precedence for method `slot` matching `path`
    /// (normalized); its raw parameter values are left in `values`.
    fn find<'p>(&self, slot: usize, path: &'p str, values: &mut Vec<&'p str>) -> Option<&Route> {
        values.clear();
        let i = self
            .root
            .find(slot, 1 << slot, path, Some(0), tokens(path), values)?;
        self.routes.get(i)
    }
}

/// The token starting at `at` and where the next one starts, if any.
fn token(path: &str, at: usize) -> (&str, Option<usize>) {
    let rest = &path[at..];
    match rest.find('/') {
        Some(end) => (&rest[..end], Some(at + end + 1)),
        None => (rest, None),
    }
}

/// How many tokens `path` has.
fn tokens(path: &str) -> usize {
    path.bytes().filter(|&b| b == b'/').count() + 1
}

impl Node {
    /// Count a route reaching this node with `left` tokens to go (at
    /// least `left` for a catch-all).
    fn widen(&mut self, left: usize, catch_all: bool) {
        self.min_left = self.min_left.min(left);
        self.max_left = if catch_all {
            usize::MAX
        } else {
            self.max_left.max(left)
        };
    }

    /// Whether a route of this node or below may take a request with
    /// `left` tokens to go.
    fn reaches(&self, left: usize) -> bool {
        (self.min_left..=self.max_left).contains(&left)
    }

    /// The templates that may match `token`, in rank order.
    fn candidates(&self, token: &str) -> Candidates<'_> {
        let all = Candidates::All(0..self.templates.len());
        if self.templates.len() <= TEMPLATE_SCAN {
            return all;
        }
        let index = &self.template_index;
        let mut lists: [&[usize]; TEMPLATE_SCAN] = [&[]; TEMPLATE_SCAN];
        let mut k = 0;
        for &len in &index.lens {
            let Some(prefix) = token.get(..len) else {
                continue;
            };
            if let Ok(i) = index
                .by_prefix
                .binary_search_by(|(p, _)| p.as_str().cmp(prefix))
            {
                if k == TEMPLATE_SCAN {
                    return all;
                }
                lists[k] = &index.by_prefix[i].1;
                k += 1;
            }
        }
        Candidates::Merge { lists, k }
    }

    /// Depth-first in precedence order, so the first route found is the
    /// one whose first differing segment ranks highest. `at` is where the
    /// next token starts (`None` past the last one), `left` how many
    /// tokens remain. Every node is visited at most once per request.
    fn find<'p>(
        &self,
        slot: usize,
        bit: u8,
        path: &'p str,
        at: Option<usize>,
        left: usize,
        values: &mut Vec<&'p str>,
    ) -> Option<usize> {
        if self.methods & bit == 0 || !self.reaches(left) {
            return None;
        }
        let Some(at) = at else {
            return self.routes[slot];
        };
        let (token, next) = token(path, at);
        let below = left - 1;
        if let Ok(i) = self
            .statics
            .binary_search_by(|(t, _)| t.as_str().cmp(token))
            && let Some(found) = self.statics[i].1.find(slot, bit, path, next, below, values)
        {
            return Some(found);
        }
        for i in self.candidates(token) {
            let template = &self.templates[i];
            if template.node.methods & bit == 0 || !template.node.reaches(below) {
                continue;
            }
            let mark = values.len();
            if match_template(&template.rank.pieces, token, values)
                && let Some(found) = template.node.find(slot, bit, path, next, below, values)
            {
                return Some(found);
            }
            values.truncate(mark);
        }
        if let Some(child) = &self.param
            && !token.is_empty()
        {
            values.push(token);
            if let Some(found) = child.find(slot, bit, path, next, below, values) {
                return Some(found);
            }
            values.pop();
        }
        if let Some(found) = self.catch_all[slot] {
            let rest = &path[at..];
            if !rest.is_empty() {
                values.push(rest);
                return Some(found);
            }
        }
        None
    }

    /// Adds to `found` the method bits of every route matching `path` from
    /// here, in one walk; `scratch` takes template values, unused.
    fn allowed<'p>(
        &self,
        path: &'p str,
        at: Option<usize>,
        left: usize,
        found: &mut u8,
        scratch: &mut Vec<&'p str>,
    ) {
        if self.methods & !*found == 0 || !self.reaches(left) {
            return;
        }
        let Some(at) = at else {
            *found |= bits(&self.routes);
            return;
        };
        let (token, next) = token(path, at);
        let below = left - 1;
        if let Ok(i) = self
            .statics
            .binary_search_by(|(t, _)| t.as_str().cmp(token))
        {
            self.statics[i].1.allowed(path, next, below, found, scratch);
        }
        for i in self.candidates(token) {
            let template = &self.templates[i];
            scratch.clear();
            if template.node.reaches(below) && match_template(&template.rank.pieces, token, scratch)
            {
                template.node.allowed(path, next, below, found, scratch);
            }
        }
        if let Some(child) = &self.param
            && !token.is_empty()
        {
            child.allowed(path, next, below, found, scratch);
        }
        if !path[at..].is_empty() {
            *found |= bits(&self.catch_all);
        }
    }
}

fn bits(routes: &[Option<usize>; 7]) -> u8 {
    routes
        .iter()
        .enumerate()
        .filter(|(_, r)| r.is_some())
        .fold(0, |acc, (i, _)| acc | (1 << i))
}

/// Whether `token` matches the template `pieces`, pushing its values onto
/// `values` when it does (left as they were when it does not).
///
/// Each literal between parameters is placed at its first occurrence
/// after at least one byte of value. That is also the first occurrence
/// that lets the rest match: a later start leaves the rest less room, so if
/// the rest cannot match after the first occurrence it cannot after any.
/// A literal ending the template must end the token. Linear in the token
/// length per piece.
fn match_template<'p>(pieces: &[Piece], token: &'p str, values: &mut Vec<&'p str>) -> bool {
    let mark = values.len();
    let mut at = 0;
    let mut i = 0;
    if let Some(Piece::Lit(prefix)) = pieces.first() {
        if !token.starts_with(prefix.as_str()) {
            return false;
        }
        at = prefix.len();
        i = 1;
    }
    while i < pieces.len() {
        // `pieces[i]` is a parameter: `parse_segment` never puts two
        // literals or two parameters in a row.
        let rest = &token[at..];
        let ok = match pieces.get(i + 1) {
            None => {
                values.push(rest);
                !rest.is_empty()
            }
            Some(Piece::Lit(suffix)) if i + 2 == pieces.len() => {
                match rest.strip_suffix(suffix.as_str()) {
                    Some(value) if !value.is_empty() => {
                        values.push(value);
                        true
                    }
                    _ => false,
                }
            }
            Some(Piece::Lit(next)) => {
                // At least one byte (one character) of value first.
                let skip = rest.chars().next().map_or(0, char::len_utf8);
                match rest.get(skip..).and_then(|r| r.find(next.as_str())) {
                    Some(found) if skip > 0 => {
                        let end = skip + found;
                        values.push(&rest[..end]);
                        at += end + next.len();
                        true
                    }
                    _ => false,
                }
            }
            Some(Piece::Param) => false,
        };
        if !ok {
            values.truncate(mark);
            return false;
        }
        i += 2;
    }
    true
}

/// One `/`-separated segment of a route pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Static(String),
    /// Literal text and parameters, at least one of each or two
    /// parameters; never two parameters in a row.
    Template(Vec<Part>),
    Param(String),
    CatchAll(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Lit(String),
    Param(String),
}

fn template_rank(parts: &[Part]) -> TemplateRank {
    let pieces: Vec<Piece> = parts
        .iter()
        .map(|p| match p {
            Part::Lit(text) => Piece::Lit(text.clone()),
            Part::Param(_) => Piece::Param,
        })
        .collect();
    let literal = parts
        .iter()
        .map(|p| match p {
            Part::Lit(text) => text.len(),
            Part::Param(_) => 0,
        })
        .sum();
    TemplateRank {
        literal: Reverse(literal),
        pieces,
    }
}

/// The segments of a route pattern.
fn parse(pattern: &str) -> Result<Vec<Segment>, String> {
    let tokens: Vec<&str> = pattern.split('/').collect();
    let last = tokens.len() - 1;
    tokens
        .iter()
        .enumerate()
        .map(|(i, token)| {
            let segment = parse_segment(token)?;
            if matches!(segment, Segment::CatchAll(_)) && i != last {
                return Err(format!(
                    "the catch-all parameter in `{token}` must be the last segment"
                ));
            }
            Ok(segment)
        })
        .collect()
}

fn parse_segment(token: &str) -> Result<Segment, String> {
    let mut parts: Vec<Part> = Vec::new();
    let mut literal = String::new();
    let mut catch_all = None;
    let mut chars = token.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '}' => return Err(format!("unmatched `}}` in `{token}`")),
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('{') | None => return Err(format!("unclosed `{{` in `{token}`")),
                        Some(c) => name.push(c),
                    }
                }
                if !literal.is_empty() {
                    parts.push(Part::Lit(std::mem::take(&mut literal)));
                } else if matches!(parts.last(), Some(Part::Param(_))) {
                    return Err(format!(
                        "two parameters in `{token}` must be separated by literal text"
                    ));
                }
                let name = match name.strip_prefix('*') {
                    Some(rest) => {
                        catch_all = Some(rest.to_owned());
                        rest.to_owned()
                    }
                    None => name,
                };
                if name.is_empty() {
                    return Err(format!("empty parameter name in `{token}`"));
                }
                parts.push(Part::Param(name));
            }
            c => literal.push(c),
        }
    }
    if !literal.is_empty() {
        parts.push(Part::Lit(literal));
    }
    if let Some(name) = catch_all {
        return match parts.as_slice() {
            [Part::Param(_)] => Ok(Segment::CatchAll(name)),
            _ => Err(format!(
                "the catch-all parameter in `{token}` must be the whole segment"
            )),
        };
    }
    Ok(match parts.as_slice() {
        [] => Segment::Static(String::new()),
        [Part::Lit(text)] => Segment::Static(text.clone()),
        [Part::Param(name)] => Segment::Param(name.clone()),
        _ => Segment::Template(parts),
    })
}

/// `base` as a route prefix: without its trailing `/`s; `None` when it does
/// not start with `/`, is the root, or contains a route pattern brace.
fn base_path(base: &str) -> Option<&str> {
    let trimmed = base.trim_end_matches('/');
    (base.starts_with('/') && !trimmed.is_empty() && !trimmed.contains(['{', '}']))
        .then_some(trimmed)
}

/// Strip one trailing `/`, except for the root path.
fn normalize(path: &str) -> &str {
    match path.strip_suffix('/') {
        Some(stripped) if !stripped.is_empty() => stripped,
        _ => path,
    }
}

/// Decode `%XX` escapes; invalid escapes are kept verbatim and invalid
/// UTF-8 is replaced.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3)
            && hex.iter().all(u8::is_ascii_hexdigit)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests;
