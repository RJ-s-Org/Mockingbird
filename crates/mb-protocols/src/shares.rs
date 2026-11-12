//! Types matched across the mocks of a `shares` group beyond their names.
//!
//! Mocks of one API (a REST contract beside a GraphQL schema) keep their
//! sessions together, and a record one stores is read through another when
//! the type names agree beyond case, punctuation and a plural `s`, or as the
//! world's `same_as` says (see `call::shared`). This module infers the
//! remaining matches where the evidence is strong:
//!
//! - **Name:** one type's name, its last word read singular, is the leading
//!   words of the other's, on word boundaries (`pulls` and `PullRequest`;
//!   not `project` and `projectsV2`).
//! - **Uniqueness:** of the other mock's types it extends, that one is the
//!   shortest and every other one extends it in turn (`PullRequestReview`
//!   after `PullRequest`); no other type of this mock leads it so; neither
//!   type matches any type of the other mock by name, and the world names no
//!   `same_as` for either.
//! - **Members:** the two records have at least [`MIN_COMMON_MEMBERS`]
//!   members in common besides `id`, compared in lower case without
//!   punctuation (`created_at` is `createdAt`).
//!
//! Each mock registers its [`SharedCatalog`] in the [`SharesGroups`] its
//! runtimes share, and reads its peers' from there.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use mb_mcm::world::World;
use mb_mcm::{Bundle, OperationKind};
use serde_json::Value;

/// Members two records must have in common, `id` aside, to be matched.
pub const MIN_COMMON_MEMBERS: usize = 3;

/// One mock's types as the matching reads them.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SharedCatalog {
    types: BTreeMap<String, TypeShape>,
    /// Every type's name squashed (lower case, letters and digits).
    squashed: HashSet<String>,
    /// Every type's [`name_forms`].
    forms: HashSet<String>,
}

/// What the matching reads of one type.
#[derive(Debug, Clone, PartialEq)]
struct TypeShape {
    /// The words of its name, lowercase.
    words: Vec<String>,
    /// Its record's member names, lowercase alphanumerics.
    members: BTreeSet<String>,
    /// The world names its counterparts (`same_as`), so none is inferred.
    described: bool,
    /// Its name squashed.
    squashed: String,
    /// Its [`name_forms`].
    forms: Vec<String>,
}

/// A type of another mock of the group matched to one of this mock's.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct InferredMatch {
    /// The other mock.
    pub mock: String,
    /// Its type, as it stores it.
    pub r#type: String,
}

impl SharedCatalog {
    /// The catalog of `bundle`'s resource types, under the effective
    /// description `world`.
    #[must_use]
    pub fn new(bundle: &Bundle, world: Option<&World>) -> Self {
        let defs_by_name: HashMap<String, &str> = bundle
            .defs
            .keys()
            .map(|k| (squash(k), k.as_str()))
            .collect();
        let mut types: BTreeMap<String, TypeShape> = BTreeMap::new();
        for op in &bundle.operations {
            let Some(t) = op.resource_type.as_deref().filter(|t| !t.is_empty()) else {
                continue;
            };
            let shape = types.entry(t.to_string()).or_insert_with(|| {
                let def = def_named(&defs_by_name, t);
                let spelled = if t
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                {
                    // A name kept in lower case (a GraphQL type's) has its words
                    // in the definition it names.
                    def.filter(|d| squash(d) == squash(t)).unwrap_or(t)
                } else {
                    t
                };
                let mut members = BTreeSet::new();
                if let Some(props) = def
                    .and_then(|d| bundle.defs.get(d))
                    .and_then(|d| d.get("properties"))
                    .and_then(Value::as_object)
                {
                    members.extend(props.keys().map(|k| squash(k)));
                }
                TypeShape {
                    words: words(spelled),
                    members,
                    described: world
                        .and_then(|w| w.types.get(t))
                        .is_some_and(|d| !d.same_as.is_empty()),
                    squashed: squash(t),
                    forms: name_forms(t),
                }
            });
            // A read of one record answers its members.
            if op.kind == OperationKind::Query
                && op.identity.is_some()
                && let Some(props) = op
                    .success_output()
                    .and_then(|o| o.schema.as_ref())
                    .and_then(|s| crate::call::shared::record_props(bundle, s))
            {
                shape.members.extend(props.keys().map(|k| squash(k)));
            }
        }
        Self::of(types)
    }

