//! Backend-neutral `ControlStore` conformance suite. Every backend's test file
//! calls [`run_all`] so all backends run identical assertions.

use std::sync::Arc;

use chrono::Utc;
use mb_mcm::Bundle;
use mb_mcm::Trigger;
use mb_store::{
    ChannelSettings, ContractStatus, ControlChange, ControlStore, DEFAULT_WORKSPACE, LlmProfile,
    Mock, MockEventSettings, NewContractVersion, NewMock, NewRequestLogEntry,
    RETENTION_PRUNE_INTERVAL, RequestKind, RequestLogFilter, ResourceSource, Scenario,
    ScenarioRules, StoreError,
};
use serde_json::json;
use uuid::Uuid;

pub async fn run_all(store: Arc<dyn ControlStore>) {
    store.ping().await.expect("ping");
    mock_crud_and_conflict(&store).await;
    invalid_mock_name_rejected(&store).await;
    contract_versions_increment(&store).await;
    activation_is_exclusive_and_rolls_back(&store).await;
    draft_cannot_activate(&store).await;
    delete_mock_cascades(&store).await;
    scenario_upsert_and_delete(&store).await;
    profile_crud(&store).await;
    request_log_filters_and_paginates(&store).await;
    request_log_retention_prunes(&store).await;
    request_log_retention_prunes_interleaved_mocks(&store).await;
    changes_are_published(&store).await;
    channel_settings_crud(&store).await;
    event_settings_upsert(&store).await;
    scheduler_lease_exclusive(&store).await;
    delete_mock_cascades_event_tables(&store).await;
    request_log_kind_filter(&store).await;
}

fn new_mock(name: &str) -> NewMock {
    NewMock {
        name: name.to_string(),
        route_prefix: None,
        persona: None,
        llm_profile: None,
        header_overrides: false,
        source: ResourceSource::Api,
    }
}

fn bundle(title: &str) -> Bundle {
    Bundle {
        id: title.to_lowercase(),
        title: title.to_string(),
        version: "1.0.0".into(),
        source_digest: "abc".into(),
        operations: vec![],
        defs: serde_json::Map::new(),
        protocol_payload: None,
        channels: Vec::new(),
    }
}

fn new_version(status: ContractStatus, src: &str) -> NewContractVersion {
    NewContractVersion {
        source: src.as_bytes().to_vec(),
        bundle: bundle(src),
        status,
        created_by: Some("tester".into()),
    }
}

async fn create(store: &Arc<dyn ControlStore>, name: &str) -> Mock {
    store
        .create_mock(DEFAULT_WORKSPACE, new_mock(name))
        .await
        .expect("create mock")
}

fn log_entry(mock_id: Uuid, session: &str, op: &str, source: &str) -> NewRequestLogEntry {
    NewRequestLogEntry {
        mock_id,
        operation_id: Some(op.to_string()),
        session: session.to_string(),
        method: "GET".into(),
        path: "/orders".into(),
        status: 200,
        source: Some(source.to_string()),
        latency_ms: 12,
        prompt_tokens: 3,
        completion_tokens: 4,
        request: json!({"q": 1}),
        response: Some(json!({"ok": true})),
        at: Utc::now(),
        kind: RequestKind::Http,
    }
}

