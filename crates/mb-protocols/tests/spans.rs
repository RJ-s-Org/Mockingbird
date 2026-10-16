//! Per-stage trace spans (spec §14): every call's `mock.request` span has
//! one child span per pipeline stage.
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{LlmError, Provider, ScriptedProvider};
use mb_mcm::Bundle;
use mb_protocols::MockRuntime;
use mb_protocols::http_rest;
use mb_state::MemoryStore;
use serde_json::json;
use tower::ServiceExt;
use tracing::span::{Attributes, Id};
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

const REQUEST_SPAN: &str = "mock.request";
const STAGES: [&str; 8] = [
    "mb.decode",
    "mb.admission",
    "mb.state",
    "mb.prompt",
    "mb.llm",
    "mb.validate",
    "mb.repair",
    "mb.encode",
];

/// One span as it was opened.
#[derive(Clone, Debug)]
struct Opened {
    name: &'static str,
    /// Names of the enclosing spans, innermost first.
    ancestors: Vec<&'static str>,
    fields: Vec<&'static str>,
}

/// Records every span opened on this thread.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Opened>>>);

impl<S> Layer<S> for Capture
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let ancestors = span.scope().skip(1).map(|s| s.name()).collect();
        let fields = attrs.metadata().fields().iter().map(|f| f.name()).collect();
        self.0.lock().unwrap().push(Opened {
            name: attrs.metadata().name(),
            ancestors,
            fields,
        });
    }
}

impl Capture {
    /// Captures the spans of this thread until the guard drops (the tests
    /// run on a current-thread runtime, so every task they spawn counts).
    fn install() -> (Self, DefaultGuard) {
        let capture = Self::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        (capture, guard)
    }

    fn spans(&self) -> Vec<Opened> {
        self.0.lock().unwrap().clone()
    }

    fn count(&self, name: &str) -> usize {
        self.spans().iter().filter(|s| s.name == name).count()
    }

    /// Every pipeline stage span opened, each inside the request span.
    fn assert_stages(&self, expected: &[&str]) {
        let spans = self.spans();
        for stage in expected {
            let opened: Vec<&Opened> = spans.iter().filter(|s| s.name == *stage).collect();
            assert!(!opened.is_empty(), "no {stage} span; spans: {spans:#?}");
            for s in opened {
                assert!(
                    s.ancestors.contains(&REQUEST_SPAN),
                    "{stage} is not under {REQUEST_SPAN}: {s:?}"
                );
            }
        }
        // The stage spans opened by the adapter sit directly under it.
        for stage in ["mb.decode", "mb.admission", "mb.encode"] {
            for s in spans.iter().filter(|s| s.name == stage) {
                assert_eq!(s.ancestors.first(), Some(&REQUEST_SPAN), "{s:?}");
            }
        }
        for s in spans.iter().filter(|s| s.name == "mb.llm") {
            for field in ["llm.model", "llm.tokens.prompt", "llm.tokens.completion"] {
                assert!(s.fields.contains(&field), "mb.llm lacks {field}: {s:?}");
            }
        }
    }
}

fn orders() -> Arc<Bundle> {
    let path = format!(
        "{}/../../fixtures/openapi/orders-3.0.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    Arc::new(compile(&std::fs::read(path).unwrap()).unwrap())
}

fn runtime(name: &str, bundle: Arc<Bundle>, provider: Arc<ScriptedProvider>) -> MockRuntime {
    MockRuntime::new(
        name,
        bundle,
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    )
}

fn create_order() -> Request<Body> {
    Request::post("/orders")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"customerId":"c1","total":10}"#))
        .unwrap()
}

fn order() -> String {
    json!({"id": "o1", "customerId": "c1", "status": "pending", "total": 10}).to_string()
}

#[tokio::test]
async fn rest_call_emits_pipeline_spans() {
    let (capture, _guard) = Capture::install();
    let provider = Arc::new(ScriptedProvider::new(vec![
        Ok("not json".into()),
        Ok(order()),
    ]));
    let rt = Arc::new(runtime("orders", orders(), Arc::clone(&provider)));
    let resp = http_rest::router(rt).oneshot(create_order()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(resp.headers()["x-mockingbird-source"], "repaired");

    capture.assert_stages(&STAGES);
    assert_eq!(capture.count("mb.repair"), 1);
    assert_eq!(capture.count("mb.llm"), 2);
    assert_eq!(capture.count(REQUEST_SPAN), 1);
    // The repair call's LLM span sits inside the repair span.
    let spans = capture.spans();
    let repair_llm = spans
        .iter()
        .filter(|s| s.name == "mb.llm" && s.ancestors.first() == Some(&"mb.repair"))
        .count();
    assert_eq!(repair_llm, 1, "{spans:#?}");
}

#[tokio::test]
async fn fallback_call_has_no_repair_span_without_second_llm_call() {
    let (capture, _guard) = Capture::install();
    let provider = Arc::new(ScriptedProvider::new(vec![Err(LlmError::Timeout)]));
    let rt = Arc::new(runtime("orders", orders(), Arc::clone(&provider)));
    let resp = http_rest::router(rt).oneshot(create_order()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(resp.headers()["x-mockingbird-source"], "fallback");
    assert_eq!(provider.requests().len(), 1);

    let without_repair: Vec<&str> = STAGES
        .into_iter()
        .filter(|s| !matches!(*s, "mb.repair" | "mb.validate"))
        .collect();
    capture.assert_stages(&without_repair);
    assert_eq!(capture.count("mb.repair"), 0);
    assert_eq!(capture.count("mb.llm"), 1);
}

#[cfg(feature = "grpc")]
#[tokio::test]
async fn grpc_unary_emits_pipeline_spans() {
    use mb_protocols::grpc::{DynamicCodec, GrpcMock, GrpcServer};
    use prost_reflect::{DescriptorPool, DynamicMessage};
    use tonic::codegen::http::uri::PathAndQuery;

    let (capture, _guard) = Capture::install();
    let path = format!(
        "{}/../../fixtures/proto/route_guide.proto",
        env!("CARGO_MANIFEST_DIR")
    );
    let bundle = Arc::new(compile(&std::fs::read(path).unwrap()).unwrap());
    let provider = Arc::new(ScriptedProvider::new(vec![
        Ok("not json".into()),
        Ok(json!({"name": "summit"}).to_string()),
    ]));
    let rt = runtime("routes", Arc::clone(&bundle), Arc::clone(&provider));

    let server = GrpcServer::new();
    server
        .publish(vec![Arc::new(GrpcMock::new(Arc::new(rt)).unwrap())])
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();

    let Some(mb_mcm::ProtocolPayload::Grpc {
        file_descriptor_set,
    }) = &bundle.protocol_payload
    else {
        panic!("no descriptors")
    };
    let pool = DescriptorPool::decode(file_descriptor_set.as_slice()).unwrap();
    let point = pool.get_message_by_name("routeguide.Point").unwrap();
    let feature = pool.get_message_by_name("routeguide.Feature").unwrap();
    let message =
        DynamicMessage::deserialize(point.clone(), json!({"latitude": 1, "longitude": 2})).unwrap();
    let mut client = tonic::client::Grpc::new(channel);
    client.ready().await.unwrap();
    let resp = client
        .unary(
            tonic::Request::new(message),
            PathAndQuery::from_static("/routeguide.RouteGuide/GetFeature"),
            DynamicCodec::new(point, feature),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.metadata().get("x-mockingbird-source").unwrap(),
        "repaired"
    );

    capture.assert_stages(&STAGES);
    assert_eq!(capture.count("mb.repair"), 1);
    assert_eq!(capture.count("mb.llm"), 2);
}
