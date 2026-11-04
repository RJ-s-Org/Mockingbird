//! Request routing for the REST adapter.
//!
//! A route pattern is a list of `/`-separated segments. Each segment is
//! one of, in order of precedence when several routes match a request:
//!
//! 1. static text (`/orders`);
//! 2. a template: parameters with literal text around or between them in
//!    one segment (`/files/{name}.json`, `/items/{id}:archive`, OData keys
//!    `/companies({id})` and `/pairs(a={a},b={b})`), the one with more
//!    literal text first;
//! 3. a parameter that is the whole segment (`/orders/{id}`);
//! 4. a catch-all, the last segment only (`/static/{*path}`), which takes
//!    the rest of the path, slashes included.
//!
//! The first segment where two matching routes differ decides between
//! them, so `/a/b/{x}` wins over `/a/{y}/c` for `/a/b/c`. A parameter
//! never matches an empty string nor a `/`, `{{` and `}}` stand for
//! literal braces, and two parameters of one segment must be separated by
//! literal text. Between parameters, the literal text is the first
//! occurrence that lets the rest match (`{a}-{b}` reads `1-2-3` as `1` and
//! `2-3`); a trailing literal is matched at the end of the segment.
//!
//! These are `matchit`'s rules (which this module replaced) for every route
//! set `matchit` accepted; unlike `matchit`, any number of parameters per
//! segment and any siblings below a template (`/c({id})/a`,
//! `/c({id})/b({x})`) are accepted, and only routes matching exactly the
//! same requests conflict.

use std::cmp::Reverse;
use std::collections::HashMap;

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
    by_method: HashMap<HttpMethod, Routes>,
    /// The contract routes under the base paths, consulted when no
    /// contract route matches.
    prefixed: HashMap<HttpMethod, Routes>,
    skipped: Vec<RouteError>,
}

/// Shows how many methods have routes and which operations were skipped.
impl std::fmt::Debug for RouteTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteTable")
            .field("methods", &self.by_method.len())
            .field("prefixed_methods", &self.prefixed.len())
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
        let mut by_method: HashMap<HttpMethod, Routes> = HashMap::new();
        let mut skipped = Vec::new();
        for op in &bundle.operations {
            let Binding::Http { method, path } = &op.binding else {
                continue;
            };
            let inserted =
                by_method
                    .entry(*method)
                    .or_default()
                    .insert(normalize(path), path, &op.id);
            if let Err(e) = inserted {
                skipped.push(e.for_operation(&op.id, *method, path));
            }
        }
        let mut prefixed: HashMap<HttpMethod, Routes> = HashMap::new();
        for base in base_paths.iter().filter_map(|b| base_path(b)) {
            for op in &bundle.operations {
                let Binding::Http { method, path } = &op.binding else {
                    continue;
                };
                let route = match normalize(path) {
                    "/" => base.to_owned(),
                    route => format!("{base}{route}"),
                };
                let inserted = prefixed
                    .entry(*method)
                    .or_default()
                    .insert(&route, &route, &op.id);
                if let Err(e) = inserted {
                    let error = e.for_operation(&op.id, *method, &route);
                    tracing::debug!(operation = %op.id, base_path = base, %error, "prefixed route skipped");
                }
            }
        }
        Self {
            by_method,
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
        let (route, raw) = [&self.by_method, &self.prefixed]
            .into_iter()
            .find_map(|routers| routers.get(&method)?.at(path))?;
        let params = raw
            .into_iter()
            .map(|(k, v)| (k.to_owned(), Value::String(percent_decode(v))))
            .collect();
        Some((route.op.clone(), params))
    }

    /// Methods that have an operation matching `path`, in a stable order.
    #[must_use]
    pub fn allowed(&self, path: &str) -> Vec<HttpMethod> {
        let path = normalize(path);
        ALL_METHODS
            .into_iter()
            .filter(|m| {
                [&self.by_method, &self.prefixed]
                    .into_iter()
                    .any(|routers| routers.get(m).is_some_and(|r| r.at(path).is_some()))
            })
            .collect()
    }
}

/// The routes of one method.
#[derive(Debug, Default)]
struct Routes {
    routes: Vec<Route>,
    /// Indexes into `routes` of the routes without a catch-all, by their
    /// number of segments (a request must have exactly as many).
    by_len: HashMap<usize, Vec<usize>>,
    /// Indexes into `routes` of the routes ending in a catch-all.
    catch_all: Vec<usize>,
    /// The route index of each route shape, to detect conflicts.
    shapes: HashMap<Vec<Rank>, usize>,
}