async fn mock_crud_and_conflict(store: &Arc<dyn ControlStore>) {
    let mut nm = new_mock("crud");
    nm.persona = Some("grumpy".into());
    let created = store
        .create_mock(DEFAULT_WORKSPACE, nm)
        .await
        .expect("create");
    assert_eq!(created.name, "crud");
    assert_eq!(created.workspace, DEFAULT_WORKSPACE);
    assert_eq!(created.route_prefix, "/crud");
    assert_eq!(created.persona.as_deref(), Some("grumpy"));
    assert_eq!(created.source, ResourceSource::Api);

    let fetched = store
        .get_mock(DEFAULT_WORKSPACE, "crud")
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(fetched, created);
    assert!(
        store
            .get_mock(DEFAULT_WORKSPACE, "nope")
            .await
            .expect("get")
            .is_none()
    );

    let err = store
        .create_mock(DEFAULT_WORKSPACE, new_mock("crud"))
        .await
        .expect_err("duplicate name");
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");

    let mut custom = new_mock("crud-custom");
    custom.route_prefix = Some("/api/v2".into());
    let custom = store
        .create_mock(DEFAULT_WORKSPACE, custom)
        .await
        .expect("create custom");
    assert_eq!(custom.route_prefix, "/api/v2");

    let mut bad_prefix = new_mock("crud-bad-prefix");
    bad_prefix.route_prefix = Some("no-slash".into());
    let err = store
        .create_mock(DEFAULT_WORKSPACE, bad_prefix)
        .await
        .expect_err("bad prefix");
    assert!(matches!(err, StoreError::Invalid(_)), "got {err:?}");

    let names: Vec<String> = store
        .list_mocks(DEFAULT_WORKSPACE)
        .await
        .expect("list")
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert!(names.contains(&"crud".to_string()));
    assert!(names.contains(&"crud-custom".to_string()));

    let mut changed = fetched.clone();
    changed.persona = None;
    changed.header_overrides = true;
    changed.llm_profile = Some("fast".into());
    let updated = store.update_mock(&changed).await.expect("update");
    assert_eq!(updated.persona, None);
    assert!(updated.header_overrides);
    assert_eq!(updated.llm_profile.as_deref(), Some("fast"));
    assert_eq!(updated.created_at, created.created_at);
    assert!(updated.updated_at >= created.updated_at);
    let refetched = store
        .get_mock(DEFAULT_WORKSPACE, "crud")
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(refetched, updated);

    let mut ghost = updated.clone();
    ghost.id = Uuid::new_v4();
    ghost.name = "ghost".into();
    let err = store.update_mock(&ghost).await.expect_err("missing mock");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");

    let mut clash = custom.clone();
    clash.name = "crud".into();
    let err = store
        .update_mock(&clash)
        .await
        .expect_err("rename onto existing");
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");

    assert!(
        store
            .delete_mock(DEFAULT_WORKSPACE, "crud")
            .await
            .expect("delete")
    );
    assert!(
        !store
            .delete_mock(DEFAULT_WORKSPACE, "crud")
            .await
            .expect("delete again")
    );
    assert!(
        store
            .get_mock(DEFAULT_WORKSPACE, "crud")
            .await
            .expect("get")
            .is_none()
    );
}

async fn invalid_mock_name_rejected(store: &Arc<dyn ControlStore>) {
    let err = store
        .create_mock(DEFAULT_WORKSPACE, new_mock("Bad Name"))
        .await
        .expect_err("invalid name");
    assert!(matches!(err, StoreError::Invalid(_)), "got {err:?}");
    assert!(
        store
            .get_mock(DEFAULT_WORKSPACE, "Bad Name")
            .await
            .expect("get")
            .is_none()
    );
}

async fn contract_versions_increment(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "versions").await;
    let v1 = store
        .add_contract_version(mock.id, new_version(ContractStatus::Draft, "one"))
        .await
        .expect("v1");
    let v2 = store
        .add_contract_version(mock.id, new_version(ContractStatus::Approved, "two"))
        .await
        .expect("v2");
    assert_eq!(v1.version, 1);
    assert_eq!(v2.version, 2);
    assert_eq!(v1.mock_id, mock.id);
    assert_eq!(v1.source, b"one".to_vec());
    assert_eq!(v1.bundle, bundle("one"));
    assert_eq!(v1.status, ContractStatus::Draft);
    assert_eq!(v1.created_by.as_deref(), Some("tester"));
    assert!(!v1.source_digest.is_empty());
    assert_ne!(v1.source_digest, v2.source_digest);

    let listed = store.list_contract_versions(mock.id).await.expect("list");
    assert_eq!(
        listed.iter().map(|v| v.version).collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(listed[1], v1);

    let got = store
        .get_contract_version(mock.id, 2)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(got, v2);
    assert!(
        store
            .get_contract_version(mock.id, 9)
            .await
            .expect("get")
            .is_none()
    );
    assert!(
        store
            .active_contract(mock.id)
            .await
            .expect("active")
            .is_none()
    );

    let err = store
        .add_contract_version(Uuid::new_v4(), new_version(ContractStatus::Draft, "orphan"))
        .await
        .expect_err("unknown mock");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
}

