//! What the contract says of the members of a body the mock builds itself.
//!
//! A refusal (an LLM's, a repeated create's, a stale write's) and a `404` for what the
//! session does not hold are rendered from the contract's response for that status,
//! faker-filled. The faker draws a member it knows nothing more of at random, so an
//! error's `code` read `lorem` where the response's description says the status answers
//! `` `version_conflict` ``. After the faker, each scalar member of such a body takes the
//! value the contract gives it, by the first of these sources that has one:
//!
//! 1. its schema's `const`, or its `enum` when that holds one value;
//! 2. a value this status's response description names in code position (between
//!    backticks, or in `<code>`), when the description ties it to the member: the
//!    member's `enum` holds it and no other value the description names (a member
//!    with no `enum` whose own schema description lists values in code position, "Such
//!    as `ConcurrentModification` or `ResourceNotFound`", counts those as its enum), or the
//!    description names the member itself in code position (its name, or its dotted
//!    path such as `error.code`) and, after it in the same sentence and before the next
//!    member it names, one value only:
//!    `` `code` is `version_conflict` or `edit_conflict` `` names two, and is no guess.
//!    A text that is the name of a member of the body is no value (`` The `code` and
//!    `message` members `` names two members, not a code). The member may also be named as a plain word of the prose (`the code`, `Code`,
//!    whatever its case), and a member typed only as a number also takes a plain
//!    number of the prose (`code 1001`), never the response's own status;
//! 3. when the body is the mock's own refusal of a create for repeating a value of a
//!    member (a taken `sku`; never an LLM's refusal, which may be of anything), a value
//!    the contract's API-level description (`info.description`) names as in (2),
//!    within one of its sentences that holds this response's status as a number and no
//!    other, and names that repeated member, no other member of the request, and the
//!    type the call makes, singular or plural ("A product's `sku` is unique: a create
//!    with an SKU another product has is refused with 400 and the code
//!    `product_invalid_sku`"). Any other sentence names nothing for this body;
//! 4. its schema's example (`example`, else the first of `examples`, not a boolean's),
//!    only where no other status's response uses that schema: an example of a shared
//!    `Error` says nothing of this status.
//!
//! A body an LLM wrote, valid already, keeps what it chose: only its members left
//! empty (`null` or `""`) take a value. A member no source names keeps the faker's
//! value, and a number that is the response's own HTTP status, as the engine reflects
//! it into a problem's `status`, is never changed. Each change is kept only while the whole body still validates, so a value
//! that does not fit the branch the faker took changes nothing. Members are told apart by
//! structure alone: no name is special.

use std::collections::{HashMap, HashSet};

use mb_mcm::{Bundle, StatusSelector};
use serde_json::Value;

use crate::consistency::{declared_members, deref, example_value, property_schema};
use crate::validate::SchemaValidator;

/// How deep a body or a schema is walked.
const MAX_DEPTH: usize = 8;
/// Longest text in code position taken as a value.
const MAX_SPAN_CHARS: usize = 128;
/// Values a description names that are looked at, at most.
const MAX_SPANS: usize = 32;
/// Words of prose looked at, at most.
const MAX_WORDS: usize = 4096;
/// Definitions followed from one schema, at most.
const MAX_DEFS: usize = 256;

/// What the contract says around a body the mock builds (see the module's
/// documentation).
#[derive(Clone, Copy, Default)]
pub(crate) struct Prose<'a> {
    /// The description of the response for the body's status.
    pub description: Option<&'a str>,
    /// The contract's API-level description.
    pub info: Option<&'a str>,
    /// The request members whose repeated value the body refuses (source 3), if any.
    pub repeated: &'a [String],
    /// The type of resource the refused call makes.
    pub resource: Option<&'a str>,
    /// The top-level members of the refused call's request: a sentence naming another
    /// than the repeated one ties nothing (source 3).
    pub request: &'a [String],
    /// Only members the body leaves empty (`null` or `""`) take a value: the body was
    /// written by an LLM, valid already, and what it chose stays.
    pub only_empty: bool,
}

