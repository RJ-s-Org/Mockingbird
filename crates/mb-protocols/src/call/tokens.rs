//! Optimistic concurrency: a write sending a version that is not its record's current
//! one is refused before any LLM call, as the API would.
//!
//! A *token* is a top-level member of a stored record, an integer or a string with no
//! `enum` or `const`, and no identity or path member. A write ties one to a refusal only
//! when it declares a `409` or a `412`, in one of these ways:
//!
//! - **By the contract's words**: the `409`/`412`'s description names the member in code
//!   position (`` `version` ``), and the server manages it (the read marks it `readOnly`,
//!   or no create or update of the type lets a client set it). Its body is compared.
//! - **By `If-Match`**: the header's entity-tag list (RFC 9110, quoted tags kept whole,
//!   compared by opaque value: `W/"3"` is `"3"`) holds the record's value of one string
//!   member the server manages, as above. That pairs `If-Match` with the member, for the
//!   record.
//! - **By another header the write declares**, holding (whole) the value of one member
//!   the server manages: a *candidate* pairing. It pairs only once the client was seen to
//!   track the member as it moves: a later write whose header holds the member's moved
//!   value. A header that stays the same while the member moves drops the candidate.
//! - **By the body or the query**: the write declares the member, no create of the
//!   type sets it, and the request echoes its held value; and either the write requires
//!   it as the only member of the record its input declares (the `{version, actions}`
//!   shape), or the client was seen to track it: a later write sent back the value it
//!   moved to. A `readOnly` member alone ties nothing (a computed total a full PUT echoes
//!   is none). An integer one is stale only when lower than the current value.
//!
//! No member's date, time or wording is read beyond that.
//!
//! Once an update the model answered was seen to change a token (or, for `If-Match`,
//! once the mock advanced it, below), a write sending a stale value is answered with the
//! declared `409`/`412` (a body's token the `409` first, a header's the `412` first),
//! its error body filled from the contract, before any LLM call, and nothing changes. A
//! record's first write is never refused this way, nor is a request sending no token or
//! `If-Match: *`, nor a write to a record nothing stores, nor a header pairing not yet
//! seen tracked, nor an operation declaring neither status.
//!
//! A token the request sends no new value for (it sends none, or the held one) is
//! *advanced* by the update: the model's new value (another string, a greater integer)
//! is stored, even for a `readOnly` member, where the settling of an update otherwise
//! keeps the copy's, and the request's echo does not stand over it (see
//! [`mb_engine::UpdateGuard::advances`]). When the model left an `If-Match` token as it
//! was, it is *minted* anew in the same format: its trailing number one higher, or else
//! each letter and digit redrawn in its class (hex staying hex), `W/` and quotes kept.
//! No other token is minted. The `If-Match` token is also served in a declared `ETag`
//! response header.
//!
//! Which tokens were seen to change, and which headers pair (or may pair) with which
//! member, is kept per record under the store's reserved type
//! ([`mb_state::INTERNAL_TYPE`]), so every backend and node sees the same, sessions, TTLs
//! and resets clear it, a session keeps [`MAX_RECORDS`] records' worth, the newest, and
//! any delete of the record ends it. A document is read, changed and written back: two
//! writes to one record at once can lose a change, which only leaves a stale write to
//! the model.
//!
//! HTTP, an LLM and `consistency: on` only, with no scenario or forced status:
//! everything else is as it was.

use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::*;

/// Records whose tokens a session keeps, the newest.
const MAX_RECORDS: usize = 1024;
/// The document listing them, oldest first, and the prefix of their ids.
const INDEX: &str = "#tokens";
/// Longest token value compared, as text.
const MAX_VALUE_CHARS: usize = 256;
/// The standard precondition header (RFC 9110 §13.1.1), lowercase.
const IF_MATCH: &str = "if-match";

/// What a write found of its record's tokens, for its answer and for after it.
#[derive(Default)]
pub(super) struct Tokens {
    /// The tokens the update advances.
    pub(super) advancing: Vec<String>,
    /// Those of `advancing` minted anew when the model leaves them as they were.
    mintable: Vec<String>,
    /// The tokens an update was seen to change, as read.
    confirmed: Vec<String>,
    /// Which header pairs with which member (lowercase header names), as read.
    headers: Map<String, Value>,
    /// Candidate pairings, `{header: {member, value}}`, as read.
    seen: Map<String, Value>,
    /// `headers` after this write.
    headers_after: Map<String, Value>,
    /// `seen` after this write.
    seen_after: Map<String, Value>,
    /// Body tokens the client was seen to track (it sent a moved value back), as read.
    tracked: Vec<String>,
    /// `tracked` after this write.
    tracked_after: Vec<String>,
    /// Whether the record has a document of its tokens.
    kept: bool,
}

