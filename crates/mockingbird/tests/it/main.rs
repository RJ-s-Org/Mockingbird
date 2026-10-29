//! The integration tests of mockingbird-cli, linked into one binary: each
//! module was a `tests/<name>.rs` binary of its own and keeps that name, so
//! a test's path is `<module>::<test>` (`cargo test -p mockingbird-cli
//! --test it -- tls::`). Shared helpers live in [`support`].
//!
//! The tests share one process under `cargo test` (nextest runs each in its
//! own), so none may change process-global state: environment variables,
//! the current directory, or a global tracing subscriber. A test that must
//! goes in a binary of its own next to this directory
//! (`tests/telemetry_init.rs`).

mod support;

mod agent_toolkit;
mod auth_serve;
mod chaos;
mod ci;
mod client;
mod config;
mod cross_phase;
mod deploy_artifacts;
mod docs_reference;
mod e2e;
mod events_e2e;
mod events_workspaces;
mod examples_catalog;
mod ha;
mod ha_cluster;
mod import_llm;
mod limits;
mod observability;
mod protocols_e2e;
mod proxy;
mod registry;
mod release_workflow;
mod repo_community;
mod roles;
mod run;
mod scenarios_e2e;
mod shared_ext;
mod smart_import;
mod tls;
mod webhooks_e2e;
mod workspaces;
