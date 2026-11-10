//! World expressions at runtime: an operation's `refuse` and `set` (its `changes`
//! read as `set`, see [`mb_mcm::world::Expressions`]), compiled once with the
//! effective world ([`super::Described::expressions`]) and evaluated here per call.
//!
//! - The bindings: `request` is `{body, path, query, headers}` (header names lower
//!   case), `record` the target as stored before the call (`null` without one), `now`
//!   the request time as RFC 3339 text: the seeded time ([`mb_engine::seed::SEEDED_NOW`])
//!   when the call's draws are seeded, as the engine's dates are, else the clock.
//! - The session ([`SessionHost`]): `held(type, id)` finds a held record by its key
//!   (in its identity pattern's forms too), else by its identity member, else by a
//!   described alternate (one record only); `count` and `exists` read the type's held
//!   records, at most [`MAX_SCAN`]; `seq(scope)` is a per-session counter
//!   ([`SeqCounters`]); `mint(pattern)` draws an id matching the pattern from the
//!   seeded minting ([`super::header_ids::draw_id_as`]). The records of the types an
//!   operation's expressions name are read once, before they are evaluated.
//! - A call's `set` values are evaluated once and kept on its plan
//!   ([`CallExprs`]), so the stored record and the answer show the same `seq()` or
//!   `mint()` value.
//! - An expression that fails (a type mismatch, its step budget) is logged with
//!   `expr_failed` (its pointer in the world) and noted on the request record; the
//!   value stays as the engine would have made it, and a refusal whose `when` fails
//!   does not fire.

use std::collections::HashMap;
use std::sync::Mutex;

use mb_engine::consistency::property_schema;
use mb_engine::error_body::{ErrorCause, RefusalFacts};
use mb_engine::{GeneratedResponse, OperationCall};
use mb_mcm::world::expr::{Bindings, Host, Program};
use mb_mcm::world::{OpExpressions, SetValue, TextValue};
use mb_state::{SessionKey, StoredResource};
use serde_json::{Map, Value, json};

use super::header_ids::{MintSeed, draw_id_as};
use super::write_memory::read_schema;
use super::{CallPlan, MAX_SCAN, StateAccess, id_string, records};
use crate::MockRuntime;

/// Sessions [`SeqCounters`] holds at most; it is emptied when full.
const MAX_SEQ_SESSIONS: usize = 1024;
/// Scopes one session counts at most; its counters are emptied when full.
const MAX_SEQ_SCOPES: usize = 1024;

/// Each session's `seq(scope)` counters, from 1, per runtime.
#[derive(Debug, Default)]
pub(crate) struct SeqCounters(Mutex<HashMap<SessionKey, HashMap<String, i64>>>);

impl SeqCounters {
    /// The next value of `session`'s counter `scope`.
    fn next(&self, session: &SessionKey, scope: &str) -> i64 {
        let mut all = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !all.contains_key(session) && all.len() >= MAX_SEQ_SESSIONS {
            all.clear();
        }
        let counters = all.entry(session.clone()).or_default();
        if !counters.contains_key(scope) && counters.len() >= MAX_SEQ_SCOPES {
            counters.clear();
        }
        let n = counters.entry(scope.to_string()).or_insert(0);
        *n = n.saturating_add(1);
        *n
    }
}

/// A call's `set` values: each member, and its value (`None` when its expression
/// failed or gave `null`).
type SetValues = Vec<(String, Option<Value>)>;

/// What one call's world expressions decided, and what the executor noted of the call
/// for its request record.
#[derive(Debug, Default)]
pub(crate) struct CallExprs {
    notes: Mutex<Notes>,
    /// The call's `set` values, once evaluated: each member and its value (`None`
    /// when its expression failed or gave `null`).
    set: Mutex<Option<SetValues>>,
}

#[derive(Debug, Default)]
struct Notes {
    /// Why the executor went on to generation, and what caused it.
    fallback: Option<(String, Option<String>)>,
    /// Each expression that failed: `{pointer, error}`.
    failed: Vec<Value>,
}

/// How many failed expressions a request record lists at most.
const MAX_FAILED: usize = 16;