/// `body`, valid against `validator`, with each scalar member the contract names a
/// value for (see the module's documentation) holding that value. `schema` is that of
/// the response for `status`, and `prose` what the contract says around it. A body
/// that is not valid to begin with is left as it is.
pub(crate) fn fill(
    bundle: &Bundle,
    validator: &SchemaValidator,
    (schema, status): (&Value, u16),
    prose: Prose<'_>,
    body: Value,
) -> Value {
    if !validator.is_valid(&body) {
        return body;
    }
    let mut names = HashSet::new();
    property_names(bundle, schema, 0, &mut HashSet::new(), &mut names);
    let mut walk = Walk {
        bundle,
        said: said(prose, status),
        repeated: prose.repeated,
        names: &names,
        status,
        shared: None,
        changes: Vec::new(),
    };
    let alone = !walk.enters_shared(schema);
    walk.value(schema, &body, &mut String::new(), ("", ""), alone, 0);
    let mut out = body;
    for (pointer, value) in walk.changes {
        let mut candidate = out.clone();
        match candidate.pointer_mut(&pointer) {
            Some(slot) if prose.only_empty && !(slot.is_null() || *slot == "") => continue,
            Some(slot) => *slot = value,
            None => continue,
        }
        if validator.is_valid(&candidate) {
            out = candidate;
        }
    }
    out
}

/// What the status's description says (source 2), then what the sentences of the
/// API-level description that state this refusal say (source 3).
fn said(prose: Prose<'_>, status: u16) -> [Said; 2] {
    let described = prose.description.map(sentenced_tokens).unwrap_or_default();
    let info = prose
        .info
        .map(|text| {
            stating(
                sentenced_tokens(text),
                status,
                (prose.repeated, prose.resource, prose.request),
            )
        })
        .unwrap_or_default();
    [Said::new(described), Said::new(info)]
}

/// The dotted paths of the scalar members of `schema`, the response for `status`, that
/// the contract names a value for (see the module's documentation), whatever value a
/// body holds there: the members [`fill`] gives a value. A member in an array's items
/// has the array's path (`errors.title`).
pub(crate) fn named_paths(
    bundle: &Bundle,
    (schema, status): (&Value, u16),
    prose: Prose<'_>,
) -> HashSet<String> {
    let mut names = HashSet::new();
    property_names(bundle, schema, 0, &mut HashSet::new(), &mut names);
    let mut walk = Walk {
        bundle,
        said: said(prose, status),
        repeated: prose.repeated,
        names: &names,
        status,
        shared: None,
        changes: Vec::new(),
    };
    let alone = !walk.enters_shared(schema);
    let mut out = HashSet::new();
    walk.schema_paths(schema, "", alone, 0, &mut out);
    out
}

/// What a description names in code position, in order: the text between a pair of
/// backticks, and inside `<code>`, trimmed, when it is short and not empty.
#[must_use]
pub fn code_spans(text: &str) -> Vec<String> {
    sentenced_tokens(text)
        .into_iter()
        .filter_map(|(token, _)| match token {
            Token::Code(span) => Some(span),
            Token::Word(_) => None,
        })
        .collect()
}

/// A piece of a description: what it names in code position, or a word of its prose
/// (a run of letters, digits and `_`).
#[derive(Clone, Debug, PartialEq)]
enum Token {
    Code(String),
    Word(String),
}

/// What a description says, each piece with the number of its sentence, and its code
/// spans alone.
#[derive(Default)]
struct Said {
    tokens: Vec<(Token, usize)>,
    spans: Vec<(String, usize)>,
}

impl Said {
    fn new(tokens: Vec<(Token, usize)>) -> Self {
        let spans = tokens
            .iter()
            .filter_map(|(token, sentence)| match token {
                Token::Code(span) => Some((span.clone(), *sentence)),
                Token::Word(_) => None,
            })
            .collect();
        Self { tokens, spans }
    }
}

/// The next text in code position of `rest`: where it starts, what it holds, and what
/// follows it. `None` when there is none, or it is not closed.
fn next_code(rest: &str) -> Option<(usize, &str, &str)> {
    let tick = rest.find('`');
    let tag = rest.find("<code>");
    match (tick, tag) {
        (Some(t), g) if g.is_none_or(|g| t < g) => {
            let open = rest[t..].bytes().take_while(|b| *b == b'`').count();
            let fence = "`".repeat(open);
            let body = &rest[t + open..];
            let end = body.find(&fence)?;
            Some((t, &body[..end], &body[end + open..]))
        }
        (_, Some(g)) => {
            let body = &rest[g + "<code>".len()..];
            let end = body.find("</code>")?;
            Some((g, &body[..end], &body[end + "</code>".len()..]))
        }
        _ => None,
    }
}

