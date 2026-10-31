//! One mock's gRPC methods: decoding calls, running them through the
//! shared [`call`](crate::call) pipeline and encoding the generated JSON
//! as protobuf messages.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request};
use axum::response::Response;
use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use futures::{FutureExt, StreamExt};
use http_body::{Frame, SizeHint};
use mb_engine::{EngineError, GenerationContext, OperationCall, Source, StreamItem};
use mb_ext::Tenancy;
use mb_llm::Usage;
use mb_mcm::{Binding, Operation, ProtocolPayload, StatusSelector};
use mb_state::SessionKey;
use prost_reflect::{DescriptorPool, DynamicMessage, Kind, MessageDescriptor, MethodDescriptor};
use serde_json::{Value, json};
use tokio::time::Instant;
use tonic::codec::CompressionEncoding;
use tonic::codegen::Bytes;
use tonic::server::{
    ClientStreamingService, Grpc, ServerStreamingService, StreamingService, UnaryService,
};
use tonic::{Status, Streaming};
use tracing::field::Empty;
use tracing::{Instrument, Span};

use super::GrpcError;
use super::codec::DynamicCodec;
use super::status::{Failure, oversized};
use crate::call::{
    self, CallPlan, DEFAULT_SESSION, Presented, Report, forwarded_headers, recorded_headers,
};
use crate::{ChaosDecision, MockRuntime, ScenarioRuntime};

const SOURCE_HEADER: HeaderName = HeaderName::from_static("x-mockingbird-source");
const SOURCE_METADATA: &str = "x-mockingbird-source";
/// Earlier client messages handed to the engine with each call.
const REQUEST_STREAM_ITEMS: usize = 20;

type MessageStream = BoxStream<'static, Result<DynamicMessage, Status>>;

/// A mock's protobuf services, ready to be published on a
/// [`GrpcServer`](super::GrpcServer).
///
/// Per call kind:
/// - unary: one engine response;
/// - server streaming: one message per engine stream item, then `OK`;
/// - client streaming: every client message is read first; the last one is
///   the request body and the last 20 the request stream; the first item
///   of a one-item engine stream is the response;
/// - bidi: each client message gets the first item of its own one-item
///   engine stream (with up to 20 earlier messages), in order; `OK` once
///   the client half-closes. Each reply is metered as its own usage event.
///
/// In client and bidi streams every client message after the first (which
/// the call's own admission covers) runs `quota.admit` and the mock's rate
/// limit again; a refusal ends the call with `RESOURCE_EXHAUSTED`, after
/// the replies already sent.
///
/// Generated JSON that does not fit the response message is replaced by
/// the engine's fallback; if that does not fit either, the call fails with
/// `INTERNAL`.
///
/// An error injected by chaos fails the call (after any latency) with the
/// code of the declared error status, else `UNAVAILABLE`, and the metadata
/// `x-mockingbird-chaos: error`. A server or bidi stream chaos drops ends
/// with `UNAVAILABLE` ("stream dropped by chaos") after its planned
/// number of messages.
pub struct GrpcMock {
    rt: Arc<MockRuntime>,
    /// By gRPC path (`/<package>.<Service>/<Method>`).
    methods: HashMap<String, Method>,
    services: Vec<String>,
    descriptor_set: Vec<u8>,
}

/// Shows the mock and the services it serves.
impl std::fmt::Debug for GrpcMock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrpcMock")
            .field("mock", &self.rt.name)
            .field("services", &self.services)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct Method {
    op_id: String,
    path: String,
    desc: MethodDescriptor,
}

impl GrpcMock {
    /// Resolves every gRPC operation of `rt.bundle` against the bundle's
    /// descriptor set.
    ///
    /// # Errors
    ///
    /// [`GrpcError::MissingDescriptors`] when the bundle carries no
    /// descriptor set, and [`GrpcError::Descriptors`] when the set does not
    /// decode or lacks a method an operation names.
    pub fn new(rt: Arc<MockRuntime>) -> Result<Self, GrpcError> {
        let Some(ProtocolPayload::Grpc {
            file_descriptor_set,
        }) = &rt.bundle.protocol_payload
        else {
            return Err(GrpcError::MissingDescriptors);
        };
        let pool = DescriptorPool::decode(file_descriptor_set.as_slice())
            .map_err(|e| GrpcError::Descriptors(e.to_string()))?;
        let mut methods = HashMap::new();
        let mut services = BTreeSet::new();
        for op in &rt.bundle.operations {
            let (
                Binding::Grpc {
                    package,
                    service,
                    method,
                },
                Some(path),
            ) = (&op.binding, op.binding.grpc_path())
            else {
                continue;
            };
            let full = if package.is_empty() {
                service.clone()
            } else {
                format!("{package}.{service}")
            };
            let desc = pool
                .get_service_by_name(&full)
                .and_then(|s| s.methods().find(|m| m.name() == method))
                .ok_or_else(|| {
                    GrpcError::Descriptors(format!(
                        "operation {} names {path}, which the descriptors do not declare",
                        op.id
                    ))
                })?;
            let entry = Method {
                op_id: op.id.clone(),
                path: path.clone(),
                desc,
            };
            methods.insert(path, entry);
            services.insert(full);
        }
        Ok(Self {
            descriptor_set: file_descriptor_set.clone(),
            rt,
            methods,
            services: services.into_iter().collect(),
        })
    }

