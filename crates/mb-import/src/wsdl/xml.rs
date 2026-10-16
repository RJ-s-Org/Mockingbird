//! A minimal namespace-resolving XML DOM over quick-xml.
//!
//! DOCTYPE declarations are rejected and no entity is ever expanded: only
//! the five predefined entities and character references are resolved.

use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
/// Deeper documents are rejected.
const MAX_DEPTH: usize = 256;

/// An expanded name: namespace URI (empty for none) and local name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct QName {
    pub ns: String,
    pub local: String,
}

impl QName {
    pub(crate) fn new(ns: &str, local: &str) -> Self {
        QName {
            ns: ns.to_owned(),
            local: local.to_owned(),
        }
    }
}

impl fmt::Display for QName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ns.is_empty() {
            f.write_str(&self.local)
        } else {
            write!(f, "{{{}}}{}", self.ns, self.local)
        }
    }
}

/// In-scope prefix bindings (`""` is the default namespace).
type Scope = Rc<BTreeMap<String, String>>;

#[derive(Clone, Debug)]
pub(crate) struct Element {
    pub name: QName,
    /// Attributes other than namespace declarations.
    pub attrs: Vec<(QName, String)>,
    pub children: Vec<Element>,
    /// The element's own character data, concatenated.
    pub text: String,
    scope: Scope,
}

impl Element {
    /// The value of the unqualified attribute `local`.
    pub(crate) fn attr(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n.ns.is_empty() && n.local == local)
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn is(&self, ns: &str, local: &str) -> bool {
        self.name.ns == ns && self.name.local == local
    }

