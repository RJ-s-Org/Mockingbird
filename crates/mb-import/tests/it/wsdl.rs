#![cfg(test)]
use mb_import::{ImportError, SourceFile, SourceFormat, compile, compile_files, sniff};
use mb_mcm::{
    Binding, Bundle, Effect, Identity, OperationKind, Protocol, ProtocolPayload, SoapVersion,
    StatusSelector,
};
use serde_json::{Value, json};

fn fixture_dir() -> String {
    format!("{}/../../fixtures/wsdl", env!("CARGO_MANIFEST_DIR"))
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/{name}", fixture_dir())).unwrap()
}

fn calculator() -> Bundle {
    compile(&read("calculator.wsdl")).unwrap()
}

fn accounts() -> Bundle {
    compile(&read("accounts-rpc.wsdl")).unwrap()
}

fn validate(bundle: &Bundle, schema: &Value, instance: &Value) -> Vec<String> {
    let schema = bundle.with_reachable_defs(schema);
    let validator = jsonschema::draft202012::new(&schema).unwrap();
    validator
        .iter_errors(instance)
        .map(|e| e.to_string())
        .collect()
}

#[test]
fn calculator_doc_literal_snapshot() {
    let b = calculator();
    assert_eq!(
        sniff(&read("calculator.wsdl")).unwrap(),
        SourceFormat::Wsdl11
    );
    insta::assert_yaml_snapshot!(b, {
        ".source_digest" => "[digest]",
        ".protocol_payload" => "[wsdl]",
    });
}

#[test]
fn accounts_rpc_literal_snapshot() {
    let b = accounts();
    insta::assert_yaml_snapshot!(b, {
        ".source_digest" => "[digest]",
        ".protocol_payload" => "[wsdl]",
    });
}

#[test]
fn soap_bindings_versions_and_actions() {
    let b = calculator();
    assert_eq!(b.title, "Calculator");
    assert_eq!(b.id, "calculator");
    assert_eq!(b.version, "0");
    assert_eq!(b.protocol(), Protocol::Soap);
    let bytes = read("calculator.wsdl");
    assert_eq!(
        b.protocol_payload,
        Some(ProtocolPayload::Soap {
            wsdl: String::from_utf8(bytes).unwrap()
        })
    );
    let ids: Vec<&str> = b.operations.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["Add", "Subtract", "Divide"]);

    let add = b.operation("Add").unwrap();
    assert_eq!(
        add.binding,
        Binding::Soap {
            service: "Calculator".into(),
            port: "CalculatorSoap".into(),
            operation: "Add".into(),
            soap_action: Some("http://tempuri.org/Add".into()),
            version: SoapVersion::Soap11,
            address_path: "/calculator.asmx".into(),
        }
    );
    assert_eq!(add.kind, OperationKind::Command(Effect::Create));
    assert!(add.input.body_required);
    assert_eq!(
        add.input.body,
        Some(json!({
            "$ref": "#/$defs/Add",
            "x-xml-root": {"name": "Add", "namespace": "http://tempuri.org/"}
        }))
    );
    assert_eq!(add.outputs[0].status, StatusSelector::Code(200));
    assert_eq!(add.outputs[0].content_type.as_deref(), Some("text/xml"));
    assert_eq!(
        b.defs["Add"],
        json!({
            "type": "object",
            "properties": {
                "intA": {"type": "integer", "minimum": -2147483648i64, "maximum": 2147483647},
                "intB": {"type": "integer", "minimum": -2147483648i64, "maximum": 2147483647}
            },
            "required": ["intA", "intB"],
            "x-xml-order": ["intA", "intB"],
            "x-xml-namespace": "http://tempuri.org/",
            "x-xml-qualified": true
        })
    );

    // rpc style, an empty soapAction, and the SOAP address path.
    let a = accounts();
    let get = a.operation("GetAccount").unwrap();
    let Binding::Soap {
        soap_action,
        version,
        address_path,
        port,
        ..
    } = &get.binding
    else {
        panic!("not a SOAP binding")
    };
    assert_eq!(
        soap_action.as_deref(),
        Some("urn:example:accounts#GetAccount")
    );
    assert_eq!(*version, SoapVersion::Soap11);
    assert_eq!(address_path, "/services/accounts");
    assert_eq!(port, "AccountPort");
    let delete = a.operation("DeleteAccount").unwrap();
    assert!(matches!(
        &delete.binding,
        Binding::Soap {
            soap_action: None,
            ..
        }
    ));

    // A service with only a SOAP 1.2 port.
    let files = external_files();
    let ext = compile_files(&files).unwrap();
    let op = ext.operation("GetOrder").unwrap();
    assert!(matches!(
        &op.binding,
        Binding::Soap { version: SoapVersion::Soap12, address_path, .. } if address_path == "/orders"
    ));
}