#[derive(Debug)]
struct Route {
    op: String,
    /// The path as the contract wrote it, for conflict messages.
    path: String,
    segments: Vec<Segment>,
    /// The precedence of each segment; also the route's shape.
    rank: Vec<Rank>,
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

impl Routes {
    /// Add `pattern` (normalized) for `op`; `path` is the contract's own
    /// spelling, kept for messages.
    fn insert(&mut self, pattern: &str, path: &str, op: &str) -> Result<(), InsertError> {
        let segments = parse(pattern).map_err(InsertError::Invalid)?;
        let rank: Vec<Rank> = segments.iter().map(Segment::rank).collect();
        if let Some(&i) = self.shapes.get(&rank) {
            let other = &self.routes[i];
            return Err(InsertError::Conflict {
                other: other.op.clone(),
                other_path: other.path.clone(),
            });
        }
        let index = self.routes.len();
        if matches!(segments.last(), Some(Segment::CatchAll(_))) {
            self.catch_all.push(index);
        } else {
            self.by_len.entry(segments.len()).or_default().push(index);
        }
        self.shapes.insert(rank.clone(), index);
        self.routes.push(Route {
            op: op.to_owned(),
            path: path.to_owned(),
            segments,
            rank,
        });
        Ok(())
    }

    /// The route of highest precedence matching `path` (normalized), with
    /// its raw parameters.
    fn at<'p>(&self, path: &'p str) -> Option<(&Route, Vec<(&str, &'p str)>)> {
        let tokens: Vec<(usize, &str)> = split(path).collect();
        let exact = self.by_len.get(&tokens.len()).into_iter().flatten();
        let mut best: Option<(&Route, Vec<(&str, &'p str)>)> = None;
        for &i in exact.chain(&self.catch_all) {
            let route = &self.routes[i];
            if best.as_ref().is_some_and(|(b, _)| b.rank <= route.rank) {
                continue;
            }
            if let Some(params) = route.matches(path, &tokens) {
                best = Some((route, params));
            }
        }
        best
    }
}

impl Route {
    /// The raw parameters when this route matches `path`, split into
    /// `tokens` (each with its byte offset).
    fn matches<'r, 'p>(
        &'r self,
        path: &'p str,
        tokens: &[(usize, &'p str)],
    ) -> Option<Vec<(&'r str, &'p str)>> {
        let mut params = Vec::new();
        for (i, segment) in self.segments.iter().enumerate() {
            let &(offset, token) = tokens.get(i)?;
            match segment {
                Segment::Static(text) => {
                    if token != text {
                        return None;
                    }
                }
                Segment::Param(name) => {
                    if token.is_empty() {
                        return None;
                    }
                    params.push((name.as_str(), token));
                }
                Segment::Template(parts) => {
                    if !match_parts(parts, token, &mut params) {
                        return None;
                    }
                }
                Segment::CatchAll(name) => {
                    let rest = &path[offset..];
                    if rest.is_empty() {
                        return None;
                    }
                    params.push((name.as_str(), rest));
                    return Some(params);
                }
            }
        }
        (tokens.len() == self.segments.len()).then_some(params)
    }
}

/// The `/`-separated tokens of `path` with their byte offsets; a leading
/// `/` gives an empty first token, as in the route patterns.
fn split(path: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    path.split('/').map(move |token| {
        let start = offset;
        offset += token.len() + 1;
        (start, token)
    })
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

/// A segment's precedence: lower sorts first and wins. Two segments with
/// equal ranks match the same tokens.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Rank {
    /// 0 static, 1 template, 2 parameter, 3 catch-all.
    class: u8,
    /// More literal text first.
    literal: Reverse<usize>,
    /// The literal text and parameter positions, names left out.
    shape: Vec<Piece>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Piece {
    Lit(String),
    Param,
}

impl Segment {
    fn rank(&self) -> Rank {
        let (class, shape) = match self {
            Segment::Static(text) => (0, vec![Piece::Lit(text.clone())]),
            Segment::Template(parts) => (
                1,
                parts
                    .iter()
                    .map(|p| match p {
                        Part::Lit(text) => Piece::Lit(text.clone()),
                        Part::Param(_) => Piece::Param,
                    })
                    .collect(),
            ),
            Segment::Param(_) => (2, vec![Piece::Param]),
            Segment::CatchAll(_) => (3, vec![Piece::Param]),
        };
        let literal = shape
            .iter()
            .map(|p| match p {
                Piece::Lit(text) => text.len(),
                Piece::Param => 0,
            })
            .sum();
        Rank {
            class,
            literal: Reverse(literal),
            shape,
        }
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

/// Whether `token` matches the template `parts`, pushing its parameters
/// onto `params` when it does (left as they were when it does not).
fn match_parts<'r, 'p>(
    parts: &'r [Part],
    token: &'p str,
    params: &mut Vec<(&'r str, &'p str)>,
) -> bool {
    match parts {
        [] => token.is_empty(),
        [Part::Lit(text), rest @ ..] => token
            .strip_prefix(text.as_str())
            .is_some_and(|token| match_parts(rest, token, params)),
        [Part::Param(name)] => {
            let matched = !token.is_empty();
            if matched {
                params.push((name.as_str(), token));
            }
            matched
        }
        // A trailing literal is matched at the end.
        [Part::Param(name), Part::Lit(suffix)] => match token.strip_suffix(suffix.as_str()) {
            Some(value) if !value.is_empty() => {
                params.push((name.as_str(), value));
                true
            }
            _ => false,
        },
        [Part::Param(name), Part::Lit(next), rest @ ..] => {
            // Every occurrence after a non-empty value, overlapping ones too.
            let mut from = token.chars().next().map_or(1, char::len_utf8);
            while let Some(found) = token.get(from..).and_then(|t| t.find(next.as_str())) {
                let at = from + found;
                params.push((name.as_str(), &token[..at]));
                if match_parts(rest, &token[at + next.len()..], params) {
                    return true;
                }
                params.pop();
                from = at + token[at..].chars().next().map_or(1, char::len_utf8);
            }
            false
        }
        // `parse_segment` never puts two parameters in a row.
        [Part::Param(_), Part::Param(_), ..] => false,
    }
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
mod tests {
    use super::*;

    /// A table of `routes` (method, path) whose operation ids are the
    /// paths, built through the same code as a contract's.
    fn table(routes: &[(HttpMethod, &str)]) -> RouteTable {
        let mut by_method: HashMap<HttpMethod, Routes> = HashMap::new();
        let mut skipped = Vec::new();
        for (method, path) in routes {
            if let Err(e) =
                by_method
                    .entry(*method)
                    .or_default()
                    .insert(normalize(path), path, path)
            {
                skipped.push(e.for_operation(path, *method, path));
            }
        }
        RouteTable {
            by_method,
            prefixed: HashMap::new(),
            skipped,
        }
    }

    fn gets(paths: &[&str]) -> RouteTable {
        let routes: Vec<_> = paths.iter().map(|p| (HttpMethod::Get, *p)).collect();
        let t = table(&routes);
        assert!(t.skipped().is_empty(), "{:?}", t.skipped());
        t
    }

    /// The route (its path) and parameters `GET path` finds.
    fn get(t: &RouteTable, path: &str) -> Option<(String, Vec<(String, String)>)> {
        t.find(HttpMethod::Get, path).map(|(op, params)| {
            let mut params: Vec<(String, String)> = params
                .into_iter()
                .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_owned()))
                .collect();
            params.sort();
            (op, params)
        })
    }

    fn found(t: &RouteTable, path: &str) -> Option<String> {
        get(t, path).map(|(op, _)| op)
    }

    fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut v: Vec<_> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn normalizes_trailing_slash() {
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("/orders/"), "/orders");
        assert_eq!(normalize("/orders"), "/orders");
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("o%201"), "o 1");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%+1"), "%+1");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
    }

    #[test]
    fn odata_keys_with_sibling_collections() {
        let t = gets(&[
            "/companies",
            "/companies({company_id})",
            "/companies({company_id})/customers",
            "/companies({company_id})/customers({customer_id})",
            "/companies({company_id})/salesInvoices",
            "/companies({company_id})/salesInvoices({sales_invoice_id})",
        ]);
        assert_eq!(
            get(&t, "/companies(c1)/customers(k%201)"),
            Some((
                "/companies({company_id})/customers({customer_id})".into(),
                p(&[("company_id", "c1"), ("customer_id", "k 1")])
            ))
        );
        assert_eq!(
            get(&t, "/companies(c1)/salesInvoices(s2)/"),
            Some((
                "/companies({company_id})/salesInvoices({sales_invoice_id})".into(),
                p(&[("company_id", "c1"), ("sales_invoice_id", "s2")])
            ))
        );
        assert_eq!(
            found(&t, "/companies(c1)/customers").as_deref(),
            Some("/companies({company_id})/customers")
        );
        assert_eq!(
            found(&t, "/companies(c1)").as_deref(),
            Some("/companies({company_id})")
        );
        assert_eq!(found(&t, "/companies()"), None, "a key is never empty");
        assert_eq!(found(&t, "/companies(c1"), None);
        assert_eq!(found(&t, "/companies(c1)x"), None);
        assert_eq!(found(&t, "/companies(c1)/vendors(v1)"), None);
    }

    #[test]
    fn several_parameters_in_one_segment() {
        let t = gets(&[
            "/pairs(left={a},right={b})",
            "/files/{name}.{ext}",
            "/v/{a}-{b}",
        ]);
        assert_eq!(
            get(&t, "/pairs(left=1,right=2)"),
            Some((
                "/pairs(left={a},right={b})".into(),
                p(&[("a", "1"), ("b", "2")])
            ))
        );
        assert_eq!(found(&t, "/pairs(left=1,right=)"), None);
        assert_eq!(found(&t, "/pairs(left=,right=2)"), None);
        assert_eq!(
            get(&t, "/files/a.tar.gz").map(|(_, p)| p),
            Some(p(&[("name", "a"), ("ext", "tar.gz")]))
        );
        // The first separator that lets the rest match.
        assert_eq!(
            get(&t, "/v/1-2-3").map(|(_, p)| p),
            Some(p(&[("a", "1"), ("b", "2-3")]))
        );
        assert_eq!(
            get(&t, "/v/-1-2").map(|(_, p)| p),
            Some(p(&[("a", "-1"), ("b", "2")]))
        );
        assert_eq!(found(&t, "/v/12"), None);
    }

    #[test]
    fn suffixes_and_prefixes_within_a_segment() {
        let t = gets(&[
            "/files/{name}.json",
            "/files/{name}.tar.gz",
            "/files/{name}.gz",
            "/files/{name}",
            "/files/index.json",
            "/items/{id}:archive",
            "/items/{id}:restore",
            "/items/{id}",
            "/items/{id}/notes",
            "/img/img-{id}.png",
            "/a/pre{x}",
            "/a/p{x}",
            "/a/{y}",
        ]);
        let cases = [
            ("/files/a.json", "/files/{name}.json", "a"),
            ("/files/a.b.json", "/files/{name}.json", "a.b"),
            ("/files/a.tar.gz", "/files/{name}.tar.gz", "a"),
            ("/files/a.gz", "/files/{name}.gz", "a"),
            ("/files/.json", "/files/{name}", ".json"),
            ("/files/a", "/files/{name}", "a"),
            ("/items/1:archive", "/items/{id}:archive", "1"),
            ("/items/1:restore", "/items/{id}:restore", "1"),
            ("/items/1", "/items/{id}", "1"),
            ("/items/1:other", "/items/{id}", "1:other"),
            ("/img/img-7.png", "/img/img-{id}.png", "7"),
        ];
        for (path, route, value) in cases {
            let (op, params) = get(&t, path).unwrap();
            assert_eq!(op, route, "{path}");
            assert_eq!(params[0].1, value, "{path}");
        }
        assert_eq!(found(&t, "/img/img-7.jpg"), None);
        assert_eq!(found(&t, "/img/img-.png"), None);
        assert_eq!(
            found(&t, "/files/index.json").as_deref(),
            Some("/files/index.json")
        );
        assert_eq!(
            found(&t, "/items/1/notes").as_deref(),
            Some("/items/{id}/notes")
        );
        assert_eq!(found(&t, "/a/prez").as_deref(), Some("/a/pre{x}"));
        assert_eq!(found(&t, "/a/pre").as_deref(), Some("/a/p{x}"));
        assert_eq!(found(&t, "/a/q").as_deref(), Some("/a/{y}"));
    }

    #[test]
    fn precedence_static_then_template_then_param_then_catch_all() {
        let t = gets(&[
            "/{*rest}",
            "/a/{x}",
            "/a/b",
            "/a/{x}.json",
            "/s/{*path}",
            "/s/{x}/meta",
        ]);
        assert_eq!(found(&t, "/a/b").as_deref(), Some("/a/b"));
        assert_eq!(found(&t, "/a/b.json").as_deref(), Some("/a/{x}.json"));
        assert_eq!(found(&t, "/a/c").as_deref(), Some("/a/{x}"));
        assert_eq!(
            get(&t, "/a/c/d"),
            Some(("/{*rest}".into(), p(&[("rest", "a/c/d")])))
        );
        assert_eq!(
            found(&t, "/").as_deref(),
            None,
            "a catch-all is never empty"
        );
        assert_eq!(found(&t, "/s/1/meta").as_deref(), Some("/s/{x}/meta"));
        assert_eq!(
            get(&t, "/s/1/raw"),
            Some(("/s/{*path}".into(), p(&[("path", "1/raw")])))
        );
    }

    #[test]
    fn the_first_differing_segment_decides_and_matching_backtracks() {
        let t = gets(&["/a/b", "/a/{x}", "/a/b/c", "/a/{x}/d", "/a/b/{y}/e"]);
        assert_eq!(found(&t, "/a/b/c").as_deref(), Some("/a/b/c"));
        assert_eq!(found(&t, "/a/b/d").as_deref(), Some("/a/{x}/d"));
        assert_eq!(found(&t, "/a/z/d").as_deref(), Some("/a/{x}/d"));
        assert_eq!(found(&t, "/a/b/d/e").as_deref(), Some("/a/b/{y}/e"));
        let t = gets(&["/u/{id}", "/u/{user_id}/posts", "/u/{id}/{tab}"]);
        assert_eq!(
            get(&t, "/u/1/posts"),
            Some(("/u/{user_id}/posts".into(), p(&[("user_id", "1")])))
        );
        assert_eq!(found(&t, "/u/1/likes").as_deref(), Some("/u/{id}/{tab}"));
    }

    #[test]
    fn escaped_braces_are_literal() {
        let t = gets(&["/x/{{lit}}", "/x/{p}"]);
        assert_eq!(found(&t, "/x/{lit}").as_deref(), Some("/x/{{lit}}"));
        assert_eq!(found(&t, "/x/%7Blit%7D").as_deref(), Some("/x/{p}"));
    }

    #[test]
    fn root_and_trailing_slashes() {
        let t = gets(&["/", "/{x}"]);
        assert_eq!(found(&t, "/").as_deref(), Some("/"));
        assert_eq!(found(&t, "/q/").as_deref(), Some("/{x}"));
        assert_eq!(found(&t, "/q//"), None, "an empty segment is no parameter");
    }

    #[test]
    fn identical_shapes_conflict_and_the_rest_still_route() {
        let t = table(&[
            (HttpMethod::Get, "/a/{x}"),
            (HttpMethod::Get, "/a/{y}"),
            (HttpMethod::Get, "/b"),
            (HttpMethod::Get, "/b/"),
            (HttpMethod::Get, "/c({k})/d"),
            (HttpMethod::Get, "/c({key})/d"),
            (HttpMethod::Delete, "/a/{y}"),
            (HttpMethod::Get, "/ok"),
        ]);
        let skipped: Vec<&str> = t.skipped().iter().map(RouteError::operation).collect();
        assert_eq!(skipped, ["/a/{y}", "/b/", "/c({key})/d"]);
        assert_eq!(
            t.skipped()[0].to_string(),
            "operation /a/{y}: cannot route GET /a/{y}: it matches the same requests as \
             operation /a/{x} (GET /a/{x}), which takes them"
        );
        assert_eq!(found(&t, "/a/1").as_deref(), Some("/a/{x}"));
        assert_eq!(found(&t, "/ok").as_deref(), Some("/ok"));
        assert_eq!(
            t.find(HttpMethod::Delete, "/a/1")
                .map(|(op, _)| op)
                .as_deref(),
            Some("/a/{y}")
        );
    }

    #[test]
    fn invalid_patterns_are_skipped_with_a_reason() {
        let cases = [
            ("/a/{x", "unclosed `{` in `{x`"),
            ("/a/x}", "unmatched `}` in `x}`"),
            ("/a/{}", "empty parameter name in `{}`"),
            ("/a/{*}", "empty parameter name in `{*}`"),
            (
                "/a/{x}{y}",
                "two parameters in `{x}{y}` must be separated by literal text",
            ),
            (
                "/a/{*rest}/b",
                "the catch-all parameter in `{*rest}` must be the last segment",
            ),
            (
                "/a/x{*rest}",
                "the catch-all parameter in `x{*rest}` must be the whole segment",
            ),
        ];
        for (path, reason) in cases {
            let t = table(&[(HttpMethod::Get, path), (HttpMethod::Get, "/ok")]);
            assert_eq!(
                t.skipped(),
                [RouteError::Invalid {
                    op: path.into(),
                    method: HttpMethod::Get,
                    path: path.into(),
                    reason: reason.into(),
                }],
                "{path}"
            );
            assert!(found(&t, "/ok").is_some());
        }
    }
}
