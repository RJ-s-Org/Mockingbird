//! The HAR and Postman markers of a JSON document, read without building a
//! `Value` tree.
//!
//! Every value is still visited through `deserialize_any`, so a document is
//! accepted exactly when `serde_json::from_slice::<Value>` accepts it (same
//! syntax, number range and nesting limit); only the allocations of the
//! tree are skipped.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// The markers of a top-level object: `/log/entries` is an array,
/// `/info/schema` names schema.getpostman.com, `/info/_postman_id` and
/// `/item` are present. A repeated key counts as its last occurrence, as in
/// a `Value`.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct Markers {
    pub(crate) log_entries: bool,
    pub(crate) postman_schema: bool,
    pub(crate) postman_id: bool,
    pub(crate) item: bool,
}

/// The markers of `bytes`, or None when it is not valid JSON.
pub(crate) fn markers(bytes: &[u8]) -> Option<Markers> {
    serde_json::from_slice::<Probe<Root>>(bytes)
        .ok()
        .map(|p| p.0.0)
}

/// What a value of interest yields. Values of other types are consumed
/// (and so validated) and give the default.
trait Probed: Default {
    fn str(_: &str) -> Self {
        Self::default()
    }
    fn seq() -> Self {
        Self::default()
    }
    fn map<'de, A: MapAccess<'de>>(map: A) -> Result<Self, A::Error> {
        drain_map(map)?;
        Ok(Self::default())
    }
}

/// A value read as `T`.
struct Probe<T>(T);

impl<'de, T: Probed> Deserialize<'de> for Probe<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(ProbeVisitor(std::marker::PhantomData))
    }
}

struct ProbeVisitor<T>(std::marker::PhantomData<T>);

impl<'de, T: Probed> Visitor<'de> for ProbeVisitor<T> {
    type Value = Probe<T>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Probe(T::default()))
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Probe(T::default()))
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Probe(T::default()))
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Probe(T::default()))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Probe(T::default()))
    }
    fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
        Ok(Probe(T::str(s)))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<Probe<()>>()?.is_some() {}
        Ok(Probe(T::seq()))
    }
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        T::map(map).map(Probe)
    }
}

/// Any value, only validated.
impl Probed for () {}

fn drain_map<'de, A: MapAccess<'de>>(mut map: A) -> Result<(), A::Error> {
    while map.next_entry::<Probe<()>, Probe<()>>()?.is_some() {}
    Ok(())
}

/// A string key of interest, or another key.
#[derive(Default)]
struct Key(Option<&'static str>);

/// The keys the probes look at.
const KEYS: [&str; 6] = ["log", "entries", "info", "schema", "_postman_id", "item"];

impl Probed for Key {
    fn str(s: &str) -> Self {
        Key(KEYS.iter().copied().find(|k| *k == s))
    }
}

/// Whether a value is an array.
#[derive(Default)]
struct IsArray(bool);

impl Probed for IsArray {
    fn seq() -> Self {
        IsArray(true)
    }
}

/// Whether a value is a string naming schema.getpostman.com.
#[derive(Default)]
struct PostmanSchema(bool);

impl Probed for PostmanSchema {
    fn str(s: &str) -> Self {
        PostmanSchema(s.contains("schema.getpostman.com"))
    }
}

/// `log`: whether it is an object whose `entries` is an array.
#[derive(Default)]
struct Log(bool);

impl Probed for Log {
    fn map<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut out = Log(false);
        while let Some(Probe(Key(key))) = map.next_key::<Probe<Key>>()? {
            match key {
                Some("entries") => out.0 = map.next_value::<Probe<IsArray>>()?.0.0,
                _ => {
                    map.next_value::<Probe<()>>()?;
                }
            }
        }
        Ok(out)
    }
}

/// `info`: its `schema` and `_postman_id` markers.
#[derive(Default)]
struct Info {
    schema: bool,
    id: bool,
}

