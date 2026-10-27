use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use http::HeaderMap;
use mb_ext::{
    AllowAll, CallInfo, ConfiguredProfile, EnvFileSecrets, ExtError, Extensions, PrometheusUsage,
    ProviderPolicy, QuotaGuard, Rejection, SecretStore, SingleTenant, Tenancy, TenantResolver,
    UsageEvent, UsageSink,
};
use metrics_util::MetricKind;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};

fn call(headers: &HeaderMap) -> CallInfo<'_> {
    CallInfo::new("petstore", "listPets", headers)
}

fn default_tenancy() -> Tenancy {
    Tenancy::new("default", "default")
}

#[tokio::test]
async fn single_tenant_resolves_default() {
    let headers = HeaderMap::new();
    let tenancy = SingleTenant.resolve(&call(&headers)).await.ok();
    let tenancy = tenancy.map(|t| (t.tenant, t.workspace));
    assert_eq!(
        tenancy,
        Some(("default".to_string(), "default".to_string()))
    );
}

#[tokio::test]
async fn allow_all_admits() {
    let headers = HeaderMap::new();
    let admitted = AllowAll.admit(&default_tenancy(), &call(&headers)).await;
    assert!(admitted.is_ok());
}

#[tokio::test]
async fn env_secret_resolves() {
    // Reads a variable every test process has, so no test mutates the
    // environment (set_var races with concurrent readers).
    let name = "PATH";
    let expected = std::env::var(name).expect("PATH is set");
    let value = EnvFileSecrets.resolve(&format!("env:{name}")).await;
    assert_eq!(value.ok(), Some(expected));
}

#[tokio::test]
async fn file_secret_trims_newline() {
    let mut file = tempfile::NamedTempFile::new().expect("tempfile");
    file.write_all(b"s3cret\n").expect("write secret");
    let reference = format!("file:{}", file.path().display());
    let value = EnvFileSecrets.resolve(&reference).await;
    assert_eq!(value.ok().as_deref(), Some("s3cret"));
}

#[tokio::test]
async fn missing_env_secret_errors() {
    let name = "MB_EXT_TEST_SECRET_DEFINITELY_UNSET_9C1E";
    let err = EnvFileSecrets.resolve(&format!("env:{name}")).await;
    assert!(
        matches!(&err, Err(ExtError::SecretNotFound(n)) if n == name),
        "{err:?}"
    );
}

#[tokio::test]
async fn bad_secret_ref_errors() {
    for reference in ["vault:x", "plain"] {
        let err = EnvFileSecrets.resolve(reference).await;
        assert!(
            matches!(&err, Err(ExtError::BadSecretRef(r)) if r == reference),
            "{reference}: {err:?}"
        );
    }
}

#[test]
fn configured_profile_passthrough() {
    let t = default_tenancy();
    let chosen = ConfiguredProfile.choose(&t, "petstore", Some("fast")).ok();
    assert_eq!(chosen, Some(Some("fast".to_string())));
    let none = ConfiguredProfile.choose(&t, "petstore", None).ok();
    assert_eq!(none, Some(None));
}

#[test]
fn prometheus_usage_records() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let mut event = UsageEvent::new(default_tenancy(), "petstore", "listPets");
    event.status = 200;
    event.source = "llm".to_string();
    event.latency = Duration::from_millis(42);
    event.prompt_tokens = 10;
    event.completion_tokens = 5;
    metrics::with_local_recorder(&recorder, || PrometheusUsage.record(&event));

    let entries: Vec<_> = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .map(|(key, _, _, value)| {
            let (kind, key) = key.into_parts();
            let mut labels: Vec<(String, String)> = key
                .labels()
                .map(|l| (l.key().to_string(), l.value().to_string()))
                .collect();
            labels.sort();
            (kind, key.name().to_string(), labels, value)
        })
        .collect();

    let find = |name: &str, labels: &[(&str, &str)]| {
        let mut want: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        want.sort();
        entries
            .iter()
            .find(|(kind, n, l, _)| *kind == MetricKind::Counter && n == name && *l == want)
            .map(|(_, _, _, v)| match v {
                DebugValue::Counter(c) => Some(*c),
                _ => None,
            })
    };

    assert_eq!(
        find(
            "mb_requests_total",
            &[
                ("mock", "petstore"),
                ("operation", "listPets"),
                ("status", "200"),
                ("source", "llm"),
            ],
        ),
        Some(Some(1))
    );
    assert_eq!(
        find(
            "mb_llm_tokens_total",
            &[("mock", "petstore"), ("kind", "prompt")]
        ),
        Some(Some(10))
    );
    assert_eq!(
        find(
            "mb_llm_tokens_total",
            &[("mock", "petstore"), ("kind", "completion")]
        ),
        Some(Some(5))
    );
    assert!(entries.iter().any(
        |(kind, n, _, _)| *kind == MetricKind::Histogram && n == "mb_request_duration_seconds"
    ));
}