async fn activation_is_exclusive_and_rolls_back(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "activation").await;
    for src in ["a", "b"] {
        store
            .add_contract_version(mock.id, new_version(ContractStatus::Approved, src))
            .await
            .expect("add");
    }
    let active = store
        .activate_contract_version(mock.id, 1)
        .await
        .expect("activate v1");
    assert_eq!(active.version, 1);
    assert_eq!(active.status, ContractStatus::Active);

    let active = store
        .activate_contract_version(mock.id, 2)
        .await
        .expect("activate v2");
    assert_eq!(active.status, ContractStatus::Active);
    let status_of = |versions: &[mb_store::ContractVersion], n: i64| {
        versions
            .iter()
            .find(|v| v.version == n)
            .map(|v| v.status.clone())
            .expect("version present")
    };
    let versions = store.list_contract_versions(mock.id).await.expect("list");
    assert_eq!(status_of(&versions, 1), ContractStatus::Approved);
    assert_eq!(status_of(&versions, 2), ContractStatus::Active);
    assert_eq!(
        store
            .active_contract(mock.id)
            .await
            .expect("active")
            .map(|v| v.version),
        Some(2)
    );

    // Roll back to v1.
    store
        .activate_contract_version(mock.id, 1)
        .await
        .expect("rollback");
    let versions = store.list_contract_versions(mock.id).await.expect("list");
    assert_eq!(status_of(&versions, 1), ContractStatus::Active);
    assert_eq!(status_of(&versions, 2), ContractStatus::Approved);
    assert_eq!(
        versions
            .iter()
            .filter(|v| v.status == ContractStatus::Active)
            .count(),
        1
    );
    assert_eq!(
        store
            .active_contract(mock.id)
            .await
            .expect("active")
            .map(|v| v.version),
        Some(1)
    );

    let err = store
        .activate_contract_version(mock.id, 42)
        .await
        .expect_err("missing version");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
}

async fn draft_cannot_activate(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "drafty").await;
    store
        .add_contract_version(mock.id, new_version(ContractStatus::Draft, "draft"))
        .await
        .expect("add");
    let err = store
        .activate_contract_version(mock.id, 1)
        .await
        .expect_err("draft activation");
    assert!(matches!(err, StoreError::Invalid(_)), "got {err:?}");
    let v1 = store
        .get_contract_version(mock.id, 1)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(v1.status, ContractStatus::Draft);
    assert!(
        store
            .active_contract(mock.id)
            .await
            .expect("active")
            .is_none()
    );
}

async fn delete_mock_cascades(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "cascade").await;
    store
        .add_contract_version(mock.id, new_version(ContractStatus::Approved, "c"))
        .await
        .expect("add");
    store
        .put_scenario(&Scenario {
            mock_id: mock.id,
            name: "outage".into(),
            instructions: None,
            rules: ScenarioRules::default(),
            source: ResourceSource::Api,
        })
        .await
        .expect("scenario");
    store
        .append_request_log(log_entry(mock.id, "s1", "op", "llm"), 1000)
        .await
        .expect("log");

    assert!(
        store
            .delete_mock(DEFAULT_WORKSPACE, "cascade")
            .await
            .expect("delete")
    );
    assert!(
        store
            .list_contract_versions(mock.id)
            .await
            .expect("versions")
            .is_empty()
    );
    assert!(
        store
            .list_scenarios(mock.id)
            .await
            .expect("scenarios")
            .is_empty()
    );
    assert!(
        store
            .list_request_log(mock.id, &RequestLogFilter::default())
            .await
            .expect("log")
            .is_empty()
    );
}

