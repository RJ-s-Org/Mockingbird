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

mod alternate_keys;
mod api;
mod backends;
mod base64_fields;
mod call;
mod concurrency;
mod concurrency_evidence;
mod concurrency_regressions;
mod concurrency_regressions2;
mod contract_types;
mod create_refusals;
mod create_refusals_enforced;
mod create_render_errors;
mod create_repeats;
mod create_starts;
mod create_target_overlay;
mod data_plane_auth;
mod data_plane_auth_protocols;
mod default_errors;
mod described_actions_partial;
mod described_conflicts;
mod described_derived;
mod described_identity;
mod described_kinds;
mod described_lifecycle;
mod described_lists;
mod described_runtime;
mod described_state_lists;
mod effect_members;
mod engine_error_bodies;
mod envelope_parents;
mod envelope_writes;
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
mod graphql_creates;
mod graphql_def_effects;
mod graphql_links;
mod graphql_nested;
mod graphql_references;
mod graphql_relay_inputs;
mod graphql_ws;
mod grpc;
mod header_identity;
mod held_nested;
mod http_media;
mod http_rest;
mod identity_by_contract;
mod if_none_match_range;
mod in_segment_keys;
mod inferred_requests;
mod issued_aliases;
mod issued_scopes;
mod language_variants;
mod nested_identity_patches;
mod optional_null;
mod partial_creates;
mod provider_policy;
mod references_by_value;
mod related_writes;
mod repro234;
mod routing;
mod runtime;
mod seeded_mints;
mod shared_state;
mod sibling_writes;
mod skeletal_answers;
mod soap;
mod sse;
mod state_reads;
mod structural_ties;
mod structured_fill;
mod unknown_ids_inferred;
mod url_members;
mod webhooks;
mod world;
mod world_computed_effects;
mod world_expressions;
mod world_gaps;
mod world_list_exprs;
mod world_lists;
mod write_memory;
mod writes_stick;
mod ws;
