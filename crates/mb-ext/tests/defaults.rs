use std::io::Write;
use std::time::Duration;

use http::HeaderMap;
use mb_ext::{
    AllowAll, CallInfo, ConfiguredProfile, EnvFileSecrets, ExtError, PrometheusUsage,
    ProviderPolicy, QuotaGuard, SecretStore, SingleTenant, Tenancy, TenantResolver, UsageEvent,
    UsageSink,
};
use metrics_util::MetricKind;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};

fn call(headers: &HeaderMap) -> CallInfo<'_> {
    CallInfo {
        mock: "petstore",
        operation_id: "listPets",
        headers,
    }
}

fn default_tenancy() -> Tenancy {
    Tenancy {
        tenant: "default".to_string(),
        workspace: "default".to_string(),
    }
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
    let event = UsageEvent {
        tenancy: default_tenancy(),
        mock: "petstore".to_string(),
        operation_id: "listPets".to_string(),
        status: 200,
        source: "llm".to_string(),
        latency: Duration::from_millis(42),
        prompt_tokens: 10,
        completion_tokens: 5,
    };
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
