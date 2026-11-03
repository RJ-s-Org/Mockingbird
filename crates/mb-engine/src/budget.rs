//! The output budget: the part of a response the LLM writes, cut to a
//! token budget.
//!
//! Output tokens set an answer's latency. A TMDB detail page with its
//! appended sub-resources is about 5k completion tokens (17–37 s); a GitHub
//! `search/commits` page repeats a full repository and two users per item and
//! timed out after 60 s. When the estimated answer of a response schema is
//! over the budget, the schema the LLM writes to (prompt and structured
//! output) is shaped in steps until the estimate fits:
//!
//! 1. optional object and array members the request does not name are left
//!    out (`append_to_response=videos,credits` keeps `videos` and `credits`,
//!    and `type=track` keeps `tracks`: a word names a member sharing its stem);
//! 2. arrays get a `maxItems`: top-level lists 5, 3, 2, then 1 item, nested
//!    arrays 3, 2, then 1;
//! 3. required object and array members below the top level are left out,
//!    heaviest first, and filled after generation by the faker from the full
//!    schema (see [`Shaped::fills`]); an array outside any array goes last, as
//!    the rest of its items' members can go instead (a search's `tracks` is
//!    nothing without its `items`).
//!
//! A request's page size ([`requested_limit`]) holds for the page's item arrays
//! (arrays of objects, the heaviest in each object), and the estimate counts it: those arrays are not capped below
//! it unless the steps above, with them at that size, cannot fit. Then all the steps
//! are tried again with those arrays at the next lower cap, so they go no lower than
//! they must, and last as if there were no page size.
//!
//! A GraphQL selection names every field it reads, so a narrowed schema
//! only gets step 2. Validation always uses the full schema.

use std::collections::{HashMap, HashSet};

pub use mb_mcm::stem;
use serde_json::{Map, Value};

/// Completion tokens an answer is shaped to by default.
pub const DEFAULT_OUTPUT_BUDGET: u32 = 1500;

/// Characters per output token of compact JSON.
const CHARS_PER_TOKEN: f64 = 3.5;

/// Items an array is estimated to hold without a smaller `maxItems` (the
/// prompt asks for 3 to 5).
const DEFAULT_ITEMS: f64 = 4.0;

/// `maxItems` steps for (top-level, nested) arrays.
const CAPS: [(u64, u64); 4] = [(5, 3), (3, 2), (2, 1), (1, 1)];

/// A member smaller than this (tokens per instance) is never left out.
const MIN_DROP_TOKENS: f64 = 12.0;

/// `$ref` hops followed by the estimate.
const MAX_DEPTH: usize = 24;

/// Schema nodes the estimate visits before shaping gives up.
const MAX_VISITS: usize = 200_000;

/// A response schema shaped to the output budget.
#[derive(Clone, Debug)]
pub struct Shaped {
    /// The schema (with `$defs`) the LLM writes to.
    pub schema: Value,
    /// Required members left out: (location of the object schema, as a JSON
    /// pointer into the full schema where a `$ref` restarts at
    /// `/$defs/<name>`; property).
    dropped: HashSet<(String, String)>,
}

impl Shaped {
    /// Whether a missing `property` of the object schema at `location` is
    /// one the LLM was not asked for, to be filled locally.
    pub fn fills(&self, location: &str, property: &str) -> bool {
        self.dropped
            .contains(&(location.to_string(), property.to_string()))
    }

    /// Whether any required member was left out of the schema.
    #[must_use]
    pub fn fills_any(&self) -> bool {
        !self.dropped.is_empty()
    }
}

/// `doc` (a schema with its `$defs`) shaped to `budget` completion tokens,
/// or `None` when its estimated answer already fits (or is too large a
/// schema graph to estimate). `requested` are the words of the request
/// (query names and values, body keys); `selected` marks a schema already
/// narrowed to a selection; `limit` is the page size the request asks for (see
/// [`requested_limit`]).
pub fn shape(
    doc: &Value,
    requested: &HashSet<String>,
    selected: bool,
    budget: u32,
    limit: Option<PageLimit>,
) -> Option<Shaped> {
    let budget = f64::from(budget);
    if budget <= 0.0 {
        return None;
    }
    let pages = page_arrays(doc, limit.and_then(|l| l.maximum))?;
    let size = limit.map(|l| l.items);
    let before = Stats::of(doc, Held::at(size, &pages))?.tokens;
    if before <= budget {
        return None;
    }
    // Each lower floor starts over, so what it did not need stays in the schema.
    let lower = CAPS
        .iter()
        .map(|(top, _)| *top)
        .filter(|cap| size.is_some_and(|l| *cap < l))
        .map(Some);
    let last = size.map(|_| None);
    let mut planned = None;
    for floor in std::iter::once(size).chain(lower).chain(last) {
        let plan = plan(doc, requested, selected, budget, Held::at(floor, &pages))?;
        let fits = plan.tokens <= budget;
        planned = Some(plan);
        if fits {
            break;
        }
    }
    let Plan {
        schema,
        dropped,
        tokens,
    } = planned?;
    tracing::debug!(
        estimated = before.round(),
        shaped = tokens.round(),
        filled = dropped.len(),
        "shaped the answer to the output budget"
    );
    Some(Shaped { schema, dropped })
}

