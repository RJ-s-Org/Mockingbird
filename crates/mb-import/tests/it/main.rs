//! The integration tests of `mockingbird-import`, in one test binary.
//!
//! Each module is a former `tests/<name>.rs` binary and keeps its name, so a
//! test's path is `<module>::<test>`. The smart-import modules need the
//! `smart` feature; the contract importers do not. No module touches
//! process-global state (environment, cwd, global subscribers), so they can
//! share one process under `cargo test`.

mod support;

mod archive;
mod asyncapi;
mod base_paths;
mod external_refs;
mod graphql;
mod openapi3;
mod perf;
mod proto;
mod security;
mod swagger2;
mod wsdl;

#[cfg(feature = "smart")]
mod smart_complete;
#[cfg(feature = "smart")]
mod smart_embedded;
#[cfg(feature = "smart")]
mod smart_fetch;
#[cfg(feature = "smart")]
mod smart_har;
#[cfg(feature = "smart")]
mod smart_import;
#[cfg(feature = "smart")]
mod smart_infer;
#[cfg(feature = "smart")]
mod smart_llm;
#[cfg(feature = "smart")]
mod smart_postman;
#[cfg(feature = "smart")]
mod smart_text;
#[cfg(feature = "smart")]
mod smart_url_refs;