    /// The runtime this mock serves.
    pub(crate) fn runtime(&self) -> &Arc<MockRuntime> {
        &self.rt
    }

    /// The mock's name.
    pub fn name(&self) -> &str {
        &self.rt.name
    }

    /// Full names of the services this mock serves, sorted.
    pub fn services(&self) -> Vec<String> {
        self.services.clone()
    }

    /// The bundle's serialized `FileDescriptorSet`.
    pub fn descriptor_set(&self) -> &[u8] {
        &self.descriptor_set
    }

    /// Serves one HTTP/2 request addressed to one of this mock's services.
    pub(crate) async fn handle(self: &Arc<Self>, req: Request<Body>) -> Response {
        let path = req.uri().path().to_string();
        let Some(method) = self.methods.get(&path).cloned() else {
            return Status::unimplemented(format!("unknown method {path}")).into_http();
        };
        let span = tracing::info_span!(
            "mock.request",
            mock = %self.rt.name,
            operation = %method.op_id,
            session = Empty,
            status = Empty,
            source = Empty,
        );
        let limit = self.rt.protocols.grpc_max_message_bytes;
        let decoding_limit = limit.min(self.rt.limits.max_request_bytes);
        let codec = DynamicCodec::new(method.desc.output(), method.desc.input());
        // gzip requests are decoded; responses are gzipped for clients
        // that accept it (`grpc-accept-encoding`), others get them plain.
        let mut grpc = Grpc::new(codec)
            .accept_compressed(CompressionEncoding::Gzip)
            .send_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(decoding_limit)
            .max_encoding_message_size(limit);
        let headers = req.headers().clone();
        let call = Arc::new(CallState {
            mock: Arc::clone(self),
            method,
            span: span.clone(),
            invoked: AtomicBool::new(false),
            source: Mutex::new(None),
        });
        let svc = Svc(Arc::clone(&call));
        let desc = &call.method.desc;
        let resp = match (desc.is_client_streaming(), desc.is_server_streaming()) {
            (false, false) => grpc.unary(svc, req).instrument(span).await,
            (false, true) => grpc.server_streaming(svc, req).instrument(span).await,
            (true, false) => grpc.client_streaming(svc, req).instrument(span).await,
            (true, true) => grpc.streaming(svc, req).instrument(span).await,
        };
        if !call.invoked.load(Ordering::SeqCst) {
            call.report_rejected_request(headers, resp.headers()).await;
            if let Some(status) = Status::from_header_map(resp.headers())
                .as_ref()
                .and_then(oversized)
            {
                return status.into_http();
            }
        }
        resp.map(|inner| Body::new(WithSource { inner, call }))
    }
}

/// One call in flight.
struct CallState {
    mock: Arc<GrpcMock>,
    method: Method,
    span: Span,
    /// Whether tonic handed the call to the mock (it answers decoding
    /// failures of unary and server-streaming requests itself).
    invoked: AtomicBool,
    /// The (worst) source of the messages sent, for the trailers.
    source: Mutex<Option<Source>>,
}

impl CallState {
    fn rt(&self) -> &Arc<MockRuntime> {
        &self.mock.rt
    }

    fn op(&self) -> Option<&Operation> {
        self.rt().operation(&self.method.op_id)
    }

    fn source_slot(&self) -> MutexGuard<'_, Option<Source>> {
        self.source.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn add_source(&self, source: Source) {
        let mut slot = self.source_slot();
        *slot = Some(slot.map_or(source, |s| s.worst(source)));
    }

    fn reporter(&self, headers: HeaderMap) -> Reporter {
        Reporter {
            rt: Arc::clone(self.rt()),
            span: self.span.clone(),
            started: Instant::now(),
            request: if self.rt().recorder.is_some() {
                json!({"headers": recorded_headers(self.rt(), &headers)})
            } else {
                Value::Null
            },
            headers,
            operation_id: self.method.op_id.clone(),
            path: self.method.path.clone(),
            session: DEFAULT_SESSION.into(),
            tenancy: None,
            admission_ran: true,
            status: 200,
            source: None,
            usage: Usage::default(),
            response: None,
            items: 0,
            metered: false,
            done: false,
        }
    }

