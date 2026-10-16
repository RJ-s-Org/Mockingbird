//! Backend-neutral `ControlStore` conformance suite. Every backend's test file
//! calls [`run_all`] so all backends run identical assertions.

use std::sync::Arc;

use chrono::{DateTime, Duration, SubsecRound, TimeZone, Utc};
use mb_mcm::Bundle;
use mb_mcm::Trigger;
use mb_store::{
    AdminSession, ApiKey, AuditFilter, ChannelSettings, ContractStatus, ControlChange,
    ControlStore, DEFAULT_WORKSPACE, DataPlaneAuthConfig, DataPlaneAuthMode, ImportJobStatus,
    ImportOutcome, ImportProgress, LlmProfile, Mock, MockEventSettings, NewAuditEvent,
    NewContractVersion, NewImportJob, NewMock, NewRequestLogEntry, RETENTION_PRUNE_INTERVAL,
    RequestKind, RequestLogFilter, ResourceSource, Scenario, ScenarioRules, StoreError,
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
    default_workspace_exists(&store).await;
    workspace_crud_and_conflict(&store).await;
    invalid_workspace_name_rejected(&store).await;
    delete_default_workspace_conflicts(&store).await;
    delete_nonempty_workspace_requires_force(&store).await;
    mock_in_missing_workspace_not_found(&store).await;
    same_mock_name_in_two_workspaces(&store).await;
    data_plane_auth_round_trips(&store).await;
    workspace_changes_are_published(&store).await;
    api_key_create_get_list_revoke(&store).await;
    api_key_in_missing_workspace_not_found(&store).await;
    api_key_touch_updates_last_used(&store).await;
    admin_session_lifecycle(&store).await;
    expired_admin_sessions_are_swept(&store).await;
    audit_append_and_filter(&store).await;
    audit_prune_by_age(&store).await;
    force_delete_workspace_removes_api_keys_keeps_audit(&store).await;
    draft_version_round_trips_meta(&store).await;
    add_draft_requires_draft_status(&store).await;
    update_draft_replaces_source_and_meta(&store).await;
    update_draft_rejects_non_draft(&store).await;
    approve_draft_once(&store).await;
    draft_writes_publish_nothing(&store).await;
    import_job_lifecycle(&store).await;
    claim_is_fifo_and_exclusive(&store).await;
    progress_and_finish_need_running(&store).await;
    cancel_queued_and_running(&store).await;
    stale_running_jobs_fail(&store).await;
    prune_finished_jobs(&store).await;
    jobs_listed_newest_first_and_cascade_with_mock(&store).await;
    import_job_for_missing_mock_not_found(&store).await;
    complete_import_job_stores_draft_only_while_running(&store).await;
    active_import_jobs_counted_per_mock(&store).await;
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
        data_plane_auth: Default::default(),
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

fn profile(ws: &str, name: &str) -> LlmProfile {
    LlmProfile {
        workspace: ws.into(),
        name: name.into(),
        base_url: "http://localhost:11434/v1".into(),
        model: "llama3".into(),
        secret_ref: None,
        json_schema: false,
        temperature: 0.0,
        timeout_ms: 1000,
        max_tokens: 100,
        fallback_profile: None,
        source: ResourceSource::Api,
    }
}

async fn default_workspace_exists(store: &Arc<dyn ControlStore>) {
    let ws = store
        .get_workspace(DEFAULT_WORKSPACE)
        .await
        .expect("get")
        .expect("default workspace exists");
    assert_eq!(ws.name, DEFAULT_WORKSPACE);
    assert_eq!(ws.display_name, "Default");
    let names: Vec<String> = store
        .list_workspaces()
        .await
        .expect("list")
        .into_iter()
        .map(|w| w.name)
        .collect();
    assert!(names.contains(&DEFAULT_WORKSPACE.to_string()), "{names:?}");
}

async fn workspace_crud_and_conflict(store: &Arc<dyn ControlStore>) {
    let created = store
        .create_workspace("team-a", "Team A")
        .await
        .expect("create");
    assert_eq!(created.name, "team-a");
    assert_eq!(created.display_name, "Team A");
    assert_eq!(
        store.get_workspace("team-a").await.expect("get"),
        Some(created.clone())
    );
    assert!(store.get_workspace("nope").await.expect("get").is_none());

    let err = store
        .create_workspace("team-a", "Again")
        .await
        .expect_err("duplicate");
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");

    let updated = store
        .update_workspace("team-a", "Team Alpha")
        .await
        .expect("update");
    assert_eq!(updated.display_name, "Team Alpha");
    assert_eq!(updated.created_at, created.created_at);
    assert_eq!(
        store.get_workspace("team-a").await.expect("get"),
        Some(updated)
    );
    let err = store
        .update_workspace("ghost-ws", "Ghost")
        .await
        .expect_err("missing");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");

    store
        .create_workspace("aaa-first", "First")
        .await
        .expect("create");
    let names: Vec<String> = store
        .list_workspaces()
        .await
        .expect("list")
        .into_iter()
        .map(|w| w.name)
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "name ascending");
    assert!(names.contains(&"aaa-first".to_string()));
    assert!(names.contains(&"team-a".to_string()));
    assert!(
        store
            .delete_workspace("aaa-first", false)
            .await
            .expect("delete")
    );
    assert!(
        !store
            .delete_workspace("aaa-first", false)
            .await
            .expect("delete again")
    );
    assert!(
        store
            .get_workspace("aaa-first")
            .await
            .expect("get")
            .is_none()
    );
}

async fn invalid_workspace_name_rejected(store: &Arc<dyn ControlStore>) {
    let err = store
        .create_workspace("Team A", "Team A")
        .await
        .expect_err("invalid name");
    assert!(matches!(err, StoreError::Invalid(_)), "got {err:?}");
    assert!(store.get_workspace("Team A").await.expect("get").is_none());
}

async fn delete_default_workspace_conflicts(store: &Arc<dyn ControlStore>) {
    for force in [false, true] {
        let err = store
            .delete_workspace(DEFAULT_WORKSPACE, force)
            .await
            .expect_err("default is undeletable");
        assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
    }
    assert!(
        store
            .get_workspace(DEFAULT_WORKSPACE)
            .await
            .expect("get")
            .is_some()
    );
}

