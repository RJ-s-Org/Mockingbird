use rand::{Rng, RngExt};
use serde_json::{Map, Number, Value, json};

const WORDS: &[&str] = &[
    "alpha", "bravo", "cedar", "delta", "ember", "fjord", "garnet", "harbor", "iris", "juniper",
    "kestrel", "lumen", "meadow", "nectar", "onyx", "pebble", "quartz", "raven", "sable", "tundra",
];

/// Extra levels allowed beyond `max_depth` for schemas that cannot terminate
/// by omitting optional members (for example a required self-reference).
const HARD_DEPTH_SLACK: usize = 32;

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
        self.gen_value(schema_with_defs, schema_with_defs, 0, branch, rng)
    }

    fn gen_value<R: Rng>(
        &self,
        root: &Value,
        schema: &Value,
        depth: usize,
        branch: Option<usize>,
        rng: &mut R,
    ) -> Value {
        let at_limit = depth >= self.max_depth;
        // Absolute cutoff: past it, containers are emitted empty so that even
        // an unsatisfiable required self-reference terminates.
        let hard_limit = depth >= self.max_depth + HARD_DEPTH_SLACK;
        let pick = match branch {
            Some(i) => Pick::Forced(i),
            None if at_limit => Pick::Shallow,
            None => Pick::Random,
        };
        let flat = flatten(root, schema, 0, pick, rng);
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
        if at_limit && types.contains(&"null") {
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
            "integer" => fake_integer(s, rng),
            "number" => fake_number(s, rng),
            "string" => Value::String(fake_string(s, rng)),
            "array" if hard_limit => Value::Array(Vec::new()),
            "object" if hard_limit => Value::Object(Map::new()),
            "array" => self.fake_array(root, s, depth, at_limit, rng),
            "object" => self.fake_object(root, s, depth, at_limit, rng),
            _ => Value::String(word(rng)),
        }
    }

    fn fake_object<R: Rng>(
        &self,
        root: &Value,
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
                self.gen_value(root, sub, depth + 1, None, rng),
            );
        }
        if !at_limit && let Some(props) = props {
            for (name, sub) in props {
                if !out.contains_key(name) {
                    out.insert(
                        name.clone(),
                        self.gen_value(root, sub, depth + 1, None, rng),
                    );
                }
            }
        }
        Value::Object(out)
    }

    fn fake_array<R: Rng>(
        &self,
        root: &Value,
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
        let mut out: Vec<Value> = Vec::with_capacity(len);
        for _ in 0..len {
            let mut candidate = self.gen_value(root, items, depth + 1, None, rng);
            if unique {
                let mut tries = 0;
                while out.contains(&candidate) && tries < 32 {
                    candidate = self.gen_value(root, items, depth + 1, None, rng);
                    tries += 1;
                }
            }
            out.push(candidate);
        }
        Value::Array(out)
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
    /// The branch with the fewest required references, so that recursion
    /// past the depth limit converges on a leaf.
    Shallow,
}

/// Resolve `$ref` (against the root's `$defs`), and merge `allOf` and one
/// `oneOf`/`anyOf` branch (chosen by `pick`) into a single schema object.
fn flatten<R: Rng>(root: &Value, schema: &Value, hops: usize, pick: Pick, rng: &mut R) -> Value {
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
        extras.push(flatten(root, target, hops + 1, pick, rng));
    }
    if let Some(Value::Array(all)) = obj.get("allOf") {
        for sub in all {
            extras.push(flatten(root, sub, hops + 1, pick, rng));
        }
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(branches)) = obj.get(key)
            && !branches.is_empty()
        {
            let index = match pick {
                Pick::Forced(i) => i % branches.len(),
                Pick::Random => rng.random_range(0..branches.len()),
                Pick::Shallow => branches
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, b)| branch_cost(root, b, 0))
                    .map_or(0, |(i, _)| i),
            };
            extras.push(flatten(root, &branches[index], hops + 1, pick, rng));
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

/// How many references a minimal instance of `schema` must expand: one per
/// `$ref` in a required position (required properties, non-empty arrays),
/// with the schema's own top-level `$ref`/`allOf`/`oneOf`/`anyOf` followed.
/// Used to steer recursion towards terminating branches.
fn branch_cost(root: &Value, schema: &Value, hops: usize) -> usize {
    const UNBOUNDED: usize = 1 << 16;
    let Some(obj) = schema.as_object() else {
        return 0;
    };
    if hops > 8 {
        return UNBOUNDED;
    }
    let mut cost = 0usize;
    if let Some(Value::String(r)) = obj.get("$ref")
        && let Some(target) = resolve_ref(root, r)
    {
        cost = cost.saturating_add(branch_cost(root, target, hops + 1));
    }
    if let Some(Value::Array(all)) = obj.get("allOf") {
        for sub in all {
            cost = cost.saturating_add(branch_cost(root, sub, hops + 1));
        }
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(branches)) = obj.get(key)
            && let Some(min) = branches
                .iter()
                .map(|b| branch_cost(root, b, hops + 1))
                .min()
        {
            cost = cost.saturating_add(min);
        }
    }
    let member = |m: &Value| -> usize {
        if m.get("$ref").is_some() {
            1
        } else {
            branch_cost(root, m, hops + 1)
        }
    };
    let props = obj.get("properties").and_then(Value::as_object);
    if let Some(Value::Array(required)) = obj.get("required") {
        for name in required.iter().filter_map(Value::as_str) {
            if let Some(p) = props.and_then(|p| p.get(name)) {
                cost = cost.saturating_add(member(p));
            }
        }
    }
    if obj.get("minItems").and_then(Value::as_u64).unwrap_or(0) > 0
        && let Some(items) = obj.get("items")
    {
        cost = cost.saturating_add(member(items));
    }
    cost
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
    let max = s
        .get("maxLength")
        .and_then(Value::as_u64)
        .map(|m| m as usize);
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

fn fake_integer<R: Rng>(s: &Map<String, Value>, rng: &mut R) -> Value {
    let b = bounds(s);
    let step = s
        .get("multipleOf")
        .and_then(Value::as_f64)
        .filter(|m| *m > 0.0)
        .unwrap_or(1.0);
    // Candidate multiples k*step within the bounds.
    let mut k_lo = (b.lo / step).ceil();
    if b.lo_exclusive && k_lo * step <= b.lo {
        k_lo += 1.0;
    }
    let mut k_hi = (b.hi / step).floor();
    if b.hi_exclusive && k_hi * step >= b.hi {
        k_hi -= 1.0;
    }
    // Keep the choice inside i64 and a modest window for readable output.
    let k_hi = k_hi.min(k_lo + 1000.0);
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