async fn scenario_upsert_and_delete(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "scenarios").await;
    let mut s = Scenario {
        mock_id: mock.id,
        name: "slow".into(),
        instructions: Some("be slow".into()),
        rules: ScenarioRules {
            status: Some(503),
            latency_ms: Some(1500),
            set: Some(json!({"retry": true})),
            operations: vec!["listOrders".into()],
            stream_items: Some(3),
            stream_item_interval_ms: Some(250),
        },
        source: ResourceSource::File,
    };
    store.put_scenario(&s).await.expect("put");
    assert_eq!(
        store.list_scenarios(mock.id).await.expect("list"),
        vec![s.clone()]
    );

    s.instructions = None;
    s.rules.status = None;
    s.source = ResourceSource::Api;
    store.put_scenario(&s).await.expect("upsert");
    let other = Scenario {
        mock_id: mock.id,
        name: "empty".into(),
        instructions: None,
        rules: ScenarioRules::default(),
        source: ResourceSource::Api,
    };
    store.put_scenario(&other).await.expect("put other");
    let listed = store.list_scenarios(mock.id).await.expect("list");
    assert_eq!(listed.len(), 2);
    assert!(listed.contains(&s));
    assert!(listed.contains(&other));

    assert!(
        store
            .delete_scenario(mock.id, "slow")
            .await
            .expect("delete")
    );
    assert!(
        !store
            .delete_scenario(mock.id, "slow")
            .await
            .expect("delete again")
    );
    assert_eq!(
        store.list_scenarios(mock.id).await.expect("list"),
        vec![other]
    );

    let err = store
        .put_scenario(&Scenario {
            mock_id: Uuid::new_v4(),
            name: "orphan".into(),
            instructions: None,
            rules: ScenarioRules::default(),
            source: ResourceSource::Api,
        })
        .await
        .expect_err("unknown mock");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
}

async fn profile_crud(store: &Arc<dyn ControlStore>) {
    let mut p = LlmProfile {
        workspace: DEFAULT_WORKSPACE.into(),
        name: "fast".into(),
        base_url: "http://localhost:11434/v1".into(),
        model: "llama3".into(),
        secret_ref: Some("env:OPENAI_API_KEY".into()),
        json_schema: true,
        temperature: 0.5,
        timeout_ms: 30_000,
        max_tokens: 2048,
        fallback_profile: None,
        source: ResourceSource::Api,
    };
    store.put_profile(&p).await.expect("put");
    assert_eq!(
        store
            .get_profile(DEFAULT_WORKSPACE, "fast")
            .await
            .expect("get"),
        Some(p.clone())
    );
    p.model = "llama3.1".into();
    p.fallback_profile = Some("slow".into());
    p.secret_ref = None;
    store.put_profile(&p).await.expect("upsert");
    let slow = LlmProfile {
        name: "slow".into(),
        source: ResourceSource::File,
        ..p.clone()
    };
    store.put_profile(&slow).await.expect("put slow");
    let listed = store.list_profiles(DEFAULT_WORKSPACE).await.expect("list");
    assert_eq!(listed.len(), 2);
    assert!(listed.contains(&p));
    assert!(listed.contains(&slow));
    assert!(
        store
            .get_profile(DEFAULT_WORKSPACE, "missing")
            .await
            .expect("get")
            .is_none()
    );

    assert!(
        store
            .delete_profile(DEFAULT_WORKSPACE, "fast")
            .await
            .expect("delete")
    );
    assert!(
        !store
            .delete_profile(DEFAULT_WORKSPACE, "fast")
            .await
            .expect("delete again")
    );
    assert_eq!(
        store.list_profiles(DEFAULT_WORKSPACE).await.expect("list"),
        vec![slow]
    );
    assert!(
        store
            .delete_profile(DEFAULT_WORKSPACE, "slow")
            .await
            .expect("cleanup")
    );
}

