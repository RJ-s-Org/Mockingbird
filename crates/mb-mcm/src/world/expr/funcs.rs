//! The plain functions: strings, conversions, hashes and the session's `seq` and `mint`.

use md5::Md5;
use regex::Regex;
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha256};

use super::eval::{Num, double, type_name};
use super::parse::Func;
use super::{Decimal, Fail, Host, MAX_SPLIT_ITEMS, compile_regex};

pub(super) fn call(
    func: Func,
    args: &[Value],
    re: Option<&Regex>,
    host: &mut dyn Host,
    pos: usize,
) -> Result<Value, Fail> {
    let name = func.name();
    let arg = |i: usize| args.get(i).unwrap_or(&Value::Null);
    let string = |i: usize| -> Result<&str, Fail> {
        match arg(i) {
            Value::String(s) => Ok(s),
            v => Err(Fail::new(
                pos,
                format!("{name}() needs a string, got {}", type_name(v)),
            )),
        }
    };
    Ok(match func {
        Func::Size => match arg(0) {
            Value::String(s) => Value::from(s.chars().count()),
            Value::Array(a) => Value::from(a.len()),
            Value::Object(m) => Value::from(m.len()),
            v => {
                return Err(Fail::new(
                    pos,
                    format!("size() needs a string, list or map, got {}", type_name(v)),
                ));
            }
        },
        Func::StartsWith => Value::Bool(string(0)?.starts_with(string(1)?)),
        Func::EndsWith => Value::Bool(string(0)?.ends_with(string(1)?)),
        Func::Contains => Value::Bool(string(0)?.contains(string(1)?)),
        Func::Matches => {
            let s = string(0)?;
            let hit = match re {
                Some(re) => re.is_match(s),
                None => compile_regex(string(1)?)
                    .map_err(|m| Fail::new(pos, m))?
                    .is_match(s),
            };
            Value::Bool(hit)
        }
        Func::Lower => Value::String(string(0)?.to_lowercase()),
        Func::Upper => Value::String(string(0)?.to_uppercase()),
        Func::Slug => Value::String(slug(string(0)?)),
        Func::String => Value::String(match arg(0) {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => match Num::of(arg(0)) {
                Some(Num::I(i)) => i.to_string(),
                Some(Num::D(d)) => d.to_string(),
                None => n.to_string(),
            },
            v => {
                return Err(Fail::new(
                    pos,
                    format!("string() cannot convert a {}", type_name(v)),
                ));
            }
        }),
        Func::Int => match arg(0) {
            Value::String(s) => Value::from(s.parse::<i64>().map_err(|_| {
                Fail::new(
                    pos,
                    format!(
                        "int() cannot parse a string of {} characters",
                        s.chars().count()
                    ),
                )
            })?),
            v => match Num::of(v) {
                Some(Num::I(i)) => Value::from(i),
                Some(Num::D(d)) => {
                    // Truncates toward zero; NaN fails both comparisons.
                    let t = d.trunc();
                    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&t) {
                        return Err(Fail::new(pos, "int() out of range"));
                    }
                    Value::from(t as i64)
                }
                None => {
                    return Err(Fail::new(
                        pos,
                        format!("int() cannot convert a {}", type_name(v)),
                    ));
                }
            },
        },
        Func::Double => match arg(0) {
            Value::String(s) => match s.parse::<f64>() {
                Ok(d) if d.is_finite() => double(d, pos)?,
                _ => {
                    return Err(Fail::new(
                        pos,
                        format!(
                            "double() cannot parse a string of {} characters",
                            s.chars().count()
                        ),
                    ));
                }
            },
            v => match Num::of(v) {
                Some(Num::I(i)) => double(i as f64, pos)?,
                Some(Num::D(d)) => double(d, pos)?,
                None => {
                    return Err(Fail::new(
                        pos,
                        format!("double() cannot convert a {}", type_name(v)),
                    ));
                }
            },
        },
        Func::Md5 => Value::String(hex(&Md5::digest(string(0)?.as_bytes()))),
        Func::Sha1 => Value::String(hex(&Sha1::digest(string(0)?.as_bytes()))),
        Func::Sha256 => Value::String(hex(&Sha256::digest(string(0)?.as_bytes()))),
        Func::Seq => Value::from(host.seq(string(0)?)),
        Func::DecimalSum => {
            let Value::Array(items) = arg(0) else {
                return Err(Fail::new(
                    pos,
                    format!("decimal_sum() needs a list, got {}", type_name(arg(0))),
                ));
            };
            let member = string(1)?;
            let mut sum = Decimal::ZERO;
            for item in items {
                match crate::member_at(item, member) {
                    None | Some(Value::Null) => {}
                    Some(v) => {
                        sum = Decimal::parse(v)
                            .and_then(|d| sum.checked_add(d))
                            .ok_or_else(|| {
                                Fail::new(pos, format!("decimal_sum() cannot add {}", type_name(v)))
                            })?;
                    }
                }
            }
            Value::String(sum.text())
        }
        Func::Trim => Value::String(string(0)?.trim().to_string()),
        Func::Split => {
            let (s, sep) = (string(0)?, string(1)?);
            let parts: Vec<&str> = if sep.is_empty() {
                // Each character, as CEL's split on an empty separator gives.
                s.char_indices()
                    .map(|(i, c)| s.get(i..i + c.len_utf8()).unwrap_or_default())
                    .take(MAX_SPLIT_ITEMS + 1)
                    .collect()
            } else {
                s.split(sep).take(MAX_SPLIT_ITEMS + 1).collect()
            };
            if parts.len() > MAX_SPLIT_ITEMS {
                return Err(Fail::new(
                    pos,
                    format!("split() gives more than {MAX_SPLIT_ITEMS} items"),
                ));
            }
            Value::Array(parts.into_iter().map(Value::from).collect())
        }
        Func::Substring => {
            let s = string(0)?;
            let index = |i: usize| -> Result<Option<i64>, Fail> {
                match args.get(i) {
                    None => Ok(None),
                    Some(v) => match Num::of(v) {
                        Some(Num::I(n)) => Ok(Some(n)),
                        _ => Err(Fail::new(
                            pos,
                            format!("substring() needs an int index, got {}", type_name(v)),
                        )),
                    },
                }
            };
            let len = s.chars().count();
            let clamp = |n: i64| usize::try_from(n.max(0)).map_or(len, |n| n.min(len));
            let start = index(1)?.map_or(0, clamp);
            let end = index(2)?.map_or(len, clamp);
            Value::String(
                s.chars()
                    .skip(start)
                    .take(end.saturating_sub(start))
                    .collect(),
            )
        }
        Func::Replace => Value::String(string(0)?.replace(string(1)?, string(2)?)),
        Func::Mint => {
            let pattern = string(0)?;
            Value::String(host.mint(pattern).ok_or_else(|| {
                Fail::new(
                    pos,
                    format!(
                        "mint() cannot generate from a pattern of {} characters",
                        pattern.chars().count()
                    ),
                )
            })?)
        }
    })
}

/// The bytes `func` would build from `args`, where it can build far more than it reads
/// (`replace`), so that it is charged before the value is made.
pub(super) fn output_bytes(func: Func, args: &[Value]) -> u64 {
    let text = |i: usize| args.get(i).and_then(Value::as_str);
    match (func, text(0), text(1), text(2)) {
        (Func::Replace, Some(s), Some(from), Some(to)) => {
            let hits = if from.is_empty() {
                s.chars().count().saturating_add(1)
            } else {
                s.matches(from).count()
            };
            let hits = u64::try_from(hits).unwrap_or(u64::MAX);
            let to = u64::try_from(to.len()).unwrap_or(u64::MAX);
            u64::try_from(s.len())
                .unwrap_or(u64::MAX)
                .saturating_add(hits.saturating_mul(to))
        }
        _ => 0,
    }
}

/// Lowercase hex.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0xf)]));
    }
    s
}

/// Lowercase ASCII letters and digits; every run of anything else becomes one
/// `-`, and none leads or trails.
pub(super) fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            dash = true;
        }
    }
    out
}
