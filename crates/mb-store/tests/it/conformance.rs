//! Backend-neutral `ControlStore` conformance suite. Every backend's test file
//! expands [`conformance_checks`] into one test per check, each on its own
//! fresh store, so all backends run identical assertions and one failing
//! check never hides the others.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, SubsecRound, TimeZone, Utc};
use mb_mcm::Bundle;
use mb_mcm::Trigger;
use mb_store::{
    AdminSession, AdminSessionGrant, ApiKey, AuditFilter, ChannelSettings, ChaosSettings,
    Consistency, ContractStatus, ControlChange, ControlStore, DEFAULT_WORKSPACE,
    DataPlaneAuthConfig, DataPlaneAuthMode, ImportJobStatus, ImportOutcome, ImportProgress,
    LatencyJitter, LlmProfile, Mock, MockBehavior, MockEventSettings, MockLimits, NewAuditEvent,
    NewContractVersion, NewImportJob, NewMock, NewRequestLogEntry, OidcPendingLogin,
    RETENTION_PRUNE_INTERVAL, RateLimit, RequestKind, RequestLogFilter, ResourceSource, Scenario,
    ScenarioRules, StoreError, UnknownIds, WebhookSigning, WebhookTarget,
};
use serde_json::json;
use uuid::Uuid;

/// Calls `$callback! { check, ... }` with every check of the suite, so each
/// backend generates one test per check (each on a fresh store) and a new
/// check runs on every backend.
macro_rules! conformance_checks {
    ($callback:ident) => {
        $callback! {
            ping,
            mock_crud_and_conflict,
            mock_behavior_round_trips,
            invalid_mock_name_rejected,
            mock_hosts_round_trip,
            mock_webhooks_round_trip,
            contract_versions_increment,
            activation_is_exclusive_and_rolls_back,
            draft_cannot_activate,
            contract_version_delete,
            delete_mock_cascades,
            scenario_upsert_and_delete,
            profile_crud,
            request_log_filters_and_paginates,
            request_log_retention_prunes,
            request_log_retention_prunes_interleaved_mocks,
            changes_are_published,
            default_workspace_exists,
            workspace_crud_and_conflict,
            invalid_workspace_name_rejected,
            delete_default_workspace_conflicts,
            delete_nonempty_workspace_requires_force,
            mock_in_missing_workspace_not_found,
            same_mock_name_in_two_workspaces,
            data_plane_auth_round_trips,
            workspace_changes_are_published,
            api_key_create_get_list_revoke,
            api_key_in_missing_workspace_not_found,
            api_key_touch_updates_last_used,
            admin_session_lifecycle,
            expired_admin_sessions_are_swept,
            admin_sessions_listed_and_revoked_by_subject,
            idle_admin_sessions_are_swept,
            admin_session_grant_round_trips,
            admin_session_refresh_is_claimed_once,
            oidc_pending_login_is_taken_once,
            expired_oidc_pending_logins_are_not_returned,
            oidc_pending_logins_are_bounded,
            audit_append_and_filter,
            audit_prune_by_age,
            force_delete_workspace_removes_api_keys_keeps_audit,
            draft_version_round_trips_meta,
            add_draft_requires_draft_status,
            update_draft_replaces_source_and_meta,
            update_draft_rejects_non_draft,
            approve_draft_once,
            draft_writes_publish_nothing,
            import_job_lifecycle,
            claim_is_fifo_and_exclusive,
            progress_and_finish_need_running,
            cancel_queued_and_running,
            stale_running_jobs_fail,
            prune_finished_jobs,
            jobs_listed_newest_first_and_cascade_with_mock,
            import_job_for_missing_mock_not_found,
            complete_import_job_stores_draft_only_while_running,
            active_import_jobs_counted_per_mock,
            channel_settings_crud,
            event_settings_upsert,
            scheduler_lease_exclusive,
            delete_mock_cascades_event_tables,
            request_log_kind_filter,
            mock_chaos_and_limits_round_trip,
            invalid_chaos_rejected,
            scenario_chaos_round_trip,
            token_usage_accumulates_per_day,
            token_usage_cascades_with_mock,
            token_usage_pruned_before_day,
            get_request_log_by_id,
            contract_version_warnings_round_trip,
            import_job_mode_round_trips,
            active_versions_in_one_call,
            active_bundle_leaves_out_the_source,
            active_bundles_in_one_call,
            request_log_prune_keeps_exactly_the_newest,
            contract_version_infos_match_the_versions,
            latest_contract_version_by_status,
            request_log_summaries_match_the_entries,
        }
    };
}
pub(crate) use conformance_checks;

pub(crate) async fn ping(store: &Arc<dyn ControlStore>) {
    store.ping().await.expect("ping");
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
        chaos: None,
        limits: MockLimits::default(),
        behavior: Default::default(),
        hosts: vec![],
        webhooks: vec![],
    }
}

pub(crate) async fn mock_webhooks_round_trip(store: &Arc<dyn ControlStore>) {
    let mut nm = new_mock("hooks");
    nm.webhooks = vec![WebhookTarget {
        url: "http://127.0.0.1:4242/hook".into(),
        events: vec!["createOrder".into()],
        secret: Some("env:SHOP_WEBHOOK_SECRET".into()),
        signing: Some(WebhookSigning::Stripe),
        delay_ms: 250,
    }];
    let m = store
        .create_mock(DEFAULT_WORKSPACE, nm.clone())
        .await
        .expect("webhooks");
    assert_eq!(m.webhooks, nm.webhooks);
    assert_eq!(
        store
            .get_mock(DEFAULT_WORKSPACE, "hooks")
            .await
            .expect("webhooks")
            .expect("webhooks")
            .webhooks,
        nm.webhooks
    );
    let mut cleared = m.clone();
    cleared.webhooks.clear();
    assert!(
        store
            .update_mock(&cleared)
            .await
            .expect("webhooks")
            .webhooks
            .is_empty()
    );
    // A mock without webhooks serialises without the key.
    let stored = store
        .get_mock(DEFAULT_WORKSPACE, "hooks")
        .await
        .expect("webhooks")
        .expect("webhooks");
    assert!(stored.webhooks.is_empty());
    assert!(
        serde_json::to_value(&stored)
            .expect("webhooks")
            .get("webhooks")
            .is_none()
    );
    let mut bad = new_mock("hooks-bad");
    bad.webhooks = vec![WebhookTarget {
        url: "nope".into(),
        ..nm.webhooks[0].clone()
    }];
    assert!(matches!(
        store.create_mock(DEFAULT_WORKSPACE, bad).await,
        Err(StoreError::Invalid(_))
    ));
    let mut stored = stored;
    stored.webhooks = vec![WebhookTarget {
        url: "nope".into(),
        ..nm.webhooks[0].clone()
    }];
    assert!(matches!(
        store.update_mock(&stored).await,
        Err(StoreError::Invalid(_))
    ));
    store
        .delete_mock(DEFAULT_WORKSPACE, "hooks")
        .await
        .expect("webhooks");
}

