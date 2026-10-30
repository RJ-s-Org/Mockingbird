//! JSON Schema validation of requests and generated output, and the local
//! patching of output that breaks only what a fresh value fixes.

use std::collections::HashSet;
use std::sync::OnceLock;

use jsonschema::error::ValidationErrorKind;
use serde_json::Value;

use crate::EngineError;
use crate::faker::Costs;

const MAX_ERRORS: usize = 10;

/// Violations [`SchemaValidator::validate`] orders by depth before keeping
/// [`MAX_ERRORS`].
const MAX_SCANNED: usize = 100;

/// A string longer than this, where the schema sets no `maxLength`, is a
/// runaway generation.
pub const RUNAWAY_CHARS: usize = 2_000;

/// What a runaway string is cut to.
pub const TRUNCATED_CHARS: usize = 200;

/// Schema nodes visited when looking up the schema of one location.
const MAX_SCHEMA_HOPS: usize = 64;

/// The `format`s a validator built by [`SchemaValidator::for_output`]
/// asserts: the ones the faker produces, so a generated value breaking one
/// can always be refilled. Every other format stays an annotation.
pub const ASSERTED_FORMATS: &[&str] = &[
    "uuid",
    "date-time",
    "date",
    "time",
    "email",
    "uri",
    "ipv4",
    "ipv6",
    "hostname",
];

/// The formats `jsonschema` knows besides [`ASSERTED_FORMATS`]: an output
/// validator overrides them with a check that accepts everything, so they
/// stay annotations and contracts using them do not start failing.
const ANNOTATION_FORMATS: &[&str] = &[
    "duration",
    "idn-email",
    "idn-hostname",
    "iri",
    "iri-reference",
    "json-pointer",
    "regex",
    "relative-json-pointer",
    "uri-reference",
    "uri-template",
];

/// The keywords [`mend`] checks a replacement string against.
const LEAF_KEYWORDS: &[&str] = &[
    "pattern",
    "minLength",
    "maxLength",
    "enum",
    "const",
    "format",
];

/// A bad identifier at least this long is replaced wherever the answer
/// repeats it as a whole token; shorter ones could match by chance.
const MIN_SHARED_CHARS: usize = 6;

/// Longer draws are used as they are: [`overlay`] checks once per character.
const MAX_OVERLAID_CHARS: usize = 256;

/// JSON Schema (draft 2020-12) validator over a schema that carries its own
/// `$defs`.
pub struct SchemaValidator {
    inner: jsonschema::Validator,
    schema: Value,
    /// [`SchemaValidator::declared_properties`], computed on first use.
    declared: OnceLock<Option<HashSet<String>>>,
    /// The faker's cost table of `schema`, computed on first use.
    costs: OnceLock<Costs>,
    /// Whether [`ASSERTED_FORMATS`] are checked by the output checks
    /// ([`SchemaValidator::is_valid_output`], [`SchemaValidator::patch`]).
    assert_formats: bool,
    /// The format-asserting validator of `schema`, compiled on first use
    /// (`None` when it does not compile, so formats stay annotations).
    formats: OnceLock<Option<jsonschema::Validator>>,
}

impl std::fmt::Debug for SchemaValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaValidator")
            .field("schema", &self.schema)
            .field("assert_formats", &self.assert_formats)
            .finish_non_exhaustive()
    }
}

impl SchemaValidator {
    /// A validator for which `format` is an annotation, as JSON Schema
    /// 2020-12 has it: the one for requests, whose values a mock must not
    /// reject over a format the real API may not check.
    ///
    /// # Errors
    ///
    /// [`EngineError::Schema`] when the schema does not compile.
    pub fn new(schema_with_defs: &Value) -> Result<Self, EngineError> {
        let inner = jsonschema::draft202012::new(schema_with_defs)
            .map_err(|e| EngineError::Schema(e.to_string()))?;
        Ok(Self {
            inner,
            schema: schema_with_defs.clone(),
            declared: OnceLock::new(),
            costs: OnceLock::new(),
            assert_formats: false,
            formats: OnceLock::new(),
        })
    }

    /// A validator for generated output: like [`SchemaValidator::new`], and
    /// its output checks ([`SchemaValidator::is_valid_output`],
    /// [`SchemaValidator::patch`]) also assert [`ASSERTED_FORMATS`]. A
    /// value breaking one is refilled by `patch`, never reported by
    /// [`SchemaValidator::validate`] (so it never costs an LLM repair
    /// round), and [`SchemaValidator::is_valid`] stays the annotation-only
    /// verdict.
    pub fn for_output(schema_with_defs: &Value) -> Result<Self, EngineError> {
        let mut validator = Self::new(schema_with_defs)?;
        validator.assert_formats = true;
        Ok(validator)
    }

    /// The format-asserting validator, when this one asserts formats.
    fn formats(&self) -> Option<&jsonschema::Validator> {
        if !self.assert_formats {
            return None;
        }
        self.formats
            .get_or_init(|| asserting_formats(&self.schema))
            .as_ref()
    }

    /// Whether `instance` is valid. Stops at the first error and formats
    /// none: use it instead of [`SchemaValidator::validate`] when only the
    /// verdict matters. `format` is an annotation here, even for a
    /// validator built by [`SchemaValidator::for_output`].
    pub fn is_valid(&self, instance: &Value) -> bool {
        self.inner.is_valid(instance)
    }

    /// [`SchemaValidator::is_valid`], also asserting [`ASSERTED_FORMATS`]
    /// when this validator was built by [`SchemaValidator::for_output`].
    pub fn is_valid_output(&self, instance: &Value) -> bool {
        self.formats().unwrap_or(&self.inner).is_valid(instance)
    }