#[test]
fn rpc_wrappers_and_identity() {
    let a = accounts();
    let get = a.operation("GetAccount").unwrap();
    assert_eq!(get.kind, OperationKind::Query);
    assert_eq!(get.resource_type.as_deref(), Some("account"));
    assert_eq!(
        get.input.body,
        Some(json!({
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"],
            "x-xml-order": ["id"],
            "x-xml-namespace": "urn:example:accounts",
            "x-xml-qualified": false,
            "x-xml-root": {"name": "GetAccount", "namespace": "urn:example:accounts"}
        }))
    );
    let out = get.outputs[0].schema.as_ref().unwrap();
    assert_eq!(
        out["x-xml-root"],
        json!({"name": "GetAccountResponse", "namespace": "urn:example:accounts"})
    );
    assert_eq!(out["x-xml-order"], json!(["id", "account"]));
    assert_eq!(
        out["properties"]["account"],
        json!({"$ref": "#/$defs/Account"})
    );
    assert_eq!(
        get.identity,
        Some(Identity {
            path_param: None,
            field: "id".into(),
            input_field: Some("id".into()),
            path_field: None,
        })
    );
    let create = a.operation("CreateAccount").unwrap();
    assert_eq!(create.kind, OperationKind::Command(Effect::Create));
    assert_eq!(
        create.identity,
        Some(Identity {
            path_param: None,
            field: "id".into(),
            input_field: None,
            path_field: None,
        })
    );
    let delete = a.operation("DeleteAccount").unwrap();
    assert_eq!(delete.kind, OperationKind::Command(Effect::Delete));
    assert_eq!(
        delete.identity.as_ref().unwrap().input_field.as_deref(),
        Some("id")
    );

    let account = &a.defs["Account"];
    assert_eq!(account["properties"]["@id"], json!({"type": "string"}));
    assert_eq!(
        account["properties"]["status"],
        json!({"type": "string", "enum": ["active", "suspended", "closed"]})
    );
    assert_eq!(
        account["properties"]["tags"],
        json!({"type": "array", "items": {"type": "string"}})
    );
    assert_eq!(
        account["properties"]["createdAt"],
        json!({"type": "string", "format": "date-time"})
    );
    let instance = json!({
        "id": "a1",
        "account": {"@id": "a1", "name": "Ada", "tags": ["x", "y"], "status": "active",
                    "createdAt": "2026-01-01T00:00:00Z"}
    });
    assert!(validate(&a, out, &instance).is_empty());
}