    /// Reports a request tonic refused before the mock saw it.
    async fn report_rejected_request(&self, headers: HeaderMap, response: &HeaderMap) {
        let status =
            Status::from_header_map(response).unwrap_or_else(|| Status::unknown("request refused"));
        let mut rep = self.reporter(headers);
        rep.admission_ran = false;
        rep.fail(&Failure::request(status));
        rep.finish().await;
    }

    /// Plans the call. A failed plan or a forced non-2xx status (answered
    /// after any injected latency) is an `Err`.
    async fn start(&self, rep: &mut Reporter) -> Result<OwnedPlan, Failure> {
        let op = self
            .op()
            .ok_or_else(|| Failure::internal(format!("unknown operation {}", self.method.op_id)))?;
        // gRPC credentials travel in the request metadata (HTTP/2 headers).
        let plan = match call::plan(self.rt(), op, &rep.headers, Presented::default()).await {
            Ok(plan) => plan,
            Err(failure) => {
                rep.tenancy = failure.tenancy;
                if let Some(s) = failure.session {
                    rep.session = s;
                }
                return Err(Failure::plan(failure.error, &self.rt().name));
            }
        };
        rep.tenancy = Some(plan.tenancy.clone());
        rep.session = plan.session.session.clone();
        if plan.chaos.inject_error {
            sleep(plan.delay).await;
            return Err(Failure::chaos(plan.forced_status));
        }
        if let Some(status) = plan.forced_status.filter(|s| !(200..300).contains(s)) {
            sleep(plan.delay).await;
            return Err(Failure::forced(status));
        }
        Ok(OwnedPlan::new(self.rt(), &plan))
    }

    /// Admits one more client message of a call admitted with `plan`.
    async fn admit_item(&self, plan: &OwnedPlan, headers: &HeaderMap) -> Result<(), Failure> {
        call::admit_item(self.rt(), &plan.tenancy, &self.method.op_id, headers)
            .await
            .map_err(|r| Failure {
                http: r.status,
                status: Status::resource_exhausted(r.detail),
            })
    }

    fn operation_call(&self, headers: &HeaderMap, body: Option<Value>) -> OperationCall {
        OperationCall {
            operation_id: self.method.op_id.clone(),
            headers: forwarded_headers(self.rt(), headers),
            body,
            ..OperationCall::default()
        }
    }

    /// `body` as a response message; else the engine's fallback for the
    /// success variant; else `INTERNAL`. Returns the message, the JSON it
    /// was built from and its source.
    fn response(
        &self,
        call: &OperationCall,
        body: Option<Value>,
        source: Source,
    ) -> Result<(DynamicMessage, Value, Source), Failure> {
        let desc = self.method.desc.output();
        let first = match body {
            Some(body) => to_message(&desc, body),
            None => Err("no body was generated".into()),
        };
        let reason = match first {
            Ok((message, json)) => return Ok((message, json, source)),
            Err(reason) => reason,
        };
        tracing::warn!(operation = %self.method.op_id, %reason, "generated JSON does not fit {}; using the fallback", desc.full_name());
        let op = self
            .op()
            .ok_or_else(|| Failure::internal(format!("unknown operation {}", self.method.op_id)))?;
        let variant = op
            .success_output()
            .ok_or_else(|| Failure::internal(EngineError::NoOutput.to_string()))?;
        let status = match variant.status {
            StatusSelector::Code(c) => c,
            _ => 200,
        };
        let rt = self.rt();
        let fallback = rt.engine.fallback(
            &rt.bundle,
            op,
            variant,
            status,
            call,
            &GenerationContext::default(),
        );
        let body = fallback.body.unwrap_or(Value::Null);
        to_message(&desc, body)
            .map(|(message, json)| (message, json, Source::Fallback))
            .map_err(|reason| {
                Failure::internal(format!(
                    "cannot build a {} message: {reason}",
                    desc.full_name()
                ))
            })
    }

    /// The first item of a one-item engine stream for `call`, as a
    /// response message, with the tokens that item cost.
    async fn one_item(
        &self,
        plan: &OwnedPlan,
        call: OperationCall,
        request_stream: Vec<Value>,
    ) -> Result<((DynamicMessage, Value, Source), Usage), Failure> {
        let mut single = plan.plan(self)?;
        single.stream_items = Some(1);
        let mut items = call::open_stream(self.rt(), &single, call.clone(), request_stream)
            .await
            .map_err(engine_failure)?;
        let first = items.next().await;
        drop(items);
        let (body, source, usage) = match first {
            Some(StreamItem {
                value,
                source,
                usage,
            }) => (Some(value), source, usage),
            None => (None, Source::Fallback, Usage::default()),
        };
        Ok((self.response(&call, body, source)?, usage))
    }

