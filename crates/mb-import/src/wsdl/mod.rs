//! WSDL 1.1 (with inline and uploaded XSD) -> MCM compiler.
//!
//! Each `wsdl:service` is served through its first SOAP 1.1 or SOAP 1.2
//! port; every operation of that port's portType becomes one operation.
//! External schemas are never fetched: `schemaLocation`s must resolve
//! within the upload.

mod xml;
mod xsd;

use crate::heuristics::{kind_from_name, resource_from_name};
use crate::openapi3::{invalid, slug};
use crate::proto::hex_digest;
use crate::{ImportError, SourceFile, SourceFormat};
use mb_mcm::{
    Binding, Bundle, Identity, InputSchemas, Operation, OperationKind, OutputVariant, Protocol,
    ProtocolPayload, SoapVersion, StatusSelector,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use xml::{Element, QName, XmlError};
use xsd::{SchemaDoc, XSD_NS, Xsd};

const WSDL_NS: &str = "http://schemas.xmlsoap.org/wsdl/";
const WSDL20_NS: &str = "http://www.w3.org/ns/wsdl";
const SOAP11_NS: &str = "http://schemas.xmlsoap.org/wsdl/soap/";
const SOAP12_NS: &str = "http://schemas.xmlsoap.org/wsdl/soap12/";
/// The file name given to a single uploaded WSDL.
const MAIN_FILE: &str = "main.wsdl";
const DEFAULT_TITLE: &str = "SOAP API";
const CONTENT_TYPE: &str = "text/xml";

fn xml_error(e: XmlError, file: &str) -> ImportError {
    match e {
        XmlError::Doctype => invalid("/", "DOCTYPE is not allowed"),
        XmlError::Syntax(m) => ImportError::Parse(format!("{file}: {m}")),
    }
}

fn parse(bytes: &[u8], file: &str) -> Result<Element, ImportError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ImportError::Parse(format!("{file}: not valid UTF-8: {e}")))?;
    xml::parse(text).map_err(|e| xml_error(e, file))
}

/// Check that `root` is a WSDL 1.1 `definitions` element.
fn check_root(root: &Element) -> Result<(), ImportError> {
    if root.is(WSDL_NS, "definitions") {
        return Ok(());
    }
    if root.is(WSDL20_NS, "description") {
        return Err(ImportError::Unsupported(
            "WSDL 2.0 is not supported; convert to WSDL 1.1".into(),
        ));
    }
    Err(ImportError::Unsupported(format!(
        "XML root element {} is not a WSDL 1.1 definitions element",
        root.name
    )))
}

/// Whether `bytes` look like an XML document.
pub(crate) fn looks_like_xml(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<')
}

/// Detect a WSDL 1.1 document.
pub(crate) fn sniff(bytes: &[u8]) -> Result<SourceFormat, ImportError> {
    check_root(&parse(bytes, MAIN_FILE)?)?;
    Ok(SourceFormat::Wsdl11)
}

/// Compile a single WSDL document.
pub(crate) fn compile_text(bytes: &[u8]) -> Result<Bundle, ImportError> {
    let file = SourceFile {
        path: MAIN_FILE.into(),
        bytes: bytes.to_vec(),
    };
    compile_upload(&file, &[], hex_digest(bytes))
}

