//! `Extensions::default()` with a (broken) Vault environment does no I/O and
//! logs nothing: the Vault client is only built on the first `vault:` resolve.
//!
//! This is its own test binary with a single test because it sets process
//! environment variables (`set_var` would race with the other tests' threads
//! under `cargo test`), so it is not a module of `tests/it`.

#[path = "it/support.rs"]
mod support;

use mb_ext::{ExtError, Extensions};
use support::EventCounter;

#[test]
fn extensions_default_with_vault_addr_does_no_io_and_logs_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing_ca = dir.path().join("no-such-ca.pem");
    // SAFETY: this binary has exactly one test and sets the variables before
    // any other thread (runtime, subscriber) exists.
    unsafe {
        std::env::set_var("VAULT_ADDR", "http://127.0.0.1:1");
        std::env::set_var("VAULT_TOKEN", "t");
        // An unreadable CA bundle: eager construction would fail to read it
        // and warn.
        std::env::set_var("VAULT_CACERT", &missing_ca);
    }

    let counter = EventCounter::default();
    let ext = tracing::subscriber::with_default(counter.subscriber(), || {
        let exts: Vec<Extensions> = (0..10).map(|_| Extensions::default()).collect();
        exts.into_iter().next().expect("ten defaults")
    });
    assert_eq!(counter.count(), 0, "Extensions::default() logged");

    // The CA file is only touched (and the failure logged, once) on the
    // first vault: resolve.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = tracing::subscriber::set_default(counter.subscriber());
    for _ in 0..3 {
        let err = rt.block_on(ext.secrets.resolve("vault:secret/app#k"));
        assert!(
            matches!(&err, Err(ExtError::Other(m)) if m.contains("Vault is disabled") && m.contains("VAULT_CACERT")),
            "{err:?}"
        );
    }
    assert_eq!(
        counter.count(),
        1,
        "the bad Vault env is logged exactly once"
    );
}
