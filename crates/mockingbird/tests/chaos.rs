//! Per-mock and per-scenario chaos (error injection, latency jitter and
//! stream drops) through an in-process, offline app. Rates of 0 and 1 need
//! no `MB_CHAOS_SEED`.
#![cfg(test)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use mb_engine::SchemaValidator;
use mb_mcm::ProtocolPayload;
use mb_protocols::grpc::DynamicCodec;
use mockingbird::config::{App, ServeArgs, build_app};
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tonic::codegen::http::uri::PathAndQuery;

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel)
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// An offline app built from `config`, written next to any extra files.
async fn app(dir: &Path, config: &str) -> App {
    let path = dir.join("mockingbird.yaml");
    std::fs::write(&path, config).unwrap();
    let args = ServeArgs {
        config: Some(path),
        offline: true,
        ..ServeArgs::default()
    };
    build_app(&args, None).await.unwrap()
}

const THINGS: &str = r##"openapi: 3.0.3
info: {title: Things, version: 1.0.0}
paths:
  /things/{id}:
    get:
      operationId: getThing
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {type: object, required: [id], properties: {id: {type: string}}}
        "404":
          description: Missing
          content:
            application/json:
              schema: {type: object, required: [missing], properties: {missing: {type: boolean}}}
        "503":
          description: Down
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Down"}
components:
  schemas:
    Down:
      type: object
      required: [code, retryInSeconds]
      additionalProperties: false
      properties:
        code: {type: string, enum: [MAINTENANCE]}
        retryInSeconds: {type: integer, minimum: 1, maximum: 60}
"##;

/// A things mock named `name` with `extra` (indented mock keys).
fn things(name: &str, extra: &str) -> String {
    format!("  {name}:\n    contract: things.yaml\n{extra}")
}

async fn things_app(dir: &Path, mocks: &str) -> App {
    std::fs::write(dir.join("things.yaml"), THINGS).unwrap();
    app(dir, &format!("mocks:\n{mocks}")).await
}

const ALWAYS_ERROR: &str = "    chaos: {error_rate: 1}\n";

