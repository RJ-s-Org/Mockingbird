//! Members derived exactly from stored records: counts and sums.
//!
//! A type's `derived` members (`count` of a child type, or `sum` of a child member,
//! optionally over the children holding given values) are computed when a record of
//! the type is answered, and never stored:
//! - an answer from state ([`filled`], before the held answer is served);
//! - each item of a list from state ([`fill_page`]);
//! - a generated answer about a held record ([`fill`], after its writes).
//!
//! A child is a held record of the child type that is not soft-deleted, and whose
//! `parent.member` names the record's id, or which this runtime stored under a
//! collection whose `parent.path_param` value is that id ([`Collections::get`]). Ids
//! compare bare: the parent's `pattern` and the type's identity pattern are taken off.
//!
//! Nothing is set unless the record was created in the session by this runtime
//! ([`Collections::created`]; a rebuild forgets it, and a record with no child held
//! then keeps the model's value), or a child under it is held. A sum adds exactly (a
//! [`Decimal`]), and the result goes into an `integer` member only when integral, a
//! `number` member as a number, a `string` member as a decimal at the children's
//! largest scale. A result the member's schema refuses is not set, nor one over a
//! listing cut at [`MAX_SCAN`] (it would not be exact), nor a sum over a child whose
//! member is no number: the model's value stays.
//!
//! An answer costs one listing per derived child type of the answered type, whatever
//! the page size, and at most [`MAX_DERIVED_CHILD_TYPES`] child types are consulted.

use std::collections::{BTreeMap, HashMap};

use mb_engine::SchemaValidator;
use mb_mcm::world::{DerivedDesc, ParentDesc};

use super::*;

/// Child types consulted for one type's derived members, at most; members over
/// further child types are not set.
pub(crate) const MAX_DERIVED_CHILD_TYPES: usize = 4;

/// Digits a [`Decimal`] holds at most, so every sum of [`MAX_SCAN`] of them fits.
const MAX_DIGITS: usize = 30;

/// An exact decimal: `mantissa` × 10^-`scale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decimal {
    mantissa: i128,
    scale: u32,
}

impl Decimal {
    /// Zero, at scale 0.
    pub(crate) const ZERO: Self = Self {
        mantissa: 0,
        scale: 0,
    };

    /// The whole number `n`.
    pub(crate) fn whole(n: usize) -> Option<Self> {
        Some(Self {
            mantissa: i128::try_from(n).ok()?,
            scale: 0,
        })
    }

    /// The decimal a JSON number or numeric string holds (`12.50`, `-3`, `1e-2`);
    /// `None` for anything else, or more than [`MAX_DIGITS`] digits.
    pub(crate) fn parse(value: &Value) -> Option<Self> {
        match value {
            Value::Number(n) => Self::parse_text(&n.to_string()),
            Value::String(text) => Self::parse_text(text.trim()),
            _ => None,
        }
    }

    fn parse_text(text: &str) -> Option<Self> {
        let (negative, rest) = match text.as_bytes().first()? {
            b'-' => (true, &text[1..]),
            b'+' => (false, &text[1..]),
            _ => (false, text),
        };
        let (number, exponent) = match rest.split_once(['e', 'E']) {
            Some((number, exponent)) => (number, exponent.parse::<i32>().ok()?),
            None => (rest, 0),
        };
        let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
        if whole.is_empty() && fraction.is_empty() {
            return None;
        }
        let digits = format!("{whole}{fraction}");
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let digits = digits.trim_start_matches('0');
        if digits.len() > MAX_DIGITS {
            return None;
        }
        let mut mantissa: i128 = if digits.is_empty() {
            0
        } else {
            digits.parse().ok()?
        };
        let mut scale = i64::try_from(fraction.len()).ok()? - i64::from(exponent);
        if scale < 0 {
            mantissa = mantissa.checked_mul(10i128.checked_pow(u32::try_from(-scale).ok()?)?)?;
            scale = 0;
        }
        let scale = u32::try_from(scale)
            .ok()
            .filter(|s| *s as usize <= MAX_DIGITS)?;
        if negative {
            mantissa = -mantissa;
        }
        Some(Self { mantissa, scale })
    }

