#![cfg(test)]
#![cfg(feature = "grpc")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{LlmError, Provider, Role, ScriptedProvider};
use mb_mcm::Bundle;
use mb_protocols::grpc::{DynamicCodec, GrpcError, GrpcMock, GrpcServer, grpc_code_for_status};
use mb_protocols::{MockRuntime, RequestRecorder};
use mb_state::MemoryStore;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, ReflectMessage};
use serde_json::{Value, json};
use tonic::Code;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::transport::Channel;

use crate::support::{Records, wait_until};

fn route_guide() -> Arc<Bundle> {
    let path = format!(
        "{}/../../fixtures/proto/route_guide.proto",
        env!("CARGO_MANIFEST_DIR")
    );
    Arc::new(compile(&std::fs::read(path).unwrap()).unwrap())
}

const BLOBS: &str = r#"
syntax = "proto3";
package blobs;
service Blobs {
  rpc Put(Blob) returns (Ack) {}
}
message Blob { bytes data = 1; }
message Ack { string id = 1; }
"#;

fn runtime(
    name: &str,
    bundle: Arc<Bundle>,
    provider: Option<Arc<ScriptedProvider>>,
) -> MockRuntime {
    let provider = provider.map(|p| p as Arc<dyn Provider>);
    MockRuntime::new(
        name,
        bundle,
        Arc::new(Engine::new(provider, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    )
}

fn pool(bundle: &Bundle) -> DescriptorPool {
    let Some(mb_mcm::ProtocolPayload::Grpc {
        file_descriptor_set,
    }) = &bundle.protocol_payload
    else {
        panic!("no descriptors")
    };
    DescriptorPool::decode(file_descriptor_set.as_slice()).unwrap()
}

/// Publishes `rts` on one server served on an ephemeral port.
async fn serve(rts: Vec<MockRuntime>) -> (Arc<GrpcServer>, Channel) {
    let server = GrpcServer::new();
    let mocks = rts
        .into_iter()
        .map(|rt| Arc::new(GrpcMock::new(Arc::new(rt)).unwrap()))
        .collect();
    server.publish(mocks).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    (server, channel)
}

struct Client {
    grpc: tonic::client::Grpc<Channel>,
    pool: DescriptorPool,
}

impl Client {
    fn new(channel: Channel, bundle: &Bundle) -> Self {
        Self {
            grpc: tonic::client::Grpc::new(channel),
            pool: pool(bundle),
        }
    }

    fn desc(&self, name: &str) -> MessageDescriptor {
        self.pool.get_message_by_name(name).unwrap()
    }

    fn msg(&self, name: &str, value: Value) -> DynamicMessage {
        DynamicMessage::deserialize(self.desc(name), value).unwrap()
    }

    fn codec(&self, input: &str, output: &str) -> DynamicCodec {
        DynamicCodec::new(self.desc(input), self.desc(output))
    }

    async fn ready(&mut self) {
        self.grpc.ready().await.unwrap();
    }
}

fn request<T>(message: T, metadata: &[(&'static str, &str)]) -> tonic::Request<T> {
    let mut req = tonic::Request::new(message);
    for (k, v) in metadata {
        req.metadata_mut()
            .insert(*k, MetadataValue::try_from(*v).unwrap());
    }
    req
}

fn source(metadata: &MetadataMap) -> &str {
    metadata
        .get("x-mockingbird-source")
        .unwrap()
        .to_str()
        .unwrap()
}

fn json(msg: &DynamicMessage) -> Value {
    serde_json::to_value(msg).unwrap()
}

#[test]
fn grpc_code_table() {
    let cases = [
        (400, Code::InvalidArgument),
        (401, Code::Unauthenticated),
        (403, Code::PermissionDenied),
        (404, Code::NotFound),
        (409, Code::AlreadyExists),
        (412, Code::FailedPrecondition),
        (416, Code::OutOfRange),
        (429, Code::ResourceExhausted),
        (499, Code::Cancelled),
        (501, Code::Unimplemented),
        (503, Code::Unavailable),
        (504, Code::DeadlineExceeded),
        (402, Code::InvalidArgument),
        (418, Code::InvalidArgument),
        (500, Code::Internal),
        (502, Code::Internal),
        (302, Code::Internal),
        (200, Code::Internal),
    ];
    for (status, code) in cases {
        assert_eq!(grpc_code_for_status(status), code, "status {status}");
    }
}

#[tokio::test]
async fn unary_get_feature() {
    let bundle = route_guide();
    let records = Arc::new(Records::default());
    let mut rt = runtime("routes", Arc::clone(&bundle), None);
    rt.recorder = Some(Arc::clone(&records) as Arc<dyn RequestRecorder>);
    let (_server, channel) = serve(vec![rt]).await;
    let mut client = Client::new(channel, &bundle);
    let path = PathAndQuery::from_static("/routeguide.RouteGuide/GetFeature");

    client.ready().await;
    let point = client.msg("routeguide.Point", json!({"latitude": 1, "longitude": 2}));
    let codec = client.codec("routeguide.Point", "routeguide.Feature");
    let resp = client
        .grpc
        .unary(request(point.clone(), &[]), path.clone(), codec)
        .await
        .unwrap();
    assert_eq!(source(resp.metadata()), "fallback");
    let feature = resp.into_inner();
    assert_eq!(feature.descriptor().full_name(), "routeguide.Feature");
    assert!(json(&feature).is_object());

    // The same call read as a stream exposes the trailers on their own.
    client.ready().await;
    let codec = client.codec("routeguide.Point", "routeguide.Feature");
    let mut stream = client
        .grpc
        .server_streaming(request(point, &[]), path, codec)
        .await
        .unwrap()
        .into_inner();
    assert!(stream.message().await.unwrap().is_some());
    assert!(stream.message().await.unwrap().is_none());
    let trailers = stream.trailers().await.unwrap().unwrap();
    assert_eq!(source(&trailers), "fallback");

    let records = records.0.lock().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].method, "POST");
    assert_eq!(records[0].path, "/routeguide.RouteGuide/GetFeature");
    assert_eq!(
        records[0].operation_id.as_deref(),
        Some("RouteGuide.GetFeature")
    );
    assert_eq!(records[0].status, 200);
    assert_eq!(records[0].source.as_deref(), Some("fallback"));
    assert!(records[0].response.as_ref().unwrap().is_object());
}

/// A gzip client gets its compressed request decoded and a gzip response
/// back (MB-PROTOCOLS-STUB-4).
#[tokio::test]
async fn gzip_compressed_calls_are_served() {
    use tonic::codec::CompressionEncoding;
    let bundle = route_guide();
    let (_server, channel) = serve(vec![runtime("routes", Arc::clone(&bundle), None)]).await;
    let mut client = Client::new(channel, &bundle);
    client.grpc = client
        .grpc
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip);
    let path = PathAndQuery::from_static("/routeguide.RouteGuide/GetFeature");

    client.ready().await;
    let point = client.msg("routeguide.Point", json!({"latitude": 1, "longitude": 2}));
    let codec = client.codec("routeguide.Point", "routeguide.Feature");
    let resp = client
        .grpc
        .unary(request(point, &[]), path, codec)
        .await
        .unwrap();
    assert_eq!(
        resp.metadata()
            .get("grpc-encoding")
            .and_then(|v| v.to_str().ok()),
        Some("gzip")
    );
    assert_eq!(
        resp.into_inner().descriptor().full_name(),
        "routeguide.Feature"
    );
}

#[tokio::test]
async fn server_streaming_list_features_yields_n() {
    let bundle = route_guide();
    let (_server, channel) = serve(vec![runtime("routes", Arc::clone(&bundle), None)]).await;
    let mut client = Client::new(channel, &bundle);
    client.ready().await;
    let rect = client.msg(
        "routeguide.Rectangle",
        json!({"lo": {"latitude": 1, "longitude": 2}, "hi": {"latitude": 3, "longitude": 4}}),
    );
    let codec = client.codec("routeguide.Rectangle", "routeguide.Feature");
    let mut stream = client
        .grpc
        .server_streaming(
            request(rect, &[("x-mockingbird-stream-items", "4")]),
            PathAndQuery::from_static("/routeguide.RouteGuide/ListFeatures"),
            codec,
        )
        .await
        .unwrap()
        .into_inner();
    let mut n = 0;
    while let Some(feature) = stream.message().await.unwrap() {
        assert_eq!(feature.descriptor().full_name(), "routeguide.Feature");
        n += 1;
    }
    assert_eq!(n, 4);
    let trailers = stream.trailers().await.unwrap().unwrap();
    assert_eq!(source(&trailers), "fallback");
}

#[tokio::test]
async fn client_streaming_record_route() {
    let bundle = route_guide();
    let summary = json!({"pointCount": 3, "featureCount": 1, "distance": 10, "elapsedTime": 5});
    let provider = Arc::new(
        ScriptedProvider::new(vec![]).with_streams(vec![Ok(vec![Ok(format!("{summary}\n"))])]),
    );
    let rt = runtime("routes", Arc::clone(&bundle), Some(Arc::clone(&provider)));
    let (_server, channel) = serve(vec![rt]).await;
    let mut client = Client::new(channel, &bundle);
    client.ready().await;
    let points: Vec<DynamicMessage> = (0..3)
        .map(|i| {
            client.msg(
                "routeguide.Point",
                json!({"latitude": i, "longitude": i * 10}),
            )
        })
        .collect();
    let codec = client.codec("routeguide.Point", "routeguide.RouteSummary");
    let resp = client
        .grpc
        .client_streaming(
            request(futures::stream::iter(points), &[]),
            PathAndQuery::from_static("/routeguide.RouteGuide/RecordRoute"),
            codec,
        )
        .await
        .unwrap();
    assert_eq!(source(resp.metadata()), "llm");
    assert_eq!(json(&resp.into_inner()), summary);

    let requests = provider.stream_requests();
    assert_eq!(requests.len(), 1);
    let user = requests[0]
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .unwrap();
    let start = user.content.find("<request_stream>\n").unwrap() + "<request_stream>\n".len();
    let end = user.content.find("</request_stream>").unwrap();
    let items: Vec<&str> = user.content[start..end].lines().collect();
    assert_eq!(items.len(), 3, "{}", user.content);
    assert!(items[2].contains("20"), "{}", items[2]);
}

/// A selected scenario (here its per-operation variant) shapes unary and
/// streamed answers: the plan a call keeps finds it again in the runtime.
#[tokio::test]
async fn scenario_applies_to_unary_and_streamed_calls() {
    let bundle = route_guide();
    let list_features = bundle
        .operations
        .iter()
        .find(|o| o.id.ends_with("ListFeatures"))
        .unwrap()
        .id
        .clone();
    let mut rt = runtime("routes", Arc::clone(&bundle), None);
    let scenario = mb_protocols::ScenarioRuntime {
        spec: mb_engine::ScenarioSpec {
            name: "named".into(),
            set: Some(json!({"name": "from-scenario"})),
            ..Default::default()
        },
        stream_items: Some(2),
        ..Default::default()
    }
    .with_operation_instructions([(list_features, "list two".to_string())].into());
    rt.scenarios.insert("named".into(), scenario);
    let (_server, channel) = serve(vec![rt]).await;
    let mut client = Client::new(channel, &bundle);
    let scenario = [("x-mockingbird-scenario", "named")];

    client.ready().await;
    let point = client.msg("routeguide.Point", json!({"latitude": 1, "longitude": 2}));
    let feature = client
        .grpc
        .unary(
            request(point, &scenario),
            PathAndQuery::from_static("/routeguide.RouteGuide/GetFeature"),
            client.codec("routeguide.Point", "routeguide.Feature"),
        )
        .await
        .unwrap()
        .into_inner();
    assert_eq!(json(&feature)["name"], "from-scenario");

    client.ready().await;
    let rect = client.msg(
        "routeguide.Rectangle",
        json!({"lo": {"latitude": 1, "longitude": 2}, "hi": {"latitude": 3, "longitude": 4}}),
    );
    let mut stream = client
        .grpc
        .server_streaming(
            request(rect, &scenario),
            PathAndQuery::from_static("/routeguide.RouteGuide/ListFeatures"),
            client.codec("routeguide.Rectangle", "routeguide.Feature"),
        )
        .await
        .unwrap()
        .into_inner();
    let mut names = Vec::new();
    while let Some(feature) = stream.message().await.unwrap() {
        names.push(json(&feature)["name"].clone());
    }
    assert_eq!(names, vec![json!("from-scenario"); 2]);
}

#[tokio::test]
async fn bidi_route_chat_replies_per_message() {
    let bundle = route_guide();
    let reply = |i: usize| json!({"location": {"latitude": i + 1, "longitude": 5}, "message": format!("reply-{i}")});
    let streams = (0..3)
        .map(|i| Ok(vec![Ok::<_, LlmError>(format!("{}\n", reply(i)))]))
        .collect();
    let provider = Arc::new(ScriptedProvider::new(vec![]).with_streams(streams));
    let rt = runtime("routes", Arc::clone(&bundle), Some(Arc::clone(&provider)));
    let (_server, channel) = serve(vec![rt]).await;
    let mut client = Client::new(channel, &bundle);
    client.ready().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<DynamicMessage>(4);
    let codec = client.codec("routeguide.RouteNote", "routeguide.RouteNote");
    let mut replies = client
        .grpc
        .streaming(
            request(tokio_stream::wrappers::ReceiverStream::new(rx), &[]),
            PathAndQuery::from_static("/routeguide.RouteGuide/RouteChat"),
            codec,
        )
        .await
        .unwrap()
        .into_inner();
    for i in 0..3 {
        let note = client.msg(
            "routeguide.RouteNote",
            json!({"location": {"latitude": i, "longitude": 1}, "message": format!("note-{i}")}),
        );
        tx.send(note).await.unwrap();
        // Each reply arrives before the next message is sent.
        let got = replies.message().await.unwrap().unwrap();
        assert_eq!(json(&got), reply(i as usize));
    }
    drop(tx);
    assert!(replies.message().await.unwrap().is_none());
    let trailers = replies.trailers().await.unwrap().unwrap();
    assert_eq!(source(&trailers), "llm");

    let requests = provider.stream_requests();
    assert_eq!(requests.len(), 3);
    let user = |i: usize| {
        requests[i]
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .unwrap()
            .content
            .clone()
    };
    assert!(user(0).contains("note-0"));
    assert!(!user(0).contains("<request_stream>"));
    let second = user(1);
    assert!(second.contains("<request_stream>\n"), "{second}");
    let start = second.find("<request_stream>\n").unwrap();
    let block = &second[start..second.find("</request_stream>").unwrap()];
    assert!(
        block.contains("note-0") && !block.contains("note-1"),
        "{block}"
    );
}

#[tokio::test]
async fn forced_status_maps_code() {
    let bundle = route_guide();
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let records = Arc::new(Records::default());
    let mut rt = runtime("routes", Arc::clone(&bundle), Some(Arc::clone(&provider)));
    rt.recorder = Some(Arc::clone(&records) as Arc<dyn RequestRecorder>);
    let (_server, channel) = serve(vec![rt]).await;
    let mut client = Client::new(channel, &bundle);
    client.ready().await;
    let point = client.msg("routeguide.Point", json!({"latitude": 1, "longitude": 2}));
    let codec = client.codec("routeguide.Point", "routeguide.Feature");
    let err = client
        .grpc
        .unary(
            request(point, &[("x-mockingbird-status", "404")]),
            PathAndQuery::from_static("/routeguide.RouteGuide/GetFeature"),
            codec,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    assert_eq!(err.message(), "forced by x-mockingbird-status: 404");
    assert!(provider.requests().is_empty());
    assert!(provider.stream_requests().is_empty());
    {
        let records = records.0.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, 404);
        assert_eq!(records[0].source, None);
    }

    // A bad override is INVALID_ARGUMENT.
    client.ready().await;
    let point = client.msg("routeguide.Point", json!({}));
    let codec = client.codec("routeguide.Point", "routeguide.Feature");
    let err = client
        .grpc
        .unary(
            request(point, &[("x-mockingbird-scenario", "nope")]),
            PathAndQuery::from_static("/routeguide.RouteGuide/GetFeature"),
            codec,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn unknown_service_is_unimplemented() {
    let bundle = route_guide();
    let (_server, channel) = serve(vec![runtime("routes", Arc::clone(&bundle), None)]).await;
    let mut client = Client::new(channel, &bundle);
    for path in ["/nope.Missing/Call", "/routeguide.RouteGuide/Missing"] {
        client.ready().await;
        let point = client.msg("routeguide.Point", json!({}));
        let codec = client.codec("routeguide.Point", "routeguide.Feature");
        let err = client
            .grpc
            .unary(
                request(point, &[]),
                PathAndQuery::from_maybe_shared(path).unwrap(),
                codec,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::Unimplemented, "{path}");
    }
}

#[tokio::test]
async fn non_grpc_content_type_is_415() {
    let bundle = route_guide();
    let server = GrpcServer::new();
    let mock = GrpcMock::new(Arc::new(runtime("routes", bundle, None))).unwrap();
    server.publish(vec![Arc::new(mock)]).await.unwrap();
    let resp = tower::ServiceExt::oneshot(
        server.router(),
        axum::http::Request::post("/routeguide.RouteGuide/GetFeature")
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 415);
}

#[tokio::test]
async fn oversized_message_rejected() {
    let bundle = Arc::new(compile(BLOBS.as_bytes()).unwrap());
    let (_server, channel) = serve(vec![runtime("blobs", Arc::clone(&bundle), None)]).await;
    let mut client = Client::new(channel, &bundle);
    client.ready().await;
    let mut blob = DynamicMessage::new(client.desc("blobs.Blob"));
    blob.set_field_by_name(
        "data",
        prost_reflect::Value::Bytes(vec![7u8; 5 * 1024 * 1024].into()),
    );
    let codec = client.codec("blobs.Blob", "blobs.Ack");
    let err = client
        .grpc
        .unary(
            request(blob, &[]),
            PathAndQuery::from_static("/blobs.Blobs/Put"),
            codec,
        )
        .await
        .unwrap_err();
    // Over the request size cap (the 1 MiB default here): gRPC servers
    // answer RESOURCE_EXHAUSTED (tonic itself says OUT_OF_RANGE).
    assert_eq!(err.code(), Code::ResourceExhausted, "{err:?}");
}

#[tokio::test]
async fn reflection_lists_services() {
    use tonic_reflection::pb::{v1, v1alpha};

    let bundle = route_guide();
    let (_server, channel) = serve(vec![runtime("routes", Arc::clone(&bundle), None)]).await;
    let expected = ["grpc.health.v1.Health", "routeguide.RouteGuide"];

    let mut v1_client = v1::server_reflection_client::ServerReflectionClient::new(channel.clone());
    let req = v1::ServerReflectionRequest {
        host: String::new(),
        message_request: Some(v1::server_reflection_request::MessageRequest::ListServices(
            String::new(),
        )),
    };
    let mut resp = v1_client
        .server_reflection_info(futures::stream::iter([req]))
        .await
        .unwrap()
        .into_inner();
    let Some(v1::server_reflection_response::MessageResponse::ListServicesResponse(list)) =
        resp.next().await.unwrap().unwrap().message_response
    else {
        panic!("not a service list")
    };
    let names: Vec<String> = list.service.into_iter().map(|s| s.name).collect();
    for name in expected {
        assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
    }

    let mut alpha = v1alpha::server_reflection_client::ServerReflectionClient::new(channel.clone());
    let req = v1alpha::ServerReflectionRequest {
        host: String::new(),
        message_request: Some(
            v1alpha::server_reflection_request::MessageRequest::ListServices(String::new()),
        ),
    };
    let mut resp = alpha
        .server_reflection_info(futures::stream::iter([req]))
        .await
        .unwrap()
        .into_inner();
    let Some(v1alpha::server_reflection_response::MessageResponse::ListServicesResponse(list)) =
        resp.next().await.unwrap().unwrap().message_response
    else {
        panic!("not a service list")
    };
    let names: Vec<String> = list.service.into_iter().map(|s| s.name).collect();
    for name in expected {
        assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
    }
}

/// Two single-file uploads are both named `main.proto` in their bundles;
/// reflection must still list and resolve both services.
#[tokio::test]
async fn reflection_resolves_mocks_whose_files_share_a_name() {
    use tonic_reflection::pb::v1;
    use v1::server_reflection_request::MessageRequest;
    use v1::server_reflection_response::MessageResponse;

    let hello = format!(
        "{}/../../fixtures/proto/helloworld.proto",
        env!("CARGO_MANIFEST_DIR")
    );
    let hello = Arc::new(compile(&std::fs::read(hello).unwrap()).unwrap());
    let (_server, channel) = serve(vec![
        runtime("hello", hello, None),
        runtime("routes", route_guide(), None),
    ])
    .await;
    let mut client = v1::server_reflection_client::ServerReflectionClient::new(channel);
    let requests = [
        MessageRequest::ListServices(String::new()),
        MessageRequest::FileContainingSymbol("helloworld.Greeter".into()),
        MessageRequest::FileContainingSymbol("routeguide.RouteGuide".into()),
    ];
    let mut answers = Vec::new();
    for request in requests {
        let req = v1::ServerReflectionRequest {
            host: String::new(),
            message_request: Some(request),
        };
        let mut resp = client
            .server_reflection_info(futures::stream::iter([req]))
            .await
            .unwrap()
            .into_inner();
        answers.push(
            resp.next()
                .await
                .unwrap()
                .unwrap()
                .message_response
                .unwrap(),
        );
    }
    let MessageResponse::ListServicesResponse(list) = &answers[0] else {
        panic!("not a service list: {:?}", answers[0])
    };
    let names: Vec<&str> = list.service.iter().map(|s| s.name.as_str()).collect();
    for name in ["helloworld.Greeter", "routeguide.RouteGuide"] {
        assert!(names.contains(&name), "{name} in {names:?}");
    }
    let mut files = Vec::new();
    for (answer, package) in answers[1..].iter().zip(["helloworld", "routeguide"]) {
        let MessageResponse::FileDescriptorResponse(fds) = answer else {
            panic!("{package}: {answer:?}")
        };
        let file = <prost_types::FileDescriptorProto as prost::Message>::decode(
            fds.file_descriptor_proto[0].as_slice(),
        )
        .unwrap();
        assert_eq!(file.package(), package);
        files.push(file.name().to_string());
    }
    assert_ne!(files[0], files[1], "{files:?}");
}

#[tokio::test]
async fn health_reports_serving() {
    use tonic_health::pb::health_check_response::ServingStatus;
    use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};

    let bundle = route_guide();
    let (server, channel) = serve(vec![runtime("routes", Arc::clone(&bundle), None)]).await;
    let mut health = HealthClient::new(channel);
    for service in ["", "routeguide.RouteGuide"] {
        let resp = health
            .check(HealthCheckRequest {
                service: service.into(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.status(), ServingStatus::Serving, "{service:?}");
    }
    let err = health
        .check(HealthCheckRequest {
            service: "nope.Missing".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);

    // Unpublished services stop serving.
    server.publish(Vec::new()).await.unwrap();
    let resp = health
        .check(HealthCheckRequest {
            service: "routeguide.RouteGuide".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.status(), ServingStatus::NotServing);
}

#[tokio::test]
async fn publish_rejects_duplicate_services() {
    let bundle = route_guide();
    let mock = |name: &str| {
        Arc::new(GrpcMock::new(Arc::new(runtime(name, Arc::clone(&bundle), None))).unwrap())
    };
    let a = mock("a");
    assert_eq!(a.name(), "a");
    assert_eq!(a.services(), vec!["routeguide.RouteGuide".to_string()]);
    assert!(!a.descriptor_set().is_empty());

    let server = GrpcServer::new();
    server.publish(vec![Arc::clone(&a)]).await.unwrap();
    let before = server.services();
    assert_eq!(
        before,
        vec![("routeguide.RouteGuide".to_string(), "a".to_string())]
    );
    match server.publish(vec![mock("b"), mock("c")]).await {
        Err(GrpcError::ServiceCollision { service, holder }) => {
            assert_eq!(service, "routeguide.RouteGuide");
            assert_eq!(holder, "b");
        }
        other => panic!("expected a collision, got {other:?}"),
    }
    assert_eq!(
        server.services(),
        before,
        "a failed publish changes nothing"
    );

    let mut no_payload = (*bundle).clone();
    no_payload.protocol_payload = None;
    assert!(matches!(
        GrpcMock::new(Arc::new(runtime("x", Arc::new(no_payload), None))),
        Err(GrpcError::MissingDescriptors)
    ));
}

/// Counts usage events.
#[derive(Default)]
struct Usage(Mutex<Vec<mb_ext::UsageEvent>>);

impl mb_ext::UsageSink for Usage {
    fn record(&self, event: &mb_ext::UsageEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

impl Usage {
    /// The events seen once there are `n` (waiting up to 5 s; reports of a
    /// finished stream are sent from a spawned task) and no more arrive in
    /// a further 50 ms, so a caller checking for exactly `n` sees extras.
    async fn settled(&self, n: usize) -> Vec<mb_ext::UsageEvent> {
        wait_until(Duration::from_secs(5), Duration::from_millis(2), || async {
            let events = self.0.lock().unwrap().clone();
            if events.len() >= n {
                Ok(())
            } else {
                Err(format!("wanted {n} usage events, got {events:?}"))
            }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.0.lock().unwrap().clone()
    }
}

/// Admits the first `allow` calls, then refuses with 403 `quota spent`;
/// counts every admission.
struct CountingQuota {
    allow: usize,
    admitted: std::sync::atomic::AtomicUsize,
}

impl CountingQuota {
    fn new(allow: usize) -> Arc<Self> {
        Arc::new(Self {
            allow,
            admitted: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn count(&self) -> usize {
        self.admitted.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl mb_ext::QuotaGuard for CountingQuota {
    async fn admit(
        &self,
        _t: &mb_ext::Tenancy,
        _call: &mb_ext::CallInfo<'_>,
    ) -> Result<(), mb_ext::Rejection> {
        let n = self
            .admitted
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n < self.allow {
            Ok(())
        } else {
            Err(mb_ext::Rejection::new(403, "Forbidden", "quota spent"))
        }
    }
}

/// A route-guide runtime metering into `usage` and admitting with `quota`.
fn metered(usage: &Arc<Usage>, quota: &Arc<CountingQuota>) -> (Arc<Bundle>, MockRuntime) {
    let bundle = route_guide();
    let mut rt = runtime("routes", Arc::clone(&bundle), None);
    rt.ext = std::mem::take(&mut rt.ext)
        .with_usage(Arc::clone(usage) as Arc<dyn mb_ext::UsageSink>)
        .with_quota(Arc::clone(quota) as Arc<dyn mb_ext::QuotaGuard>);
    (bundle, rt)
}

/// Opens a `RouteChat` stream; returns the sender and the replies.
async fn route_chat(
    client: &mut Client,
) -> (
    tokio::sync::mpsc::Sender<DynamicMessage>,
    tonic::Streaming<DynamicMessage>,
) {
    client.ready().await;
    let (tx, rx) = tokio::sync::mpsc::channel::<DynamicMessage>(4);
    let codec = client.codec("routeguide.RouteNote", "routeguide.RouteNote");
    let replies = client
        .grpc
        .streaming(
            request(tokio_stream::wrappers::ReceiverStream::new(rx), &[]),
            PathAndQuery::from_static("/routeguide.RouteGuide/RouteChat"),
            codec,
        )
        .await
        .unwrap()
        .into_inner();
    (tx, replies)
}

fn note(client: &Client, i: i32) -> DynamicMessage {
    client.msg(
        "routeguide.RouteNote",
        json!({"location": {"latitude": i, "longitude": 1}, "message": format!("note-{i}")}),
    )
}

#[tokio::test]
async fn bidi_stream_meters_each_item() {
    let usage = Arc::new(Usage::default());
    let quota = CountingQuota::new(usize::MAX);
    let (bundle, rt) = metered(&usage, &quota);
    let (_server, channel) = serve(vec![rt]).await;
    let mut client = Client::new(channel, &bundle);
    let (tx, mut replies) = route_chat(&mut client).await;
    for i in 0..4 {
        tx.send(note(&client, i)).await.unwrap();
        replies.message().await.unwrap().unwrap();
    }
    drop(tx);
    assert!(replies.message().await.unwrap().is_none());
    let events = usage.settled(4).await;
    assert_eq!(events.len(), 4, "{events:?}");
    for e in &events {
        assert_eq!(e.operation_id, "RouteGuide.RouteChat");
        assert_eq!(e.status, 200);
        assert_eq!(e.source, "fallback");
    }
    // One admission per inbound message.
    assert_eq!(quota.count(), 4);
}

#[tokio::test]
async fn bidi_stream_meters_each_items_tokens() {
    let bundle = route_guide();
    let reply = |i: usize| json!({"location": {"latitude": i + 1, "longitude": 5}, "message": format!("reply-{i}")});
    let streams = (0..3)
        .map(|i| Ok(vec![Ok::<_, LlmError>(format!("{}\n", reply(i)))]))
        .collect();
    let provider = Arc::new(ScriptedProvider::new(vec![]).with_streams(streams));
    let usage = Arc::new(Usage::default());
    let mut rt = runtime("routes", Arc::clone(&bundle), Some(provider));
    rt.ext =
        std::mem::take(&mut rt.ext).with_usage(Arc::clone(&usage) as Arc<dyn mb_ext::UsageSink>);
    let (_server, channel) = serve(vec![rt]).await;
    let mut client = Client::new(channel, &bundle);
    let (tx, mut replies) = route_chat(&mut client).await;
    for i in 0..3 {
        tx.send(note(&client, i)).await.unwrap();
        replies.message().await.unwrap().unwrap();
    }
    drop(tx);
    assert!(replies.message().await.unwrap().is_none());
    let events = usage.settled(3).await;
    assert_eq!(events.len(), 3, "{events:?}");
    for e in &events {
        assert_eq!(e.source, "llm", "{e:?}");
        // Each reply is its own one-item engine stream: its prompt and text.
        assert!(e.prompt_tokens > 0, "{e:?}");
        assert!(e.completion_tokens > 0, "{e:?}");
    }
}

#[tokio::test]
async fn client_stream_admits_each_item() {
    let usage = Arc::new(Usage::default());
    let quota = CountingQuota::new(usize::MAX);
    let (bundle, rt) = metered(&usage, &quota);
    let (_server, channel) = serve(vec![rt]).await;
    let mut client = Client::new(channel, &bundle);
    let points = |client: &Client| -> Vec<DynamicMessage> {
        (0..3)
            .map(|i| client.msg("routeguide.Point", json!({"latitude": i, "longitude": i})))
            .collect()
    };
    client.ready().await;
    let codec = client.codec("routeguide.Point", "routeguide.RouteSummary");
    client
        .grpc
        .client_streaming(
            request(futures::stream::iter(points(&client)), &[]),
            PathAndQuery::from_static("/routeguide.RouteGuide/RecordRoute"),
            codec,
        )
        .await
        .unwrap();
    assert_eq!(quota.count(), 3);
    // One engine call, one usage event.
    assert_eq!(usage.settled(1).await.len(), 1);

    // The mock's rate limit applies to each message too: a burst of 2
    // admits the call (its first message) and one more.
    let usage = Arc::new(Usage::default());
    let quota = CountingQuota::new(usize::MAX);
    let (bundle, mut rt) = metered(&usage, &quota);
    rt.limits.rate = Some(Arc::new(mb_protocols::RateLimiter::new(0.0, 2)));
    let (_server, channel) = serve(vec![rt]).await;
    let mut client = Client::new(channel, &bundle);
    client.ready().await;
    let codec = client.codec("routeguide.Point", "routeguide.RouteSummary");
    let err = client
        .grpc
        .client_streaming(
            request(futures::stream::iter(points(&client)), &[]),
            PathAndQuery::from_static("/routeguide.RouteGuide/RecordRoute"),
            codec,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted, "{err:?}");
    assert!(err.message().contains("requests/s"), "{err:?}");
}

#[tokio::test]
async fn bidi_quota_refusal_ends_stream_resource_exhausted() {
    let usage = Arc::new(Usage::default());
    let records = Arc::new(Records::default());
    // The call (its first message) and the second message are admitted.
    let quota = CountingQuota::new(2);
    let (bundle, mut rt) = metered(&usage, &quota);
    rt.recorder = Some(Arc::clone(&records) as Arc<dyn RequestRecorder>);
    let (_server, channel) = serve(vec![rt]).await;
    let mut client = Client::new(channel, &bundle);
    let (tx, mut replies) = route_chat(&mut client).await;
    for i in 0..2 {
        tx.send(note(&client, i)).await.unwrap();
        replies.message().await.unwrap().unwrap();
    }
    tx.send(note(&client, 2)).await.unwrap();
    let err = replies.message().await.unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted, "{err:?}");
    assert_eq!(err.message(), "quota spent");
    drop(tx);
    // The two items sent were metered; the refused one was not.
    let events = usage.settled(2).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(events.iter().all(|e| e.status == 200));
    let recs = records.0.lock().unwrap().clone();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].status, 403);
}