#[tokio::test]
async fn error_uses_declared_variant() {
    let dir = tempfile::tempdir().unwrap();
    let app = things_app(dir.path(), &things("things", ALWAYS_ERROR)).await;
    let data = serve(app.data.clone()).await;

    let r = reqwest::get(format!("{data}/things/things/t1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.headers()["x-mockingbird-chaos"], "error");
    let body: Value = r.json().await.unwrap();
    let bundle = mb_import::compile(THINGS.as_bytes()).unwrap();
    let op = bundle.operation("getThing").unwrap();
    let schema = op.output_for_status(503).unwrap().schema.as_ref().unwrap();
    let validator = SchemaValidator::new(&bundle.with_reachable_defs(schema)).unwrap();
    validator.validate(&body).unwrap();
    assert_eq!(body["code"], "MAINTENANCE", "{body}");
}

const CHARGES: &str = r##"openapi: 3.0.3
info: {title: Charges, version: 1.0.0}
paths:
  /v1/charges/{charge}:
    get:
      operationId: GetChargesCharge
      parameters:
        - {name: charge, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {type: object, required: [id], properties: {id: {type: string}}}
        default:
          description: Error response.
          content:
            application/json:
              schema: {$ref: "#/components/schemas/error"}
components:
  schemas:
    error:
      type: object
      required: [error]
      properties:
        error: {$ref: "#/components/schemas/api_errors"}
    api_errors:
      type: object
      required: [type]
      properties:
        type: {type: string, enum: [api_error, card_error, invalid_request_error]}
        message: {type: string, maxLength: 40000}
"##;

/// A charges mock with chaos always on, its `default` response `contract`.
async fn charges_app(dir: &Path, contract: &str) -> App {
    std::fs::write(dir.join("charges.yaml"), contract).unwrap();
    app(
        dir,
        &format!("mocks:\n  charges:\n    contract: charges.yaml\n{ALWAYS_ERROR}"),
    )
    .await
}

#[tokio::test]
async fn default_only_error_uses_the_default_shape() {
    let dir = tempfile::tempdir().unwrap();
    let data = serve(charges_app(dir.path(), CHARGES).await.data.clone()).await;
    let r = reqwest::get(format!("{data}/charges/v1/charges/ch_1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.headers()["x-mockingbird-chaos"], "error");
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let body: Value = r.json().await.unwrap();
    let bundle = mb_import::compile(CHARGES.as_bytes()).unwrap();
    let op = bundle.operation("GetChargesCharge").unwrap();
    // No 503 is declared, so this is the `default` variant.
    let schema = op.output_for_status(503).unwrap().schema.as_ref().unwrap();
    SchemaValidator::new(&bundle.with_reachable_defs(schema))
        .unwrap()
        .validate(&body)
        .unwrap();
    assert_eq!(
        body["error"]["message"], "Service Unavailable (injected by chaos)",
        "{body}"
    );
}

/// A `default` without a schema has no shape to fill.
#[tokio::test]
async fn default_without_schema_is_protocol_native() {
    let dir = tempfile::tempdir().unwrap();
    let contract = CHARGES.replace(
        "          description: Error response.\n          content:\n            application/json:\n              schema: {$ref: \"#/components/schemas/error\"}\n",
        "          description: Error response.\n",
    );
    assert_ne!(contract, CHARGES);
    let data = serve(charges_app(dir.path(), &contract).await.data.clone()).await;
    let r = reqwest::get(format!("{data}/charges/v1/charges/ch_1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.headers()["content-type"], "application/problem+json");
}

struct Grpc {
    client: tonic::client::Grpc<tonic::transport::Channel>,
    pool: DescriptorPool,
}

impl Grpc {
    async fn connect(addr: &str, proto: &str) -> Self {
        let bundle = mb_import::compile(&std::fs::read(fixture(proto)).unwrap()).unwrap();
        let Some(ProtocolPayload::Grpc {
            file_descriptor_set,
        }) = &bundle.protocol_payload
        else {
            panic!("no descriptors")
        };
        let pool = DescriptorPool::decode(file_descriptor_set.as_slice()).unwrap();
        let channel = tonic::transport::Endpoint::from_shared(addr.to_string())
            .unwrap()
            .connect()
            .await
            .unwrap();
        Self {
            client: tonic::client::Grpc::new(channel),
            pool,
        }
    }

    fn message(&self, name: &str) -> MessageDescriptor {
        self.pool.get_message_by_name(name).unwrap()
    }
}

#[tokio::test]
async fn undeclared_error_is_protocol_native() {
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "mocks:\n  orders:\n    contract: {orders}\n{ALWAYS_ERROR}  github:\n    contract: {github}\n{ALWAYS_ERROR}  hello:\n    contract: {hello}\n{ALWAYS_ERROR}  calculator:\n    contract: {calculator}\n{ALWAYS_ERROR}",
        orders = fixture("openapi/orders-3.0.yaml").display(),
        github = fixture("graphql/github.graphql").display(),
        hello = fixture("proto/helloworld.proto").display(),
        calculator = fixture("wsdl/calculator.wsdl").display(),
    );
    let app = app(dir.path(), &config).await;
    let data = serve(app.data.clone()).await;
    let grpc_addr = serve(app.grpc.clone()).await;
    let http = reqwest::Client::new();

    // REST: getOrder declares 200 and 404 only.
    let r = http
        .get(format!("{data}/orders/orders/o1"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.headers()["x-mockingbird-chaos"], "error");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["title"], "Service Unavailable", "{body}");
    assert_eq!(body["detail"], "injected by chaos", "{body}");

    // GraphQL: a field error with code UNAVAILABLE.
    let r = http
        .post(format!("{data}/github/graphql"))
        .json(&json!({"query": "{ viewer { login } }"}))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let error = &body["errors"][0];
    assert_eq!(error["extensions"]["code"], "UNAVAILABLE", "{body}");
    assert_eq!(error["extensions"]["chaos"], "error", "{body}");

    // gRPC: UNAVAILABLE with the chaos metadata.
    let mut grpc = Grpc::connect(&grpc_addr, "proto/helloworld.proto").await;
    let request_desc = grpc.message("helloworld.HelloRequest");
    let reply_desc = grpc.message("helloworld.HelloReply");
    let request = DynamicMessage::deserialize(request_desc.clone(), json!({"name": "a"})).unwrap();
    grpc.client.ready().await.unwrap();
    let status = grpc
        .client
        .unary(
            tonic::Request::new(request),
            PathAndQuery::from_static("/helloworld.Greeter/SayHello"),
            DynamicCodec::new(request_desc, reply_desc),
        )
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unavailable, "{status:?}");
    assert_eq!(
        status.metadata().get("x-mockingbird-chaos").unwrap(),
        "error"
    );

    // SOAP: Add declares no fault: a Server fault with HTTP 503.
    let envelope = r#"<?xml version="1.0" encoding="utf-8"?><soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"><soap:Body><Add xmlns="http://tempuri.org/"><intA>2</intA><intB>3</intB></Add></soap:Body></soap:Envelope>"#;
    let r = http
        .post(format!("{data}/calculator/calculator.asmx"))
        .header("content-type", "text/xml; charset=utf-8")
        .header("SOAPAction", "\"http://tempuri.org/Add\"")
        .body(envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.headers()["x-mockingbird-chaos"], "error");
    let text = r.text().await.unwrap();
    assert!(
        text.contains("<faultcode>soap:Server</faultcode>"),
        "{text}"
    );
    app.shutdown().await;
}

#[tokio::test]
async fn forced_status_header_wins_over_chaos() {
    let dir = tempfile::tempdir().unwrap();
    let app = things_app(dir.path(), &things("things", ALWAYS_ERROR)).await;
    let data = serve(app.data.clone()).await;

    let r = reqwest::Client::new()
        .get(format!("{data}/things/things/t1"))
        .header("X-Mockingbird-Status", "404")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert!(r.headers().get("x-mockingbird-chaos").is_none());
    let body: Value = r.json().await.unwrap();
    assert!(body["missing"].is_boolean(), "{body}");
}

#[tokio::test]
async fn header_off_bypasses_only_when_overrides_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let mocks = things("open", ALWAYS_ERROR)
        + &things(
            "locked",
            &format!("    header_overrides: false\n{ALWAYS_ERROR}"),
        );
    let app = things_app(dir.path(), &mocks).await;
    let data = serve(app.data.clone()).await;
    let http = reqwest::Client::new();
    let get = |mock: &str, value: &str| {
        http.get(format!("{data}/{mock}/things/t1"))
            .header("X-Mockingbird-Chaos", value)
            .send()
    };

    let r = get("open", "off").await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("x-mockingbird-chaos").is_none());

    // With header overrides off, the header is ignored.
    let r = get("locked", "off").await.unwrap();
    assert_eq!(r.status(), 503);
    let r = get("locked", "on").await.unwrap();
    assert_eq!(r.status(), 503);

    // Any other value is an override problem.
    let r = get("open", "on").await.unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["title"], "Invalid override", "{body}");
    assert_eq!(
        body["detail"], r#"X-Mockingbird-Chaos must be "off""#,
        "{body}"
    );
}

#[tokio::test]
async fn scenario_chaos_replaces_mock_chaos() {
    let dir = tempfile::tempdir().unwrap();
    let extra =
        format!("{ALWAYS_ERROR}    scenarios:\n      calm:\n        chaos: {{error_rate: 0}}\n");
    let app = things_app(dir.path(), &things("things", &extra)).await;
    let data = serve(app.data.clone()).await;
    let http = reqwest::Client::new();

    for _ in 0..20 {
        let r = http
            .get(format!("{data}/things/things/t1"))
            .header("X-Mockingbird-Scenario", "calm")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    let r = http
        .get(format!("{data}/things/things/t1"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
}

#[tokio::test]
async fn latency_jitter_applied() {
    let dir = tempfile::tempdir().unwrap();
    let extra = "    chaos: {latency_jitter: {min: 300ms, max: 400ms}}\n";
    let app = things_app(dir.path(), &things("things", extra)).await;
    let data = serve(app.data.clone()).await;
    let http = reqwest::Client::new();

    let started = Instant::now();
    let r = http
        .get(format!("{data}/things/things/t1"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(started.elapsed() >= Duration::from_millis(300));

    // Bypassed: no added latency.
    let started = Instant::now();
    let r = http
        .get(format!("{data}/things/things/t1"))
        .header("X-Mockingbird-Chaos", "off")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(started.elapsed() < Duration::from_millis(300));
}

fn events_config(chaos: &str) -> String {
    format!(
        "protocols:\n  sse: {{keepalive: 1s}}\nmocks:\n  events:\n    contract: {}\n    chaos: {chaos}\n",
        fixture("openapi/events-3.1.yaml").display()
    )
}

/// The SSE body of `GET /events/ticks` asking for 5 items: its text and
/// whether it ended with an error (an aborted connection).
async fn read_ticks(data: &str) -> (String, bool) {
    let mut resp = reqwest::Client::new()
        .get(format!("{data}/events/ticks"))
        .header("X-Mockingbird-Stream-Items", "5")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut text = String::new();
    let aborted = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => text.push_str(std::str::from_utf8(&chunk).unwrap()),
                Ok(None) => return false,
                Err(_) => return true,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the stream was not dropped: {text:?}"));
    (text, aborted)
}

#[tokio::test]
async fn sse_stream_dropped_early() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), &events_config("{drop_stream_rate: 1}")).await;
    let data = serve(app.data.clone()).await;

    let (text, aborted) = read_ticks(&data).await;
    assert!(aborted, "{text:?}");
    assert!(text.matches("event: tick").count() < 5, "{text}");
    assert!(!text.contains("event: error"), "{text}");
    app.shutdown().await;
}

#[tokio::test]
async fn grpc_server_stream_dropped_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "mocks:\n  route-guide:\n    contract: {}\n    chaos: {{drop_stream_rate: 1}}\n",
        fixture("proto/route_guide.proto").display()
    );
    let app = app(dir.path(), &config).await;
    let grpc_addr = serve(app.grpc.clone()).await;
    let mut grpc = Grpc::connect(&grpc_addr, "proto/route_guide.proto").await;
    let rect = grpc.message("routeguide.Rectangle");
    let feature = grpc.message("routeguide.Feature");

    let request = DynamicMessage::deserialize(rect.clone(), json!({})).unwrap();
    let mut request = tonic::Request::new(request);
    request
        .metadata_mut()
        .insert("x-mockingbird-stream-items", "5".parse().unwrap());
    grpc.client.ready().await.unwrap();
    let mut stream = grpc
        .client
        .server_streaming(
            request,
            PathAndQuery::from_static("/routeguide.RouteGuide/ListFeatures"),
            DynamicCodec::new(rect, feature),
        )
        .await
        .unwrap()
        .into_inner();
    let mut received = 0;
    let status = loop {
        match stream.message().await {
            Ok(Some(_)) => received += 1,
            Ok(None) => panic!("the stream ended without a drop after {received} messages"),
            Err(status) => break status,
        }
    };
    assert!(received < 5, "{received}");
    assert_eq!(status.code(), tonic::Code::Unavailable, "{status:?}");
    assert_eq!(status.message(), "stream dropped by chaos");
    app.shutdown().await;
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The next text message as JSON.
async fn ws_next(ws: &mut Ws) -> Value {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out")
            .unwrap()
            .unwrap();
        if let Message::Text(t) = frame {
            return serde_json::from_str(t.as_str()).unwrap();
        }
    }
}

#[tokio::test]
async fn graphql_subscription_dropped_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "mocks:\n  github:\n    contract: {}\n    chaos: {{drop_stream_rate: 1}}\n",
        fixture("graphql/github.graphql").display()
    );
    let app = app(dir.path(), &config).await;
    let data = serve(app.data.clone()).await;

    let mut req = format!("{}/github/graphql", data.replacen("http://", "ws://", 1))
        .into_client_request()
        .unwrap();
    let headers = req.headers_mut();
    headers.insert(
        "sec-websocket-protocol",
        "graphql-transport-ws".parse().unwrap(),
    );
    headers.insert("x-mockingbird-stream-items", "5".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let send = |msg: Value| Message::text(msg.to_string());
    ws.send(send(json!({"type": "connection_init"})))
        .await
        .unwrap();
    assert_eq!(ws_next(&mut ws).await["type"], "connection_ack");
    ws.send(send(json!({
        "type": "subscribe",
        "id": "s1",
        "payload": {"query": "subscription { issueAdded(repositoryId: \"r1\") { id } }"},
    })))
    .await
    .unwrap();

    let mut items = 0;
    let error = loop {
        let msg = ws_next(&mut ws).await;
        match msg["type"].as_str().unwrap() {
            "next" => items += 1,
            "error" => break msg,
            other => panic!("unexpected {other}: {msg}"),
        }
    };
    assert!(items < 5, "{items}");
    assert_eq!(error["id"], "s1", "{error}");
    assert_eq!(
        error["payload"][0]["extensions"]["code"], "UNAVAILABLE",
        "{error}"
    );
    // No `complete` follows the error.
    let after = tokio::time::timeout(Duration::from_millis(300), ws.next()).await;
    assert!(after.is_err(), "{after:?}");
    app.shutdown().await;
}

#[tokio::test]
async fn chaos_metric_counts_kinds() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("things.yaml"), THINGS).unwrap();
    let config = format!(
        "{}  metered:\n    contract: things.yaml\n    chaos: {{error_rate: 1, latency_jitter: {{min: 1ms, max: 2ms}}}}\n",
        events_config("{drop_stream_rate: 1}").replace("  events:", "  metered-events:")
    );
    let app = app(dir.path(), &config).await;
    let data = serve(app.data.clone()).await;
    let admin = serve(app.admin.clone()).await;

    let r = reqwest::get(format!("{data}/metered/things/t1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    let mut resp = reqwest::get(format!("{data}/metered-events/ticks"))
        .await
        .unwrap();
    while let Ok(Some(_)) = resp.chunk().await {}

    let metrics = reqwest::get(format!("{admin}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let counted = |mock: &str, kind: &str| {
        metrics.lines().any(|l| {
            l.starts_with("mb_chaos_injected_total{")
                && l.contains(&format!("mock=\"{mock}\""))
                && l.contains(&format!("kind=\"{kind}\""))
        })
    };
    assert!(counted("metered", "error"), "{metrics}");
    assert!(counted("metered", "latency"), "{metrics}");
    assert!(counted("metered-events", "drop"), "{metrics}");
    app.shutdown().await;
}