    async fn unary(self: Arc<Self>, req: tonic::Request<DynamicMessage>) -> UnaryResult {
        self.invoked.store(true, Ordering::SeqCst);
        let (metadata, _, message) = req.into_parts();
        let mut rep = self.reporter(metadata.into_headers());
        let body = tracing::info_span!("mb.decode").in_scope(|| message_json(&message));
        rep.set_body(&body);
        let outcome = async {
            let plan = self.start(&mut rep).await?;
            let call = self.operation_call(&rep.headers, Some(body));
            let generated = call::generate_once(self.rt(), &plan.plan(&self)?, &call)
                .await
                .map_err(engine_failure)?;
            rep.usage = generated.usage;
            call::record_effects(&mut rep.request, &generated);
            let (message, json, source) = tracing::info_span!("mb.encode")
                .in_scope(|| self.response(&call, generated.body, generated.source))?;
            sleep(plan.delay).await;
            Ok((message, json, source))
        }
        .await;
        match outcome {
            Ok((message, json, source)) => {
                rep.sent(source, json);
                self.add_source(source);
                rep.finish().await;
                let mut resp = tonic::Response::new(message);
                resp.metadata_mut()
                    .insert(SOURCE_METADATA, source_value(source));
                Ok(resp)
            }
            Err(failure) => {
                rep.fail(&failure);
                rep.finish().await;
                Err(failure.status)
            }
        }
    }

    async fn server_streaming(
        self: Arc<Self>,
        req: tonic::Request<DynamicMessage>,
    ) -> StreamResult {
        self.invoked.store(true, Ordering::SeqCst);
        let (metadata, _, message) = req.into_parts();
        let mut rep = self.reporter(metadata.into_headers());
        let body = tracing::info_span!("mb.decode").in_scope(|| message_json(&message));
        rep.set_body(&body);
        let opened = async {
            let plan = self.start(&mut rep).await?;
            let call = self.operation_call(&rep.headers, Some(body));
            let items = call::open_stream(self.rt(), &plan.plan(&self)?, call.clone(), Vec::new())
                .await
                .map_err(engine_failure)?;
            Ok((plan, call, items))
        }
        .await;
        let (plan, call, items) = match opened {
            Ok(opened) => opened,
            Err(failure) => {
                rep.fail(&failure);
                rep.finish().await;
                return Err(failure.status);
            }
        };
        let state = ServerStream {
            call: Arc::clone(&self),
            delay: plan.delay,
            drop_after: plan.chaos.drop_after,
            op_call: call,
            items,
            rep,
            ended: false,
        };
        let stream = stream::unfold(state, |mut st| async move {
            let span = st.call.span.clone();
            let out = st.next().instrument(span).await?;
            Some((out, st))
        });
        Ok(tonic::Response::new(stream.boxed()))
    }

    async fn client_streaming(
        self: Arc<Self>,
        req: tonic::Request<Streaming<DynamicMessage>>,
    ) -> UnaryResult {
        self.invoked.store(true, Ordering::SeqCst);
        let (metadata, _, inbound) = req.into_parts();
        let mut rep = self.reporter(metadata.into_headers());
        let outcome = async {
            let plan = self.start(&mut rep).await?;
            let headers = rep.headers.clone();
            let (this, plan_ref, headers_ref) = (&*self, &plan, &headers);
            let messages = inbound.enumerate().then(|(i, m)| async move {
                let m = m.map_err(Failure::request)?;
                if i > 0 {
                    this.admit_item(plan_ref, headers_ref).await?;
                }
                Ok::<_, Failure>(message_json(&m))
            });
            let request_stream = last_items(messages, REQUEST_STREAM_ITEMS).await?;
            let last = request_stream.back().cloned();
            if let Some(body) = &last {
                rep.set_body(body);
            }
            let request_stream = Vec::from(request_stream);
            let call = self.operation_call(&rep.headers, last);
            let (reply, usage) = self.one_item(&plan, call, request_stream).await?;
            rep.usage = usage;
            sleep(plan.delay).await;
            Ok(reply)
        }
        .await;
        match outcome {
            Ok((message, _, source)) => {
                rep.sent(source, json!({"items": 1}));
                self.add_source(source);
                rep.finish().await;
                Ok(tonic::Response::new(message))
            }
            Err(failure) => {
                rep.fail(&failure);
                rep.finish().await;
                Err(failure.status)
            }
        }
    }

