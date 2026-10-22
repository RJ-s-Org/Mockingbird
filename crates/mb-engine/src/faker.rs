use base64::Engine as _;
use rand::{Rng, RngExt};
use serde_json::{Map, Number, Value, json};
use std::collections::{HashMap, HashSet, VecDeque};

const WORDS: &[&str] = &[
    "alpha", "bravo", "cedar", "delta", "ember", "fjord", "garnet", "harbor", "iris", "juniper",
    "kestrel", "lumen", "meadow", "nectar", "onyx", "pebble", "quartz", "raven", "sable", "tundra",
];

/// Extra levels allowed beyond `max_depth` for schemas that cannot terminate
/// by omitting optional members (for example a required self-reference).
/// Bounds the stack; the node caps below bound the total work.
const HARD_DEPTH_SLACK: usize = 32;

/// Values generated per `fake` call before every further position switches
/// to its minimal instance (cheapest branch, no optional properties,
/// `minItems` elements).
const NODE_BUDGET: usize = 10_000;

/// Values generated past [`NODE_BUDGET`] before containers are emitted
/// empty. Only reached when a schema has no finite instance (or a very large
/// minimal one); keeps the worst case linear.
const HARD_NODE_EXTRA: usize = 100_000;

/// Most characters in a string whose schema sets no `maxLength`.
pub const UNCONSTRAINED_MAX_CHARS: usize = 64;

/// Width of the window an integer without tight bounds is drawn from.
pub const SMALL_INT_WINDOW: i64 = 100;

/// Cost of a schema with no finite instance.
const INFINITE: u64 = u64::MAX;

/// Deterministic-shape, seeded-random generator of values that satisfy a
/// JSON Schema.
pub struct Faker {
    max_depth: usize,
}

impl Default for Faker {
    fn default() -> Self {
        Self::new(6)
    }
}

/// Per-call state: the root (for `$ref`), its minimal-cost table and the
/// number of values generated so far.
struct Ctx<'a> {
    root: &'a Value,
    costs: &'a Costs,
    generated: usize,
}

impl Faker {
    pub fn new(max_depth: usize) -> Self {
        Self { max_depth }
    }

    pub fn fake(&self, schema_with_defs: &Value, rng: &mut impl Rng) -> Value {
        self.fake_with_branch(schema_with_defs, None, rng)
    }

    /// Like [`Faker::fake`], but the outermost `oneOf`/`anyOf` choice is
    /// branch `branch` (modulo the branch count) instead of a random one.
    /// Callers that validate the output can try each branch in turn.
    pub fn fake_with_branch(
        &self,
        schema_with_defs: &Value,
        branch: Option<usize>,
        rng: &mut impl Rng,
    ) -> Value {
        let costs = Costs::compute(schema_with_defs);
        self.fake_with_costs(schema_with_defs, branch, &costs, rng)
    }

    /// Like [`Faker::fake_with_branch`], with the cost table precomputed by
    /// [`Costs::compute`] on the same `schema_with_defs`. Callers that fake
    /// one schema repeatedly compute it once and reuse it.
    pub fn fake_with_costs(
        &self,
        schema_with_defs: &Value,
        branch: Option<usize>,
        costs: &Costs,
        rng: &mut impl Rng,
    ) -> Value {
        let mut ctx = Ctx {
            root: schema_with_defs,
            costs,
            generated: 0,
        };
        self.gen_value(&mut ctx, schema_with_defs, None, 0, branch, rng)
    }