    /// Whether every asserted format `instance` breaks is at a location
    /// `keep` accepts (always true when no format is asserted).
    pub(crate) fn formats_hold(
        &self,
        instance: &Value,
        keep: &impl Fn(&str, &Value) -> bool,
    ) -> bool {
        let Some(formats) = self.formats() else {
            return true;
        };
        if formats.is_valid(instance) {
            return true;
        }
        formats.iter_errors(instance).all(|e| {
            !matches!(e.kind(), ValidationErrorKind::Format { .. })
                || keep(e.instance_path().as_str(), e.instance())
        })
    }

    /// The schema (with its `$defs`) this validator checks against.
    pub(crate) fn schema(&self) -> &Value {
        &self.schema
    }

    /// The faker's cost table of [`SchemaValidator::schema`].
    pub(crate) fn costs(&self) -> &Costs {
        self.costs.get_or_init(|| Costs::compute(&self.schema))
    }

    /// `instance` with the errors a fresh value can fix patched locally: a
    /// `null` the schema forbids is removed where it is an optional property,
    /// else replaced by `fill` of the expected schema; a value failing
    /// `pattern`, `maxLength`, `minLength`, `enum`, `format`, `const` or a
    /// numeric range (`minimum`, `maximum`, their exclusive forms,
    /// `multipleOf`) is
    /// replaced from `fill` of the schema holding that keyword, keeping what
    /// fits of the old value (see [`mend`]). `fill` gets the
    /// schema with the root's `$defs`. `None` unless such errors are all that
    /// is wrong and the patched value is valid. `format` counts only for a
    /// validator built by [`SchemaValidator::for_output`], and only for
    /// [`ASSERTED_FORMATS`].
    pub fn patch(&self, instance: &Value, fill: impl FnMut(&Value) -> Value) -> Option<Value> {
        self.patch_keeping(instance, |_, _| false, fill)
    }

    /// [`SchemaValidator::patch`], keeping every value that breaks a format
    /// where `keep` (given its JSON pointer and the value) says so: a value
    /// the request sent (a path id echoed into a `format: uuid` field) is
    /// not overwritten. `keep` applies to format errors only.
    pub fn patch_keeping(
        &self,
        instance: &Value,
        keep: impl Fn(&str, &Value) -> bool,
        mut fill: impl FnMut(&Value) -> Value,
    ) -> Option<Value> {
        self.patch_nodes(instance, keep, |node| fill(&self.with_root_defs(node)))
    }

    /// [`SchemaValidator::patch_keeping`], with `fill` given the schema node
    /// inside [`SchemaValidator::schema`] (its `$ref`s resolve against that
    /// root) instead of a copy that carries the root's `$defs`.
    pub(crate) fn patch_nodes(
        &self,
        instance: &Value,
        keep: impl Fn(&str, &Value) -> bool,
        mut fill: impl FnMut(&Value) -> Value,
    ) -> Option<Value> {
        let mut nulls = Vec::new();
        let mut refills = Vec::new();
        // The annotation-only validator never reports a format; the
        // asserting one adds the format errors, and only those.
        if let Some(formats) = self.formats()
            && !formats.is_valid(instance)
        {
            for e in formats.iter_errors(instance) {
                if !matches!(e.kind(), ValidationErrorKind::Format { .. }) {
                    continue;
                }
                let pointer = e.instance_path().as_str().to_string();
                if keep(&pointer, e.instance()) {
                    continue;
                }
                refills.push((pointer, self.subschema(e.evaluation_path().as_str())?));
            }
        }
        for e in self.inner.iter_errors(instance) {
            self.collect(&e, &mut nulls, &mut refills)?;
        }
        if nulls.is_empty() && refills.is_empty() {
            return None;
        }
        let mut out = instance.clone();
        // Shortest first: a value that embeds another (a secret holding its
        // id) then finds that one's replacement.
        refills.sort_by_key(|(pointer, _)| {
            instance
                .pointer(pointer)
                .and_then(Value::as_str)
                .map_or(usize::MAX, |text| text.chars().count())
        });
        let mut replaced = Vec::new();
        for (pointer, expected) in &refills {
            let bad = out.pointer(pointer)?.clone();
            *out.pointer_mut(pointer)? = mend(&bad, expected, &mut replaced, &mut fill);
        }
        for (pointer, _) in &nulls {
            let (parent, key) = pointer.rsplit_once('/')?;
            if let Some(Value::Object(map)) = out.pointer_mut(parent) {
                map.remove(&unescape(key));
            }
        }
        let mut missing = HashSet::new();
        for e in self.inner.iter_errors(&out) {
            required_members(&e, &mut missing);
        }
        for (pointer, expected) in &nulls {
            let dropped = out.pointer(pointer).is_none();
            if dropped && !missing.contains(pointer) {
                continue;
            }
            let value = fill(expected);
            let (parent, key) = pointer.rsplit_once('/')?;
            match out.pointer_mut(parent)? {
                Value::Object(map) => {
                    map.insert(unescape(key), value);
                }
                Value::Array(items) => *items.get_mut(key.parse::<usize>().ok()?)? = value,
                _ => return None,
            }
        }
        // A value left as it was may hold a replaced one (a URL holding an id).
        let refilled: HashSet<&str> = refills.iter().map(|(p, _)| p.as_str()).collect();
        let mut linked = out.clone();
        carry(&mut linked, String::new(), &replaced, &|pointer, value| {
            refilled.contains(pointer) || keep(pointer, value)
        });
        let out = if self.is_valid(&linked) && self.formats_hold(&linked, &keep) {
            linked
        } else {
            out
        };
        (self.is_valid(&out) && self.formats_hold(&out, &keep)).then_some(out)
    }

