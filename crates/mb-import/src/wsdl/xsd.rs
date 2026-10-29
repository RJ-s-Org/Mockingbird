//! XML Schema (XSD 1.0) -> JSON Schema 2020-12, with the `x-xml-*`
//! annotations the SOAP adapter needs to re-encode values as XML.
//!
//! Named complexTypes and top-level elements become `$defs` entries;
//! named simpleTypes are inlined where they are used.

use super::xml::{Element, QName};
use crate::ImportError;
use crate::openapi3::invalid;
use serde_json::{Map, Number, Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

pub(crate) const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema";
/// Deeper chains of type references are taken to be circular.
const MAX_DEPTH: usize = 64;
/// The most JSON nodes that replaying memoised group and attributeGroup
/// expansions may add to the schemas of one import.
const MAX_EXPANSION: usize = 1_000_000;
const HEX_PATTERN: &str = "^[0-9A-Fa-f]*$";

/// One `xs:schema` element and where it came from.
pub(crate) struct SchemaDoc<'a> {
    pub el: &'a Element,
    /// The file it was read from, for error pointers.
    pub file: String,
    /// Its target namespace (inherited from the includer when it has none).
    pub tns: String,
}

struct Ctx {
    file: String,
    tns: String,
    qualified: bool,
}

/// A global component and the index of the schema declaring it.
#[derive(Clone, Copy)]
struct Comp<'a> {
    el: &'a Element,
    schema: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DefKind {
    ComplexType,
    Element,
}

pub(crate) struct Xsd<'a> {
    schemas: Vec<Ctx>,
    complex: BTreeMap<QName, Comp<'a>>,
    simple: BTreeMap<QName, Comp<'a>>,
    elements: BTreeMap<QName, Comp<'a>>,
    groups: BTreeMap<QName, Comp<'a>>,
    attr_groups: BTreeMap<QName, Comp<'a>>,
    attributes: BTreeMap<QName, Comp<'a>>,
    def_names: BTreeMap<(DefKind, QName), String>,
    /// Finished expansions of named groups (keyed by name, optional and
    /// in_choice) and attributeGroups (keyed by name), each with its size in
    /// JSON nodes. Expanding a group once keeps nested or repeated references
    /// linear instead of exponential.
    group_memo: RefCell<BTreeMap<(QName, bool, bool), Memo>>,
    attr_group_memo: RefCell<BTreeMap<QName, Memo>>,
    /// JSON nodes added so far by replaying memoised expansions.
    expanded: Cell<usize>,
}

/// A finished group expansion and its size in JSON nodes.
type Memo = Rc<(Obj, usize)>;

/// The properties of one object schema being built.
#[derive(Default)]
struct Obj {
    props: Map<String, Value>,
    required: Vec<String>,
    order: Vec<String>,
    choice: Vec<String>,
}

impl Obj {
    fn add(&mut self, name: String, schema: Value, required: bool) {
        if required && !self.required.contains(&name) {
            self.required.push(name.clone());
        }
        self.props.insert(name, schema);
    }

    fn element(&mut self, name: String, schema: Value, required: bool, in_choice: bool) {
        if !self.order.contains(&name) {
            self.order.push(name.clone());
        }
        if in_choice && !self.choice.contains(&name) {
            self.choice.push(name.clone());
        }
        self.add(name, schema, required);
    }

    /// Apply everything `other` recorded, as if its `add`/`element` calls
    /// were made on `self` in the same order.
    fn merge(&mut self, other: &Obj) {
        for (list, from) in [
            (&mut self.order, &other.order),
            (&mut self.choice, &other.choice),
            (&mut self.required, &other.required),
        ] {
            for name in from {
                if !list.contains(name) {
                    list.push(name.clone());
                }
            }
        }
        for (name, schema) in &other.props {
            self.props.insert(name.clone(), schema.clone());
        }
    }

    /// The number of JSON nodes in the property schemas.
    fn size(&self) -> usize {
        self.props.values().map(nodes).sum()
    }