    /// A value for `schema`; `field` is the object member it fills, if any.
    fn gen_value<R: Rng>(
        &self,
        ctx: &mut Ctx<'_>,
        schema: &Value,
        field: Option<&str>,
        depth: usize,
        branch: Option<usize>,
        rng: &mut R,
    ) -> Value {
        ctx.generated = ctx.generated.saturating_add(1);
        // Past the depth limit or the node budget, every position takes its
        // minimal instance.
        let at_limit = depth >= self.max_depth || ctx.generated > NODE_BUDGET;
        // Absolute cutoff: past it, containers are emitted empty so that even
        // an unsatisfiable required self-reference terminates.
        let hard_limit = depth >= self.max_depth + HARD_DEPTH_SLACK || hard_exhausted(ctx);
        let pick = match branch {
            Some(i) => Pick::Forced(i),
            None if at_limit => Pick::Shallow,
            None => Pick::Random,
        };
        let flat = flatten(ctx.root, ctx.costs, schema, 0, pick, rng);
        let Some(s) = flat.as_object() else {
            // `true` accepts anything, `false` accepts nothing.
            return Value::String(word(rng));
        };

        if let Some(c) = s.get("const") {
            return c.clone();
        }
        if let Some(Value::Array(options)) = s.get("enum")
            && !options.is_empty()
        {
            return options[rng.random_range(0..options.len())].clone();
        }

        let types = type_names(s);
        // A list is never null: at the limit it is empty instead.
        if at_limit && types.contains(&"null") && !types.contains(&"array") {
            return Value::Null;
        }

        let chosen = types
            .iter()
            .copied()
            .find(|t| *t != "null")
            .or(types.first().copied())
            .unwrap_or_else(|| infer_type(s));

        match chosen {
            "null" => Value::Null,
            "boolean" => Value::Bool(rng.random_bool(0.5)),
            "integer" => fake_integer(s, field, rng),
            "number" => fake_number(s, rng),
            "string" => Value::String(fake_string(s, rng)),
            "array" if hard_limit => Value::Array(Vec::new()),
            "object" if hard_limit => Value::Object(Map::new()),
            "array" => self.fake_array(ctx, s, depth, at_limit, rng),
            "object" => self.fake_object(ctx, s, depth, at_limit, rng),
            _ => Value::String(word(rng)),
        }
    }

    fn fake_object<R: Rng>(
        &self,
        ctx: &mut Ctx<'_>,
        s: &Map<String, Value>,
        depth: usize,
        at_limit: bool,
        rng: &mut R,
    ) -> Value {
        let props = s.get("properties").and_then(Value::as_object);
        let required: Vec<&str> = s
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let any = Value::Bool(true);
        let mut out = Map::new();
        for name in &required {
            let sub = props.and_then(|p| p.get(*name)).unwrap_or(&any);
            out.insert(
                (*name).to_string(),
                self.gen_value(ctx, sub, Some(name), depth + 1, None, rng),
            );
        }
        if let Some(props) = props {
            for (name, sub) in props {
                // Re-checked per property: the budget may run out mid-object.
                if at_limit || ctx.generated >= NODE_BUDGET {
                    break;
                }
                if !out.contains_key(name) {
                    let value = self.gen_value(ctx, sub, Some(name), depth + 1, None, rng);
                    out.insert(name.clone(), value);
                }
            }
        }
        Value::Object(out)
    }

    fn fake_array<R: Rng>(
        &self,
        ctx: &mut Ctx<'_>,
        s: &Map<String, Value>,
        depth: usize,
        at_limit: bool,
        rng: &mut R,
    ) -> Value {
        let min = s.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
        let max = s
            .get("maxItems")
            .and_then(Value::as_u64)
            .map(|m| m as usize);
        let mut len = if at_limit { min } else { min.max(1) };
        if let Some(max) = max {
            len = len.min(max);
        }
        let unique = s.get("uniqueItems").and_then(Value::as_bool) == Some(true);
        let any = Value::Bool(true);
        let items = s.get("items").unwrap_or(&any);
        let mut out: Vec<Value> = Vec::with_capacity(len.min(NODE_BUDGET));
        for i in 0..len {
            if hard_exhausted(ctx) || (i >= min && ctx.generated >= NODE_BUDGET) {
                break;
            }
            let mut candidate = self.gen_value(ctx, items, None, depth + 1, None, rng);
            if unique {
                let mut tries = 0;
                while out.contains(&candidate) && tries < 32 && !hard_exhausted(ctx) {
                    candidate = self.gen_value(ctx, items, None, depth + 1, None, rng);
                    tries += 1;
                }
            }
            out.push(candidate);
        }
        Value::Array(out)
    }
}

fn hard_exhausted(ctx: &Ctx<'_>) -> bool {
    ctx.generated > NODE_BUDGET + HARD_NODE_EXTRA
}

/// `$ref` targets inlined through `allOf` before falling back to the
/// fixed-point table (bounds `allOf` reference cycles).
const MAX_INLINE_HOPS: usize = 32;

/// Minimal instance cost (roughly, the number of values a smallest instance
/// generates) of every `$ref` target reachable from a root, computed as a
/// fixed point so that recursion is accounted for: a definition whose only
/// instances recurse forever stays infinite.
///
/// Computing it walks every definition, so callers that fake the same
/// schema repeatedly compute it once (see [`Faker::fake_with_costs`]).
pub struct Costs {
    by_ref: HashMap<String, u64>,
}