async fn request_log_filters_and_paginates(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "logs").await;
    let mut ids = Vec::new();
    for (session, op, source) in [
        ("alpha", "listOrders", "llm"),
        ("beta", "listOrders", "faker"),
        ("alpha", "getOrder", "llm"),
        ("beta", "getOrder", "llm"),
        ("alpha", "listOrders", "faker"),
    ] {
        let id = store
            .append_request_log(log_entry(mock.id, session, op, source), 1000)
            .await
            .expect("append");
        ids.push(id);
    }
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids increase: {ids:?}");

    let all = store
        .list_request_log(mock.id, &RequestLogFilter::default())
        .await
        .expect("all");
    assert_eq!(
        all.iter().map(|e| e.id).collect::<Vec<_>>(),
        ids.iter().rev().copied().collect::<Vec<_>>()
    );
    let first = all.last().expect("first row");
    assert_eq!(first.session, "alpha");
    assert_eq!(first.operation_id.as_deref(), Some("listOrders"));
    assert_eq!(first.method, "GET");
    assert_eq!(first.path, "/orders");
    assert_eq!(first.status, 200);
    assert_eq!(first.latency_ms, 12);
    assert_eq!(first.prompt_tokens, 3);
    assert_eq!(first.completion_tokens, 4);
    assert_eq!(first.request, json!({"q": 1}));
    assert_eq!(first.response, Some(json!({"ok": true})));

    let alpha = store
        .list_request_log(
            mock.id,
            &RequestLogFilter {
                session: Some("alpha".into()),
                ..RequestLogFilter::default()
            },
        )
        .await
        .expect("alpha");
    assert_eq!(alpha.len(), 3);
    assert!(alpha.iter().all(|e| e.session == "alpha"));

    let filtered = store
        .list_request_log(
            mock.id,
            &RequestLogFilter {
                operation_id: Some("getOrder".into()),
                source: Some("llm".into()),
                ..RequestLogFilter::default()
            },
        )
        .await
        .expect("op+source");
    assert_eq!(
        filtered.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![ids[3], ids[2]]
    );

    let page1 = store
        .list_request_log(
            mock.id,
            &RequestLogFilter {
                limit: 2,
                ..RequestLogFilter::default()
            },
        )
        .await
        .expect("page1");
    assert_eq!(
        page1.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![ids[4], ids[3]]
    );
    let page2 = store
        .list_request_log(
            mock.id,
            &RequestLogFilter {
                limit: 2,
                before_id: Some(page1[1].id),
                ..RequestLogFilter::default()
            },
        )
        .await
        .expect("page2");
    assert_eq!(
        page2.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![ids[2], ids[1]]
    );
    let page3 = store
        .list_request_log(
            mock.id,
            &RequestLogFilter {
                limit: 2,
                before_id: Some(page2[1].id),
                ..RequestLogFilter::default()
            },
        )
        .await
        .expect("page3");
    assert_eq!(page3.iter().map(|e| e.id).collect::<Vec<_>>(), vec![ids[0]]);

    // limit 0 is clamped to 1.
    let clamped = store
        .list_request_log(
            mock.id,
            &RequestLogFilter {
                limit: 0,
                ..RequestLogFilter::default()
            },
        )
        .await
        .expect("clamped");
    assert_eq!(clamped.len(), 1);
}

async fn request_log_retention_prunes(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "retention").await;
    let mut newest = 0;
    for i in 0..250 {
        newest = store
            .append_request_log(
                log_entry(mock.id, &format!("s{}", i % 3), "op", "faker"),
                10,
            )
            .await
            .expect("append");
    }
    let remaining = store
        .list_request_log(
            mock.id,
            &RequestLogFilter {
                limit: 500,
                ..RequestLogFilter::default()
            },
        )
        .await
        .expect("list");
    assert!(remaining.len() <= 110, "{} rows remain", remaining.len());
    assert!(remaining.len() >= 10, "{} rows remain", remaining.len());
    // The newest rows are the ones kept, contiguous up to the last insert.
    assert_eq!(remaining.first().map(|e| e.id), Some(newest));
    assert!(
        remaining.windows(2).all(|w| w[0].id == w[1].id + 1),
        "kept rows are the newest, contiguous: {:?}",
        remaining.iter().map(|e| e.id).collect::<Vec<_>>()
    );
}

/// Two mocks whose inserts strictly alternate both stay bounded: pruning is
/// counted per mock, not keyed on the shared id sequence.
async fn request_log_retention_prunes_interleaved_mocks(store: &Arc<dyn ControlStore>) {
    let retention = 10;
    let mocks = [
        create(store, "interleave-a").await,
        create(store, "interleave-b").await,
    ];
    for i in 0..300 {
        store
            .append_request_log(log_entry(mocks[i % 2].id, "s", "op", "faker"), retention)
            .await
            .expect("append");
    }
    let bound = usize::try_from(i64::from(retention) + RETENTION_PRUNE_INTERVAL).expect("bound");
    for mock in &mocks {
        let rows = store
            .list_request_log(
                mock.id,
                &RequestLogFilter {
                    limit: 500,
                    ..RequestLogFilter::default()
                },
            )
            .await
            .expect("list");
        assert!(
            rows.len() <= bound,
            "mock {}: {} rows remain, more than retention + interval ({bound})",
            mock.name,
            rows.len()
        );
        assert!(rows.len() >= 10, "{} rows remain", rows.len());
    }
}

/// Next published change, failing (instead of hanging) if none arrives.
async fn next_change(
    rx: &mut tokio::sync::broadcast::Receiver<ControlChange>,
    what: &str,
) -> ControlChange {
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("{what}: no change published within 5s"))
        .unwrap_or_else(|e| panic!("{what}: {e}"))
}