/// A quota guard that refuses everything, to tell it apart from `AllowAll`.
struct DenyAll;

#[async_trait::async_trait]
impl QuotaGuard for DenyAll {
    async fn admit(&self, _t: &Tenancy, _call: &CallInfo<'_>) -> Result<(), Rejection> {
        Err(Rejection::new(429, "Too Many Requests", "denied"))
    }
}

#[tokio::test]
async fn builders_replace_one_extension() {
    let quota: Arc<dyn QuotaGuard> = Arc::new(DenyAll);
    let ext = Extensions::default().with_quota(quota.clone());
    assert!(Arc::ptr_eq(&ext.quota, &quota));

    let headers = HeaderMap::new();
    let info = call(&headers);
    let denied = ext.quota.admit(&default_tenancy(), &info).await;
    assert_eq!(denied.err().map(|r| r.status), Some(429));
    // Every other extension keeps its default.
    let tenancy = ext.tenants.resolve(&info).await.ok();
    assert_eq!(tenancy, Some(default_tenancy()));
    let chosen = ext
        .providers
        .choose(&default_tenancy(), "petstore", Some("p"));
    assert_eq!(chosen.ok(), Some(Some("p".to_string())));
    assert!(ext.admin_hooks.routes().is_none());

    let tenants: Arc<dyn TenantResolver> = Arc::new(SingleTenant);
    let usage: Arc<dyn UsageSink> = Arc::new(PrometheusUsage);
    let secrets: Arc<dyn SecretStore> = Arc::new(EnvFileSecrets);
    let providers: Arc<dyn ProviderPolicy> = Arc::new(ConfiguredProfile);
    let hooks: Arc<dyn mb_ext::AdminHooks> = Arc::new(mb_ext::NoHooks);
    let ext = Extensions::default()
        .with_tenants(tenants.clone())
        .with_usage(usage.clone())
        .with_secrets(secrets.clone())
        .with_providers(providers.clone())
        .with_admin_hooks(hooks.clone());
    assert!(Arc::ptr_eq(&ext.tenants, &tenants));
    assert!(Arc::ptr_eq(&ext.usage, &usage));
    assert!(Arc::ptr_eq(&ext.secrets, &secrets));
    assert!(Arc::ptr_eq(&ext.providers, &providers));
    assert!(Arc::ptr_eq(&ext.admin_hooks, &hooks));
}

#[test]
fn rejection_new_and_retry_after() {
    let r = Rejection::new(403, "Forbidden", "no access");
    assert_eq!(
        r,
        Rejection {
            status: 403,
            title: "Forbidden".to_string(),
            detail: "no access".to_string(),
            retry_after: None,
        }
    );
    let r = r.with_retry_after(Duration::from_secs(7));
    assert_eq!(r.retry_after, Some(Duration::from_secs(7)));
    assert_eq!(r.status, 403);
}

#[test]
fn usage_event_new_defaults() {
    let e = UsageEvent::new(Tenancy::new("t", "w"), "petstore", "listPets");
    assert_eq!(e.tenancy, Tenancy::new("t", "w"));
    assert_eq!(e.tenancy.tenant, "t");
    assert_eq!(e.tenancy.workspace, "w");
    assert_eq!(e.mock, "petstore");
    assert_eq!(e.operation_id, "listPets");
    assert_eq!(e.status, 0);
    assert_eq!(e.source, "");
    assert_eq!(e.latency, Duration::ZERO);
    assert_eq!(e.prompt_tokens, 0);
    assert_eq!(e.completion_tokens, 0);
}

#[cfg(unix)]
#[tokio::test]
async fn key_store_files_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().expect("tempdir");
    let store = home.path().join(".mockingbird/secrets");
    std::fs::create_dir_all(&store).expect("mkdir");
    let key = store.join("openrouter");
    std::fs::write(&key, "sk-test\n").expect("write");
    assert!(mb_ext::in_key_store(&key));
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let r = EnvFileSecrets
        .resolve(&format!("file:{}", key.display()))
        .await;
    let err = r.expect_err("0644 must be refused").to_string();
    assert!(err.contains("chmod 600") && !err.contains("sk-test"), "{err}");
    assert!(mb_ext::check_secret_file(&key).is_err());
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    assert_eq!(
        EnvFileSecrets
            .resolve(&format!("file:{}", key.display()))
            .await
            .expect("0600"),
        "sk-test"
    );
    let shared = home.path().join("mounted");
    std::fs::write(&shared, "v").expect("write");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    assert!(!mb_ext::in_key_store(&shared));
    assert_eq!(
        EnvFileSecrets
            .resolve(&format!("file:{}", shared.display()))
            .await
            .expect("outside the store"),
        "v"
    );
}