    /// Adds what `e` needs patched to `nulls` and `refills`; `None` when a
    /// local fix cannot solve it. A failed `anyOf` or `oneOf` is patched in
    /// the branch needing the fewest fixes, among those whose `const` and
    /// `enum` members already hold: changing one would change what the
    /// answer is.
    fn collect<'s>(
        &'s self,
        e: &jsonschema::ValidationError<'_>,
        nulls: &mut Vec<(String, &'s Value)>,
        refills: &mut Vec<(String, &'s Value)>,
    ) -> Option<()> {
        let pointer = e.instance_path().as_str().to_string();
        let expected = || self.subschema(e.evaluation_path().as_str());
        match e.kind() {
            ValidationErrorKind::Type { .. } if e.instance().is_null() => {
                nulls.push((pointer, expected()?));
            }
            ValidationErrorKind::Pattern { .. }
            | ValidationErrorKind::MaxLength { .. }
            | ValidationErrorKind::MinLength { .. }
            | ValidationErrorKind::Enum { .. }
            | ValidationErrorKind::Constant { .. }
            | ValidationErrorKind::Minimum { .. }
            | ValidationErrorKind::Maximum { .. }
            | ValidationErrorKind::ExclusiveMinimum { .. }
            | ValidationErrorKind::ExclusiveMaximum { .. }
            | ValidationErrorKind::MultipleOf { .. } => refills.push((pointer, expected()?)),
            ValidationErrorKind::AnyOf { context }
            | ValidationErrorKind::OneOfNotValid { context } => {
                let mut best: Option<(Vec<_>, Vec<_>)> = None;
                for branch in context {
                    let discriminated = branch.iter().any(|e| {
                        matches!(
                            e.kind(),
                            ValidationErrorKind::Constant { .. } | ValidationErrorKind::Enum { .. }
                        )
                    });
                    let (mut branch_nulls, mut branch_refills) = (Vec::new(), Vec::new());
                    if discriminated
                        || !branch.iter().all(|e| {
                            self.collect(e, &mut branch_nulls, &mut branch_refills)
                                .is_some()
                        })
                    {
                        continue;
                    }
                    let fixes = branch_nulls.len() + branch_refills.len();
                    if best.as_ref().is_none_or(|(n, r)| fixes < n.len() + r.len()) {
                        best = Some((branch_nulls, branch_refills));
                    }
                }
                let (branch_nulls, branch_refills) = best?;
                nulls.extend(branch_nulls);
                refills.extend(branch_refills);
            }
            _ => return None,
        }
        Some(())
    }

    /// `instance` with every missing required property that `wanted`
    /// accepts set to `fill` of its schema (with the root's `$defs`).
    /// `wanted` gets the location of the object schema (a JSON pointer where
    /// a `$ref` restarts at its target, e.g. `/$defs/user`) and the
    /// property. `None` when nothing was filled.
    pub fn fill_required(
        &self,
        instance: &Value,
        wanted: impl FnMut(&str, &str) -> bool,
        mut fill: impl FnMut(&Value) -> Value,
    ) -> Option<Value> {
        self.fill_required_nodes(instance, wanted, |node| fill(&self.with_root_defs(node)))
    }

    /// [`SchemaValidator::fill_required`], with `fill` given the schema node
    /// inside [`SchemaValidator::schema`] (see
    /// [`SchemaValidator::patch_nodes`]).
    pub(crate) fn fill_required_nodes(
        &self,
        instance: &Value,
        mut wanted: impl FnMut(&str, &str) -> bool,
        mut fill: impl FnMut(&Value) -> Value,
    ) -> Option<Value> {
        let mut out = instance.clone();
        let mut filled = false;
        // A filled value may itself lack a member the next round fills.
        for _ in 0..4 {
            let mut missing = Vec::new();
            for e in self.inner.iter_errors(&out) {
                let ValidationErrorKind::Required { property } = e.kind() else {
                    continue;
                };
                let Some(property) = property.as_str() else {
                    continue;
                };
                let Some(location) = self.location(e.evaluation_path().as_str()) else {
                    continue;
                };
                if !wanted(&location, property) {
                    continue;
                }
                let Some(schema) = self.at(&format!("{location}/properties/{}", escape(property)))
                else {
                    continue;
                };
                missing.push((
                    e.instance_path().as_str().to_string(),
                    property.to_string(),
                    schema,
                ));
            }
            if missing.is_empty() {
                break;
            }
            for (pointer, property, schema) in missing {
                if let Some(Value::Object(map)) = out.pointer_mut(&pointer)
                    && !map.contains_key(&property)
                {
                    map.insert(property, fill(schema));
                    filled = true;
                }
            }
        }
        filled.then_some(out)
    }

    /// The location of the schema holding the keyword that failed at
    /// `evaluation_path`: a JSON pointer into the schema that restarts at a
    /// `$ref`'s target.
    fn location(&self, evaluation_path: &str) -> Option<String> {
        let segments: Vec<String> = evaluation_path.split('/').skip(1).map(unescape).collect();
        let (_keyword, path) = segments.split_last()?;
        let mut location = String::new();
        let mut node = &self.schema;
        for segment in path {
            if segment == "$ref" {
                let target = node.get("$ref")?.as_str()?.strip_prefix('#')?;
                location = target.to_string();
                node = self.schema.pointer(target)?;
            } else {
                node = match node {
                    Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
                    _ => node.get(segment)?,
                };
                location.push('/');
                location.push_str(&escape(segment));
            }
        }
        Some(location)
    }

    /// The property names the root object schema declares, following
    /// `$ref`s and the branches of `allOf`/`anyOf`/`oneOf`; `None` when it
    /// declares none (a free-form object).
    pub fn declared_properties(&self) -> Option<HashSet<String>> {
        self.declared_property_names().cloned()
    }

    /// [`SchemaValidator::declared_properties`], borrowed: computed once
    /// per validator.
    pub fn declared_property_names(&self) -> Option<&HashSet<String>> {
        self.declared
            .get_or_init(|| self.compute_declared_properties())
            .as_ref()
    }

    fn compute_declared_properties(&self) -> Option<HashSet<String>> {
        let mut names = HashSet::new();
        let mut pending = vec![&self.schema];
        let mut hops = 0;
        while let Some(node) = pending.pop() {
            hops += 1;
            if hops > MAX_SCHEMA_HOPS {
                break;
            }
            if let Some(target) = node
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix('#'))
                .and_then(|pointer| self.schema.pointer(pointer))
            {
                pending.push(target);
            }
            if let Some(Value::Object(props)) = node.get("properties") {
                names.extend(props.keys().cloned());
            }
            for keyword in ["allOf", "anyOf", "oneOf"] {
                if let Some(Value::Array(branches)) = node.get(keyword) {
                    pending.extend(branches.iter());
                }
            }
        }
        (!names.is_empty()).then_some(names)
    }

