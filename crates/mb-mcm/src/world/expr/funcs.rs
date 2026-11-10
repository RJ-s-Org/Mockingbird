//! The plain functions: strings, conversions, hashes and the session's `seq` and `mint`.

use regex::Regex;
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha256};

use super::eval::{Num, double, type_name};
use super::parse::Func;
use super::{Fail, Host, compile_regex, md5};

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
            Value::String(s) => Value::from(
                s.parse::<i64>()
                    .map_err(|_| Fail::new(pos, format!("int() cannot parse {s:?}")))?,
            ),
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
                _ => return Err(Fail::new(pos, format!("double() cannot parse {s:?}"))),
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
        Func::Md5 => Value::String(hex(&md5::digest(string(0)?.as_bytes()))),
        Func::Sha1 => Value::String(hex(&Sha1::digest(string(0)?.as_bytes()))),
        Func::Sha256 => Value::String(hex(&Sha256::digest(string(0)?.as_bytes()))),
        Func::Seq => Value::from(host.seq(string(0)?)),
        Func::Mint => {
            let pattern = string(0)?;
            Value::String(host.mint(pattern).ok_or_else(|| {
                Fail::new(pos, format!("mint() cannot generate from {pattern:?}"))
            })?)
        }
    })
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