    /// The catalog of `types`, indexed.
    fn of(types: BTreeMap<String, TypeShape>) -> Self {
        let squashed = types.values().map(|s| s.squashed.clone()).collect();
        let forms = types.values().flat_map(|s| s.forms.clone()).collect();
        Self {
            types,
            squashed,
            forms,
        }
    }

    /// Whether `shape` agrees by name (case, punctuation and plural aside)
    /// with a type of this catalog, one way or the other.
    fn names(&self, shape: &TypeShape) -> bool {
        self.forms.contains(&shape.squashed)
            || shape.forms.iter().any(|f| self.squashed.contains(f))
    }

    /// Whether the catalog holds no type.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }
}

/// Each type of `own` matched to types of the `peers` (by mock name).
#[must_use]
pub fn inferred<'a>(
    own: &SharedCatalog,
    peers: impl IntoIterator<Item = (&'a str, &'a SharedCatalog)>,
) -> BTreeMap<String, Vec<InferredMatch>> {
    let mut out: BTreeMap<String, Vec<InferredMatch>> = BTreeMap::new();
    for (mock, peer) in peers {
        let pairs = one_way(own, peer)
            .into_iter()
            .chain(one_way(peer, own).into_iter().map(|(p, o)| (o, p)));
        for (t, other) in pairs {
            let found = out.entry(t.to_string()).or_default();
            let m = InferredMatch {
                mock: mock.to_string(),
                r#type: other.to_string(),
            };
            if !found.contains(&m) {
                found.push(m);
            }
        }
    }
    out
}

/// The types of `a` whose name `b`'s extends, matched to that type of `b`.
/// Candidates and rivals are drawn from every type, so a type left out of
/// the matching (named alike, or described) still makes the match ambiguous
/// rather than passing it to the next longer name.
fn one_way<'a>(a: &'a SharedCatalog, b: &'a SharedCatalog) -> Vec<(&'a str, &'a str)> {
    let open = |shape: &TypeShape, other: &SharedCatalog| !shape.described && !other.names(shape);
    let mut out = Vec::new();
    for (t, shape) in &a.types {
        if !open(shape, b) {
            continue;
        }
        let candidates: Vec<(&str, &TypeShape)> = b
            .types
            .iter()
            .filter(|(_, s)| leads(&shape.words, &s.words))
            .map(|(c, s)| (c.as_str(), s))
            .collect();
        let Some(&(head, head_shape)) = candidates.iter().min_by_key(|(_, s)| s.words.len()) else {
            continue;
        };
        let n = head_shape.words.len();
        // Every other candidate is the head and more (`PullRequestReview`);
        // a second of the same length, or another branch, is ambiguous.
        let chain = candidates
            .iter()
            .filter(|(c, _)| *c != head)
            .all(|(_, s)| s.words.len() > n && s.words[..n] == head_shape.words[..]);
        let rivals = a
            .types
            .iter()
            .any(|(o, s)| o != t && leads(&s.words, &head_shape.words));
        let common = shape
            .members
            .intersection(&head_shape.members)
            .filter(|m| m.as_str() != "id")
            .count();
        if chain && !rivals && open(head_shape, a) && common >= MIN_COMMON_MEMBERS {
            out.push((t.as_str(), head));
        }
    }
    out
}

/// Whether `short`, its last word read singular, is the leading words of
/// `long`, which has more.
fn leads(short: &[String], long: &[String]) -> bool {
    let Some((last, init)) = short.split_last() else {
        return false;
    };
    long.len() > short.len()
        && long[..init.len()] == *init
        && singulars(last).iter().any(|s| *s == long[init.len()])
}

/// `word` and the singulars its ending may make (`pulls` → `pull`,
/// `branches` → `branch`, `policies` → `policy`).
fn singulars(word: &str) -> Vec<String> {
    let mut out = vec![word.to_string()];
    if let Some(root) = word.strip_suffix("ies") {
        out.push(format!("{root}y"));
    }
    out.extend(word.strip_suffix("es").map(str::to_string));
    out.extend(
        word.strip_suffix('s')
            .filter(|w| !w.ends_with('s'))
            .map(str::to_string),
    );
    out
}