const INLINE: &str = r#"<?xml version="1.0"?>
<wsdl:definitions name="Inline"
    xmlns:wsdl="http://schemas.xmlsoap.org/wsdl/"
    xmlns:soap="http://schemas.xmlsoap.org/wsdl/soap/"
    xmlns:xs="http://www.w3.org/2001/XMLSchema"
    xmlns:tns="urn:inline"
    targetNamespace="urn:inline">
  <wsdl:types>
    <xs:schema targetNamespace="urn:inline" elementFormDefault="qualified">
      <xs:simpleType name="Color">
        <xs:restriction base="xs:string">
          <xs:enumeration value="red"/>
          <xs:enumeration value="green"/>
        </xs:restriction>
      </xs:simpleType>
      <xs:simpleType name="Code">
        <xs:restriction base="xs:string">
          <xs:pattern value="[A-Z]{3}"/>
          <xs:minLength value="3"/>
          <xs:maxLength value="3"/>
        </xs:restriction>
      </xs:simpleType>
      <xs:simpleType name="Percent">
        <xs:restriction base="xs:integer">
          <xs:minInclusive value="0"/>
          <xs:maxExclusive value="101"/>
        </xs:restriction>
      </xs:simpleType>
      <xs:complexType name="Money">
        <xs:simpleContent>
          <xs:extension base="xs:decimal">
            <xs:attribute name="currency" type="xs:string" use="required"/>
          </xs:extension>
        </xs:simpleContent>
      </xs:complexType>
      <xs:complexType name="Base">
        <xs:sequence>
          <xs:element name="id" type="xs:string"/>
        </xs:sequence>
      </xs:complexType>
      <xs:complexType name="Item">
        <xs:complexContent>
          <xs:extension base="tns:Base">
            <xs:sequence>
              <xs:element name="price" type="tns:Money"/>
              <xs:element name="note" type="xs:string" minOccurs="0" nillable="true"/>
              <xs:element name="color" type="tns:Color"/>
              <xs:element name="code" type="tns:Code"/>
              <xs:element name="percent" type="tns:Percent"/>
              <xs:element name="sizes" type="xs:unsignedByte" minOccurs="1" maxOccurs="4"/>
              <xs:element name="updatedAt" type="xs:dateTime"/>
              <xs:element name="day" type="xs:date"/>
              <xs:element name="blob" type="xs:hexBinary"/>
              <xs:element name="data" type="xs:base64Binary"/>
              <xs:element name="ttl" type="xs:duration"/>
              <xs:element name="anything" type="xs:anyType"/>
              <xs:choice>
                <xs:element name="sku" type="xs:string"/>
                <xs:element name="ean" type="xs:long"/>
              </xs:choice>
            </xs:sequence>
            <xs:attribute name="version" type="xs:int"/>
          </xs:extension>
        </xs:complexContent>
      </xs:complexType>
      <xs:element name="Money" type="tns:Money"/>
      <xs:element name="Put">
        <xs:complexType>
          <xs:sequence>
            <xs:element name="item" type="tns:Item" maxOccurs="unbounded"/>
          </xs:sequence>
        </xs:complexType>
      </xs:element>
    </xs:schema>
  </wsdl:types>
  <wsdl:message name="PutIn"><wsdl:part name="parameters" element="tns:Put"/></wsdl:message>
  <wsdl:message name="PutOut"><wsdl:part name="parameters" element="tns:Money"/></wsdl:message>
  <wsdl:portType name="P">
    <wsdl:operation name="Put">
      <wsdl:input message="tns:PutIn"/>
      <wsdl:output message="tns:PutOut"/>
    </wsdl:operation>
  </wsdl:portType>
  <wsdl:binding name="B" type="tns:P">
    <soap:binding style="document" transport="http://schemas.xmlsoap.org/soap/http"/>
    <wsdl:operation name="Put">
      <soap:operation soapAction=""/>
      <wsdl:input><soap:body use="literal"/></wsdl:input>
      <wsdl:output><soap:body use="literal"/></wsdl:output>
    </wsdl:operation>
  </wsdl:binding>
  <wsdl:service name="S">
    <wsdl:port name="Port" binding="tns:B"><soap:address location="urn:no-path"/></wsdl:port>
  </wsdl:service>
</wsdl:definitions>
"#;