/// The object keywords of a schema merged across its `allOf` members (and
/// their inlined `$ref` targets), as [`flatten`] merges them for the faker.
#[derive(Default)]
struct Parts<'a> {
    props: HashMap<&'a str, Vec<&'a Value>>,
    required: Vec<&'a str>,
    /// Cost of everything other than the required members.
    extra: u64,
    /// A merged `const`/`enum` ends generation with a single value.
    terminal: bool,
}

impl Costs {
    /// The cost table for `root` (a schema with its `$defs` attached).
    pub fn compute(root: &Value) -> Self {
        let mut refs = Vec::new();
        collect_refs(root, &mut refs);
        refs.sort_unstable();
        refs.dedup();
        let targets: Vec<(String, &Value)> = refs
            .into_iter()
            .filter_map(|r| resolve_ref(root, &r).map(|t| (r, t)))
            .collect();
        let index: HashMap<&str, usize> = targets
            .iter()
            .enumerate()
            .map(|(i, (r, _))| (r.as_str(), i))
            .collect();
        // dependents[j]: targets whose cost reads target j's cost.
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); targets.len()];
        for (i, (_, target)) in targets.iter().enumerate() {
            let mut deps = Vec::new();
            let mut visited = HashSet::new();
            cost_deps(root, target, false, &mut deps, &mut visited);
            deps.sort_unstable();
            deps.dedup();
            for d in deps {
                if let Some(&j) = index.get(d.as_str()) {
                    dependents[j].push(i);
                }
            }
        }
        let mut costs = Self {
            by_ref: targets.iter().map(|(r, _)| (r.clone(), INFINITE)).collect(),
        };
        // Worklist fixed point: costs only decrease from infinity, and a
        // target is re-evaluated only when a cost it reads has decreased.
        // The evaluation cap matches the round-robin bound (`n + 1` rounds
        // of `n` targets) and is not reached in practice.
        let mut queue: VecDeque<usize> = (0..targets.len()).rev().collect();
        let mut queued = vec![true; targets.len()];
        let mut budget = (targets.len() + 1).saturating_mul(targets.len());
        while let Some(i) = queue.pop_front() {
            queued[i] = false;
            if budget == 0 {
                break;
            }
            budget -= 1;
            let (r, target) = &targets[i];
            let cost = costs.of(root, target);
            if let Some(slot) = costs.by_ref.get_mut(r)
                && cost < *slot
            {
                *slot = cost;
                for &d in &dependents[i] {
                    if !queued[d] {
                        queued[d] = true;
                        queue.push_back(d);
                    }
                }
            }
        }
        costs
    }

    /// `1 +` the cost of everything a minimal instance of `schema` must
    /// contain: required properties (with `properties` and `required`
    /// merged across `allOf` members and their `$ref` targets), `minItems`
    /// elements, the cheapest `oneOf`/`anyOf` branch and the `$ref`
    /// target's current cost. Mirrors what [`Faker`] generates past its
    /// limits.
    fn of(&self, root: &Value, schema: &Value) -> u64 {
        let Some(obj) = schema.as_object() else {
            return 1;
        };
        if obj.contains_key("const")
            || matches!(obj.get("enum"), Some(Value::Array(a)) if !a.is_empty())
            || type_names(obj).contains(&"null")
        {
            return 1;
        }
        let mut cost = 1u64;
        if let Some(Value::String(r)) = obj.get("$ref")
            && resolve_ref(root, r).is_some()
        {
            cost = cost.saturating_add(self.ref_cost(r));
        }
        let mut parts = Parts::default();
        self.gather(root, obj, false, 0, &mut parts);
        if parts.terminal {
            return 1;
        }
        cost = cost.saturating_add(parts.extra);
        for name in &parts.required {
            let member = match parts.props.get(name) {
                None => 1,
                Some(decls) => decls
                    .iter()
                    .fold(0u64, |acc, p| acc.saturating_add(self.of(root, p))),
            };
            cost = cost.saturating_add(member);
        }
        cost
    }

    fn ref_cost(&self, r: &str) -> u64 {
        self.by_ref.get(r).copied().unwrap_or(INFINITE)
    }

    /// Adds `obj`'s keywords to `parts`, then those of its `allOf` members.
    /// With `inline_ref`, `obj`'s own `$ref` target is merged in too (an
    /// `allOf` member's reference is part of the same object); otherwise
    /// the caller costs it from the table.
    fn gather<'a>(
        &self,
        root: &'a Value,
        obj: &'a Map<String, Value>,
        inline_ref: bool,
        hops: usize,
        parts: &mut Parts<'a>,
    ) {
        if hops > 0
            && (obj.contains_key("const")
                || matches!(obj.get("enum"), Some(Value::Array(a)) if !a.is_empty()))
        {
            parts.terminal = true;
        }
        if inline_ref
            && let Some(Value::String(r)) = obj.get("$ref")
            && let Some(target) = resolve_ref(root, r)
        {
            match target.as_object() {
                Some(t) if hops < MAX_INLINE_HOPS => self.gather(root, t, true, hops + 1, parts),
                Some(_) => parts.extra = parts.extra.saturating_add(self.ref_cost(r)),
                None => {}
            }
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(Value::Array(branches)) = obj.get(key)
                && let Some(min) = branches.iter().map(|b| self.of(root, b)).min()
            {
                parts.extra = parts.extra.saturating_add(min);
            }
        }
        let min_items = obj.get("minItems").and_then(Value::as_u64).unwrap_or(0);
        if min_items > 0 {
            let item = obj.get("items").map_or(1, |i| self.of(root, i));
            parts.extra = parts.extra.saturating_add(item.saturating_mul(min_items));
        }
        if let Some(Value::Object(props)) = obj.get("properties") {
            for (name, sub) in props {
                parts.props.entry(name.as_str()).or_default().push(sub);
            }
        }
        if let Some(Value::Array(required)) = obj.get("required") {
            for name in required.iter().filter_map(Value::as_str) {
                if !parts.required.contains(&name) {
                    parts.required.push(name);
                }
            }
        }
        if let Some(Value::Array(all)) = obj.get("allOf") {
            for member in all.iter().filter_map(Value::as_object) {
                self.gather(root, member, true, hops + 1, parts);
            }
        }
    }
}