/// Whether this call keeps and honours tokens.
pub(super) fn applies(rt: &MockRuntime, plan: &CallPlan<'_>) -> bool {
    rt.consistency
        && plan.engine.has_provider()
        && plain(plan)
        && matches!(plan.op.binding, Binding::Http { .. })
        && matches!(
            plan.op.kind,
            OperationKind::Command(Effect::Update | Effect::Delete)
        )
}

/// A string or an integer as text.
fn as_text(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        _ => return None,
    };
    (text.chars().count() <= MAX_VALUE_CHARS).then_some(text)
}

/// `text` as a token is compared by: an entity tag's weak prefix and quotes dropped,
/// so `W/"3"`, `"3"` and `3` are one.
fn normalized(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_prefix("W/").unwrap_or(text);
    text.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(text)
        .to_string()
}

/// A value as a token is compared by (see [`normalized`]).
fn compared(value: &Value) -> Option<String> {
    as_text(value).map(|t| normalized(&t))
}

/// `schema` with `$ref`s to the bundle's definitions followed (bounded).
fn deref_schema<'a>(bundle: &'a mb_mcm::Bundle, mut schema: &'a Value) -> &'a Value {
    for _ in 0..16 {
        let Some(target) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|name| bundle.defs.get(name))
        else {
            break;
        };
        schema = target;
    }
    schema
}

/// Whether `schema` marks itself `readOnly`, through `$ref`s and `allOf`s.
fn read_only(bundle: &mb_mcm::Bundle, schema: &Value, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    let node = deref_schema(bundle, schema);
    if schema.get("readOnly").and_then(Value::as_bool) == Some(true)
        || node.get("readOnly").and_then(Value::as_bool) == Some(true)
    {
        return true;
    }
    node.get("allOf")
        .and_then(Value::as_array)
        .is_some_and(|parts| parts.iter().any(|p| read_only(bundle, p, depth + 1)))
}

/// Whether object `schema` lists `member` as required, through `$ref`s and `allOf`s.
fn requires(bundle: &mb_mcm::Bundle, schema: &Value, member: &str, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    let node = deref_schema(bundle, schema);
    node.get("required")
        .and_then(Value::as_array)
        .is_some_and(|r| r.iter().any(|m| m.as_str() == Some(member)))
        || node
            .get("allOf")
            .and_then(Value::as_array)
            .is_some_and(|parts| parts.iter().any(|p| requires(bundle, p, member, depth + 1)))
}

/// Whether `op` declares a `409` or a `412` by code.
fn declares_conflict(op: &Operation) -> bool {
    op.outputs.iter().any(|variant| {
        matches!(
            variant.status,
            mb_mcm::StatusSelector::Code(409) | mb_mcm::StatusSelector::Code(412)
        )
    })
}

/// The schema of member `member` of a stored `t`'s read, when it may be a token by its
/// shape: an integer or a string with no `enum` or `const`, no identity or path member,
/// and (unless `dated` allows it) no date or time.
fn token_schema(rt: &MockRuntime, t: &str, member: &str, dated: bool) -> Option<Value> {
    let index = rt.index();
    if index.identity_fields(t).iter().any(|f| f == member)
        || index.path_fields(t).iter().any(|f| f == member)
    {
        return None;
    }
    let bundle = rt.bundle();
    let schema = write_memory::read_schema(rt, t)
        .and_then(|s| mb_engine::consistency::property_schema(bundle, s, member))?;
    let node = deref_schema(bundle, &schema);
    if node.get("enum").is_some() || node.get("const").is_some() {
        return None;
    }
    let types: Vec<&str> = match node.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let is_dated = node
        .get("format")
        .and_then(Value::as_str)
        .is_some_and(|f| matches!(f, "date-time" | "date" | "time"));
    let shaped = types.iter().any(|t| matches!(*t, "integer" | "string"));
    (shaped && (dated || !is_dated)).then_some(schema)
}

/// Whether `op` ties member `member` of a stored `t` to a refusal by its words (see the
/// module's documentation).
fn tied(rt: &MockRuntime, op: &Operation, t: &str, member: &str) -> bool {
    let named = op.outputs.iter().any(|variant| {
        matches!(
            variant.status,
            mb_mcm::StatusSelector::Code(409) | mb_mcm::StatusSelector::Code(412)
        ) && variant.description.as_deref().is_some_and(|d| {
            mb_engine::contract_values::code_spans(d)
                .iter()
                .any(|s| s == member)
        })
    });
    if !named {
        return false;
    }
    let Some(schema) = token_schema(rt, t, member, true) else {
        return false;
    };
    // Server-managed: the record's read says the client does not set it, or no write of
    // the type lets it.
    read_only(rt.bundle(), &schema, 0) || !settable_by(rt, t, member, true)
}

