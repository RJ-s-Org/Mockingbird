//! The integration tests of mb-mcm, in one binary: one module per former
//! `tests/<name>.rs`, so test paths read `<name>::<test>`.
// `cfg(test)` marks the helpers as test code for clippy (`allow-expect-in-tests`).
#![cfg(test)]

mod model;
