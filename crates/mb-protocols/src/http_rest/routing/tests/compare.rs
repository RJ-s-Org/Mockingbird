//! Seeded random route sets, each request checked against an independent
//! brute-force oracle of the stated precedence (every route matched on
//! its own, the lowest rank vector wins) and against `matchit`.
//!
//! - Always: the router answers as the oracle does, and skips exactly
//!   the routes whose shape an earlier route has.
//! - Where `matchit` accepted every route: nothing is skipped, and where
//!   `matchit` matched, the router picks the same operation and values, or
//!   an operation the stated precedence ranks strictly higher (`matchit`
//!   does not always backtrack out of a template or prefixed parameter).
//!   Where `matchit` answered 404, the router may match.

use std::cmp::Reverse;

use super::{get, table};
use crate::http_rest::routing::{HttpMethod, RouteTable, Segment, normalize, parse};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(self.0 >> 33).unwrap_or_default()
    }

    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.next() % xs.len()]
    }
}

/// Class, then more literal text first, then the pieces: a literal
/// (`false`, its text) before a parameter (`true`).
type Rank = (u8, Reverse<usize>, Vec<(bool, String)>);

/// The oracle's own precedence of a segment (not the router's code).
fn rank(segment: &Segment) -> Rank {
    use crate::http_rest::routing::Part;
    match segment {
        Segment::Static(text) => (0, Reverse(text.len()), vec![(false, text.clone())]),
        Segment::Template(parts) => {
            let pieces: Vec<(bool, String)> = parts
                .iter()
                .map(|p| match p {
                    Part::Lit(t) => (false, t.clone()),
                    Part::Param(_) => (true, String::new()),
                })
                .collect();
            let literal = pieces.iter().map(|(_, t)| t.len()).sum();
            (1, Reverse(literal), pieces)
        }
        Segment::Param(_) => (2, Reverse(0), vec![(true, String::new())]),
        Segment::CatchAll(_) => (3, Reverse(0), vec![(true, String::new())]),
    }
}

/// Every way `parts` can split `token`, in the stated order (each literal
/// between parameters at its earliest workable occurrence, a final literal
/// at the end); the first is the stated answer. Exponential: short tokens
/// only.
fn split_template(
    parts: &[crate::http_rest::routing::Part],
    token: &str,
    values: &mut Vec<String>,
) -> bool {
    use crate::http_rest::routing::Part;
    match parts {
        [] => token.is_empty(),
        [Part::Lit(t), rest @ ..] => token
            .strip_prefix(t.as_str())
            .is_some_and(|token| split_template(rest, token, values)),
        [Part::Param(_)] => {
            values.push(token.to_owned());
            !token.is_empty() || {
                values.pop();
                false
            }
        }
        [Part::Param(_), Part::Lit(t)] => match token.strip_suffix(t.as_str()) {
            Some(v) if !v.is_empty() => {
                values.push(v.to_owned());
                true
            }
            _ => false,
        },
        [Part::Param(_), Part::Lit(t), rest @ ..] => {
            for end in (1..=token.len()).filter(|&i| token.is_char_boundary(i)) {
                if token[end..].starts_with(t.as_str()) {
                    values.push(token[..end].to_owned());
                    if split_template(rest, &token[end + t.len()..], values) {
                        return true;
                    }
                    values.pop();
                }
            }
            false
        }
        [Part::Param(_), Part::Param(_), ..] => false,
    }
}

/// The values of `segments` matching `path`, by the oracle.
fn oracle_match(segments: &[Segment], path: &str) -> Option<Vec<String>> {
    let tokens: Vec<&str> = path.split('/').collect();
    let mut values = Vec::new();
    for (i, segment) in segments.iter().enumerate() {
        let token = tokens.get(i)?;
        match segment {
            Segment::Static(t) if t == token => {}
            Segment::Static(_) => return None,
            Segment::Param(_) if !token.is_empty() => values.push((*token).to_owned()),
            Segment::Param(_) => return None,
            Segment::Template(parts) => {
                if !split_template(parts, token, &mut values) {
                    return None;
                }
            }
            Segment::CatchAll(_) => {
                let rest = tokens[i..].join("/");
                if rest.is_empty() {
                    return None;
                }
                values.push(rest);
                return Some(values);
            }
        }
    }
    (tokens.len() == segments.len()).then_some(values)
}

struct Oracle {
    /// The routes kept: (path, segments, rank vector).
    kept: Vec<(String, Vec<Segment>, Vec<Rank>)>,
    skipped: Vec<String>,
}