/// One attempt at shaping.
struct Plan {
    schema: Value,
    /// See [`Shaped::fills`].
    dropped: HashSet<(String, String)>,
    tokens: f64,
}

/// The steps of shaping, with the page's arrays held as `held` says.
fn plan(
    doc: &Value,
    requested: &HashSet<String>,
    selected: bool,
    budget: f64,
    held: Held<'_>,
) -> Option<Plan> {
    let mut doc = doc.clone();
    let mut stats = Stats::of(&doc, held)?;
    let floor = held.floor;
    if let Some(floor) = floor {
        for (location, _) in stats.arrays.iter().filter(|(_, array)| array.page) {
            cap(&mut doc, location, floor);
        }
        stats = Stats::of(&doc, held)?;
    }
    if !selected {
        let optional = |m: &Member| !m.required && !requested.contains(&m.property);
        drop_until(&mut doc, &mut stats, optional, budget, held, None)?;
    }
    for (top, nested) in CAPS {
        if stats.tokens <= budget {
            break;
        }
        for (location, array) in &stats.arrays {
            if !(array.page && floor.is_some()) {
                cap(&mut doc, location, if array.top { top } else { nested });
            }
        }
        stats = Stats::of(&doc, held)?;
    }
    let mut dropped = HashSet::new();
    if !selected {
        for pages in [false, true] {
            // With a floor, pages stay: a lower floor is tried before they go.
            if stats.tokens <= budget || (pages && floor.is_some()) {
                break;
            }
            let required = |m: &Member| m.required && m.min_depth >= 2 && (pages || !m.page);
            drop_until(
                &mut doc,
                &mut stats,
                required,
                budget,
                held,
                Some(&mut dropped),
            )?;
        }
    }
    Some(Plan {
        schema: doc,
        dropped,
        tokens: stats.tokens,
    })
}

/// Leaves out the members `keep` accepts (heaviest first) until the estimate fits;
/// `dropped` records them when given. A member's tokens count those of the members
/// within it, which may go too, so what one pass saves is less than it counts: it
/// re-estimates and goes on while any are left.
fn drop_until(
    doc: &mut Value,
    stats: &mut Stats,
    keep: impl Fn(&Member) -> bool,
    budget: f64,
    held: Held<'_>,
    mut dropped: Option<&mut HashSet<(String, String)>>,
) -> Option<()> {
    while stats.tokens > budget {
        let candidates = stats.candidates(&keep);
        if candidates.is_empty() {
            break;
        }
        drop_pass(doc, stats, candidates, budget, held, dropped.as_deref_mut())?;
    }
    Some(())
}

/// One pass of [`drop_until`] over `candidates`, then re-estimates.
fn drop_pass(
    doc: &mut Value,
    stats: &mut Stats,
    candidates: Vec<(String, String, f64)>,
    budget: f64,
    held: Held<'_>,
    mut dropped: Option<&mut HashSet<(String, String)>>,
) -> Option<()> {
    let mut saved = 0.0;
    for (location, property, tokens) in candidates {
        if stats.tokens - saved <= budget {
            break;
        }
        if remove_property(doc, &location, &property) {
            saved += tokens;
            if let Some(d) = dropped.as_deref_mut() {
                d.insert((location, property));
            }
        }
    }
    *stats = Stats::of(doc, held)?;
    Some(())
}

fn remove_property(doc: &mut Value, location: &str, property: &str) -> bool {
    let Some(Value::Object(node)) = doc.pointer_mut(location) else {
        return false;
    };
    let removed = node
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|p| p.remove(property))
        .is_some();
    if let Some(Value::Array(required)) = node.get_mut("required") {
        required.retain(|r| r.as_str() != Some(property));
    }
    removed
}

/// Gives the array schema at `location` a `maxItems` of at most `n` (and at
/// least its `minItems`).
fn cap(doc: &mut Value, location: &str, n: u64) {
    let Some(Value::Object(node)) = doc.pointer_mut(location) else {
        return;
    };
    let min = node.get("minItems").and_then(Value::as_u64).unwrap_or(0);
    let current = node
        .get("maxItems")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    node.insert("maxItems".into(), Value::from(n.max(min).min(current)));
}

/// One object member, over every place the estimated answer holds it.
#[derive(Debug, Default)]
struct Member {
    property: String,
    required: bool,
    /// Object nesting of the shallowest occurrence: 1 for the root object's
    /// members.
    min_depth: usize,
    /// Tokens over all occurrences.
    tokens: f64,
    /// Tokens of one occurrence.
    each: f64,
    /// Object or array valued.
    structured: bool,
    /// An array, outside any array in some occurrence.
    page: bool,
}