    fn finish(self, ctx: &Ctx) -> Value {
        let mut out = Map::new();
        out.insert("type".into(), json!("object"));
        out.insert("properties".into(), Value::Object(self.props));
        if !self.required.is_empty() {
            out.insert("required".into(), json!(self.required));
        }
        out.insert("x-xml-order".into(), json!(self.order));
        out.insert("x-xml-namespace".into(), json!(ctx.tns));
        out.insert("x-xml-qualified".into(), json!(ctx.qualified));
        if !self.choice.is_empty() {
            out.insert("x-xml-choice".into(), json!(self.choice));
        }
        Value::Object(out)
    }
}

/// The number of JSON nodes in `v`.
fn nodes(v: &Value) -> usize {
    1 + match v {
        Value::Array(items) => items.iter().map(nodes).sum(),
        Value::Object(map) => map.values().map(nodes).sum(),
        _ => 0,
    }
}

fn def_ref(name: &str) -> Value {
    json!({ "$ref": format!("#/$defs/{name}") })
}

fn min_occurs(el: &Element) -> u64 {
    el.attr("minOccurs")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(1)
}

/// `None` for `unbounded`.
fn max_occurs(el: &Element) -> Option<u64> {
    match el.attr("maxOccurs").map(str::trim) {
        Some("unbounded") => None,
        Some(v) => Some(v.parse().unwrap_or(1)),
        None => Some(1),
    }
}

/// The JSON Schema of an XSD built-in type.
fn builtin(local: &str) -> Value {
    let int = |min: i64, max: i64| json!({"type": "integer", "minimum": min, "maximum": max});
    let uint = |max: u64| json!({"type": "integer", "minimum": 0, "maximum": max});
    let format = |f: &str| json!({"type": "string", "format": f});
    match local {
        "int" => int(i32::MIN.into(), i32::MAX.into()),
        "short" => int(i16::MIN.into(), i16::MAX.into()),
        "byte" => int(i8::MIN.into(), i8::MAX.into()),
        "unsignedLong" => uint(u64::MAX),
        "unsignedInt" => uint(u32::MAX.into()),
        "unsignedShort" => uint(u16::MAX.into()),
        "unsignedByte" => uint(u8::MAX.into()),
        "positiveInteger" => json!({"type": "integer", "minimum": 1}),
        "nonNegativeInteger" => json!({"type": "integer", "minimum": 0}),
        "integer" | "long" | "negativeInteger" | "nonPositiveInteger" => {
            json!({"type": "integer"})
        }
        "decimal" | "float" | "double" => json!({"type": "number"}),
        "boolean" => json!({"type": "boolean"}),
        "date" => format("date"),
        "dateTime" => format("date-time"),
        "time" => format("time"),
        "duration" => format("duration"),
        "base64Binary" => format("byte"),
        "hexBinary" => json!({"type": "string", "format": "hex", "pattern": HEX_PATTERN}),
        "anyType" | "anySimpleType" => json!({}),
        _ => json!({"type": "string"}),
    }
}

/// A facet value as a JSON number, if it is one.
fn number(v: &str) -> Option<Value> {
    let v = v.trim();
    if let Ok(i) = v.parse::<i64>() {
        return Some(json!(i));
    }
    if let Ok(u) = v.parse::<u64>() {
        return Some(json!(u));
    }
    v.parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .map(Value::Number)
}

/// An enumeration value typed after the base schema's `type`.
fn enum_value(v: &str, ty: Option<&str>) -> Value {
    let typed = match ty {
        Some("integer" | "number") => number(v),
        Some("boolean") => match v.trim() {
            "true" | "1" => Some(json!(true)),
            "false" | "0" => Some(json!(false)),
            _ => None,
        },
        _ => None,
    };
    typed.unwrap_or_else(|| json!(v))
}

