//! The shared h2c server: routes calls to the published mocks and serves
//! health and reflection.

use std::collections::{BTreeMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use prost::Message;
use prost_types::FileDescriptorSet;
use tonic::Status;
use tonic::codegen::Service;
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

use super::{GrpcError, GrpcMock};

const HEALTH_SERVICE: &str = "grpc.health.v1.Health";
const REFLECTION_V1: &str = "grpc.reflection.v1.ServerReflection";
const REFLECTION_V1ALPHA: &str = "grpc.reflection.v1alpha.ServerReflection";

/// One h2c endpoint serving every published gRPC mock, plus
/// `grpc.health.v1.Health` and server reflection (v1 and v1alpha).
pub struct GrpcServer {
    published: RwLock<Arc<Published>>,
    /// Serializes [`GrpcServer::publish`], so health updates do not race.
    publishing: tokio::sync::Mutex<()>,
    health: HealthReporter,
    health_service: Router,
}

/// What one [`GrpcServer::publish`] installed.
struct Published {
    /// Service full name → the mock serving it.
    by_service: BTreeMap<String, Arc<GrpcMock>>,
    /// Both reflection versions; `None` if they could not be built.
    reflection: Option<Router>,
}

impl GrpcServer {
    /// A server publishing no mocks; health reports `""` as `SERVING`.
    pub fn new() -> Arc<Self> {
        let (health, health_server) = tonic_health::server::health_reporter();
        let reflection = match reflection(&[]) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::error!(error = %e, "cannot build gRPC reflection");
                None
            }
        };
        Arc::new(Self {
            published: RwLock::new(Arc::new(Published {
                by_service: BTreeMap::new(),
                reflection,
            })),
            publishing: tokio::sync::Mutex::new(()),
            health,
            health_service: Router::new().fallback_service(health_server),
        })
    }

    /// Replaces the published mocks. Fails, changing nothing, when two mocks
    /// serve the same service or their descriptors cannot be reflected.
    /// Health then reports `""` and every published service as `SERVING`,
    /// and services no longer published as `NOT_SERVING`.
    pub async fn publish(&self, mocks: Vec<Arc<GrpcMock>>) -> Result<(), GrpcError> {
        let _publishing = self.publishing.lock().await;
        let mut by_service: BTreeMap<String, Arc<GrpcMock>> = BTreeMap::new();
        for mock in &mocks {
            for service in mock.services() {
                if let Some(holder) = by_service.get(&service) {
                    return Err(GrpcError::ServiceCollision {
                        service,
                        holder: holder.name().to_string(),
                    });
                }
                by_service.insert(service, Arc::clone(mock));
            }
        }
        let reflection = reflection(&mocks)?;
        let new = Arc::new(Published {
            by_service,
            reflection: Some(reflection),
        });
        let old = std::mem::replace(&mut *self.write(), Arc::clone(&new));

        self.health
            .set_service_status("", ServingStatus::Serving)
            .await;
        for service in new.by_service.keys() {
            self.health
                .set_service_status(service, ServingStatus::Serving)
                .await;
        }
        for service in old.by_service.keys() {
            if !new.by_service.contains_key(service) {
                self.health
                    .set_service_status(service, ServingStatus::NotServing)
                    .await;
            }
        }
        Ok(())
    }

    /// A router answering every gRPC call: health, reflection, then the
    /// published mocks by service; anything else is `UNIMPLEMENTED`, and a
    /// request that is not `application/grpc*` is a `415`.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .fallback(dispatch)
            .with_state(Arc::clone(self))
    }

    /// `(service, mock)` for every published service, sorted.
    pub fn services(&self) -> Vec<(String, String)> {
        self.current()
            .by_service
            .iter()
            .map(|(service, mock)| (service.clone(), mock.name().to_string()))
            .collect()
    }

    fn current(&self) -> Arc<Published> {
        Arc::clone(&self.published.read().unwrap_or_else(|p| p.into_inner()))
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Arc<Published>> {
        self.published.write().unwrap_or_else(|p| p.into_inner())
    }
}

async fn dispatch(State(server): State<Arc<GrpcServer>>, req: Request) -> Response {
    if !is_grpc(req.headers()) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "gRPC requests must have an application/grpc content type",
        )
            .into_response();
    }
    let path = req.uri().path().to_string();
    let service = path
        .strip_prefix('/')
        .and_then(|p| p.split_once('/'))
        .map_or("", |(service, _)| service);
    match service {
        HEALTH_SERVICE => call(server.health_service.clone(), req).await,
        REFLECTION_V1 | REFLECTION_V1ALPHA => match server.current().reflection.clone() {
            Some(reflection) => call(reflection, req).await,
            None => unimplemented(&path),
        },
        _ => {
            let mock = server.current().by_service.get(service).cloned();
            match mock {
                Some(mock) => mock.handle(req).await,
                None => unimplemented(&path),
            }
        }
    }
}

fn is_grpc(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("application/grpc"))
}

fn unimplemented(path: &str) -> Response {
    Status::unimplemented(format!("unknown method {path}")).into_http()
}

async fn call(mut router: Router, req: Request<Body>) -> Response {
    let ready: Result<(), Infallible> =
        std::future::poll_fn(|cx| <Router as Service<Request<Body>>>::poll_ready(&mut router, cx))
            .await;
    match ready {
        Ok(()) => match router.call(req).await {
            Ok(resp) => resp,
            Err(never) => match never {},
        },
        Err(never) => match never {},
    }
}

/// Reflection (v1 and v1alpha) over the union of the mocks' descriptor
/// sets, deduplicated by file name (first wins), plus the health service.
fn reflection(mocks: &[Arc<GrpcMock>]) -> Result<Router, GrpcError> {
    let mut seen = HashSet::new();
    let mut union = FileDescriptorSet::default();
    let sets = mocks
        .iter()
        .map(|m| m.descriptor_set())
        .chain([tonic_health::pb::FILE_DESCRIPTOR_SET]);
    for bytes in sets {
        let set =
            FileDescriptorSet::decode(bytes).map_err(|e| GrpcError::Descriptors(e.to_string()))?;
        for file in set.file {
            if seen.insert(file.name.clone().unwrap_or_default()) {
                union.file.push(file);
            }
        }
    }
    let builder = || {
        tonic_reflection::server::Builder::configure().register_file_descriptor_set(union.clone())
    };
    let v1 = builder()
        .build_v1()
        .map_err(|e| GrpcError::Descriptors(e.to_string()))?;
    let v1alpha = builder()
        .build_v1alpha()
        .map_err(|e| GrpcError::Descriptors(e.to_string()))?;
    Ok(Router::new()
        .route_service(&format!("/{REFLECTION_V1}/{{*method}}"), v1)
        .route_service(&format!("/{REFLECTION_V1ALPHA}/{{*method}}"), v1alpha))
}