    /// `self` + `other`, at the larger scale of the two; `None` on overflow.
    pub(crate) fn add(self, other: Self) -> Option<Self> {
        let scale = self.scale.max(other.scale);
        let at = |d: Self| d.mantissa.checked_mul(10i128.checked_pow(scale - d.scale)?);
        Some(Self {
            mantissa: at(self)?.checked_add(at(other)?)?,
            scale,
        })
    }

    /// The whole number, when `self` is one.
    fn integral(&self) -> Option<i128> {
        let unit = 10i128.checked_pow(self.scale)?;
        (self.mantissa % unit == 0).then_some(self.mantissa / unit)
    }

    /// The decimal text at `self`'s scale: `13.75`, `14.00`, `-0.5`.
    fn text(&self) -> String {
        let digits = self.mantissa.unsigned_abs().to_string();
        let scale = self.scale as usize;
        let padded = format!("{digits:0>width$}", width = scale + 1);
        let (whole, fraction) = padded.split_at(padded.len() - scale);
        let sign = if self.mantissa < 0 { "-" } else { "" };
        match fraction.is_empty() {
            true => format!("{sign}{whole}"),
            false => format!("{sign}{whole}.{fraction}"),
        }
    }

    /// `self` written in the type `member_schema` declares: an `integer` only when
    /// integral, a `number`, else a decimal `string`. `None` when it declares none of
    /// these, or the value cannot be written in it.
    pub(crate) fn to_value(self, member_schema: &Value) -> Option<Value> {
        let declared: Vec<&str> = match member_schema.get("type") {
            Some(Value::String(t)) => vec![t.as_str()],
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let whole = || self.integral().and_then(|n| i64::try_from(n).ok());
        if declared.contains(&"integer")
            && let Some(n) = whole()
        {
            return Some(Value::from(n));
        }
        if declared.contains(&"number") {
            if let Some(n) = whole() {
                return Some(Value::from(n));
            }
            let float: f64 = self.text().parse().ok()?;
            return serde_json::Number::from_f64(float).map(Value::Number);
        }
        declared
            .contains(&"string")
            .then(|| Value::String(self.text()))
    }
}

/// The held children of one type for one session, by the bare id of their parent.
#[derive(Debug, Default)]
pub(crate) struct Children {
    by_parent: HashMap<String, Vec<Value>>,
    /// The listing returned [`MAX_SCAN`] records: some may be missing.
    cut: bool,
}

impl Children {
    /// The held children of type `of`, whose described parent `parent` is type `t`:
    /// one listing. `None` when the store did not answer.
    async fn of(
        state: &mut StateAccess<'_>,
        session: &SessionKey,
        (t, of): (&str, &str),
        parent: &ParentDesc,
    ) -> Option<Self> {
        let rt = state.rt;
        let listed = state.list(session, of, MAX_SCAN).await;
        if !state.available() {
            return None;
        }
        let gone = lifecycle::exclusion(rt, of);
        // Where `parent.path_param` is in each collection template met.
        let mut positions: HashMap<String, Option<usize>> = HashMap::new();
        let mut children = Self {
            by_parent: HashMap::new(),
            cut: listed.len() >= MAX_SCAN,
        };
        for child in listed {
            if gone.as_ref().is_some_and(|g| g.excludes(&child.data)) {
                continue;
            }
            let mut parents: Vec<String> = Vec::new();
            if let Some(member) = &parent.member
                && let Some(value) = mb_mcm::member_at(&child.data, member).and_then(id_string)
            {
                parents.push(bare(rt, t, parent.pattern.as_deref(), &value));
            }
            if let Some(param) = &parent.path_param
                && let Some(collection) = rt.collections().get(session, &child)
                && let Some(at) = *positions
                    .entry(collection.template.clone())
                    .or_insert_with(|| param_position(rt, &collection.template, param))
                && let Some(value) = path_segments(&collection.path).get(at)
            {
                let value = value.to_string();
                let value = bare(rt, t, parent.pattern.as_deref(), &value);
                if !parents.contains(&value) {
                    parents.push(value);
                }
            }
            for id in parents {
                children
                    .by_parent
                    .entry(id)
                    .or_default()
                    .push(child.data.clone());
            }
        }
        Some(children)
    }