/// Apply the facets of `restriction` to a simple `schema`.
fn apply_facets(schema: &mut Value, restriction: &Element) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    let ty = obj.get("type").and_then(Value::as_str).map(str::to_owned);
    let mut enums = Vec::new();
    let mut patterns = Vec::new();
    for facet in restriction.children_in(XSD_NS) {
        let Some(value) = facet.attr("value") else {
            continue;
        };
        let length = || value.trim().parse::<u64>().ok().map(Value::from);
        let mut set = |key: &str, v: Option<Value>| {
            if let Some(v) = v {
                obj.insert(key.into(), v);
            }
        };
        match facet.name.local.as_str() {
            "enumeration" => enums.push(enum_value(value, ty.as_deref())),
            "pattern" => patterns.push(value.to_owned()),
            "length" => {
                set("minLength", length());
                set("maxLength", length());
            }
            "minLength" => set("minLength", length()),
            "maxLength" => set("maxLength", length()),
            "minInclusive" => set("minimum", number(value)),
            "maxInclusive" => set("maximum", number(value)),
            "minExclusive" => set("exclusiveMinimum", number(value)),
            "maxExclusive" => set("exclusiveMaximum", number(value)),
            _ => {}
        }
    }
    if !enums.is_empty() {
        obj.insert("enum".into(), Value::Array(enums));
    }
    // XSD patterns match the whole value.
    match patterns.as_slice() {
        [] => {}
        [one] => {
            obj.insert("pattern".into(), json!(format!("^(?:{one})$")));
        }
        many => {
            let alternatives: Vec<String> = many.iter().map(|p| format!("(?:{p})")).collect();
            obj.insert(
                "pattern".into(),
                json!(format!("^(?:{})$", alternatives.join("|"))),
            );
        }
    }
}