    /// A copy of `node` (a schema inside the root) carrying the root's
    /// `$defs`, so it stands on its own.
    fn with_root_defs(&self, node: &Value) -> Value {
        let mut node = node.clone();
        if let (Value::Object(map), Some(defs)) = (&mut node, self.schema.get("$defs")) {
            map.insert("$defs".into(), defs.clone());
        }
        node
    }

    /// The schema at `location`.
    fn at(&self, location: &str) -> Option<&Value> {
        self.schema.pointer(location)
    }

    /// The schema whose keyword failed at `evaluation_path` (which follows
    /// `$ref`s).
    fn subschema(&self, evaluation_path: &str) -> Option<&Value> {
        let segments: Vec<String> = evaluation_path.split('/').skip(1).map(unescape).collect();
        let (_keyword, path) = segments.split_last()?;
        let mut node = &self.schema;
        for segment in path {
            node = match segment.as_str() {
                "$ref" => {
                    let name = node.get("$ref")?.as_str()?.strip_prefix("#/$defs/")?;
                    self.schema.get("$defs")?.get(name)?
                }
                _ => match node {
                    Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
                    _ => node.get(segment)?,
                },
            };
        }
        Some(node)
    }

    /// `instance` with every string longer than [`RUNAWAY_CHARS`] whose
    /// schema location declares no `maxLength` cut to its first
    /// [`TRUNCATED_CHARS`] characters; `None` unless something was cut and
    /// the result validates.
    pub fn truncate_runaway(&self, instance: &Value) -> Option<Value> {
        let mut long = Vec::new();
        collect_long_strings(instance, String::new(), &mut long);
        let mut out = instance.clone();
        let mut cut = false;
        for pointer in long {
            if self.declares_max_length(&pointer) {
                continue;
            }
            if let Some(Value::String(text)) = out.pointer_mut(&pointer) {
                *text = text.chars().take(TRUNCATED_CHARS).collect();
                cut = true;
            }
        }
        (cut && self.is_valid(&out)).then_some(out)
    }

    /// Whether any schema reachable for the instance location `pointer`
    /// (through `$ref`, `allOf`, `oneOf`, `anyOf`) declares `maxLength`.
    fn declares_max_length(&self, pointer: &str) -> bool {
        let mut nodes = self.expand(vec![&self.schema]);
        for segment in pointer.split('/').skip(1).map(unescape) {
            let index = segment.parse::<usize>().ok();
            let next: Vec<&Value> = nodes
                .iter()
                .filter_map(|node| {
                    let property = node.get("properties").and_then(|p| p.get(&segment));
                    let additional = node.get("additionalProperties").filter(|a| a.is_object());
                    let item = index.and_then(|i| {
                        node.get("prefixItems")
                            .and_then(|p| p.get(i))
                            .or_else(|| node.get("items"))
                    });
                    property.or(item).or(additional)
                })
                .collect();
            nodes = self.expand(next);
            if nodes.is_empty() {
                return false;
            }
        }
        nodes.iter().any(|node| node.get("maxLength").is_some())
    }

    /// `nodes` and every schema they reach through `$ref` (to `$defs`),
    /// `allOf`, `oneOf` and `anyOf`, bounded.
    fn expand<'a>(&'a self, mut pending: Vec<&'a Value>) -> Vec<&'a Value> {
        let mut out = Vec::new();
        while let Some(node) = pending.pop() {
            if out.len() >= MAX_SCHEMA_HOPS {
                break;
            }
            out.push(node);
            if let Some(target) = node
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/$defs/"))
                .and_then(|name| self.schema.get("$defs")?.get(name))
            {
                pending.push(target);
            }
            for key in ["allOf", "oneOf", "anyOf"] {
                if let Some(Value::Array(members)) = node.get(key) {
                    pending.extend(members);
                }
            }
        }
        out
    }