/// Every `$ref` whose table cost [`Costs::of`] may read while costing
/// `value`: all references inside it, plus those inside the targets it
/// inlines through `allOf` (see [`Costs::gather`]).
fn cost_deps(
    root: &Value,
    value: &Value,
    inline_ref: bool,
    out: &mut Vec<String>,
    visited: &mut HashSet<String>,
) {
    match value {
        Value::Object(obj) => {
            if inline_ref
                && let Some(Value::String(r)) = obj.get("$ref")
                && visited.insert(r.clone())
                && let Some(target) = resolve_ref(root, r)
            {
                cost_deps(root, target, true, out, visited);
            }
            for (k, v) in obj {
                match (k.as_str(), v) {
                    ("$ref", Value::String(r)) => out.push(r.clone()),
                    ("allOf", Value::Array(members)) => {
                        for m in members {
                            cost_deps(root, m, true, out, visited);
                        }
                    }
                    _ => cost_deps(root, v, false, out, visited),
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                cost_deps(root, v, false, out, visited);
            }
        }
        _ => {}
    }
}

fn collect_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(obj) => {
            for (k, v) in obj {
                match (k.as_str(), v) {
                    ("$ref", Value::String(r)) => out.push(r.clone()),
                    _ => collect_refs(v, out),
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_refs(v, out);
            }
        }
        _ => {}
    }
}

fn type_names(s: &Map<String, Value>) -> Vec<&str> {
    match s.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn infer_type(s: &Map<String, Value>) -> &'static str {
    let has = |keys: &[&str]| keys.iter().any(|k| s.contains_key(*k));
    if has(&["properties", "required"]) {
        "object"
    } else if has(&["items", "minItems", "maxItems", "uniqueItems"]) {
        "array"
    } else if has(&[
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
    ]) {
        "number"
    } else {
        "string"
    }
}

/// How [`flatten`] chooses among `oneOf`/`anyOf` branches.
#[derive(Clone, Copy)]
enum Pick {
    /// This branch index (modulo the branch count).
    Forced(usize),
    /// A uniformly random branch.
    Random,
    /// The branch with the smallest minimal-instance cost (see [`Costs`]),
    /// so that recursion past the limits converges on a finite instance.
    Shallow,
}