/// Compile `wsdl`, resolving `schemaLocation`s among `files`.
pub(crate) fn compile_upload(
    wsdl: &SourceFile,
    files: &[SourceFile],
    source_digest: String,
) -> Result<Bundle, ImportError> {
    let root = parse(&wsdl.bytes, &wsdl.path)?;
    check_root(&root)?;
    let text = std::str::from_utf8(&wsdl.bytes)
        .map_err(|e| ImportError::Parse(format!("{}: not valid UTF-8: {e}", wsdl.path)))?;
    let uploads: BTreeMap<&str, &[u8]> = files
        .iter()
        .map(|f| (f.path.as_str(), f.bytes.as_slice()))
        .collect();
    let loaded = load_schemas(&wsdl.path, &root, &uploads)?;
    let mut docs = Vec::new();
    for schema in inline_schemas(&root) {
        docs.push(SchemaDoc {
            el: schema,
            file: wsdl.path.clone(),
            tns: schema
                .attr("targetNamespace")
                .unwrap_or_default()
                .to_owned(),
        });
    }
    for l in &loaded {
        docs.push(SchemaDoc {
            el: &l.root,
            file: l.path.clone(),
            tns: l.tns.clone(),
        });
    }
    let xsd = Xsd::new(&docs);
    let defs = xsd.defs()?;
    let wsdl_doc = Wsdl::new(&root, &wsdl.path);
    let operations = wsdl_doc.operations(&xsd, &defs)?;
    let title = root
        .attr("name")
        .filter(|n| !n.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| wsdl_doc.services.first().map(|s| s.name.clone()))
        .unwrap_or_else(|| DEFAULT_TITLE.to_owned());
    Ok(Bundle {
        id: slug(&title),
        title,
        version: "0".into(),
        source_digest,
        operations,
        defs,
        protocol_payload: Some(ProtocolPayload::Soap {
            wsdl: text.strip_prefix('\u{feff}').unwrap_or(text).to_owned(),
        }),
        channels: Vec::new(),
        webhooks: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

fn inline_schemas(root: &Element) -> impl Iterator<Item = &Element> {
    root.children_named(WSDL_NS, "types")
        .flat_map(|t| t.children_named(XSD_NS, "schema"))
}

/// An uploaded schema reached through `xs:import`/`xs:include`.
struct Loaded {
    path: String,
    root: Element,
    tns: String,
}

/// `location` relative to the directory of `importer`, normalized.
fn join(importer: &str, location: &str) -> String {
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

/// The `schemaLocation`s a schema imports or includes; a location-less
/// import (SOAP encoding, XSD itself) is ignored.
fn locations(schema: &Element) -> Vec<String> {
    schema
        .children_in(XSD_NS)
        .filter(|c| matches!(c.name.local.as_str(), "import" | "include" | "redefine"))
        .filter_map(|c| c.attr("schemaLocation"))
        .map(|l| l.trim().to_owned())
        .collect()
}

fn load_schemas(
    wsdl_path: &str,
    root: &Element,
    uploads: &BTreeMap<&str, &[u8]>,
) -> Result<Vec<Loaded>, ImportError> {
    // (importing file, location, the importer's target namespace)
    let mut queue: VecDeque<(String, String, String)> = VecDeque::new();
    for schema in inline_schemas(root) {
        let tns = schema.attr("targetNamespace").unwrap_or_default();
        for loc in locations(schema) {
            queue.push_back((wsdl_path.to_owned(), loc, tns.to_owned()));
        }
    }
    let mut seen = BTreeSet::new();
    let mut loaded = Vec::new();
    while let Some((importer, location, inherited)) = queue.pop_front() {
        let path = join(&importer, &location);
        if !seen.insert(path.clone()) {
            continue;
        }
        let bytes = uploads.get(path.as_str()).ok_or_else(|| {
            invalid(
                importer.as_str(),
                format!(
                    "schemaLocation \"{location}\" is not in the upload; external schemas are not fetched"
                ),
            )
        })?;
        let schema = parse(bytes, &path)?;
        if !schema.is(XSD_NS, "schema") {
            return Err(invalid(path.as_str(), "not an XML Schema document"));
        }
        let tns = schema
            .attr("targetNamespace")
            .map_or(inherited, str::to_owned);
        for loc in locations(&schema) {
            queue.push_back((path.clone(), loc, tns.clone()));
        }
        loaded.push(Loaded {
            path,
            root: schema,
            tns,
        });
    }
    Ok(loaded)
}

// ---------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------

/// The local part of a QName-valued attribute.
fn local_of(value: &str) -> &str {
    value.rsplit_once(':').map_or(value, |(_, l)| l).trim()
}

/// A service's chosen SOAP port.
struct Service<'a> {
    name: String,
    port: String,
    binding: &'a Element,
    /// The namespace of the binding's SOAP extension elements.
    soap_ns: &'static str,
    version: SoapVersion,
    address_path: String,
}

struct Wsdl<'a> {
    file: &'a str,
    tns: &'a str,
    messages: BTreeMap<&'a str, &'a Element>,
    port_types: BTreeMap<&'a str, &'a Element>,
    services: Vec<Service<'a>>,
}

fn named<'a>(root: &'a Element, local: &'static str) -> BTreeMap<&'a str, &'a Element> {
    let mut map = BTreeMap::new();
    for el in root.children_named(WSDL_NS, local) {
        if let Some(name) = el.attr("name") {
            map.entry(name).or_insert(el);
        }
    }
    map
}

/// The SOAP version and extension namespace of a binding, if it is SOAP.
fn soap_binding(binding: &Element) -> Option<(SoapVersion, &'static str)> {
    if binding.child(SOAP11_NS, "binding").is_some() {
        Some((SoapVersion::Soap11, SOAP11_NS))
    } else if binding.child(SOAP12_NS, "binding").is_some() {
        Some((SoapVersion::Soap12, SOAP12_NS))
    } else {
        None
    }
}

/// The path of a `soap:address` location, else `/<service>`.
fn address_path(location: Option<&str>, service: &str) -> String {
    let path = location.map(str::trim).and_then(|loc| {
        let rest = match loc.split_once("://") {
            Some((_, rest)) => rest.find('/').map(|i| &rest[i..])?,
            None if loc.starts_with('/') => loc,
            None => return None,
        };
        let end = rest.find(['?', '#']).unwrap_or(rest.len());
        Some(rest[..end].to_owned())
    });
    path.filter(|p| !p.is_empty())
        .unwrap_or_else(|| format!("/{service}"))
}

fn services<'a>(root: &'a Element) -> Vec<Service<'a>> {
    let bindings = named(root, "binding");
    let mut out = Vec::new();
    for service in root.children_named(WSDL_NS, "service") {
        let name = service.attr("name").unwrap_or_default().to_owned();
        let ports: Vec<(&Element, &Element, SoapVersion, &'static str)> = service
            .children_named(WSDL_NS, "port")
            .filter_map(|port| {
                let binding = *bindings.get(local_of(port.attr("binding")?))?;
                let (version, ns) = soap_binding(binding)?;
                Some((port, binding, version, ns))
            })
            .collect();
        let Some(&(port, binding, _, soap_ns)) = ports.first() else {
            continue;
        };
        let version = if ports.iter().any(|p| p.2 == SoapVersion::Soap11) {
            SoapVersion::Soap11
        } else {
            SoapVersion::Soap12
        };
        let location = port
            .child(soap_ns, "address")
            .and_then(|a| a.attr("location"));
        out.push(Service {
            address_path: address_path(location, &name),
            port: port.attr("name").unwrap_or_default().to_owned(),
            name,
            binding,
            soap_ns,
            version,
        });
    }
    out
}

