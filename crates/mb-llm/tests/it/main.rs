//! mockingbird-llm integration tests in one binary: one module per former
//! `tests/<name>.rs` file, so the test paths are `<module>::<test>`.
//!
//! `fallback` and `scripted` drive `ScriptedProvider`, which needs the
//! `testing` feature; without it they compile out, as their old
//! `required-features` binaries did.

#[cfg(feature = "testing")]
mod fallback;
mod openai;
#[cfg(feature = "testing")]
mod scripted;
