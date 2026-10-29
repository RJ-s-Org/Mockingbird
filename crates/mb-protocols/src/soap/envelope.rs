//! SOAP on the wire: a minimal namespace-resolving XML DOM for requests,
//! envelope and fault writers for responses, and the `?wsdl` address
//! rewrite.
//!
//! DOCTYPE declarations are rejected and no entity is ever expanded: only
//! the five predefined entities and character references are resolved.

use std::collections::HashMap;
use std::sync::Arc;

use quick_xml::XmlVersion;
use quick_xml::escape::{escape, resolve_predefined_entity};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::{NsReader, Reader};

pub(crate) const SOAP11_ENV_NS: &str = "http://schemas.xmlsoap.org/soap/envelope/";
pub(crate) const SOAP12_ENV_NS: &str = "http://www.w3.org/2003/05/soap-envelope";
pub(crate) const XSI_NS: &str = "http://www.w3.org/2001/XMLSchema-instance";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const WSDL_NS: &str = "http://schemas.xmlsoap.org/wsdl/";
const WSDL_SOAP11_NS: &str = "http://schemas.xmlsoap.org/wsdl/soap/";
const WSDL_SOAP12_NS: &str = "http://schemas.xmlsoap.org/wsdl/soap12/";
const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema";
/// Deeper documents are rejected.
const MAX_DEPTH: usize = 256;
/// More namespace declarations on one element are rejected.
const MAX_NS_PER_ELEMENT: usize = 64;
/// More namespace declarations in one document are rejected.
const MAX_NS_PER_DOCUMENT: usize = 256;
pub(crate) const DOCTYPE_MESSAGE: &str = "DOCTYPE is not allowed";

/// An expanded name: namespace URI (empty for none) and local name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QName {
    pub ns: String,
    pub local: String,
}

impl QName {
    fn new(ns: &str, local: &str) -> Self {
        QName {
            ns: ns.to_owned(),
            local: local.to_owned(),
        }
    }
}

/// In-scope prefix bindings (`""` is the default namespace): a persistent
/// linked list of the declarations of each enclosing element, so an
/// element shares its parent's bindings instead of copying them.
type Scope = Option<Arc<ScopeNode>>;

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
    /// Whether the element carries `xsi:nil="true"`.
    pub(crate) fn is_nil(&self) -> bool {
        self.attrs
            .iter()
            .any(|(n, v)| n.ns == XSI_NS && n.local == "nil" && matches!(v.trim(), "true" | "1"))
    }
}

fn split(raw: &str) -> (&str, &str) {
    raw.split_once(':').unwrap_or(("", raw))
}

fn syntax(e: impl std::fmt::Display) -> String {
    format!("malformed XML: {e}")
}