async fn delete_nonempty_workspace_requires_force(store: &Arc<dyn ControlStore>) {
    store
        .create_workspace("doomed", "Doomed")
        .await
        .expect("create");
    let mut nm = new_mock("doomed-mock");
    nm.route_prefix = Some("/doomed-mock".into());
    let mock = store.create_mock("doomed", nm).await.expect("mock");
    store
        .add_contract_version(mock.id, new_version(ContractStatus::Approved, "d"))
        .await
        .expect("version");
    store
        .put_profile(&profile("doomed", "doomed-profile"))
        .await
        .expect("profile");

    let err = store
        .delete_workspace("doomed", false)
        .await
        .expect_err("non-empty");
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
    assert!(
        store
            .get_mock("doomed", "doomed-mock")
            .await
            .expect("get")
            .is_some()
    );

    assert!(store.delete_workspace("doomed", true).await.expect("force"));
    assert!(store.get_workspace("doomed").await.expect("get").is_none());
    assert!(
        store
            .get_mock("doomed", "doomed-mock")
            .await
            .expect("get")
            .is_none()
    );
    assert!(
        store
            .list_profiles("doomed")
            .await
            .expect("profiles")
            .is_empty()
    );
    assert!(
        store
            .list_contract_versions(mock.id)
            .await
            .expect("versions")
            .is_empty()
    );

    // A profile alone also makes a workspace non-empty.
    store
        .create_workspace("doomed-2", "Doomed 2")
        .await
        .expect("create");
    store
        .put_profile(&profile("doomed-2", "only"))
        .await
        .expect("profile");
    let err = store
        .delete_workspace("doomed-2", false)
        .await
        .expect_err("non-empty");
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
    assert!(
        store
            .delete_workspace("doomed-2", true)
            .await
            .expect("force")
    );
}

async fn mock_in_missing_workspace_not_found(store: &Arc<dyn ControlStore>) {
    let err = store
        .create_mock("no-such-ws", new_mock("orphan-mock"))
        .await
        .expect_err("missing workspace");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
    let err = store
        .put_profile(&profile("no-such-ws", "orphan-profile"))
        .await
        .expect_err("missing workspace");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
    assert!(
        store
            .list_mocks("no-such-ws")
            .await
            .expect("list")
            .is_empty()
    );
}

async fn same_mock_name_in_two_workspaces(store: &Arc<dyn ControlStore>) {
    store
        .create_workspace("ws-one", "One")
        .await
        .expect("create");
    store
        .create_workspace("ws-two", "Two")
        .await
        .expect("create");
    let mut a = new_mock("shared");
    a.route_prefix = Some("/one/shared".into());
    let mut b = new_mock("shared");
    b.route_prefix = Some("/two/shared".into());
    let in_two = store.create_mock("ws-two", b).await.expect("two");
    let in_one = store.create_mock("ws-one", a).await.expect("one");
    assert_eq!(in_one.workspace, "ws-one");
    assert_eq!(in_two.workspace, "ws-two");

    let all = store.list_all_mocks().await.expect("list all");
    let keys: Vec<(String, String)> = all
        .iter()
        .map(|m| (m.workspace.clone(), m.name.clone()))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "(workspace, name) ascending");
    let shared: Vec<&Mock> = all.iter().filter(|m| m.name == "shared").collect();
    assert_eq!(shared, vec![&in_one, &in_two]);
    assert!(
        all.iter().any(|m| m.workspace == DEFAULT_WORKSPACE),
        "default workspace mocks are listed too"
    );
}

async fn data_plane_auth_round_trips(store: &Arc<dyn ControlStore>) {
    let fresh = create(store, "dp-default").await;
    assert_eq!(fresh.data_plane_auth, DataPlaneAuthConfig::default());
    assert_eq!(fresh.data_plane_auth.mode, DataPlaneAuthMode::None);
    assert_eq!(fresh.data_plane_auth.header, "x-api-key");

    let mut nm = new_mock("dp-keyed");
    nm.data_plane_auth = DataPlaneAuthConfig {
        mode: DataPlaneAuthMode::ApiKey,
        header: "x-key".into(),
    };
    let created = store
        .create_mock(DEFAULT_WORKSPACE, nm)
        .await
        .expect("create");
    assert_eq!(created.data_plane_auth.mode, DataPlaneAuthMode::ApiKey);
    let fetched = store
        .get_mock(DEFAULT_WORKSPACE, "dp-keyed")
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(fetched, created);

    let mut changed = fetched.clone();
    changed.data_plane_auth = DataPlaneAuthConfig {
        mode: DataPlaneAuthMode::PassthroughValidate,
        header: "authorization".into(),
    };
    let updated = store.update_mock(&changed).await.expect("update");
    assert_eq!(updated.data_plane_auth, changed.data_plane_auth);
    assert_eq!(
        store
            .get_mock(DEFAULT_WORKSPACE, "dp-keyed")
            .await
            .expect("get")
            .expect("exists")
            .data_plane_auth,
        changed.data_plane_auth
    );

    let mut bad = new_mock("dp-bad");
    bad.data_plane_auth.header = "Bad Header".into();
    let err = store
        .create_mock(DEFAULT_WORKSPACE, bad)
        .await
        .expect_err("bad header");
    assert!(matches!(err, StoreError::Invalid(_)), "got {err:?}");
    let mut bad_update = updated.clone();
    bad_update.data_plane_auth.header = "Bad Header".into();
    let err = store
        .update_mock(&bad_update)
        .await
        .expect_err("bad header");
    assert!(matches!(err, StoreError::Invalid(_)), "got {err:?}");

    // Serde defaults: an absent field is the default config.
    let parsed: DataPlaneAuthConfig = serde_json::from_value(json!({})).expect("parse");
    assert_eq!(parsed, DataPlaneAuthConfig::default());
    let parsed: DataPlaneAuthConfig =
        serde_json::from_value(json!({"mode": "passthrough_validate"})).expect("parse");
    assert_eq!(parsed.mode, DataPlaneAuthMode::PassthroughValidate);
    assert_eq!(parsed.header, "x-api-key");
}