impl<'a> Xsd<'a> {
    pub(crate) fn new(docs: &[SchemaDoc<'a>]) -> Self {
        let mut xsd = Xsd {
            schemas: Vec::new(),
            complex: BTreeMap::new(),
            simple: BTreeMap::new(),
            elements: BTreeMap::new(),
            groups: BTreeMap::new(),
            attr_groups: BTreeMap::new(),
            attributes: BTreeMap::new(),
            def_names: BTreeMap::new(),
            group_memo: RefCell::new(BTreeMap::new()),
            attr_group_memo: RefCell::new(BTreeMap::new()),
            expanded: Cell::new(0),
        };
        for (schema, doc) in docs.iter().enumerate() {
            xsd.schemas.push(Ctx {
                file: doc.file.clone(),
                tns: doc.tns.clone(),
                qualified: doc.el.attr("elementFormDefault") == Some("qualified"),
            });
            for child in doc.el.children_in(XSD_NS) {
                let Some(name) = child.attr("name") else {
                    continue;
                };
                let table = match child.name.local.as_str() {
                    "complexType" => &mut xsd.complex,
                    "simpleType" => &mut xsd.simple,
                    "element" => &mut xsd.elements,
                    "group" => &mut xsd.groups,
                    "attributeGroup" => &mut xsd.attr_groups,
                    "attribute" => &mut xsd.attributes,
                    _ => continue,
                };
                table
                    .entry(QName::new(&doc.tns, name))
                    .or_insert(Comp { el: child, schema });
            }
        }
        let mut used = BTreeSet::new();
        let named = xsd
            .complex
            .keys()
            .map(|q| (DefKind::ComplexType, q))
            .chain(xsd.elements.keys().map(|q| (DefKind::Element, q)));
        let mut def_names = BTreeMap::new();
        for (kind, q) in named {
            let mut name = q.local.clone();
            let mut n = 2;
            while used.contains(&name) {
                name = format!("{}_{n}", q.local);
                n += 1;
            }
            used.insert(name.clone());
            def_names.insert((kind, q.clone()), name);
        }
        xsd.def_names = def_names;
        xsd
    }

    /// Every `$defs` entry.
    pub(crate) fn defs(&self) -> Result<Map<String, Value>, ImportError> {
        let mut defs = Map::new();
        for ((kind, q), name) in &self.def_names {
            let schema = match kind {
                DefKind::ComplexType => {
                    let c = self.complex[q];
                    self.object(c.el, c.schema, 0)?
                }
                DefKind::Element => {
                    let c = self.elements[q];
                    self.element_type(c.el, c.schema, 0)?
                }
            };
            defs.insert(name.clone(), schema);
        }
        Ok(defs)
    }

    /// A `$ref` to the definition of the global element `q`.
    pub(crate) fn element_ref(&self, q: &QName, file: &str) -> Result<Value, ImportError> {
        self.def_names
            .get(&(DefKind::Element, q.clone()))
            .map(|n| def_ref(n))
            .ok_or_else(|| invalid(file, format!("unknown element \"{q}\"")))
    }

    /// The schema of the type `q`: a built-in, a `$ref` to a complexType or
    /// an inlined simpleType.
    pub(crate) fn type_schema(&self, q: &QName, file: &str) -> Result<Value, ImportError> {
        self.type_ref(q, file, 0)
    }

    fn type_ref(&self, q: &QName, file: &str, depth: usize) -> Result<Value, ImportError> {
        if depth > MAX_DEPTH {
            return Err(invalid(file, format!("type \"{q}\" is circular")));
        }
        if q.ns == XSD_NS {
            return Ok(builtin(&q.local));
        }
        if let Some(name) = self.def_names.get(&(DefKind::ComplexType, q.clone())) {
            return Ok(def_ref(name));
        }
        if let Some(c) = self.simple.get(q) {
            return self.simple_type(c.el, c.schema, depth + 1);
        }
        Err(invalid(file, format!("unknown type \"{q}\"")))
    }

    fn file(&self, schema: usize) -> &str {
        &self.schemas[schema].file
    }

    fn resolve(&self, el: &Element, value: &str, schema: usize) -> Result<QName, ImportError> {
        el.resolve(value).ok_or_else(|| {
            invalid(
                self.file(schema),
                format!("unbound namespace prefix in \"{value}\""),
            )
        })
    }

    fn check_depth(&self, depth: usize, schema: usize) -> Result<(), ImportError> {
        if depth > MAX_DEPTH {
            return Err(invalid(
                self.file(schema),
                "type definitions are circular or nested too deeply",
            ));
        }
        Ok(())
    }

    /// Account for replaying a memoised expansion of `size` JSON nodes.
    fn charge(&self, size: usize, schema: usize) -> Result<(), ImportError> {
        let total = self.expanded.get().saturating_add(size);
        if total > MAX_EXPANSION {
            return Err(invalid(self.file(schema), "schema expansion too large"));
        }
        self.expanded.set(total);
        Ok(())
    }

    /// The type of an element declaration, without occurrence or nillable.
    fn element_type(&self, el: &Element, s: usize, depth: usize) -> Result<Value, ImportError> {
        self.check_depth(depth, s)?;
        if let Some(t) = el.attr("type") {
            let q = self.resolve(el, t, s)?;
            return self.type_ref(&q, self.file(s), depth + 1);
        }
        if let Some(ct) = el.child(XSD_NS, "complexType") {
            return self.object(ct, s, depth + 1);
        }
        if let Some(st) = el.child(XSD_NS, "simpleType") {
            return self.simple_type(st, s, depth + 1);
        }
        Ok(json!({}))
    }

    fn simple_type(&self, st: &Element, s: usize, depth: usize) -> Result<Value, ImportError> {
        self.check_depth(depth, s)?;
        let Some(r) = st.child(XSD_NS, "restriction") else {
            // `list` and `union` are carried as text.
            return Ok(json!({"type": "string"}));
        };
        let mut base = if let Some(b) = r.attr("base") {
            let q = self.resolve(r, b, s)?;
            self.type_ref(&q, self.file(s), depth + 1)?
        } else if let Some(inner) = r.child(XSD_NS, "simpleType") {
            self.simple_type(inner, s, depth + 1)?
        } else {
            json!({"type": "string"})
        };
        apply_facets(&mut base, r);
        Ok(base)
    }

    /// The object schema of a complexType.
    fn object(&self, ct: &Element, s: usize, depth: usize) -> Result<Value, ImportError> {
        let mut obj = Obj::default();
        self.content(ct, s, &mut obj, depth)?;
        Ok(obj.finish(&self.schemas[s]))
    }

    /// Add the content model and attributes of `el` (a complexType, or an
    /// extension/restriction inside one) to `obj`.
    fn content(
        &self,
        el: &Element,
        s: usize,
        obj: &mut Obj,
        depth: usize,
    ) -> Result<(), ImportError> {
        self.check_depth(depth, s)?;
        for child in el.children_in(XSD_NS) {
            match child.name.local.as_str() {
                "sequence" | "all" | "choice" => {
                    self.particles(child, s, obj, false, false, depth + 1)?;
                }
                "group" => self.group_ref(child, s, obj, false, false, depth + 1)?,
                "attribute" => self.attribute(child, s, obj, depth + 1)?,
                "attributeGroup" => self.attribute_group(child, s, obj, depth + 1)?,
                "simpleContent" => self.simple_content(child, s, obj, depth + 1)?,
                "complexContent" => self.complex_content(child, s, obj, depth + 1)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn particles(
        &self,
        group: &Element,
        s: usize,
        obj: &mut Obj,
        optional: bool,
        in_choice: bool,
        depth: usize,
    ) -> Result<(), ImportError> {
        self.check_depth(depth, s)?;
        let is_choice = group.name.local == "choice";
        let optional = optional || is_choice || min_occurs(group) == 0;
        let in_choice = in_choice || is_choice;
        for child in group.children_in(XSD_NS) {
            match child.name.local.as_str() {
                "element" => self.local_element(child, s, obj, optional, in_choice, depth + 1)?,
                "sequence" | "all" | "choice" => {
                    self.particles(child, s, obj, optional, in_choice, depth + 1)?;
                }
                "group" => self.group_ref(child, s, obj, optional, in_choice, depth + 1)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn group_ref(
        &self,
        el: &Element,
        s: usize,
        obj: &mut Obj,
        optional: bool,
        in_choice: bool,
        depth: usize,
    ) -> Result<(), ImportError> {
        let Some(r) = el.attr("ref") else {
            return Ok(());
        };
        let q = self.resolve(el, r, s)?;
        let g = self
            .groups
            .get(&q)
            .ok_or_else(|| invalid(self.file(s), format!("unknown group \"{q}\"")))?;
        let optional = optional || min_occurs(el) == 0;
        let key = (q, optional, in_choice);
        let cached = self.group_memo.borrow().get(&key).cloned();
        let memo = match cached {
            Some(memo) => {
                self.charge(memo.1, s)?;
                memo
            }
            None => {
                let mut sub = Obj::default();
                for child in g.el.children_in(XSD_NS) {
                    if matches!(child.name.local.as_str(), "sequence" | "all" | "choice") {
                        self.particles(child, g.schema, &mut sub, optional, in_choice, depth + 1)?;
                    }
                }
                let size = sub.size();
                let memo = Rc::new((sub, size));
                self.group_memo.borrow_mut().insert(key, Rc::clone(&memo));
                memo
            }
        };
        obj.merge(&memo.0);
        Ok(())
    }

    fn local_element(
        &self,
        el: &Element,
        s: usize,
        obj: &mut Obj,
        optional: bool,
        in_choice: bool,
        depth: usize,
    ) -> Result<(), ImportError> {
        let (name, base) = if let Some(r) = el.attr("ref") {
            let q = self.resolve(el, r, s)?;
            let schema = self.element_ref(&q, self.file(s))?;
            (q.local, schema)
        } else {
            let Some(name) = el.attr("name") else {
                return Err(invalid(self.file(s), "element without a name or ref"));
            };
            (name.to_owned(), self.element_type(el, s, depth + 1)?)
        };
        let min = min_occurs(el);
        let max = max_occurs(el);
        if max == Some(0) {
            return Ok(());
        }
        let item = if el.attr("nillable") == Some("true") {
            json!({"anyOf": [base, {"type": "null"}]})
        } else {
            base
        };
        let schema = if max == Some(1) {
            item
        } else {
            let mut array = Map::new();
            array.insert("type".into(), json!("array"));
            array.insert("items".into(), item);
            if min > 0 {
                array.insert("minItems".into(), json!(min));
            }
            if let Some(max) = max {
                array.insert("maxItems".into(), json!(max));
            }
            Value::Object(array)
        };
        obj.element(name, schema, !optional && min >= 1, in_choice);
        Ok(())
    }

    fn attribute(
        &self,
        el: &Element,
        s: usize,
        obj: &mut Obj,
        depth: usize,
    ) -> Result<(), ImportError> {
        let usage = el.attr("use");
        if usage == Some("prohibited") {
            return Ok(());
        }
        let (name, schema) = if let Some(r) = el.attr("ref") {
            let q = self.resolve(el, r, s)?;
            let schema = match self.attributes.get(&q) {
                Some(a) => self.attribute_type(a.el, a.schema, depth + 1)?,
                None => json!({"type": "string"}),
            };
            (q.local, schema)
        } else {
            let Some(name) = el.attr("name") else {
                return Err(invalid(self.file(s), "attribute without a name or ref"));
            };
            (name.to_owned(), self.attribute_type(el, s, depth + 1)?)
        };
        obj.add(format!("@{name}"), schema, usage == Some("required"));
        Ok(())
    }

    fn attribute_type(&self, el: &Element, s: usize, depth: usize) -> Result<Value, ImportError> {
        if let Some(t) = el.attr("type") {
            let q = self.resolve(el, t, s)?;
            return self.type_ref(&q, self.file(s), depth + 1);
        }
        if let Some(st) = el.child(XSD_NS, "simpleType") {
            return self.simple_type(st, s, depth + 1);
        }
        Ok(json!({"type": "string"}))
    }

    fn attribute_group(
        &self,
        el: &Element,
        s: usize,
        obj: &mut Obj,
        depth: usize,
    ) -> Result<(), ImportError> {
        self.check_depth(depth, s)?;
        let Some(r) = el.attr("ref") else {
            return Ok(());
        };
        let q = self.resolve(el, r, s)?;
        let g = self
            .attr_groups
            .get(&q)
            .ok_or_else(|| invalid(self.file(s), format!("unknown attributeGroup \"{q}\"")))?;
        let cached = self.attr_group_memo.borrow().get(&q).cloned();
        let memo = match cached {
            Some(memo) => {
                self.charge(memo.1, s)?;
                memo
            }
            None => {
                let mut sub = Obj::default();
                for child in g.el.children_in(XSD_NS) {
                    match child.name.local.as_str() {
                        "attribute" => self.attribute(child, g.schema, &mut sub, depth + 1)?,
                        "attributeGroup" => {
                            self.attribute_group(child, g.schema, &mut sub, depth + 1)?;
                        }
                        _ => {}
                    }
                }
                let size = sub.size();
                let memo = Rc::new((sub, size));
                self.attr_group_memo
                    .borrow_mut()
                    .insert(q, Rc::clone(&memo));
                memo
            }
        };
        obj.merge(&memo.0);
        Ok(())
    }

    /// `simpleContent`: the text as `#text`, plus attributes.
    fn simple_content(
        &self,
        sc: &Element,
        s: usize,
        obj: &mut Obj,
        depth: usize,
    ) -> Result<(), ImportError> {
        for derivation in sc.children_in(XSD_NS) {
            let local = derivation.name.local.as_str();
            if !matches!(local, "extension" | "restriction") {
                continue;
            }
            if let Some(b) = derivation.attr("base") {
                let q = self.resolve(derivation, b, s)?;
                match self.complex.get(&q) {
                    Some(base) => self.content(base.el, base.schema, obj, depth + 1)?,
                    None => {
                        let text = self.type_ref(&q, self.file(s), depth + 1)?;
                        obj.add("#text".into(), text, true);
                    }
                }
            }
            if local == "restriction"
                && let Some(text) = obj.props.get_mut("#text")
            {
                apply_facets(text, derivation);
            }
            for child in derivation.children_in(XSD_NS) {
                match child.name.local.as_str() {
                    "attribute" => self.attribute(child, s, obj, depth + 1)?,
                    "attributeGroup" => self.attribute_group(child, s, obj, depth + 1)?,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// `complexContent`: an extension flattens its base's properties first;
    /// a restriction restates its content.
    fn complex_content(
        &self,
        cc: &Element,
        s: usize,
        obj: &mut Obj,
        depth: usize,
    ) -> Result<(), ImportError> {
        for derivation in cc.children_in(XSD_NS) {
            match derivation.name.local.as_str() {
                "extension" => {
                    if let Some(b) = derivation.attr("base") {
                        let q = self.resolve(derivation, b, s)?;
                        if q.ns != XSD_NS {
                            let base = self.complex.get(&q).ok_or_else(|| {
                                invalid(self.file(s), format!("unknown type \"{q}\""))
                            })?;
                            self.content(base.el, base.schema, obj, depth + 1)?;
                        }
                    }
                    self.content(derivation, s, obj, depth + 1)?;
                }
                "restriction" => self.content(derivation, s, obj, depth + 1)?,
                _ => {}
            }
        }
        Ok(())
    }
}