/// Whether some create (or, with `updates`, update) of `t` declares `member` as one its
/// client sets: in its body schema, not `readOnly`, or as a query parameter.
fn settable_by(rt: &MockRuntime, t: &str, member: &str, updates: bool) -> bool {
    let bundle = rt.bundle();
    bundle.operations.iter().any(|op| {
        op.resource_type.as_deref() == Some(t)
            && match op.kind {
                OperationKind::Command(Effect::Create) => true,
                OperationKind::Command(Effect::Update) => updates,
                _ => false,
            }
            && (op
                .input
                .body
                .as_ref()
                .and_then(|s| mb_engine::consistency::property_schema(bundle, s, member))
                .is_some_and(|input| !read_only(bundle, &input, 0))
                || op
                    .input
                    .query
                    .as_ref()
                    .and_then(|s| mb_engine::consistency::property_schema(bundle, s, member))
                    .is_some())
    })
}

/// The top-level member names object `schema` declares, through `$ref`s and `allOf`s.
fn properties_of(bundle: &mb_mcm::Bundle, schema: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > 8 {
        return;
    }
    let node = deref_schema(bundle, schema);
    if let Some(Value::Object(props)) = node.get("properties") {
        for name in props.keys() {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
    }
    for part in node
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        properties_of(bundle, part, depth + 1, out);
    }
}

/// Whether the server manages member `member` of a stored `t` that may be a token by
/// its shape: the read marks it `readOnly`, or no create or update of the type sets it.
fn server_managed(rt: &MockRuntime, t: &str, member: &str) -> bool {
    token_schema(rt, t, member, false).is_some_and(|schema| {
        read_only(rt.bundle(), &schema, 0) || !settable_by(rt, t, member, true)
    })
}

/// Whether `If-Match` on `op` may hold member `member` of a stored `t`: `op` declares a
/// `409` or a `412`, and the member is a string the server manages.
fn if_match_member(rt: &MockRuntime, op: &Operation, t: &str, member: &str) -> bool {
    declares_conflict(op)
        && server_managed(rt, t, member)
        && token_schema(rt, t, member, false).is_some_and(|schema| {
            let node = deref_schema(rt.bundle(), &schema);
            match node.get("type") {
                Some(Value::String(t)) => t == "string",
                Some(Value::Array(ts)) => ts.iter().any(|t| t.as_str() == Some("string")),
                _ => false,
            }
        })
}

/// Whether another request header of `op` may hold member `member` of a stored `t`:
/// `op` declares a `409` or a `412`, and the server manages the member.
fn header_member(rt: &MockRuntime, op: &Operation, t: &str, member: &str) -> bool {
    declares_conflict(op) && server_managed(rt, t, member)
}

/// Whether `op` may carry member `member` of a stored `t` as a token in its body or its
/// query (see the module's documentation): it declares a `409` or a `412` and the
/// member, which no create sets, and that the read marks `readOnly` or that has the
/// token's shape ([`body_shaped`]). A `readOnly` member alone ties nothing: it is a
/// token once [`body_shaped`] or once the client was seen to track it.
fn body_candidate(rt: &MockRuntime, op: &Operation, t: &str, member: &str) -> bool {
    if !declares_conflict(op) {
        return false;
    }
    let Some(read) = token_schema(rt, t, member, false) else {
        return false;
    };
    let bundle = rt.bundle();
    let declares =
        |s: &&Value| mb_engine::consistency::property_schema(bundle, s, member).is_some();
    let body = op.input.body.as_ref().filter(declares);
    let query = op.input.query.as_ref().filter(declares);
    if body.is_none() && query.is_none() {
        return false;
    }
    if settable_by(rt, t, member, false) {
        return false;
    }
    read_only(bundle, &read, 0) || body_shaped(rt, op, t, member)
}

/// Whether `op`'s input requires member `member` of a stored `t` as the only member of
/// the record it declares (the `{version, actions}` shape).
fn body_shaped(rt: &MockRuntime, op: &Operation, t: &str, member: &str) -> bool {
    let bundle = rt.bundle();
    let declares =
        |s: &&Value| mb_engine::consistency::property_schema(bundle, s, member).is_some();
    let body = op.input.body.as_ref().filter(declares);
    let query = op.input.query.as_ref().filter(declares);
    let required = [body, query]
        .into_iter()
        .flatten()
        .any(|s| requires(bundle, s, member, 0));
    // The only member of the record the write's input declares.
    let mut record = Vec::new();
    if let Some(schema) = write_memory::read_schema(rt, t) {
        properties_of(bundle, schema, 0, &mut record);
    }
    let mut input = Vec::new();
    for schema in [op.input.body.as_ref(), op.input.query.as_ref()]
        .into_iter()
        .flatten()
    {
        properties_of(bundle, schema, 0, &mut input);
    }
    let shared: Vec<&String> = input.iter().filter(|m| record.contains(m)).collect();
    required && shared.len() == 1 && shared[0] == member
}