/// How an array schema occurs in the estimated answer.
#[derive(Clone, Copy, Debug, Default)]
struct ArrayUse {
    /// Some occurrence is a top-level list (the root, or a member of the root
    /// object, outside any array).
    top: bool,
    /// Some occurrence is outside any array, and it is a page's items.
    page: bool,
}

/// What an estimate holds a page's arrays to.
#[derive(Clone, Copy)]
struct Held<'a> {
    /// How many items they hold, when known.
    floor: Option<u64>,
    /// Which arrays outside any array they are; all of them when not given.
    pages: Option<&'a HashSet<String>>,
}

impl<'a> Held<'a> {
    fn at(floor: Option<u64>, pages: &'a HashSet<String>) -> Self {
        Self {
            floor,
            pages: Some(pages),
        }
    }

    /// Whether the array at `location`, outside any array, is a page's items.
    fn is_page(&self, location: &str) -> bool {
        self.pages.is_none_or(|pages| pages.contains(location))
    }
}

/// The estimate of a schema's answer and where its tokens go.
struct Stats {
    tokens: f64,
    members: HashMap<(String, String), Member>,
    /// Array schema location → how it occurs.
    arrays: HashMap<String, ArrayUse>,
}

impl Stats {
    fn of(doc: &Value, held: Held<'_>) -> Option<Self> {
        let mut walk = Walk {
            doc,
            held,
            memo: HashMap::new(),
            stack: Vec::new(),
            visits: 0,
            stats: Stats {
                tokens: 0.0,
                members: HashMap::new(),
                arrays: HashMap::new(),
            },
        };
        let chars = walk.visit(doc, String::new(), 1.0, 0, false);
        if walk.visits > MAX_VISITS {
            return None;
        }
        walk.stats.tokens = chars / CHARS_PER_TOKEN;
        Some(walk.stats)
    }

    /// Members `keep` accepts that are worth leaving out, heaviest first:
    /// (location, property, tokens).
    fn candidates(&self, keep: &impl Fn(&Member) -> bool) -> Vec<(String, String, f64)> {
        let mut out: Vec<(String, String, f64)> = self
            .members
            .iter()
            .filter(|(_, m)| m.structured && m.each >= MIN_DROP_TOKENS && keep(m))
            .map(|((loc, prop), m)| (loc.clone(), prop.clone(), m.tokens))
            .collect();
        out.sort_by(|a, b| {
            b.2.total_cmp(&a.2)
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        out
    }
}

struct Walk<'a> {
    doc: &'a Value,
    held: Held<'a>,
    /// Characters of one instance of the definition at a location.
    memo: HashMap<String, f64>,
    stack: Vec<String>,
    visits: usize,
    stats: Stats,
}

impl Walk<'_> {
    /// Characters of one instance of `node` (at `location`), recording each
    /// member's and array's share at `mult` occurrences. `depth` is the
    /// object nesting; `in_array` whether an array encloses it.
    fn visit(
        &mut self,
        node: &Value,
        location: String,
        mult: f64,
        depth: usize,
        in_array: bool,
    ) -> f64 {
        self.visits += 1;
        if self.visits > MAX_VISITS {
            return 0.0;
        }
        let Value::Object(obj) = node else {
            return 4.0;
        };
        if let Some(target) = ref_target(obj) {
            if self.stack.contains(&target) || self.stack.len() >= MAX_DEPTH {
                return 16.0;
            }
            let Some(def) = self.doc.pointer(&target) else {
                return 16.0;
            };
            self.stack.push(target.clone());
            let chars = self.visit(def, target, mult, depth, in_array);
            self.stack.pop();
            return chars;
        }
        if let Some(Value::Array(members)) = obj.get("allOf") {
            return members
                .iter()
                .enumerate()
                .map(|(i, m)| self.visit(m, format!("{location}/allOf/{i}"), mult, depth, in_array))
                .sum();
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(Value::Array(branches)) = obj.get(key) {
                // The model writes the cheapest non-null branch.
                let best = branches
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.get("type").and_then(Value::as_str) != Some("null"))
                    .map(|(i, b)| (i, self.cost(b, &format!("{location}/{key}/{i}"))))
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                return match best {
                    Some((i, _)) => self.visit(
                        &branches[i],
                        format!("{location}/{key}/{i}"),
                        mult,
                        depth,
                        in_array,
                    ),
                    None => 4.0,
                };
            }
        }
        match schema_type(obj) {
            Some("object") => self.object(obj, &location, mult, depth, in_array),
            Some("array") => {
                let top = !in_array && depth <= 1;
                let page = !in_array && self.held.is_page(&location);
                let entry = self.stats.arrays.entry(location.clone()).or_default();
                entry.top |= top;
                entry.page |= page;
                let n = items(obj, self.held.floor.filter(|_| page));
                let item = obj.get("items").cloned().unwrap_or(Value::Bool(true));
                2.0 + n
                    * (1.0 + self.visit(&item, format!("{location}/items"), mult * n, depth, true))
            }
            Some(other) => scalar(obj, other, None),
            None => 12.0,
        }
    }

    fn object(
        &mut self,
        obj: &Map<String, Value>,
        location: &str,
        mult: f64,
        depth: usize,
        in_array: bool,
    ) -> f64 {
        let Some(Value::Object(props)) = obj.get("properties") else {
            return 30.0;
        };
        let required: HashSet<&str> = obj
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut chars = 2.0;
        for (name, schema) in props {
            let child_location = format!("{location}/properties/{}", escape(name));
            let value = match leaf_type(schema) {
                Some(t) => scalar(schema.as_object().unwrap_or(&Map::new()), t, Some(name)),
                None => self.visit(schema, child_location.clone(), mult, depth + 1, in_array),
            };
            let each = (name.len() as f64 + 4.0 + value) / CHARS_PER_TOKEN;
            let page = !in_array
                && self
                    .array_location(schema, &child_location)
                    .is_some_and(|at| self.held.is_page(&at));
            let member = self
                .stats
                .members
                .entry((location.to_string(), name.clone()))
                .or_insert_with(|| Member {
                    property: name.clone(),
                    required: required.contains(name.as_str()),
                    min_depth: usize::MAX,
                    structured: leaf_type(schema).is_none(),
                    ..Member::default()
                });
            member.min_depth = member.min_depth.min(depth + 1);
            member.page |= page;
            member.tokens += mult * each;
            member.each = member.each.max(each);
            chars += name.len() as f64 + 4.0 + value;
        }
        chars
    }

    /// Where the array schema `node` (at `location`) is, by its `$ref`s, if it is one.
    fn array_location(&self, node: &Value, location: &str) -> Option<String> {
        let mut node = node;
        let mut location = location.to_string();
        for _ in 0..MAX_DEPTH {
            let obj = node.as_object()?;
            let Some((target, next)) = ref_target(obj)
                .and_then(|target| Some((target.clone(), self.doc.pointer(&target)?)))
            else {
                return (schema_type(obj) == Some("array")).then_some(location);
            };
            node = next;
            location = target;
        }
        None
    }

    /// Characters of one instance of `node`, without recording anything
    /// (memoized per definition).
    fn cost(&mut self, node: &Value, location: &str) -> f64 {
        if let Some(target) = node.as_object().and_then(ref_target)
            && let Some(&chars) = self.memo.get(&target)
        {
            return chars;
        }
        let saved = std::mem::replace(
            &mut self.stats,
            Stats {
                tokens: 0.0,
                members: HashMap::new(),
                arrays: HashMap::new(),
            },
        );
        let chars = self.visit(node, location.to_string(), 1.0, 2, true);
        self.stats = saved;
        if let Some(target) = node.as_object().and_then(ref_target) {
            self.memo.insert(target, chars);
        }
        chars
    }
}