    async fn bidi(self: Arc<Self>, req: tonic::Request<Streaming<DynamicMessage>>) -> StreamResult {
        self.invoked.store(true, Ordering::SeqCst);
        let (metadata, _, inbound) = req.into_parts();
        let mut rep = self.reporter(metadata.into_headers());
        let plan = match self.start(&mut rep).await {
            Ok(plan) => plan,
            Err(failure) => {
                rep.fail(&failure);
                rep.finish().await;
                return Err(failure.status);
            }
        };
        // Each reply is metered on its own.
        rep.metered = true;
        let state = Bidi {
            call: Arc::clone(&self),
            delay: plan.delay,
            drop_after: plan.chaos.drop_after,
            plan,
            inbound,
            earlier: VecDeque::new(),
            rep,
            received: 0,
            ended: false,
        };
        let stream = stream::unfold(state, |mut st| async move {
            let span = st.call.span.clone();
            let out = st.next().instrument(span).await?;
            Some((out, st))
        });
        Ok(tonic::Response::new(stream.boxed()))
    }
}

type UnaryResult = Result<tonic::Response<DynamicMessage>, Status>;
type StreamResult = Result<tonic::Response<MessageStream>, Status>;

/// A server-streaming call's remaining engine items.
struct ServerStream {
    call: Arc<CallState>,
    op_call: OperationCall,
    items: BoxStream<'static, StreamItem>,
    rep: Reporter,
    /// Slept before the first message.
    delay: Option<Duration>,
    /// Messages sent before chaos drops the stream.
    drop_after: Option<usize>,
    ended: bool,
}

impl ServerStream {
    async fn next(&mut self) -> Option<Result<DynamicMessage, Status>> {
        if self.ended {
            return None;
        }
        if self.drop_after == Some(self.rep.items) {
            self.ended = true;
            sleep(self.delay.take()).await;
            let failure = Failure::dropped(self.call.rt());
            self.rep.fail(&failure);
            return Some(Err(failure.status));
        }
        let Some(item) = self.items.next().await else {
            self.ended = true;
            return None;
        };
        sleep(self.delay.take()).await;
        match self
            .call
            .response(&self.op_call, Some(item.value), item.source)
        {
            Ok((message, _, source)) => {
                self.rep.items += 1;
                self.rep.add_source(source);
                self.call.add_source(source);
                Some(Ok(message))
            }
            Err(failure) => {
                self.ended = true;
                self.rep.fail(&failure);
                Some(Err(failure.status))
            }
        }
    }
}

/// A bidi call: the client's messages still to answer.
struct Bidi {
    call: Arc<CallState>,
    plan: OwnedPlan,
    inbound: Streaming<DynamicMessage>,
    /// Up to 20 earlier client messages.
    earlier: VecDeque<Value>,
    rep: Reporter,
    /// Slept before the first reply.
    delay: Option<Duration>,
    /// Client messages read so far.
    received: usize,
    /// Replies sent before chaos drops the stream.
    drop_after: Option<usize>,
    ended: bool,
}

impl Bidi {
    async fn next(&mut self) -> Option<Result<DynamicMessage, Status>> {
        if self.ended {
            return None;
        }
        if self.drop_after == Some(self.rep.items) {
            self.ended = true;
            sleep(self.delay.take()).await;
            let failure = Failure::dropped(self.call.rt());
            self.rep.fail(&failure);
            return Some(Err(failure.status));
        }
        let reply = async {
            let Some(message) = self.inbound.message().await.map_err(Failure::request)? else {
                return Ok(None);
            };
            let started = Instant::now();
            self.received += 1;
            if self.received > 1 {
                self.call.admit_item(&self.plan, &self.rep.headers).await?;
            }
            let body = message_json(&message);
            self.rep.set_body(&body);
            // The earlier messages only go in the LLM prompt: without a
            // provider they are neither kept nor copied.
            let prompted = self.plan.engine.has_provider();
            let (request_stream, kept) = if prompted {
                (self.earlier.iter().cloned().collect(), Some(body.clone()))
            } else {
                (Vec::new(), None)
            };
            let call = self.call.operation_call(&self.rep.headers, Some(body));
            let reply = self.call.one_item(&self.plan, call, request_stream).await?;
            if let Some(body) = kept {
                if self.earlier.len() == REQUEST_STREAM_ITEMS {
                    self.earlier.pop_front();
                }
                self.earlier.push_back(body);
            }
            sleep(self.delay.take()).await;
            Ok(Some((reply, started.elapsed())))
        }
        .await;
        match reply {
            Ok(Some((((message, _, source), usage), latency))) => {
                call::meter(
                    self.call.rt(),
                    self.plan.tenancy.clone(),
                    &self.call.method.op_id,
                    source,
                    latency,
                    usage,
                );
                self.rep.items += 1;
                self.rep.add_source(source);
                self.call.add_source(source);
                Some(Ok(message))
            }
            Ok(None) => {
                self.ended = true;
                None
            }
            Err(failure) => {
                self.ended = true;
                self.rep.fail(&failure);
                Some(Err(failure.status))
            }
        }
    }
}

/// The tonic service of one call.
struct Svc(Arc<CallState>);

