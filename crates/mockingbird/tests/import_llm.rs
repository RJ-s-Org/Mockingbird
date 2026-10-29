//! The LLM access of imports and draft previews: `runtime::import_provider`
//! and `runtime::build_engine`.
#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::StreamConfig;
use mb_ext::Extensions;
use mb_protocols::ProtocolSettings;
use mb_protocols::events::EventDeps;
use mb_state::{MemoryStore, StateStore};
use mb_store::{ControlStore, DEFAULT_WORKSPACE, LlmProfile, Mock, NewMock, ResourceSource};
use mockingbird::runtime::{
    IMPORT_MIN_MAX_TOKENS, IMPORT_MIN_TIMEOUT, RuntimeDeps, build_engine, import_provider,
};

async fn store() -> Arc<dyn ControlStore> {
    mb_store::connect("sqlite::memory:").await.unwrap()
}

fn deps(store: Arc<dyn ControlStore>, offline: bool, default_profile: Option<&str>) -> RuntimeDeps {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    RuntimeDeps {
        store,
        state,
        ext: Extensions::default(),
        recorder: None,
        offline,
        default_profile: default_profile.map(str::to_string).into(),
        data_plane_keys: None,
        protocols: ProtocolSettings::default(),
        stream: StreamConfig::default(),
        events: EventDeps::default(),
        limiter: mb_engine::GenerationLimiter::new(64),
        scrubber: None,
        limits: Default::default(),
        http: Default::default(),
        mode: mockingbird::runtime::ServeMode::Full,
        webhook_log: Default::default(),
    }
}

fn profile(workspace: &str, name: &str, timeout_ms: u64, max_tokens: u32) -> LlmProfile {
    LlmProfile {
        workspace: workspace.into(),
        name: name.into(),
        base_url: "http://127.0.0.1:9/v1".into(),
        model: "m".into(),
        secret_ref: None,
        json_schema: false,
        temperature: 0.7,
        timeout_ms,
        max_tokens,
        fallback_profile: None,
        headers: Default::default(),
        extra_body: Default::default(),
        source: ResourceSource::Api,
    }
}

