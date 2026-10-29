//! `vault:` secret references against a wiremock'd Vault KV v2 API.

use std::collections::HashMap;
use std::time::Duration;

use mb_ext::{DefaultSecrets, ExtError, SecretStore, VaultSecrets};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Builds a `VaultSecrets` from an explicit variable map, so no test mutates
/// the process environment.
fn vault(vars: &[(&str, &str)]) -> VaultSecrets {
    let vars: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    match VaultSecrets::from_vars(|name| vars.get(name).cloned()) {
        Ok(Some(v)) => v,
        Ok(None) => panic!("VAULT_ADDR was set, expected a client"),
        Err(e) => panic!("building VaultSecrets: {e}"),
    }
}

fn kv2(data: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "data": { "data": data, "metadata": { "version": 3 } }
    }))
}

#[tokio::test]
async fn vault_reads_kv2_field() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/secret/data/mockingbird"))
        .and(header("X-Vault-Token", "root-token"))
        .and(header("X-Vault-Namespace", "team-a"))
        .respond_with(kv2(json!({ "openai_key": "sk-123", "other": "x" })))
        .expect(1)
        .mount(&server)
        .await;

    let store = vault(&[
        ("VAULT_ADDR", &server.uri()),
        ("VAULT_TOKEN", "root-token"),
        ("VAULT_NAMESPACE", "team-a"),
    ]);
    let value = store.resolve("vault:secret/mockingbird#openai_key").await;
    assert_eq!(value.ok().as_deref(), Some("sk-123"));
    // Served from the cache: the mock expects exactly one request.
    let again = store.resolve("vault:secret/mockingbird#openai_key").await;
    assert_eq!(again.ok().as_deref(), Some("sk-123"));
}

#[tokio::test]
async fn vault_missing_field_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/secret/data/mockingbird"))
        .respond_with(kv2(json!({ "other": "x" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/secret/data/absent"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "errors": [] })))
        .mount(&server)
        .await;

    let store = vault(&[("VAULT_ADDR", &server.uri()), ("VAULT_TOKEN", "t")]);
    let err = store.resolve("vault:secret/mockingbird#openai_key").await;
    assert!(matches!(err, Err(ExtError::SecretNotFound(_))), "{err:?}");
    let err = store.resolve("vault:secret/absent#openai_key").await;
    assert!(matches!(err, Err(ExtError::SecretNotFound(_))), "{err:?}");
}

#[tokio::test]
async fn vault_403_is_other_without_value() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/secret/data/mockingbird"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_string("permission denied sk-leaked-value root-token"),
        )
        .mount(&server)
        .await;

    let store = vault(&[("VAULT_ADDR", &server.uri()), ("VAULT_TOKEN", "root-token")]);
    let reference = "vault:secret/mockingbird#openai_key";
    match store.resolve(reference).await {
        Err(ExtError::Other(msg)) => {
            assert!(msg.contains(reference), "{msg}");
            assert!(!msg.contains("sk-leaked-value"), "{msg}");
            assert!(!msg.contains("root-token"), "{msg}");
        }
        other => panic!("expected ExtError::Other, got {other:?}"),
    }
}

#[tokio::test]
async fn vault_token_file_reread_after_cache_expiry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/secret/data/mockingbird"))
        .and(header("X-Vault-Token", "token-one"))
        .respond_with(kv2(json!({ "k": "value-one" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/secret/data/mockingbird"))
        .and(header("X-Vault-Token", "token-two"))
        .respond_with(kv2(json!({ "k": "value-two" })))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, "token-one\n").expect("write token");
    let token_path = token_file.display().to_string();
    let store = vault(&[
        ("VAULT_ADDR", &server.uri()),
        ("VAULT_TOKEN_FILE", &token_path),
    ]);
    let reference = "vault:secret/mockingbird#k";

    assert_eq!(
        store.resolve(reference).await.ok().as_deref(),
        Some("value-one")
    );
    // The token rotates on disk; the cached value is still served.
    std::fs::write(&token_file, "token-two\n").expect("rotate token");
    assert_eq!(
        store.resolve(reference).await.ok().as_deref(),
        Some("value-one")
    );

    // Let the 5-minute cache expire. Time is only paused while advancing, so
    // the real HTTP round trips below are not cut short by auto-advance.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(5 * 60 + 1)).await;
    tokio::time::resume();

    assert_eq!(
        store.resolve(reference).await.ok().as_deref(),
        Some("value-two")
    );
}

#[tokio::test]
async fn default_secrets_without_vault_addr_rejects_vault_refs() {
    let none = VaultSecrets::from_vars(|_| None);
    assert!(matches!(none, Ok(None)));

    let store = DefaultSecrets::from_vars(|_| None);
    let reference = "vault:secret/mockingbird#k";
    let err = store.resolve(reference).await;
    assert!(
        matches!(&err, Err(ExtError::BadSecretRef(r)) if r == reference),
        "{err:?}"
    );
    // env:/file: still work through the defaults.
    let path = std::env::var("PATH").expect("PATH is set");
    assert_eq!(store.resolve("env:PATH").await.ok(), Some(path));
}

#[tokio::test]
async fn malformed_vault_refs_are_bad_refs() {
    let store = vault(&[("VAULT_ADDR", "http://127.0.0.1:1"), ("VAULT_TOKEN", "t")]);
    for reference in [
        "vault:secret/mockingbird",
        "vault:secret#k",
        "vault:/mockingbird#k",
        "vault:secret/mockingbird#",
        "vault:secret/../x#k",
        "vault:secret//x#k",
    ] {
        let err = store.resolve(reference).await;
        assert!(
            matches!(&err, Err(ExtError::BadSecretRef(r)) if r == reference),
            "{reference}: {err:?}"
        );
    }
}

#[tokio::test]
async fn default_secrets_builds_vault_lazily_and_warns_once() {
    let counter = crate::support::EventCounter::default();
    let _guard = tracing::subscriber::set_default(counter.subscriber());
    // VAULT_ADDR set but no token: unusable, which eager construction would
    // have logged immediately.
    let store = DefaultSecrets::from_vars(|name| {
        (name == "VAULT_ADDR").then(|| "http://127.0.0.1:1".to_string())
    });
    assert_eq!(counter.count(), 0, "construction logged");

    let reference = "vault:secret/mockingbird#k";
    for _ in 0..3 {
        let err = store.resolve(reference).await;
        assert!(
            matches!(&err, Err(ExtError::BadSecretRef(r)) if r == reference),
            "{err:?}"
        );
    }
    assert_eq!(
        counter.count(),
        1,
        "the bad Vault env is logged exactly once"
    );
    // env:/file: are unaffected.
    let path = std::env::var("PATH").expect("PATH is set");
    assert_eq!(store.resolve("env:PATH").await.ok(), Some(path));
}

#[tokio::test]
async fn default_secrets_resolves_vault_refs_lazily() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/secret/data/mockingbird"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": {"data": {"k": "v"}}})),
        )
        .mount(&server)
        .await;
    let addr = server.uri();
    let store = DefaultSecrets::from_vars(|name| match name {
        "VAULT_ADDR" => Some(addr.clone()),
        "VAULT_TOKEN" => Some("t".to_string()),
        _ => None,
    });
    assert_eq!(
        store
            .resolve("vault:secret/mockingbird#k")
            .await
            .ok()
            .as_deref(),
        Some("v")
    );
}