impl CallExprs {
    fn with_notes(&self, f: impl FnOnce(&mut Notes)) {
        let mut notes = self
            .notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut notes);
    }

    /// Notes that the executor went on to generation for `reason`, caused by `on`.
    pub(super) fn fell_back(&self, reason: &str, on: Option<&str>) {
        self.with_notes(|n| n.fallback = Some((reason.to_string(), on.map(str::to_string))));
    }

    /// Notes that the expression at `pointer` failed with `error`.
    pub(super) fn failed(&self, pointer: &str, error: &str) {
        self.with_notes(|n| {
            if n.failed.len() < MAX_FAILED {
                n.failed.push(json!({"pointer": pointer, "error": error}));
            }
        });
    }

    /// The notes, as a request record's `request` keeps them: `exec_fallback`,
    /// `exec_fallback_on` and `expr_failed`, each only when there is one.
    pub(crate) fn notes(&self) -> Map<String, Value> {
        let notes = self
            .notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = Map::new();
        if let Some((reason, on)) = &notes.fallback {
            out.insert("exec_fallback".into(), Value::from(reason.as_str()));
            if let Some(on) = on {
                out.insert(
                    "exec_fallback_on".into(),
                    Value::from(super::log_capped(on)),
                );
            }
        }
        if !notes.failed.is_empty() {
            out.insert("expr_failed".into(), Value::Array(notes.failed.clone()));
        }
        out
    }
}

/// Adds what `plan`'s call noted ([`CallExprs::notes`]) to `generated`, for the request
/// record.
pub(crate) fn seed_notes(plan: &CallPlan<'_>, generated: &mut GeneratedResponse) {
    generated.exec_notes.extend(plan.exprs.notes());
}

/// The session as an expression reads it, the records of the types it names read
/// beforehand.
pub(super) struct SessionHost<'r> {
    rt: &'r MockRuntime,
    session: &'r SessionKey,
    call: &'r OperationCall,
    records: HashMap<String, Vec<StoredResource>>,
}

impl<'r> SessionHost<'r> {
    /// Reads the session's records of `types` (at most [`MAX_SCAN`] each).
    pub(super) async fn load(
        state: &mut StateAccess<'_>,
        (rt, session, call): (&'r MockRuntime, &'r SessionKey, &'r OperationCall),
        types: &[String],
    ) -> SessionHost<'r> {
        let mut records = HashMap::new();
        for t in types {
            let held = state.list(session, t, MAX_SCAN).await;
            records.insert(t.clone(), held);
        }
        SessionHost {
            rt,
            session,
            call,
            records,
        }
    }

    /// The member type `t`'s records are named by: the described identity member,
    /// else that of the type's read by id, else `id`.
    fn identity_member(&self, t: &str) -> String {
        self.rt
            .described()
            .type_desc(t)
            .and_then(|d| d.identity.as_ref()?.member.clone())
            .or_else(|| {
                let i = self.rt.index().detail_reads.get(t)?;
                let read = self.rt.bundle().operations.get(*i)?;
                read.identity.as_ref().map(|i| i.field.clone())
            })
            .unwrap_or_else(|| "id".to_string())
    }
}

impl Host for SessionHost<'_> {
    fn held(&mut self, t: &str, id: &Value) -> Option<Value> {
        let id = id_string(id)?;
        let held = self.records.get(t)?;
        if let Some(r) = held.iter().find(|r| r.id == id) {
            return Some(r.data.clone());
        }
        if let Some(pattern) = records::pattern(self.rt, t) {
            let other =
                records::bare_id(pattern, &id).unwrap_or_else(|| records::patterned(pattern, &id));
            if let Some(r) = held.iter().find(|r| r.id == other) {
                return Some(r.data.clone());
            }
        }
        let member = self.identity_member(t);
        let mut fields = vec![member.clone()];
        fields.extend(records::extra_fields(self.rt, t, &[member.as_str()]));
        for field in &fields {
            let mut holding = held.iter().filter(|r| {
                mb_mcm::member_at(&r.data, field)
                    .and_then(id_string)
                    .is_some_and(|v| v == id)
            });
            if let (Some(found), None) = (holding.next(), holding.next()) {
                return Some(found.data.clone());
            }
        }
        None
    }

    fn all(&mut self, t: &str) -> Vec<Value> {
        self.records
            .get(t)
            .map(|held| held.iter().take(MAX_SCAN).map(|r| r.data.clone()).collect())
            .unwrap_or_default()
    }

    fn seq(&mut self, scope: &str) -> i64 {
        self.rt.seq_counters().next(self.session, scope)
    }

    fn mint(&mut self, pattern: &str) -> Option<String> {
        let seed = MintSeed::new(self.rt, self.session, self.call);
        let schema = json!({"type": "string", "pattern": pattern});
        draw_id_as(self.rt, (seed, "expr", pattern), &schema)
            .as_ref()
            .and_then(id_string)
    }
}