async fn mock(
    store: &Arc<dyn ControlStore>,
    workspace: &str,
    name: &str,
    llm_profile: Option<&str>,
) -> Mock {
    store
        .create_mock(
            workspace,
            NewMock {
                name: name.into(),
                route_prefix: None,
                persona: Some("terse".into()),
                llm_profile: llm_profile.map(str::to_string),
                header_overrides: true,
                source: ResourceSource::Api,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: Default::default(),
                behavior: Default::default(),
                hosts: vec![],
                webhooks: Vec::new(),
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn import_llm_raises_profile_limits() {
    let store = store().await;
    store
        .put_profile(&profile(DEFAULT_WORKSPACE, "small", 3000, 1024))
        .await
        .unwrap();
    let m = mock(&store, DEFAULT_WORKSPACE, "pets", Some("small")).await;
    let deps = deps(store, false, None);
    let llm = import_provider(&deps, &m, None).await.unwrap().unwrap();
    assert_eq!(IMPORT_MIN_TIMEOUT, Duration::from_secs(120));
    assert_eq!(IMPORT_MIN_MAX_TOKENS, 8192);
    assert_eq!(llm.settings.timeout, Duration::from_secs(120));
    assert_eq!(llm.settings.max_tokens, 8192);
    assert!((llm.settings.temperature - 0.2).abs() < f32::EPSILON);

    // Larger profile values are kept; a lower temperature too.
    let store = deps.store.clone();
    let mut big = profile(DEFAULT_WORKSPACE, "big", 300_000, 16_000);
    big.temperature = 0.1;
    store.put_profile(&big).await.unwrap();
    let m = mock(&store, DEFAULT_WORKSPACE, "big-pets", Some("big")).await;
    let llm = import_provider(&deps, &m, None).await.unwrap().unwrap();
    assert_eq!(llm.settings.timeout, Duration::from_secs(300));
    assert_eq!(llm.settings.max_tokens, 16_000);
    assert!((llm.settings.temperature - 0.1).abs() < f32::EPSILON);

    // The default profile applies to a mock naming none; no profile at all
    // means no LLM.
    let m = mock(&store, DEFAULT_WORKSPACE, "plain", None).await;
    assert!(import_provider(&deps, &m, None).await.unwrap().is_none());
    let with_default = RuntimeDeps {
        default_profile: Some("small".to_string()).into(),
        ..deps.clone()
    };
    let llm = import_provider(&with_default, &m, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(llm.settings.max_tokens, 8192);
}

#[tokio::test]
async fn import_llm_override_profile() {
    let store = store().await;
    store.create_workspace("team-a", "Team A").await.unwrap();
    store
        .put_profile(&profile(DEFAULT_WORKSPACE, "importer", 200_000, 32_000))
        .await
        .unwrap();
    store
        .put_profile(&profile("team-a", "own", 3000, 1024))
        .await
        .unwrap();
    let m = mock(&store, "team-a", "pets", Some("own")).await;
    let deps = deps(store.clone(), false, None);

    // The override comes from `default` when the mock's workspace lacks it.
    let llm = import_provider(&deps, &m, Some("importer"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(llm.settings.timeout, Duration::from_secs(200));
    assert_eq!(llm.settings.max_tokens, 32_000);

    // The mock's workspace wins when it has the profile.
    store
        .put_profile(&profile("team-a", "importer", 150_000, 9000))
        .await
        .unwrap();
    let llm = import_provider(&deps, &m, Some("importer"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(llm.settings.timeout, Duration::from_secs(150));
    assert_eq!(llm.settings.max_tokens, 9000);

    // An override found nowhere is an error.
    let err = import_provider(&deps, &m, Some("missing"))
        .await
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("missing"), "{err:#}");
}

#[tokio::test]
async fn import_llm_offline_is_none() {
    let store = store().await;
    store
        .put_profile(&profile(DEFAULT_WORKSPACE, "small", 3000, 1024))
        .await
        .unwrap();
    let m = mock(&store, DEFAULT_WORKSPACE, "pets", Some("small")).await;
    let deps = deps(store, true, Some("small"));
    assert!(import_provider(&deps, &m, None).await.unwrap().is_none());
    assert!(
        import_provider(&deps, &m, Some("small"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn build_engine_matches_runtime_profile() {
    let store = store().await;
    let mut p = profile(DEFAULT_WORKSPACE, "small", 3000, 1024);
    p.temperature = 0.4;
    store.put_profile(&p).await.unwrap();
    let m = mock(&store, DEFAULT_WORKSPACE, "pets", Some("small")).await;

    let online = deps(store.clone(), false, None);
    let engine = build_engine(&online, &m).await.unwrap();
    let config = engine.config();
    assert_eq!(config.timeout, Duration::from_millis(3000));
    assert_eq!(config.max_tokens, 1024);
    assert!((config.temperature - 0.4).abs() < f32::EPSILON);
    assert_eq!(config.persona.as_deref(), Some("terse"));

    // Offline engines keep the defaults and the persona.
    let offline = deps(store, true, None);
    let engine = build_engine(&offline, &m).await.unwrap();
    let defaults = mb_engine::EngineConfig::default();
    assert_eq!(engine.config().timeout, defaults.timeout);
    assert_eq!(engine.config().max_tokens, defaults.max_tokens);
    assert_eq!(engine.config().persona.as_deref(), Some("terse"));
}

/// Maps profile `importer` to `vetted`, refuses `forbidden`, and records
/// what it was asked.
struct RemapPolicy(std::sync::Mutex<Vec<(mb_ext::Tenancy, String, Option<String>)>>);

impl mb_ext::ProviderPolicy for RemapPolicy {
    fn choose(
        &self,
        t: &mb_ext::Tenancy,
        mock: &str,
        configured: Option<&str>,
    ) -> Result<Option<String>, mb_ext::Rejection> {
        self.0
            .lock()
            .unwrap()
            .push((t.clone(), mock.to_string(), configured.map(str::to_string)));
        match configured {
            Some("forbidden") => Err(mb_ext::Rejection {
                status: 403,
                title: "Forbidden".into(),
                detail: "profile forbidden is not allowed".into(),
                retry_after: None,
            }),
            Some("importer") => Ok(Some("vetted".into())),
            other => Ok(other.map(str::to_string)),
        }
    }
}

#[tokio::test]
async fn import_llm_override_goes_through_provider_policy() {
    let store = store().await;
    store.create_workspace("team-a", "Team A").await.unwrap();
    store
        .put_profile(&profile(DEFAULT_WORKSPACE, "importer", 200_000, 32_000))
        .await
        .unwrap();
    store
        .put_profile(&profile(DEFAULT_WORKSPACE, "vetted", 130_000, 10_000))
        .await
        .unwrap();
    let m = mock(&store, "team-a", "pets", None).await;
    let policy = Arc::new(RemapPolicy(std::sync::Mutex::new(Vec::new())));
    let mut deps = deps(store.clone(), false, None);
    deps.ext.providers = policy.clone();

    let llm = import_provider(&deps, &m, Some("importer"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        llm.settings.timeout,
        Duration::from_secs(130),
        "the policy's pick"
    );
    let asked = policy.0.lock().unwrap().clone();
    assert_eq!(
        asked,
        vec![(
            mb_ext::Tenancy {
                tenant: "default".into(),
                workspace: "team-a".into()
            },
            "pets".to_string(),
            Some("importer".to_string())
        )]
    );

    let err = import_provider(&deps, &m, Some("forbidden"))
        .await
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("not allowed"), "{err:#}");
}
