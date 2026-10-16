//! A minimal namespace-resolving XML DOM over quick-xml.
//!
//! DOCTYPE declarations are rejected and no entity is ever expanded: only
//! the five predefined entities and character references are resolved.

use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use std::fmt;
use std::rc::Rc;

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
/// Deeper documents are rejected.
const MAX_DEPTH: usize = 256;
/// More namespace declarations on one element are rejected.
const MAX_NS_PER_ELEMENT: usize = 64;
/// More namespace declarations in one document are rejected.
const MAX_NS_PER_DOCUMENT: usize = 4096;

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

/// In-scope prefix bindings (`""` is the default namespace): a persistent
/// linked list of the declarations of each enclosing element, so an
/// element shares its parent's bindings instead of copying them.
type Scope = Option<Rc<ScopeNode>>;

#[derive(Debug)]
struct ScopeNode {
    /// `(prefix, namespace)` declared on one element.
    bindings: Vec<(String, String)>,
    parent: Scope,
}

/// The namespace bound to `prefix` in `scope`, innermost first.
fn lookup_in<'s>(scope: &'s Scope, prefix: &str) -> Option<&'s str> {
    let mut node = scope.as_deref();
    while let Some(n) = node {
        if let Some((_, ns)) = n.bindings.iter().rev().find(|(p, _)| p == prefix) {
            return Some(ns);
        }
        node = n.parent.as_deref();
    }
    None
}

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
        let ns = lookup_in(&self.scope, prefix).map(str::to_owned).or_else(|| {
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

/// Opens element `e` under `parent`; `declared` counts the namespace
/// declarations seen so far in the document.
fn open(e: &BytesStart<'_>, parent: &Scope, declared: &mut usize) -> Result<Element, XmlError> {
    let mut bindings: Vec<(String, String)> = Vec::new();
    let mut raw_attrs = Vec::new();
    for attr in e.attributes() {
        let attr = attr.map_err(|e| XmlError::Syntax(e.to_string()))?;
        let key = attr.key.as_ref().to_owned();
        let value = attr.normalized_value(XmlVersion::Implicit1_0)?.into_owned();
        let prefix = if key == "xmlns" {
            Some(String::new())
        } else {
            key.strip_prefix("xmlns:").map(str::to_owned)
        };
        match prefix {
            Some(prefix) => {
                *declared += 1;
                if bindings.len() >= MAX_NS_PER_ELEMENT {
                    return Err(XmlError::Syntax(format!(
                        "more than {MAX_NS_PER_ELEMENT} namespace declarations on one element"
                    )));
                }
                if *declared > MAX_NS_PER_DOCUMENT {
                    return Err(XmlError::Syntax(format!(
                        "more than {MAX_NS_PER_DOCUMENT} namespace declarations in the document"
                    )));
                }
                bindings.push((prefix, value));
            }
            None => raw_attrs.push((key, value)),
        }
    }
    let scope = if bindings.is_empty() {
        parent.clone()
    } else {
        Some(Rc::new(ScopeNode {
            bindings,
            parent: parent.clone(),
        }))
    };
    let lookup = |prefix: &str| -> Result<String, XmlError> {
        if prefix == "xml" {
            return Ok(XML_NS.to_owned());
        }
        lookup_in(&scope, prefix)
            .map(str::to_owned)
            .ok_or_else(|| XmlError::Syntax(format!("unbound namespace prefix \"{prefix}\"")))
    };
    let raw_name = e.name().as_ref().to_owned();
    let (prefix, local) = split(&raw_name);
    let ns = if prefix.is_empty() {
        lookup_in(&scope, "").unwrap_or_default().to_owned()
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
    let root_scope: Scope = None;
    let mut declared = 0usize;
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
                let el = open(&e, scope, &mut declared)?;
                if root.is_some() && stack.is_empty() {
                    return Err(XmlError::Syntax("more than one root element".into()));
                }
                stack.push(el);
            }
            Event::Empty(e) => {
                let scope = stack.last().map_or(&root_scope, |p| &p.scope);
                let el = open(&e, scope, &mut declared)?;
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

    /// Namespace bindings stored across the tree, each shared scope
    /// node counted once.
    fn stored_bindings(el: &Element, seen: &mut std::collections::HashSet<usize>) -> usize {
        let mut own = 0;
        let mut node = el.scope.as_ref();
        while let Some(n) = node {
            if !seen.insert(Rc::as_ptr(n) as usize) {
                break;
            }
            own += n.bindings.len();
            node = n.parent.as_ref();
        }
        own + el
            .children
            .iter()
            .map(|c| stored_bindings(c, seen))
            .sum::<usize>()
    }

    fn many_namespaces(root_decls: usize, children: usize) -> String {
        let decls: String = (0..root_decls)
            .map(|i| format!(" xmlns:p{i}=\"urn:p{i}\""))
            .collect();
        let kids = "<c xmlns:z=\"urn:z\" t=\"p0:x\"/>".repeat(children);
        format!("<r xmlns=\"urn:d\"{decls}>{kids}</r>")
    }

    #[test]
    fn namespace_scopes_are_linked_not_copied() {
        let (n, m) = (60, 3000);
        let root = parse(&many_namespaces(n, m)).unwrap();
        let stored = stored_bindings(&root, &mut Default::default());
        assert!(stored <= n + m + 1, "{stored} bindings stored");
        let last = &root.children[m - 1];
        assert_eq!(last.name, QName::new("urn:d", "c"));
        assert_eq!(last.resolve("p0:x"), Some(QName::new("urn:p0", "x")));
        assert_eq!(last.resolve("z:y"), Some(QName::new("urn:z", "y")));
        assert_eq!(root.resolve("z:y"), None);
    }

    #[test]
    fn caps_namespace_declarations() {
        let per_element: String = (0..=MAX_NS_PER_ELEMENT)
            .map(|i| format!(" xmlns:p{i}=\"urn:p\""))
            .collect();
        assert!(matches!(
            parse(&format!("<r{per_element}/>")),
            Err(XmlError::Syntax(m)) if m.contains("namespace declarations")
        ));
        assert!(matches!(
            parse(&many_namespaces(4, MAX_NS_PER_DOCUMENT)),
            Err(XmlError::Syntax(m)) if m.contains("namespace declarations")
        ));
        assert!(parse(&many_namespaces(MAX_NS_PER_ELEMENT - 2, 100)).is_ok());
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