/// What `text` names in code position and the words of its prose, in order, each with
/// the number of the sentence it stands in: a `.`, `!` or `?` outside code, followed
/// by a space or the end, ends one. Text after a code span left open is read as prose.
fn sentenced_tokens(text: &str) -> Vec<(Token, usize)> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut sentence = 0;
    let mut spans = 0;
    loop {
        let found = next_code(rest).filter(|_| spans < MAX_SPANS);
        let prose = found.map_or(rest, |(start, _, _)| &rest[..start]);
        sentence = prose_words(prose, sentence, &mut out);
        let Some((_, inner, after)) = found else {
            break;
        };
        let inner = inner.trim();
        if !inner.is_empty() && inner.chars().count() <= MAX_SPAN_CHARS {
            out.push((Token::Code(inner.to_string()), sentence));
            spans += 1;
        }
        rest = after;
    }
    out
}

/// Pushes the words of `prose` onto `out`, from sentence `sentence` on: the number of
/// the sentence it ends in.
fn prose_words(prose: &str, mut sentence: usize, out: &mut Vec<(Token, usize)>) -> usize {
    let chars: Vec<char> = prose.chars().collect();
    let mut word = String::new();
    for (i, c) in chars.iter().enumerate() {
        if c.is_alphanumeric() || *c == '_' {
            word.push(*c);
            continue;
        }
        if !word.is_empty() && out.len() < MAX_WORDS {
            out.push((Token::Word(std::mem::take(&mut word)), sentence));
        }
        word.clear();
        if matches!(c, '.' | '!' | '?') && chars.get(i + 1).is_none_or(|n| n.is_whitespace()) {
            sentence += 1;
        }
    }
    if !word.is_empty() && out.len() < MAX_WORDS {
        out.push((Token::Word(word), sentence));
    }
    sentence
}

/// Whether `token` names `member`: in code position as it is, or as a word of the prose
/// whatever its case.
fn names_member(token: &Token, member: &str) -> bool {
    match token {
        Token::Code(span) => span == member,
        Token::Word(word) => word.eq_ignore_ascii_case(member),
    }
}

/// The pieces of the sentences of `said` that hold `status` as a word, and no other
/// status, and name one of `repeated`, no other member of `request`, and the type
/// `resource` (source 3 of the module's documentation); none without a repeat or a type.
/// A sentence naming two statuses or two members of the request ties neither.
fn stating(
    said: Vec<(Token, usize)>,
    status: u16,
    (repeated, resource, request): (&[String], Option<&str>, &[String]),
) -> Vec<(Token, usize)> {
    let Some(resource) = resource.filter(|_| !repeated.is_empty()) else {
        return Vec::new();
    };
    let mut texts: HashMap<usize, String> = HashMap::new();
    for (token, sentence) in &said {
        let text = texts.entry(*sentence).or_default();
        text.push(' ');
        text.push_str(match token {
            Token::Code(span) | Token::Word(span) => span,
        });
    }
    let status_code = status;
    let stated: HashSet<usize> = texts
        .into_iter()
        .filter(|(_, text)| {
            crate::guards::status_numbers(text) == [status_code]
                && crate::guards::names_phrase(text, resource)
                && !request
                    .iter()
                    .filter(|m| !repeated.contains(m))
                    .any(|m| crate::guards::names_phrase(text, m))
                && repeated
                    .iter()
                    .any(|m| crate::guards::names_phrase(text, m))
        })
        .map(|(sentence, _)| sentence)
        .collect();
    said.into_iter()
        .filter(|(_, s)| stated.contains(s))
        .collect()
}

