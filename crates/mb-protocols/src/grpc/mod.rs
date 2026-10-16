//! gRPC adapter: serves a bundle's protobuf services from the descriptor
//! set the importer stored in the bundle, with dynamic messages.
//!
//! Every published mock shares one [`GrpcServer`] (an h2c listener), which
//! routes each call by its `/<service>/<method>` path and also answers
//! `grpc.health.v1.Health` and server reflection (v1 and v1alpha). A call
//! runs through the shared [`call`](crate::call) pipeline; see
//! [`GrpcMock`] for how each call kind maps onto the engine.

mod codec;
mod server;
mod service;
mod status;

pub use codec::DynamicCodec;
pub use server::GrpcServer;
pub use service::GrpcMock;
pub use status::grpc_code_for_status;

/// Why a mock cannot be served, or a set of mocks published, over gRPC.
#[derive(Debug, thiserror::Error)]
pub enum GrpcError {
    #[error("bundle has no protobuf descriptors")]
    MissingDescriptors,
    #[error("invalid descriptors: {0}")]
    Descriptors(String),
    #[error("service {service} is already served by mock {holder}")]
    ServiceCollision { service: String, holder: String },
}