/// Whether a body token `sent` is stale against `now`: for an integer one, lower; for
/// any other, another value.
fn stale(sent: &Value, now: &Value) -> bool {
    if let Some(now) = now.as_i64() {
        let sent = match sent {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        };
        if let Some(sent) = sent {
            return sent < now;
        }
    }
    compared(sent) != compared(now)
}

/// The entity tags of an `If-Match` value (RFC 9110 §8.8.3), each by its opaque value: a
/// quoted tag is kept whole, commas and all. None for `*`, which any current
/// representation matches, or for an empty list.
fn entity_tags(text: &str) -> Option<Vec<String>> {
    let mut tags = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        rest = rest.trim_start_matches(|c: char| c == ',' || c.is_whitespace());
        if rest.is_empty() {
            break;
        }
        if rest.starts_with('*') {
            return None;
        }
        let body = rest.strip_prefix("W/").unwrap_or(rest);
        if let Some(quoted) = body.strip_prefix('"') {
            let end = quoted.find('"').unwrap_or(quoted.len());
            tags.push(quoted[..end].to_string());
            rest = quoted.get(end + 1..).unwrap_or("");
        } else {
            let end = body.find(',').unwrap_or(body.len());
            tags.push(body[..end].trim().to_string());
            rest = &body[end..];
        }
    }
    tags.retain(|t| !t.is_empty());
    (!tags.is_empty()).then_some(tags)
}

/// The request headers `op` declares that `call` sends, lowercase: `If-Match` with its
/// entity tags (none for `*`), any other with its whole value, trimmed.
fn sent_headers(op: &Operation, call: &OperationCall) -> Vec<(String, Vec<String>)> {
    let Some(Value::Object(declared)) = op.input.headers.as_ref().and_then(|s| s.get("properties"))
    else {
        return Vec::new();
    };
    declared
        .keys()
        .filter_map(|name| {
            let text = call
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .and_then(|(_, v)| as_text(v))?;
            let lower = name.to_ascii_lowercase();
            let values = if lower == IF_MATCH {
                entity_tags(&text)?
            } else {
                let whole = text.trim().to_string();
                if whole.is_empty() {
                    return None;
                }
                vec![whole]
            };
            Some((lower, values))
        })
        .collect()
}

/// Whether header `header`'s `values` hold `held`: an entity tag by its opaque value for
/// `If-Match`, the whole text for any other.
fn holds(header: &str, values: &[String], held: &Value) -> bool {
    let held = if header == IF_MATCH {
        compared(held)
    } else {
        as_text(held)
    };
    held.is_some_and(|held| values.contains(&held))
}

/// The document of `t` `id`'s tokens.
fn key(t: &str, id: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [t, id] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let hash: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{INDEX}/{hash}")
}