pub(crate) async fn mock_hosts_round_trip(store: &Arc<dyn ControlStore>) {
    let mut new = new_mock("hosted");
    new.route_prefix = Some("/".into());
    new.hosts = vec!["api.github.com".into()];
    let m = store
        .create_mock(DEFAULT_WORKSPACE, new)
        .await
        .expect("create");
    assert_eq!(m.hosts, ["api.github.com"]);
    let got = store
        .get_mock(DEFAULT_WORKSPACE, "hosted")
        .await
        .expect("get")
        .expect("some");
    assert_eq!(got.hosts, ["api.github.com"]);
    let updated = store
        .update_mock(&Mock {
            hosts: vec![],
            ..got.clone()
        })
        .await
        .expect("update");
    assert!(updated.hosts.is_empty());
    let bad = store
        .update_mock(&Mock {
            hosts: vec!["Bad:1".into()],
            ..got
        })
        .await;
    assert!(matches!(bad, Err(StoreError::Invalid(_))));
    assert!(
        store
            .delete_mock(DEFAULT_WORKSPACE, "hosted")
            .await
            .expect("delete")
    );
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
        webhooks: Vec::new(),
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

pub(crate) async fn mock_behavior_round_trips(store: &Arc<dyn ControlStore>) {
    let mut mock = create(store, "behaving").await;
    assert_eq!(mock.behavior, MockBehavior::default());
    mock.behavior = MockBehavior {
        consistency: Consistency::Off,
        unknown_ids: UnknownIds::NotFound,
        default_scenario: Some("calm".into()),
        path_rewrite: mb_store::PathRewrite {
            strip_prefix: Some("/api".into()),
            add_prefix: Some("/3".into()),
        },
        graphql_path: Some("/graphql.json".into()),
    };
    store.update_mock(&mock).await.expect("update");
    let got = store
        .get_mock(DEFAULT_WORKSPACE, "behaving")
        .await
        .expect("get")
        .expect("mock");
    assert_eq!(got.behavior, mock.behavior);
    let rules = ScenarioRules {
        operation_instructions: BTreeMap::from([("getOrder".into(), "A gift.".into())]),
        ..ScenarioRules::default()
    };
    store
        .put_scenario(&Scenario {
            mock_id: got.id,
            name: "calm".into(),
            instructions: None,
            rules: rules.clone(),
            source: ResourceSource::Api,
        })
        .await
        .expect("put scenario");
    assert_eq!(
        store.list_scenarios(got.id).await.expect("list")[0].rules,
        rules
    );
    assert_eq!(
        serde_json::to_value(MockBehavior::default()).expect("serialize"),
        json!({"consistency": "on", "unknown_ids": "generate"})
    );
    let parsed: MockBehavior =
        serde_json::from_value(json!({"consistency": false, "unknown_ids": 404}))
            .expect("deserialize");
    assert_eq!(
        (parsed.consistency, parsed.unknown_ids),
        (Consistency::Off, UnknownIds::NotFound)
    );
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
        fallback_reason: None,
        latency_ms: 12,
        prompt_tokens: 3,
        completion_tokens: 4,
        request: json!({"q": 1}),
        response: Some(json!({"ok": true})),
        at: Utc::now(),
        kind: RequestKind::Http,
    }
}

