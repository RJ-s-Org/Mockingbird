#![cfg(feature = "soap")]
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mb_engine::{Engine, EngineConfig, SchemaValidator};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_mcm::{Bundle, StatusSelector};
use mb_protocols::soap::{self, envelope_body_to_json};
use mb_protocols::{MockRuntime, MountPrefix, RequestRecord, RequestRecorder};
use mb_state::MemoryStore;
use serde_json::{Value, json};

const SOAP11_NS: &str = "http://schemas.xmlsoap.org/soap/envelope/";
const SOAP12_NS: &str = "http://www.w3.org/2003/05/soap-envelope";

fn fixture(name: &str) -> Arc<Bundle> {
    let path = format!("{}/../../fixtures/wsdl/{name}", env!("CARGO_MANIFEST_DIR"));
    Arc::new(compile(&std::fs::read(path).unwrap()).unwrap())
}

#[derive(Default)]
struct Records(Mutex<Vec<RequestRecord>>);

impl RequestRecorder for Records {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

fn scripted(script: Vec<Value>) -> Arc<ScriptedProvider> {
    Arc::new(ScriptedProvider::new(
        script.into_iter().map(|v| Ok(v.to_string())).collect(),
    ))
}

fn runtime(bundle: Arc<Bundle>, provider: &Arc<ScriptedProvider>) -> MockRuntime {
    let provider = Some(Arc::clone(provider) as Arc<dyn Provider>);
    MockRuntime::new(
        "soapy",
        bundle,
        Arc::new(Engine::new(provider, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    )
}

/// Serves `rt` on an ephemeral port behind mount prefix `prefix`; returns
/// its base URL.
async fn serve(rt: MockRuntime, prefix: &str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = soap::try_router(Arc::new(rt))
        .unwrap()
        .layer(axum::Extension(MountPrefix(prefix.to_string())));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

struct Answer {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: String,
}

async fn post(
    base: &str,
    path: &str,
    content_type: &str,
    headers: &[(&str, &str)],
    body: String,
) -> Answer {
    let mut req = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header("content-type", content_type)
        .body(body);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.unwrap();
    Answer {
        status: resp.status().as_u16(),
        headers: resp.headers().clone(),
        body: resp.text().await.unwrap(),
    }
}

fn envelope11(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><soap:Envelope xmlns:soap="{SOAP11_NS}"><soap:Header/><soap:Body>{body}</soap:Body></soap:Envelope>"#
    )
}

fn envelope12(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><env:Envelope xmlns:env="{SOAP12_NS}"><env:Body>{body}</env:Body></env:Envelope>"#
    )
}

fn add_body(a: &str, b: &str) -> String {
    format!(r#"<Add xmlns="http://tempuri.org/"><intA>{a}</intA><intB>{b}</intB></Add>"#)
}

fn schema(bundle: &Bundle, op: &str, status: u16) -> Value {
    let op = bundle.operation(op).unwrap();
    let variant = op
        .outputs
        .iter()
        .find(|o| o.status == StatusSelector::Code(status))
        .unwrap();
    bundle.with_defs(variant.schema.as_ref().unwrap())
}

fn assert_valid(schema: &Value, value: &Value) {
    let validator = SchemaValidator::new(schema).unwrap();
    if let Err(errors) = validator.validate(value) {
        panic!("{value} does not validate: {errors:?}");
    }
}

fn content_type(a: &Answer) -> &str {
    a.headers.get("content-type").unwrap().to_str().unwrap()
}

#[tokio::test]
async fn soap11_add_round_trip() {
    let bundle = fixture("calculator.wsdl");
    let provider = scripted(vec![json!({"AddResult": 5})]);
    let records = Arc::new(Records::default());
    let mut rt = runtime(Arc::clone(&bundle), &provider);
    rt.recorder = Some(records.clone());
    let base = serve(rt, "").await;
    let r = post(
        &base,
        "/calculator.asmx",
        "text/xml; charset=utf-8",
        &[("SOAPAction", "\"http://tempuri.org/Add\"")],
        envelope11(&add_body("2", "3")),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        content_type(&r).starts_with("text/xml"),
        "{}",
        content_type(&r)
    );
    assert_eq!(r.headers.get("x-mockingbird-source").unwrap(), "llm");
    assert!(r.body.contains(SOAP11_NS), "{}", r.body);
    let out_schema = schema(&bundle, "Add", 200);
    let value = envelope_body_to_json(&r.body, &out_schema).unwrap();
    // `Add` is a create: the engine also reflects the request fields.
    assert_eq!(value["AddResult"], 5);
    assert_valid(&out_schema, &value);

    let records = records.0.lock().unwrap();
    assert_eq!(records.len(), 1);
    let rec = &records[0];
    assert_eq!(rec.operation_id.as_deref(), Some("Add"));
    assert_eq!(rec.status, 200);
    assert_eq!(rec.request["body"], json!({"intA": 2, "intB": 3}));
    assert!(rec.request["headers"].is_object());
}

#[tokio::test]
async fn soap12_dispatch_by_action_param() {
    let bundle = fixture("calculator.wsdl");
    let provider = scripted(vec![json!({"SubtractResult": 7})]);
    let base = serve(runtime(Arc::clone(&bundle), &provider), "").await;
    let body = r#"<t:Subtract xmlns:t="http://tempuri.org/"><t:intA>9</t:intA><t:intB>2</t:intB></t:Subtract>"#;
    // The action names Subtract; the body QName agrees.
    let r = post(
        &base,
        "/calculator.asmx",
        "application/soap+xml; charset=utf-8; action=\"http://tempuri.org/Subtract\"",
        &[],
        envelope12(body),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(content_type(&r).starts_with("application/soap+xml"));
    assert!(r.body.contains(SOAP12_NS), "{}", r.body);
    let value = envelope_body_to_json(&r.body, &schema(&bundle, "Subtract", 200)).unwrap();
    assert_eq!(value, json!({"SubtractResult": 7}));

    // The action wins over the body QName.
    let provider = scripted(vec![json!({"AddResult": 1})]);
    let base = serve(runtime(Arc::clone(&bundle), &provider), "").await;
    let r = post(
        &base,
        "/calculator.asmx",
        "application/soap+xml; action=\"http://tempuri.org/Add\"",
        &[],
        envelope12(body),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("AddResponse"), "{}", r.body);
}

#[tokio::test]
async fn dispatch_by_body_qname_without_action() {
    let bundle = fixture("calculator.wsdl");
    let provider = scripted(vec![json!({"SubtractResult": 1})]);
    let base = serve(runtime(Arc::clone(&bundle), &provider), "").await;
    let body = r#"<Subtract xmlns="http://tempuri.org/"><intA>3</intA><intB>2</intB></Subtract>"#;
    let r = post(&base, "/calculator.asmx", "text/xml", &[], envelope11(body)).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let value = envelope_body_to_json(&r.body, &schema(&bundle, "Subtract", 200)).unwrap();
    assert_eq!(value, json!({"SubtractResult": 1}));

    // An unknown QName and no action: a Client fault.
    let r = post(
        &base,
        "/calculator.asmx",
        "text/xml",
        &[("SOAPAction", "\"\"")],
        envelope11(r#"<Multiply xmlns="http://tempuri.org/"/>"#),
    )
    .await;
    assert_eq!(r.status, 500);
    assert!(r.body.contains("soap:Client"), "{}", r.body);
    assert!(r.body.contains("unknown operation"), "{}", r.body);
}

#[tokio::test]
async fn invalid_input_returns_client_fault_without_llm() {
    let bundle = fixture("calculator.wsdl");
    let provider = scripted(vec![]);
    let base = serve(runtime(bundle, &provider), "").await;
    let r = post(
        &base,
        "/calculator.asmx",
        "text/xml; charset=utf-8",
        &[("SOAPAction", "http://tempuri.org/Add")],
        envelope11(r#"<Add xmlns="http://tempuri.org/"><intA>two</intA></Add>"#),
    )
    .await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(
        r.body.contains("<faultcode>soap:Client</faultcode>"),
        "{}",
        r.body
    );
    assert!(
        r.body.contains("intB"),
        "missing-field error listed: {}",
        r.body
    );
    assert!(r.body.contains("/intA"), "type error listed: {}", r.body);
    assert!(provider.requests().is_empty());

    // SOAP 1.2: a Sender fault answered 400.
    let r = post(
        &base,
        "/calculator.asmx",
        "application/soap+xml",
        &[],
        envelope12(r#"<Add xmlns="http://tempuri.org/"><intA>two</intA></Add>"#),
    )
    .await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("env:Sender"), "{}", r.body);
    assert!(r.body.contains(r#"xml:lang="en""#), "{}", r.body);
    assert!(provider.requests().is_empty());

    // An unsupported content type: 415 with a 1.1 Client fault.
    let r = post(
        &base,
        "/calculator.asmx",
        "application/json",
        &[],
        "{}".into(),
    )
    .await;
    assert_eq!(r.status, 415, "{}", r.body);
    assert!(r.body.contains("soap:Client"), "{}", r.body);

    // An oversized body: 413, in the request's version.
    let huge = envelope12(&"x".repeat(4 * 1024 * 1024));
    let r = post(&base, "/calculator.asmx", "application/soap+xml", &[], huge).await;
    assert_eq!(r.status, 413, "{}", r.body);
    assert!(r.body.contains("env:Sender"), "{}", r.body);
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn forced_500_returns_fault_with_detail() {
    let bundle = fixture("calculator.wsdl");
    let fault = json!({"message": "cannot divide by zero", "dividend": 4.5});
    let provider = scripted(vec![fault.clone(), fault.clone()]);
    let base = serve(runtime(Arc::clone(&bundle), &provider), "").await;
    let body = r#"<Divide xmlns="http://tempuri.org/"><dividend>4.5</dividend><divisor>0</divisor></Divide>"#;
    let r = post(
        &base,
        "/calculator.asmx",
        "text/xml",
        &[
            ("SOAPAction", "\"http://tempuri.org/Divide\""),
            ("X-Mockingbird-Status", "500"),
        ],
        envelope11(body),
    )
    .await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(
        r.body.contains("<faultcode>soap:Server</faultcode>"),
        "{}",
        r.body
    );
    assert!(r.body.contains("<faultstring>"), "{}", r.body);
    assert!(r.headers.get("x-mockingbird-source").is_some());
    let start = r.body.find("<detail>").unwrap() + "<detail>".len();
    let end = r.body.find("</detail>").unwrap();
    let detail = &r.body[start..end];
    let fault_schema = schema(&bundle, "Divide", 500);
    let value = envelope_body_to_json(&envelope11(detail), &fault_schema).unwrap();
    assert_eq!(value, fault);
    assert_valid(&fault_schema, &value);

    let r = post(
        &base,
        "/calculator.asmx",
        "application/soap+xml; action=\"http://tempuri.org/Divide\"",
        &[("X-Mockingbird-Status", "500")],
        envelope12(body),
    )
    .await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(r.body.contains("env:Receiver"), "{}", r.body);
    assert!(r.body.contains("<env:Detail>"), "{}", r.body);

    // An operation without a declared fault: a Fault without detail and
    // no engine call.
    let before = provider.requests().len();
    let r = post(
        &base,
        "/calculator.asmx",
        "text/xml",
        &[
            ("SOAPAction", "http://tempuri.org/Add"),
            ("X-Mockingbird-Status", "503"),
        ],
        envelope11(&add_body("1", "2")),
    )
    .await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(r.body.contains("soap:Server"), "{}", r.body);
    assert!(!r.body.contains("<detail>"), "{}", r.body);
    assert_eq!(provider.requests().len(), before);
}

#[tokio::test]
async fn doctype_request_rejected() {
    let bundle = fixture("calculator.wsdl");
    let provider = scripted(vec![]);
    let records = Arc::new(Records::default());
    let mut rt = runtime(bundle, &provider);
    rt.recorder = Some(records.clone());
    let base = serve(rt, "").await;
    let laughs = format!(
        r#"<?xml version="1.0"?>
<!DOCTYPE lolz [
 <!ENTITY lol "lol">
 <!ENTITY lol1 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
 <!ENTITY lol2 "&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;">
 <!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">
]>
<soap:Envelope xmlns:soap="{SOAP11_NS}"><soap:Body><Add xmlns="http://tempuri.org/"><intA>&lol3;</intA><intB>1</intB></Add></soap:Body></soap:Envelope>"#
    );
    let r = post(
        &base,
        "/calculator.asmx",
        "text/xml",
        &[("SOAPAction", "http://tempuri.org/Add")],
        laughs,
    )
    .await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(r.body.contains("soap:Client"), "{}", r.body);
    assert!(r.body.contains("DOCTYPE"), "{}", r.body);
    assert!(provider.requests().is_empty());
    let records = records.0.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].request["body"], "[undecodable]");
}

#[tokio::test]
async fn wsdl_served_with_rewritten_address() {
    let bundle = fixture("calculator.wsdl");
    let provider = scripted(vec![]);
    let base = serve(runtime(bundle, &provider), "/calc").await;
    for query in ["wsdl", "WSDL"] {
        let resp = reqwest::Client::new()
            .get(format!("{base}/calculator.asmx?{query}"))
            .header("host", "127.0.0.1:1234")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/xml; charset=utf-8"
        );
        let text = resp.text().await.unwrap();
        assert_eq!(
            text.matches(r#"location="http://127.0.0.1:1234/calc/calculator.asmx""#)
                .count(),
            2,
            "{text}"
        );
        assert!(!text.contains("www.example.com"), "{text}");
        assert!(text.contains("<wsdl:portType name=\"CalculatorSoap\">"));
    }
}

#[tokio::test]
async fn rpc_literal_state_round_trip() {
    let bundle = fixture("accounts-rpc.wsdl");
    let account = |name: &str| {
        json!({
            "@id": "a-1",
            "name": name,
            "tags": ["vip", "beta"],
            "status": "active",
            "createdAt": "2024-01-01T00:00:00Z",
        })
    };
    let provider = scripted(vec![
        json!({"id": "a-1", "account": account("Ada")}),
        json!({"id": "a-1", "account": account("Someone else")}),
    ]);
    let base = serve(runtime(Arc::clone(&bundle), &provider), "").await;
    let session = [("X-Mockingbird-Session", "s1")];
    let create = r#"<acc:CreateAccount xmlns:acc="urn:example:accounts"><account id="a-1"><name>Ada</name><tags>vip</tags><tags>beta</tags><status>active</status><createdAt>2024-01-01T00:00:00Z</createdAt></account></acc:CreateAccount>"#;
    let r = post(
        &base,
        "/services/accounts",
        "text/xml",
        &[
            session[0],
            ("SOAPAction", "\"urn:example:accounts#CreateAccount\""),
        ],
        envelope11(create),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // rpc/literal: a namespaced wrapper with unqualified parts; the `@id`
    // attribute and the repeated `tags` element.
    assert!(r.body.contains(r#"id="a-1""#), "{}", r.body);
    assert_eq!(r.body.matches("<tags>").count(), 2, "{}", r.body);
    let created = envelope_body_to_json(&r.body, &schema(&bundle, "CreateAccount", 200)).unwrap();
    assert_eq!(created["account"]["name"], "Ada");

    let get = r#"<acc:GetAccount xmlns:acc="urn:example:accounts"><id>a-1</id></acc:GetAccount>"#;
    let r = post(
        &base,
        "/services/accounts",
        "text/xml",
        &[
            session[0],
            ("SOAPAction", "urn:example:accounts#GetAccount"),
        ],
        envelope11(get),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let got = envelope_body_to_json(&r.body, &schema(&bundle, "GetAccount", 200)).unwrap();
    assert_eq!(got["id"], "a-1");
    assert_eq!(got["account"]["name"], "Ada");
    assert_eq!(got["account"]["tags"], json!(["vip", "beta"]));
    assert_valid(&schema(&bundle, "GetAccount", 200), &got);
}

/// The `?wsdl` document of `rt` behind mount prefix `/calc`, fetched with
/// `headers`.
async fn wsdl_with(rt: MockRuntime, headers: &[(&str, &str)]) -> String {
    let base = serve(rt, "/calc").await;
    let mut req = reqwest::Client::new()
        .get(format!("{base}/calculator.asmx?wsdl"))
        .header("host", "127.0.0.1:1234");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    resp.text().await.unwrap()
}

fn calculator_runtime() -> MockRuntime {
    runtime(fixture("calculator.wsdl"), &scripted(vec![]))
}

fn assert_location(text: &str, base: &str) {
    let location = format!(r#"location="{base}/calc/calculator.asmx""#);
    assert_eq!(text.matches(&location).count(), 2, "{location}\n{text}");
}

#[tokio::test]
async fn wsdl_uses_forwarded_headers_when_trusted() {
    let mut rt = calculator_runtime();
    rt.protocols.trust_forwarded_headers = true;
    let text = wsdl_with(
        rt,
        &[
            ("x-forwarded-proto", "https, http"),
            ("x-forwarded-host", "api.example.org, proxy.internal"),
        ],
    )
    .await;
    assert_location(&text, "https://api.example.org");
}

#[tokio::test]
async fn wsdl_ignores_forwarded_headers_by_default() {
    let text = wsdl_with(
        calculator_runtime(),
        &[
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "api.example.org"),
            ("forwarded", "proto=https;host=api.example.org"),
        ],
    )
    .await;
    assert_location(&text, "http://127.0.0.1:1234");
}

#[tokio::test]
async fn wsdl_forwarded_rfc7239() {
    let mut rt = calculator_runtime();
    rt.protocols.trust_forwarded_headers = true;
    let text = wsdl_with(
        rt,
        &[(
            "forwarded",
            r#"for=192.0.2.60;proto=https;host="mocks.example.org:8443", for=10.0.0.1;proto=http;host=inner"#,
        )],
    )
    .await;
    assert_location(&text, "https://mocks.example.org:8443");
}

#[tokio::test]
async fn wsdl_https_when_tls_terminated() {
    let mut rt = calculator_runtime();
    rt.protocols.tls_terminated = true;
    let text = wsdl_with(rt, &[("x-forwarded-proto", "http")]).await;
    assert_location(&text, "https://127.0.0.1:1234");

    // TLS termination wins over a forwarded scheme, even when trusted.
    let mut rt = calculator_runtime();
    rt.protocols.tls_terminated = true;
    rt.protocols.trust_forwarded_headers = true;
    let text = wsdl_with(
        rt,
        &[
            ("x-forwarded-proto", "http"),
            ("x-forwarded-host", "a.example"),
        ],
    )
    .await;
    assert_location(&text, "https://a.example");
}

#[tokio::test]
async fn wsdl_rejects_bad_forwarded_host() {
    for bad in ["evil.example/path", "evil example", "a\tb"] {
        let mut rt = calculator_runtime();
        rt.protocols.trust_forwarded_headers = true;
        let text = wsdl_with(
            rt,
            &[
                ("x-forwarded-host", bad),
                ("x-forwarded-proto", "javascript"),
            ],
        )
        .await;
        assert_location(&text, "http://127.0.0.1:1234");
        assert!(!text.contains("evil"), "{text}");
    }
    let mut rt = calculator_runtime();
    rt.protocols.trust_forwarded_headers = true;
    let text = wsdl_with(rt, &[("forwarded", "host=\"x/y\";proto=https")]).await;
    assert_location(&text, "https://127.0.0.1:1234");
}