/// The location a local `$ref` points to (`#/$defs/x` → `/$defs/x`).
fn ref_target(obj: &Map<String, Value>) -> Option<String> {
    obj.get("$ref")?
        .as_str()?
        .strip_prefix('#')
        .map(str::to_string)
}

/// The non-null `type` of a schema object.
fn schema_type(obj: &Map<String, Value>) -> Option<&str> {
    match obj.get("type")? {
        Value::String(t) => Some(t.as_str()),
        Value::Array(types) => types
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null"),
        _ => None,
    }
    .or_else(|| obj.get("properties").map(|_| "object"))
}

/// The scalar type of `schema` when it is a plain leaf (no reference or
/// combinator), else `None`.
fn leaf_type(schema: &Value) -> Option<&str> {
    let obj = schema.as_object()?;
    if ["$ref", "allOf", "anyOf", "oneOf"]
        .iter()
        .any(|k| obj.contains_key(*k))
    {
        return None;
    }
    match schema_type(obj) {
        Some("object" | "array") | None => None,
        Some(t) => Some(t),
    }
}

/// Items an array is estimated to hold: `wanted`, or by default what the prompt
/// asks for, within its bounds.
fn items(obj: &Map<String, Value>, wanted: Option<u64>) -> f64 {
    let min = obj.get("minItems").and_then(Value::as_f64).unwrap_or(0.0);
    let max = obj
        .get("maxItems")
        .and_then(Value::as_f64)
        .unwrap_or(f64::MAX);
    wanted.map_or(DEFAULT_ITEMS, |w| w as f64).min(max).max(min)
}