async fn workspace_changes_are_published(store: &Arc<dyn ControlStore>) {
    let mut rx = store.subscribe();
    let ws_change = ControlChange::Workspace {
        name: "published-ws".into(),
    };
    store
        .create_workspace("published-ws", "Published")
        .await
        .expect("create");
    assert_eq!(next_change(&mut rx, "create workspace").await, ws_change);
    store
        .update_workspace("published-ws", "Renamed")
        .await
        .expect("update");
    assert_eq!(next_change(&mut rx, "update workspace").await, ws_change);

    let mut nm = new_mock("pub-mock");
    nm.route_prefix = Some("/published-ws/pub-mock".into());
    store.create_mock("published-ws", nm).await.expect("mock");
    let mock_change = ControlChange::Mock {
        workspace: "published-ws".into(),
        name: "pub-mock".into(),
    };
    assert_eq!(next_change(&mut rx, "create mock").await, mock_change);

    assert!(
        store
            .delete_workspace("published-ws", true)
            .await
            .expect("delete")
    );
    let seen = [
        next_change(&mut rx, "delete change 1").await,
        next_change(&mut rx, "delete change 2").await,
    ];
    assert!(seen.contains(&ws_change), "{seen:?}");
    assert!(seen.contains(&mock_change), "{seen:?}");
}

/// Now at microsecond precision, which every backend round-trips.
fn now_us() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(6)
}

fn api_key(ws: &str, key_id: &str, created_at: DateTime<Utc>) -> ApiKey {
    ApiKey {
        key_id: key_id.into(),
        workspace: ws.into(),
        name: format!("{key_id} key"),
        role: "editor".into(),
        scopes: vec!["admin".into(), "data-plane".into()],
        sysadmin: false,
        secret_hash: "ab".repeat(32),
        created_by: "alice".into(),
        created_at,
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
    }
}

async fn api_key_create_get_list_revoke(store: &Arc<dyn ControlStore>) {
    store
        .create_workspace("keys-ws", "Keys")
        .await
        .expect("create");
    let t0 = now_us();
    let mut older = api_key("keys-ws", "aaaaaaaaaaaa", t0 - Duration::seconds(10));
    older.expires_at = Some(t0 + Duration::days(30));
    older.sysadmin = true;
    older.role = "owner".into();
    older.scopes = vec!["data-plane".into()];
    let newer = api_key("keys-ws", "bbbbbbbbbbbb", t0);
    store.create_api_key(&older).await.expect("create older");
    store.create_api_key(&newer).await.expect("create newer");

    let err = store
        .create_api_key(&api_key("keys-ws", "aaaaaaaaaaaa", t0))
        .await
        .expect_err("duplicate key_id");
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");

    assert_eq!(
        store.get_api_key("aaaaaaaaaaaa").await.expect("get"),
        Some(older.clone())
    );
    assert_eq!(store.get_api_key("zzzzzzzzzzzz").await.expect("get"), None);

    let listed = store.list_api_keys("keys-ws").await.expect("list");
    assert_eq!(
        listed,
        vec![newer.clone(), older.clone()],
        "created_at desc"
    );
    assert!(
        store
            .list_api_keys(DEFAULT_WORKSPACE)
            .await
            .expect("list")
            .iter()
            .all(|k| k.workspace == DEFAULT_WORKSPACE)
    );

    assert!(
        !store
            .revoke_api_key(DEFAULT_WORKSPACE, "bbbbbbbbbbbb")
            .await
            .expect("revoke in other workspace"),
        "a key is revoked only through its own workspace"
    );
    assert!(
        store
            .revoke_api_key("keys-ws", "bbbbbbbbbbbb")
            .await
            .expect("revoke")
    );
    assert!(
        !store
            .revoke_api_key("keys-ws", "bbbbbbbbbbbb")
            .await
            .expect("revoke again"),
        "second revoke is false"
    );
    assert!(
        !store
            .revoke_api_key("keys-ws", "cccccccccccc")
            .await
            .expect("revoke missing")
    );
    let revoked = store
        .get_api_key("bbbbbbbbbbbb")
        .await
        .expect("get")
        .expect("revoked key is kept");
    let revoked_at = revoked.revoked_at.expect("revoked_at set");
    assert!(revoked_at >= t0, "{revoked_at} >= {t0}");

    let listed = store.list_api_keys("keys-ws").await.expect("list");
    assert_eq!(listed.len(), 2, "revoked keys are still listed");
    assert_eq!(listed[0].key_id, "bbbbbbbbbbbb");
    assert_eq!(listed[0].revoked_at, Some(revoked_at));
    assert_eq!(listed[1], older);
}

async fn api_key_in_missing_workspace_not_found(store: &Arc<dyn ControlStore>) {
    let err = store
        .create_api_key(&api_key("no-such-ws", "dddddddddddd", now_us()))
        .await
        .expect_err("missing workspace");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
    assert_eq!(store.get_api_key("dddddddddddd").await.expect("get"), None);
}

async fn api_key_touch_updates_last_used(store: &Arc<dyn ControlStore>) {
    let key = api_key(DEFAULT_WORKSPACE, "eeeeeeeeeeee", now_us());
    store.create_api_key(&key).await.expect("create");
    let at = now_us() + Duration::seconds(5);
    store
        .touch_api_key("eeeeeeeeeeee", at)
        .await
        .expect("touch");
    let touched = store
        .get_api_key("eeeeeeeeeeee")
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(touched.last_used_at, Some(at));
    assert_eq!(
        touched,
        ApiKey {
            last_used_at: Some(at),
            ..key
        },
        "only last_used_at changes"
    );
    // Touching an unknown key is not an error.
    store
        .touch_api_key("ffffffffffff", at)
        .await
        .expect("touch missing");
}

fn admin_session(id_hash: &str, expires_at: DateTime<Utc>) -> AdminSession {
    let created_at = now_us();
    AdminSession {
        id_hash: id_hash.into(),
        principal: json!({"subject": "alice", "kind": "user", "roles": {"default": "admin"}}),
        created_at,
        expires_at,
        last_seen: created_at,
    }
}

async fn admin_session_lifecycle(store: &Arc<dyn ControlStore>) {
    let s = admin_session(&"1".repeat(64), now_us() + Duration::hours(12));
    store.create_admin_session(&s).await.expect("create");
    let err = store
        .create_admin_session(&s)
        .await
        .expect_err("duplicate id");
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
    assert_eq!(
        store.get_admin_session(&s.id_hash).await.expect("get"),
        Some(s.clone())
    );
    assert_eq!(
        store.get_admin_session(&"2".repeat(64)).await.expect("get"),
        None
    );

    let seen = s.last_seen + Duration::seconds(90);
    store
        .touch_admin_session(&s.id_hash, seen)
        .await
        .expect("touch");
    assert_eq!(
        store.get_admin_session(&s.id_hash).await.expect("get"),
        Some(AdminSession {
            last_seen: seen,
            ..s.clone()
        })
    );

    assert!(
        store
            .delete_admin_session(&s.id_hash)
            .await
            .expect("delete")
    );
    assert!(
        !store
            .delete_admin_session(&s.id_hash)
            .await
            .expect("delete again")
    );
    assert_eq!(
        store.get_admin_session(&s.id_hash).await.expect("get"),
        None
    );
}

