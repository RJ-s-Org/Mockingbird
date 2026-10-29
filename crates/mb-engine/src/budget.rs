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
//!    out (`append_to_response=videos,credits` keeps `videos` and `credits`);
//! 2. arrays get a `maxItems`: top-level lists 5, 3, 2, then 1 item, nested
//!    arrays 3, 2, then 1;
//! 3. required object and array members below the top level are left out,
//!    heaviest first, and filled after generation by the faker from the full
//!    schema (see [`Shaped::fills`]).
//!
//! A GraphQL selection names every field it reads, so a narrowed schema
//! only gets step 2. Validation always uses the full schema.

use std::collections::{HashMap, HashSet};

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

    pub fn fills_any(&self) -> bool {
        !self.dropped.is_empty()
    }
}

/// `doc` (a schema with its `$defs`) shaped to `budget` completion tokens,
/// or `None` when its estimated answer already fits (or is too large a
/// schema graph to estimate). `requested` are the words of the request
/// (query names and values, body keys); `selected` marks a schema already
/// narrowed to a selection.
pub fn shape(
    doc: &Value,
    requested: &HashSet<String>,
    selected: bool,
    budget: u32,
) -> Option<Shaped> {
    let budget = f64::from(budget);
    if budget <= 0.0 {
        return None;
    }
    let mut doc = doc.clone();
    let mut stats = Stats::of(&doc)?;
    if stats.tokens <= budget {
        return None;
    }
    let before = stats.tokens;
    if !selected {
        let optional = stats.candidates(|m| !m.required && !requested.contains(&m.property));
        drop_until(&mut doc, &mut stats, optional, budget, None)?;
    }
    for (top, nested) in CAPS {
        if stats.tokens <= budget {
            break;
        }
        for (location, is_top) in &stats.arrays {
            cap(&mut doc, location, if *is_top { top } else { nested });
        }
        stats = Stats::of(&doc)?;
    }
    let mut dropped = HashSet::new();
    if !selected && stats.tokens > budget {
        let required = stats.candidates(|m| m.required && m.min_depth >= 2);
        drop_until(&mut doc, &mut stats, required, budget, Some(&mut dropped))?;
    }
    tracing::debug!(
        estimated = before.round(),
        shaped = stats.tokens.round(),
        filled = dropped.len(),
        "shaped the answer to the output budget"
    );
    Some(Shaped {
        schema: doc,
        dropped,
    })
}

/// Leaves out `candidates` (heaviest first) until the estimate fits, then
/// re-estimates; `dropped` records them when given.
fn drop_until(
    doc: &mut Value,
    stats: &mut Stats,
    candidates: Vec<(String, String, f64)>,
    budget: f64,
    mut dropped: Option<&mut HashSet<(String, String)>>,
) -> Option<()> {
    if candidates.is_empty() || stats.tokens <= budget {
        return Some(());
    }
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
    *stats = Stats::of(doc)?;
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
}

/// The estimate of a schema's answer and where its tokens go.
struct Stats {
    tokens: f64,
    members: HashMap<(String, String), Member>,
    /// Array schema location → whether some occurrence is a top-level list
    /// (the root, or a member of the root object, outside any array).
    arrays: HashMap<String, bool>,
}

impl Stats {
    fn of(doc: &Value) -> Option<Self> {
        let mut walk = Walk {
            doc,
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
    fn candidates(&self, keep: impl Fn(&Member) -> bool) -> Vec<(String, String, f64)> {
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
                let entry = self.stats.arrays.entry(location.clone()).or_insert(false);
                *entry |= top;
                let n = items(obj);
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
            let child = format!("{location}/properties/{}", escape(name));
            let value = match leaf_type(schema) {
                Some(t) => scalar(schema.as_object().unwrap_or(&Map::new()), t, Some(name)),
                None => self.visit(schema, child, mult, depth + 1, in_array),
            };
            let each = (name.len() as f64 + 4.0 + value) / CHARS_PER_TOKEN;
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
            member.tokens += mult * each;
            member.each = member.each.max(each);
            chars += name.len() as f64 + 4.0 + value;
        }
        chars
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

fn items(obj: &Map<String, Value>) -> f64 {
    let min = obj.get("minItems").and_then(Value::as_f64).unwrap_or(0.0);
    let max = obj
        .get("maxItems")
        .and_then(Value::as_f64)
        .unwrap_or(f64::MAX);
    DEFAULT_ITEMS.min(max).max(min)
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

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert!(shape(&doc, &HashSet::new(), false, 100).is_none());
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
        let shaped = shape(&doc, &words(&["videos"]), false, 300).unwrap();
        let props = shaped.schema["properties"].as_object().unwrap();
        assert!(props.contains_key("videos"), "{}", shaped.schema);
        assert!(!props.contains_key("credits"));
        assert!(!shaped.fills_any(), "optional members are not filled");
    }

    #[test]
    fn lists_are_capped_then_deep_required_members_filled() {
        let shaped = shape(&search(), &HashSet::new(), false, 150).unwrap();
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
        let shaped = shape(&search(), &HashSet::new(), true, 150).unwrap();
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
        let shaped = shape(&doc, &HashSet::new(), false, 10).unwrap();
        assert_eq!(shaped.schema["maxItems"], 3);
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