/// Follow `$ref`s through `defs` to the top-level properties of `schema`.
fn top_properties<'a>(
    schema: &'a Value,
    defs: &'a Map<String, Value>,
) -> Option<&'a Map<String, Value>> {
    let mut cur = schema;
    for _ in 0..16 {
        match cur
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
        {
            Some(name) => cur = defs.get(name)?,
            None => return cur.get("properties")?.as_object(),
        }
    }
    None
}

fn id_field(schema: Option<&Value>, defs: &Map<String, Value>) -> Option<String> {
    let props = top_properties(schema?, defs)?;
    ["id", "@id"]
        .into_iter()
        .find(|k| props.contains_key(*k))
        .map(str::to_owned)
}

/// Which side of an operation a message is on.
#[derive(Clone, Copy)]
enum Side {
    Input,
    Output,
}

impl<'a> Wsdl<'a> {
    fn new(root: &'a Element, file: &'a str) -> Self {
        Wsdl {
            file,
            tns: root.attr("targetNamespace").unwrap_or_default(),
            messages: named(root, "message"),
            port_types: named(root, "portType"),
            services: services(root),
        }
    }

    fn message(&self, ref_el: Option<&Element>) -> Result<Option<&'a Element>, ImportError> {
        let Some(name) = ref_el.and_then(|e| e.attr("message")) else {
            return Ok(None);
        };
        self.messages
            .get(local_of(name))
            .copied()
            .map(Some)
            .ok_or_else(|| invalid(self.file, format!("unknown message \"{name}\"")))
    }

    fn resolve(&self, el: &Element, value: &str) -> Result<QName, ImportError> {
        el.resolve(value).ok_or_else(|| {
            invalid(
                self.file,
                format!("unbound namespace prefix in \"{value}\""),
            )
        })
    }

    /// The schema of a part that names a global element, rooted at it.
    fn element_part(
        &self,
        xsd: &Xsd<'_>,
        part: &Element,
        element: &str,
    ) -> Result<Value, ImportError> {
        let q = self.resolve(part, element)?;
        let mut schema = xsd.element_ref(&q, self.file)?;
        if let Some(obj) = schema.as_object_mut() {
            obj.insert(
                "x-xml-root".into(),
                json!({"name": q.local, "namespace": q.ns}),
            );
        }
        Ok(schema)
    }

    fn part_type(&self, xsd: &Xsd<'_>, part: &Element) -> Result<Value, ImportError> {
        if let Some(element) = part.attr("element") {
            let q = self.resolve(part, element)?;
            return xsd.element_ref(&q, self.file);
        }
        match part.attr("type") {
            Some(t) => xsd.type_schema(&self.resolve(part, t)?, self.file),
            None => Ok(json!({})),
        }
    }

    /// The message's parts, limited to `soap:body@parts` when present.
    fn parts(msg: &'a Element, body: Option<&Element>) -> Vec<&'a Element> {
        let listed: Option<Vec<&str>> = body
            .and_then(|b| b.attr("parts"))
            .map(|p| p.split_whitespace().collect());
        msg.children_named(WSDL_NS, "part")
            .filter(|p| match &listed {
                Some(names) => p.attr("name").is_some_and(|n| names.contains(&n)),
                None => true,
            })
            .collect()
    }

    fn document_body(
        &self,
        xsd: &Xsd<'_>,
        msg: &Element,
        parts: &[&Element],
    ) -> Result<Option<Value>, ImportError> {
        let msg_name = msg.attr("name").unwrap_or_default();
        match parts {
            [] => Ok(None),
            [part] => match part.attr("element") {
                Some(element) => self.element_part(xsd, part, element).map(Some),
                None => Err(invalid(
                    format!("/messages/{msg_name}"),
                    "a document/literal part must reference an element",
                )),
            },
            many => Err(invalid(
                format!("/messages/{msg_name}"),
                format!(
                    "document/literal message has {} parts; expected one",
                    many.len()
                ),
            )),
        }
    }

    fn rpc_body(
        &self,
        xsd: &Xsd<'_>,
        parts: &[&Element],
        wrapper: String,
        namespace: &str,
    ) -> Result<Value, ImportError> {
        let mut props = Map::new();
        let mut order = Vec::new();
        for part in parts {
            let Some(name) = part.attr("name") else {
                continue;
            };
            props.insert(name.to_owned(), self.part_type(xsd, part)?);
            order.push(name.to_owned());
        }
        let mut out = Map::new();
        out.insert("type".into(), json!("object"));
        out.insert("properties".into(), Value::Object(props));
        if !order.is_empty() {
            out.insert("required".into(), json!(order));
        }
        out.insert("x-xml-order".into(), json!(order));
        out.insert("x-xml-namespace".into(), json!(namespace));
        out.insert("x-xml-qualified".into(), json!(false));
        out.insert(
            "x-xml-root".into(),
            json!({"name": wrapper, "namespace": namespace}),
        );
        Ok(Value::Object(out))
    }

    #[allow(clippy::too_many_arguments)]
    fn body(
        &self,
        xsd: &Xsd<'_>,
        service: &Service<'_>,
        op_name: &str,
        rpc: bool,
        side: Side,
        msg: Option<&'a Element>,
        binding_op: Option<&Element>,
    ) -> Result<Option<Value>, ImportError> {
        let Some(msg) = msg else {
            return Ok(None);
        };
        let tag = match side {
            Side::Input => "input",
            Side::Output => "output",
        };
        let soap_body = binding_op
            .and_then(|b| b.child(WSDL_NS, tag))
            .and_then(|io| io.child(service.soap_ns, "body"));
        let parts = Self::parts(msg, soap_body);
        if !rpc {
            return self.document_body(xsd, msg, &parts);
        }
        let namespace = soap_body
            .and_then(|b| b.attr("namespace"))
            .unwrap_or(self.tns);
        let wrapper = match side {
            Side::Input => op_name.to_owned(),
            Side::Output => format!("{op_name}Response"),
        };
        self.rpc_body(xsd, &parts, wrapper, namespace).map(Some)
    }

    fn fault(&self, xsd: &Xsd<'_>, op: &Element) -> Result<Option<Value>, ImportError> {
        let Some(msg) = self.message(op.child(WSDL_NS, "fault"))? else {
            return Ok(None);
        };
        let Some(part) = msg.child(WSDL_NS, "part") else {
            return Ok(None);
        };
        match part.attr("element") {
            Some(element) => self.element_part(xsd, part, element).map(Some),
            None => {
                let mut schema = self.part_type(xsd, part)?;
                if let Some(obj) = schema.as_object_mut() {
                    let name = part.attr("name").unwrap_or_default();
                    obj.insert("x-xml-root".into(), json!({"name": name, "namespace": ""}));
                }
                Ok(Some(schema))
            }
        }
    }

    fn operation(
        &self,
        xsd: &Xsd<'_>,
        defs: &Map<String, Value>,
        service: &Service<'_>,
        op: &Element,
    ) -> Result<Operation, ImportError> {
        let name = op.attr("name").unwrap_or_default();
        let binding_op = service
            .binding
            .children_named(WSDL_NS, "operation")
            .find(|b| b.attr("name") == Some(name));
        let soap_op = binding_op.and_then(|b| b.child(service.soap_ns, "operation"));
        let soap_action = soap_op
            .and_then(|o| o.attr("soapAction"))
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_owned);
        let style = soap_op
            .and_then(|o| o.attr("style"))
            .or_else(|| {
                service
                    .binding
                    .child(service.soap_ns, "binding")
                    .and_then(|b| b.attr("style"))
            })
            .unwrap_or("document");
        let rpc = style == "rpc";
        let input_msg = self.message(op.child(WSDL_NS, "input"))?;
        let output_msg = self.message(op.child(WSDL_NS, "output"))?;
        let input = self.body(xsd, service, name, rpc, Side::Input, input_msg, binding_op)?;
        let output = self.body(
            xsd,
            service,
            name,
            rpc,
            Side::Output,
            output_msg,
            binding_op,
        )?;
        let identity = id_field(output.as_ref(), defs).map(|field| Identity {
            path_param: None,
            field,
            input_field: id_field(input.as_ref(), defs),
            path_field: None,
        });
        let mut outputs = vec![OutputVariant {
            status: StatusSelector::Code(200),
            content_type: Some(CONTENT_TYPE.into()),
            schema: output,
            headers: Default::default(),
        }];
        if let Some(fault) = self.fault(xsd, op)? {
            outputs.push(OutputVariant {
                status: StatusSelector::Code(500),
                content_type: Some(CONTENT_TYPE.into()),
                schema: Some(fault),
                headers: Default::default(),
            });
        }
        Ok(Operation {
            id: name.to_owned(),
            protocol: Protocol::Soap,
            binding: Binding::Soap {
                service: service.name.clone(),
                port: service.port.clone(),
                operation: name.to_owned(),
                soap_action,
                version: service.version,
                address_path: service.address_path.clone(),
            },
            kind: kind_from_name(name, OperationKind::Query),
            summary: None,
            input: InputSchemas {
                path: None,
                query: None,
                headers: None,
                body: input,
                body_required: true,
            },
            outputs,
            resource_type: resource_from_name(name),
            identity,
            semantic_hints: Vec::new(),
            security: Vec::new(),
        })
    }

    /// One operation per portType operation of each service's chosen
    /// binding; ids are `<operation>`, or `<Service>.<operation>` when the
    /// name is used by more than one service.
    fn operations(
        &self,
        xsd: &Xsd<'_>,
        defs: &Map<String, Value>,
    ) -> Result<Vec<Operation>, ImportError> {
        if self.services.is_empty() {
            return Err(invalid(
                "/",
                "no wsdl:service has a SOAP 1.1 or SOAP 1.2 port",
            ));
        }
        let mut ops = Vec::new();
        for service in &self.services {
            let type_name = service.binding.attr("type").unwrap_or_default();
            let port_type = self
                .port_types
                .get(local_of(type_name))
                .ok_or_else(|| invalid(self.file, format!("unknown portType \"{type_name}\"")))?;
            for op in port_type.children_named(WSDL_NS, "operation") {
                ops.push(self.operation(xsd, defs, service, op)?);
            }
        }
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for op in &ops {
            *counts.entry(op.id.clone()).or_default() += 1;
        }
        for op in &mut ops {
            if counts.get(&op.id).copied().unwrap_or(0) > 1
                && let Binding::Soap { service, .. } = &op.binding
            {
                op.id = format!("{service}.{}", op.id);
            }
        }
        Ok(ops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_relative_locations() {
        assert_eq!(join("service.wsdl", "types.xsd"), "types.xsd");
        assert_eq!(join("api/service.wsdl", "types.xsd"), "api/types.xsd");
        assert_eq!(
            join("api/v1/s.wsdl", "../common/./t.xsd"),
            "api/common/t.xsd"
        );
        assert_eq!(join("api/s.wsdl", "/t.xsd"), "t.xsd");
    }

    #[test]
    fn address_paths() {
        assert_eq!(address_path(Some("http://h:1/a/b?x=1"), "S"), "/a/b");
        assert_eq!(address_path(Some("http://h"), "S"), "/S");
        assert_eq!(address_path(Some("/local"), "S"), "/local");
        assert_eq!(address_path(None, "S"), "/S");
    }
}