/// `request` as expressions see it: `{body, path, query, headers}`, header names
/// lower case.
fn request_value(call: &OperationCall) -> Value {
    let headers: Map<String, Value> = call
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect();
    json!({
        "body": call.body.clone().unwrap_or(Value::Null),
        "path": call.path_params,
        "query": call.query,
        "headers": headers,
    })
}

/// `now` as expressions see it: the request time as RFC 3339 text, the seeded time
/// when the call's draws are seeded (as the engine's generated dates are).
fn now_value(rt: &MockRuntime, call: &OperationCall) -> Value {
    let seeded = mb_engine::seed::request_seed(call)
        .or(rt.engine.faker_seed())
        .is_some();
    let at = match seeded {
        true => chrono::DateTime::from_timestamp(mb_engine::seed::SEEDED_NOW, 0),
        false => Some(chrono::Utc::now()),
    };
    at.map_or(Value::Null, |at| {
        Value::from(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    })
}

/// What one evaluation reads: the request, the target before the call and the time.
struct Bound {
    request: Value,
    record: Value,
    now: Value,
}

impl Bound {
    fn new(rt: &MockRuntime, call: &OperationCall, record: Option<&Value>) -> Self {
        Self {
            request: request_value(call),
            record: record.cloned().unwrap_or(Value::Null),
            now: now_value(rt, call),
        }
    }

    fn bindings(&self) -> Bindings<'_> {
        Bindings {
            request: &self.request,
            record: &self.record,
            now: &self.now,
            it: None,
        }
    }
}

/// Evaluates `program` (at `pointer` in the world); a failure is logged and noted on
/// the plan, and is `None`.
fn eval(
    plan: &CallPlan<'_>,
    rt: &MockRuntime,
    (program, pointer): (&Program, &str),
    bound: &Bound,
    host: &mut SessionHost<'_>,
) -> Option<Value> {
    match program.eval(&bound.bindings(), host) {
        Ok(v) => Some(v),
        Err(e) => {
            failed(plan, rt, pointer, &e.to_string());
            None
        }
    }
}

fn failed(plan: &CallPlan<'_>, rt: &MockRuntime, pointer: &str, error: &str) {
    tracing::info!(
        mock = %rt.name,
        operation = %plan.op.id,
        expr_failed = %pointer,
        error = %error,
        "a world expression failed: the value stays as the engine makes it"
    );
    plan.exprs.failed(pointer, error);
}

/// Reads what `ops`' expressions need of the session.
async fn host<'r>(
    state: &mut StateAccess<'_>,
    rt: &'r MockRuntime,
    (plan, call): (&'r CallPlan<'_>, &'r OperationCall),
    ops: &OpExpressions,
) -> SessionHost<'r> {
    SessionHost::load(state, (rt, &plan.session, call), &ops.types).await
}

/// The refusal `plan`'s operation states for this call: its first `refuse` entry whose
/// `when` is `true` (a failed or non-boolean `when` does not fire), answered with its
/// status by [`mb_engine::Engine::refuse_held`], its `code` and `message` placed
/// where the declared error schema has members for them. `None` when none fires, or
/// the operation declares no response for its status.
pub(super) async fn refusal(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target: Option<&StoredResource>,
) -> Option<GeneratedResponse> {
    let rt = state.rt;
    let ops = rt.described().expressions(&plan.op.id)?;
    if ops.refuse.is_empty() {
        return None;
    }
    let mut host = host(state, rt, (plan, call), ops).await;
    let bound = Bound::new(rt, call, target.map(|t| &t.data));
    for entry in &ops.refuse {
        let when = format!("{}/when", entry.pointer);
        let fires = match eval(plan, rt, (&entry.when, &when), &bound, &mut host) {
            Some(Value::Bool(fires)) => fires,
            Some(Value::Null) | None => false,
            Some(other) => {
                failed(plan, rt, &when, &format!("`when` gave {other}, not a bool"));
                false
            }
        };
        if !fires {
            continue;
        }
        let mut text = |t: &Option<TextValue>, key: &str| match t {
            None => None,
            Some(TextValue::Text(s)) => Some(s.clone()),
            Some(TextValue::Expr(p)) => {
                let at = format!("{}/{key}", entry.pointer);
                match eval(plan, rt, (p, &at), &bound, &mut host)? {
                    Value::String(s) => Some(s),
                    Value::Null => None,
                    other => Some(other.to_string()),
                }
            }
        };
        let code = text(&entry.code, "code");
        let message = text(&entry.message, "message");
        let facts = RefusalFacts::stated(code, message);
        let refusal = plan
            .engine
            .refuse_held(
                rt.bundle(),
                plan.op,
                call,
                entry.status,
                ErrorCause::Refused,
                &facts,
            )
            .await;
        tracing::info!(
            mock = %rt.name,
            operation = %plan.op.id,
            refused_by = %entry.pointer,
            status = entry.status,
            answered = refusal.is_some(),
            "a described refusal holds"
        );
        return refusal;
    }
    None
}