impl UnaryService<DynamicMessage> for Svc {
    type Response = DynamicMessage;
    type Future = BoxFuture<'static, UnaryResult>;

    fn call(&mut self, request: tonic::Request<DynamicMessage>) -> Self::Future {
        Arc::clone(&self.0).unary(request).boxed()
    }
}

impl ServerStreamingService<DynamicMessage> for Svc {
    type Response = DynamicMessage;
    type ResponseStream = MessageStream;
    type Future = BoxFuture<'static, StreamResult>;

    fn call(&mut self, request: tonic::Request<DynamicMessage>) -> Self::Future {
        Arc::clone(&self.0).server_streaming(request).boxed()
    }
}

impl ClientStreamingService<DynamicMessage> for Svc {
    type Response = DynamicMessage;
    type Future = BoxFuture<'static, UnaryResult>;

    fn call(&mut self, request: tonic::Request<Streaming<DynamicMessage>>) -> Self::Future {
        Arc::clone(&self.0).client_streaming(request).boxed()
    }
}

impl StreamingService<DynamicMessage> for Svc {
    type Response = DynamicMessage;
    type ResponseStream = MessageStream;
    type Future = BoxFuture<'static, StreamResult>;

    fn call(&mut self, request: tonic::Request<Streaming<DynamicMessage>>) -> Self::Future {
        Arc::clone(&self.0).bidi(request).boxed()
    }
}

/// A [`CallPlan`] that owns what it borrows, so streams can outlive the
/// handler. The operation and the scenario are not copied: they are found
/// again in the call's (fixed) runtime.
struct OwnedPlan {
    session: SessionKey,
    scenario: Option<PlanScenario>,
    delay: Option<Duration>,
    stream_items: Option<usize>,
    tenancy: Tenancy,
    engine: Arc<mb_engine::Engine>,
    forced_status: Option<u16>,
    chaos: ChaosDecision,
}

/// The scenario of an [`OwnedPlan`].
enum PlanScenario {
    /// The runtime's scenario of this name, as it applies to the operation.
    Named(String),
    /// A scenario that is not the runtime's (not expected: plans select
    /// the runtime's scenarios), copied.
    Owned(Box<ScenarioRuntime>),
}

impl OwnedPlan {
    fn new(rt: &MockRuntime, plan: &CallPlan<'_>) -> Self {
        let scenario = plan.scenario.map(|selected| {
            rt.scenarios
                .iter()
                .find(|(_, s)| std::ptr::eq(s.for_operation(&plan.op.id), selected))
                .map_or_else(
                    || PlanScenario::Owned(Box::new(selected.clone())),
                    |(name, _)| PlanScenario::Named(name.clone()),
                )
        });
        Self {
            session: plan.session.clone(),
            scenario,
            delay: plan.delay,
            stream_items: plan.stream_items,
            tenancy: plan.tenancy.clone(),
            engine: plan.engine.clone(),
            forced_status: plan.forced_status,
            chaos: plan.chaos,
        }
    }

    /// The plan again, borrowing the operation and scenario from `call`'s
    /// runtime.
    fn plan<'a>(&'a self, call: &'a CallState) -> Result<CallPlan<'a>, Failure> {
        let op = call
            .op()
            .ok_or_else(|| Failure::internal(format!("unknown operation {}", call.method.op_id)))?;
        let scenario = match &self.scenario {
            None => None,
            Some(PlanScenario::Owned(s)) => Some(s.as_ref()),
            Some(PlanScenario::Named(name)) => Some(
                call.rt()
                    .scenarios
                    .get(name)
                    .ok_or_else(|| Failure::internal(format!("unknown scenario {name}")))?
                    .for_operation(&op.id),
            ),
        };
        Ok(CallPlan {
            op,
            session: self.session.clone(),
            scenario,
            forced_status: self.forced_status,
            delay: self.delay,
            stream_items: self.stream_items,
            tenancy: self.tenancy.clone(),
            engine: self.engine.clone(),
            chaos: self.chaos,
        })
    }
}

/// Collects one call's [`Report`]; reports it on [`Reporter::finish`], or
/// when dropped (a stream finished, failed or was cancelled).
struct Reporter {
    rt: Arc<MockRuntime>,
    span: Span,
    started: Instant,
    headers: HeaderMap,
    operation_id: String,
    path: String,
    session: String,
    tenancy: Option<Tenancy>,
    admission_ran: bool,
    status: u16,
    source: Option<Source>,
    usage: Usage,
    /// `{headers, body}`; `Null` without a recorder.
    request: Value,
    response: Option<Value>,
    /// Messages sent (streams).
    items: usize,
    /// Usage was already recorded per message (bidi replies): the report
    /// goes to the request recorder only.
    metered: bool,
    done: bool,
}

