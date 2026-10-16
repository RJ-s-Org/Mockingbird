//! The shared h2c server: routes calls to the published mocks and serves
//! health and reflection.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex, RwLock};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use prost::Message;
use prost_types::{FileDescriptorProto, FileDescriptorSet};
use tonic::Status;
use tonic::codegen::Service;
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

use super::{GrpcError, GrpcMock};
use crate::DataPlaneAuthMode;
use crate::dp_auth::{authenticate_contract, presented_key};

const HEALTH_SERVICE: &str = "grpc.health.v1.Health";
const REFLECTION_V1: &str = "grpc.reflection.v1.ServerReflection";
const REFLECTION_V1ALPHA: &str = "grpc.reflection.v1alpha.ServerReflection";

/// One h2c endpoint serving every published gRPC mock, plus
/// `grpc.health.v1.Health` and server reflection (v1 and v1alpha), which
/// lists only the mocks the caller's data-plane credentials reach.
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
    /// The published mocks, in publish order.
    mocks: Vec<Arc<GrpcMock>>,
    /// Reflection (both versions) by the indexes in `mocks` a caller may
    /// see; the entry for every mock is built at publish, so a publish
    /// whose descriptors cannot be reflected fails.
    reflection: Mutex<HashMap<Vec<usize>, Router>>,
}

impl Published {
    fn new(by_service: BTreeMap<String, Arc<GrpcMock>>, mocks: Vec<Arc<GrpcMock>>) -> Self {
        Self {
            by_service,
            mocks,
            reflection: Mutex::new(HashMap::new()),
        }
    }

    /// Reflection over the mocks the caller may see (data-plane auth,
    /// spec §11.2): every mock without data-plane auth; `api_key` mocks
    /// whose key header carries a valid key of their workspace;
    /// `passthrough_validate` mocks when the credentials of one of their
    /// operations are present. When any published mock is in `api_key`
    /// mode, the caller must present a valid key for at least one of them
    /// (`UNAUTHENTICATED` otherwise).
    async fn reflection_for(&self, headers: &HeaderMap) -> Result<Router, Status> {
        let mut visible = Vec::new();
        let mut key_required = false;
        let mut key_accepted = false;
        // (workspace, key) → verified, so one key is checked once.
        let mut verified: HashMap<(String, String), bool> = HashMap::new();
        for (i, mock) in self.mocks.iter().enumerate() {
            let rt = mock.runtime();
            let allowed = match rt.data_plane_auth.mode {
                DataPlaneAuthMode::None => true,
                DataPlaneAuthMode::ApiKey => {
                    key_required = true;
                    let ok = match (presented_key(rt, headers, &[]), &rt.data_plane_keys) {
                        (Some(key), Some(keys)) => {
                            let slot = (rt.workspace.clone(), key.to_string());
                            match verified.get(&slot) {
                                Some(ok) => *ok,
                                None => {
                                    let ok = keys.verify(&rt.workspace, key).await;
                                    verified.insert(slot, ok);
                                    ok
                                }
                            }
                        }
                        _ => false,
                    };
                    key_accepted |= ok;
                    ok
                }
                DataPlaneAuthMode::PassthroughValidate => {
                    authenticate_contract(rt, headers, &[]).await.is_ok()
                }
            };
            if allowed {
                visible.push(i);
            }
        }
        if key_required && !key_accepted {
            return Err(Status::unauthenticated(
                "reflection requires a valid data-plane API key",
            ));
        }
        self.reflection_of(visible)
    }

    /// Reflection over the mocks at `visible`, built once per set.
    fn reflection_of(&self, visible: Vec<usize>) -> Result<Router, Status> {
        let mut cache = self.reflection.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(router) = cache.get(&visible) {
            return Ok(router.clone());
        }
        let mocks: Vec<Arc<GrpcMock>> = visible
            .iter()
            .filter_map(|&i| self.mocks.get(i).cloned())
            .collect();
        let router = reflection(&mocks).map_err(|e| {
            tracing::error!(error = %e, "cannot build gRPC reflection");
            Status::internal("reflection is unavailable")
        })?;
        cache.insert(visible, router.clone());
        Ok(router)
    }
}