#[test]
fn xsd_mapping_exact() {
    let b = compile(INLINE.as_bytes()).unwrap();
    let names: Vec<&str> = b.defs.keys().map(String::as_str).collect();
    assert_eq!(names, ["Base", "Item", "Money", "Money_2", "Put"]);
    assert_eq!(b.defs["Money_2"], json!({"$ref": "#/$defs/Money"}));
    assert_eq!(
        b.defs["Money"],
        json!({
            "type": "object",
            "properties": {
                "#text": {"type": "number"},
                "@currency": {"type": "string"}
            },
            "required": ["#text", "@currency"],
            "x-xml-order": [],
            "x-xml-namespace": "urn:inline",
            "x-xml-qualified": true
        })
    );
    assert_eq!(
        b.defs["Item"],
        json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "price": {"$ref": "#/$defs/Money"},
                "note": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                "color": {"type": "string", "enum": ["red", "green"]},
                "code": {"type": "string", "pattern": "^(?:[A-Z]{3})$", "minLength": 3, "maxLength": 3},
                "percent": {"type": "integer", "minimum": 0, "exclusiveMaximum": 101},
                "sizes": {
                    "type": "array",
                    "items": {"type": "integer", "minimum": 0, "maximum": 255},
                    "minItems": 1,
                    "maxItems": 4
                },
                "updatedAt": {"type": "string", "format": "date-time"},
                "day": {"type": "string", "format": "date"},
                "blob": {"type": "string", "format": "hex", "pattern": "^[0-9A-Fa-f]*$"},
                "data": {"type": "string", "format": "byte"},
                "ttl": {"type": "string", "format": "duration"},
                "anything": {},
                "sku": {"type": "string"},
                "ean": {"type": "integer"},
                "@version": {"type": "integer", "minimum": -2147483648i64, "maximum": 2147483647}
            },
            "required": [
                "id", "price", "color", "code", "percent", "sizes", "updatedAt", "day",
                "blob", "data", "ttl", "anything"
            ],
            "x-xml-order": [
                "id", "price", "note", "color", "code", "percent", "sizes", "updatedAt", "day",
                "blob", "data", "ttl", "anything", "sku", "ean"
            ],
            "x-xml-namespace": "urn:inline",
            "x-xml-qualified": true,
            "x-xml-choice": ["sku", "ean"]
        })
    );
    assert_eq!(
        b.defs["Put"]["properties"]["item"],
        json!({"type": "array", "items": {"$ref": "#/$defs/Item"}, "minItems": 1})
    );
    let put = b.operation("Put").unwrap();
    let Binding::Soap {
        address_path,
        soap_action,
        ..
    } = &put.binding
    else {
        panic!("not SOAP")
    };
    assert_eq!(address_path, "/S");
    assert_eq!(soap_action, &None);
    assert_eq!(
        put.outputs[0].schema,
        Some(json!({
            "$ref": "#/$defs/Money_2",
            "x-xml-root": {"name": "Money", "namespace": "urn:inline"}
        }))
    );
    let body = put.input.body.as_ref().unwrap();
    let ok = json!({"item": [{
        "id": "1", "price": {"#text": 9.5, "@currency": "EUR"}, "note": null, "color": "red",
        "code": "ABC", "percent": 100, "sizes": [1, 2], "updatedAt": "2026-01-01T00:00:00Z",
        "day": "2026-01-01", "blob": "0aFF", "data": "AAEC", "ttl": "PT1H", "anything": 1,
        "sku": "s", "@version": 2
    }]});
    assert_eq!(validate(&b, body, &ok), Vec::<String>::new());
    let bad = json!({"item": [{
        "id": "1", "price": {"#text": 9.5}, "color": "blue", "code": "abc", "percent": 101,
        "sizes": [1, 2, 3, 4, 5], "updatedAt": "x", "day": "x", "blob": "zz", "data": "x",
        "ttl": "x", "anything": 1
    }]});
    assert!(validate(&b, body, &bad).len() >= 5);
}