/// What a write `plan` to `target` decides of its tokens, before any generation: the
/// refusal of a stale one (see the module's documentation), or what its answer
/// advances.
pub(super) async fn check(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target: &StoredResource,
) -> Result<Tokens, GeneratedResponse> {
    let rt = state.rt;
    let op = plan.op;
    let t = target.resource_type.as_str();
    let doc = state
        .get(&plan.session, INTERNAL_TYPE, &key(t, &target.id))
        .await;
    let part = |name: &str| doc.as_ref().and_then(|d| d.data.get(name));
    let confirmed: Vec<String> = part("confirmed")
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default();
    let object = |name: &str| {
        part(name)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    let (pairs, seen) = (object("headers"), object("seen"));
    let tracked_body: Vec<String> = part("tracked")
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default();
    let body = match call.body.as_ref() {
        Some(Value::Object(members)) => Some(members),
        _ => None,
    };
    let current = |member: &str| target.data.get(member).filter(|v| as_text(v).is_some());
    let headers = sent_headers(op, call);
    let is_confirmed = |member: &str| confirmed.iter().any(|m| m == member);

    let mut codes: &[u16] = &[];
    // The body: a token tied by words or by evidence; the query: by evidence only.
    let sent_members = body.into_iter().flatten().map(|(m, v)| (m, v, true)).chain(
        call.query
            .iter()
            .filter(|(m, _)| body.is_none_or(|b| !b.contains_key(*m)))
            .map(|(m, v)| (m, v, false)),
    );
    for (member, sent, in_body) in sent_members {
        let Some(now) = current(member) else {
            continue;
        };
        if compared(sent).is_none() || !is_confirmed(member) {
            continue;
        }
        let by_words = in_body && tied(rt, op, t, member) && compared(sent) != compared(now);
        let by_evidence = (body_shaped(rt, op, t, member) || tracked_body.contains(member))
            && body_candidate(rt, op, t, member)
            && stale(sent, now);
        if by_words || by_evidence {
            codes = &[409, 412];
            break;
        }
    }
    // A header: only a pairing seen tracked.
    if codes.is_empty() {
        for (header, values) in &headers {
            let Some(member) = pairs.get(header).and_then(Value::as_str) else {
                continue;
            };
            let Some(now) = current(member) else {
                continue;
            };
            let gate = match header.as_str() {
                IF_MATCH => if_match_member(rt, op, t, member),
                _ => header_member(rt, op, t, member),
            };
            if gate && is_confirmed(member) && !holds(header, values, now) {
                codes = &[412, 409];
                break;
            }
        }
    }
    if let Some(refusal) = codes
        .iter()
        .find_map(|code| plan.engine.refuse_from_state(rt.bundle(), op, call, *code))
    {
        tracing::info!(
            mock = %rt.name,
            operation = %op.id,
            status = refusal.status,
            "write refused before generation: it sent a stale concurrency token"
        );
        return Err(refusal);
    }

    // What the update advances: a token its body or query sets no new value for.
    let mut advancing: Vec<String> = Vec::new();
    let mut mintable: Vec<String> = Vec::new();
    let (mut headers_after, mut seen_after) = (pairs.clone(), seen.clone());
    let mut tracked_after = tracked_body.clone();
    if op.kind == OperationKind::Command(Effect::Update)
        && let Value::Object(held) = &target.data
    {
        // The client sets it anew: it is the client's.
        let set_anew = |member: &str| {
            body.and_then(|b| b.get(member))
                .or_else(|| call.query.get(member))
                .is_some_and(|s| compared(s) != current(member).and_then(compared))
        };
        let mut advance = |member: &str, mint: bool, advancing: &mut Vec<String>| {
            if !set_anew(member) && !advancing.iter().any(|m| m == member) {
                advancing.push(member.to_string());
                if mint {
                    mintable.push(member.to_string());
                }
            }
        };
        // The one member holding what a header holds, of those it may hold.
        let only_match = |header: &str, values: &[String], fits: &dyn Fn(&str) -> bool| {
            let matching: Vec<&String> = held
                .iter()
                .filter(|(k, v)| fits(k) && holds(header, values, v))
                .map(|(k, _)| k)
                .collect();
            match matching.as_slice() {
                [one] => Some((*one).clone()),
                _ => None,
            }
        };
        for (header, values) in &headers {
            let header = header.as_str();
            if header == IF_MATCH {
                let fits = |m: &str| if_match_member(rt, op, t, m);
                let learned = pairs
                    .get(IF_MATCH)
                    .and_then(Value::as_str)
                    .filter(|m| held.contains_key(*m) && fits(m))
                    .map(str::to_string);
                let Some(member) = learned.or_else(|| only_match(header, values, &fits)) else {
                    continue;
                };
                if current(&member).is_some_and(|now| holds(header, values, now)) {
                    headers_after.insert(IF_MATCH.to_string(), Value::String(member.clone()));
                    advance(&member, true, &mut advancing);
                }
                continue;
            }
            let fits = |m: &str| held.contains_key(m) && header_member(rt, op, t, m);
            // A pairing seen tracked.
            if let Some(member) = pairs.get(header).and_then(Value::as_str) {
                if fits(member) && current(member).is_some_and(|now| holds(header, values, now)) {
                    advance(member, false, &mut advancing);
                }
                continue;
            }
            // A candidate: tracked once the header follows the member as it moves; dropped
            // when the header stays while the member moves.
            if let Some(candidate) = seen.get(header) {
                let member = candidate.get("member").and_then(Value::as_str);
                let was = candidate.get("value").and_then(Value::as_str);
                let (Some(member), Some(was)) = (member, was) else {
                    seen_after.remove(header);
                    continue;
                };
                let now = current(member);
                let moved = now.and_then(as_text).is_some_and(|now| now != was);
                if !fits(member) {
                    seen_after.remove(header);
                } else if moved && now.is_some_and(|now| holds(header, values, now)) {
                    seen_after.remove(header);
                    headers_after.insert(header.to_string(), Value::String(member.to_string()));
                    advance(member, false, &mut advancing);
                    continue;
                } else if moved {
                    seen_after.remove(header);
                    continue;
                } else if now.is_some_and(|now| holds(header, values, now)) {
                    advance(member, false, &mut advancing);
                    continue;
                }
            }
            if let Some(member) = only_match(header, values, &fits)
                && let Some(value) = current(&member).and_then(as_text)
            {
                seen_after.insert(
                    header.to_string(),
                    serde_json::json!({ "member": member, "value": value }),
                );
                advance(&member, false, &mut advancing);
            }
        }
        let tracked = |member: &str| {
            headers_after.iter().any(|(header, m)| {
                m.as_str() == Some(member)
                    && match header.as_str() {
                        IF_MATCH => if_match_member(rt, op, t, member),
                        _ => header_member(rt, op, t, member),
                    }
            })
        };
        let by_if_match =
            |member: &str| headers_after.get(IF_MATCH).and_then(Value::as_str) == Some(member);
        for (member, value) in held {
            if as_text(value).is_none() || advancing.contains(member) {
                continue;
            }
            let by_body = body_candidate(rt, op, t, member);
            let sent =
                body.is_some_and(|b| b.contains_key(member)) || call.query.contains_key(member);
            // Tracked: the client sends back the value it moved to.
            if by_body
                && sent
                && !set_anew(member)
                && is_confirmed(member)
                && !tracked_after.contains(member)
            {
                tracked_after.push(member.clone());
            }
            if is_confirmed(member) && (by_body || tracked(member)) {
                advance(member, by_if_match(member), &mut advancing);
            } else if tied(rt, op, t, member) || (by_body && sent) {
                advance(member, false, &mut advancing);
            }
        }
    }
    Ok(Tokens {
        advancing,
        mintable,
        confirmed,
        headers: pairs,
        seen,
        headers_after,
        seen_after,
        tracked: tracked_body,
        tracked_after,
        kept: doc.is_some(),
    })
}

/// After a 2xx update of `before` the model answered: each advancing token it changed is
/// confirmed, so a later write sending a stale value is refused; an `If-Match` token it
/// left as it was is minted anew, in the store and in the served record `served`. The
/// header pairings the write found are kept.
pub(super) async fn after_update(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    before: &StoredResource,
    tokens: &Tokens,
    served: Option<&mut Value>,
) {
    let pairings_moved = tokens.headers_after != tokens.headers
        || tokens.seen_after != tokens.seen
        || tokens.tracked_after != tokens.tracked;
    if tokens.advancing.is_empty() && !pairings_moved {
        return;
    }
    let t = before.resource_type.as_str();
    let Some(mut after) = state.get(session, t, &before.id).await else {
        return;
    };
    let mut confirmed = tokens.confirmed.clone();
    let mut minted = Map::new();
    for member in &tokens.advancing {
        let (Some(was), Some(now)) = (before.data.get(member), after.data.get(member)) else {
            continue;
        };
        let (Some(was_text), Some(now_text)) = (compared(was), compared(now)) else {
            continue;
        };
        if was_text == now_text {
            if !tokens.mintable.contains(member) {
                continue;
            }
            let Some(new) = mint(was, &format!("{t}\0{}", before.id)) else {
                continue;
            };
            minted.insert(member.clone(), new);
        }
        if !confirmed.contains(member) {
            confirmed.push(member.clone());
        }
    }
    if !minted.is_empty()
        && let Value::Object(data) = &mut after.data
    {
        data.extend(minted.clone());
        state.upsert(session, after).await;
        if let Some(Value::Object(record)) = served {
            for (member, new) in &minted {
                let shows_old = record.get(member).and_then(compared)
                    == before.data.get(member).and_then(compared);
                if shows_old {
                    record.insert(member.clone(), new.clone());
                }
            }
        }
    }
    if confirmed == tokens.confirmed && !pairings_moved {
        return;
    }
    let id = key(t, &before.id);
    state
        .upsert(
            session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_string(),
                id: id.clone(),
                data: serde_json::json!({
                    "confirmed": confirmed,
                    "headers": tokens.headers_after,
                    "seen": tokens.seen_after,
                    "tracked": tokens.tracked_after,
                }),
            },
        )
        .await;
    if tokens.kept {
        return;
    }
    let mut listed: Vec<String> = match state.get(session, INTERNAL_TYPE, INDEX).await {
        Some(doc) => serde_json::from_value(doc.data).unwrap_or_default(),
        None => Vec::new(),
    };
    listed.retain(|held| *held != id);
    listed.push(id);
    let excess = listed.len().saturating_sub(MAX_RECORDS);
    for oldest in listed.drain(..excess) {
        state.delete(session, INTERNAL_TYPE, &oldest).await;
    }
    state
        .upsert(
            session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_string(),
                id: INDEX.to_string(),
                data: Value::from(listed),
            },
        )
        .await;
}