/// Resolve `$ref` (against the root's `$defs`), and merge `allOf` and one
/// `oneOf`/`anyOf` branch (chosen by `pick`) into a single schema object.
fn flatten<R: Rng>(
    root: &Value,
    costs: &Costs,
    schema: &Value,
    hops: usize,
    pick: Pick,
    rng: &mut R,
) -> Value {
    let Some(obj) = schema.as_object() else {
        return schema.clone();
    };
    if hops > 64 {
        return Value::Object(Map::new());
    }
    let mut out = Map::new();
    let mut extras: Vec<Value> = Vec::new();

    if let Some(Value::String(r)) = obj.get("$ref")
        && let Some(target) = resolve_ref(root, r)
    {
        extras.push(flatten(root, costs, target, hops + 1, pick, rng));
    }
    if let Some(Value::Array(all)) = obj.get("allOf") {
        for sub in all {
            extras.push(flatten(root, costs, sub, hops + 1, pick, rng));
        }
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(branches)) = obj.get(key)
            && !branches.is_empty()
        {
            // A nullable list takes its list branch: lists are never null.
            let has_list = branches.iter().any(|b| is_array_schema(root, b));
            let eligible: Vec<usize> = (0..branches.len())
                .filter(|i| !(has_list && is_null_schema(&branches[*i])))
                .collect();
            let index = match pick {
                Pick::Forced(i) => i % branches.len(),
                Pick::Random => eligible[rng.random_range(0..eligible.len())],
                Pick::Shallow => eligible
                    .iter()
                    .copied()
                    .min_by_key(|i| costs.of(root, &branches[*i]))
                    .unwrap_or(0),
            };
            extras.push(flatten(root, costs, &branches[index], hops + 1, pick, rng));
        }
    }

    for (k, v) in obj {
        if !matches!(k.as_str(), "$ref" | "allOf" | "oneOf" | "anyOf" | "$defs") {
            out.insert(k.clone(), v.clone());
        }
    }
    for extra in extras {
        let Value::Object(extra) = extra else {
            continue;
        };
        for (k, v) in extra {
            merge_keyword(&mut out, k, v);
        }
    }
    Value::Object(out)
}

/// Whether `schema` (through one `$ref`) is a list: `type: array` or `items`.
fn is_array_schema(root: &Value, schema: &Value) -> bool {
    let schema = match schema.get("$ref").and_then(Value::as_str) {
        Some(r) => resolve_ref(root, r).unwrap_or(schema),
        None => schema,
    };
    let Some(obj) = schema.as_object() else {
        return false;
    };
    obj.contains_key("items") || type_names(obj) == ["array"]
}

/// Whether `schema` admits only `null`.
fn is_null_schema(schema: &Value) -> bool {
    schema
        .as_object()
        .is_some_and(|obj| type_names(obj) == ["null"])
}

fn merge_keyword(out: &mut Map<String, Value>, key: String, value: Value) {
    match (key.as_str(), out.get_mut(&key), &value) {
        ("properties", Some(Value::Object(existing)), Value::Object(incoming)) => {
            // A property declared on both sides must satisfy both (e.g. a
            // base `petType: string` narrowed by a branch to `const: cat`).
            for (k, v) in incoming {
                match existing.get_mut(k) {
                    None => {
                        existing.insert(k.clone(), v.clone());
                    }
                    Some(current) if current != v => {
                        let both = json!({"allOf": [current.clone(), v.clone()]});
                        *current = both;
                    }
                    Some(_) => {}
                }
            }
        }
        ("required", Some(Value::Array(existing)), Value::Array(incoming)) => {
            for v in incoming {
                if !existing.contains(v) {
                    existing.push(v.clone());
                }
            }
        }
        (_, Some(_), _) => {}
        _ => {
            out.insert(key, value);
        }
    }
}

fn resolve_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    root.pointer(pointer)
}

fn word<R: Rng>(rng: &mut R) -> String {
    WORDS[rng.random_range(0..WORDS.len())].to_string()
}