/// Characters of a scalar of type `t` (`name` is its member name).
fn scalar(obj: &Map<String, Value>, t: &str, name: Option<&str>) -> f64 {
    match t {
        "integer" => 6.0,
        "number" => 8.0,
        "boolean" => 5.0,
        "null" => 4.0,
        _ => {
            if let Some(Value::Array(values)) = obj.get("enum") {
                let total: usize = values.iter().map(|v| v.to_string().len()).sum();
                return total as f64 / values.len().max(1) as f64;
            }
            let by_format = match obj.get("format").and_then(Value::as_str) {
                Some("date-time") => Some(24.0),
                Some("date") => Some(12.0),
                Some("uri" | "url" | "uri-reference") => Some(50.0),
                Some("email") => Some(24.0),
                Some("uuid") => Some(38.0),
                _ => None,
            };
            let by_name = name.map(str::to_ascii_lowercase).and_then(|n| {
                if n.ends_with("url") || n.ends_with("_uri") {
                    Some(50.0)
                } else if [
                    "description",
                    "overview",
                    "body",
                    "message",
                    "biography",
                    "bio",
                    "summary",
                ]
                .iter()
                .any(|w| n.contains(w))
                {
                    Some(160.0)
                } else {
                    None
                }
            });
            let base: f64 = by_format.or(by_name).unwrap_or(22.0);
            let capped = obj
                .get("maxLength")
                .and_then(Value::as_f64)
                .map_or(base, |m| base.min(m + 2.0));
            capped + 2.0
        }
    }
}

/// The words a request names: query names and values, path values and
/// body keys and string values, split on commas and whitespace.
pub fn requested_words(call: &crate::OperationCall) -> HashSet<String> {
    let mut words = HashSet::new();
    let mut add = |text: &str| {
        for w in text.split(|c: char| c == ',' || c.is_whitespace()) {
            if !w.is_empty() {
                words.insert(w.to_string());
            }
        }
    };
    for (k, v) in call.query.iter().chain(&call.path_params) {
        add(k);
        match v {
            Value::String(s) => add(s),
            Value::Array(items) => items.iter().filter_map(Value::as_str).for_each(&mut add),
            _ => {}
        }
    }
    if let Some(Value::Object(body)) = &call.body {
        for (k, v) in body {
            add(k);
            if let Value::String(s) = v {
                add(s);
            }
        }
    }
    words
}

/// A page size a request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageLimit {
    /// The number of items.
    pub items: u64,
    /// The parameter's `maximum`, when an array declares it as its `maxItems`, which
    /// says which array this is the size of.
    pub maximum: Option<u64>,
}

impl From<u64> for PageLimit {
    fn from(items: u64) -> Self {
        Self {
            items,
            maximum: None,
        }
    }
}

/// What a query parameter's name holds, normalized (see [`normalized`]), when it asks
/// for a page size, as a last resort. Earlier words go first.
const PAGE_SIZE_WORDS: [&str; 5] = ["limit", "perpage", "size", "first", "top"];

/// Lower case, with only letters and digits.
fn normalized(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a query parameter's declared schema allows an integer. One the contract
/// does not declare, or gives no type, may be one.
fn may_be_integer(param: Option<&Value>) -> bool {
    match param.and_then(|p| p.get("type")) {
        Some(Value::String(t)) => t == "integer",
        Some(Value::Array(types)) => types.iter().any(|t| t == "integer"),
        _ => true,
    }
}

/// The page size a request asks for: an integer query parameter whose `maximum` is the
/// `maxItems` of an array `doc()` holds outside any array, as the contract says so; or
/// else, as a last resort, one whose name holds `limit`, `per_page`, `size`, `first` or `top`. Zero, or a
/// value that is no number, asks for nothing the budget can keep.
#[must_use]
pub fn requested_limit<'a>(
    query: Option<&Value>,
    doc: impl Fn() -> &'a Value,
    call: &crate::OperationCall,
) -> Option<PageLimit> {
    let declared = query.and_then(|q| q.get("properties"));
    let mut sizes: Vec<(&String, u64, Option<&Value>)> = call
        .query
        .iter()
        .filter_map(|(name, value)| {
            let size = match value {
                Value::Number(n) => n.as_u64(),
                Value::String(s) => s.trim().parse().ok(),
                _ => None,
            }
            .filter(|size| *size > 0)?;
            let param = declared.and_then(|d| d.get(name));
            may_be_integer(param).then_some((name, size, param))
        })
        .collect();
    sizes.sort_by_key(|(name, ..)| *name);
    for (_, items, param) in &sizes {
        let maximum = param.and_then(|p| p.get("maximum")).and_then(Value::as_u64);
        if let Some(maximum) = maximum
            && !arrays_up_to(doc(), maximum)?.is_empty()
        {
            return Some(PageLimit {
                items: *items,
                maximum: Some(maximum),
            });
        }
    }
    PAGE_SIZE_WORDS.iter().find_map(|word| {
        sizes
            .iter()
            .find(|(name, ..)| normalized(name).contains(word))
            .map(|(_, items, _)| PageLimit::from(*items))
    })
}

/// The locations of the arrays outside any array whose `maxItems` is `maximum`.
fn arrays_up_to(doc: &Value, maximum: u64) -> Option<HashSet<String>> {
    Some(
        outer_arrays(doc)?
            .into_iter()
            .filter(|at| {
                doc.pointer(at)
                    .and_then(|array| array.get("maxItems"))
                    .and_then(Value::as_u64)
                    == Some(maximum)
            })
            .collect(),
    )
}