/// Every member name `schema` declares, at any depth, through `$ref`s and compositions.
fn property_names(
    bundle: &Bundle,
    schema: &Value,
    depth: usize,
    seen: &mut HashSet<String>,
    out: &mut HashSet<String>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    if let Some(name) = def_ref(schema) {
        if !seen.insert(name.to_string()) || seen.len() > MAX_DEFS {
            return;
        }
        if let Some(target) = bundle.defs.get(name) {
            property_names(bundle, target, depth + 1, seen, out);
        }
        return;
    }
    if let Some(Value::Object(properties)) = schema.get("properties") {
        for (name, inner) in properties {
            out.insert(name.clone());
            property_names(bundle, inner, depth + 1, seen, out);
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(inner) = schema.get(key).filter(|v| v.is_object()) {
            property_names(bundle, inner, depth + 1, seen, out);
        }
    }
    for key in ["allOf", "oneOf", "anyOf"] {
        for inner in schema
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            property_names(bundle, inner, depth + 1, seen, out);
        }
    }
}

/// The definition `schema` refers to, by name.
fn def_ref(schema: &Value) -> Option<&str> {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
}

/// Every definition `schema` reaches, by name.
fn reached_defs(bundle: &Bundle, schema: &Value, out: &mut HashSet<String>) {
    fn walk(bundle: &Bundle, value: &Value, out: &mut HashSet<String>, depth: usize) {
        if depth > 64 || out.len() > MAX_DEFS {
            return;
        }
        match value {
            Value::Object(members) => {
                if let Some(name) = def_ref(value)
                    && out.insert(name.to_string())
                    && let Some(target) = bundle.defs.get(name)
                {
                    walk(bundle, target, out, depth + 1);
                }
                for inner in members.values() {
                    walk(bundle, inner, out, depth + 1);
                }
            }
            Value::Array(items) => items.iter().for_each(|v| walk(bundle, v, out, depth + 1)),
            _ => {}
        }
    }
    walk(bundle, schema, out, 0);
}

/// The statuses whose responses reach each definition, across every operation: `None`
/// for a `default` response, which answers any.
fn statuses_by_def(bundle: &Bundle) -> HashMap<String, HashSet<Option<u16>>> {
    let mut out: HashMap<String, HashSet<Option<u16>>> = HashMap::new();
    for op in &bundle.operations {
        for variant in &op.outputs {
            let Some(schema) = variant.schema.as_ref() else {
                continue;
            };
            let status = match variant.status {
                StatusSelector::Code(code) => Some(code),
                StatusSelector::Default => None,
            };
            let mut defs = HashSet::new();
            reached_defs(bundle, schema, &mut defs);
            for def in defs {
                out.entry(def).or_default().insert(status);
            }
        }
    }
    out
}

struct Walk<'a> {
    bundle: &'a Bundle,
    /// What the status's description says (source 2), then what the sentences of the
    /// API-level description that state this refusal say (source 3).
    said: [Said; 2],
    /// The request members whose repeated value the body refuses: where the prose names
    /// one, it speaks of the request, not of a member of the body of that name.
    repeated: &'a [String],
    /// Every member name the body's schema declares.
    names: &'a HashSet<String>,
    /// The response's HTTP status.
    status: u16,
    /// [`statuses_by_def`], made when first needed.
    shared: Option<HashMap<String, HashSet<Option<u16>>>>,
    /// JSON pointers into the body, with the value the contract gives them.
    changes: Vec<(String, Value)>,
}

