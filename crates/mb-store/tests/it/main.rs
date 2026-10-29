//! mockingbird-store integration tests in one binary: one module per former
//! `tests/<name>.rs` file, so the test paths are `<module>::<test>`.
#![cfg(test)]

mod conformance;
mod postgres;
mod sqlite;
mod support;
mod upgrade;