/// The members `plan`'s operation sets on the record it stores, each with its value
/// (`None` when its expression failed or gave `null`: the member is left as the
/// engine makes it), evaluated once per call against `before`, the target as stored
/// before it.
pub(super) async fn set_values(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    before: Option<&Value>,
) -> SetValues {
    let kept = plan
        .exprs
        .set
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(kept) = kept {
        return kept;
    }
    let rt = state.rt;
    let Some(ops) = rt
        .described()
        .expressions(&plan.op.id)
        .filter(|o| !o.set.is_empty())
    else {
        return Vec::new();
    };
    let mut host = host(state, rt, (plan, call), ops).await;
    let bound = Bound::new(rt, call, before);
    let values: Vec<(String, Option<Value>)> =
        ops.set
            .iter()
            .map(|entry| {
                let value = match &entry.value {
                    SetValue::Literal(v) => Some(v.clone()),
                    SetValue::Expr(p) => eval(plan, rt, (p, &entry.pointer), &bound, &mut host)
                        .filter(|v| !v.is_null()),
                };
                (entry.member.clone(), value)
            })
            .collect();
    *plan
        .exprs
        .set
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(values.clone());
    values
}

/// Whether `plan`'s operation sets members of the record it stores (`set`, or
/// `changes`).
pub(super) fn sets(rt: &MockRuntime, op_id: &str) -> bool {
    rt.described()
        .expressions(op_id)
        .is_some_and(|o| !o.set.is_empty())
}

/// `value` written as type `t`'s `member` declares it: an integral number into an
/// `integer` member as an integer, a number or boolean into a `string` member as its
/// text, a text holding a number into a `number` or `integer` member as that number,
/// `"true"`/`"false"` into a `boolean` member. A value of a type the member declares
/// (or one with no declared type) stays as it is.
pub(super) fn written_back(rt: &MockRuntime, t: &str, member: &str, value: Value) -> Value {
    let bundle = rt.bundle();
    let Some(schema) = read_schema(rt, t).and_then(|r| property_schema(bundle, r, member)) else {
        return value;
    };
    let kinds = declared_types(bundle, &schema, 0);
    if kinds.is_empty() {
        return value;
    }
    let has = |k: &str| kinds.iter().any(|d| d == k);
    let integral = |v: &Value| -> Option<i64> {
        match v {
            Value::Number(n) => n.as_i64().or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15)
                    .map(|f| f as i64)
            }),
            _ => None,
        }
    };
    match &value {
        Value::Number(_) if has("integer") && integral(&value).is_some() => {
            integral(&value).map_or(value, Value::from)
        }
        Value::Number(_) if has("number") || has("integer") => value,
        Value::Number(n) if has("string") => {
            Value::from(integral(&value).map_or_else(|| n.to_string(), |i| i.to_string()))
        }
        Value::Bool(b) if !has("boolean") && has("string") => Value::from(b.to_string()),
        Value::String(s) if !has("string") => {
            let text = s.trim();
            if has("integer")
                && let Ok(i) = text.parse::<i64>()
            {
                Value::from(i)
            } else if (has("number") || has("integer"))
                && let Some(n) = text
                    .parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .and_then(serde_json::Number::from_f64)
            {
                Value::Number(n)
            } else if has("boolean") && matches!(text, "true" | "false") {
                Value::Bool(text == "true")
            } else {
                value
            }
        }
        _ => value,
    }
}

/// The JSON types `schema` declares, through `$ref`s and compositions.
fn declared_types(bundle: &mb_mcm::Bundle, schema: &Value, depth: usize) -> Vec<String> {
    if depth > 8 {
        return Vec::new();
    }
    let schema = bundle.resolve(schema);
    let mut out: Vec<String> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    for key in ["allOf", "anyOf", "oneOf"] {
        for part in schema
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for t in declared_types(bundle, part, depth + 1) {
                if !out.contains(&t) {
                    out.push(t);
                }
            }
        }
    }
    out
}