fn fake_string<R: Rng>(s: &Map<String, Value>, rng: &mut R) -> String {
    match s.get("format").and_then(Value::as_str) {
        Some("uuid") => {
            let mut bytes = [0u8; 16];
            rng.fill(&mut bytes[..]);
            return uuid::Builder::from_random_bytes(bytes)
                .into_uuid()
                .to_string();
        }
        Some("date-time") => {
            return random_datetime(rng).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        }
        Some("date") => return random_datetime(rng).format("%Y-%m-%d").to_string(),
        Some("email") => return format!("{}@example.com", word(rng)),
        Some("uri") => return format!("https://example.com/{}", word(rng)),
        Some("uri-reference") => return format!("/{}/{}", word(rng), word(rng)),
        Some("hostname") => return format!("{}.example.com", word(rng)),
        Some("ipv4") => {
            return (0..4)
                .map(|_| rng.random_range(0..=255u8).to_string())
                .collect::<Vec<_>>()
                .join(".");
        }
        Some("ipv6") => {
            return (0..8)
                .map(|_| format!("{:04x}", rng.random_range(0..=u16::MAX)))
                .collect::<Vec<_>>()
                .join(":");
        }
        Some("int64") => return rng.random_range(-1_000_000_i64..1_000_000).to_string(),
        Some("uint64") => return rng.random_range(0_u64..1_000_000).to_string(),
        Some("byte") => {
            let mut bytes = vec![0u8; rng.random_range(4..17usize)];
            rng.fill(&mut bytes[..]);
            return base64::engine::general_purpose::STANDARD.encode(bytes);
        }
        Some("time") => return random_datetime(rng).format("%H:%M:%SZ").to_string(),
        Some("duration") => {
            return format!(
                "PT{}H{}M{}S",
                rng.random_range(0..24u8),
                rng.random_range(0..60u8),
                rng.random_range(0..60u8)
            );
        }
        Some("proto-duration") => {
            let secs = rng.random_range(0..86_400u32);
            return if rng.random_bool(0.5) {
                format!("{secs}s")
            } else {
                format!("{secs}.{:03}s", rng.random_range(0..1000u16))
            };
        }
        Some("hex") => {
            let len = rng.random_range(1..17usize);
            return (0..len)
                .map(|_| format!("{:02x}", rng.random_range(0..=255u8)))
                .collect();
        }
        _ => {}
    }
    if let Some(lit) = s
        .get("pattern")
        .and_then(Value::as_str)
        .and_then(fixed_literal)
    {
        return lit;
    }
    let min = s.get("minLength").and_then(Value::as_u64).unwrap_or(0) as usize;
    let max = Some(
        s.get("maxLength")
            .and_then(Value::as_u64)
            .map_or(UNCONSTRAINED_MAX_CHARS, |m| m as usize)
            .max(min),
    );
    let mut out = word(rng);
    let mut len = out.chars().count();
    if len < min {
        out.reserve(min - len);
        while len < min {
            out.push(char::from(b'a' + rng.random_range(0..26u8)));
            len += 1;
        }
    }
    if let Some(max) = max
        && len > max
    {
        out = out.chars().take(max).collect();
    }
    out
}

fn random_datetime<R: Rng>(rng: &mut R) -> chrono::DateTime<chrono::Utc> {
    // 2000-01-01 .. ~2033
    let secs = rng.random_range(946_684_800_i64..2_000_000_000);
    chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default()
}

/// A pattern that is just a literal (optionally anchored) can be satisfied
/// verbatim; anything else is ignored.
fn fixed_literal(pattern: &str) -> Option<String> {
    let body = pattern.strip_prefix('^').unwrap_or(pattern);
    let body = body.strip_suffix('$').unwrap_or(body);
    let is_literal = !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' ' | ':' | '/' | '@'));
    is_literal.then(|| body.to_string())
}

struct Bounds {
    lo: f64,
    hi: f64,
    lo_exclusive: bool,
    hi_exclusive: bool,
}

fn bounds(s: &Map<String, Value>) -> Bounds {
    let num = |k: &str| s.get(k).and_then(Value::as_f64);
    let (mut lo, mut lo_exclusive) = (num("minimum"), false);
    if let Some(x) = num("exclusiveMinimum")
        && lo.is_none_or(|l| x >= l)
    {
        lo = Some(x);
        lo_exclusive = true;
    }
    let (mut hi, mut hi_exclusive) = (num("maximum"), false);
    if let Some(x) = num("exclusiveMaximum")
        && hi.is_none_or(|h| x <= h)
    {
        hi = Some(x);
        hi_exclusive = true;
    }
    let (lo, hi) = match (lo, hi) {
        (Some(l), Some(h)) => (l, h),
        (Some(l), None) => (l, l + 1000.0),
        (None, Some(h)) => (h - 1000.0, h),
        (None, None) => (0.0, 1000.0),
    };
    Bounds {
        lo,
        hi,
        lo_exclusive,
        hi_exclusive,
    }
}