async fn expired_admin_sessions_are_swept(store: &Arc<dyn ControlStore>) {
    let now = now_us();
    let live = admin_session(&"3".repeat(64), now + Duration::hours(1));
    let expired = admin_session(&"4".repeat(64), now - Duration::seconds(1));
    store.create_admin_session(&live).await.expect("live");
    store.create_admin_session(&expired).await.expect("expired");
    assert_eq!(
        store
            .delete_expired_admin_sessions(now)
            .await
            .expect("sweep"),
        1
    );
    assert!(
        store
            .get_admin_session(&live.id_hash)
            .await
            .expect("get")
            .is_some()
    );
    assert!(
        store
            .get_admin_session(&expired.id_hash)
            .await
            .expect("get")
            .is_none()
    );
    assert_eq!(
        store
            .delete_expired_admin_sessions(now)
            .await
            .expect("sweep again"),
        0
    );
}

fn audit_event(ws: Option<&str>, actor: &str, action: &str, target: &str) -> NewAuditEvent {
    NewAuditEvent {
        workspace: ws.map(str::to_string),
        actor_subject: actor.into(),
        actor_kind: "user".into(),
        action: action.into(),
        target: target.into(),
        before: None,
        after: Some(json!({"name": target})),
        request_id: Some(format!("req-{target}")),
        at: now_us(),
    }
}

async fn audit_append_and_filter(store: &Arc<dyn ControlStore>) {
    let mut first = audit_event(Some("audit-a"), "audit-alice", "mock.create", "mock:one");
    first.before = Some(json!({"old": true}));
    first.after = None;
    first.request_id = None;
    let inputs = [
        first,
        audit_event(Some("audit-a"), "audit-bob", "mock.update", "mock:one"),
        audit_event(Some("audit-b"), "audit-alice", "mock.create", "mock:two"),
        audit_event(Some("audit-b"), "audit-bob", "profile.put", "profile:p"),
        audit_event(Some("audit-a"), "audit-alice", "mock.delete", "mock:one"),
    ];
    let mut appended = Vec::new();
    for input in inputs {
        let event = store.append_audit(input.clone()).await.expect("append");
        assert_eq!(event.workspace, input.workspace);
        assert_eq!(event.actor_subject, input.actor_subject);
        assert_eq!(event.actor_kind, input.actor_kind);
        assert_eq!(event.action, input.action);
        assert_eq!(event.target, input.target);
        assert_eq!(event.before, input.before);
        assert_eq!(event.after, input.after);
        assert_eq!(event.request_id, input.request_id);
        assert_eq!(event.at, input.at);
        appended.push(event);
    }
    assert!(
        appended.windows(2).all(|w| w[0].id < w[1].id),
        "ids increase"
    );
    let ids = |events: &[mb_store::AuditEvent]| events.iter().map(|e| e.id).collect::<Vec<_>>();
    let id = |i: usize| appended[i].id;

    let by_ws = |ws: &str| AuditFilter {
        workspace: Some(ws.into()),
        ..AuditFilter::default()
    };
    let a = store.list_audit(&by_ws("audit-a")).await.expect("list a");
    assert_eq!(ids(&a), vec![id(4), id(1), id(0)], "id desc");
    assert_eq!(a[2], appended[0], "round-trips");
    let b = store.list_audit(&by_ws("audit-b")).await.expect("list b");
    assert_eq!(ids(&b), vec![id(3), id(2)]);

    let creates = store
        .list_audit(&AuditFilter {
            action: Some("mock.create".into()),
            ..AuditFilter::default()
        })
        .await
        .expect("by action");
    assert!(creates.iter().all(|e| e.action == "mock.create"));
    let mine: Vec<i64> = ids(&creates)
        .into_iter()
        .filter(|i| appended.iter().any(|e| e.id == *i))
        .collect();
    assert_eq!(mine, vec![id(2), id(0)]);

    let alice = store
        .list_audit(&AuditFilter {
            actor: Some("audit-alice".into()),
            ..AuditFilter::default()
        })
        .await
        .expect("by actor");
    assert_eq!(ids(&alice), vec![id(4), id(2), id(0)]);

    let combined = store
        .list_audit(&AuditFilter {
            workspace: Some("audit-a".into()),
            actor: Some("audit-alice".into()),
            action: Some("mock.create".into()),
            ..AuditFilter::default()
        })
        .await
        .expect("combined");
    assert_eq!(ids(&combined), vec![id(0)]);

    // Keyset paging.
    let page1 = store
        .list_audit(&AuditFilter {
            workspace: Some("audit-a".into()),
            limit: 2,
            ..AuditFilter::default()
        })
        .await
        .expect("page 1");
    assert_eq!(ids(&page1), vec![id(4), id(1)]);
    let page2 = store
        .list_audit(&AuditFilter {
            workspace: Some("audit-a".into()),
            limit: 2,
            before_id: Some(id(1)),
            ..AuditFilter::default()
        })
        .await
        .expect("page 2");
    assert_eq!(ids(&page2), vec![id(0)]);

    // No workspace filter lists every workspace's events, newest first.
    let all = store
        .list_audit(&AuditFilter::default())
        .await
        .expect("all");
    let all_ids = ids(&all);
    assert!(all_ids.windows(2).all(|w| w[0] > w[1]), "id desc");
    for e in &appended {
        assert!(all_ids.contains(&e.id), "{} listed", e.id);
    }

    // Limit defaults and clamping.
    assert_eq!(AuditFilter::default().limit, AuditFilter::DEFAULT_LIMIT);
    let parsed: AuditFilter = serde_json::from_value(json!({})).expect("parse");
    assert_eq!(parsed.limit, 100);
    assert_eq!(parsed.effective_limit(), 100);
    let clamp = |limit| AuditFilter {
        limit,
        ..AuditFilter::default()
    };
    assert_eq!(clamp(0).effective_limit(), 1);
    assert_eq!(clamp(5000).effective_limit(), AuditFilter::MAX_LIMIT);
    assert_eq!(AuditFilter::MAX_LIMIT, 1000);
    let one = store.list_audit(&clamp(0)).await.expect("limit 0");
    assert_eq!(one.len(), 1, "limit 0 is clamped to 1");
}

