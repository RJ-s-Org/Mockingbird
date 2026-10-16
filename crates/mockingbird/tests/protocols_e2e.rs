#![allow(clippy::unwrap_used)]

//! One real call per protocol family through a built app: GraphQL, gRPC
//! (on its own h2c listener), SOAP 1.1 and SSE.

use std::path::PathBuf;
use std::time::Duration;

use mb_mcm::ProtocolPayload;
use mb_protocols::grpc::DynamicCodec;
use mockingbird::config::{ServeArgs, build_app};
use prost_reflect::{DescriptorPool, DynamicMessage, ReflectMessage};
use serde_json::{Value, json};
use tonic::codegen::http::uri::PathAndQuery;

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel)
}

async fn serve(app: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn config() -> String {
    format!(
        r#"
protocols:
  sse: {{keepalive: 300ms}}
mocks:
  events:
    contract: {events}
    scenarios:
      pair:
        stream_items: 2
  github:
    contract: {github}
  route-guide:
    contract: {route_guide}
  calculator:
    contract: {calculator}
"#,
        events = fixture("openapi/events-3.1.yaml").display(),
        github = fixture("graphql/github.graphql").display(),
        route_guide = fixture("proto/route_guide.proto").display(),
        calculator = fixture("wsdl/calculator.wsdl").display(),
    )
}

#[tokio::test]
async fn protocols_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mockingbird.yaml");
    std::fs::write(&path, config()).unwrap();
    let args = ServeArgs {
        config: Some(path),
        offline: true,
        ..ServeArgs::default()
    };
    let app = build_app(&args, None).await.unwrap();
    let data = format!("http://{}", serve(app.data.clone()).await);
    let grpc_addr = serve(app.grpc.clone()).await;
    let http = reqwest::Client::new();

    // GraphQL.
    let r = http
        .post(format!("{data}/github/graphql"))
        .json(&json!({"query": "{ viewer { login } }"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["data"]["viewer"]["login"].is_string(), "{body}");

    // gRPC unary GetFeature.
    let bundle =
        mb_import::compile(&std::fs::read(fixture("proto/route_guide.proto")).unwrap()).unwrap();
    let Some(ProtocolPayload::Grpc {
        file_descriptor_set,
    }) = &bundle.protocol_payload
    else {
        panic!("no descriptors")
    };
    let pool = DescriptorPool::decode(file_descriptor_set.as_slice()).unwrap();
    let point = pool.get_message_by_name("routeguide.Point").unwrap();
    let feature = pool.get_message_by_name("routeguide.Feature").unwrap();
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{grpc_addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.unwrap();
    let request =
        DynamicMessage::deserialize(point.clone(), json!({"latitude": 1, "longitude": 2})).unwrap();
    let resp = grpc
        .unary(
            tonic::Request::new(request),
            PathAndQuery::from_static("/routeguide.RouteGuide/GetFeature"),
            DynamicCodec::new(point, feature),
        )
        .await
        .unwrap();
    let reply = resp.into_inner();
    assert_eq!(reply.descriptor().full_name(), "routeguide.Feature");
    assert!(serde_json::to_value(&reply).unwrap().is_object());

    // SOAP 1.1 Add.
    let envelope = r#"<?xml version="1.0" encoding="utf-8"?><soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"><soap:Body><Add xmlns="http://tempuri.org/"><intA>2</intA><intB>3</intB></Add></soap:Body></soap:Envelope>"#;
    let r = http
        .post(format!("{data}/calculator/calculator.asmx"))
        .header("content-type", "text/xml; charset=utf-8")
        .header("SOAPAction", "\"http://tempuri.org/Add\"")
        .body(envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(text.contains("AddResponse"), "{text}");
    assert!(text.contains("AddResult"), "{text}");

    // SSE: the scenario asks for 2 events.
    let mut resp = http
        .get(format!("{data}/events/ticks"))
        .header("X-Mockingbird-Scenario", "pair")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !text.contains("keepalive") {
            let chunk = resp.chunk().await.unwrap().unwrap();
            text.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no keepalive in {text:?}"));
    assert_eq!(text.matches("event: tick").count(), 2, "{text}");

    app.shutdown().await;
}