impl Walk<'_> {
    /// Whether a definition `schema` is, or composes, is used by a response of another
    /// status than this one.
    fn enters_shared(&mut self, schema: &Value) -> bool {
        let mut node = schema;
        let mut names = Vec::new();
        for _ in 0..16 {
            let Some(name) = def_ref(node) else { break };
            names.push(name.to_string());
            match self.bundle.defs.get(name) {
                Some(target) => node = target,
                None => break,
            }
        }
        for key in ["allOf", "oneOf", "anyOf"] {
            for inner in node
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = def_ref(inner) {
                    names.push(name.to_string());
                }
            }
        }
        if names.is_empty() {
            return false;
        }
        let bundle = self.bundle;
        let shared = self.shared.get_or_insert_with(|| statuses_by_def(bundle));
        let status = Some(self.status);
        names.iter().any(|name| {
            shared
                .get(name)
                .is_some_and(|statuses| statuses.iter().any(|s| *s != status))
        })
    }

    /// `value` at `pointer`, of `schema`: the member named and dotted as `at`, under
    /// examples this status alone uses when `alone`.
    fn value(
        &mut self,
        schema: &Value,
        value: &Value,
        pointer: &mut String,
        at: (&str, &str),
        alone: bool,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let (name, dotted) = at;
        match value {
            Value::Object(members) => {
                for (key, member) in members {
                    let Some(inner) = property_schema(self.bundle, schema, key) else {
                        continue;
                    };
                    let path = match dotted {
                        "" => key.clone(),
                        _ => format!("{dotted}.{key}"),
                    };
                    let alone = alone && !self.enters_shared(&inner);
                    let len = pointer.len();
                    pointer.push('/');
                    pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
                    self.value(&inner, member, pointer, (key, &path), alone, depth + 1);
                    pointer.truncate(len);
                }
            }
            Value::Array(items) => {
                let Some(inner) = deref(self.bundle, schema).get("items").cloned() else {
                    return;
                };
                let alone = alone && !self.enters_shared(&inner);
                for (i, item) in items.iter().enumerate() {
                    let len = pointer.len();
                    pointer.push('/');
                    pointer.push_str(&i.to_string());
                    self.value(&inner, item, pointer, at, alone, depth + 1);
                    pointer.truncate(len);
                }
            }
            scalar => {
                // What the engine reflects of the HTTP status stays.
                if scalar.as_u64() == Some(u64::from(self.status)) {
                    return;
                }
                if let Some(named) = self.named(schema, (name, dotted), alone)
                    && &named != scalar
                {
                    self.changes.push((pointer.clone(), named));
                }
            }
        }
    }

    /// Into `out`, the dotted paths below `dotted` of the members of `schema` the
    /// contract names a value for, under examples this status alone uses when `alone`
    /// ([`named_paths`]).
    fn schema_paths(
        &mut self,
        schema: &Value,
        dotted: &str,
        alone: bool,
        depth: usize,
        out: &mut HashSet<String>,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let mut members: Vec<String> = declared_members(self.bundle, schema)
            .into_iter()
            .flatten()
            .collect();
        members.sort();
        for key in members {
            let Some(inner) = property_schema(self.bundle, schema, &key) else {
                continue;
            };
            let path = match dotted {
                "" => key.clone(),
                _ => format!("{dotted}.{key}"),
            };
            let alone = alone && !self.enters_shared(&inner);
            if self.named(&inner, (&key, &path), alone).is_some() {
                out.insert(path.clone());
            }
            self.schema_paths(&inner, &path, alone, depth + 1, out);
        }
        if let Some(items) = deref(self.bundle, schema).get("items").cloned() {
            let alone = alone && !self.enters_shared(&items);
            self.schema_paths(&items, dotted, alone, depth + 1, out);
        }
    }

    /// The value the contract gives a scalar member of `schema` named and dotted as `at`
    /// (see the module's documentation).
    fn named(&self, schema: &Value, (name, dotted): (&str, &str), alone: bool) -> Option<Value> {
        let node = deref(self.bundle, schema);
        if let Some(fixed) = node.get("const") {
            return scalar(fixed);
        }
        let values = node.get("enum").and_then(Value::as_array);
        if let Some([only]) = values.map(Vec::as_slice) {
            return scalar(only);
        }
        // The values the member's own description lists in code position, when it has
        // no `enum`: as an enum's, the one the status's prose names is its value.
        let listed: Option<Vec<Value>> = values.is_none().then(|| {
            let own = node
                .get("description")
                .or_else(|| schema.get("description"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut out: Vec<Value> = Vec::new();
            for span in code_spans(own) {
                if self.names.contains(&span) {
                    continue;
                }
                if let Some(value) = typed(node, &span)
                    && !out.contains(&value)
                {
                    out.push(value);
                }
            }
            out
        });
        let listed = listed.filter(|l| !l.is_empty());
        for said in &self.said {
            if let Some(listed) = listed.as_ref()
                && let Some(one) = self.described(said, node, Some(listed), (name, dotted))
            {
                return Some(one);
            }
            if let Some(described) = self.described(said, node, values, (name, dotted)) {
                return Some(described);
            }
        }
        if alone && let Some(example) = example_value(self.bundle, node) {
            return scalar(&example);
        }
        None
    }

    /// Whether `token` names a member of the body: its name in code position, alone or
    /// ending a dotted path, or as a word of the prose whatever its case.
    fn is_member(&self, token: &Token) -> bool {
        match token {
            Token::Code(span) => {
                self.names.contains(span)
                    || span
                        .rsplit_once('.')
                        .is_some_and(|(_, last)| self.names.contains(last))
            }
            Token::Word(word) => self.names.iter().any(|n| n.eq_ignore_ascii_case(word)),
        }
    }

    /// What `said` names for the member (sources 2 and 3 of the module's
    /// documentation): one value, or none.
    fn described(
        &self,
        said: &Said,
        node: &Value,
        values: Option<&Vec<Value>>,
        (name, dotted): (&str, &str),
    ) -> Option<Value> {
        if said.tokens.is_empty() || name.is_empty() {
            return None;
        }
        if let Some(values) = values {
            let mut named = values
                .iter()
                .filter(|v| said.spans.iter().any(|(s, _)| matches_span(v, s)));
            return match (named.next(), named.next()) {
                (Some(one), None) => scalar(one),
                _ => None,
            };
        }
        let numeric = numeric(node);
        let status = self.status.to_string();
        let mut found: Vec<Value> = Vec::new();
        // Every value after the member's name, to the end of its sentence or the next
        // member named: a value belongs to the member named nearest before it.
        let mut owner: Option<(bool, usize)> = None;
        for (token, sentence) in &said.tokens {
            if owner.is_some_and(|(_, s)| s != *sentence) {
                owner = None;
            }
            if self.repeated.iter().any(|m| names_member(token, m)) {
                owner = Some((false, *sentence));
                continue;
            }
            let ours = match token {
                Token::Code(span) => span == name || span == dotted,
                Token::Word(_) => names_member(token, name),
            };
            if ours || self.is_member(token) {
                owner = Some((ours, *sentence));
                continue;
            }
            if owner.is_none_or(|(ours, _)| !ours) {
                continue;
            }
            let value = match token {
                Token::Code(value) => typed(node, value),
                // A plain number of the prose, for a number, other than the status.
                Token::Word(word)
                    if numeric && *word != status && word.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    typed(node, word)
                }
                Token::Word(_) => None,
            };
            if let Some(value) = value
                && !found.contains(&value)
            {
                found.push(value);
            }
        }
        match found.as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    }
}