async fn audit_prune_by_age(store: &Arc<dyn ControlStore>) {
    let at = |year| {
        Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0)
            .single()
            .expect("valid date")
    };
    let mut kept = Vec::new();
    for (year, target) in [(2000, "old-1"), (2001, "old-2"), (2003, "recent")] {
        let mut e = audit_event(Some("audit-prune"), "pruner", "mock.create", target);
        e.at = at(year);
        kept.push(store.append_audit(e).await.expect("append"));
    }
    assert_eq!(store.prune_audit(at(2002)).await.expect("prune"), 2);
    let left = store
        .list_audit(&AuditFilter {
            workspace: Some("audit-prune".into()),
            ..AuditFilter::default()
        })
        .await
        .expect("list");
    assert_eq!(left, vec![kept[2].clone()]);
    assert_eq!(store.prune_audit(at(2002)).await.expect("prune again"), 0);
}

async fn force_delete_workspace_removes_api_keys_keeps_audit(store: &Arc<dyn ControlStore>) {
    store
        .create_workspace("keyed-ws", "Keyed")
        .await
        .expect("create");
    let key = api_key("keyed-ws", "gggggggggggg", now_us());
    store.create_api_key(&key).await.expect("key");
    let mut nm = new_mock("keyed-mock");
    nm.route_prefix = Some("/keyed-ws/keyed-mock".into());
    store.create_mock("keyed-ws", nm).await.expect("mock");
    let event = store
        .append_audit(audit_event(
            Some("keyed-ws"),
            "alice",
            "api_key.create",
            "api_key:gggggggggggg",
        ))
        .await
        .expect("audit");

    assert!(
        store
            .delete_workspace("keyed-ws", true)
            .await
            .expect("force delete")
    );
    assert_eq!(store.get_api_key("gggggggggggg").await.expect("get"), None);
    assert!(
        store
            .list_api_keys("keyed-ws")
            .await
            .expect("list")
            .is_empty()
    );
    let audit = store
        .list_audit(&AuditFilter {
            workspace: Some("keyed-ws".into()),
            ..AuditFilter::default()
        })
        .await
        .expect("audit");
    assert_eq!(audit, vec![event], "audit outlives the workspace");

    // A key may be created again under the same id once its workspace is gone
    // and recreated.
    store
        .create_workspace("keyed-ws", "Keyed again")
        .await
        .expect("recreate");
    store.create_api_key(&key).await.expect("key again");
    assert!(
        store
            .delete_workspace("keyed-ws", false)
            .await
            .expect("delete with keys only")
    );
    assert_eq!(store.get_api_key("gggggggggggg").await.expect("get"), None);
}

fn draft_meta(note: &str) -> serde_json::Value {
    json!({"operations": [{"key": "GET /pets", "confidence": 0.8}], "note": note})
}

async fn draft_version_round_trips_meta(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "draft-meta").await;
    let meta = draft_meta("first");
    let draft = store
        .add_draft_contract_version(
            mock.id,
            new_version(ContractStatus::Draft, "draft"),
            meta.clone(),
        )
        .await
        .expect("add draft");
    assert_eq!(draft.version, 1);
    assert_eq!(draft.status, ContractStatus::Draft);
    assert_eq!(draft.draft_meta.as_ref(), Some(&meta));
    let got = store
        .get_contract_version(mock.id, 1)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(got, draft);
    let listed = store.list_contract_versions(mock.id).await.expect("list");
    assert_eq!(listed[0].draft_meta.as_ref(), Some(&meta));

    let plain = store
        .add_contract_version(mock.id, new_version(ContractStatus::Approved, "plain"))
        .await
        .expect("add plain");
    assert_eq!(plain.draft_meta, None);
    let got = store
        .get_contract_version(mock.id, plain.version)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(got.draft_meta, None);
}

async fn add_draft_requires_draft_status(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "draft-status").await;
    let err = store
        .add_draft_contract_version(
            mock.id,
            new_version(ContractStatus::Approved, "nope"),
            draft_meta("x"),
        )
        .await
        .expect_err("approved draft");
    assert!(matches!(err, StoreError::Invalid(_)), "got {err:?}");
    assert!(
        store
            .list_contract_versions(mock.id)
            .await
            .expect("list")
            .is_empty()
    );
}

async fn update_draft_replaces_source_and_meta(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "draft-update").await;
    let draft = store
        .add_draft_contract_version(
            mock.id,
            new_version(ContractStatus::Draft, "before"),
            draft_meta("before"),
        )
        .await
        .expect("add draft");
    let meta = draft_meta("after");
    let updated = store
        .update_draft_contract_version(
            mock.id,
            draft.version,
            b"after".to_vec(),
            bundle("after"),
            meta.clone(),
        )
        .await
        .expect("update draft");
    assert_eq!(updated.version, draft.version);
    assert_eq!(updated.id, draft.id);
    assert_eq!(updated.status, ContractStatus::Draft);
    assert_eq!(updated.source, b"after".to_vec());
    assert_eq!(updated.bundle, bundle("after"));
    assert_eq!(updated.draft_meta.as_ref(), Some(&meta));
    // sha256("after")
    assert_eq!(
        updated.source_digest,
        "f39592393ef0859cb196a52693d2cea00fb2df784b3c04ae54aa7cadb8e562f8"
    );
    let got = store
        .get_contract_version(mock.id, draft.version)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(got, updated);
    assert_eq!(
        store
            .list_contract_versions(mock.id)
            .await
            .expect("list")
            .len(),
        1
    );
}

async fn update_draft_rejects_non_draft(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "draft-locked").await;
    store
        .add_draft_contract_version(
            mock.id,
            new_version(ContractStatus::Draft, "d"),
            draft_meta("d"),
        )
        .await
        .expect("add draft");
    store
        .approve_contract_version(mock.id, 1)
        .await
        .expect("approve");
    let err = store
        .update_draft_contract_version(mock.id, 1, b"x".to_vec(), bundle("x"), draft_meta("x"))
        .await
        .expect_err("approved version");
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
    let err = store
        .update_draft_contract_version(mock.id, 99, b"x".to_vec(), bundle("x"), draft_meta("x"))
        .await
        .expect_err("missing version");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
    let v1 = store
        .get_contract_version(mock.id, 1)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(v1.source, b"d".to_vec());
}