impl Oracle {
    fn new(routes: &[String]) -> Self {
        let mut kept: Vec<(String, Vec<Segment>, Vec<Rank>)> = Vec::new();
        let mut skipped = Vec::new();
        for route in routes {
            let Ok(segments) = parse(normalize(route)) else {
                skipped.push(route.clone());
                continue;
            };
            let ranks: Vec<Rank> = segments.iter().map(rank).collect();
            if kept.iter().any(|(_, _, r)| *r == ranks) {
                skipped.push(route.clone());
            } else {
                kept.push((route.clone(), segments, ranks));
            }
        }
        Self { kept, skipped }
    }

    /// The winning route (its index in `kept`) and its values.
    fn find(&self, path: &str) -> Option<(usize, Vec<String>)> {
        let path = normalize(path);
        self.kept
            .iter()
            .enumerate()
            .filter_map(|(i, (_, segments, _))| Some((i, oracle_match(segments, path)?)))
            .min_by(|(a, _), (b, _)| self.kept[*a].2.cmp(&self.kept[*b].2))
    }

    fn index(&self, route: &str) -> Option<usize> {
        self.kept.iter().position(|(r, _, _)| r == route)
    }
}

fn matchit_router(routes: &[String]) -> Option<matchit::Router<String>> {
    let mut router = matchit::Router::new();
    for route in routes {
        router.insert(normalize(route), route.clone()).ok()?;
    }
    Some(router)
}

/// `get`'s parameter values in the order the route names them.
fn ordered_values(table: &RouteTable, path: &str) -> Option<(String, Vec<String>)> {
    let (op, params) = table.find(HttpMethod::Get, path)?;
    let segments = parse(normalize(&op)).ok()?;
    let names = segments.iter().flat_map(|s| {
        use crate::http_rest::routing::Part;
        match s {
            Segment::Static(_) => vec![],
            Segment::Param(n) | Segment::CatchAll(n) => vec![n.clone()],
            Segment::Template(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    Part::Param(n) => Some(n.clone()),
                    Part::Lit(_) => None,
                })
                .collect(),
        }
    });
    let values = names
        .map(|n| params[&n].as_str().unwrap_or_default().to_owned())
        .collect();
    Some((op, values))
}

#[derive(Default, Debug)]
struct Tally {
    sets: usize,
    accepted: usize,
    probes: usize,
    same_as_matchit: usize,
    better_than_matchit: usize,
    matched_matchit_404: usize,
}

fn fuzz(seed: u64, segments: &[&str], tokens: &[&str], rounds: usize, tally: &mut Tally) {
    let mut rng = Lcg(seed);
    for _ in 0..rounds {
        let n = 2 + rng.next() % 6;
        let mut routes: Vec<String> = Vec::new();
        for _ in 0..n {
            let depth = 1 + rng.next() % 3;
            let mut route = String::new();
            for d in 0..depth {
                let mut s = rng.pick(segments);
                if s.contains('*') && d + 1 != depth {
                    s = "{x}";
                }
                route.push('/');
                route.push_str(
                    &s.replace("{x}", &format!("{{x{d}}}"))
                        .replace("{y}", &format!("{{y{d}}}"))
                        .replace("{*r}", &format!("{{*r{d}}}")),
                );
            }
            if !routes.contains(&route) {
                routes.push(route);
            }
        }
        tally.sets += 1;
        let pairs: Vec<(HttpMethod, &str)> = routes
            .iter()
            .map(|r| (HttpMethod::Get, r.as_str()))
            .collect();
        let ours = table(&pairs);
        let oracle = Oracle::new(&routes);
        let skipped: Vec<&str> = ours.skipped().iter().map(|e| e.operation()).collect();
        assert_eq!(skipped, oracle.skipped, "{routes:?}");
        let theirs = matchit_router(&routes);
        if theirs.is_some() {
            tally.accepted += 1;
            assert!(skipped.is_empty(), "matchit took {routes:?}");
        }
        // Fewer probes where only the oracle checks the answers.
        let probes = if theirs.is_some() { 80 } else { 20 };
        for _ in 0..probes {
            let depth = 1 + rng.next() % 4;
            let mut path = String::new();
            for _ in 0..depth {
                path.push('/');
                path.push_str(rng.pick(tokens));
            }
            if rng.next().is_multiple_of(5) {
                path.push('/');
            }
            tally.probes += 1;
            let got = ordered_values(&ours, &path);
            let want = oracle
                .find(&path)
                .map(|(i, values)| (oracle.kept[i].0.clone(), values));
            assert_eq!(got, want, "{routes:?} GET {path}");
            assert_eq!(
                !ours.allowed(&path).is_empty(),
                got.is_some(),
                "{routes:?} allowed {path}"
            );
            let Some(theirs) = &theirs else { continue };
            let Ok(m) = theirs.at(normalize(&path)) else {
                tally.matched_matchit_404 += usize::from(got.is_some());
                continue;
            };
            let mine = get(&ours, &path);
            let mut their_params: Vec<(String, String)> = m
                .params
                .iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
            their_params.sort();
            if mine.as_ref() == Some(&(m.value.clone(), their_params)) {
                tally.same_as_matchit += 1;
                continue;
            }
            // Never worse than matchit: its route matches too and ranks lower.
            let (Some((mine, _)), Some(their_index)) = (want, oracle.index(m.value)) else {
                panic!("{routes:?} GET {path}: matchit {} and we do not", m.value);
            };
            let mine = oracle.index(&mine).unwrap_or_default();
            assert!(
                oracle_match(&oracle.kept[their_index].1, normalize(&path)).is_some()
                    && oracle.kept[mine].2 < oracle.kept[their_index].2,
                "{routes:?} GET {path}: matchit picks {}, we pick {}",
                m.value,
                oracle.kept[mine].0
            );
            tally.better_than_matchit += 1;
        }
    }
}

