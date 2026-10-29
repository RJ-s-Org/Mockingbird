//! Data-plane auth across the streaming and non-REST transports: SSE,
//! gRPC streaming and reflection, GraphQL over HTTP and over WebSocket,
//! and SOAP `?wsdl`. Every refusal happens before the engine runs, and the
//! key never reaches the prompt or the request record.
#![cfg(test)]
#![cfg(all(
    feature = "sse",
    feature = "grpc",
    feature = "graphql",
    feature = "soap"
))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::{SinkExt, StreamExt};
use mb_engine::{Engine, EngineConfig};
use mb_ext::{CallInfo, QuotaGuard, Rejection, Tenancy, UsageEvent, UsageSink};
use mb_import::compile;
use mb_llm::{ChatRequest, LlmError, Provider, ScriptedProvider};
use mb_mcm::Bundle;
use mb_protocols::grpc::{DynamicCodec, GrpcMock, GrpcServer};
use mb_protocols::{
    DataPlaneAuth, DataPlaneAuthMode, DataPlaneKeys, MockRuntime, RequestRecord, RequestRecorder,
    graphql, http_rest, soap,
};
use mb_state::MemoryStore;
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tonic::Code;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tower::ServiceExt;

// ---------------------------------------------------------------- helpers

fn fixture(rel: &str) -> Arc<Bundle> {
    let path = format!("{}/../../fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
    Arc::new(compile(&std::fs::read(path).unwrap()).unwrap())
}

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

/// Accepts exactly the listed `(workspace, key)` pairs and counts calls.
#[derive(Default)]
struct StubKeys {
    valid: Vec<(&'static str, &'static str)>,
    calls: AtomicUsize,
}

#[async_trait]
impl DataPlaneKeys for StubKeys {
    async fn verify(&self, workspace: &str, key: &str) -> bool {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.valid.iter().any(|(w, k)| *w == workspace && *k == key)
    }
}

fn keys(valid: &[(&'static str, &'static str)]) -> Arc<StubKeys> {
    Arc::new(StubKeys {
        valid: valid.to_vec(),
        calls: AtomicUsize::new(0),
    })
}

fn api_key(header: &str) -> DataPlaneAuth {
    DataPlaneAuth {
        mode: DataPlaneAuthMode::ApiKey,
        header: header.into(),
    }
}

/// `rt` in workspace `ws`, `api_key` mode with key header `header`.
fn secured(mut rt: MockRuntime, ws: &str, header: &str, keys: Arc<StubKeys>) -> MockRuntime {
    rt.workspace = ws.into();
    rt.set_data_plane_auth(api_key(header));
    rt.data_plane_keys = Some(keys);
    rt
}

#[derive(Default)]
struct Records(Mutex<Vec<RequestRecord>>);

impl RequestRecorder for Records {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

impl Records {
    /// Waits up to 2 s for at least `n` records and returns them all.
    async fn wait(&self, n: usize) -> Vec<RequestRecord> {
        for _ in 0..100 {
            let got = self.0.lock().unwrap().clone();
            if got.len() >= n {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.0.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct Usage(Mutex<Vec<UsageEvent>>);

impl UsageSink for Usage {
    fn record(&self, event: &UsageEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

/// Every message of every request (plain and streamed) as one string.
fn prompts(provider: &ScriptedProvider) -> Vec<String> {
    let all: Vec<ChatRequest> = provider
        .requests()
        .into_iter()
        .chain(provider.stream_requests())
        .collect();
    all.iter()
        .map(|r| r.messages.iter().map(|m| m.content.as_str()).collect())
        .collect()
}

fn assert_engine_not_called(provider: &ScriptedProvider, case: &str) {
    assert!(provider.requests().is_empty(), "{case}: engine was called");
    assert!(
        provider.stream_requests().is_empty(),
        "{case}: engine stream was opened"
    );
}

fn assert_no_secret(records: &[RequestRecord], secrets: &[&str]) {
    for r in records {
        let text = serde_json::to_string(&r.request).unwrap();
        for s in secrets {
            assert!(!text.contains(s), "record leaks {s:?}: {text}");
        }
    }
}

// -------------------------------------------------------------------- SSE

async fn serve_http(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr.to_string()
}

#[tokio::test]
async fn sse_requires_key_and_never_leaks_it() {
    let tick = json!({"seq": 1, "at": "2026-01-01T00:00:00Z"}).to_string();
    let provider = Arc::new(
        ScriptedProvider::new(vec![])
            .with_streams(vec![Ok(vec![Ok::<_, LlmError>(format!("{tick}\n"))])]),
    );
    let records = Arc::new(Records::default());
    let mut rt = secured(
        runtime(
            "events",
            fixture("openapi/events-3.1.yaml"),
            Some(provider.clone()),
        ),
        "team-a",
        "x-key",
        keys(&[("team-a", "sse-good")]),
    );
    rt.recorder = Some(records.clone());
    let addr = serve_http(http_rest::router(Arc::new(rt))).await;
    let client = reqwest::Client::new();

    for (case, url, header) in [
        ("no key", format!("http://{addr}/ticks"), None),
        ("bad key", format!("http://{addr}/ticks"), Some("nope")),
        (
            "bad query key",
            format!("http://{addr}/ticks?api_key=nope"),
            None,
        ),
    ] {
        let mut req = client.get(url);
        if let Some(k) = header {
            req = req.header("x-key", k);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{case}");
        assert_eq!(
            resp.headers()["www-authenticate"],
            r#"ApiKey realm="mockingbird", header="x-key""#,
            "{case}"
        );
    }
    assert_engine_not_called(&provider, "sse refused");

    let resp = client
        .get(format!("http://{addr}/ticks?api_key=sse-good"))
        .header("x-key", "sse-good")
        .header("x-mockingbird-stream-items", "1")
        .header("x-trace", "t1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    // The stream stays open for state changes: read the first event.
    let mut body = resp.bytes_stream();
    let mut text = String::new();
    while !text.contains("data:") {
        let chunk = tokio::time::timeout(Duration::from_secs(10), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        text.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    drop(body);

    assert_eq!(provider.stream_requests().len(), 1, "one engine stream");
    let prompts = prompts(&provider);
    for p in &prompts {
        assert!(!p.contains("sse-good"), "prompt leaks the key: {p}");
        assert!(!p.contains("api_key"), "prompt names api_key: {p}");
    }
    let records = records.wait(4).await;
    let statuses: Vec<u16> = records.iter().map(|r| r.status).collect();
    assert_eq!(statuses, [401, 401, 401, 200]);
    assert!(records.iter().all(|r| r.workspace == "team-a"));
    assert_eq!(records[3].request["headers"]["x-trace"], "t1");
    assert_no_secret(&records, &["sse-good", "nope"]);
}

// ------------------------------------------------------------------- gRPC

const BLOBS: &str = r#"
syntax = "proto3";
package blobs;
service Blobs {
  rpc Put(Blob) returns (Ack) {}
}
message Blob { bytes data = 1; }
message Ack { string id = 1; }
"#;

fn pool(bundle: &Bundle) -> DescriptorPool {
    let Some(mb_mcm::ProtocolPayload::Grpc {
        file_descriptor_set,
    }) = &bundle.protocol_payload
    else {
        panic!("no descriptors")
    };
    DescriptorPool::decode(file_descriptor_set.as_slice()).unwrap()
}

async fn serve_grpc(rts: Vec<MockRuntime>) -> Channel {
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
    tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

fn grpc_request<T>(message: T, metadata: &[(&'static str, &str)]) -> tonic::Request<T> {
    let mut req = tonic::Request::new(message);
    for (k, v) in metadata {
        req.metadata_mut()
            .insert(*k, MetadataValue::try_from(*v).unwrap());
    }
    req
}

struct RouteClient {
    grpc: tonic::client::Grpc<Channel>,
    pool: DescriptorPool,
}

impl RouteClient {
    fn msg(&self, name: &str, value: Value) -> DynamicMessage {
        DynamicMessage::deserialize(self.pool.get_message_by_name(name).unwrap(), value).unwrap()
    }

    fn codec(&self, input: &str, output: &str) -> DynamicCodec {
        DynamicCodec::new(
            self.pool.get_message_by_name(input).unwrap(),
            self.pool.get_message_by_name(output).unwrap(),
        )
    }

    /// ListFeatures (server streaming): the status code of the call and
    /// the number of messages read.
    async fn list_features(&mut self, metadata: &[(&'static str, &str)]) -> (Code, usize) {
        self.grpc.ready().await.unwrap();
        let rect = self.msg(
            "routeguide.Rectangle",
            json!({"lo": {"latitude": 1, "longitude": 2}, "hi": {"latitude": 3, "longitude": 4}}),
        );
        let codec = self.codec("routeguide.Rectangle", "routeguide.Feature");
        let resp = self
            .grpc
            .server_streaming(
                grpc_request(rect, metadata),
                PathAndQuery::from_static("/routeguide.RouteGuide/ListFeatures"),
                codec,
            )
            .await;
        let mut stream = match resp {
            Ok(r) => r.into_inner(),
            Err(status) => return (status.code(), 0),
        };
        let mut n = 0;
        loop {
            match stream.message().await {
                Ok(Some(_)) => n += 1,
                Ok(None) => return (Code::Ok, n),
                Err(status) => return (status.code(), n),
            }
        }
    }

    /// RecordRoute (client streaming): the status code.
    async fn record_route(&mut self, metadata: &[(&'static str, &str)]) -> Code {
        self.grpc.ready().await.unwrap();
        let points: Vec<DynamicMessage> = (0..2)
            .map(|i| self.msg("routeguide.Point", json!({"latitude": i, "longitude": i})))
            .collect();
        let codec = self.codec("routeguide.Point", "routeguide.RouteSummary");
        match self
            .grpc
            .client_streaming(
                grpc_request(futures::stream::iter(points), metadata),
                PathAndQuery::from_static("/routeguide.RouteGuide/RecordRoute"),
                codec,
            )
            .await
        {
            Ok(_) => Code::Ok,
            Err(status) => status.code(),
        }
    }

    /// RouteChat (bidi): sends one note and reads replies to the end;
    /// the status code.
    async fn route_chat(&mut self, metadata: &[(&'static str, &str)]) -> Code {
        self.grpc.ready().await.unwrap();
        let note = self.msg(
            "routeguide.RouteNote",
            json!({"location": {"latitude": 1, "longitude": 1}, "message": "note-0"}),
        );
        let codec = self.codec("routeguide.RouteNote", "routeguide.RouteNote");
        let resp = self
            .grpc
            .streaming(
                grpc_request(futures::stream::iter([note]), metadata),
                PathAndQuery::from_static("/routeguide.RouteGuide/RouteChat"),
                codec,
            )
            .await;
        let mut stream = match resp {
            Ok(r) => r.into_inner(),
            Err(status) => return status.code(),
        };
        loop {
            match stream.message().await {
                Ok(Some(_)) => {}
                Ok(None) => return Code::Ok,
                Err(status) => return status.code(),
            }
        }
    }
}

#[tokio::test]
async fn grpc_streaming_calls_require_key_and_never_leak_it() {
    let bundle = fixture("proto/route_guide.proto");
    let feature = json!({"name": "f", "location": {"latitude": 1, "longitude": 2}});
    let summary = json!({"pointCount": 2, "featureCount": 1, "distance": 10, "elapsedTime": 5});
    let note = json!({"location": {"latitude": 2, "longitude": 5}, "message": "reply"});
    let provider = Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![
        Ok(vec![Ok::<_, LlmError>(format!("{feature}\n"))]),
        Ok(vec![Ok(format!("{summary}\n"))]),
        Ok(vec![Ok(format!("{note}\n"))]),
    ]));
    let records = Arc::new(Records::default());
    let mut rt = secured(
        runtime("routes", Arc::clone(&bundle), Some(provider.clone())),
        "team-a",
        "x-mb-key",
        keys(&[("team-a", "grpc-good")]),
    );
    rt.recorder = Some(records.clone());
    let channel = serve_grpc(vec![rt]).await;
    let mut client = RouteClient {
        grpc: tonic::client::Grpc::new(channel),
        pool: pool(&bundle),
    };

    for (case, md) in [
        ("no key", vec![]),
        ("bad key", vec![("x-mb-key", "nope")]),
        ("key in another header", vec![("x-api-key", "grpc-good")]),
    ] {
        assert_eq!(
            client.list_features(&md).await,
            (Code::Unauthenticated, 0),
            "server streaming, {case}"
        );
        assert_eq!(
            client.record_route(&md).await,
            Code::Unauthenticated,
            "client streaming, {case}"
        );
        assert_eq!(
            client.route_chat(&md).await,
            Code::Unauthenticated,
            "bidi, {case}"
        );
    }
    assert_engine_not_called(&provider, "grpc refused");

    let good = [
        ("x-mb-key", "grpc-good"),
        ("x-mockingbird-stream-items", "1"),
        ("x-trace", "t1"),
    ];
    assert_eq!(client.list_features(&good).await, (Code::Ok, 1));
    assert_eq!(client.record_route(&good).await, Code::Ok);
    assert_eq!(client.route_chat(&good).await, Code::Ok);

    assert_eq!(
        provider.stream_requests().len(),
        3,
        "one engine stream per call"
    );
    let prompts = prompts(&provider);
    for p in &prompts {
        assert!(!p.contains("grpc-good"), "prompt leaks the key: {p}");
        assert!(!p.contains("x-mb-key"), "prompt names the key header: {p}");
    }
    let records = records.wait(12).await;
    let statuses: Vec<u16> = records.iter().map(|r| r.status).collect();
    assert_eq!(
        statuses.iter().filter(|s| **s == 401).count(),
        9,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        3,
        "{statuses:?}"
    );
    assert!(records.iter().all(|r| r.workspace == "team-a"));
    assert_no_secret(&records, &["grpc-good", "nope"]);
    let ok = records.iter().find(|r| r.status == 200).unwrap();
    assert_eq!(ok.request["headers"]["x-trace"], "t1", "{:?}", ok.request);
}

/// Lists the services reflection (v1) answers with `metadata`, or the
/// status code of the refusal.
async fn reflected_services(
    channel: &Channel,
    metadata: &[(&'static str, &str)],
) -> Result<Vec<String>, Code> {
    use tonic_reflection::pb::v1;
    use v1::server_reflection_request::MessageRequest;
    use v1::server_reflection_response::MessageResponse;

    let mut client = v1::server_reflection_client::ServerReflectionClient::new(channel.clone());
    let req = v1::ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::ListServices(String::new())),
    };
    let mut resp = client
        .server_reflection_info(grpc_request(futures::stream::iter([req]), metadata))
        .await
        .map_err(|s| s.code())?
        .into_inner();
    match resp
        .next()
        .await
        .unwrap()
        .map_err(|s| s.code())?
        .message_response
    {
        Some(MessageResponse::ListServicesResponse(list)) => {
            let mut names: Vec<String> = list.service.into_iter().map(|s| s.name).collect();
            names.retain(|n| !n.starts_with("grpc."));
            names.sort();
            Ok(names)
        }
        other => panic!("not a service list: {other:?}"),
    }
}

/// Whether reflection resolves `symbol` for `metadata`.
async fn resolves(channel: &Channel, metadata: &[(&'static str, &str)], symbol: &str) -> bool {
    use tonic_reflection::pb::v1;
    use v1::server_reflection_request::MessageRequest;
    use v1::server_reflection_response::MessageResponse;

    let mut client = v1::server_reflection_client::ServerReflectionClient::new(channel.clone());
    let req = v1::ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::FileContainingSymbol(symbol.into())),
    };
    let Ok(resp) = client
        .server_reflection_info(grpc_request(futures::stream::iter([req]), metadata))
        .await
    else {
        return false;
    };
    matches!(
        resp.into_inner().next().await,
        Some(Ok(v1::ServerReflectionResponse {
            message_response: Some(MessageResponse::FileDescriptorResponse(_)),
            ..
        }))
    )
}

#[tokio::test]
async fn grpc_reflection_lists_only_what_the_key_reaches() {
    let stub = keys(&[("team-a", "a-key"), ("team-b", "b-key")]);
    let blobs = runtime("blobs", Arc::new(compile(BLOBS.as_bytes()).unwrap()), None);
    let routes = secured(
        runtime("routes", fixture("proto/route_guide.proto"), None),
        "team-a",
        "x-mb-key",
        stub.clone(),
    );
    let hello = secured(
        runtime("hello", fixture("proto/helloworld.proto"), None),
        "team-b",
        "x-mb-key",
        stub.clone(),
    );
    let channel = serve_grpc(vec![blobs, routes, hello]).await;

    // An `api_key` mock is published: reflection needs a valid key.
    assert_eq!(
        reflected_services(&channel, &[]).await,
        Err(Code::Unauthenticated)
    );
    assert_eq!(
        reflected_services(&channel, &[("x-mb-key", "nope")]).await,
        Err(Code::Unauthenticated)
    );
    assert!(!resolves(&channel, &[], "blobs.Blobs").await);

    // A key lists its workspace's mocks plus the unauthenticated ones.
    assert_eq!(
        reflected_services(&channel, &[("x-mb-key", "a-key")]).await,
        Ok(vec![
            "blobs.Blobs".to_string(),
            "routeguide.RouteGuide".to_string()
        ])
    );
    assert_eq!(
        reflected_services(&channel, &[("x-mb-key", "b-key")]).await,
        Ok(vec![
            "blobs.Blobs".to_string(),
            "helloworld.Greeter".to_string()
        ])
    );
    assert!(resolves(&channel, &[("x-mb-key", "a-key")], "routeguide.RouteGuide").await);
    assert!(!resolves(&channel, &[("x-mb-key", "a-key")], "helloworld.Greeter").await);
    assert!(resolves(&channel, &[("x-mb-key", "b-key")], "helloworld.Greeter").await);
}

#[tokio::test]
async fn grpc_reflection_is_open_without_api_key_mocks() {
    let blobs = runtime("blobs", Arc::new(compile(BLOBS.as_bytes()).unwrap()), None);
    let channel = serve_grpc(vec![blobs]).await;
    assert_eq!(
        reflected_services(&channel, &[]).await,
        Ok(vec!["blobs.Blobs".to_string()])
    );
}

// ---------------------------------------------------------- GraphQL HTTP

struct Exhausted;

#[async_trait]
impl QuotaGuard for Exhausted {
    async fn admit(&self, _t: &Tenancy, _call: &CallInfo<'_>) -> Result<(), Rejection> {
        Err(Rejection::new(429, "Too Many Requests", "quota exhausted")
            .with_retry_after(Duration::from_secs(3)))
    }
}

#[derive(Default)]
struct CountingQuota(AtomicUsize);

#[async_trait]
impl QuotaGuard for CountingQuota {
    async fn admit(&self, _t: &Tenancy, _call: &CallInfo<'_>) -> Result<(), Rejection> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn graphql_post(query: &str, key: Option<&str>) -> Request<Body> {
    let mut req = Request::post("/graphql").header("content-type", "application/json");
    if let Some(k) = key {
        req = req.header("x-key", k);
    }
    req.body(Body::from(json!({"query": query}).to_string()))
        .unwrap()
}

#[tokio::test]
async fn graphql_http_admits_before_checking_the_key() {
    let stub = keys(&[("team-a", "gql-good")]);
    let mut rt = secured(
        runtime("github", fixture("graphql/github.graphql"), None),
        "team-a",
        "x-key",
        stub.clone(),
    );
    rt.ext.quota = Arc::new(Exhausted);
    let app = graphql::try_router(Arc::new(rt)).unwrap();
    let resp = app
        .oneshot(graphql_post("{ viewer { login } }", None))
        .await
        .unwrap();
    // The quota refuses first: the key is never looked at.
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(resp.headers()["retry-after"], "3");
    assert_eq!(stub.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn graphql_http_401_is_reported_and_requests_are_admitted_once() {
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let records = Arc::new(Records::default());
    let usage = Arc::new(Usage::default());
    let quota = Arc::new(CountingQuota::default());
    let mut rt = secured(
        runtime(
            "github",
            fixture("graphql/github.graphql"),
            Some(provider.clone()),
        ),
        "team-a",
        "x-key",
        keys(&[("team-a", "gql-good")]),
    );
    rt.recorder = Some(records.clone());
    rt.ext.usage = usage.clone();
    rt.ext.quota = quota.clone();
    let app = graphql::try_router(Arc::new(rt)).unwrap();

    let resp = app
        .clone()
        .oneshot(graphql_post("{ viewer { login } }", Some("nope")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_engine_not_called(&provider, "graphql refused");
    assert_eq!(quota.0.load(Ordering::Relaxed), 1);

    let records_now = records.wait(1).await;
    assert_eq!(records_now.len(), 1);
    let r = &records_now[0];
    assert_eq!(r.status, 401);
    assert_eq!(r.workspace, "team-a");
    assert_eq!(r.mock, "github");
    assert_eq!(r.path, "/graphql");
    assert_no_secret(&records_now, &["nope"]);
    let events = usage.0.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].status, 401);
    // Refused before generation: counted under source `not_generated`.
    assert_eq!(events[0].source, "not_generated");

    // Two root fields in one admitted request: admitted once, metered as
    // two calls, recorded as one request.
    let resp = app
        .clone()
        .oneshot(graphql_post(
            "{ viewer { login } issue(id: \"1\") { id } }",
            Some("gql-good"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(quota.0.load(Ordering::Relaxed), 2);
    let statuses: Vec<u16> = records.wait(2).await.iter().map(|r| r.status).collect();
    assert_eq!(statuses, [401, 200]);
    let events = usage.0.lock().unwrap().clone();
    assert_eq!(events.len(), 3, "one usage event per root field");
}

// ----------------------------------------------------- GraphQL WebSocket

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const SUBSCRIPTION: &str = r#"subscription { issueAdded(repositoryId: "r-1") { id title } }"#;

fn issue_line() -> String {
    format!(
        "{}\n",
        json!({
            "id": "i-1", "number": 1, "title": "first", "body": null, "state": "OPEN",
            "author": null, "createdAt": "2026-01-01T00:00:00Z", "labels": [], "comments": []
        })
    )
}

async fn ws_connect(
    addr: &str,
    query: &str,
    protocol: &str,
    headers: &[(&'static str, &str)],
) -> Result<Ws, WsError> {
    let mut req = format!("ws://{addr}/graphql{query}")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("sec-websocket-protocol", protocol.parse().unwrap());
    for (k, v) in headers {
        req.headers_mut().insert(*k, v.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
}

async fn ws_send(ws: &mut Ws, msg: Value) {
    ws.send(Message::text(msg.to_string())).await.unwrap();
}

/// The next text message as JSON, or the close frame's code.
async fn ws_next(ws: &mut Ws) -> Result<Value, Option<u16>> {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out");
        match msg {
            Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Close(frame))) => return Err(frame.map(|f| u16::from(f.code))),
            None | Some(Err(_)) => return Err(None),
            Some(Ok(other)) => panic!("unexpected {other:?}"),
        }
    }
}

struct WsMock {
    addr: String,
    provider: Arc<ScriptedProvider>,
    records: Arc<Records>,
}

async fn ws_mock(streams: usize) -> WsMock {
    let scripts = (0..streams)
        .map(|_| Ok(vec![Ok::<_, LlmError>(issue_line())]))
        .collect();
    let provider = Arc::new(ScriptedProvider::new(vec![]).with_streams(scripts));
    let records = Arc::new(Records::default());
    let mut rt = secured(
        runtime(
            "github",
            fixture("graphql/github.graphql"),
            Some(provider.clone()),
        ),
        "team-a",
        "x-key",
        keys(&[("team-a", "ws-good")]),
    );
    rt.recorder = Some(records.clone());
    let addr = serve_http(graphql::try_router(Arc::new(rt)).unwrap()).await;
    WsMock {
        addr,
        provider,
        records,
    }
}

#[tokio::test]
async fn subscription_connection_without_key_is_refused() {
    let m = ws_mock(0).await;

    // graphql-transport-ws: close code 4401.
    let mut ws = ws_connect(&m.addr, "", "graphql-transport-ws", &[])
        .await
        .unwrap();
    ws_send(
        &mut ws,
        json!({"type": "connection_init", "payload": {"apiKey": "nope"}}),
    )
    .await;
    assert_eq!(ws_next(&mut ws).await, Err(Some(4401)));

    // A subscribe before (or instead of) init never runs.
    let mut ws = ws_connect(&m.addr, "", "graphql-transport-ws", &[])
        .await
        .unwrap();
    ws_send(
        &mut ws,
        json!({"type": "subscribe", "id": "1", "payload": {"query": SUBSCRIPTION}}),
    )
    .await;
    assert!(ws_next(&mut ws).await.is_err());

    // Legacy graphql-ws: connection_error.
    let mut ws = ws_connect(&m.addr, "", "graphql-ws", &[]).await.unwrap();
    ws_send(&mut ws, json!({"type": "connection_init", "payload": {}})).await;
    let msg = ws_next(&mut ws).await.unwrap();
    assert_eq!(msg["type"], "connection_error", "{msg}");

    // An invalid key on the upgrade itself: 401, no WebSocket.
    for (query, headers) in [("", vec![("x-key", "nope")]), ("?api_key=nope", vec![])] {
        match ws_connect(&m.addr, query, "graphql-transport-ws", &headers).await {
            Err(WsError::Http(resp)) => assert_eq!(resp.status(), StatusCode::UNAUTHORIZED),
            other => panic!("upgrade with a bad key: {:?}", other.map(|_| ())),
        }
    }

    assert_engine_not_called(&m.provider, "subscription refused");
    let records = m.records.wait(4).await;
    let statuses: Vec<u16> = records.iter().map(|r| r.status).collect();
    assert_eq!(statuses, [401, 401, 401, 401]);
    assert!(records.iter().all(|r| r.operation_id.is_none()));
    assert!(records.iter().all(|r| r.workspace == "team-a"));
    assert_no_secret(&records, &["nope"]);
}

async fn subscribe_one(ws: &mut Ws) {
    ws_send(
        ws,
        json!({"type": "subscribe", "id": "1", "payload": {"query": SUBSCRIPTION}}),
    )
    .await;
    let msg = ws_next(ws).await.unwrap();
    assert_eq!(msg["type"], "next", "{msg}");
    assert!(msg["payload"]["data"]["issueAdded"].is_object(), "{msg}");
    ws_send(ws, json!({"type": "complete", "id": "1"})).await;
}

#[tokio::test]
async fn subscription_key_in_init_payload_or_upgrade_never_leaks() {
    let m = ws_mock(4).await;
    let ack = json!({"type": "connection_ack"});

    // The key in each accepted connection_init member.
    for (member, secret) in [
        ("apiKey", "ws-good"),
        ("api_key", "ws-good"),
        ("x-key", "ws-good"),
    ] {
        let mut ws = ws_connect(&m.addr, "", "graphql-transport-ws", &[])
            .await
            .unwrap();
        ws_send(
            &mut ws,
            json!({"type": "connection_init", "payload": {
                member: secret,
                "Authorization": "Bearer bearer-secret",
                "x-mockingbird-stream-items": "1",
                "x-trace": "t1",
            }}),
        )
        .await;
        assert_eq!(ws_next(&mut ws).await, Ok(ack.clone()), "{member}");
        subscribe_one(&mut ws).await;
    }

    // The key on the upgrade URL.
    let mut ws = ws_connect(&m.addr, "?api_key=ws-good", "graphql-transport-ws", &[])
        .await
        .unwrap();
    ws_send(
        &mut ws,
        json!({"type": "connection_init", "payload": {"x-mockingbird-stream-items": "1"}}),
    )
    .await;
    assert_eq!(ws_next(&mut ws).await, Ok(ack));
    subscribe_one(&mut ws).await;
    drop(ws);

    assert_eq!(
        m.provider.stream_requests().len(),
        4,
        "one engine stream per subscription"
    );
    let prompts = prompts(&m.provider);
    for p in &prompts {
        for secret in ["ws-good", "bearer-secret", "apiKey", "api_key"] {
            assert!(!p.contains(secret), "prompt leaks {secret}: {p}");
        }
    }
    let records = m.records.wait(4).await;
    assert_eq!(records.len(), 4);
    assert!(records.iter().all(|r| r.status == 200));
    assert_no_secret(&records, &["ws-good", "bearer-secret"]);
    for r in &records[..3] {
        let headers = r.request["headers"].as_object().unwrap();
        assert_eq!(headers["x-trace"], "t1", "{headers:?}");
        for name in ["apikey", "api_key", "x-key", "authorization"] {
            assert!(!headers.contains_key(name), "{name} in {headers:?}");
        }
    }
}

// ------------------------------------------------------------- SOAP WSDL

#[tokio::test]
async fn soap_wsdl_requires_the_operations_credentials() {
    let rt = secured(
        runtime("calc", fixture("wsdl/calculator.wsdl"), None),
        "team-a",
        "x-api-key",
        keys(&[("team-a", "soap-good")]),
    );
    let app = soap::try_router(Arc::new(rt)).unwrap();
    let get = |key: Option<&'static str>| {
        let mut req = Request::get("/calculator.asmx?wsdl").header("host", "example.test");
        if let Some(k) = key {
            req = req.header("x-api-key", k);
        }
        app.clone().oneshot(req.body(Body::empty()).unwrap())
    };
    for key in [None, Some("nope")] {
        let resp = get(key).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{key:?}");
        assert_eq!(
            resp.headers()["www-authenticate"],
            r#"ApiKey realm="mockingbird", header="x-api-key""#
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("definitions"));
    }
    let resp = get(Some("soap-good")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("definitions"));
}

#[tokio::test]
async fn soap_wsdl_open_without_data_plane_auth() {
    let app = soap::try_router(Arc::new(runtime(
        "calc",
        fixture("wsdl/calculator.wsdl"),
        None,
    )))
    .unwrap();
    let resp = app
        .oneshot(
            Request::get("/calculator.asmx?wsdl")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