async fn approve_draft_once(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "draft-approve").await;
    store
        .add_draft_contract_version(
            mock.id,
            new_version(ContractStatus::Draft, "a"),
            draft_meta("a"),
        )
        .await
        .expect("add draft");
    let approved = store
        .approve_contract_version(mock.id, 1)
        .await
        .expect("approve");
    assert_eq!(approved.status, ContractStatus::Approved);
    assert_eq!(approved.version, 1);
    assert_eq!(approved.draft_meta, Some(draft_meta("a")));
    let err = store
        .approve_contract_version(mock.id, 1)
        .await
        .expect_err("approve twice");
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
    let err = store
        .approve_contract_version(mock.id, 7)
        .await
        .expect_err("missing version");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
    let active = store
        .activate_contract_version(mock.id, 1)
        .await
        .expect("activate approved draft");
    assert_eq!(active.status, ContractStatus::Active);
}

async fn draft_writes_publish_nothing(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "draft-quiet").await;
    let mut rx = store.subscribe();
    store
        .add_draft_contract_version(
            mock.id,
            new_version(ContractStatus::Draft, "q"),
            draft_meta("q"),
        )
        .await
        .expect("add draft");
    store
        .update_draft_contract_version(mock.id, 1, b"q2".to_vec(), bundle("q2"), draft_meta("q2"))
        .await
        .expect("update draft");
    store
        .approve_contract_version(mock.id, 1)
        .await
        .expect("approve");
    let job = store
        .create_import_job(new_job(mock.id, b"doc"))
        .await
        .expect("create job");
    store
        .cancel_import_job(job.id, now_us())
        .await
        .expect("cancel");
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "draft and job writes publish no change"
    );
}

fn new_job(mock_id: Uuid, input: &[u8]) -> NewImportJob {
    NewImportJob {
        mock_id,
        kind: "markdown".into(),
        source_name: Some("petstore.md".into()),
        input: input.to_vec(),
        created_by: Some("alice".into()),
    }
}

/// Creates a job, then waits a little so the next job's `created_at` is
/// strictly later (FIFO order is by `created_at`).
async fn create_job(
    store: &Arc<dyn ControlStore>,
    mock_id: Uuid,
    input: &[u8],
) -> mb_store::ImportJob {
    let job = store
        .create_import_job(new_job(mock_id, input))
        .await
        .expect("create job");
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    job
}

/// Claims every queued job so a test starts from an empty queue (the queue is
/// shared by every mock in the store).
async fn drain_queue(store: &Arc<dyn ControlStore>) {
    while let Some(job) = store.claim_import_job(now_us()).await.expect("claim") {
        store
            .cancel_import_job(job.id, now_us())
            .await
            .expect("cancel drained job");
    }
}

async fn import_job_lifecycle(store: &Arc<dyn ControlStore>) {
    drain_queue(store).await;
    let mock = create(store, "jobs-life").await;
    let job = create_job(store, mock.id, b"# Petstore").await;
    assert_eq!(job.workspace, DEFAULT_WORKSPACE);
    assert_eq!(job.mock_id, mock.id);
    assert_eq!(job.mock, "jobs-life");
    assert_eq!(job.kind, "markdown");
    assert_eq!(job.source_name.as_deref(), Some("petstore.md"));
    assert_eq!(job.status, ImportJobStatus::Queued);
    assert_eq!(
        job.progress,
        ImportProgress {
            phase: "queued".into(),
            done: 0,
            total: 0
        }
    );
    assert_eq!(job.created_by.as_deref(), Some("alice"));
    assert_eq!(job.result_version, None);
    assert_eq!(job.error, None);
    assert_eq!(job.started_at, None);
    assert_eq!(job.finished_at, None);
    assert_eq!(job.updated_at, job.created_at);
    assert_eq!(
        store.get_import_job(job.id).await.expect("get"),
        Some(job.clone())
    );
    assert_eq!(
        store.import_job_input(job.id).await.expect("input"),
        Some(b"# Petstore".to_vec())
    );

    let t1 = now_us();
    let claimed = store
        .claim_import_job(t1)
        .await
        .expect("claim")
        .expect("a queued job");
    assert_eq!(claimed.id, job.id);
    assert_eq!(claimed.status, ImportJobStatus::Running);
    assert_eq!(claimed.started_at, Some(t1));
    assert_eq!(claimed.updated_at, t1);

    let t2 = t1 + Duration::seconds(1);
    let progress = ImportProgress {
        phase: "analyzing".into(),
        done: 2,
        total: 5,
    };
    assert!(
        store
            .update_import_progress(job.id, &progress, t2)
            .await
            .expect("progress")
    );
    let got = store
        .get_import_job(job.id)
        .await
        .expect("get")
        .expect("job");
    assert_eq!(got.progress, progress);
    assert_eq!(got.updated_at, t2);
    assert_eq!(
        store.import_job_input(job.id).await.expect("input"),
        Some(b"# Petstore".to_vec()),
        "input kept while running"
    );

    let t3 = t2 + Duration::seconds(1);
    assert!(
        store
            .finish_import_job(job.id, ImportOutcome::Done { version: 3 }, t3)
            .await
            .expect("finish")
    );
    let done = store
        .get_import_job(job.id)
        .await
        .expect("get")
        .expect("job");
    assert_eq!(done.status, ImportJobStatus::Done);
    assert!(done.status.is_finished());
    assert_eq!(done.result_version, Some(3));
    assert_eq!(done.error, None);
    assert_eq!(done.finished_at, Some(t3));
    assert_eq!(done.updated_at, t3);
    assert_eq!(store.import_job_input(job.id).await.expect("input"), None);

    let failing = create_job(store, mock.id, b"x").await;
    store.claim_import_job(now_us()).await.expect("claim");
    assert!(
        store
            .finish_import_job(
                failing.id,
                ImportOutcome::Failed {
                    error: "no operations found".into()
                },
                now_us()
            )
            .await
            .expect("finish failed")
    );
    let failed = store
        .get_import_job(failing.id)
        .await
        .expect("get")
        .expect("job");
    assert_eq!(failed.status, ImportJobStatus::Failed);
    assert_eq!(failed.error.as_deref(), Some("no operations found"));
    assert_eq!(failed.result_version, None);
    assert_eq!(
        store.import_job_input(failing.id).await.expect("input"),
        None
    );

    assert_eq!(
        store.get_import_job(Uuid::new_v4()).await.expect("get"),
        None
    );
    assert_eq!(
        store.import_job_input(Uuid::new_v4()).await.expect("input"),
        None
    );
}