impl GrpcServer {
    /// A server publishing no mocks; health reports `""` as `SERVING`.
    pub fn new() -> Arc<Self> {
        let (health, health_server) = tonic_health::server::health_reporter();
        Arc::new(Self {
            published: RwLock::new(Arc::new(Published::new(BTreeMap::new(), Vec::new()))),
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
        let all = reflection(&mocks)?;
        let new = Published::new(by_service, mocks);
        new.reflection
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert((0..new.mocks.len()).collect(), all);
        let new = Arc::new(new);
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

/// The union of each mock's descriptor set. A file whose name is already
/// taken by a file with other content (two single-file uploads are both
/// `main.proto`) is renamed `<mock>/<name>`, as is every file of that set
/// importing a renamed file, and imports within the set are rewritten to
/// match; identical files are kept once.
fn union_descriptor_sets(sets: Vec<(String, FileDescriptorSet)>) -> FileDescriptorSet {
    let mut union = FileDescriptorSet::default();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (mock, set) in sets {
        let differs = |f: &FileDescriptorProto| {
            index
                .get(f.name())
                .is_some_and(|&i| union.file.get(i) != Some(f))
        };
        let mut renamed: HashSet<String> = set
            .file
            .iter()
            .filter(|f| differs(f))
            .map(|f| f.name().to_string())
            .collect();
        // A file importing a renamed file changes too.
        loop {
            let before = renamed.len();
            for f in &set.file {
                if f.dependency.iter().any(|d| renamed.contains(d)) {
                    renamed.insert(f.name().to_string());
                }
            }
            if renamed.len() == before {
                break;
            }
        }
        let rename = |name: &str| format!("{mock}/{name}");
        for mut file in set.file {
            if renamed.contains(file.name()) {
                let new = rename(file.name());
                tracing::info!(
                    mock = %mock,
                    file = file.name(),
                    renamed = %new,
                    "gRPC reflection: renamed a descriptor file whose name another mock uses"
                );
                file.name = Some(new);
            }
            for dep in &mut file.dependency {
                if renamed.contains(dep.as_str()) {
                    *dep = rename(dep);
                }
            }
            match index.get(file.name()) {
                Some(&i) if union.file.get(i) == Some(&file) => {}
                Some(_) => tracing::warn!(
                    mock = %mock,
                    file = file.name(),
                    "gRPC reflection: two descriptor files share a name; the first is reflected"
                ),
                None => {
                    index.insert(file.name().to_string(), union.file.len());
                    union.file.push(file);
                }
            }
        }
    }
    union
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
        REFLECTION_V1 | REFLECTION_V1ALPHA => {
            let published = server.current();
            match published.reflection_for(req.headers()).await {
                Ok(reflection) => call(reflection, req).await,
                Err(status) => status.into_http(),
            }
        }
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
/// sets (see [`union_descriptor_sets`]), plus the health service.
fn reflection(mocks: &[Arc<GrpcMock>]) -> Result<Router, GrpcError> {
    let decode = |bytes: &[u8]| {
        FileDescriptorSet::decode(bytes).map_err(|e| GrpcError::Descriptors(e.to_string()))
    };
    let mut sets = Vec::with_capacity(mocks.len() + 1);
    for mock in mocks {
        sets.push((mock.name().to_string(), decode(mock.descriptor_set())?));
    }
    sets.push((
        String::new(),
        decode(tonic_health::pb::FILE_DESCRIPTOR_SET)?,
    ));
    let union = union_descriptor_sets(sets);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, package: &str, deps: &[&str]) -> FileDescriptorProto {
        FileDescriptorProto {
            name: Some(name.into()),
            package: Some(package.into()),
            dependency: deps.iter().map(|d| d.to_string()).collect(),
            ..Default::default()
        }
    }

    fn set(files: Vec<FileDescriptorProto>) -> FileDescriptorSet {
        FileDescriptorSet { file: files }
    }

    #[test]
    fn union_renames_clashing_files_and_their_importers() {
        let shared = file("google/protobuf/empty.proto", "google.protobuf", &[]);
        let a = set(vec![
            shared.clone(),
            file("common.proto", "a.common", &[]),
            file(
                "main.proto",
                "a",
                &["common.proto", "google/protobuf/empty.proto"],
            ),
        ]);
        let b = set(vec![
            shared.clone(),
            file("common.proto", "b.common", &[]),
            file(
                "main.proto",
                "a",
                &["common.proto", "google/protobuf/empty.proto"],
            ),
            file("other.proto", "b.other", &["main.proto"]),
        ]);
        let union = union_descriptor_sets(vec![("a".into(), a), ("b".into(), b)]);
        let names: Vec<&str> = union.file.iter().map(|f| f.name()).collect();
        assert_eq!(
            names,
            [
                "google/protobuf/empty.proto",
                "common.proto",
                "main.proto",
                "b/common.proto",
                "b/main.proto",
                "b/other.proto",
            ]
        );
        // b's main.proto has the same bytes as a's but imports b's
        // common.proto, so it is renamed too, and the imports follow.
        assert_eq!(
            union.file[4].dependency,
            ["b/common.proto", "google/protobuf/empty.proto"]
        );
        assert_eq!(union.file[5].dependency, ["b/main.proto"]);
        assert_eq!(
            union.file[2].dependency,
            ["common.proto", "google/protobuf/empty.proto"]
        );
    }
}
