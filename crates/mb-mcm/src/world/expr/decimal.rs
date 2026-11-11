//! Exact decimals: the sums a world's `derived` members add, and the values they
//! are written as.

use serde_json::Value;

/// Digits a [`Decimal`] holds at most, so every sum of 500 of them fits.
const MAX_DIGITS: usize = 30;

/// An exact decimal: `mantissa` × 10^-`scale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    mantissa: i128,
    scale: u32,
}

impl Decimal {
    /// Zero, at scale 0.
    pub const ZERO: Self = Self {
        mantissa: 0,
        scale: 0,
    };

    /// The whole number `n`.
    pub fn whole(n: usize) -> Option<Self> {
        Some(Self {
            mantissa: i128::try_from(n).ok()?,
            scale: 0,
        })
    }

    /// The decimal a JSON number or numeric string holds (`12.50`, `-3`, `1e-2`);
    /// `None` for anything else, or more than [`MAX_DIGITS`] digits.
    pub fn parse(value: &Value) -> Option<Self> {
        match value {
            Value::Number(n) => Self::parse_text(&n.to_string(), MAX_DIGITS),
            Value::String(text) => Self::parse_text(text.trim(), MAX_DIGITS),
            _ => None,
        }
    }

    /// The decimal [`Self::text`] wrote, whatever its number of digits (a sum may hold
    /// more than [`MAX_DIGITS`]); `None` for anything else.
    pub fn read_back(value: &Value) -> Option<Self> {
        match value {
            Value::String(text) => Self::parse_text(text, usize::MAX),
            other => Self::parse(other),
        }
    }

    fn parse_text(text: &str, max_digits: usize) -> Option<Self> {
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
        if digits.len() > max_digits {
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
            .filter(|s| *s as usize <= max_digits.min(i128::MAX.ilog10() as usize))?;
        if negative {
            mantissa = -mantissa;
        }
        Some(Self { mantissa, scale })
    }

    /// `self` + `other`, at the larger scale of the two; `None` on overflow.
    pub fn checked_add(self, other: Self) -> Option<Self> {
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
    pub fn text(&self) -> String {
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
    pub fn to_value(self, member_schema: &Value) -> Option<Value> {
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_sum_read_back_keeps_digits_past_what_a_child_holds() {
        let big = "1234567890123456789012345678901.5";
        assert_eq!(Decimal::parse(&json!(big)), None);
        let sum = Decimal::read_back(&json!(big)).unwrap();
        assert_eq!(sum.text(), big);
        assert_eq!(Decimal::read_back(&json!(3)), Decimal::whole(3));
    }

    fn d(v: Value) -> Decimal {
        Decimal::parse(&v).unwrap()
    }

    #[test]
    fn decimal_adds_exactly() {
        let sum = d(json!(0.1)).checked_add(d(json!(0.2))).unwrap();
        assert_eq!(sum.to_value(&json!({"type": "number"})), Some(json!(0.3)));
        let sum = d(json!("12.50")).checked_add(d(json!("1.25"))).unwrap();
        assert_eq!(
            sum.to_value(&json!({"type": "string"})),
            Some(json!("13.75"))
        );
        let sum = d(json!("12.50")).checked_add(d(json!(1.5))).unwrap();
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
}