async fn claim_is_fifo_and_exclusive(store: &Arc<dyn ControlStore>) {
    drain_queue(store).await;
    let mock = create(store, "jobs-fifo").await;
    let first = create_job(store, mock.id, b"1").await;
    let second = create_job(store, mock.id, b"2").await;
    let a = store
        .claim_import_job(now_us())
        .await
        .expect("claim")
        .expect("first");
    let b = store
        .claim_import_job(now_us())
        .await
        .expect("claim")
        .expect("second");
    assert_eq!(a.id, first.id);
    assert_eq!(b.id, second.id);
    assert_eq!(store.claim_import_job(now_us()).await.expect("claim"), None);

    let only = create_job(store, mock.id, b"3").await;
    let (x, y) = tokio::join!(
        store.claim_import_job(now_us()),
        store.claim_import_job(now_us())
    );
    let claimed: Vec<_> = [x.expect("claim x"), y.expect("claim y")]
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(claimed.len(), 1, "exactly one claim wins");
    assert_eq!(claimed[0].id, only.id);
    // Leave no running job behind for later cases.
    for id in [first.id, second.id, only.id] {
        assert!(store.cancel_import_job(id, now_us()).await.expect("cancel"));
    }
}

async fn progress_and_finish_need_running(store: &Arc<dyn ControlStore>) {
    drain_queue(store).await;
    let mock = create(store, "jobs-queued").await;
    let job = create_job(store, mock.id, b"q").await;
    let progress = ImportProgress {
        phase: "extracting".into(),
        done: 1,
        total: 1,
    };
    assert!(
        !store
            .update_import_progress(job.id, &progress, now_us())
            .await
            .expect("progress")
    );
    assert!(
        !store
            .finish_import_job(job.id, ImportOutcome::Done { version: 1 }, now_us())
            .await
            .expect("finish")
    );
    let got = store
        .get_import_job(job.id)
        .await
        .expect("get")
        .expect("job");
    assert_eq!(got.status, ImportJobStatus::Queued);
    assert_eq!(got.progress, job.progress);
    assert!(
        !store
            .update_import_progress(Uuid::new_v4(), &progress, now_us())
            .await
            .expect("progress unknown")
    );
    drain_queue(store).await;
}

async fn cancel_queued_and_running(store: &Arc<dyn ControlStore>) {
    drain_queue(store).await;
    let mock = create(store, "jobs-cancel").await;
    let running = create_job(store, mock.id, b"r").await;
    store.claim_import_job(now_us()).await.expect("claim");
    let queued = create_job(store, mock.id, b"q").await;
    let later = create_job(store, mock.id, b"l").await;

    for job in [&running, &queued] {
        let at = now_us();
        assert!(store.cancel_import_job(job.id, at).await.expect("cancel"));
        let got = store
            .get_import_job(job.id)
            .await
            .expect("get")
            .expect("job");
        assert_eq!(got.status, ImportJobStatus::Canceled);
        assert!(got.status.is_finished());
        assert_eq!(got.finished_at, Some(at));
        assert_eq!(store.import_job_input(job.id).await.expect("input"), None);
    }
    assert!(
        !store
            .cancel_import_job(queued.id, now_us())
            .await
            .expect("cancel again")
    );

    // The canceled queued job is skipped.
    let claimed = store
        .claim_import_job(now_us())
        .await
        .expect("claim")
        .expect("later job");
    assert_eq!(claimed.id, later.id);
    store
        .finish_import_job(later.id, ImportOutcome::Done { version: 1 }, now_us())
        .await
        .expect("finish");
    assert!(
        !store
            .cancel_import_job(later.id, now_us())
            .await
            .expect("cancel done")
    );
    assert_eq!(
        store
            .get_import_job(later.id)
            .await
            .expect("get")
            .expect("job")
            .status,
        ImportJobStatus::Done
    );
    assert!(
        !store
            .cancel_import_job(Uuid::new_v4(), now_us())
            .await
            .expect("cancel unknown")
    );
}

async fn stale_running_jobs_fail(store: &Arc<dyn ControlStore>) {
    drain_queue(store).await;
    let mock = create(store, "jobs-stale").await;
    let job = create_job(store, mock.id, b"s").await;
    let queued = create_job(store, mock.id, b"keep").await;
    let now = now_us();
    let claimed = store
        .claim_import_job(now)
        .await
        .expect("claim")
        .expect("job");
    assert_eq!(claimed.id, job.id);
    // Nothing is older than `now - 1s`.
    assert_eq!(
        store
            .fail_stale_import_jobs(now - Duration::seconds(1), now)
            .await
            .expect("stale none"),
        0
    );
    let later = now + Duration::seconds(5);
    assert_eq!(
        store
            .fail_stale_import_jobs(now + Duration::seconds(1), later)
            .await
            .expect("stale"),
        1
    );
    let failed = store
        .get_import_job(job.id)
        .await
        .expect("get")
        .expect("job");
    assert_eq!(failed.status, ImportJobStatus::Failed);
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|e| e.contains("interrupted")),
        "error {:?}",
        failed.error
    );
    assert_eq!(failed.finished_at, Some(later));
    assert_eq!(store.import_job_input(job.id).await.expect("input"), None);
    // Queued jobs are not stale, however old.
    assert_eq!(
        store
            .get_import_job(queued.id)
            .await
            .expect("get")
            .expect("job")
            .status,
        ImportJobStatus::Queued
    );
    drain_queue(store).await;
}

