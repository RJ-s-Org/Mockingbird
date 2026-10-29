//! The mockingbird-admin integration tests, in one binary: one module per
//! area (the former `tests/<name>.rs` files), so the crate and its
//! dependencies are linked once instead of once per file.
//!
//! None of the modules touch process-global state (environment variables,
//! the working directory, a global tracing subscriber or metrics recorder;
//! the metrics tests use `metrics::with_local_recorder`), so they can share
//! a process under `cargo test`.

mod support;

mod api;
mod apikeys;
mod audit;
mod auth_flow;
mod channels;
mod channels_workspace;
mod cross_phase;
mod drafts;
mod imports;
mod live;
mod login_limit;
mod mocks;
mod perf;
mod rbac;
mod scenarios;
mod webhook_deliveries;
mod workspaces;