/// After any 2xx delete of `t` `id`, whatever answered it: its tokens go with it. The
/// index keeps the entry, which the bound takes out.
pub(super) async fn forget(state: &mut StateAccess<'_>, session: &SessionKey, t: &str, id: &str) {
    state.delete(session, INTERNAL_TYPE, &key(t, id)).await;
}

/// The declared `ETag` response header of a 2xx answer, by its name as declared, with
/// the record's token paired with `If-Match` as an entity tag: what the client sends
/// back. None for a record with no such token, a scenario or a forced status.
#[cfg(feature = "http-rest")]
pub(super) async fn entity_tag(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    answer: &GeneratedResponse,
) -> Option<(String, String)> {
    let op = plan.op;
    if !rt.consistency
        || !plain(plan)
        || !matches!(op.binding, Binding::Http { .. })
        || !(200..300).contains(&answer.status)
    {
        return None;
    }
    let name = op
        .output_for_status(answer.status)?
        .headers
        .keys()
        .find(|k| k.eq_ignore_ascii_case("etag"))?
        .clone();
    let t = op.resource_type.as_deref()?;
    let record = answer.body.as_ref().and_then(|b| match &op.identity {
        Some(identity) => identity.record_in(b),
        None => Some(b),
    });
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let id = target_id(op, call).or_else(|| record?.get(field).and_then(id_string))?;
    let mut state = StateAccess::new(rt);
    let doc = state
        .get(&plan.session, INTERNAL_TYPE, &key(t, &id))
        .await?;
    let member = doc
        .data
        .get("headers")?
        .get(IF_MATCH)?
        .as_str()?
        .to_string();
    let value = match record.and_then(|r| r.get(&member)).and_then(as_text) {
        Some(value) => value,
        None => state
            .get(&plan.session, t, &id)
            .await?
            .data
            .get(&member)
            .and_then(as_text)?,
    };
    let tag = if value.starts_with('"') || value.starts_with("W/\"") {
        value
    } else {
        format!("\"{value}\"")
    };
    Some((name, tag))
}

