//! XML ⇄ JSON per the importer's annotated JSON Schemas.
//!
//! Attributes are `@<name>` members and simple content is `#text`.
//! Arrays, numbers and booleans are recognised from the schema; anything
//! else is a string. Element names, order and namespaces come from the
//! `x-xml-root`, `x-xml-order`, `x-xml-namespace` and `x-xml-qualified`
//! annotations.

use std::fmt::Write as _;

use quick_xml::escape::escape;
use serde_json::{Map, Number, Value};

use super::envelope::{Element, XSI_NS, body_child};

/// `$ref`/`anyOf` hops followed before giving up on a schema.
const MAX_HOPS: usize = 32;

/// Schemas resolved against the root that carries the `$defs`.
#[derive(Clone, Copy)]
struct Schemas<'a> {
    root: &'a Value,
}

impl<'a> Schemas<'a> {
    /// `schema` with `$ref`s followed and a nullable `anyOf`/`oneOf`
    /// reduced to its first non-null branch.
    fn effective(self, schema: &'a Value) -> &'a Value {
        let mut s = schema;
        for _ in 0..MAX_HOPS {
            if let Some(r) = s.get("$ref").and_then(Value::as_str) {
                match r.strip_prefix('#').and_then(|p| self.root.pointer(p)) {
                    Some(target) => s = target,
                    None => return s,
                }
            } else if let Some(branches) = s
                .get("anyOf")
                .or_else(|| s.get("oneOf"))
                .and_then(Value::as_array)
            {
                match branches.iter().find(|b| !is_null_schema(b)) {
                    Some(b) => s = b,
                    None => return s,
                }
            } else {
                return s;
            }
        }
        s
    }

    /// The JSON type `schema` describes, if any.
    fn kind(self, schema: Option<&'a Value>) -> Kind {
        let Some(s) = schema.map(|s| self.effective(s)) else {
            return Kind::Unknown;
        };
        let ty = match s.get("type") {
            Some(Value::String(t)) => Some(t.as_str()),
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null"),
            _ => None,
        };
        match ty {
            Some("object") => Kind::Object,
            Some("array") => Kind::Array,
            Some("integer") => Kind::Integer,
            Some("number") => Kind::Number,
            Some("boolean") => Kind::Boolean,
            Some(_) => Kind::String,
            None if s.get("properties").is_some() => Kind::Object,
            None if s.get("items").is_some() => Kind::Array,
            None => Kind::Unknown,
        }
    }

    fn property(self, object: Option<&'a Value>, key: &str) -> Option<&'a Value> {
        object
            .map(|s| self.effective(s))
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.get(key))
    }

    fn items(self, array: Option<&'a Value>) -> Option<&'a Value> {
        array
            .map(|s| self.effective(s))
            .and_then(|s| s.get("items"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Object,
    Array,
    Integer,
    Number,
    Boolean,
    String,
    Unknown,
}

fn is_null_schema(s: &Value) -> bool {
    s.get("type").and_then(Value::as_str) == Some("null")
}

/// The first child of the envelope's Body, converted per `schema_with_defs`.
pub fn envelope_body_to_json(envelope: &str, schema_with_defs: &Value) -> Result<Value, String> {
    let child = body_child(envelope)?;
    Ok(element_to_json(&child, schema_with_defs))
}

/// `el` converted per `schema_with_defs` (which carries the `$defs` its
/// references point into). Conversion never fails: text that does not fit
/// the schema's type stays a string, for the validator to report.
pub(crate) fn element_to_json(el: &Element, schema_with_defs: &Value) -> Value {
    let schemas = Schemas {
        root: schema_with_defs,
    };
    decode(schemas, el, Some(schema_with_defs))
}

fn decode<'a>(schemas: Schemas<'a>, el: &Element, schema: Option<&'a Value>) -> Value {
    if el.is_nil() {
        return Value::Null;
    }
    match schemas.kind(schema) {
        Kind::Object => decode_object(schemas, el, schema),
        Kind::Array => Value::Array(vec![decode(schemas, el, schemas.items(schema))]),
        Kind::Unknown if !el.children.is_empty() || !own_attrs(el).is_empty() => {
            decode_object(schemas, el, schema)
        }
        kind => scalar(&el.text, kind),
    }
}

/// Attributes other than `xsi:*`.
fn own_attrs(el: &Element) -> Vec<&(super::envelope::QName, String)> {
    el.attrs.iter().filter(|(n, _)| n.ns != XSI_NS).collect()
}

fn decode_object<'a>(schemas: Schemas<'a>, el: &Element, schema: Option<&'a Value>) -> Value {
    let mut out = Map::new();
    for (name, value) in own_attrs(el) {
        let key = format!("@{}", name.local);
        let kind = schemas.kind(schemas.property(schema, &key));
        out.insert(key, scalar(value, kind));
    }
    for child in &el.children {
        let key = child.name.local.as_str();
        let prop = schemas.property(schema, key);
        if schemas.kind(prop) == Kind::Array {
            let item = decode(schemas, child, schemas.items(prop));
            match out.get_mut(key) {
                Some(Value::Array(items)) => items.push(item),
                _ => {
                    out.insert(key.to_string(), Value::Array(vec![item]));
                }
            }
            continue;
        }
        let value = decode(schemas, child, prop);
        match out.get_mut(key) {
            // An undeclared repeat: keep every occurrence.
            Some(Value::Array(items)) if prop.is_none() => items.push(value),
            Some(existing) => {
                let first = existing.take();
                *existing = Value::Array(vec![first, value]);
            }
            None => {
                out.insert(key.to_string(), value);
            }
        }
    }
    let text_schema = schemas.property(schema, "#text");
    if text_schema.is_some() || (el.children.is_empty() && !el.text.trim().is_empty()) {
        out.insert("#text".into(), scalar(&el.text, schemas.kind(text_schema)));
    }
    Value::Object(out)
}

fn scalar(text: &str, kind: Kind) -> Value {
    let trimmed = text.trim();
    let parsed = match kind {
        Kind::Integer => integer(trimmed),
        Kind::Number => integer(trimmed).or_else(|| {
            trimmed
                .parse::<f64>()
                .ok()
                .and_then(Number::from_f64)
                .map(Value::Number)
        }),
        Kind::Boolean => match trimmed {
            "true" | "1" => Some(Value::Bool(true)),
            "false" | "0" => Some(Value::Bool(false)),
            _ => None,
        },
        _ => None,
    };
    parsed.unwrap_or_else(|| Value::String(text.to_string()))
}

fn integer(text: &str) -> Option<Value> {
    text.parse::<i64>()
        .map(Value::from)
        .or_else(|_| text.parse::<u64>().map(Value::from))
        .ok()
}

/// `value` as the XML element named by the schema's `x-xml-root`.
pub(crate) fn json_to_xml(value: &Value, schema_with_defs: &Value) -> Result<String, String> {
    let root = schema_with_defs
        .get("x-xml-root")
        .ok_or("the schema has no x-xml-root")?;
    let name = root
        .get("name")
        .and_then(Value::as_str)
        .ok_or("x-xml-root has no name")?;
    let ns = root.get("namespace").and_then(Value::as_str).unwrap_or("");
    let mut w = Writer {
        schemas: Schemas {
            root: schema_with_defs,
        },
        out: String::new(),
        scope: Vec::new(),
        next_prefix: 0,
    };
    w.element(ns, name, value, Some(schema_with_defs));
    Ok(w.out)
}

struct Writer<'a> {
    schemas: Schemas<'a>,
    out: String,
    /// In-scope `(namespace, prefix)` declarations, innermost last.
    scope: Vec<(String, String)>,
    next_prefix: usize,
}

impl<'a> Writer<'a> {
    /// The prefix bound to `ns`, and the declaration to add when it is not
    /// in scope yet (pushed onto the scope; the caller pops it).
    fn prefix(&mut self, ns: &str, preferred: Option<&str>) -> (String, Option<String>) {
        if let Some((_, p)) = self.scope.iter().rev().find(|(n, _)| n == ns) {
            return (p.clone(), None);
        }
        let prefix = preferred.map_or_else(
            || {
                let p = format!("ns{}", self.next_prefix);
                self.next_prefix += 1;
                p
            },
            str::to_string,
        );
        self.scope.push((ns.to_string(), prefix.clone()));
        let decl = format!(" xmlns:{prefix}=\"{}\"", escape(ns));
        (prefix, Some(decl))
    }

    fn element(&mut self, ns: &str, local: &str, value: &Value, schema: Option<&'a Value>) {
        if let Value::Array(items) = value {
            let items_schema = match self.schemas.kind(schema) {
                Kind::Array => self.schemas.items(schema),
                _ => schema,
            };
            for item in items {
                self.element(ns, local, item, items_schema);
            }
            return;
        }
        let depth = self.scope.len();
        let mut decls = String::new();
        let tag = if ns.is_empty() {
            local.to_string()
        } else {
            let (prefix, decl) = self.prefix(ns, None);
            decls.extend(decl);
            format!("{prefix}:{local}")
        };
        match value {
            Value::Null => {
                let (xsi, decl) = self.prefix(XSI_NS, Some("xsi"));
                decls.extend(decl);
                let _ = write!(self.out, "<{tag}{decls} {xsi}:nil=\"true\"/>");
            }
            Value::Object(members) => self.object(&tag, &decls, members, schema),
            scalar => {
                let _ = write!(self.out, "<{tag}{decls}>{}</{tag}>", text(scalar));
            }
        }
        self.scope.truncate(depth);
    }

    fn object(
        &mut self,
        tag: &str,
        decls: &str,
        members: &Map<String, Value>,
        schema: Option<&'a Value>,
    ) {
        let object = schema.map(|s| self.schemas.effective(s));
        let qualified = object
            .and_then(|o| o.get("x-xml-qualified"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let child_ns = if qualified {
            object
                .and_then(|o| o.get("x-xml-namespace"))
                .and_then(Value::as_str)
                .unwrap_or("")
        } else {
            ""
        };
        let _ = write!(self.out, "<{tag}{decls}");
        for (key, value) in members {
            if let Some(name) = key.strip_prefix('@')
                && !value.is_null()
                && is_ncname(name)
                && name != "xmlns"
            {
                let _ = write!(self.out, " {name}=\"{}\"", text(value));
            }
        }
        self.out.push('>');
        if let Some(t) = members.get("#text").filter(|v| !v.is_null()) {
            self.out.push_str(&text(t));
        }
        for key in element_order(members, object) {
            let prop = self.schemas.property(schema, key);
            if let Some(value) = members.get(key) {
                self.element(child_ns, key, value, prop);
            }
        }
        let _ = write!(self.out, "</{tag}>");
    }
}

/// The element members of `members`: those listed in `x-xml-order` first,
/// in that order, then the others sorted.
fn element_order<'m>(members: &'m Map<String, Value>, object: Option<&Value>) -> Vec<&'m str> {
    let is_element = |k: &str| !k.starts_with('@') && k != "#text" && is_ncname(k);
    let listed: Vec<&str> = object
        .and_then(|o| o.get("x-xml-order"))
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut order: Vec<&'m str> = listed
        .iter()
        .filter_map(|name| members.get_key_value(*name).map(|(k, _)| k.as_str()))
        .filter(|k| is_element(k))
        .collect();
    let mut rest: Vec<&'m str> = members
        .keys()
        .map(String::as_str)
        .filter(|k| is_element(k) && !listed.contains(k))
        .collect();
    rest.sort_unstable();
    order.extend(rest);
    order
}

/// Whether `name` is an XML NCName (a name without a colon), so it can be
/// written as an element or attribute name. Members with other keys (from
/// an LLM item or a scenario patch) are skipped rather than written raw.
fn is_ncname(name: &str) -> bool {
    let mut chars = name.chars();
    let start = |c: char| c == '_' || c.is_alphabetic();
    chars.next().is_some_and(start)
        && chars.all(|c| start(c) || c.is_alphanumeric() || matches!(c, '-' | '.' | '\u{B7}'))
}

/// A scalar's escaped text; objects and arrays as escaped JSON.
fn text(value: &Value) -> String {
    match value {
        Value::String(s) => escape(s.as_str()).into_owned(),
        Value::Null => String::new(),
        Value::Bool(_) | Value::Number(_) => value.to_string(),
        other => escape(other.to_string()).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use serde_json::json;

    use super::super::envelope::{Version, envelope};
    use super::*;

    fn account_schema() -> Value {
        json!({
            "x-xml-root": {"name": "GetAccountResponse", "namespace": "urn:a"},
            "type": "object",
            "x-xml-namespace": "urn:a",
            "x-xml-qualified": false,
            "x-xml-order": ["id", "account"],
            "properties": {
                "id": {"type": "string"},
                "account": {"$ref": "#/$defs/Account"},
            },
            "$defs": {
                "Account": {
                    "type": "object",
                    "x-xml-namespace": "urn:a",
                    "x-xml-qualified": true,
                    "x-xml-order": ["name", "tags", "age", "active", "score"],
                    "properties": {
                        "@id": {"type": "string"},
                        "@rank": {"type": "integer"},
                        "name": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "age": {"anyOf": [{"type": "integer"}, {"type": "null"}]},
                        "active": {"type": "boolean"},
                        "score": {"type": "number"},
                        "note": {"$ref": "#/$defs/Note"},
                    },
                },
                "Note": {
                    "type": "object",
                    "properties": {"#text": {"type": "string"}, "@lang": {"type": "string"}},
                },
            },
        })
    }

    #[test]
    fn round_trips_through_xml() {
        let schema = account_schema();
        let value = json!({
            "id": "a-1",
            "account": {
                "@id": "a-1",
                "@rank": 3,
                "zzz": "extra",
                "score": 1.5,
                "active": true,
                "age": null,
                "tags": ["x", "y & z"],
                "name": "Ada <3",
                "note": {"#text": "hi", "@lang": "en"},
            },
        });
        let xml = json_to_xml(&value, &schema).unwrap();
        assert_eq!(
            xml,
            concat!(
                r#"<ns0:GetAccountResponse xmlns:ns0="urn:a"><id>a-1</id>"#,
                r#"<account id="a-1" rank="3"><ns0:name>Ada &lt;3</ns0:name>"#,
                r#"<ns0:tags>x</ns0:tags><ns0:tags>y &amp; z</ns0:tags>"#,
                r#"<ns0:age xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:nil="true"/>"#,
                r#"<ns0:active>true</ns0:active><ns0:score>1.5</ns0:score>"#,
                r#"<ns0:note lang="en">hi</ns0:note><ns0:zzz>extra</ns0:zzz></account>"#,
                r#"</ns0:GetAccountResponse>"#,
            )
        );
        let back = envelope_body_to_json(&envelope(Version::V11, &xml), &schema).unwrap();
        assert_eq!(back, value);
    }

    #[test]
    fn single_array_items_and_bad_scalars() {
        let schema = account_schema();
        let xml = r#"<r xmlns:a="urn:a"><account><a:tags>only</a:tags><a:age>old</a:age><a:active>0</a:active><a:score>2</a:score></account></r>"#;
        let back = envelope_body_to_json(&envelope(Version::V12, xml), &schema).unwrap();
        assert_eq!(
            back,
            json!({"account": {"tags": ["only"], "age": "old", "active": false, "score": 2}})
        );
    }

    #[test]
    fn keys_that_are_not_xml_names_are_skipped() {
        let schema = account_schema();
        let value = json!({
            "id": "a-1",
            "a b": 1,
            "1x": 2,
            "x><y": 3,
            "p:q": 4,
            "": 5,
            "ok-name.é_2": 6,
            "account": {
                "@a b": "1",
                "@x=\"1\" y": "2",
                "@xmlns": "urn:evil",
                "@xmlns:e": "urn:evil",
                "@": "3",
                "@good": "4",
            },
        });
        let xml = json_to_xml(&value, &schema).unwrap();
        assert_eq!(
            xml,
            concat!(
                r#"<ns0:GetAccountResponse xmlns:ns0="urn:a"><id>a-1</id>"#,
                r#"<account good="4"></account><ok-name.é_2>6</ok-name.é_2>"#,
                r#"</ns0:GetAccountResponse>"#,
            )
        );
        // Still well-formed.
        envelope_body_to_json(&envelope(Version::V11, &xml), &schema).unwrap();
    }

    #[test]
    fn missing_root_annotation_is_an_error() {
        assert!(json_to_xml(&json!({}), &json!({"type": "object"})).is_err());
    }
}