/// Opens element `e` under `parent`; `declared` counts the namespace
/// declarations seen so far in the document.
fn open(e: &BytesStart<'_>, parent: &Scope, declared: &mut usize) -> Result<Element, String> {
    let mut bindings: Vec<(String, String)> = Vec::new();
    let mut raw_attrs = Vec::new();
    for attr in e.attributes() {
        let attr = attr.map_err(syntax)?;
        let key = attr.key.as_ref().to_owned();
        let value = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(syntax)?
            .into_owned();
        let prefix = if key == "xmlns" {
            Some(String::new())
        } else {
            key.strip_prefix("xmlns:").map(str::to_owned)
        };
        match prefix {
            Some(prefix) => {
                *declared += 1;
                if bindings.len() >= MAX_NS_PER_ELEMENT {
                    return Err(syntax(format!(
                        "more than {MAX_NS_PER_ELEMENT} namespace declarations on one element"
                    )));
                }
                if *declared > MAX_NS_PER_DOCUMENT {
                    return Err(syntax(format!(
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
        Some(Arc::new(ScopeNode {
            bindings,
            parent: parent.clone(),
        }))
    };
    let lookup = |prefix: &str| -> Result<String, String> {
        if prefix == "xml" {
            return Ok(XML_NS.to_owned());
        }
        lookup_in(&scope, prefix)
            .map(str::to_owned)
            .ok_or_else(|| syntax(format!("unbound namespace prefix \"{prefix}\"")))
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

/// Parses `text` into its root element. The error is a fault string:
/// [`DOCTYPE_MESSAGE`] for a DOCTYPE, else `malformed XML: …`.
pub(crate) fn parse(text: &str) -> Result<Element, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut reader = Reader::from_str(text);
    reader.config_mut().check_end_names = true;
    let root_scope: Scope = None;
    let mut declared = 0usize;
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    loop {
        match reader.read_event().map_err(syntax)? {
            Event::DocType(_) => return Err(DOCTYPE_MESSAGE.into()),
            Event::Start(e) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(syntax(format!(
                        "elements are nested deeper than {MAX_DEPTH}"
                    )));
                }
                if root.is_some() && stack.is_empty() {
                    return Err(syntax("more than one root element"));
                }
                let scope = stack.last().map_or(&root_scope, |p| &p.scope);
                let el = open(&e, scope, &mut declared)?;
                stack.push(el);
            }
            Event::Empty(e) => {
                let scope = stack.last().map_or(&root_scope, |p| &p.scope);
                let el = open(&e, scope, &mut declared)?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None if root.is_none() => root = Some(el),
                    None => return Err(syntax("more than one root element")),
                }
            }
            Event::End(_) => {
                let el = stack.pop().ok_or_else(|| syntax("unmatched end tag"))?;
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
                    None => return Err(syntax("text outside the root element")),
                }
            }
            Event::CData(t) => {
                if let Some(el) = stack.last_mut() {
                    el.text.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref().map_err(syntax)? {
                    Some(c) => c.to_string(),
                    None => {
                        let name = r.xml10_content();
                        resolve_predefined_entity(&name)
                            .ok_or_else(|| syntax(format!("undeclared entity \"&{name};\"")))?
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
        return Err(syntax("unexpected end of document"));
    }
    root.ok_or_else(|| syntax("no root element"))
}

/// The first element child of the `Body` of the SOAP 1.1 or 1.2 envelope
/// `text`. The error is a fault string.
pub(crate) fn body_child(text: &str) -> Result<Element, String> {
    let root = parse(text)?;
    let ns = root.name.ns.as_str();
    if root.name.local != "Envelope" || (ns != SOAP11_ENV_NS && ns != SOAP12_ENV_NS) {
        return Err("the request is not a SOAP Envelope".into());
    }
    let ns = ns.to_owned();
    let mut root = root;
    let pos = root
        .children
        .iter()
        .position(|c| c.name.ns == ns && c.name.local == "Body")
        .ok_or("the Envelope has no Body")?;
    let mut body = root.children.swap_remove(pos);
    if body.children.is_empty() {
        return Err("the Body is empty".into());
    }
    Ok(body.children.swap_remove(0))
}

/// The SOAP version of a request or response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Version {
    V11,
    V12,
}

impl Version {
    fn ns(self) -> &'static str {
        match self {
            Version::V11 => SOAP11_ENV_NS,
            Version::V12 => SOAP12_ENV_NS,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Version::V11 => "soap",
            Version::V12 => "env",
        }
    }

    pub(crate) fn content_type(self) -> &'static str {
        match self {
            Version::V11 => "text/xml; charset=utf-8",
            Version::V12 => "application/soap+xml; charset=utf-8",
        }
    }
}

/// An envelope whose Body holds `body` (already serialized XML).
pub(crate) fn envelope(version: Version, body: &str) -> String {
    let (p, ns) = (version.prefix(), version.ns());
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><{p}:Envelope xmlns:{p}="{ns}"><{p}:Body>{body}</{p}:Body></{p}:Envelope>"#
    )
}

/// A Fault envelope. `client` picks `Client`/`Sender` over
/// `Server`/`Receiver`; `detail` is serialized XML.
pub(crate) fn fault(version: Version, client: bool, message: &str, detail: Option<&str>) -> String {
    let message = escape(message);
    let p = version.prefix();
    let body = match version {
        Version::V11 => {
            let code = if client { "Client" } else { "Server" };
            let detail = detail.map_or(String::new(), |d| format!("<detail>{d}</detail>"));
            format!(
                "<{p}:Fault><faultcode>{p}:{code}</faultcode><faultstring>{message}</faultstring>{detail}</{p}:Fault>"
            )
        }
        Version::V12 => {
            let code = if client { "Sender" } else { "Receiver" };
            let detail = detail.map_or(String::new(), |d| format!("<{p}:Detail>{d}</{p}:Detail>"));
            format!(
                r#"<{p}:Fault><{p}:Code><{p}:Value>{p}:{code}</{p}:Value></{p}:Code><{p}:Reason><{p}:Text xml:lang="en">{message}</{p}:Text></{p}:Reason>{detail}</{p}:Fault>"#
            )
        }
    };
    envelope(version, &body)
}

/// `wsdl` with the `location` of every `soap:address` / `soap12:address`
/// set to `base` followed by the address path of the enclosing service
/// (from `services`, else the path of the original location). Everything
/// else is copied byte for byte.
pub(crate) fn rewrite_wsdl(
    wsdl: &str,
    base: &str,
    services: &HashMap<String, String>,
) -> Result<String, String> {
    let mut reader = NsReader::from_str(wsdl);
    let mut out = String::with_capacity(wsdl.len() + 64);
    let mut copied = 0usize;
    let mut service: Option<String> = None;
    loop {
        let before = usize::try_from(reader.buffer_position()).map_err(syntax)?;
        let (resolved, event) = reader.read_resolved_event().map_err(syntax)?;
        let ns = match resolved {
            ResolveResult::Bound(ns) => ns.0.to_owned(),
            _ => String::new(),
        };
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let local = e.local_name().as_ref().to_owned();
                if ns == WSDL_NS && local == "service" {
                    service = attr_value(e, "name")?;
                }
                if (ns == WSDL_SOAP11_NS || ns == WSDL_SOAP12_NS) && local == "address" {
                    let after = usize::try_from(reader.buffer_position()).map_err(syntax)?;
                    let start = wsdl[before..after]
                        .find('<')
                        .map(|i| before + i)
                        .ok_or_else(|| syntax("address tag not found"))?;
                    let original = attr_value(e, "location")?.unwrap_or_default();
                    let path = service
                        .as_ref()
                        .and_then(|s| services.get(s).cloned())
                        .unwrap_or_else(|| url_path(&original));
                    out.push_str(&wsdl[copied..start]);
                    out.push_str(&address_tag(
                        e,
                        &format!("{base}{path}"),
                        matches!(event, Event::Empty(_)),
                    )?);
                    copied = after;
                }
            }
            Event::End(ref e) if e.local_name().as_ref() == "service" => service = None,
            Event::Eof => break,
            _ => {}
        }
    }
    out.push_str(&wsdl[copied..]);
    Ok(out)
}

fn attr_value(e: &BytesStart<'_>, local: &str) -> Result<Option<String>, String> {
    for attr in e.attributes() {
        let attr = attr.map_err(syntax)?;
        if attr.key.as_ref() == local {
            let v = attr
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(syntax)?;
            return Ok(Some(v.into_owned()));
        }
    }
    Ok(None)
}

/// The tag `e` with its `location` attribute set to `location`.
fn address_tag(e: &BytesStart<'_>, location: &str, empty: bool) -> Result<String, String> {
    tag_with(e, "location", location, empty)
}

/// Start tag `e` with attribute `name` set to `value` (added when absent);
/// the other attributes are copied.
fn tag_with(e: &BytesStart<'_>, name: &str, value: &str, empty: bool) -> Result<String, String> {
    let mut tag = format!("<{}", e.name().as_ref());
    let mut found = false;
    for attr in e.attributes() {
        let attr = attr.map_err(syntax)?;
        let key = attr.key.as_ref();
        if key == name {
            found = true;
            tag.push_str(&format!(" {name}=\"{}\"", escape(value)));
        } else {
            // Raw (still escaped) value; a `"` can only come from a
            // single-quoted value.
            let value = attr.value.replace('"', "&quot;");
            tag.push_str(&format!(" {key}=\"{value}\""));
        }
    }
    if !found {
        tag.push_str(&format!(" {name}=\"{}\"", escape(value)));
    }
    tag.push_str(if empty { "/>" } else { ">" });
    Ok(tag)
}

/// The uploaded schemas embedded in a stored WSDL (see
/// [`mb_mcm::ProtocolPayload::Soap`]).
#[derive(Debug, Default)]
pub(crate) struct Schemas {
    /// The WSDL's path in the upload, which its `schemaLocation`s are
    /// relative to.
    pub wsdl_path: String,
    /// Upload path → schema text.
    pub files: HashMap<String, String>,
}

/// `wsdl` without its embedded schemas, and those schemas.
pub(crate) fn split_schemas(wsdl: &str) -> Result<(String, Schemas), String> {
    let mut reader = NsReader::from_str(wsdl);
    let mut schemas = Schemas::default();
    // The byte range of the `<schemas>` element.
    let mut span: Option<(usize, usize)> = None;
    let mut inside = false;
    // The schema being read: its path and text.
    let mut current: Option<(String, String)> = None;
    loop {
        let before = usize::try_from(reader.buffer_position()).map_err(syntax)?;
        let (resolved, event) = reader.read_resolved_event().map_err(syntax)?;
        let embedded =
            matches!(resolved, ResolveResult::Bound(ns) if ns.0 == mb_mcm::EMBEDDED_SCHEMAS_NS);
        match event {
            Event::Start(ref e) if embedded && e.local_name().as_ref() == "schemas" => {
                inside = true;
                schemas.wsdl_path = attr_value(e, "wsdl")?.unwrap_or_default();
                span = Some((before, before));
            }
            Event::Start(ref e) if inside && embedded && e.local_name().as_ref() == "schema" => {
                current = Some((attr_value(e, "path")?.unwrap_or_default(), String::new()));
            }
            Event::CData(ref c) => {
                if let Some((_, text)) = current.as_mut() {
                    text.push_str(c.as_ref());
                }
            }
            Event::End(ref e) if embedded && e.local_name().as_ref() == "schema" => {
                if let Some((path, text)) = current.take() {
                    schemas.files.insert(path, text);
                }
            }
            Event::End(ref e) if embedded && e.local_name().as_ref() == "schemas" => {
                let after = usize::try_from(reader.buffer_position()).map_err(syntax)?;
                if let Some((start, _)) = span {
                    span = Some((start, after));
                }
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let clean = match span {
        Some((start, end)) if end > start => format!("{}{}", &wsdl[..start], &wsdl[end..]),
        _ => wsdl.to_string(),
    };
    Ok((clean, schemas))
}

/// `location` relative to the directory of `importer`, normalized (as the
/// import resolved it).
pub(crate) fn join_location(importer: &str, location: &str) -> String {
    let dir = importer.rsplit_once('/').map_or("", |(d, _)| d);
    let mut parts: Vec<&str> = if location.starts_with('/') {
        Vec::new()
    } else {
        dir.split('/').filter(|s| !s.is_empty()).collect()
    };
    for seg in location.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// `doc` (the document at upload path `doc_path`) with every XSD
/// `import`/`include`/`redefine` `schemaLocation` that names one of
/// `schemas` set to `url(<its upload path>)`. Everything else is copied
/// byte for byte.
pub(crate) fn rewrite_schema_locations(
    doc: &str,
    doc_path: &str,
    schemas: &Schemas,
    url: &dyn Fn(&str) -> String,
) -> Result<String, String> {
    if schemas.files.is_empty() {
        return Ok(doc.to_string());
    }
    let mut reader = NsReader::from_str(doc);
    let mut out = String::with_capacity(doc.len() + 256);
    let mut copied = 0usize;
    loop {
        let before = usize::try_from(reader.buffer_position()).map_err(syntax)?;
        let (resolved, event) = reader.read_resolved_event().map_err(syntax)?;
        let in_xsd = matches!(resolved, ResolveResult::Bound(ns) if ns.0 == XSD_NS);
        match event {
            Event::Start(ref e) | Event::Empty(ref e)
                if in_xsd
                    && matches!(e.local_name().as_ref(), "import" | "include" | "redefine") =>
            {
                let Some(location) = attr_value(e, "schemaLocation")? else {
                    continue;
                };
                let target = join_location(doc_path, location.trim());
                if !schemas.files.contains_key(&target) {
                    continue;
                }
                let after = usize::try_from(reader.buffer_position()).map_err(syntax)?;
                let start = doc[before..after]
                    .find('<')
                    .map(|i| before + i)
                    .ok_or_else(|| syntax("schema reference tag not found"))?;
                out.push_str(&doc[copied..start]);
                out.push_str(&tag_with(
                    e,
                    "schemaLocation",
                    &url(&target),
                    matches!(event, Event::Empty(_)),
                )?);
                copied = after;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    out.push_str(&doc[copied..]);
    Ok(out)
}

/// The path of an absolute URL (`/` when it has none); a relative
/// reference is returned as given.
fn url_path(url: &str) -> String {
    let Some((_, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    match rest.find(['/', '?', '#']) {
        Some(i) if rest[i..].starts_with('/') => {
            let path = &rest[i..];
            path.split(['?', '#']).next().unwrap_or(path).to_string()
        }
        _ => "/".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_child_of_both_versions() {
        let el = body_child(&envelope(Version::V11, "<a xmlns=\"urn:x\"><b>1</b></a>")).unwrap();
        assert_eq!(el.name, QName::new("urn:x", "a"));
        assert_eq!(el.children[0].text, "1");
        let el = body_child(&envelope(Version::V12, "<a/>")).unwrap();
        assert_eq!(el.name, QName::new("", "a"));
        assert_eq!(
            body_child(&envelope(Version::V11, "")).unwrap_err(),
            "the Body is empty"
        );
        assert_eq!(
            body_child("<Envelope/>").unwrap_err(),
            "the request is not a SOAP Envelope"
        );
        assert_eq!(body_child("<!DOCTYPE r><r/>").unwrap_err(), DOCTYPE_MESSAGE);
        assert!(
            body_child("<r>&e;</r>")
                .unwrap_err()
                .starts_with("malformed XML")
        );
    }

    /// Namespace bindings stored across the tree, each shared scope
    /// node counted once.
    fn stored_bindings(el: &Element, seen: &mut std::collections::HashSet<usize>) -> usize {
        let mut own = 0;
        let mut node = el.scope.as_ref();
        while let Some(n) = node {
            if !seen.insert(Arc::as_ptr(n) as usize) {
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
        let kids = "<c xmlns:z=\"urn:z\"/>".repeat(children);
        format!(
            "<soap:Envelope xmlns:soap=\"{SOAP11_ENV_NS}\"{decls}><soap:Body><a>{kids}</a></soap:Body></soap:Envelope>"
        )
    }

    #[test]
    fn namespace_scopes_are_linked_not_copied() {
        // N declarations on the root and M siblings declaring one prefix
        // each must store O(N + M) bindings, not O(N * M).
        let (n, m) = (60, 190);
        let root = parse(&many_namespaces(n, m)).unwrap();
        let stored = stored_bindings(&root, &mut Default::default());
        assert!(stored <= n + m + 1, "{stored} bindings stored");
        let a = &root.children[0].children[0];
        assert_eq!(a.children.len(), m);
        assert_eq!(a.children[m - 1].name, QName::new("", "c"));
    }

    #[test]
    fn caps_namespace_declarations() {
        let per_element: String = (0..=MAX_NS_PER_ELEMENT)
            .map(|i| format!(" xmlns:p{i}=\"urn:p\""))
            .collect();
        let err = parse(&format!("<r{per_element}/>")).unwrap_err();
        assert!(err.contains("namespace declarations"), "{err}");
        // Under the per-element cap, over the per-document one.
        let err = parse(&many_namespaces(4, MAX_NS_PER_DOCUMENT)).unwrap_err();
        assert!(err.contains("namespace declarations"), "{err}");
        // The reviewer's N = M = 2000 envelope is refused outright.
        assert!(parse(&many_namespaces(2000, 2000)).is_err());
        // At the caps, a document still parses.
        assert!(parse(&many_namespaces(MAX_NS_PER_ELEMENT - 1, 100)).is_ok());
    }

    #[test]
    fn faults_escape_their_message() {
        let f = fault(Version::V11, true, "a < b", None);
        assert!(f.contains(
            "<faultcode>soap:Client</faultcode><faultstring>a &lt; b</faultstring></soap:Fault>"
        ));
        let f = fault(Version::V12, false, "x", Some("<d/>"));
        assert!(f.contains("<env:Value>env:Receiver</env:Value>"));
        assert!(f.contains("<env:Detail><d/></env:Detail>"));
    }

    #[test]
    fn url_paths() {
        assert_eq!(url_path("http://h:1/a/b?x#y"), "/a/b");
        assert_eq!(url_path("http://h"), "/");
        assert_eq!(url_path("http://h?x"), "/");
        assert_eq!(url_path("/rel"), "/rel");
    }

    #[test]
    fn splits_embedded_schemas() {
        let wsdl = r#"<d:definitions xmlns:d="http://schemas.xmlsoap.org/wsdl/"><d:types/><mb:schemas xmlns:mb="urn:mockingbird:embedded-schemas" wsdl="a/s.wsdl"><mb:schema path="a/t.xsd"><![CDATA[<x>]]]]><![CDATA[></x>]]></mb:schema></mb:schemas></d:definitions>"#;
        let (clean, schemas) = split_schemas(wsdl).unwrap();
        assert_eq!(
            clean,
            r#"<d:definitions xmlns:d="http://schemas.xmlsoap.org/wsdl/"><d:types/></d:definitions>"#
        );
        assert_eq!(schemas.wsdl_path, "a/s.wsdl");
        assert_eq!(schemas.files["a/t.xsd"], "<x>]]></x>");
        assert_eq!(join_location("a/s.wsdl", "../b/./c.xsd"), "b/c.xsd");

        let plain = r#"<d:definitions xmlns:d="http://schemas.xmlsoap.org/wsdl/"/>"#;
        let (clean, schemas) = split_schemas(plain).unwrap();
        assert_eq!(clean, plain);
        assert!(schemas.files.is_empty());
    }

    #[test]
    fn rewrites_only_known_schema_locations() {
        let doc = r#"<s:schema xmlns:s="http://www.w3.org/2001/XMLSchema"><s:import namespace="a" schemaLocation="t.xsd"/><s:import namespace="b" schemaLocation="u.xsd"/><x:include xmlns:x="other" schemaLocation="t.xsd"/></s:schema>"#;
        let schemas = Schemas {
            wsdl_path: "d/s.wsdl".into(),
            files: HashMap::from([("d/t.xsd".to_string(), String::new())]),
        };
        let out =
            rewrite_schema_locations(doc, "d/s.wsdl", &schemas, &|t| format!("U?{t}")).unwrap();
        assert_eq!(
            out,
            r#"<s:schema xmlns:s="http://www.w3.org/2001/XMLSchema"><s:import namespace="a" schemaLocation="U?d/t.xsd"/><s:import namespace="b" schemaLocation="u.xsd"/><x:include xmlns:x="other" schemaLocation="t.xsd"/></s:schema>"#
        );
    }

    #[test]
    fn rewrites_only_soap_addresses() {
        let wsdl = r#"<d:definitions xmlns:d="http://schemas.xmlsoap.org/wsdl/" xmlns:s="http://schemas.xmlsoap.org/wsdl/soap/" xmlns:h="http://schemas.xmlsoap.org/wsdl/http/">
  <d:service name="A"><d:port name="p"><s:address location="http://old/x"/></d:port><d:port name="q"><h:address location="http://old/h"/></d:port></d:service>
  <d:service name="B"><d:port name="p"><s:address location='http://old/y?z' extra="&amp;"></s:address></d:port></d:service>
</d:definitions>"#;
        let services = HashMap::from([("A".to_string(), "/a".to_string())]);
        let out = rewrite_wsdl(wsdl, "http://h:1/m", &services).unwrap();
        assert!(
            out.contains(r#"<s:address location="http://h:1/m/a"/>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<h:address location="http://old/h"/>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<s:address location="http://h:1/m/y" extra="&amp;"></s:address>"#),
            "{out}"
        );
        assert!(out.starts_with("<d:definitions"));
        assert!(out.ends_with("</d:definitions>"));
    }
}