    /// Returns up to 10 violations formatted as `"<instance path>:
    /// <keyword>: <message>"`, shallowest first (of the first 100 found): a
    /// wrong shape near the root is what a repair must fix first. A failed
    /// `anyOf`/`oneOf` names the errors of its closest branch; quoted values
    /// are capped.
    pub fn validate(&self, instance: &Value) -> Result<(), Vec<String>> {
        let mut found: Vec<_> = self.inner.iter_errors(instance).take(MAX_SCANNED).collect();
        // Stable: errors at one depth keep the validator's order.
        found.sort_by_key(|e| e.instance_path().as_str().matches('/').count());
        let errors: Vec<String> = found
            .iter()
            .take(MAX_ERRORS)
            .map(|e| describe(e, 0))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Longest message quoted for one violation; a longer one (it quotes a big
/// value) is told with the value masked.
const MAX_MESSAGE_CHARS: usize = 160;

/// Branch errors named for a failed `anyOf`/`oneOf`.
const MAX_BRANCH_ERRORS: usize = 3;

/// Nesting of `anyOf`/`oneOf` followed into a branch.
const MAX_BRANCH_DEPTH: usize = 4;

/// The validator of `schema` asserting [`ASSERTED_FORMATS`]; `None` when it
/// does not compile.
fn asserting_formats(schema: &Value) -> Option<jsonschema::Validator> {
    let mut options = jsonschema::draft202012::options().should_validate_formats(true);
    for name in ANNOTATION_FORMATS {
        options = options.with_format(*name, |_: &str| true);
    }
    options.build(schema).ok()
}

/// The replacement for `bad`, which breaks a keyword of `node`, keeping as
/// much of what the LLM wrote as still fits: replacements made earlier in
/// this answer (`replaced`) carry over, a value too long is cut, and a drawn
/// value takes the characters of `bad` that its pattern or length allows.
fn mend(
    bad: &Value,
    node: &Value,
    replaced: &mut Vec<(String, String)>,
    fill: &mut impl FnMut(&Value) -> Value,
) -> Value {
    let (Value::String(old), Value::Object(keywords)) = (bad, node) else {
        return fill(node);
    };
    let leaf: serde_json::Map<String, Value> = keywords
        .iter()
        .filter(|(k, _)| LEAF_KEYWORDS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let Some(leaf) = asserting_formats(&Value::Object(leaf)) else {
        return fill(node);
    };
    let fits = |text: &str| leaf.is_valid(&Value::String(text.to_string()));

    let mut text = old.clone();
    for (from, to) in replaced.iter() {
        text = swap(&text, from, to);
    }
    let mended = if text != *old && fits(&text) {
        text
    } else if let Some(cut) = keywords
        .get("maxLength")
        .and_then(Value::as_u64)
        .map(|max| text.chars().take(max as usize).collect::<String>())
        .filter(|cut| *cut != text && fits(cut))
    {
        cut
    } else {
        match fill(node) {
            Value::String(drawn)
                if keywords.contains_key("pattern") || keywords.contains_key("minLength") =>
            {
                overlay(&text, &drawn, &fits)
            }
            Value::String(drawn) => drawn,
            other => return other,
        }
    };
    // Identifiers only: a word or a number under a pattern also appears
    // with another meaning elsewhere.
    let identifier = keywords.contains_key("pattern")
        || keywords.get("format").and_then(Value::as_str) == Some("uuid");
    if identifier
        && old.chars().count() >= MIN_SHARED_CHARS
        && old.chars().any(char::is_alphabetic)
        && old.chars().any(|c| c.is_ascii_digit())
    {
        replaced.push((old.clone(), mended.clone()));
    }
    Value::String(mended)
}

/// Adds the JSON pointer of every member `e` reports missing, also inside
/// the branches of a failed `anyOf` or `oneOf`.
fn required_members(e: &jsonschema::ValidationError<'_>, out: &mut HashSet<String>) {
    match e.kind() {
        ValidationErrorKind::Required { property } => {
            if let Some(name) = property.as_str() {
                out.insert(format!("{}/{}", e.instance_path().as_str(), escape(name)));
            }
        }
        ValidationErrorKind::AnyOf { context } | ValidationErrorKind::OneOfNotValid { context } => {
            for e in context.iter().flatten() {
                required_members(e, out);
            }
        }
        _ => {}
    }
}

/// `value` with every replaced identifier swapped in its strings, except
/// where `skip` says so (given the JSON pointer and the value).
fn carry(
    value: &mut Value,
    pointer: String,
    replaced: &[(String, String)],
    skip: &impl Fn(&str, &Value) -> bool,
) {
    if skip(&pointer, value) {
        return;
    }
    match value {
        Value::String(text) => {
            for (from, to) in replaced {
                *text = swap(text, from, to);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                carry(item, format!("{pointer}/{i}"), replaced, skip);
            }
        }
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                carry(item, format!("{pointer}/{}", escape(key)), replaced, skip);
            }
        }
        _ => {}
    }
}

/// `text` with each occurrence of `from` that is a whole token (no letter or
/// digit touches it) replaced by `to`.
fn swap(text: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (at, _) in text.match_indices(from) {
        let end = at + from.len();
        let touches = text[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
            || text[end..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric);
        if !touches {
            out.push_str(&text[copied..at]);
            out.push_str(to);
            copied = end;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// `drawn` with each character replaced by the one at the same place in
/// `text`, wherever the result still `fits`.
fn overlay(text: &str, drawn: &str, fits: &impl Fn(&str) -> bool) -> String {
    let mut chars: Vec<char> = drawn.chars().collect();
    if chars.len() > MAX_OVERLAID_CHARS {
        return drawn.to_string();
    }
    for (i, c) in text.chars().take(chars.len()).enumerate() {
        let drawn_char = std::mem::replace(&mut chars[i], c);
        if drawn_char != c && !fits(&chars.iter().collect::<String>()) {
            chars[i] = drawn_char;
        }
    }
    chars.into_iter().collect()
}

/// `"<pointer>: <keyword>: <message>"`; for a failed `anyOf`/`oneOf`, the
/// errors of the branch that got furthest into the value (the deepest
/// pointer, then the fewest errors).
fn describe(e: &jsonschema::ValidationError<'_>, depth: usize) -> String {
    let pointer = e.instance_path().as_str();
    let keyword = e.kind().keyword();
    if let ValidationErrorKind::AnyOf { context } | ValidationErrorKind::OneOfNotValid { context } =
        e.kind()
        && depth < MAX_BRANCH_DEPTH
        && let Some(branch) = context.iter().filter(|b| !b.is_empty()).max_by_key(|b| {
            let deepest = b
                .iter()
                .map(|e| e.instance_path().as_str().matches('/').count())
                .max()
                .unwrap_or(0);
            (deepest, std::cmp::Reverse(b.len()))
        })
    {
        let mut parts: Vec<String> = branch
            .iter()
            .take(MAX_BRANCH_ERRORS)
            .map(|e| describe(e, depth + 1))
            .collect();
        if branch.len() > MAX_BRANCH_ERRORS {
            parts.push(format!("{} more", branch.len() - MAX_BRANCH_ERRORS));
        }
        return format!(
            "{pointer}: {keyword}: no branch matches; closest: {}",
            parts.join(", ")
        );
    }
    let mut message = e.to_string();
    if message.chars().count() > MAX_MESSAGE_CHARS {
        message = e.masked().to_string();
    }
    if message.chars().count() > MAX_MESSAGE_CHARS {
        message = message.chars().take(MAX_MESSAGE_CHARS).collect::<String>() + "…";
    }
    format!("{pointer}: {keyword}: {message}")
}

/// Pointers of every string in `value` longer than [`RUNAWAY_CHARS`].
fn collect_long_strings(value: &Value, pointer: String, out: &mut Vec<String>) {
    match value {
        Value::String(text) if text.chars().count() > RUNAWAY_CHARS => out.push(pointer),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                collect_long_strings(item, format!("{pointer}/{i}"), out);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                collect_long_strings(item, format!("{pointer}/{}", escape(key)), out);
            }
        }
        _ => {}
    }
}

fn unescape(segment: &str) -> String {
    segment.replace("~1", "/").replace("~0", "~")
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn validator() -> SchemaValidator {
        SchemaValidator::new(&json!({
            "$ref": "#/$defs/Movie",
            "$defs": {
                "Movie": {
                    "type": "object",
                    "required": ["title", "images"],
                    "properties": {
                        "title": {"type": "string"},
                        "collection": {"$ref": "#/$defs/Collection"},
                        "images": {"type": "array", "items": {"type": "string"}}
                    }
                },
                "Collection": {"type": "object", "properties": {"id": {"type": "integer"}}}
            }
        }))
        .unwrap()
    }

    fn fill(schema: &Value) -> Value {
        match schema.get("type").and_then(Value::as_str) {
            Some("string") => json!("filled"),
            Some("object") => json!({}),
            _ => json!(0),
        }
    }

    #[test]
    fn optional_nulls_are_dropped_and_required_ones_filled() {
        let v = validator();
        let out = v
            .patch(
                &json!({"title": null, "collection": null, "images": ["a", null]}),
                fill,
            )
            .unwrap();
        assert_eq!(out, json!({"title": "filled", "images": ["a", "filled"]}));
    }

    #[test]
    fn a_ref_target_is_filled_from_its_definition() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "required": ["c"],
            "properties": {"c": {"$ref": "#/$defs/C"}},
            "$defs": {"C": {"type": "object", "required": ["id"], "properties": {"id": {"type": "integer"}}}}
        }))
        .unwrap();
        let out = v
            .patch(&json!({"c": null}), |s| {
                assert!(s.get("$defs").is_some(), "{s}");
                json!({"id": 1})
            })
            .unwrap();
        assert_eq!(out, json!({"c": {"id": 1}}));
    }