/// A new value of token `held`, in its format: an integer one higher; a string's
/// trailing number one higher, else each letter and digit of it redrawn in its class
/// from `salt` (hex staying hex), an entity tag's `W/` and quotes kept. None when no new
/// value differs.
fn mint(held: &Value, salt: &str) -> Option<Value> {
    match held {
        Value::Number(n) => n
            .as_i64()
            .and_then(|n| n.checked_add(1))
            .map(Value::from)
            .or_else(|| n.as_u64().and_then(|n| n.checked_add(1)).map(Value::from)),
        Value::String(text) => minted_text(text, salt).map(Value::String),
        _ => None,
    }
}

/// [`mint`] of a string.
fn minted_text(text: &str, salt: &str) -> Option<String> {
    let (weak, rest) = match text.strip_prefix("W/") {
        Some(rest) => ("W/", rest),
        None => ("", text),
    };
    let (quote, inner) = match rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(inner) => ("\"", inner),
        None => ("", rest),
    };
    if inner.is_empty() {
        return None;
    }
    let new = incremented(inner).unwrap_or_else(|| redrawn(inner, salt));
    (new != inner).then(|| format!("{weak}{quote}{new}{quote}"))
}

/// `text` with its trailing run of digits one higher, as wide or wider.
fn incremented(text: &str) -> Option<String> {
    let head = text.trim_end_matches(|c: char| c.is_ascii_digit());
    let digits = text.len() - head.len();
    if digits == 0 || digits > 38 {
        return None;
    }
    let next = text[head.len()..].parse::<u128>().ok()?.checked_add(1)?;
    Some(format!("{head}{next:0digits$}"))
}

