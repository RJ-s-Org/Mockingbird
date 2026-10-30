//! Outbound webhooks: after a successful write a mock POSTs a signed event
//! to each configured target, retrying failures, and keeps a bounded log of
//! the deliveries.

pub mod deliver;
#[cfg(any(
    feature = "http-rest",
    feature = "graphql",
    feature = "grpc",
    feature = "soap"
))]
pub(crate) mod emit;
pub mod event;
pub mod log;
pub mod sign;