impl Reporter {
    /// Records the (latest) request message.
    fn set_body(&mut self, body: &Value) {
        if let Value::Object(request) = &mut self.request {
            request.insert("body".into(), body.clone());
        }
    }

    fn add_source(&mut self, source: Source) {
        self.source = Some(self.source.map_or(source, |s| s.worst(source)));
    }

    /// A single response message was sent.
    fn sent(&mut self, source: Source, response: Value) {
        self.add_source(source);
        self.items = 1;
        self.response = Some(response);
    }

    fn fail(&mut self, failure: &Failure) {
        self.status = failure.http;
        self.response = Some(json!({
            "code": i32::from(failure.status.code()),
            "message": failure.status.message(),
        }));
    }

    fn take(&mut self) -> Report {
        self.done = true;
        self.span.record("status", self.status);
        if let Some(source) = self.source {
            self.span.record("source", source.as_str());
        }
        let response = match self.response.take() {
            Some(r) if self.status != 200 || self.items <= 1 => r,
            _ => json!({"items": self.items}),
        };
        Report {
            operation_id: Some(std::mem::take(&mut self.operation_id)),
            session: std::mem::take(&mut self.session),
            method: "POST".into(),
            path: std::mem::take(&mut self.path),
            status: self.status,
            source: self.source,
            fallback_reason: None,
            usage: self.usage,
            latency: self.started.elapsed(),
            tenancy: self.tenancy.take(),
            admission_ran: self.admission_ran,
            request: self.request.take(),
            request_body_bytes: None,
            response: self.rt.recorder.is_some().then_some(response),
        }
    }

    async fn finish(mut self) {
        let report = self.take();
        let span = self.span.clone();
        send_report(&self.rt, &self.headers, report, self.metered)
            .instrument(span)
            .await;
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let report = self.take();
        let rt = Arc::clone(&self.rt);
        let headers = std::mem::take(&mut self.headers);
        let span = self.span.clone();
        let metered = self.metered;
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    send_report(&rt, &headers, report, metered)
                        .instrument(span)
                        .await
                });
            }
            Err(_) => tracing::warn!(mock = %rt.name, "gRPC call not reported: no runtime"),
        }
    }
}

/// Reports a call; to the request recorder only when its usage was
/// `metered` item by item.
async fn send_report(rt: &MockRuntime, headers: &HeaderMap, report: Report, metered: bool) {
    if metered {
        call::report_unmetered(rt, headers, report).await;
    } else {
        call::report(rt, headers, report).await;
    }
}

/// A response body that adds `x-mockingbird-source` to the trailers of a
/// successful call.
struct WithSource {
    inner: tonic::body::Body,
    call: Arc<CallState>,
}

