//! The integration tests of mb-engine, in one binary: one module per former
//! `tests/<name>.rs`, so test paths read `<name>::<test>`.
//!
//! No module changes process-global state (environment variables, the
//! current directory, a global tracing subscriber or metrics recorder), so
//! they share one process safely under `cargo test`.
// `cfg(test)` marks the helpers as test code for clippy (`allow-expect-in-tests`).
#![cfg(test)]

mod budget;
mod debug;
mod faker;
mod generate;
mod guards;
mod message;
mod perf;
mod pii;
mod scenario;
mod stream;
mod support;