async fn changes_are_published(store: &Arc<dyn ControlStore>) {
    let mut rx = store.subscribe();
    let mock = create(store, "published").await;
    let expected = ControlChange::Mock {
        workspace: DEFAULT_WORKSPACE.into(),
        name: "published".into(),
    };
    assert_eq!(next_change(&mut rx, "create change").await, expected);

    store
        .add_contract_version(mock.id, new_version(ContractStatus::Approved, "p"))
        .await
        .expect("add");
    assert_eq!(next_change(&mut rx, "version change").await, expected);

    store
        .activate_contract_version(mock.id, 1)
        .await
        .expect("activate");
    assert_eq!(next_change(&mut rx, "activate change").await, expected);
}

fn channel_settings(mock_id: Uuid, channel_id: &str) -> ChannelSettings {
    ChannelSettings {
        mock_id,
        channel_id: channel_id.to_string(),
        enabled: true,
        triggers: None,
        source: ResourceSource::Api,
        updated_at: micros_now(),
    }
}

/// Now at microsecond precision, which every backend round-trips.
fn micros_now() -> chrono::DateTime<Utc> {
    use chrono::SubsecRound;
    Utc::now().trunc_subsecs(6)
}

async fn channel_settings_crud(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "channels").await;
    let mut rx = store.subscribe();
    let expected = ControlChange::Mock {
        workspace: DEFAULT_WORKSPACE.into(),
        name: "channels".into(),
    };

    let first = channel_settings(mock.id, "lightMeasured");
    store.put_channel_settings(&first).await.expect("put");
    assert_eq!(next_change(&mut rx, "put change").await, expected);
    assert_eq!(
        store.list_channel_settings(mock.id).await.expect("list"),
        vec![first.clone()],
        "triggers: None round-trips as None"
    );

    let updated = ChannelSettings {
        enabled: false,
        triggers: Some(vec![
            Trigger::Schedule {
                every: std::time::Duration::from_millis(500),
            },
            Trigger::Manual,
        ]),
        updated_at: micros_now(),
        ..first.clone()
    };
    store
        .put_channel_settings(&updated)
        .await
        .expect("put again");
    assert_eq!(next_change(&mut rx, "second put change").await, expected);
    let other = channel_settings(mock.id, "alpha");
    store.put_channel_settings(&other).await.expect("put other");
    assert_eq!(next_change(&mut rx, "other put change").await, expected);
    assert_eq!(
        store.list_channel_settings(mock.id).await.expect("list"),
        vec![other.clone(), updated.clone()],
        "one row per channel, updated in place, sorted by channel id"
    );

    assert!(
        store
            .delete_channel_settings(mock.id, "lightMeasured")
            .await
            .expect("delete")
    );
    assert_eq!(next_change(&mut rx, "delete change").await, expected);
    assert!(
        !store
            .delete_channel_settings(mock.id, "lightMeasured")
            .await
            .expect("delete again")
    );
    assert_eq!(
        store.list_channel_settings(mock.id).await.expect("list"),
        vec![other]
    );
}

async fn event_settings_upsert(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "event-settings").await;
    assert_eq!(store.get_event_settings(mock.id).await.expect("get"), None);
    let mut rx = store.subscribe();
    let expected = ControlChange::Mock {
        workspace: DEFAULT_WORKSPACE.into(),
        name: "event-settings".into(),
    };
    let s = MockEventSettings {
        mock_id: mock.id,
        broker: Some("local".into()),
        source: ResourceSource::Api,
        updated_at: micros_now(),
    };
    store.put_event_settings(&s).await.expect("put");
    assert_eq!(next_change(&mut rx, "put change").await, expected);
    assert_eq!(
        store.get_event_settings(mock.id).await.expect("get"),
        Some(s.clone())
    );
    let s2 = MockEventSettings {
        broker: None,
        source: ResourceSource::File,
        updated_at: micros_now(),
        ..s
    };
    store.put_event_settings(&s2).await.expect("put again");
    assert_eq!(next_change(&mut rx, "second put change").await, expected);
    assert_eq!(
        store.get_event_settings(mock.id).await.expect("get"),
        Some(s2)
    );
}