    fn under(&self, id: &str) -> &[Value] {
        self.by_parent.get(id).map_or(&[], Vec::as_slice)
    }
}

/// The segment of collection `template` (`/orders/{}/lines`, its parameters unnamed)
/// that path parameter `param` fills, as a route of the contract beginning with that
/// collection names it.
fn param_position(rt: &MockRuntime, template: &str, param: &str) -> Option<usize> {
    let wanted = path_segments(template);
    rt.bundle().operations.iter().find_map(|op| {
        let Binding::Http { path, .. } = &op.binding else {
            return None;
        };
        let segments = path_segments(path);
        let prefix = segments.get(..wanted.len())?;
        let same = prefix
            .iter()
            .zip(&wanted)
            .all(|(segment, want)| match segment_param(segment) {
                Some(_) => *want == "{}",
                None => segment == want,
            });
        if !same {
            return None;
        }
        prefix
            .iter()
            .position(|segment| segment_param(segment) == Some(param))
    })
}

/// `value`, an id of type `t`, bare: the parent's `pattern` taken off, else `t`'s
/// identity pattern, else as it is.
fn bare(rt: &MockRuntime, t: &str, pattern: Option<&str>, value: &str) -> String {
    pattern
        .and_then(|p| records::bare_id(p, value))
        .or_else(|| records::bare_id(records::pattern(rt, t)?, value))
        .unwrap_or_else(|| value.to_string())
}

/// The member type `t`'s records are named by.
fn identity_member<'a>(rt: &'a MockRuntime, t: &str) -> &'a str {
    rt.described()
        .type_desc(t)
        .and_then(|d| d.identity.as_ref())
        .and_then(|i| i.member.as_deref())
        .unwrap_or("id")
}

/// The child type a derived member is over.
fn child_type(d: &DerivedDesc) -> &str {
    match d {
        DerivedDesc::Count { of, .. } | DerivedDesc::Sum { of, .. } => of,
    }
}

/// The child types type `t`'s derived members are over, in the description's order,
/// at most [`MAX_DERIVED_CHILD_TYPES`].
fn child_types(derived: &BTreeMap<String, DerivedDesc>) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for d in derived.values() {
        let of = child_type(d);
        if !out.contains(&of) {
            out.push(of);
        }
    }
    out.truncate(MAX_DERIVED_CHILD_TYPES);
    out
}

/// The types of `world` whose derived members are over more than
/// [`MAX_DERIVED_CHILD_TYPES`] child types: those past it are not set. Logged once per
/// runtime, when it is built.
pub(crate) fn log_skipped_child_types(world: &mb_mcm::world::World) {
    for (t, desc) in &world.types {
        let mut all: Vec<&str> = desc.derived.values().map(child_type).collect();
        all.sort_unstable();
        all.dedup();
        if all.len() > MAX_DERIVED_CHILD_TYPES {
            tracing::info!(
                r#type = %t,
                child_types = all.len(),
                max = MAX_DERIVED_CHILD_TYPES,
                "derived members over child types past the cap are not set"
            );
        }
    }
}

/// Whether the description derives any member of type `t`.
pub(super) fn describes(rt: &MockRuntime, t: &str) -> bool {
    rt.world().is_some()
        && rt
            .described()
            .type_desc(t)
            .is_some_and(|d| !d.derived.is_empty())
}

/// Whether `child` holds each of `wanted`'s values: equal, or the same text.
fn holds(child: &Value, wanted: Option<&BTreeMap<String, Value>>) -> bool {
    wanted.into_iter().flatten().all(|(member, want)| {
        let Some(held) = mb_mcm::member_at(child, member) else {
            return false;
        };
        held == want
            || match (held, want) {
                (Value::String(text), other) | (other, Value::String(text)) => {
                    !other.is_string()
                        && serde_json::from_str::<Value>(text.trim()).is_ok_and(|v| v == *other)
                }
                _ => false,
            }
    })
}