/// The names a type `t` agrees with beyond case, punctuation and a plural:
/// `t` squashed, with an `s`, and without an `es` or `s` ending.
pub(crate) fn name_forms(t: &str) -> Vec<String> {
    let base = squash(t);
    let mut names = vec![base.clone(), format!("{base}s")];
    names.extend(base.strip_suffix("es").map(str::to_string));
    names.extend(base.strip_suffix('s').map(str::to_string));
    names
}

/// `name` in lower case, its letters and digits only.
fn squash(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_lowercase()
}

/// The definition whose name is `t`'s, or its singular's, beyond case and
/// punctuation.
fn def_named<'a>(defs: &HashMap<String, &'a str>, t: &str) -> Option<&'a str> {
    let base = squash(t);
    std::iter::once(base.clone())
        .chain(singulars(&base).into_iter().skip(1))
        .find_map(|n| defs.get(&n).copied())
}

/// The words of a name, lowercase: split at punctuation, at a lower-case
/// letter or digit followed by a capital, at the last capital of a run
/// followed by a lower-case letter (`HTTPServer`), and between letters and
/// digits (`projectsV2` → `projects`, `v`, `2`).
fn words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_ascii_alphanumeric() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            continue;
        }
        let prev = i.checked_sub(1).and_then(|p| chars.get(p)).copied();
        let next = chars.get(i + 1).copied();
        let split = prev.is_some_and(|p| {
            p.is_ascii_alphanumeric()
                && ((c.is_ascii_uppercase() && (p.is_ascii_lowercase() || p.is_ascii_digit()))
                    || (c.is_ascii_uppercase()
                        && p.is_ascii_uppercase()
                        && next.is_some_and(|n| n.is_ascii_lowercase()))
                    || (c.is_ascii_digit() != p.is_ascii_digit()))
        });
        if split && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        current.push(c.to_ascii_lowercase());
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The catalogs of the mocks of every `shares` group, by workspace and
/// group, shared by the runtimes of one process: the server registers the
/// mocks it serves ([`SharesGroups::sync`]) and each reads its peers' at a
/// read.
#[derive(Clone, Debug, Default)]
pub struct SharesGroups(Arc<Mutex<Groups>>);

/// The registered catalogs by workspace and group, each group with the
/// count of changes at its last change.
#[derive(Debug, Default)]
struct Groups {
    changes: u64,
    by_group: HashMap<(String, String), Group>,
}

/// The mocks of one group and when it last changed.
#[derive(Debug, Default)]
struct Group {
    generation: u64,
    mocks: BTreeMap<String, Arc<SharedCatalog>>,
}

/// The other mocks of a group, by name, and the group's generation.
pub(crate) type Peers = (u64, Vec<(String, Arc<SharedCatalog>)>);

/// A mock to register: its workspace and name, its group, its catalog.
pub type Member = ((String, String), String, Arc<SharedCatalog>);

impl SharesGroups {
    /// Registers mock `mock` of `workspace` in `group` with `catalog`, out of
    /// any group it was in; with no group, only takes it out.
    pub fn join(&self, workspace: &str, mock: &str, group: Option<(&str, Arc<SharedCatalog>)>) {
        let mut groups = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next: HashMap<(String, String), BTreeMap<String, Arc<SharedCatalog>>> = groups
            .by_group
            .iter()
            .map(|(k, g)| (k.clone(), g.mocks.clone()))
            .collect();
        for ((ws, _), mocks) in &mut next {
            if ws == workspace {
                mocks.remove(mock);
            }
        }
        if let Some((group, catalog)) = group {
            next.entry((workspace.to_string(), group.to_string()))
                .or_default()
                .insert(mock.to_string(), catalog);
        }
        groups.replace(next);
    }

    /// Registers exactly `members`: a mock not among them (deleted, without
    /// a contract, or no longer served) leaves its group.
    pub fn sync(&self, members: impl IntoIterator<Item = Member>) {
        let mut next: HashMap<(String, String), BTreeMap<String, Arc<SharedCatalog>>> =
            HashMap::new();
        for ((ws, mock), group, catalog) in members {
            next.entry((ws, group)).or_default().insert(mock, catalog);
        }
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(next);
    }