    /// The children named `{ns}local`, in document order.
    pub(crate) fn children_named<'a>(
        &'a self,
        ns: &'a str,
        local: &'a str,
    ) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.is(ns, local))
    }

    pub(crate) fn child<'a>(&'a self, ns: &'a str, local: &'a str) -> Option<&'a Element> {
        self.children_named(ns, local).next()
    }

    /// The children in namespace `ns`, in document order.
    pub(crate) fn children_in<'a>(&'a self, ns: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name.ns == ns)
    }

    /// Resolve a `prefix:local` attribute value against this element's
    /// namespace bindings; an unprefixed value takes the default namespace.
    /// `None` for an unbound prefix.
    pub(crate) fn resolve(&self, value: &str) -> Option<QName> {
        let value = value.trim();
        let (prefix, local) = value.split_once(':').unwrap_or(("", value));
        let ns = self.scope.get(prefix).cloned().or_else(|| {
            if prefix.is_empty() {
                Some(String::new())
            } else {
                None
            }
        })?;
        Some(QName {
            ns,
            local: local.to_owned(),
        })
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum XmlError {
    /// The document declares a DOCTYPE.
    Doctype,
    Syntax(String),
}

impl From<quick_xml::Error> for XmlError {
    fn from(e: quick_xml::Error) -> Self {
        XmlError::Syntax(e.to_string())
    }
}

fn split(raw: &str) -> (&str, &str) {
    raw.split_once(':').unwrap_or(("", raw))
}

fn open(e: &BytesStart<'_>, parent: &Scope) -> Result<Element, XmlError> {
    let mut bindings: Option<BTreeMap<String, String>> = None;
    let mut raw_attrs = Vec::new();
    for attr in e.attributes() {
        let attr = attr.map_err(|e| XmlError::Syntax(e.to_string()))?;
        let key = attr.key.as_ref().to_owned();
        let value = attr.normalized_value(XmlVersion::Implicit1_0)?.into_owned();
        let declared = if key == "xmlns" {
            Some(String::new())
        } else {
            key.strip_prefix("xmlns:").map(str::to_owned)
        };
        match declared {
            Some(prefix) => {
                bindings
                    .get_or_insert_with(|| (**parent).clone())
                    .insert(prefix, value);
            }
            None => raw_attrs.push((key, value)),
        }
    }
    let scope = bindings.map_or_else(|| Rc::clone(parent), Rc::new);
    let lookup = |prefix: &str| -> Result<String, XmlError> {
        if prefix == "xml" {
            return Ok(XML_NS.to_owned());
        }
        scope
            .get(prefix)
            .cloned()
            .ok_or_else(|| XmlError::Syntax(format!("unbound namespace prefix \"{prefix}\"")))
    };
    let raw_name = e.name().as_ref().to_owned();
    let (prefix, local) = split(&raw_name);
    let ns = if prefix.is_empty() {
        scope.get("").cloned().unwrap_or_default()
    } else {
        lookup(prefix)?
    };
    let mut attrs = Vec::with_capacity(raw_attrs.len());
    for (key, value) in raw_attrs {
        let (prefix, local) = split(&key);
        let ns = if prefix.is_empty() {
            String::new()
        } else {
            lookup(prefix)?
        };
        attrs.push((QName::new(&ns, local), value));
    }
    Ok(Element {
        name: QName::new(&ns, local),
        attrs,
        children: Vec::new(),
        text: String::new(),
        scope,
    })
}

/// Parse `text` into its root element.
pub(crate) fn parse(text: &str) -> Result<Element, XmlError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut reader = Reader::from_str(text);
    reader.config_mut().check_end_names = true;
    let root_scope: Scope = Rc::new(BTreeMap::new());
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    loop {
        match reader.read_event()? {
            Event::DocType(_) => return Err(XmlError::Doctype),
            Event::Start(e) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(XmlError::Syntax(format!(
                        "elements are nested deeper than {MAX_DEPTH}"
                    )));
                }
                let scope = stack.last().map_or(&root_scope, |p| &p.scope);
                let el = open(&e, scope)?;
                if root.is_some() && stack.is_empty() {
                    return Err(XmlError::Syntax("more than one root element".into()));
                }
                stack.push(el);
            }
            Event::Empty(e) => {
                let scope = stack.last().map_or(&root_scope, |p| &p.scope);
                let el = open(&e, scope)?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None if root.is_none() => root = Some(el),
                    None => return Err(XmlError::Syntax("more than one root element".into())),
                }
            }
            Event::End(_) => {
                let el = stack
                    .pop()
                    .ok_or_else(|| XmlError::Syntax("unmatched end tag".into()))?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None => root = Some(el),
                }
            }
            Event::Text(t) => {
                let content = t.xml10_content();
                match stack.last_mut() {
                    Some(el) => el.text.push_str(&content),
                    None if content.trim().is_empty() => {}
                    None => return Err(XmlError::Syntax("text outside the root element".into())),
                }
            }
            Event::CData(t) => {
                if let Some(el) = stack.last_mut() {
                    el.text.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref()? {
                    Some(c) => c.to_string(),
                    None => {
                        let name = r.xml10_content();
                        resolve_predefined_entity(&name)
                            .ok_or_else(|| {
                                XmlError::Syntax(format!("undeclared entity \"&{name};\""))
                            })?
                            .to_owned()
                    }
                };
                if let Some(el) = stack.last_mut() {
                    el.text.push_str(&resolved);
                }
            }
            Event::Comment(_) | Event::Decl(_) | Event::PI(_) => {}
            Event::Eof => break,
        }
    }
    if !stack.is_empty() {
        return Err(XmlError::Syntax("unexpected end of document".into()));
    }
    root.ok_or_else(|| XmlError::Syntax("no root element".into()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn resolves_namespaces_and_entities() {
        let doc = parse(
            r#"<?xml version="1.0"?><a:r xmlns:a="urn:a" xmlns="urn:d" t="a:x"><c k="&lt;1&#65;">x &amp; y<![CDATA[<z>]]></c></a:r>"#,
        )
        .unwrap();
        assert_eq!(doc.name, QName::new("urn:a", "r"));
        assert_eq!(doc.resolve("a:x"), Some(QName::new("urn:a", "x")));
        assert_eq!(doc.resolve("x"), Some(QName::new("urn:d", "x")));
        assert_eq!(doc.resolve("b:x"), None);
        let c = &doc.children[0];
        assert_eq!(c.name, QName::new("urn:d", "c"));
        assert_eq!(c.attr("k"), Some("<1A"));
        assert_eq!(c.text, "x & y<z>");
    }

    #[test]
    fn rejects_doctype_and_unknown_entities() {
        assert_eq!(
            parse("<!DOCTYPE r [<!ENTITY e \"x\">]><r>&e;</r>").unwrap_err(),
            XmlError::Doctype
        );
        assert!(matches!(parse("<r>&e;</r>"), Err(XmlError::Syntax(_))));
        assert!(matches!(parse("<r a=\"&e;\"/>"), Err(XmlError::Syntax(_))));
        assert!(matches!(parse("<p:r/>"), Err(XmlError::Syntax(_))));
    }
}