/// The locations of the arrays outside any array.
fn outer_arrays(doc: &Value) -> Option<Vec<String>> {
    let every = Held {
        floor: None,
        pages: None,
    };
    let mut outer: Vec<String> = Stats::of(doc, every)?
        .arrays
        .into_iter()
        .filter(|(_, array)| array.page)
        .map(|(location, _)| location)
        .collect();
    outer.sort();
    Some(outer)
}

/// Which arrays outside any array hold a page's items. Those `maximum` names if any
/// does (see [`requested_limit`]). Otherwise the shape says: arrays of objects if there
/// are any, as words beside a page are no page, and of those the heaviest item in each
/// object, so that a search's `tracks` and `artists` are each a page and a `warnings`
/// beside `items` is not.
fn page_arrays(doc: &Value, maximum: Option<u64>) -> Option<HashSet<String>> {
    if let Some(maximum) = maximum {
        let named = arrays_up_to(doc, maximum)?;
        if !named.is_empty() {
            return Some(named);
        }
    }
    let outer = outer_arrays(doc)?;
    let item = |at: &String| doc.pointer(at).and_then(|array| array.get("items"));
    let structured = |at: &&String| item(at).is_some_and(|i| leaf_type(i).is_none());
    let any_structured = outer.iter().any(|at| structured(&at));
    let mut walk = Walk {
        doc,
        held: Held {
            floor: None,
            pages: None,
        },
        memo: HashMap::new(),
        stack: Vec::new(),
        visits: 0,
        stats: Stats {
            tokens: 0.0,
            members: HashMap::new(),
            arrays: HashMap::new(),
        },
    };
    let mut heaviest: HashMap<&str, (f64, &String)> = HashMap::new();
    for at in outer.iter().filter(|at| !any_structured || structured(at)) {
        let weight = item(at).map_or(0.0, |i| walk.cost(i, &format!("{at}/items")));
        let parent = at
            .rsplit_once("/properties/")
            .map_or("", |(parent, _)| parent);
        let held = heaviest.entry(parent).or_insert((weight, at));
        if weight > held.0 {
            *held = (weight, at);
        }
    }
    Some(heaviest.into_values().map(|(_, at)| at.clone()).collect())
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(items: u64) -> Option<PageLimit> {
        Some(PageLimit::from(items))
    }

    fn every(floor: Option<u64>) -> Held<'static> {
        Held { floor, pages: None }
    }

    fn words(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn user() -> Value {
        let mut props = Map::new();
        props.insert("login".into(), json!({"type": "string"}));
        for f in [
            "avatar_url",
            "html_url",
            "repos_url",
            "events_url",
            "followers_url",
            "gists_url",
        ] {
            props.insert(f.into(), json!({"type": "string", "format": "uri"}));
        }
        let required: Vec<&String> = props.keys().collect();
        json!({"type": "object", "required": required, "properties": props})
    }

    fn search() -> Value {
        json!({
            "type": "object",
            "required": ["total_count", "items"],
            "properties": {
                "total_count": {"type": "integer"},
                "items": {"type": "array", "items": {"$ref": "#/$defs/item"}}
            },
            "$defs": {
                "user": user(),
                "item": {
                    "type": "object",
                    "required": ["sha", "author", "committer", "repository"],
                    "properties": {
                        "sha": {"type": "string"},
                        "author": {"$ref": "#/$defs/user"},
                        "committer": {"$ref": "#/$defs/user"},
                        "repository": {
                            "type": "object",
                            "required": ["owner", "name"],
                            "properties": {"name": {"type": "string"}, "owner": {"$ref": "#/$defs/user"}}
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn a_small_answer_is_left_alone() {
        let doc = json!({"type": "object", "properties": {"id": {"type": "integer"}}});
        assert!(shape(&doc, &HashSet::new(), false, 100, None).is_none());
    }

    #[test]
    fn unrequested_optional_sub_objects_go_first() {
        let doc = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "integer"},
                "credits": {"type": "array", "items": user()},
                "videos": {"type": "array", "items": user()}
            }
        });
        let shaped = shape(&doc, &words(&["videos"]), false, 300, None).unwrap();
        let props = shaped.schema["properties"].as_object().unwrap();
        assert!(props.contains_key("videos"), "{}", shaped.schema);
        assert!(!props.contains_key("credits"));
        assert!(!shaped.fills_any(), "optional members are not filled");
    }

    #[test]
    fn lists_are_capped_then_deep_required_members_filled() {
        let shaped = shape(&search(), &HashSet::new(), false, 150, None).unwrap();
        assert_eq!(
            shaped.schema["properties"]["items"]["maxItems"], 1,
            "{}",
            shaped.schema
        );
        let item = &shaped.schema["$defs"]["item"];
        // The heaviest members of an item are filled, its own fields kept.
        assert!(item["properties"].get("sha").is_some());
        assert!(
            item["properties"].get("repository").is_none()
                || item["properties"].get("author").is_none()
        );
        assert!(shaped.fills("/$defs/item", "author") || shaped.fills("/$defs/item", "repository"));
        let still_required: Vec<&str> = item["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for p in still_required {
            assert!(
                item["properties"].get(p).is_some(),
                "{p} required but left out"
            );
        }
        // Top-level members are never left out.
        assert!(
            shaped.schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("total_count"))
        );
    }

    #[test]
    fn a_selection_is_only_capped() {
        let shaped = shape(&search(), &HashSet::new(), true, 150, None).unwrap();
        assert!(!shaped.fills_any());
        assert!(
            shaped.schema["$defs"]["item"]["properties"]
                .get("repository")
                .is_some()
        );
        assert_eq!(shaped.schema["properties"]["items"]["maxItems"], 1);
    }

    #[test]
    fn min_items_wins_over_the_cap() {
        let doc = json!({"type": "array", "minItems": 3, "items": user()});
        let shaped = shape(&doc, &HashSet::new(), false, 10, None).unwrap();
        assert_eq!(shaped.schema["maxItems"], 3);
    }

    /// A page of `n` items, each with heavy optional and required members.
    fn page() -> Value {
        json!({
            "type": "object",
            "required": ["items"],
            "properties": {
                "items": {"type": "array", "items": {
                    "type": "object",
                    "required": ["id", "owner"],
                    "properties": {
                        "id": {"type": "string"},
                        "owner": user(),
                        "extra": user(),
                        "tags": {"type": "array", "items": {"type": "string"}}
                    }
                }}
            }
        })
    }

    #[test]
    fn a_word_names_a_member_sharing_its_stem() {
        for (word, member) in [
            ("track", "tracks"),
            ("Tracks", "track"),
            ("category", "categories"),
            ("movie", "movies"),
            ("box", "boxes"),
            ("status", "statuses"),
            ("class", "classes"),
            ("release", "releases"),
            ("match", "matches"),
        ] {
            assert_eq!(stem(word), stem(member), "{word} {member}");
        }
        for (word, member) in [("track", "tracker"), ("art", "artists"), ("id", "ideas")] {
            assert_ne!(stem(word), stem(member), "{word} {member}");
        }
        assert_eq!(stem("s"), "s");
        assert_eq!(stem("es"), "es");
    }

    #[test]
    fn a_limit_is_kept_when_dropping_deep_optional_members_fits() {
        // Five items with one heavy member each fit; with both they do not.
        let shaped = shape(&page(), &HashSet::new(), false, 900, at(5)).unwrap();
        let items = &shaped.schema["properties"]["items"];
        assert_eq!(items["maxItems"], 5, "{}", shaped.schema);
        assert!(items["items"]["properties"].get("extra").is_none());
        assert!(!shaped.fills_any());
    }

    #[test]
    fn nested_arrays_and_required_members_go_before_the_limit() {
        let shaped = shape(&page(), &HashSet::new(), false, 120, at(5)).unwrap();
        let items = &shaped.schema["properties"]["items"];
        assert_eq!(items["maxItems"], 5, "{}", shaped.schema);
        assert!(shaped.fills("/properties/items/items", "owner"));
    }

    /// Required pages under members: what a request names is more than a key.
    fn pages() -> Value {
        json!({
            "type": "object",
            "properties": {
                "tracks": {
                    "type": "object",
                    "required": ["total", "items"],
                    "properties": {
                        "total": {"type": "integer"},
                        "items": {"type": "array", "items": {
                            "type": "object",
                            "required": ["id", "album"],
                            "properties": {"id": {"type": "string"}, "album": user()}
                        }}
                    }
                }
            }
        })
    }

    #[test]
    fn a_page_outlasts_the_members_of_its_items() {
        for limit in [at(5), None] {
            let shaped = shape(&pages(), &words(&["tracks"]), false, 100, limit).unwrap();
            let tracks = &shaped.schema["properties"]["tracks"];
            assert!(tracks["properties"].get("items").is_some(), "{tracks}");
            assert!(shaped.fills("/properties/tracks/properties/items/items", "album"));
        }
        // When only the page itself can bring it within budget, it goes.
        let shaped = shape(&pages(), &words(&["tracks"]), false, 8, at(5)).unwrap();
        let tracks = &shaped.schema["properties"]["tracks"];
        assert!(tracks["properties"].get("items").is_none(), "{tracks}");
        assert!(shaped.fills("/properties/tracks", "items"));
    }

    #[test]
    fn a_limit_too_large_to_fit_is_capped_as_far_as_it_must_be() {
        let shaped = shape(&page(), &HashSet::new(), false, 120, at(50)).unwrap();
        let items = &shaped.schema["properties"]["items"];
        assert_eq!(items["maxItems"], 5, "{}", shaped.schema);
        let estimate = Stats::of(&shaped.schema, every(Some(5))).unwrap().tokens;
        assert!(estimate <= 120.0, "{estimate}");
        // Nothing can bring this one down to its budget: as low as it goes.
        let shaped = shape(&page(), &HashSet::new(), false, 3, at(50)).unwrap();
        assert_eq!(shaped.schema["properties"]["items"]["maxItems"], 1);
    }

    #[test]
    fn nested_members_counted_twice_do_not_stop_it_over_budget() {
        // `outer` holds `inner`: a pass counts both, though one removal saves it.
        let doc = json!({
            "type": "object",
            "required": ["item"],
            "properties": {"item": {
                "type": "object",
                "required": ["outer", "other"],
                "properties": {
                    "other": user(),
                    "outer": {
                        "type": "object",
                        "required": ["inner"],
                        "properties": {"inner": user(), "more": user()}
                    }
                }
            }}
        });
        let budget = 100;
        let shaped = shape(&doc, &HashSet::new(), false, budget, None).unwrap();
        let estimate = Stats::of(&shaped.schema, every(None)).unwrap().tokens;
        assert!(estimate <= f64::from(budget), "{estimate}");
    }

    #[test]
    fn a_limit_counts_in_the_estimate() {
        // Fits at the default four items, not at the forty asked for.
        assert!(shape(&page(), &HashSet::new(), false, 1300, None).is_none());
        assert!(shape(&page(), &HashSet::new(), false, 1300, at(40)).is_some());
        // And a smaller limit needs no shaping that the default would.
        assert!(shape(&page(), &HashSet::new(), false, 400, None).is_some());
        assert!(shape(&page(), &HashSet::new(), false, 400, at(1)).is_none());
    }

    #[test]
    fn the_limit_is_read_from_the_query() {
        let doc = json!({"type": "array", "maxItems": 50, "items": {"type": "object"}});
        let limit = |query: Value, declared: Value| {
            requested_limit(
                Some(&json!({"properties": declared})),
                || &doc,
                &crate::OperationCall {
                    query: serde_json::from_value(query).unwrap(),
                    ..Default::default()
                },
            )
        };
        let size = |query: Value| limit(query, json!({})).map(|l| l.items);
        assert_eq!(size(json!({"limit": 5})), Some(5));
        assert_eq!(size(json!({"limit": "5"})), Some(5));
        assert_eq!(size(json!({"limit": "0"})), None);
        assert_eq!(size(json!({"limit": "many"})), None);
        assert_eq!(size(json!({"q": "5"})), None);
        assert_eq!(size(json!({"PageSize": 5})), Some(5));
        assert_eq!(size(json!({"per-page": 5})), Some(5));
        // Earlier words go first, whatever the query's order.
        assert_eq!(size(json!({"top": 7, "limit": 5})), Some(5));
        // Declared as text, it is no size.
        assert_eq!(
            limit(json!({"limit": 5}), json!({"limit": {"type": "string"}})),
            None
        );
        // The contract's signal, under a name no word covers, and over a word's.
        let named = json!({"n": {"type": "integer", "maximum": 50}});
        let found = limit(json!({"n": 9, "limit": 5}), named).unwrap();
        assert_eq!((found.items, found.maximum), (9, Some(50)));
        // A maximum no array declares says nothing.
        let unmatched = json!({"n": {"type": "integer", "maximum": 60}});
        assert_eq!(limit(json!({"n": 9}), unmatched), None);
    }

    #[test]
    fn the_pages_of_an_answer() {
        let thing = json!({"type": "object", "properties": {
            "id": {"type": "string"}, "name": {"type": "string"}, "about": {"type": "string"}
        }});
        let note = json!({"type": "object", "properties": {"code": {"type": "string"}}});
        let doc = json!({"type": "object", "properties": {
            "things": {"type": "array", "items": thing},
            "notes": {"type": "array", "maxItems": 500, "items": note},
            "tags": {"type": "array", "items": {"type": "string"}},
            "found": {"type": "object", "properties": {
                "items": {"type": "array", "items": thing}
            }}
        }});
        let set = |locations: &[&str]| {
            locations
                .iter()
                .map(ToString::to_string)
                .collect::<HashSet<_>>()
        };
        // The heaviest in each object; a list of words is none while objects are listed.
        assert_eq!(
            page_arrays(&doc, None).unwrap(),
            set(&["/properties/things", "/properties/found/properties/items"])
        );
        // The contract's signal wins over the weight.
        assert_eq!(
            page_arrays(&doc, Some(500)).unwrap(),
            set(&["/properties/notes"])
        );
        // A bare list is a page, words or not.
        let words = json!({"type": "array", "items": {"type": "string"}});
        assert_eq!(page_arrays(&words, None).unwrap(), set(&[""]));
    }

    #[test]
    fn request_words_split_lists() {
        let call = crate::OperationCall {
            query: serde_json::from_value(
                json!({"append_to_response": "videos,credits", "page": 2}),
            )
            .unwrap(),
            ..Default::default()
        };
        let w = requested_words(&call);
        assert!(w.contains("videos") && w.contains("credits") && w.contains("append_to_response"));
    }
}
