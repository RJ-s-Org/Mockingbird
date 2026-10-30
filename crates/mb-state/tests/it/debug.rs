//! `Debug` output of the public types: present, and free of the secrets the
//! values carry.

use std::time::Duration;

use mb_state::{LeaderBackend, LocalBus, MemoryBusHub, MemoryStore, ResponseCache};

const SECRET: &str = "s3cr3t-hunter2";

#[test]
fn leader_backend_debug_redacts_url_credentials() {
    for backend in [
        LeaderBackend::Postgres(format!("postgres://admin:{SECRET}@db.internal:5432/mb")),
        LeaderBackend::Redis(format!("rediss://default:{SECRET}@cache.internal:6380/0")),
        LeaderBackend::Redis(format!("redis://{SECRET}@cache.internal")),
        LeaderBackend::Postgres(format!("postgres://db.internal/mb?password={SECRET}")),
        LeaderBackend::Postgres(format!("host=db.internal password={SECRET}")),
    ] {
        let text = format!("{backend:?}");
        assert!(!text.contains(SECRET), "{text}");
    }
    let text = format!(
        "{:?}",
        LeaderBackend::Postgres(format!("postgres://admin:{SECRET}@db.internal:5432/mb"))
    );
    assert!(text.contains("db.internal:5432/mb"), "{text}");
    assert_eq!(format!("{:?}", LeaderBackend::Always), "Always");
}

#[test]
fn leader_backend_debug_redacts_passwords_with_reserved_characters() {
    for url in [
        format!("redis://u:{SECRET}/x@y@cache.internal/0"),
        format!("redis://u:{SECRET}?x@cache.internal/0"),
        format!("redis://u:{SECRET}#x@cache.internal/0"),
    ] {
        let text = format!("{:?}", LeaderBackend::Redis(url));
        assert!(!text.contains(SECRET), "{text}");
        assert!(text.contains("***@cache.internal/0"), "{text}");
    }
}

#[tokio::test]
async fn stores_and_buses_implement_debug() {
    let store = MemoryStore::new(Duration::from_secs(60));
    assert!(format!("{store:?}").starts_with("MemoryStore"));
    let cache = ResponseCache::new(4, Duration::from_secs(60));
    assert!(format!("{cache:?}").starts_with("ResponseCache"));
    let bus = LocalBus::new_shared("node-a".into());
    assert!(format!("{bus:?}").contains("node-a"));
    let hub = MemoryBusHub::new_shared();
    assert!(format!("{hub:?}").starts_with("MemoryBusHub"));
}