impl Probed for Info {
    fn map<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut out = Info::default();
        while let Some(Probe(Key(key))) = map.next_key::<Probe<Key>>()? {
            match key {
                Some("schema") => out.schema = map.next_value::<Probe<PostmanSchema>>()?.0.0,
                Some("_postman_id") => {
                    map.next_value::<Probe<()>>()?;
                    out.id = true;
                }
                _ => {
                    map.next_value::<Probe<()>>()?;
                }
            }
        }
        Ok(out)
    }
}

/// The document.
#[derive(Default)]
struct Root(Markers);

impl Probed for Root {
    fn map<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut out = Markers::default();
        while let Some(Probe(Key(key))) = map.next_key::<Probe<Key>>()? {
            match key {
                Some("log") => out.log_entries = map.next_value::<Probe<Log>>()?.0.0,
                Some("info") => {
                    let info = map.next_value::<Probe<Info>>()?.0;
                    out.postman_schema = info.schema;
                    out.postman_id = info.id;
                }
                Some("item") => {
                    map.next_value::<Probe<()>>()?;
                    out.item = true;
                }
                _ => {
                    map.next_value::<Probe<()>>()?;
                }
            }
        }
        Ok(Root(out))
    }
}

#[cfg(test)]
mod tests {
    use super::{Markers, markers};
    use serde_json::Value;

    /// The markers as the `Value`-based check computed them.
    fn from_value(bytes: &[u8]) -> Option<Markers> {
        let doc: Value = serde_json::from_slice(bytes).ok()?;
        Some(Markers {
            log_entries: doc.pointer("/log/entries").is_some_and(Value::is_array),
            postman_schema: doc
                .pointer("/info/schema")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains("schema.getpostman.com")),
            postman_id: doc.pointer("/info/_postman_id").is_some(),
            item: doc.get("item").is_some(),
        })
    }

    #[test]
    fn markers_match_the_value_tree() {
        let deep = format!("{{\"a\":{}{}}}", "[".repeat(200), "]".repeat(200));
        let cases: Vec<String> = [
            r#"{"log":{"entries":[]}}"#,
            r#"{"log":{"entries":{}}}"#,
            r#"{"log":{"entries":[1],"entries":2}}"#,
            r#"{"log":{"entries":2,"entries":[{"a":[1,2,{"b":null}]}]}}"#,
            r#"{"log":[{"entries":[]}]}"#,
            r#"{"log":{"entries":[]},"log":1}"#,
            r#"{"info":{"schema":"https://schema.getpostman.com/json/collection/v2.1.0/"}}"#,
            r#"{"info":{"schema":"https:\/\/schema.getpostman.com\/x"}}"#,
            r#"{"info":{"schema":5}}"#,
            r#"{"info":{"_postman_id":null},"item":[]}"#,
            r#"{"info":{"_postman_id":"x"},"info":{}}"#,
            r#"{"info":[{"schema":"schema.getpostman.com"}],"item":0}"#,
            r#"{"log":{"entries":[]}}"#,
            r#"{"x":1e400}"#,
            r#"{"x":-12.5e-3,"y":18446744073709551615,"z":-9223372036854775808}"#,
            r#"{"log":{"entries":[]}} trailing"#,
            r#"{"log":{"entries":[]},}"#,
            r#"{"s":"\ud800"}"#,
            "{\"s\":\"\u{7f}\"}",
            r#"{"a":"unterminated}"#,
            "{}",
            r#"[{"log":{"entries":[]}}]"#,
            r#""log""#,
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([deep])
        .collect();
        for case in &cases {
            assert_eq!(
                markers(case.as_bytes()),
                from_value(case.as_bytes()),
                "{case}"
            );
        }
        let invalid_utf8 = b"{\"s\":\"\xff\"}";
        assert_eq!(markers(invalid_utf8), from_value(invalid_utf8));
    }
}
