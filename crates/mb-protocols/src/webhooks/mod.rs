//! Outbound webhooks: after a successful write a mock POSTs a signed event
//! to each configured target, retrying failures, and keeps a bounded log of
//! the deliveries.

pub mod event;
pub mod sign;