    /// The other mocks of `group` in `workspace` than `mock`, and the
    /// generation of the group they were read at.
    pub(crate) fn peers(&self, workspace: &str, group: &str, mock: &str) -> Peers {
        let groups = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(g) = groups
            .by_group
            .get(&(workspace.to_string(), group.to_string()))
        else {
            return (0, Vec::new());
        };
        let peers = g
            .mocks
            .iter()
            .filter(|(name, _)| name.as_str() != mock)
            .map(|(name, c)| (name.clone(), Arc::clone(c)))
            .collect();
        (g.generation, peers)
    }
}

impl Groups {
    /// Makes `next` the registered groups, a group whose mocks or catalogs
    /// changed (or that is new) at a new generation.
    fn replace(&mut self, next: HashMap<(String, String), BTreeMap<String, Arc<SharedCatalog>>>) {
        let mut by_group = HashMap::new();
        for (key, mocks) in next {
            if mocks.is_empty() {
                continue;
            }
            let same = self.by_group.get(&key).filter(|g| {
                g.mocks.len() == mocks.len()
                    && g.mocks
                        .iter()
                        .zip(&mocks)
                        .all(|((a, x), (b, y))| a == b && Arc::ptr_eq(x, y))
            });
            let generation = match same {
                Some(g) => g.generation,
                None => {
                    self.changes += 1;
                    self.changes
                }
            };
            by_group.insert(key, Group { generation, mocks });
        }
        self.by_group = by_group;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(name: &str, members: &[&str]) -> TypeShape {
        TypeShape {
            words: words(name),
            members: members.iter().map(|m| squash(m)).collect(),
            described: false,
            squashed: String::new(),
            forms: Vec::new(),
        }
    }

    fn catalog(types: &[(&str, &str, &[&str])]) -> SharedCatalog {
        described(types, &[])
    }

    /// [`catalog`] with the types of `same_as` described.
    fn described(types: &[(&str, &str, &[&str])], same_as: &[&str]) -> SharedCatalog {
        SharedCatalog::of(
            types
                .iter()
                .map(|(t, spelled, members)| {
                    let mut s = shape(spelled, members);
                    s.squashed = squash(t);
                    s.forms = name_forms(t);
                    s.described = same_as.contains(t);
                    (t.to_string(), s)
                })
                .collect(),
        )
    }

    const MERGE: &[&str] = &["id", "number", "title", "state", "created_at"];

    #[test]
    fn words_split_at_case_punctuation_and_digits() {
        assert_eq!(words("PullRequest"), ["pull", "request"]);
        assert_eq!(words("check-runs"), ["check", "runs"]);
        assert_eq!(words("projectsV2"), ["projects", "v", "2"]);
        assert_eq!(words("HTTPServer"), ["http", "server"]);
        assert_eq!(words("pulls"), ["pulls"]);
    }

    #[test]
    fn a_plural_leads_the_longer_name_on_word_boundaries() {
        let w = |s: &str| words(s);
        assert!(leads(&w("merges"), &w("MergeRequest")));
        assert!(leads(&w("policies"), &w("PolicyRule")));
        assert!(!leads(&w("project"), &w("projectsV2")));
        assert!(!leads(&w("repos"), &w("Repository")));
        assert!(!leads(&w("merges"), &w("merges")));
    }

    #[test]
    fn the_head_of_a_chain_matches_both_ways() {
        let rest = catalog(&[("merges", "merges", MERGE), ("issues", "issues", MERGE)]);
        let gql = catalog(&[
            (
                "mergerequest",
                "MergeRequest",
                &["id", "number", "title", "state", "createdAt"],
            ),
            ("mergerequestnote", "MergeRequestNote", MERGE),
            ("issue", "Issue", MERGE),
        ]);
        let m = inferred(&rest, [("gql", &gql)]);
        assert_eq!(
            m,
            BTreeMap::from([(
                "merges".to_string(),
                vec![InferredMatch {
                    mock: "gql".into(),
                    r#type: "mergerequest".into()
                }]
            )])
        );
        let back = inferred(&gql, [("rest", &rest)]);
        assert_eq!(back["mergerequest"][0].r#type, "merges");
        assert_eq!(back.len(), 1);
    }

    #[test]
    fn ambiguity_few_members_or_a_described_type_match_nothing() {
        let rest = catalog(&[("merges", "merges", MERGE)]);
        let two = catalog(&[
            ("mergerequest", "MergeRequest", MERGE),
            ("mergecommit", "MergeCommit", MERGE),
        ]);
        assert!(inferred(&rest, [("gql", &two)]).is_empty());
        let few = catalog(&[("mergerequest", "MergeRequest", &["id", "number", "url"])]);
        assert!(inferred(&rest, [("gql", &few)]).is_empty());
        let rivals = catalog(&[("merges", "merges", MERGE), ("merge", "merge", MERGE)]);
        let one = catalog(&[("mergerequest", "MergeRequest", MERGE)]);
        assert!(inferred(&rivals, [("gql", &one)]).is_empty());
        let own = described(&[("merges", "merges", MERGE)], &["merges"]);
        assert!(inferred(&own, [("gql", &one)]).is_empty());
    }

    /// A head left out of the matching (described, or named like a type of
    /// the other mock) still holds its place: the match does not pass to the
    /// next longer name.
    #[test]
    fn a_head_left_out_is_not_skipped_for_a_longer_name() {
        let types: &[(&str, &str, &[&str])] = &[
            ("mergerequest", "MergeRequest", MERGE),
            ("mergerequestnote", "MergeRequestNote", MERGE),
        ];
        let rest = catalog(&[("merges", "merges", MERGE)]);
        let gql = described(types, &["mergerequest"]);
        assert!(inferred(&rest, [("gql", &gql)]).is_empty());
        let named = catalog(&[
            ("merges", "merges", MERGE),
            ("merge-requests", "merge-requests", MERGE),
        ]);
        assert!(inferred(&named, [("gql", &catalog(types))]).is_empty());
    }

    #[test]
    fn a_type_matched_by_name_is_not_inferred() {
        let rest = catalog(&[("merges", "merges", MERGE)]);
        let gql = catalog(&[
            ("merge", "Merge", MERGE),
            ("mergerequest", "MergeRequest", MERGE),
        ]);
        assert!(inferred(&rest, [("gql", &gql)]).is_empty());
    }

    #[test]
    fn a_mock_joins_one_group_at_a_time() {
        let groups = SharesGroups::default();
        let c = Arc::new(SharedCatalog::default());
        groups.join("ws", "a", Some(("g", Arc::clone(&c))));
        groups.join("ws", "b", Some(("g", Arc::clone(&c))));
        assert_eq!(groups.peers("ws", "g", "a").1.len(), 1);
        groups.join("ws", "b", Some(("h", Arc::clone(&c))));
        assert!(groups.peers("ws", "g", "a").1.is_empty());
        assert_eq!(groups.peers("ws", "h", "a").1.len(), 1);
        groups.join("ws", "b", None);
        assert!(groups.peers("ws", "h", "a").1.is_empty());
        assert!(groups.peers("other", "g", "a").1.is_empty());
    }

    /// `sync` registers exactly what it is given, and moves only the groups
    /// that changed to a new generation.
    #[test]
    fn sync_drops_unlisted_mocks_and_keeps_unchanged_generations() {
        let groups = SharesGroups::default();
        let c = Arc::new(SharedCatalog::default());
        let m =
            |ws: &str, name: &str, g: &str| ((ws.into(), name.into()), g.into(), Arc::clone(&c));
        groups.sync([
            m("ws", "a", "g"),
            m("ws", "b", "g"),
            m("ws", "x", "h"),
            m("ws", "y", "h"),
        ]);
        let (g1, peers) = groups.peers("ws", "g", "a");
        assert_eq!(peers.len(), 1);
        let (h1, _) = groups.peers("ws", "h", "x");
        groups.sync([m("ws", "a", "g"), m("ws", "x", "h"), m("ws", "y", "h")]);
        let (g2, peers) = groups.peers("ws", "g", "a");
        assert!(peers.is_empty());
        assert_ne!(g1, g2);
        assert_eq!(groups.peers("ws", "h", "x").0, h1);
    }
}