async fn prune_finished_jobs(store: &Arc<dyn ControlStore>) {
    drain_queue(store).await;
    let mock = create(store, "jobs-prune").await;
    let old = create_job(store, mock.id, b"old").await;
    store.claim_import_job(now_us()).await.expect("claim");
    let t0 = now_us() - Duration::days(8);
    store
        .finish_import_job(old.id, ImportOutcome::Done { version: 1 }, t0)
        .await
        .expect("finish old");
    let recent = create_job(store, mock.id, b"recent").await;
    store
        .cancel_import_job(recent.id, now_us())
        .await
        .expect("cancel");
    let queued = create_job(store, mock.id, b"queued").await;

    let cutoff = now_us() - Duration::days(7);
    assert_eq!(store.prune_import_jobs(cutoff).await.expect("prune"), 1);
    assert_eq!(store.get_import_job(old.id).await.expect("get"), None);
    assert!(
        store
            .get_import_job(recent.id)
            .await
            .expect("get")
            .is_some()
    );
    assert!(
        store
            .get_import_job(queued.id)
            .await
            .expect("get")
            .is_some()
    );
    // A cutoff in the future prunes every finished job (earlier cases' too),
    // never a queued one.
    assert!(
        store
            .prune_import_jobs(now_us() + Duration::days(1))
            .await
            .expect("prune all")
            >= 1
    );
    assert_eq!(store.get_import_job(recent.id).await.expect("get"), None);
    assert!(
        store
            .get_import_job(queued.id)
            .await
            .expect("get")
            .is_some()
    );
    drain_queue(store).await;
}

async fn jobs_listed_newest_first_and_cascade_with_mock(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "jobs-list").await;
    let other = create(store, "jobs-other").await;
    let mut ids = Vec::new();
    for i in 0..3u8 {
        ids.push(create_job(store, mock.id, &[i]).await.id);
    }
    let other_job = create_job(store, other.id, b"o").await;
    let listed: Vec<Uuid> = store
        .list_import_jobs(mock.id, 50)
        .await
        .expect("list")
        .into_iter()
        .map(|j| j.id)
        .collect();
    ids.reverse();
    assert_eq!(listed, ids);
    assert_eq!(
        store
            .list_import_jobs(mock.id, 2)
            .await
            .expect("list")
            .len(),
        2
    );
    assert_eq!(
        store
            .list_import_jobs(mock.id, 0)
            .await
            .expect("list 0")
            .len(),
        1,
        "limit is clamped to at least 1"
    );

    assert!(
        store
            .delete_mock(DEFAULT_WORKSPACE, "jobs-list")
            .await
            .expect("delete")
    );
    assert!(
        store
            .list_import_jobs(mock.id, 50)
            .await
            .expect("list")
            .is_empty()
    );
    for id in &ids {
        assert_eq!(store.get_import_job(*id).await.expect("get"), None);
    }
    assert!(
        store
            .get_import_job(other_job.id)
            .await
            .expect("get")
            .is_some()
    );
    drain_queue(store).await;
}

async fn import_job_for_missing_mock_not_found(store: &Arc<dyn ControlStore>) {
    let err = store
        .create_import_job(new_job(Uuid::new_v4(), b"x"))
        .await
        .expect_err("missing mock");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
}

async fn complete_import_job_stores_draft_only_while_running(store: &Arc<dyn ControlStore>) {
    drain_queue(store).await;
    let mock = create(store, "jobs-complete").await;
    let job = create_job(store, mock.id, b"c").await;
    store.claim_import_job(now_us()).await.expect("claim");
    let at = now_us();
    let meta = draft_meta("imported");
    let draft = store
        .complete_import_job(
            job.id,
            mock.id,
            new_version(ContractStatus::Draft, "imported"),
            meta.clone(),
            at,
        )
        .await
        .expect("complete")
        .expect("the job was running");
    assert_eq!(draft.version, 1);
    assert_eq!(draft.status, ContractStatus::Draft);
    assert_eq!(draft.draft_meta.as_ref(), Some(&meta));
    let done = store
        .get_import_job(job.id)
        .await
        .expect("get")
        .expect("job");
    assert_eq!(done.status, ImportJobStatus::Done);
    assert_eq!(done.result_version, Some(1));
    assert_eq!(done.finished_at, Some(at));
    assert_eq!(store.import_job_input(job.id).await.expect("input"), None);

    // A job that is no longer running (canceled, failed as stale, queued,
    // already done or unknown) stores nothing.
    let canceled = create_job(store, mock.id, b"x").await;
    store.claim_import_job(now_us()).await.expect("claim");
    assert!(
        store
            .cancel_import_job(canceled.id, now_us())
            .await
            .expect("cancel")
    );
    let queued = create_job(store, mock.id, b"q").await;
    for id in [canceled.id, queued.id, job.id, Uuid::new_v4()] {
        let got = store
            .complete_import_job(
                id,
                mock.id,
                new_version(ContractStatus::Draft, "late"),
                draft_meta("late"),
                now_us(),
            )
            .await
            .expect("complete");
        assert_eq!(got, None, "job {id}");
    }
    let versions = store.list_contract_versions(mock.id).await.expect("list");
    assert_eq!(versions.len(), 1, "{versions:?}");
    let got = store
        .get_import_job(canceled.id)
        .await
        .expect("get")
        .expect("job");
    assert_eq!(got.status, ImportJobStatus::Canceled);
    assert_eq!(got.result_version, None);
    drain_queue(store).await;
}

async fn active_import_jobs_counted_per_mock(store: &Arc<dyn ControlStore>) {
    drain_queue(store).await;
    let mock = create(store, "jobs-active").await;
    let other = create(store, "jobs-active-other").await;
    assert_eq!(
        store
            .count_active_import_jobs(mock.id)
            .await
            .expect("count"),
        0
    );
    let first = create_job(store, mock.id, b"1").await;
    let second = create_job(store, mock.id, b"2").await;
    let third = create_job(store, mock.id, b"3").await;
    create_job(store, other.id, b"o").await;
    assert_eq!(
        store
            .count_active_import_jobs(mock.id)
            .await
            .expect("count"),
        3
    );
    // Running jobs count; finished and canceled ones do not.
    let claimed = store
        .claim_import_job(now_us())
        .await
        .expect("claim")
        .expect("job");
    assert_eq!(claimed.id, first.id);
    assert_eq!(
        store
            .count_active_import_jobs(mock.id)
            .await
            .expect("count"),
        3
    );
    store
        .finish_import_job(
            first.id,
            ImportOutcome::Failed { error: "x".into() },
            now_us(),
        )
        .await
        .expect("finish");
    store
        .cancel_import_job(second.id, now_us())
        .await
        .expect("cancel");
    assert_eq!(
        store
            .count_active_import_jobs(mock.id)
            .await
            .expect("count"),
        1
    );
    assert_eq!(
        store
            .count_active_import_jobs(other.id)
            .await
            .expect("count"),
        1
    );
    store
        .cancel_import_job(third.id, now_us())
        .await
        .expect("cancel");
    drain_queue(store).await;
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