/// `value` when it is a scalar a member can hold.
fn scalar(value: &Value) -> Option<Value> {
    (!value.is_object() && !value.is_array() && !value.is_null()).then(|| value.clone())
}

/// Whether `value` is the text `span` names.
fn matches_span(value: &Value, span: &str) -> bool {
    match value {
        Value::String(text) => text == span,
        Value::Number(n) => n.to_string() == span,
        _ => false,
    }
}

/// Whether `node` is typed only as a number or an integer.
fn numeric(node: &Value) -> bool {
    let types: Vec<&str> = match node.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    !types.is_empty()
        && !types.contains(&"string")
        && types.iter().any(|t| matches!(*t, "integer" | "number"))
}

/// The text `span` as a value of `node`'s type: a number for a member typed only as a
/// number or an integer, otherwise the text.
fn typed(node: &Value, span: &str) -> Option<Value> {
    if numeric(node) {
        let parsed: Value = serde_json::from_str(span).ok()?;
        return parsed.is_number().then_some(parsed);
    }
    Some(Value::String(span.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_spans_read_backticks_and_code_tags_in_order() {
        assert_eq!(
            code_spans("Set `code` to `version_conflict`; see <code>E42</code> and ``a`b``."),
            vec!["code", "version_conflict", "E42", "a`b"]
        );
        assert!(code_spans("no code here, and an `unclosed one").is_empty());
        assert!(code_spans("`  `").is_empty());
    }

    #[test]
    fn typed_spans_follow_the_member_type() {
        assert_eq!(
            typed(&serde_json::json!({"type": "integer"}), "409"),
            Some(409.into())
        );
        assert_eq!(typed(&serde_json::json!({"type": "integer"}), "x"), None);
        assert_eq!(
            typed(&serde_json::json!({"type": ["string", "null"]}), "409"),
            Some("409".into())
        );
    }
}
