//! The integration tests of mb-state, in one binary: one module per former
//! `tests/<name>.rs`, so test paths read `<name>::<test>`.
//!
//! No module changes process-global state (environment variables, the
//! current directory, a global tracing subscriber or metrics recorder), so
//! they share one process safely under `cargo test`. The `bus` tests that
//! kill listener connections server-wide still serialise on their own lock;
//! nextest runs them in the `cross-node` test group.
// `cfg(test)` marks the helpers as test code for clippy (`allow-expect-in-tests`).
#![cfg(test)]

mod bus;
mod cache;
mod conformance;
mod leader;
mod memory;
mod redis;
mod sql;
mod support;
