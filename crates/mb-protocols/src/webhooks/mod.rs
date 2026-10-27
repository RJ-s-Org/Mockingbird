//! Outbound webhooks: after a successful write a mock POSTs a signed event
//! to each configured target, retrying failures, and keeps a bounded log of
//! the deliveries.

pub mod deliver;
#[cfg_attr(
    not(any(
        feature = "http-rest",
        feature = "graphql",
        feature = "grpc",
        feature = "soap"
    )),
    allow(dead_code)
)]
pub(crate) mod emit;
pub mod event;
pub mod log;
pub mod sign;