async fn scheduler_lease_exclusive(store: &Arc<dyn ControlStore>) {
    use std::time::Duration;
    let mock = create(store, "lease").await;
    let ttl = Duration::from_secs(30);
    assert!(
        store
            .acquire_scheduler_lease(mock.id, "a", ttl)
            .await
            .expect("a")
    );
    assert!(
        !store
            .acquire_scheduler_lease(mock.id, "b", ttl)
            .await
            .expect("b")
    );
    assert!(
        store
            .acquire_scheduler_lease(mock.id, "a", ttl)
            .await
            .expect("a renews")
    );
    assert!(
        store
            .acquire_scheduler_lease(mock.id, "a", Duration::from_millis(1))
            .await
            .expect("a short")
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        store
            .acquire_scheduler_lease(mock.id, "b", ttl)
            .await
            .expect("b after expiry")
    );
    assert!(
        !store
            .acquire_scheduler_lease(mock.id, "a", ttl)
            .await
            .expect("a after b took over")
    );
}

async fn delete_mock_cascades_event_tables(store: &Arc<dyn ControlStore>) {
    use std::time::Duration;
    let mock = create(store, "event-cascade").await;
    store
        .put_channel_settings(&channel_settings(mock.id, "c"))
        .await
        .expect("channel settings");
    store
        .put_event_settings(&MockEventSettings {
            mock_id: mock.id,
            broker: Some("local".into()),
            source: ResourceSource::Api,
            updated_at: micros_now(),
        })
        .await
        .expect("event settings");
    assert!(
        store
            .acquire_scheduler_lease(mock.id, "a", Duration::from_secs(30))
            .await
            .expect("lease")
    );
    assert!(
        store
            .delete_mock(DEFAULT_WORKSPACE, "event-cascade")
            .await
            .expect("delete")
    );
    assert!(
        store
            .list_channel_settings(mock.id)
            .await
            .expect("list")
            .is_empty()
    );
    assert_eq!(store.get_event_settings(mock.id).await.expect("get"), None);

    // A mock re-created under the same name gets a fresh id; the old lease
    // row went with the old mock, so another holder acquires at once.
    let again = create(store, "event-cascade").await;
    assert!(
        store
            .acquire_scheduler_lease(again.id, "b", Duration::from_secs(30))
            .await
            .expect("lease on new mock")
    );
    // Writing settings for the deleted mock fails rather than resurrecting rows.
    assert!(
        store
            .put_channel_settings(&channel_settings(mock.id, "c"))
            .await
            .is_err()
    );
}

async fn request_log_kind_filter(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "kinds").await;
    let mut event = log_entry(mock.id, "s", "lightMeasured", "llm");
    event.kind = RequestKind::Event;
    event.method = "PUBLISH".into();
    let event_id = store.append_request_log(event, 1000).await.expect("event");
    let http_id = store
        .append_request_log(log_entry(mock.id, "s", "listOrders", "llm"), 1000)
        .await
        .expect("http");
    // A P2-shaped entry (no `kind`) is an `http` one.
    let mut p2 = serde_json::to_value(log_entry(mock.id, "s", "getOrder", "llm")).expect("json");
    p2.as_object_mut().expect("object").remove("kind");
    let p2: NewRequestLogEntry = serde_json::from_value(p2).expect("p2 entry");
    assert_eq!(p2.kind, RequestKind::Http);
    let p2_id = store.append_request_log(p2, 1000).await.expect("p2");

    let events = store
        .list_request_log(
            mock.id,
            &RequestLogFilter {
                kind: Some(RequestKind::Event),
                ..RequestLogFilter::default()
            },
        )
        .await
        .expect("events");
    assert_eq!(events.iter().map(|e| e.id).collect::<Vec<_>>(), [event_id]);
    assert_eq!(events[0].kind, RequestKind::Event);

    let http = store
        .list_request_log(
            mock.id,
            &RequestLogFilter {
                kind: Some(RequestKind::Http),
                ..RequestLogFilter::default()
            },
        )
        .await
        .expect("http");
    assert_eq!(
        http.iter().map(|e| e.id).collect::<Vec<_>>(),
        [p2_id, http_id]
    );
    assert!(http.iter().all(|e| e.kind == RequestKind::Http));

    let all = store
        .list_request_log(mock.id, &RequestLogFilter::default())
        .await
        .expect("all");
    assert_eq!(all.len(), 3);
}