pub(crate) async fn mock_crud_and_conflict(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn invalid_mock_name_rejected(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn contract_versions_increment(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn activation_is_exclusive_and_rolls_back(store: &Arc<dyn ControlStore>) {
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
            .map(|v| v.status)
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

pub(crate) async fn contract_version_delete(store: &Arc<dyn ControlStore>) {
    drain_queue(store).await;
    let mock = create(store, "deletable").await;
    store
        .add_contract_version(mock.id, new_version(ContractStatus::Active, "a"))
        .await
        .expect("add v1");
    let job = create_job(store, mock.id, b"d").await;
    store.claim_import_job(now_us()).await.expect("claim");
    let draft = store
        .complete_import_job(
            job.id,
            mock.id,
            new_version(ContractStatus::Draft, "rejected"),
            draft_meta("rejected"),
            now_us(),
        )
        .await
        .expect("complete")
        .expect("the job was running");
    assert_eq!(draft.version, 2);

    let err = store
        .delete_contract_version(mock.id, 1)
        .await
        .expect_err("the active version");
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
    assert!(
        !store
            .delete_contract_version(mock.id, 9)
            .await
            .expect("missing version")
    );

    assert!(
        store
            .delete_contract_version(mock.id, 2)
            .await
            .expect("delete the draft")
    );
    let versions = store.list_contract_versions(mock.id).await.expect("list");
    assert_eq!(
        versions.iter().map(|v| v.version).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(versions[0].status, ContractStatus::Active);
    // The job whose result it was goes with it.
    assert!(store.get_import_job(job.id).await.expect("get").is_none());
    assert!(
        !store
            .delete_contract_version(mock.id, 2)
            .await
            .expect("already deleted")
    );
}

pub(crate) async fn draft_cannot_activate(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn delete_mock_cascades(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn scenario_upsert_and_delete(store: &Arc<dyn ControlStore>) {
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
            chaos: None,
            operation_instructions: Default::default(),
            headers: BTreeMap::from([("Retry-After".into(), "30".into())]),
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

pub(crate) async fn profile_crud(store: &Arc<dyn ControlStore>) {
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
        headers: Default::default(),
        extra_body: Default::default(),
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
    p.headers = [
        (
            "cf-aig-authorization".to_string(),
            "Bearer ${env:CF_AIG_API_KEY}".to_string(),
        ),
        ("x-title".to_string(), "Mockingbird".to_string()),
    ]
    .into();
    p.extra_body = json!({"provider": {"order": ["atlas-cloud"]}, "reasoning": {"enabled": false}})
        .as_object()
        .cloned()
        .unwrap_or_default();
    store.put_profile(&p).await.expect("upsert");
    assert_eq!(
        store
            .get_profile(DEFAULT_WORKSPACE, "fast")
            .await
            .expect("get")
            .expect("present")
            .headers,
        p.headers,
        "headers round-trip"
    );
    assert_eq!(
        store
            .get_profile(DEFAULT_WORKSPACE, "fast")
            .await
            .expect("get")
            .expect("present")
            .extra_body,
        p.extra_body,
        "extra_body round-trip"
    );
    let slow = LlmProfile {
        name: "slow".into(),
        headers: Default::default(),
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

pub(crate) async fn request_log_filters_and_paginates(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn request_log_retention_prunes(store: &Arc<dyn ControlStore>) {
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
pub(crate) async fn request_log_retention_prunes_interleaved_mocks(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn changes_are_published(store: &Arc<dyn ControlStore>) {
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
        headers: Default::default(),
        extra_body: Default::default(),
        source: ResourceSource::Api,
    }
}

pub(crate) async fn default_workspace_exists(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn workspace_crud_and_conflict(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn invalid_workspace_name_rejected(store: &Arc<dyn ControlStore>) {
    let err = store
        .create_workspace("Team A", "Team A")
        .await
        .expect_err("invalid name");
    assert!(matches!(err, StoreError::Invalid(_)), "got {err:?}");
    assert!(store.get_workspace("Team A").await.expect("get").is_none());
}

pub(crate) async fn delete_default_workspace_conflicts(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn delete_nonempty_workspace_requires_force(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn mock_in_missing_workspace_not_found(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn same_mock_name_in_two_workspaces(store: &Arc<dyn ControlStore>) {
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
    // Created out of (workspace, name) order, so the listing must sort.
    let in_two = store.create_mock("ws-two", b).await.expect("two");
    create(store, "in-default").await;
    let in_one = store.create_mock("ws-one", a).await.expect("one");
    assert_eq!(in_one.workspace, "ws-one");
    assert_eq!(in_two.workspace, "ws-two");

    let all = store.list_all_mocks().await.expect("list all");
    let keys: Vec<(&str, &str)> = all
        .iter()
        .map(|m| (m.workspace.as_str(), m.name.as_str()))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted, "(workspace, name) ascending");
    // Other checks may share the store (the Postgres suite), so look only at
    // this check's mocks.
    let mine: Vec<(&str, &str)> = keys
        .into_iter()
        .filter(|&(ws, name)| ws.starts_with("ws-") || name == "in-default")
        .collect();
    assert_eq!(
        mine,
        [
            (DEFAULT_WORKSPACE, "in-default"),
            ("ws-one", "shared"),
            ("ws-two", "shared")
        ],
        "every workspace's mocks, default included"
    );
    let shared: Vec<&Mock> = all.iter().filter(|m| m.name == "shared").collect();
    assert_eq!(shared, vec![&in_one, &in_two]);
}

pub(crate) async fn data_plane_auth_round_trips(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn workspace_changes_are_published(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn api_key_create_get_list_revoke(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn api_key_in_missing_workspace_not_found(store: &Arc<dyn ControlStore>) {
    let err = store
        .create_api_key(&api_key("no-such-ws", "dddddddddddd", now_us()))
        .await
        .expect_err("missing workspace");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
    assert_eq!(store.get_api_key("dddddddddddd").await.expect("get"), None);
}

pub(crate) async fn api_key_touch_updates_last_used(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn admin_session_lifecycle(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn expired_admin_sessions_are_swept(store: &Arc<dyn ControlStore>) {
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

fn admin_session_of(
    id_hash: &str,
    subject: &str,
    created_at: DateTime<Utc>,
    last_seen: DateTime<Utc>,
) -> AdminSession {
    AdminSession {
        id_hash: id_hash.into(),
        principal: json!({"subject": subject, "kind": "user", "roles": {}}),
        created_at,
        expires_at: created_at + Duration::hours(12),
        last_seen,
    }
}

pub(crate) async fn admin_sessions_listed_and_revoked_by_subject(store: &Arc<dyn ControlStore>) {
    // Subjects of their own: on Postgres every check shares one store.
    let now = now_us();
    let a1 = admin_session_of(&"a".repeat(64), "list-alice", now - Duration::hours(2), now);
    let a2 = admin_session_of(&"b".repeat(64), "list-alice", now - Duration::hours(1), now);
    let b1 = admin_session_of(&"c".repeat(64), "list-bob", now, now);
    // A subject that only looks like a pattern matching another.
    let tricky = admin_session_of(
        &"d".repeat(64),
        "list-ali%",
        now - Duration::minutes(30),
        now,
    );
    let mine = [&a1, &a2, &b1, &tricky];
    for s in mine {
        store.create_admin_session(s).await.expect("create");
    }
    let listed_of_mine = |all: Vec<AdminSession>| -> Vec<AdminSession> {
        all.into_iter()
            .filter(|s| mine.iter().any(|m| m.id_hash == s.id_hash))
            .collect()
    };

    // Newest (`created_at`) first.
    let all = store.list_admin_sessions(None).await.expect("list all");
    assert_eq!(
        listed_of_mine(all),
        vec![b1.clone(), tricky.clone(), a2.clone(), a1.clone()]
    );
    assert_eq!(
        store
            .list_admin_sessions(Some("list-alice"))
            .await
            .expect("list"),
        vec![a2.clone(), a1.clone()]
    );
    assert_eq!(
        store
            .list_admin_sessions(Some("list-ali%"))
            .await
            .expect("list"),
        vec![tricky.clone()]
    );
    assert!(
        store
            .list_admin_sessions(Some("list-nobody"))
            .await
            .expect("list")
            .is_empty()
    );

    assert_eq!(
        store
            .delete_admin_sessions_of("list-alice")
            .await
            .expect("revoke"),
        2
    );
    assert_eq!(
        store
            .delete_admin_sessions_of("list-alice")
            .await
            .expect("revoke again"),
        0
    );
    let all = store.list_admin_sessions(None).await.expect("list");
    assert_eq!(listed_of_mine(all), vec![b1, tricky]);
}

pub(crate) async fn admin_session_grant_round_trips(store: &Arc<dyn ControlStore>) {
    let now = now_us();
    let s = admin_session_of(&"5".repeat(64), "grant-alice", now, now);
    store.create_admin_session(&s).await.expect("create");
    // A session starts without a grant.
    assert_eq!(
        store
            .get_admin_session_with_grant(&s.id_hash)
            .await
            .expect("get"),
        Some((s.clone(), None))
    );
    assert_eq!(
        store
            .get_admin_session_with_grant(&"6".repeat(64))
            .await
            .expect("get missing"),
        None
    );

    let principal = json!({"subject": "grant-alice", "kind": "user", "roles": {"team": "editor"}});
    let grant = AdminSessionGrant {
        claims: json!({"sub": "grant-alice", "groups": ["mb:team:editor"]}),
        mapping_hash: "abc".into(),
        refresh_token: Some("sealed".into()),
        refreshed_at: now,
    };
    assert!(
        store
            .put_admin_session_grant(&s.id_hash, &principal, &grant)
            .await
            .expect("put")
    );
    let (got, got_grant) = store
        .get_admin_session_with_grant(&s.id_hash)
        .await
        .expect("get")
        .expect("session");
    assert_eq!(got.principal, principal);
    assert_eq!(got_grant, Some(grant.clone()));
    // The plain read sees the new principal too.
    assert_eq!(
        store
            .get_admin_session(&s.id_hash)
            .await
            .expect("get")
            .expect("session")
            .principal,
        principal
    );

    // Without a refresh token.
    let without = AdminSessionGrant {
        refresh_token: None,
        ..grant
    };
    store
        .put_admin_session_grant(&s.id_hash, &principal, &without)
        .await
        .expect("put");
    assert_eq!(
        store
            .get_admin_session_with_grant(&s.id_hash)
            .await
            .expect("get")
            .and_then(|(_, g)| g),
        Some(without.clone())
    );
    assert!(
        !store
            .put_admin_session_grant(&"6".repeat(64), &principal, &without)
            .await
            .expect("put missing")
    );
}

pub(crate) async fn admin_session_refresh_is_claimed_once(store: &Arc<dyn ControlStore>) {
    let now = now_us();
    let s = admin_session_of(&"7".repeat(64), "grant-bob", now, now);
    store.create_admin_session(&s).await.expect("create");
    // Without a grant there is nothing to refresh.
    assert!(
        !store
            .claim_admin_session_refresh(&s.id_hash, now, now)
            .await
            .expect("claim")
    );
    let seen = now - Duration::minutes(10);
    let grant = AdminSessionGrant {
        claims: json!({"sub": "grant-bob"}),
        mapping_hash: "abc".into(),
        refresh_token: Some("sealed".into()),
        refreshed_at: seen,
    };
    store
        .put_admin_session_grant(&s.id_hash, &s.principal, &grant)
        .await
        .expect("put");
    // Read back, as the session manager does, before claiming.
    let seen = store
        .get_admin_session_with_grant(&s.id_hash)
        .await
        .expect("get")
        .and_then(|(_, g)| g)
        .expect("grant")
        .refreshed_at;
    let later = now_us();
    assert!(
        store
            .claim_admin_session_refresh(&s.id_hash, seen, later)
            .await
            .expect("claim")
    );
    assert!(
        !store
            .claim_admin_session_refresh(&s.id_hash, seen, later)
            .await
            .expect("claim again")
    );
    let after = store
        .get_admin_session_with_grant(&s.id_hash)
        .await
        .expect("get")
        .and_then(|(_, g)| g)
        .expect("grant");
    assert_eq!(after.refreshed_at, later);
    assert_eq!(after.refresh_token.as_deref(), Some("sealed"));
}

pub(crate) async fn idle_admin_sessions_are_swept(store: &Arc<dyn ControlStore>) {
    let now = now_us();
    let idle = admin_session_of(
        &"e".repeat(64),
        "alice",
        now - Duration::hours(3),
        now - Duration::hours(2),
    );
    let edge = admin_session_of(
        &"f".repeat(64),
        "alice",
        now - Duration::hours(3),
        now - Duration::hours(1),
    );
    let active = admin_session_of(&"0".repeat(64), "bob", now - Duration::hours(3), now);
    for s in [&idle, &edge, &active] {
        store.create_admin_session(s).await.expect("create");
    }
    // `last_seen < before`: a session seen exactly at the cutoff stays.
    // (At least: on Postgres other checks' sessions share the store.)
    let swept = store
        .delete_idle_admin_sessions(now - Duration::hours(1))
        .await
        .expect("sweep");
    assert!(swept >= 1, "{swept}");
    assert!(
        store
            .get_admin_session(&idle.id_hash)
            .await
            .expect("get")
            .is_none()
    );
    assert!(
        store
            .get_admin_session(&edge.id_hash)
            .await
            .expect("get")
            .is_some()
    );
    assert!(
        store
            .get_admin_session(&active.id_hash)
            .await
            .expect("get")
            .is_some()
    );
}

fn pending_login(state_hash: &str, created_at: DateTime<Utc>, ttl: Duration) -> OidcPendingLogin {
    OidcPendingLogin {
        state_hash: state_hash.into(),
        data: json!({"pkce_verifier": "v", "nonce": "n", "return_to": "/mocks"}),
        created_at,
        expires_at: created_at + ttl,
    }
}

pub(crate) async fn oidc_pending_login_is_taken_once(store: &Arc<dyn ControlStore>) {
    let now = now_us();
    let p = pending_login(&"a".repeat(64), now, Duration::minutes(10));
    store.put_oidc_pending_login(&p, 100).await.expect("put");
    let err = store
        .put_oidc_pending_login(&p, 100)
        .await
        .expect_err("duplicate state");
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
    assert_eq!(
        store
            .get_oidc_pending_login(&p.state_hash, now)
            .await
            .expect("get"),
        Some(p.clone())
    );
    assert_eq!(
        store
            .get_oidc_pending_login(&"b".repeat(64), now)
            .await
            .expect("get unknown"),
        None
    );
    // `get` does not consume it; `take` does, exactly once.
    assert_eq!(
        store
            .take_oidc_pending_login(&p.state_hash, now)
            .await
            .expect("take"),
        Some(p.clone())
    );
    assert_eq!(
        store
            .take_oidc_pending_login(&p.state_hash, now)
            .await
            .expect("take again"),
        None
    );
    assert_eq!(
        store
            .get_oidc_pending_login(&p.state_hash, now)
            .await
            .expect("get after take"),
        None
    );
}

pub(crate) async fn expired_oidc_pending_logins_are_not_returned(store: &Arc<dyn ControlStore>) {
    let now = now_us();
    let expired = pending_login(
        &"c".repeat(64),
        now - Duration::minutes(11),
        Duration::minutes(10),
    );
    store
        .put_oidc_pending_login(&expired, 100)
        .await
        .expect("put expired");
    assert_eq!(
        store
            .get_oidc_pending_login(&expired.state_hash, now)
            .await
            .expect("get"),
        None
    );
    assert_eq!(
        store
            .take_oidc_pending_login(&expired.state_hash, now)
            .await
            .expect("take"),
        None
    );
    // Taking an expired login still removes it.
    assert_eq!(
        store
            .get_oidc_pending_login(&expired.state_hash, expired.created_at)
            .await
            .expect("get at creation time"),
        None
    );

    let old = pending_login(
        &"d".repeat(64),
        now - Duration::minutes(11),
        Duration::minutes(10),
    );
    store
        .put_oidc_pending_login(&old, 100)
        .await
        .expect("put old");
    assert_eq!(
        store
            .delete_expired_oidc_pending_logins(now)
            .await
            .expect("sweep"),
        1
    );
    assert_eq!(
        store
            .delete_expired_oidc_pending_logins(now)
            .await
            .expect("sweep again"),
        0
    );
}

pub(crate) async fn oidc_pending_logins_are_bounded(store: &Arc<dyn ControlStore>) {
    let now = now_us();
    let ttl = Duration::minutes(10);
    let logins: Vec<OidcPendingLogin> = (0..4)
        .map(|i| pending_login(&format!("{i:064}"), now + Duration::seconds(i), ttl))
        .collect();
    for p in &logins {
        store.put_oidc_pending_login(p, 3).await.expect("put");
    }
    // The oldest was evicted to make room for the fourth.
    assert_eq!(
        store
            .get_oidc_pending_login(&logins[0].state_hash, now)
            .await
            .expect("get oldest"),
        None
    );
    for p in &logins[1..] {
        assert!(
            store
                .get_oidc_pending_login(&p.state_hash, now)
                .await
                .expect("get")
                .is_some(),
            "{} was evicted",
            p.state_hash
        );
    }
    // Expired logins are dropped before anything live is evicted.
    let expired = pending_login(&"e".repeat(64), now - Duration::minutes(20), ttl);
    store
        .delete_expired_oidc_pending_logins(now)
        .await
        .expect("sweep");
    let fresh = pending_login(&"f".repeat(64), now + Duration::seconds(10), ttl);
    store
        .put_oidc_pending_login(&fresh, 5)
        .await
        .expect("put fresh");
    store
        .put_oidc_pending_login(&expired, 5)
        .await
        .expect("put expired");
    let newest = pending_login(&"9".repeat(64), now + Duration::seconds(11), ttl);
    store
        .put_oidc_pending_login(&newest, 5)
        .await
        .expect("put newest");
    for p in logins[1..].iter().chain([&fresh, &newest]) {
        assert!(
            store
                .get_oidc_pending_login(&p.state_hash, now)
                .await
                .expect("get")
                .is_some(),
            "{} was evicted",
            p.state_hash
        );
    }
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

pub(crate) async fn audit_append_and_filter(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn audit_prune_by_age(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn force_delete_workspace_removes_api_keys_keeps_audit(
    store: &Arc<dyn ControlStore>,
) {
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

pub(crate) async fn draft_version_round_trips_meta(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn add_draft_requires_draft_status(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn update_draft_replaces_source_and_meta(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn update_draft_rejects_non_draft(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn approve_draft_once(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn draft_writes_publish_nothing(store: &Arc<dyn ControlStore>) {
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
        mode: None,
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

pub(crate) async fn import_job_lifecycle(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn claim_is_fifo_and_exclusive(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn progress_and_finish_need_running(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn cancel_queued_and_running(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn stale_running_jobs_fail(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn prune_finished_jobs(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn jobs_listed_newest_first_and_cascade_with_mock(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn import_job_for_missing_mock_not_found(store: &Arc<dyn ControlStore>) {
    let err = store
        .create_import_job(new_job(Uuid::new_v4(), b"x"))
        .await
        .expect_err("missing mock");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
}

pub(crate) async fn complete_import_job_stores_draft_only_while_running(
    store: &Arc<dyn ControlStore>,
) {
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

pub(crate) async fn active_import_jobs_counted_per_mock(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn channel_settings_crud(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn event_settings_upsert(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn scheduler_lease_exclusive(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn delete_mock_cascades_event_tables(store: &Arc<dyn ControlStore>) {
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

pub(crate) async fn request_log_kind_filter(store: &Arc<dyn ControlStore>) {
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

fn chaos(error_rate: f64) -> ChaosSettings {
    ChaosSettings {
        error_rate,
        latency_jitter: Some(LatencyJitter {
            min_ms: 10,
            max_ms: 250,
        }),
        drop_stream_rate: 0.25,
    }
}

pub(crate) async fn mock_chaos_and_limits_round_trip(store: &Arc<dyn ControlStore>) {
    let mut nm = new_mock("chaos-limits");
    nm.chaos = Some(chaos(0.5));
    nm.limits = MockLimits {
        rate_limit: Some(RateLimit {
            requests_per_second: 2.5,
            burst: 5,
        }),
        daily_token_budget: Some(10_000),
        max_request_bytes: Some(4096),
        max_response_bytes: None,
    };
    let created = store
        .create_mock(DEFAULT_WORKSPACE, nm.clone())
        .await
        .expect("create");
    assert_eq!(created.chaos, nm.chaos);
    assert_eq!(created.limits, nm.limits);
    let got = store
        .get_mock(DEFAULT_WORKSPACE, "chaos-limits")
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(got, created);

    let mut changed = got.clone();
    changed.chaos = None;
    changed.limits = MockLimits {
        rate_limit: None,
        daily_token_budget: None,
        max_request_bytes: None,
        max_response_bytes: Some(2048),
    };
    let updated = store.update_mock(&changed).await.expect("update");
    assert_eq!(updated.chaos, None);
    assert_eq!(updated.limits, changed.limits);
    let got = store
        .get_mock(DEFAULT_WORKSPACE, "chaos-limits")
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(got, updated);
    assert!(
        store
            .list_all_mocks()
            .await
            .expect("list")
            .contains(&updated)
    );

    // A mock without either reads back with no chaos and empty limits.
    let plain = create(store, "chaos-plain").await;
    let got = store
        .get_mock(DEFAULT_WORKSPACE, "chaos-plain")
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(got, plain);
    assert_eq!(got.chaos, None);
    assert_eq!(got.limits, MockLimits::default());
}

pub(crate) async fn invalid_chaos_rejected(store: &Arc<dyn ControlStore>) {
    let mut nm = new_mock("bad-chaos");
    nm.chaos = Some(chaos(1.5));
    let err = store
        .create_mock(DEFAULT_WORKSPACE, nm)
        .await
        .expect_err("invalid chaos");
    assert!(
        matches!(&err, StoreError::Invalid(m) if m.contains("chaos.error_rate")),
        "{err:?}"
    );
    assert_eq!(
        store
            .get_mock(DEFAULT_WORKSPACE, "bad-chaos")
            .await
            .expect("get"),
        None
    );

    let mut nm = new_mock("bad-limits");
    nm.limits.rate_limit = Some(RateLimit {
        requests_per_second: 1.0,
        burst: 0,
    });
    assert!(matches!(
        store.create_mock(DEFAULT_WORKSPACE, nm).await,
        Err(StoreError::Invalid(_))
    ));

    let mut m = create(store, "bad-chaos-update").await;
    m.chaos = Some(chaos(-0.1));
    assert!(matches!(
        store.update_mock(&m).await,
        Err(StoreError::Invalid(_))
    ));
    m.chaos = None;
    m.limits.max_request_bytes = Some(10);
    assert!(matches!(
        store.update_mock(&m).await,
        Err(StoreError::Invalid(_))
    ));
}

pub(crate) async fn scenario_chaos_round_trip(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "scenario-chaos").await;
    let mut s = Scenario {
        mock_id: mock.id,
        name: "flaky".into(),
        instructions: None,
        rules: ScenarioRules {
            chaos: Some(chaos(0.75)),
            ..ScenarioRules::default()
        },
        source: ResourceSource::Api,
    };
    store.put_scenario(&s).await.expect("put");
    assert_eq!(
        store.list_scenarios(mock.id).await.expect("list"),
        vec![s.clone()]
    );
    s.rules.chaos = None;
    store.put_scenario(&s).await.expect("clear");
    assert_eq!(
        store.list_scenarios(mock.id).await.expect("list"),
        vec![s.clone()]
    );
    s.rules.chaos = Some(chaos(2.0));
    assert!(matches!(
        store.put_scenario(&s).await,
        Err(StoreError::Invalid(_))
    ));
}

pub(crate) async fn token_usage_pruned_before_day(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "tokens-pruned").await;
    // Long past, so rows other checks wrote are not affected.
    let old = chrono::NaiveDate::from_ymd_opt(2001, 1, 30).expect("date");
    let older = chrono::NaiveDate::from_ymd_opt(2000, 12, 31).expect("date");
    let kept = chrono::NaiveDate::from_ymd_opt(2001, 2, 1).expect("date");
    for day in [older, old, kept] {
        store.add_token_usage(mock.id, day, 10).await.expect("add");
    }
    let cutoff = chrono::NaiveDate::from_ymd_opt(2001, 2, 1).expect("date");
    assert_eq!(store.prune_token_usage(cutoff).await.expect("prune"), 2);
    assert_eq!(store.token_usage(mock.id, older).await.expect("usage"), 0);
    assert_eq!(store.token_usage(mock.id, old).await.expect("usage"), 0);
    assert_eq!(store.token_usage(mock.id, kept).await.expect("usage"), 10);
    assert_eq!(store.prune_token_usage(cutoff).await.expect("prune"), 0);
}

pub(crate) async fn token_usage_accumulates_per_day(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "tokens").await;
    let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 18).expect("date");
    let next = day.succ_opt().expect("next day");
    assert_eq!(store.token_usage(mock.id, day).await.expect("usage"), 0);
    assert_eq!(
        store.add_token_usage(mock.id, day, 100).await.expect("add"),
        100
    );
    assert_eq!(
        store.add_token_usage(mock.id, day, 50).await.expect("add"),
        150
    );
    assert_eq!(store.token_usage(mock.id, day).await.expect("usage"), 150);
    assert_eq!(store.token_usage(mock.id, next).await.expect("usage"), 0);
    assert_eq!(
        store.add_token_usage(mock.id, next, 7).await.expect("add"),
        7
    );
    assert_eq!(store.token_usage(mock.id, day).await.expect("usage"), 150);

    // Another mock's usage is separate.
    let other = create(store, "tokens-other").await;
    assert_eq!(store.token_usage(other.id, day).await.expect("usage"), 0);
}

pub(crate) async fn token_usage_cascades_with_mock(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "tokens-cascade").await;
    let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 18).expect("date");
    store.add_token_usage(mock.id, day, 42).await.expect("add");
    assert!(
        store
            .delete_mock(DEFAULT_WORKSPACE, "tokens-cascade")
            .await
            .expect("delete")
    );
    assert_eq!(store.token_usage(mock.id, day).await.expect("usage"), 0);
}

pub(crate) async fn get_request_log_by_id(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "log-by-id").await;
    let mut entry = log_entry(mock.id, "s1", "getOrder", "llm");
    entry.at = micros_now();
    entry.kind = RequestKind::Grpc;
    entry.source = Some("fallback".into());
    entry.fallback_reason = Some("LLM request timed out".into());
    let id = store
        .append_request_log(entry.clone(), 1000)
        .await
        .expect("append");
    let got = store
        .get_request_log(id)
        .await
        .expect("get")
        .expect("entry exists");
    assert_eq!(got.id, id);
    let listed = store
        .list_request_log(mock.id, &RequestLogFilter::default())
        .await
        .expect("list");
    assert_eq!(listed, vec![got.clone()]);
    assert_eq!(got.mock_id, entry.mock_id);
    assert_eq!(got.operation_id, entry.operation_id);
    assert_eq!(got.request, entry.request);
    assert_eq!(got.response, entry.response);
    assert_eq!(got.at, entry.at);
    assert_eq!(got.kind, RequestKind::Grpc);
    assert_eq!(
        got.fallback_reason.as_deref(),
        Some("LLM request timed out"),
        "fallback reason round-trip"
    );
    assert_eq!(
        store.get_request_log(id + 1_000_000).await.expect("get"),
        None
    );
}

pub(crate) async fn contract_version_warnings_round_trip(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "warned").await;
    let v = store
        .add_contract_version(mock.id, new_version(ContractStatus::Approved, "a"))
        .await
        .expect("add");
    assert_eq!(v.warnings, None);
    let w =
        json!([{"code": "schema_too_large", "operation": "getBig", "message": "m", "size": 70000}]);
    let set = store
        .set_contract_version_warnings(mock.id, v.version, w.clone())
        .await
        .expect("set");
    assert_eq!(set.warnings, Some(w.clone()));
    let read = store
        .get_contract_version(mock.id, v.version)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(read.warnings, Some(w.clone()));
    assert_eq!(
        store.list_contract_versions(mock.id).await.expect("list")[0].warnings,
        Some(w)
    );
    assert!(matches!(
        store
            .set_contract_version_warnings(mock.id, 99, json!([]))
            .await,
        Err(StoreError::NotFound(_))
    ));
}

pub(crate) async fn import_job_mode_round_trips(store: &Arc<dyn ControlStore>) {
    drain_queue(store).await;
    let mock = create(store, "moded").await;
    let mut j = new_job(mock.id, b"x");
    j.mode = Some("replace".into());
    let created = store.create_import_job(j).await.expect("create");
    assert_eq!(created.mode.as_deref(), Some("replace"));
    assert_eq!(
        store
            .get_import_job(created.id)
            .await
            .expect("get")
            .expect("exists")
            .mode
            .as_deref(),
        Some("replace")
    );
    let claimed = store
        .claim_import_job(now_us())
        .await
        .expect("claim")
        .expect("queued");
    assert_eq!(claimed.id, created.id);
    assert_eq!(claimed.mode.as_deref(), Some("replace"));
}

/// A bundle with `ops` HTTP operations and `channels` Kafka channels.
fn sized_bundle(title: &str, ops: usize, channels: usize) -> Bundle {
    let mut b = bundle(title);
    b.operations = (0..ops)
        .map(|i| {
            serde_json::from_value(json!({
                "id": format!("op{i}"),
                "protocol": "http",
                "binding": { "http": { "method": "get", "path": format!("/r{i}") } },
                "kind": "query",
                "summary": null,
                "input": { "path": null, "query": null, "headers": null, "body": null },
                "outputs": [],
                "resource_type": null,
                "identity": null,
                "semantic_hints": [],
            }))
            .expect("operation")
        })
        .collect();
    b.channels = (0..channels)
        .map(|i| {
            serde_json::from_value(json!({
                "id": format!("ch{i}"),
                "title": format!("Channel {i}"),
                "address": format!("topic{i}"),
                "binding": { "kafka": { "topic": format!("topic{i}") } },
                "direction": "publish",
                "message_schema": { "type": "object" },
                "message_names": ["m"],
                "content_type": "application/json",
            }))
            .expect("channel")
        })
        .collect();
    b
}

pub(crate) async fn active_versions_in_one_call(store: &Arc<dyn ControlStore>) {
    let active = create(store, "active").await;
    let inactive = create(store, "inactive").await;
    let empty = create(store, "empty").await;
    for src in ["a", "b"] {
        store
            .add_contract_version(active.id, new_version(ContractStatus::Approved, src))
            .await
            .expect("add");
    }
    store
        .activate_contract_version(active.id, 2)
        .await
        .expect("activate");
    store
        .add_contract_version(inactive.id, new_version(ContractStatus::Approved, "c"))
        .await
        .expect("add");

    assert!(store.active_versions(&[]).await.expect("none").is_empty());
    // More ids than one SQLite statement binds, unknown ones included.
    let mut ids: Vec<Uuid> = (0..1200).map(|_| Uuid::new_v4()).collect();
    ids.extend([empty.id, inactive.id, active.id]);
    let got = store.active_versions(&ids).await.expect("active versions");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got.get(&active.id), Some(&2));
    let expected = store
        .active_contract(active.id)
        .await
        .expect("active")
        .map(|v| v.version);
    assert_eq!(got.get(&active.id).copied(), expected);
}

pub(crate) async fn active_bundle_leaves_out_the_source(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "bundled").await;
    assert_eq!(store.active_bundle(mock.id).await.expect("none"), None);
    let mut v = new_version(ContractStatus::Active, "src");
    v.bundle = sized_bundle("src", 3, 2);
    store.add_contract_version(mock.id, v).await.expect("add");
    let full = store
        .active_contract(mock.id)
        .await
        .expect("active")
        .expect("exists");
    let got = store
        .active_bundle(mock.id)
        .await
        .expect("bundle")
        .expect("exists");
    assert_eq!(got.version, full.version);
    assert_eq!(got.bundle, full.bundle);
}

pub(crate) async fn active_bundles_in_one_call(store: &Arc<dyn ControlStore>) {
    let active = create(store, "bundles-active").await;
    let inactive = create(store, "bundles-inactive").await;
    let empty = create(store, "bundles-empty").await;
    let mut first = new_version(ContractStatus::Approved, "first");
    first.bundle = sized_bundle("first", 1, 0);
    let mut second = new_version(ContractStatus::Approved, "second");
    second.bundle = sized_bundle("second", 3, 2);
    for v in [first, second] {
        store.add_contract_version(active.id, v).await.expect("add");
    }
    store
        .activate_contract_version(active.id, 2)
        .await
        .expect("activate");
    store
        .add_contract_version(inactive.id, new_version(ContractStatus::Approved, "c"))
        .await
        .expect("add");

    assert!(store.active_bundles(&[]).await.expect("none").is_empty());
    // More ids than one SQLite statement binds, unknown ones included.
    let mut ids: Vec<Uuid> = (0..1200).map(|_| Uuid::new_v4()).collect();
    ids.extend([empty.id, inactive.id, active.id]);
    let got = store.active_bundles(&ids).await.expect("active bundles");
    assert_eq!(got.len(), 1, "{:?}", got.keys().collect::<Vec<_>>());
    let expected = store
        .active_bundle(active.id)
        .await
        .expect("active")
        .expect("exists");
    assert_eq!(got.get(&active.id), Some(&expected));
    assert_eq!(expected.version, 2);
}

/// A prune keeps exactly the `retention` newest rows of its mock (none
/// with a retention of 0), and leaves other mocks' rows alone.
pub(crate) async fn request_log_prune_keeps_exactly_the_newest(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "prune-exact").await;
    let other = create(store, "prune-other").await;
    let interval = usize::try_from(RETENTION_PRUNE_INTERVAL).expect("interval");
    let mut other_ids = Vec::new();
    for _ in 0..30 {
        other_ids.push(
            store
                .append_request_log(log_entry(other.id, "s", "op", "faker"), 1000)
                .await
                .expect("append other"),
        );
    }
    // The first append prunes (below retention: nothing goes), the next
    // prune is the (interval + 1)-th append.
    let mut ids = Vec::new();
    for _ in 0..=interval {
        ids.push(
            store
                .append_request_log(log_entry(mock.id, "s", "op", "faker"), 10)
                .await
                .expect("append"),
        );
    }
    let all = RequestLogFilter {
        limit: 1000,
        ..RequestLogFilter::default()
    };
    let kept: Vec<i64> = store
        .list_request_log(mock.id, &all)
        .await
        .expect("list")
        .iter()
        .map(|e| e.id)
        .collect();
    let newest: Vec<i64> = ids.iter().rev().take(10).copied().collect();
    assert_eq!(kept, newest);
    let mut others: Vec<i64> = store
        .list_request_log(other.id, &all)
        .await
        .expect("list other")
        .iter()
        .map(|e| e.id)
        .collect();
    others.reverse();
    assert_eq!(others, other_ids, "another mock's rows are untouched");

    let zero = create(store, "prune-zero").await;
    store
        .append_request_log(log_entry(zero.id, "s", "op", "faker"), 0)
        .await
        .expect("append zero");
    assert!(
        store
            .list_request_log(zero.id, &all)
            .await
            .expect("list zero")
            .is_empty(),
        "a retention of 0 keeps nothing"
    );
}

pub(crate) async fn contract_version_infos_match_the_versions(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "infos").await;
    assert!(
        store
            .list_contract_version_infos(mock.id)
            .await
            .expect("empty")
            .is_empty()
    );
    let mut plain = new_version(ContractStatus::Approved, "plain");
    plain.bundle = sized_bundle("plain", 4, 0);
    store
        .add_contract_version(mock.id, plain)
        .await
        .expect("add");
    let mut events = new_version(ContractStatus::Active, "events");
    events.bundle = sized_bundle("events", 1, 3);
    events.created_by = None;
    store
        .add_contract_version(mock.id, events)
        .await
        .expect("add");
    store
        .set_contract_version_warnings(mock.id, 2, json!([{ "code": "w", "message": "m" }]))
        .await
        .expect("warnings");
    let mut draft = new_version(ContractStatus::Draft, "draft");
    draft.bundle = sized_bundle("draft", 2, 1);
    store
        .add_draft_contract_version(mock.id, draft, draft_meta("note"))
        .await
        .expect("draft");

    let infos = store
        .list_contract_version_infos(mock.id)
        .await
        .expect("infos");
    let expected: Vec<mb_store::ContractVersionInfo> = store
        .list_contract_versions(mock.id)
        .await
        .expect("versions")
        .iter()
        .map(Into::into)
        .collect();
    assert_eq!(infos, expected);
    assert_eq!(
        infos
            .iter()
            .map(|i| (i.version, i.operations, i.channels, i.imported))
            .collect::<Vec<_>>(),
        vec![(3, 2, 1, true), (2, 1, 3, false), (1, 4, 0, false)]
    );
    assert!(infos[1].warnings.is_some() && infos[0].warnings.is_none());
}

pub(crate) async fn latest_contract_version_by_status(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "latest").await;
    let wanted = [ContractStatus::Draft, ContractStatus::Active];
    assert_eq!(
        store
            .latest_contract_version(mock.id, &wanted)
            .await
            .expect("none"),
        None
    );
    store
        .add_contract_version(mock.id, new_version(ContractStatus::Active, "one"))
        .await
        .expect("v1");
    store
        .add_draft_contract_version(
            mock.id,
            new_version(ContractStatus::Draft, "two"),
            draft_meta("n"),
        )
        .await
        .expect("v2");
    store
        .add_contract_version(mock.id, new_version(ContractStatus::Approved, "three"))
        .await
        .expect("v3");
    for (statuses, want) in [
        (wanted.to_vec(), Some(2)),
        (vec![ContractStatus::Active], Some(1)),
        (vec![ContractStatus::Approved], Some(3)),
        (vec![ContractStatus::Archived], None),
        (vec![], None),
    ] {
        let got = store
            .latest_contract_version(mock.id, &statuses)
            .await
            .expect("latest");
        assert_eq!(got.as_ref().map(|v| v.version), want, "{statuses:?}");
        if let Some(v) = got {
            assert_eq!(
                Some(v),
                store
                    .get_contract_version(mock.id, want.unwrap_or_default())
                    .await
                    .expect("get")
            );
        }
    }
}

pub(crate) async fn request_log_summaries_match_the_entries(store: &Arc<dyn ControlStore>) {
    let mock = create(store, "summaries").await;
    let other = create(store, "other").await;
    let bodies = [
        (json!({"q": 1}), Some(json!({"ok": true}))),
        (
            json!({"name": "Zo\u{eb} \u{2713} \u{1F426}", "esc": "a\"b\\c\n\u{1}"}),
            None,
        ),
        (
            json!({"n": [1.5, -0.0, 1e300, 12_345_678_901_234_567_u64], "z": null}),
            Some(json!(null)),
        ),
        (
            json!({"b": {"a": [], "c": {}}, "a": "x".repeat(10_000)}),
            Some(json!("y")),
        ),
    ];
    for (i, (request, response)) in bodies.into_iter().enumerate() {
        let mut e = log_entry(mock.id, &format!("s{}", i % 2), "op", "llm");
        e.request = request;
        e.response = response;
        if i == 0 {
            e.kind = RequestKind::Event;
        }
        store.append_request_log(e, 1000).await.expect("append");
    }
    store
        .append_request_log(log_entry(other.id, "s0", "op", "llm"), 1000)
        .await
        .expect("append");

    for f in [
        RequestLogFilter::default(),
        RequestLogFilter {
            session: Some("s1".into()),
            ..RequestLogFilter::default()
        },
        RequestLogFilter {
            kind: Some(RequestKind::Event),
            ..RequestLogFilter::default()
        },
        RequestLogFilter {
            limit: 2,
            ..RequestLogFilter::default()
        },
    ] {
        let summaries = store
            .list_request_log_summaries(mock.id, &f)
            .await
            .expect("summaries");
        let expected: Vec<mb_store::RequestLogSummary> = store
            .list_request_log(mock.id, &f)
            .await
            .expect("entries")
            .iter()
            .map(Into::into)
            .collect();
        assert!(!summaries.is_empty(), "{f:?}");
        assert_eq!(summaries, expected, "{f:?}");
    }
    let all = store
        .list_request_log_summaries(mock.id, &RequestLogFilter::default())
        .await
        .expect("all");
    // Newest first: the 10 KB request, then null, none and `{"ok":true}`.
    let sizes: Vec<(u64, u64)> = all
        .iter()
        .map(|s| (s.request_bytes, s.response_bytes))
        .collect();
    assert_eq!(sizes.len(), 4);
    assert!(sizes[0].0 > 10_000, "{sizes:?}");
    assert_eq!(
        (sizes[0].1, sizes[1].1, sizes[2].1, sizes[3]),
        (3, 4, 0, (7, 11)),
        "{sizes:?}"
    );
}