    #[test]
    fn wanted_missing_members_are_filled_through_refs() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "required": ["items"],
            "properties": {"items": {"type": "array", "items": {"$ref": "#/$defs/Item"}}},
            "$defs": {
                "Item": {"type": "object", "required": ["sha", "author"], "properties": {
                    "sha": {"type": "string"},
                    "author": {"$ref": "#/$defs/User"}
                }},
                "User": {"type": "object", "required": ["login"], "properties": {"login": {"type": "string"}}}
            }
        }))
        .unwrap();
        let mut seen = Vec::new();
        let out = v
            .fill_required(
                &json!({"items": [{"sha": "a"}, {"sha": "b"}]}),
                |loc, p| {
                    seen.push(format!("{loc}#{p}"));
                    loc == "/$defs/Item" && p == "author"
                },
                |s| {
                    assert!(s.get("$defs").is_some());
                    json!({"login": "x"})
                },
            )
            .unwrap();
        assert_eq!(out["items"][1]["author"], json!({"login": "x"}));
        assert!(v.validate(&out).is_ok());
        assert!(seen.contains(&"/$defs/Item#author".to_string()), "{seen:?}");
        // Members the caller did not leave out stay missing.
        assert!(
            v.fill_required(
                &json!({"items": [{"author": {"login": "x"}}]}),
                |_, p| p == "author",
                |_| json!(1)
            )
            .is_none()
        );
    }

    #[test]
    fn other_errors_are_not_patched() {
        let v = validator();
        assert!(
            v.patch(&json!({"title": 1, "collection": null, "images": []}), fill)
                .is_none()
        );
        assert!(
            v.patch(&json!({"title": "t", "images": []}), fill)
                .is_none(),
            "nothing to patch"
        );
    }

    #[test]
    fn pattern_length_enum_and_const_errors_are_refilled() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "pattern": "^/x$"},
                "code": {"type": "string", "maxLength": 2},
                "kind": {"enum": ["a", "b"]},
                "v": {"const": 1}
            }
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"path": "/y", "code": "long", "kind": "c", "v": 2}),
                |s| {
                    s.get("const")
                        .or_else(|| s.get("enum").and_then(|e| e.get(0)))
                        .cloned()
                        .unwrap_or_else(|| {
                            if s.get("pattern").is_some() {
                                json!("/x")
                            } else {
                                json!("ok")
                            }
                        })
                },
            )
            .unwrap();
        assert_eq!(
            out,
            json!({"path": "/x", "code": "lo", "kind": "a", "v": 1})
        );
    }

    /// Draws a different value on every call, as the faker does.
    fn drawing(values: &[&str]) -> impl FnMut(&Value) -> Value {
        let mut draws: Vec<Value> = values.iter().rev().map(|v| json!(v)).collect();
        move |_| draws.pop().unwrap_or(json!("spent"))
    }

    #[test]
    fn a_broken_value_inside_one_of_is_patched_in_its_nearest_branch() {
        let v = SchemaValidator::new(&json!({
            "oneOf": [
                {"type": "object", "required": ["kind", "n"], "properties": {
                    "kind": {"const": "count"}, "n": {"type": "integer"}}},
                {"type": "object", "required": ["kind", "code"], "properties": {
                    "kind": {"const": "text"}, "code": {"type": "string", "pattern": "^[a-z]{2}$"}}}
            ]
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"kind": "text", "code": "TOOLONG"}),
                drawing(&["xy"]),
            )
            .unwrap();
        assert_eq!(out["kind"], "text");
        assert!(v.is_valid(&out), "{out}");
    }

    #[test]
    fn a_broken_value_inside_any_of_is_patched_at_depth() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {"pay": {"anyOf": [
                {"type": "null"},
                {"$ref": "#/$defs/Card"}
            ]}},
            "$defs": {"Card": {"type": "object", "properties": {
                "last4": {"type": "string", "pattern": "^[0-9]{4}$"}}}}
        }))
        .unwrap();
        let out = v
            .patch(&json!({"pay": {"last4": "x"}}), drawing(&["1234"]))
            .unwrap();
        assert_eq!(out, json!({"pay": {"last4": "1234"}}));
    }

    #[test]
    fn a_branch_with_an_unpatchable_error_is_not_chosen() {
        let v = SchemaValidator::new(&json!({
            "oneOf": [{"type": "integer"}, {"type": "boolean"}]
        }))
        .unwrap();
        assert_eq!(v.patch(&json!("nope"), drawing(&["x"])), None);
    }

    #[test]
    fn the_same_bad_value_gets_the_same_replacement() {
        let id = json!({"type": "string", "pattern": "^id_[0-9]{4}$"});
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {"id": id, "copy": id}
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"id": "bad_value1", "copy": "bad_value1"}),
                drawing(&["id_1111", "id_2222"]),
            )
            .unwrap();
        assert_eq!(out["id"], out["copy"], "{out}");
    }

    #[test]
    fn a_replaced_value_is_carried_into_values_that_embed_it() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "pattern": "^pi_[a-z0-9]{6}$"},
                "secret": {"type": "string", "pattern": "^pi_[a-z0-9]{6}_secret_[a-z0-9]{4}$"}
            }
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"id": "pi_harbor1", "secret": "pi_harbor1_secret_abcd"}),
                drawing(&["pi_aaaaaa", "pi_bbbbbb_secret_cccc"]),
            )
            .unwrap();
        assert_eq!(
            out["secret"].as_str().unwrap(),
            format!("{}_secret_abcd", out["id"].as_str().unwrap())
        );
    }

    #[test]
    fn a_value_breaking_a_pattern_keeps_the_characters_that_fit() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {"id": {"type": "string", "pattern": "^U_kgDO[A-Za-z0-9]{8}$"}}
        }))
        .unwrap();
        let out = v
            .patch(&json!({"id": "U_kgDOabc"}), drawing(&["U_kgDOZZZZZZZZ"]))
            .unwrap();
        assert_eq!(out["id"], "U_kgDOabcZZZZZ");
    }

    #[test]
    fn a_value_over_max_length_is_trimmed() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {"code": {"type": "string", "maxLength": 3}}
        }))
        .unwrap();
        let out = v
            .patch(&json!({"code": "abcdef"}), drawing(&["zz"]))
            .unwrap();
        assert_eq!(out["code"], "abc");
    }

    #[test]
    fn branch_choice_does_not_flip_a_discriminator() {
        let v = SchemaValidator::new(&json!({
            "oneOf": [
                {"type": "object", "properties": {"kind": {"const": "count"}}},
                {"type": "object", "properties": {
                    "kind": {"const": "text"},
                    "code": {"type": "string", "pattern": "^[a-z]{2}$"},
                    "sub": {"type": "string", "pattern": "^[0-9]$"}}}
            ]
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"kind": "text", "code": "TOOLONG", "sub": "x"}),
                drawing(&["1", "ab"]),
            )
            .unwrap();
        assert_eq!(out["kind"], "text");
        assert_eq!(out["sub"], "1");
    }

    #[test]
    fn a_null_for_a_member_a_branch_requires_is_refilled() {
        let v = SchemaValidator::new(&json!({
            "oneOf": [
                {"type": "object", "required": ["last4"], "properties": {
                    "kind": {"const": "card"}, "last4": {"type": "string"}}},
                {"type": "object", "required": ["iban"], "properties": {
                    "kind": {"const": "bank"}, "iban": {"type": "string"}}}
            ]
        }))
        .unwrap();
        let out = v
            .patch(&json!({"kind": "card", "last4": null}), drawing(&["1234"]))
            .unwrap();
        assert_eq!(out, json!({"kind": "card", "last4": "1234"}));
    }

    #[test]
    fn an_unconstrained_string_follows_a_replaced_identifier() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "pattern": "^pi_[0-9]{4}$"},
                "url": {"type": "string"}
            }
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"id": "pi_harbor1", "url": "/intents/pi_harbor1"}),
                drawing(&["pi_1111"]),
            )
            .unwrap();
        assert_eq!(
            out["url"].as_str().unwrap(),
            format!("/intents/{}", out["id"].as_str().unwrap())
        );
    }

    #[test]
    fn short_or_enum_values_are_not_carried_between_fields() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "a": {"type": "string", "pattern": "^[0-9]{3}$"},
                "b": {"type": "string", "pattern": "^[0-9]{3}$"},
                "state": {"enum": ["open", "shut"]},
                "note": {"type": "string"}
            }
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"a": "n/a", "b": "n/a", "state": "closed", "note": "closed early"}),
                drawing(&["111", "222", "shut"]),
            )
            .unwrap();
        assert_ne!(out["a"], out["b"]);
        assert_eq!(out["note"], "closed early");
    }

    #[test]
    fn a_branch_with_another_discriminator_is_never_patched_into() {
        let required = SchemaValidator::new(&json!({"oneOf": [
            {"type": "object", "properties": {"kind": {"const": "count"}}},
            {"type": "object", "required": ["code"], "properties": {"kind": {"const": "text"}}}
        ]}))
        .unwrap();
        assert_eq!(
            required.patch(&json!({"kind": "text", "msg": "hello"}), drawing(&["x"])),
            None
        );
        let nested = SchemaValidator::new(&json!({"oneOf": [
            {"oneOf": [
                {"type": "object", "properties": {"kind": {"const": "cat"}}},
                {"type": "object", "properties": {"kind": {"const": "dog"}}}
            ]},
            {"type": "object", "properties": {
                "kind": {"const": "bird"},
                "code": {"type": "string", "pattern": "^[a-z]{2}$"}}}
        ]}))
        .unwrap();
        let out = nested
            .patch(
                &json!({"kind": "bird", "code": "TOOLONG"}),
                drawing(&["ab"]),
            )
            .unwrap();
        assert_eq!(out["kind"], "bird");
    }

    #[test]
    fn replacing_a_word_leaves_ordinary_text_alone() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "status": {"type": "string", "pattern": "^[A-Z]+$"},
                "note": {"type": "string"},
                "prev": {"type": "string"}
            }
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"status": "active", "note": "was active", "prev": "inactive"}),
                drawing(&["QXZ"]),
            )
            .unwrap();
        assert_eq!(out["note"], "was active");
        assert_eq!(out["prev"], "inactive");
    }

    #[test]
    fn an_identifier_inside_a_longer_token_is_left_alone() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "pattern": "^x_[0-9]{4}$"},
                "other": {"type": "string"}
            }
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"id": "pi_abc1", "other": "pi_abc12 and pi_abc1"}),
                drawing(&["x_1111"]),
            )
            .unwrap();
        assert_eq!(out["other"], "pi_abc12 and x_1111");
    }

    #[test]
    fn a_value_extended_from_the_old_one_is_carried_once() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "pattern": "^pi_[a-z0-9]{8,30}$"},
                "url": {"type": "string"}
            }
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"id": "pi_abc1", "url": "/x/pi_abc1"}),
                drawing(&["pi_zzzzzzzz"]),
            )
            .unwrap();
        assert_eq!(
            out["url"].as_str().unwrap(),
            format!("/x/{}", out["id"].as_str().unwrap())
        );
    }

    #[test]
    fn a_value_keep_accepts_is_not_rewritten_by_a_replacement() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "pattern": "^pi_[a-z]{3}$"},
                "query": {"type": "string"}
            }
        }))
        .unwrap();
        let out = v
            .patch_keeping(
                &json!({"id": "harbor-1", "query": "harbor-1"}),
                |_, value| value == "harbor-1",
                drawing(&["pi_abc"]),
            )
            .unwrap();
        assert_eq!(out["query"], "harbor-1");
    }

    #[test]
    fn an_optional_null_holding_a_union_is_dropped() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "pay": {"anyOf": [{"type": "object"}, {"type": "string"}]},
                "note": {"$ref": "#/$defs/Note"}
            },
            "$defs": {"Note": {"type": "string"}}
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"pay": null, "note": null}),
                drawing(&["FAKE", "FAKE"]),
            )
            .unwrap();
        assert_eq!(out, json!({}));
    }

    #[test]
    fn words_and_numbers_under_a_pattern_are_not_carried() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "status": {"type": "string", "pattern": "^[A-Z_]+$"},
                "zip": {"type": "string", "pattern": "^[0-9]{5}$"},
                "note": {"type": "string"},
                "qty": {"type": "string"}
            }
        }))
        .unwrap();
        let out = v
            .patch(
                &json!({"status": "in_progress", "zip": "100001",
                        "note": "still in_progress", "qty": "100001"}),
                drawing(&["10000", "QX_"]),
            )
            .unwrap();
        assert_eq!(out["note"], "still in_progress");
        assert_eq!(out["qty"], "100001");
    }

    #[test]
    fn an_optional_null_inside_a_union_is_dropped() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {"pay": {"anyOf": [
                {"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "string"}}},
                {"type": "string"}
            ]}}
        }))
        .unwrap();
        let out = v
            .patch(&json!({"pay": {"a": null, "b": "x"}}), drawing(&["FAKE"]))
            .unwrap();
        assert_eq!(out, json!({"pay": {"b": "x"}}));
    }

    #[test]
    fn errors_name_the_deepest_branch_pointer_and_keyword() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "owner": {"anyOf": [{"type": "null"}, {"$ref": "#/$defs/User"}]}
            },
            "$defs": {
                "User": {
                    "type": "object",
                    "required": ["login"],
                    "properties": {"login": {"type": "string"}, "id": {"type": "integer"}}
                }
            }
        }))
        .unwrap();
        let big = "x".repeat(5_000);
        let errors = v
            .validate(&json!({"owner": {"login": 7, "id": 1, "bio": big}}))
            .unwrap_err();
        assert_eq!(errors.len(), 1, "{errors:?}");
        let e = &errors[0];
        assert!(e.starts_with("/owner: anyOf: "), "{e}");
        assert!(
            e.contains("/owner/login: type: 7 is not of type \"string\""),
            "{e}"
        );
        assert!(e.len() < 400, "{e}");
    }

    #[test]
    fn is_valid_agrees_with_validate_and_declared_names_are_kept() {
        let v = validator();
        for value in [
            json!({"title": "t", "images": []}),
            json!({"title": 1, "images": []}),
            json!({"title": "t"}),
            json!(null),
        ] {
            assert_eq!(v.is_valid(&value), v.validate(&value).is_ok(), "{value}");
        }
        let names = v.declared_property_names().unwrap();
        assert_eq!(v.declared_properties().as_ref(), Some(names));
        assert!(names.contains("collection"), "{names:?}");
        let free = SchemaValidator::new(&json!({"type": "object"})).unwrap();
        assert!(free.declared_property_names().is_none());
        assert!(free.declared_properties().is_none());
    }

    #[test]
    fn long_values_are_not_quoted() {
        let v = SchemaValidator::new(&json!({"type": "integer"})).unwrap();
        let errors = v.validate(&json!({"text": "y".repeat(5_000)})).unwrap_err();
        assert!(errors[0].starts_with(": type: "), "{errors:?}");
        assert!(errors[0].len() < 300, "{errors:?}");
    }
}