/// `text` with each ASCII letter and digit redrawn in its class from a hash of `salt`
/// and `text`: hex stays hex (in its case), digits stay digits, letters keep their case.
/// Anything else stays.
fn redrawn(text: &str, salt: &str) -> String {
    let hex = text.chars().all(|c| c.is_ascii_hexdigit());
    let hex_set: &[u8] = if text.chars().any(|c| c.is_ascii_uppercase()) {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut pool: Vec<u8> = Vec::new();
    let mut block = 0u32;
    let mut draw = |set: &[u8]| {
        if pool.is_empty() {
            let mut hasher = Sha256::new();
            for part in [salt.as_bytes(), text.as_bytes()] {
                hasher.update(part);
                hasher.update([0]);
            }
            hasher.update(block.to_le_bytes());
            block = block.wrapping_add(1);
            pool = hasher.finalize().to_vec();
        }
        let byte = pool.pop().unwrap_or_default();
        char::from(set[usize::from(byte) % set.len()])
    };
    let out: String = text
        .chars()
        .map(|c| match c {
            _ if hex => draw(hex_set),
            '0'..='9' => draw(b"0123456789"),
            'a'..='z' => draw(b"abcdefghijklmnopqrstuvwxyz"),
            'A'..='Z' => draw(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
            c => c,
        })
        .collect();
    if out != text {
        return out;
    }
    // The draw gave the same: the first letter or digit turns to the next of its class.
    let mut turned = false;
    text.chars()
        .map(|c| {
            if turned || !c.is_ascii_alphanumeric() {
                return c;
            }
            turned = true;
            let set: &[u8] = match c {
                _ if hex => hex_set,
                '0'..='9' => b"0123456789",
                'a'..='z' => b"abcdefghijklmnopqrstuvwxyz",
                _ => b"ABCDEFGHIJKLMNOPQRSTUVWXYZ",
            };
            let at = set.iter().position(|b| char::from(*b) == c).unwrap_or(0);
            char::from(set[(at + 1) % set.len()])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tokens_compare_as_text() {
        assert_eq!(as_text(&json!(7)), Some("7".into()));
        assert_eq!(as_text(&json!("7")), Some("7".into()));
        assert_eq!(as_text(&json!(7.5)), None);
        assert_eq!(as_text(&json!(true)), None);
    }

    #[test]
    fn entity_tags_compare_by_their_opaque_value() {
        assert_eq!(normalized("W/\"3\""), "3");
        assert_eq!(normalized("\"3\""), "3");
        assert_eq!(normalized(" 3 "), "3");
        assert_eq!(compared(&json!("W/\"abc\"")), compared(&json!("\"abc\"")));
        assert_ne!(compared(&json!("\"abc\"")), compared(&json!("\"abd\"")));
    }

    #[test]
    fn if_match_is_an_entity_tag_list_that_respects_quotes() {
        let tags = |s: &str| entity_tags(s);
        assert_eq!(tags("\"x,y\""), Some(vec!["x,y".to_string()]));
        assert_eq!(
            tags("W/\"a\", \"b,c\""),
            Some(vec!["a".to_string(), "b,c".to_string()])
        );
        assert_eq!(tags("abc"), Some(vec!["abc".to_string()]));
        assert_eq!(tags("*"), None);
        assert_eq!(tags("\"a\", *"), None);
        assert_eq!(tags(" , "), None);
    }

    #[test]
    fn an_integer_body_token_is_stale_only_when_lower() {
        assert!(stale(&json!(1), &json!(2)));
        assert!(stale(&json!("1"), &json!(2)));
        assert!(!stale(&json!(2), &json!(2)));
        assert!(!stale(&json!(9), &json!(2)));
        assert!(stale(&json!("a"), &json!("b")));
        assert!(!stale(&json!("\"b\""), &json!("b")));
    }

    #[test]
    fn a_mint_keeps_the_format() {
        assert_eq!(mint(&json!(7), "s"), Some(json!(8)));
        assert_eq!(mint(&json!("0"), "s"), Some(json!("1")));
        assert_eq!(mint(&json!("\"v-09\""), "s"), Some(json!("\"v-10\"")));
        assert_eq!(mint(&json!("W/\"41\""), "s"), Some(json!("W/\"42\"")));

        let hex = mint(&json!("\"7f3a\""), "s").unwrap();
        let hex = hex.as_str().unwrap();
        assert_ne!(hex, "\"7f3a\"");
        assert_eq!(hex.len(), 6);
        assert!(
            hex[1..5]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );

        let opaque = "W/\"JzQ0O0pZRUFB;Mz\"";
        let new = mint(&json!(opaque), "s").unwrap();
        let new = new.as_str().unwrap();
        assert_ne!(new, opaque);
        assert!(new.starts_with("W/\"") && new.ends_with('"'), "{new}");
        assert_eq!(new.len(), opaque.len());
        for (a, b) in opaque.chars().zip(new.chars()) {
            assert_eq!(a.is_ascii_digit(), b.is_ascii_digit(), "{new}");
            assert_eq!(a.is_ascii_lowercase(), b.is_ascii_lowercase(), "{new}");
            assert_eq!(a.is_ascii_uppercase(), b.is_ascii_uppercase(), "{new}");
            if !a.is_ascii_alphanumeric() {
                assert_eq!(a, b);
            }
        }
        assert_eq!(mint(&json!("\"\""), "s"), None);
        assert_eq!(mint(&json!(true), "s"), None);
    }
}