/// The exact value of `d` over `children`; `None` when a summed member is not a
/// number, or the sum overflows.
fn compute(d: &DerivedDesc, children: &[Value]) -> Option<Decimal> {
    match d {
        DerivedDesc::Count { .. } => Decimal::whole(children.len()),
        DerivedDesc::Sum {
            member, r#where, ..
        } => {
            let mut sum = Decimal::ZERO;
            for child in children.iter().filter(|c| holds(c, r#where.as_ref())) {
                match mb_mcm::member_at(child, member) {
                    None | Some(Value::Null) => {}
                    Some(value) => sum = sum.add(Decimal::parse(value)?)?,
                }
            }
            Some(sum)
        }
    }
}

/// How one derived member of a type is written: its schema's declared types, and a
/// validator of its whole schema.
struct Target {
    typed: Value,
    validator: SchemaValidator,
}

impl Target {
    /// `member` as record schema `record` declares it; `None` when it does not.
    fn of(rt: &MockRuntime, record: &Value, member: &str) -> Option<Self> {
        let bundle = rt.bundle();
        let schema = property_schema(bundle, record, member)?;
        let (parts, _) = schema_parts(bundle, Some(&schema));
        let mut types: Vec<Value> = Vec::new();
        for part in parts {
            let declared = match part.get("type") {
                Some(Value::String(t)) => vec![Value::String(t.clone())],
                Some(Value::Array(ts)) => ts.clone(),
                _ => Vec::new(),
            };
            for t in declared {
                if !types.contains(&t) {
                    types.push(t);
                }
            }
        }
        let validator = SchemaValidator::for_output(&bundle.with_reachable_defs(&schema)).ok()?;
        Some(Self {
            typed: serde_json::json!({ "type": types }),
            validator,
        })
    }

    fn value(&self, d: Decimal) -> Option<Value> {
        d.to_value(&self.typed)
            .filter(|v| self.validator.is_valid(v))
    }
}

/// Fills the derived members of type `t` on each of `items`, records of it: one
/// children listing per child type for the whole page. Answers, per item, the members
/// it set.
///
/// `items_schema` is the schema each item answers to (a list's item schema): only
/// the members it declares are set, each written as it types it, and an item it
/// refuses once filled is put back as it was. With none, the members are typed by
/// type `t`'s read schema and the caller checks the answer they end up in.
pub(super) async fn fill_page(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    items: &mut [Value],
    items_schema: Option<&Value>,
) -> Vec<Vec<String>> {
    let mut set: Vec<Vec<String>> = vec![Vec::new(); items.len()];
    let rt = state.rt;
    if items.is_empty() || !describes(rt, t) {
        return set;
    }
    let Some(desc) = rt.described().type_desc(t) else {
        return set;
    };
    let Some(record_schema) = items_schema.or_else(|| write_memory::read_schema(rt, t)) else {
        return set;
    };
    let mut groups: HashMap<&str, Children> = HashMap::new();
    for of in child_types(&desc.derived) {
        let Some(parent) = rt
            .described()
            .type_desc(of)
            .and_then(|d| d.parent.as_ref())
            .filter(|p| p.r#type == t)
        else {
            continue;
        };
        if let Some(children) = Children::of(state, session, (t, of), parent).await {
            groups.insert(of, children);
        }
    }
    if groups.is_empty() {
        return set;
    }
    // Each filled item still answers to `items_schema`, when one is given.
    let item_check = match items_schema {
        Some(schema) => {
            match SchemaValidator::for_output(&rt.bundle().with_reachable_defs(schema)) {
                Ok(validator) => Some(validator),
                Err(_) => return set,
            }
        }
        None => None,
    };
    let targets: HashMap<&str, Target> = desc
        .derived
        .keys()
        .filter_map(|member| Some((member.as_str(), Target::of(rt, record_schema, member)?)))
        .collect();
    let field = identity_member(rt, t);
    for (item, set) in items.iter_mut().zip(set.iter_mut()) {
        if !item.is_object() {
            continue;
        }
        let Some(value) = mb_mcm::member_at(item, field).and_then(id_string) else {
            continue;
        };
        let id = bare(rt, t, None, &value);
        let created = [value.as_str(), id.as_str()]
            .into_iter()
            .chain(records::in_pattern(rt, t, &id).as_deref())
            .any(|key| rt.collections().created(session, t, key));
        let any_child = groups.values().any(|g| !g.under(&id).is_empty());
        if !created && !any_child {
            continue;
        }
        let before = item_check.as_ref().map(|_| item.clone());
        for (member, d) in &desc.derived {
            let (Some(children), Some(target)) =
                (groups.get(child_type(d)), targets.get(member.as_str()))
            else {
                continue;
            };
            if children.cut {
                continue;
            }
            let Some(result) = compute(d, children.under(&id)).and_then(|r| target.value(r)) else {
                tracing::info!(
                    mock = %rt.name,
                    r#type = %t,
                    member = %member,
                    "a derived member's result does not fit its schema: not set"
                );
                continue;
            };
            if mb_mcm::set_member_at(item, member, result) {
                set.push(member.clone());
            }
        }
        if !set.is_empty()
            && let (Some(validator), Some(before)) = (&item_check, before)
            && !validator.is_valid(item)
        {
            tracing::info!(
                mock = %rt.name,
                r#type = %t,
                "an item with its derived members does not fit its schema: not set"
            );
            *item = before;
            set.clear();
        }
    }
    set
}

/// Fills the derived members of type `t` on `record`, one record of it, typed by `t`'s
/// read schema (the caller checks the answer it ends up in); answers the members it
/// set.
pub(super) async fn fill(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    record: &mut Value,
) -> Vec<String> {
    fill_page(state, session, t, std::slice::from_mut(record), None)
        .await
        .pop()
        .unwrap_or_default()
}

/// `stored`, a held record, with its derived members filled; `None` when none is set.
pub(super) async fn filled(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    stored: &StoredResource,
) -> Option<StoredResource> {
    if !describes(state.rt, &stored.resource_type) {
        return None;
    }
    let mut data = stored.data.clone();
    let set = fill(state, session, &stored.resource_type, &mut data).await;
    (!set.is_empty()).then(|| StoredResource {
        data,
        ..stored.clone()
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn d(v: Value) -> Decimal {
        Decimal::parse(&v).unwrap()
    }

    #[test]
    fn decimal_adds_exactly() {
        let sum = d(json!(0.1)).add(d(json!(0.2))).unwrap();
        assert_eq!(sum.to_value(&json!({"type": "number"})), Some(json!(0.3)));
        let sum = d(json!("12.50")).add(d(json!("1.25"))).unwrap();
        assert_eq!(
            sum.to_value(&json!({"type": "string"})),
            Some(json!("13.75"))
        );
        let sum = d(json!("12.50")).add(d(json!(1.5))).unwrap();
        assert_eq!(
            sum.to_value(&json!({"type": "string"})),
            Some(json!("14.00"))
        );
        assert_eq!(sum.to_value(&json!({"type": "integer"})), Some(json!(14)));
        assert_eq!(sum.to_value(&json!({"type": "number"})), Some(json!(14)));
        let half = d(json!("-0.5"));
        assert_eq!(half.to_value(&json!({"type": "integer"})), None);
        assert_eq!(
            half.to_value(&json!({"type": "string"})),
            Some(json!("-0.5"))
        );
        assert_eq!(half.to_value(&json!({"type": "boolean"})), None);
        assert_eq!(
            d(json!(1e-2)).to_value(&json!({"type": "string"})),
            Some(json!("0.01"))
        );
        assert_eq!(
            d(json!("2E3")).to_value(&json!({"type": "integer"})),
            Some(json!(2000))
        );
    }

    #[test]
    fn decimal_refuses_what_is_no_number() {
        for v in [
            json!("abc"),
            json!(""),
            json!("."),
            json!("1.2.3"),
            json!(true),
            json!(null),
        ] {
            assert_eq!(Decimal::parse(&v), None, "{v}");
        }
        assert_eq!(Decimal::parse(&json!("1".repeat(40))), None);
    }

    #[test]
    fn a_where_compares_values_and_their_text() {
        let wanted: BTreeMap<String, Value> = [("status".to_string(), json!("posted"))]
            .into_iter()
            .collect();
        assert!(holds(&json!({"status": "posted"}), Some(&wanted)));
        assert!(!holds(&json!({"status": "pending"}), Some(&wanted)));
        assert!(!holds(&json!({}), Some(&wanted)));
        let flag: BTreeMap<String, Value> = [("ok".to_string(), json!(true))].into_iter().collect();
        assert!(holds(&json!({"ok": "true"}), Some(&flag)));
        assert!(holds(&json!({"anything": 1}), None));
    }
}
