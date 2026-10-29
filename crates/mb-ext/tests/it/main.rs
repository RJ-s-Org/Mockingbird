//! mb-ext integration tests in one binary: one module per former
//! `tests/<name>.rs` file, so the test paths are `<module>::<test>`.
//!
//! `tests/default_extensions_vault_env.rs` stays a binary of its own: it sets
//! process environment variables and must be alone in its process.

mod defaults;
mod support;
mod vault;
