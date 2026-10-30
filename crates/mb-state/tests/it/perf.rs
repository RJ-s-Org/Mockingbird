//! Scaling harnesses for the state hot paths, ignored by default. Run with
//! `cargo test -p mockingbird-state --test it perf:: -- --ignored --nocapture`
//! and compare the time per call at N and 10N.

use std::time::{Duration, Instant};

use mb_state::{
    CachedResponse, MemoryStore, ResponseCache, SessionKey, StateStore, StoredResource,
};
use serde_json::json;

fn key() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "perf".into(),
        session: "a".into(),
    }
}

const TYPES: usize = 10;

/// `MemoryStore::list` of 20 items of one type, and `get`, in a session
/// holding `n` resources spread over ten types.
#[ignore = "timing harness"]
#[tokio::test]
async fn memory_list_scaling() {
    for n in [1_000, 10_000] {
        let st = MemoryStore::new(Duration::from_secs(3600));
        let k = key();
        for i in 0..n {
            st.upsert(
                &k,
                StoredResource {
                    resource_type: format!("t{}", i % TYPES),
                    id: i.to_string(),
                    data: json!({"i": i}),
                },
            )
            .await
            .expect("upsert");
        }
        let calls: u32 = 500;
        let types: Vec<String> = (0..TYPES).map(|t| format!("t{t}")).collect();
        let start = Instant::now();
        for c in 0..calls {
            let l = st
                .list(&k, &types[c as usize % TYPES], 20)
                .await
                .expect("list");
            assert_eq!(l.len(), 20);
        }
        let per_list = start.elapsed() / calls;
        let start = Instant::now();
        for c in 0..calls {
            let id = (c as usize * 7) % n;
            let got = st
                .get(&k, &types[id % TYPES], &id.to_string())
                .await
                .expect("get");
            assert!(got.is_some());
        }
        let per_get = start.elapsed() / calls;
        println!("memory: n={n:>6} list(20)={per_list:?} get={per_get:?}");
    }
}

/// `ResponseCache::put` of a new key into a full session cache of
/// capacity `c`, so every put evicts.
#[ignore = "timing harness"]
#[tokio::test]
async fn cache_put_at_capacity_scaling() {
    for c in [500, 5_000] {
        let cache = ResponseCache::new(c, Duration::from_secs(3600));
        let k = key();
        let answer = CachedResponse {
            status: 200,
            content_type: None,
            body: Some(json!({"ok": true})),
            fallback_reason: None,
        };
        for i in 0..c {
            cache.put(&k, format!("k{i}"), Some("t"), answer.clone());
        }
        let puts: u32 = 5_000;
        let start = Instant::now();
        for i in 0..puts {
            cache.put(&k, format!("new{i}"), Some("t"), answer.clone());
        }
        let per_put = start.elapsed() / puts;
        assert_eq!(cache.len(&k), c);
        println!("cache put at capacity: c={c:>5} put={per_put:?}");
    }
}
