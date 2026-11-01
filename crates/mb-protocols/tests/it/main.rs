//! The integration tests of mockingbird-protocols, in one test binary.
//!
//! Each module is a former `tests/<name>.rs` binary and keeps its name, so a
//! test's path is `<module>::<test>` (run one module with
//! `cargo nextest run -p mockingbird-protocols -E 'test(/^grpc::/)'`). One
//! binary links the crate and its dependencies once instead of once per file.
//!
//! `tests/spans.rs` stays its own binary: it installs a tracing subscriber,
//! and tracing caches each callsite's interest process-wide, so a span first
//! hit by another test's thread could race the subscriber's registration.
//!
//! `#![cfg(test)]` (always true here) puts the helpers under clippy's
//! test-code settings, which allow `unwrap` and `expect`.
#![cfg(test)]

mod support;

mod api;
mod call;
mod data_plane_auth;
mod data_plane_auth_protocols;
#[cfg(feature = "amqp")]
mod events_amqp;
mod events_broker;
#[cfg(feature = "kafka")]
mod events_kafka;
#[cfg(feature = "mqtt")]
mod events_mqtt;
mod events_policy;
mod events_runtime;
mod events_triggers;
mod events_ws;
mod graphql;
mod graphql_ws;
mod grpc;
mod http_media;
mod http_rest;
mod learned_identity;
mod provider_policy;
mod runtime;
mod soap;
mod sse;
mod webhooks;
mod write_memory;
mod writes_stick;
mod ws;