impl http_body::Body for WithSource {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        let mut polled = ready!(Pin::new(&mut self.inner).poll_frame(cx));
        if let Some(Ok(frame)) = &mut polled
            && let Some(trailers) = frame.trailers_mut()
            && trailers.get("grpc-status").is_some_and(|v| v == "0")
            && let Some(source) = *self.call.source_slot()
        {
            trailers.insert(SOURCE_HEADER, source_value_header(source));
        }
        Poll::Ready(polled)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

fn source_value_header(source: Source) -> HeaderValue {
    HeaderValue::from_static(source.as_str())
}

fn source_value(source: Source) -> tonic::metadata::AsciiMetadataValue {
    tonic::metadata::AsciiMetadataValue::from_static(source.as_str())
}

async fn sleep(delay: Option<Duration>) {
    if let Some(d) = delay {
        tokio::time::sleep(d).await;
    }
}

fn engine_failure(e: EngineError) -> Failure {
    match e {
        EngineError::UndeclaredStatus(code) => Failure::new(
            400,
            format!("the operation declares no response for status {code}"),
        ),
        e => Failure::internal(e.to_string()),
    }
}

/// A request message as JSON (prost-reflect's default serde options).
fn message_json(message: &DynamicMessage) -> Value {
    serde_json::to_value(message).unwrap_or(Value::Null)
}

/// `json` as a `desc` message, after keeping only the first set member of
/// each oneof group (declared order), at every level. Returns the message
/// and the JSON it was built from.
fn to_message(
    desc: &MessageDescriptor,
    mut json: Value,
) -> Result<(DynamicMessage, Value), String> {
    first_oneof_members(desc, &mut json);
    let message = DynamicMessage::deserialize(desc.clone(), &json).map_err(|e| e.to_string())?;
    Ok((message, json))
}

/// The JSON keys a field may be written under.
fn field_keys(field: &prost_reflect::FieldDescriptor) -> Vec<String> {
    let mut keys = vec![field.json_name().to_string()];
    if field.name() != field.json_name() {
        keys.push(field.name().to_string());
    }
    keys
}

fn first_oneof_members(desc: &MessageDescriptor, value: &mut Value) {
    let Value::Object(obj) = value else {
        return;
    };
    for oneof in desc.oneofs().filter(|o| !o.is_synthetic()) {
        let mut kept = false;
        for field in oneof.fields() {
            for key in field_keys(&field) {
                if obj.get(&key).is_none_or(Value::is_null) {
                    continue;
                }
                if kept {
                    obj.remove(&key);
                } else {
                    kept = true;
                }
            }
        }
    }
    for field in desc.fields() {
        let Kind::Message(inner) = field.kind() else {
            continue;
        };
        for key in field_keys(&field) {
            let Some(v) = obj.get_mut(&key) else { continue };
            if field.is_map() {
                let value_kind = inner.map_entry_value_field().kind();
                if let (Kind::Message(m), Value::Object(entries)) = (value_kind, v) {
                    entries
                        .values_mut()
                        .for_each(|e| first_oneof_members(&m, e));
                }
            } else if field.is_list() {
                if let Value::Array(items) = v {
                    items
                        .iter_mut()
                        .for_each(|e| first_oneof_members(&inner, e));
                }
            } else {
                first_oneof_members(&inner, v);
            }
        }
    }
}

/// The last `keep` items of `items`, read to its end, in order. At most
/// `keep` items are held at any time, however long the stream is.
async fn last_items<T, E>(
    items: impl futures::Stream<Item = Result<T, E>>,
    keep: usize,
) -> Result<VecDeque<T>, E> {
    let mut kept = VecDeque::with_capacity(keep);
    let mut items = std::pin::pin!(items);
    while let Some(item) = items.next().await {
        let item = item?;
        if keep == 0 {
            continue;
        }
        if kept.len() == keep {
            kept.pop_front();
        }
        kept.push_back(item);
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {

    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// Counts live instances, and the most ever alive at once.
    struct Tracked(Arc<(AtomicUsize, AtomicUsize)>, usize);

    impl Tracked {
        fn new(counts: &Arc<(AtomicUsize, AtomicUsize)>, n: usize) -> Self {
            let live = counts.0.fetch_add(1, Ordering::SeqCst) + 1;
            counts.1.fetch_max(live, Ordering::SeqCst);
            Self(Arc::clone(counts), n)
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn client_stream_keeps_only_the_last_items_while_reading() {
        let counts = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
        let c = Arc::clone(&counts);
        let items = stream::iter(0..100_000).map(move |n| Ok::<_, Status>(Tracked::new(&c, n)));
        let kept = last_items(items, REQUEST_STREAM_ITEMS).await.unwrap();
        let numbers: Vec<usize> = kept.iter().map(|t| t.1).collect();
        let expected: Vec<usize> = (100_000 - REQUEST_STREAM_ITEMS..100_000).collect();
        assert_eq!(numbers, expected);
        let peak = counts.1.load(Ordering::SeqCst);
        assert!(
            peak <= REQUEST_STREAM_ITEMS + 1,
            "{peak} messages held at once"
        );

        let failing = stream::iter([Ok(1), Err(Status::internal("x"))]);
        assert!(last_items(failing, 2).await.is_err());
        let empty = stream::iter(Vec::<Result<u8, Status>>::new());
        assert!(last_items(empty, 2).await.unwrap().is_empty());
    }

    const ONEOFS: &str = r#"
syntax = "proto3";
package t;
message Outer {
  oneof choice { string a = 1; int32 b = 2; Inner c = 3; }
  repeated Inner items = 4;
  map<string, Inner> by_name = 5;
}
message Inner { oneof pick { string x = 1; string y = 2; } }
"#;

    fn outer() -> MessageDescriptor {
        let bundle = mb_import::compile(ONEOFS.as_bytes()).unwrap();
        let Some(ProtocolPayload::Grpc {
            file_descriptor_set,
        }) = bundle.protocol_payload
        else {
            panic!("no descriptors")
        };
        DescriptorPool::decode(file_descriptor_set.as_slice())
            .unwrap()
            .get_message_by_name("t.Outer")
            .unwrap()
    }

    #[test]
    fn keeps_first_oneof_member_at_every_level() {
        let desc = outer();
        let json = json!({
            "a": null,
            "b": 2,
            "c": {"x": "1"},
            "items": [{"x": "1", "y": "2"}],
            "byName": {"k": {"y": "2", "x": "1"}},
        });
        let (message, json) = to_message(&desc, json).unwrap();
        assert_eq!(
            json,
            json!({"a": null, "b": 2, "items": [{"x": "1"}], "byName": {"k": {"x": "1"}}})
        );
        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            json!({"b": 2, "items": [{"x": "1"}], "byName": {"k": {"x": "1"}}})
        );
    }

    #[test]
    fn unknown_fields_do_not_fit() {
        assert!(to_message(&outer(), json!({"nope": 1})).is_err());
    }
}