#[test]
fn seeded_route_sets_route_by_the_stated_precedence_and_never_worse_than_matchit() {
    let mut tally = Tally::default();
    let segments = [
        "a", "b", "ab", "{x}", "{y}", "a{x}", "ab{x}", "{x}a", "{x}ba", "{x}.a", "a{x}b", "{*r}",
        "",
    ];
    let tokens = [
        "a", "b", "ab", "aa", "ba", "aba", "abba", "x.a", "xa", "xba", "axb", "abxb", "q", "",
        "a.a", "aab", "abab", "a.ba", "ba.a",
    ];
    fuzz(42, &segments, &tokens, 30_000, &mut tally);
    let segments = ["a", "{x}", "{y}", "{*r}", "b", "a{x}", "{x}a"];
    let tokens = ["a", "b", "aa", "q", "", "ab", "ba"];
    fuzz(7, &segments, &tokens, 30_000, &mut tally);
    // Several parameters per segment, which matchit refuses: oracle only.
    let segments = [
        "a",
        "{x}",
        "{x}.{y}",
        "{x}-{y}a",
        "a{x}-{y}",
        "({x})",
        "({x},{y})",
        "{x}.a",
        "{*r}",
    ];
    let tokens = [
        "a", "a.b", "a.b.c", "a-ba", "a-b-ba", "aa-b", "(a)", "(a,b)", "(a,b,c)", "x.a", "q", "",
    ];
    fuzz(3, &segments, &tokens, 5_000, &mut tally);
    // The run is meaningful: most sets matchit took, and most answers agree.
    assert!(tally.accepted > 20_000, "{tally:?}");
    assert!(tally.same_as_matchit > 400_000, "{tally:?}");
}

/// More templates at one node than it scans one by one, so they are
/// looked up by leading literal: shared, nested and empty prefixes.
#[test]
fn many_templates_at_one_node_route_by_the_oracle() {
    let mut rng = Lcg(11);
    let literals = [
        "a", "ab", "abc", "b", "(", "e1(", "e10(", "e2(", ".", ")", "-",
    ];
    let tokens = [
        "a", "ab", "abc", "abcd", "b", "ba", "(1)", "e1(1)", "e10(1)", "e2(1)", "e1(1", "a.b",
        "ab-c", "abc)", "a-b-c", "e10(1)x", "(a.b)", "q",
    ];
    for _ in 0..300 {
        let mut routes: Vec<String> = Vec::new();
        for _ in 0..(12 + rng.next() % 30) {
            let mut segment = String::new();
            let pieces = 1 + rng.next() % 3;
            if rng.next().is_multiple_of(2) {
                segment.push_str(rng.pick(&literals));
            }
            for p in 0..pieces {
                segment.push_str(&format!("{{v{p}}}"));
                segment.push_str(rng.pick(&literals));
            }
            let tail = ["", "/x", "/{w}"][rng.next() % 3];
            let route = format!("/t/{segment}{tail}");
            if !routes.contains(&route) {
                routes.push(route);
            }
        }
        if rng.next().is_multiple_of(3) {
            routes.push("/t/{z}".into());
        }
        let pairs: Vec<(HttpMethod, &str)> = routes
            .iter()
            .map(|r| (HttpMethod::Get, r.as_str()))
            .collect();
        let ours = table(&pairs);
        let oracle = Oracle::new(&routes);
        let skipped: Vec<&str> = ours.skipped().iter().map(|e| e.operation()).collect();
        assert_eq!(skipped, oracle.skipped, "{routes:?}");
        for token in tokens {
            for tail in ["", "/x", "/y"] {
                let path = format!("/t/{token}{tail}");
                let want = oracle
                    .find(&path)
                    .map(|(i, values)| (oracle.kept[i].0.clone(), values));
                assert_eq!(ordered_values(&ours, &path), want, "{routes:?} GET {path}");
                assert_eq!(!ours.allowed(&path).is_empty(), want.is_some(), "{path}");
            }
        }
    }
}
