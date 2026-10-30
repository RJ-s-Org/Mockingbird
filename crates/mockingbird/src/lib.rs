//! The library behind the `mockingbird` command.
//!
//! [`mockingbird-cli`](https://crates.io/crates/mockingbird-cli) installs
//! the `mockingbird` binary: it serves LLM-backed mocks from API contracts
//! ([`server`], [`config`]), runs the intercepting proxy ([`proxy`]) and the
//! MCP server ([`mcp`]), and drives a running server through its Admin API
//! ([`client`]). This library exists for that binary and its integration
//! tests: its modules are the binary's internals, not a designed embedding
//! API, and they change as the binary needs (within semver). To build on
//! Mockingbird, run the server or use the `mockingbird-*` crates it is made
//! of.
//!
//! ```
//! use mockingbird::file_config::Role;
//! use mockingbird::server::Binds;
//!
//! let role: Role = "control".parse()?;
//! assert_eq!(role.to_string(), "control");
//! // A control node serves the Admin API only.
//! assert_eq!(
//!     Binds::for_role(role),
//!     Binds { data: false, admin: true, grpc: false },
//! );
//! # Ok::<(), String>(())
//! ```
//!
//! # Features
//!
//! - `mqtt` and `amqp` (default): the MQTT and AMQP brokers.
//! - `kafka`: the Kafka broker.
//! - `full`: every broker.
#![warn(
    missing_debug_implementations,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

pub mod agents;
pub mod budget;
pub mod client;
pub mod config;
pub mod daemon;
pub mod dp_keys;
pub mod events;
pub mod examples;
pub mod file_config;
pub mod ha;
pub mod host_guard;
pub mod import_llm;
pub mod keys;
pub mod maintenance;
pub mod mcp;
pub mod onboard;
pub mod ops;
pub mod providers;
pub mod proxy;
pub mod reconcile;
pub mod recorder;
pub mod registry;
pub mod report;
pub mod run;
pub mod runtime;
pub mod server;
pub mod shell_env;
pub mod snapshot;
pub mod telemetry;
pub mod tls;
pub mod watch;