#[test]
fn fault_becomes_500_variant() {
    let b = calculator();
    let divide = b.operation("Divide").unwrap();
    assert_eq!(divide.outputs.len(), 2);
    let fault = &divide.outputs[1];
    assert_eq!(fault.status, StatusSelector::Code(500));
    assert_eq!(fault.content_type.as_deref(), Some("text/xml"));
    assert_eq!(
        fault.schema,
        Some(json!({
            "$ref": "#/$defs/DivideByZero",
            "x-xml-root": {"name": "DivideByZero", "namespace": "http://tempuri.org/"}
        }))
    );
    assert_eq!(b.operation("Add").unwrap().outputs.len(), 1);
}

#[test]
fn wsdl20_unsupported() {
    let wsdl20 = br#"<?xml version="1.0"?>
<description xmlns="http://www.w3.org/ns/wsdl" targetNamespace="urn:x"/>"#;
    let err = compile(wsdl20).unwrap_err();
    assert!(
        matches!(&err, ImportError::Unsupported(m) if m == "WSDL 2.0 is not supported; convert to WSDL 1.1"),
        "{err:?}"
    );
}

#[test]
fn doctype_rejected() {
    let laughs = br#"<?xml version="1.0"?>
<!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;">]>
<definitions xmlns="http://schemas.xmlsoap.org/wsdl/" name="&lol2;"/>"#;
    let err = compile(laughs).unwrap_err();
    assert!(
        matches!(&err, ImportError::Invalid { pointer, message }
            if pointer == "/" && message == "DOCTYPE is not allowed"),
        "{err:?}"
    );
    // An undeclared entity is never expanded.
    let entity = br#"<definitions xmlns="http://schemas.xmlsoap.org/wsdl/" name="&ext;"/>"#;
    let err = compile(entity).unwrap_err();
    assert!(
        matches!(&err, ImportError::Parse(message)
            if message.contains("entity") && message.contains("ext")),
        "{err:?}"
    );
}

fn external_files() -> Vec<SourceFile> {
    ["service.wsdl", "types.xsd"]
        .into_iter()
        .map(|path| SourceFile {
            path: path.into(),
            bytes: read(&format!("external/{path}")),
        })
        .collect()
}

#[test]
fn external_import_requires_upload() {
    let err = compile(&read("external/service.wsdl")).unwrap_err();
    assert!(
        matches!(&err, ImportError::Invalid { message, .. }
            if message == "schemaLocation \"types.xsd\" is not in the upload; external schemas are not fetched"),
        "{err:?}"
    );
    let files = external_files();
    let b = compile_files(&files).unwrap();
    assert_eq!(b.title, "Orders");
    let op = b.operation("GetOrder").unwrap();
    assert_eq!(op.resource_type.as_deref(), Some("order"));
    assert_eq!(
        op.input.body,
        Some(json!({
            "$ref": "#/$defs/GetOrder",
            "x-xml-root": {"name": "GetOrder", "namespace": "urn:example:orders:types"}
        }))
    );
    assert_eq!(b.defs["GetOrderResponse"], json!({"$ref": "#/$defs/Order"}));
    assert_eq!(
        op.identity,
        Some(Identity {
            path_param: None,
            field: "id".into(),
            input_field: Some("id".into()),
            path_field: None,
        })
    );
    // Order-independent and nested paths resolve relative to the importer.
    let mut reversed = files.clone();
    reversed.reverse();
    assert_eq!(compile_files(&reversed).unwrap(), b);
    let nested: Vec<SourceFile> = files
        .iter()
        .map(|f| SourceFile {
            path: format!("api/{}", f.path),
            bytes: f.bytes.clone(),
        })
        .collect();
    assert_eq!(compile_files(&nested).unwrap().defs, b.defs);
    // A missing file in a multi-file upload names the importing file.
    let err = compile_files(&nested[..1]).unwrap_err();
    assert!(
        matches!(&err, ImportError::Invalid { pointer, .. } if pointer == "api/service.wsdl"),
        "{err:?}"
    );
}

#[test]
fn compile_is_deterministic() {
    assert_eq!(calculator(), calculator());
    let b = calculator();
    assert_eq!(b.source_digest.len(), 64);
}