/// Field names whose integers are counts: small and non-negative.
const COUNT_SUFFIXES: [&str; 8] = [
    "count", "total", "size", "length", "page", "pages", "limit", "offset",
];

/// An integer within the declared bounds. Without tight bounds (missing,
/// wider than 10 000, or at the 32-bit limits) and for count-like fields,
/// it comes from a small non-negative window instead.
fn fake_integer<R: Rng>(s: &Map<String, Value>, field: Option<&str>, rng: &mut R) -> Value {
    let num = |k: &str| s.get(k).and_then(Value::as_f64);
    let mut lo = num("minimum").map(f64::ceil);
    if let Some(x) = num("exclusiveMinimum") {
        let x = x.floor() + 1.0;
        lo = Some(lo.map_or(x, |l| l.max(x)));
    }
    let mut hi = num("maximum").map(f64::floor);
    if let Some(x) = num("exclusiveMaximum") {
        let x = x.ceil() - 1.0;
        hi = Some(hi.map_or(x, |h| h.min(x)));
    }
    const I32_MIN: f64 = -2_147_483_648.0;
    const I32_MAX: f64 = 2_147_483_647.0;
    let window = SMALL_INT_WINDOW as f64;
    let huge = match (lo, hi) {
        (Some(l), Some(h)) => h - l > 10_000.0 || l <= I32_MIN || h >= I32_MAX,
        _ => true,
    };
    let county = field.is_some_and(|f| {
        let f = f.to_ascii_lowercase();
        COUNT_SUFFIXES.iter().any(|suffix| f.ends_with(suffix))
    });
    let wide = match (lo, hi) {
        (Some(l), Some(h)) => h - l > window,
        _ => true,
    };
    let (w_lo, w_hi) = if huge || (county && wide) {
        let base = lo.unwrap_or(0.0).max(0.0);
        match hi {
            Some(h) if h < base => (lo.map_or(h - window, |l| l.max(h - window)), h),
            Some(h) => (base, h.min(base + window)),
            None => (base, base + window),
        }
    } else {
        (lo.unwrap_or(0.0), hi.unwrap_or(0.0))
    };
    let step = s
        .get("multipleOf")
        .and_then(Value::as_f64)
        .filter(|m| *m > 0.0)
        .unwrap_or(1.0);
    // Candidate multiples k*step within the window.
    let k_lo = (w_lo / step).ceil();
    let k_hi = (w_hi / step).floor();
    let k = if k_hi >= k_lo {
        let span = (k_hi - k_lo) as i64;
        k_lo + rng.random_range(0..=span) as f64
    } else {
        k_lo
    };
    Value::from((k * step) as i64)
}

fn fake_number<R: Rng>(s: &Map<String, Value>, rng: &mut R) -> Value {
    let b = bounds(s);
    let multiple = s
        .get("multipleOf")
        .and_then(Value::as_f64)
        .filter(|m| *m > 0.0);
    let inside = |x: f64| {
        (x > b.lo || (x == b.lo && !b.lo_exclusive)) && (x < b.hi || (x == b.hi && !b.hi_exclusive))
    };
    let value = if let Some(m) = multiple {
        let mut k_lo = (b.lo / m).ceil();
        if b.lo_exclusive && k_lo * m <= b.lo {
            k_lo += 1.0;
        }
        let mut k_hi = (b.hi / m).floor();
        if b.hi_exclusive && k_hi * m >= b.hi {
            k_hi -= 1.0;
        }
        let k_hi = k_hi.min(k_lo + 1000.0);
        let k = if k_hi >= k_lo {
            k_lo + rng.random_range(0..=((k_hi - k_lo) as i64)) as f64
        } else {
            k_lo
        };
        round_to(k * m, 10)
    } else {
        let u: f64 = rng.random_range(0.0..=1.0);
        let candidate = round_to(b.lo + (b.hi - b.lo) * u, 2);
        if inside(candidate) {
            candidate
        } else {
            (b.lo + b.hi) / 2.0
        }
    };
    Number::from_f64(value).map_or_else(|| json!(0), Value::Number)
}

fn round_to(x: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (x * f).round() / f
}
