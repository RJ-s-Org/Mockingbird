//! Helpers shared by several test modules.
#![cfg(test)]

use std::path::PathBuf;

/// Reads `fixtures/<rel>` from the repository root.
pub fn fixture(rel: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel);
    std::fs::read(p).unwrap()
}

/// An extraction LLM over `provider` with a 5 s timeout and 1000 tokens.
#[cfg(feature = "smart")]
pub fn llm(provider: std::sync::Arc<mb_llm::ScriptedProvider>) -> mb_import::smart::ExtractLlm {
    mb_import::smart::ExtractLlm {
        provider,
        settings: mb_llm::HopSettings {
            timeout: std::time::Duration::from_secs(5),
            temperature: 0.1,
            max_tokens: 1000,
        },
    }
}
