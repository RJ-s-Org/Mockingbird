//! The deployment artifacts: the Helm chart (`deploy/helm/mockingbird`) and
//! the Compose stack with its `ha`, `kafka`, `amqp` and `mqtt` profiles
//! (`deploy/compose`). Neither Kubernetes nor Docker runs locally, so this
//! parses them and checks the parts Mockingbird itself reads: the chart's
//! version, every config the chart can render and the Compose HA config all
//! load with `file_config::load`. `scripts/check-helm.sh` lints and renders
//! the chart itself.
#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::support::{read, repo};
use mockingbird::file_config::{self, AuthMode, Role};
use serde_json::Value;

fn chart() -> PathBuf {
    repo().join("deploy/helm/mockingbird")
}

fn compose_dir() -> PathBuf {
    repo().join("deploy/compose")
}

fn yaml(path: &Path) -> Value {
    serde_saphyr::from_str(&read(path)).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Helm's `mergeOverwrite`: mappings merge key by key, recursively, and any
/// other value in `over` replaces the one in `base`. A `null` in `over`
/// leaves `base` as it is (`mergeOverwrite` skips empty values).
fn merge_overwrite(base: &mut Value, over: &Value) {
    match (base, over) {
        (_, Value::Null) => {}
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                merge_overwrite(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// Loads `config` (a config document as JSON, which is also YAML) with
/// `file_config::load`.
fn load_config(config: &Value, origin: &str) -> file_config::FileConfig {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mockingbird.yaml");
    std::fs::write(&path, serde_json::to_string_pretty(config).unwrap()).unwrap();
    let cfg = file_config::load(&path).unwrap_or_else(|e| panic!("{origin}: {e:#}"));
    if cfg.auth.mode == AuthMode::Oidc {
        cfg.auth
            .validate()
            .unwrap_or_else(|e| panic!("{origin}: {e}"));
    }
    cfg
}

/// The chart's `files/default-config.yaml` with `values`' `config` merged
/// over it, as the chart's ConfigMap renders it.
fn rendered_config(values: &Value) -> Value {
    let mut config = yaml(&chart().join("files/default-config.yaml"));
    merge_overwrite(&mut config, &values["config"]);
    config
}

/// Every values file the chart is tested or documented with.
fn values_files() -> Vec<PathBuf> {
    let mut files = vec![chart().join("values.yaml")];
    for dir in ["ci", "examples"] {
        let mut found: Vec<PathBuf> = std::fs::read_dir(chart().join(dir))
            .unwrap_or_else(|e| panic!("{dir}: {e}"))
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
            .collect();
        found.sort();
        files.extend(found);
    }
    files
}

#[test]
fn chart_app_version_matches_workspace() {
    let chart = yaml(&chart().join("Chart.yaml"));
    assert_eq!(chart["apiVersion"], "v2");
    assert_eq!(chart["name"], "mockingbird");
    assert_eq!(chart["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(
        chart["appVersion"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(chart["kubeVersion"], ">=1.27.0-0");
    assert!(
        chart.get("dependencies").is_none(),
        "the chart has no subcharts"
    );
}

#[test]
fn chart_default_config_is_valid() {
    let defaults = yaml(&chart().join("files/default-config.yaml"));
    assert_eq!(defaults["server"]["host"], "0.0.0.0");
    assert!(
        defaults.get("auth").is_none() && defaults["server"].get("insecure_no_auth").is_none(),
        "the chart leaves auth to the user"
    );
    let cfg = load_config(&defaults, "files/default-config.yaml");
    assert_eq!(cfg.server.role, Role::All, "the role comes from --role");

    let files = values_files();
    assert!(files.len() >= 4, "values.yaml, two ci files, an example");
    for file in files {
        let values = yaml(&file);
        let config = rendered_config(&values);
        assert!(
            config["server"].get("role").is_none(),
            "{}: server.role is set per Deployment with --role",
            file.display()
        );
        load_config(&config, &file.display().to_string());
    }
}

#[test]
fn chart_merge_is_later_wins() {
    let mut base = serde_json::json!({"server": {"host": "0.0.0.0", "port": 8080}, "a": [1]});
    merge_overwrite(
        &mut base,
        &serde_json::json!({"server": {"port": 9000}, "a": [2], "b": null}),
    );
    assert_eq!(
        base,
        serde_json::json!({"server": {"host": "0.0.0.0", "port": 9000}, "a": [2], "b": null})
    );
}

#[test]
fn values_schema_is_json() {
    let path = chart().join("values.schema.json");
    let schema: Value =
        serde_json::from_str(&read(&path)).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(schema["type"], "object");
    let properties = schema["properties"].as_object().expect("properties");
    let values = yaml(&chart().join("values.yaml"));
    for key in values.as_object().expect("values.yaml is a mapping").keys() {
        assert!(
            properties.contains_key(key),
            "values.schema.json lacks {key}"
        );
    }
    assert_eq!(
        schema["properties"]["mode"]["enum"],
        serde_json::json!(["allInOne", "split"])
    );
    assert_eq!(
        schema["properties"]["image"]["properties"]["flavor"]["enum"],
        serde_json::json!(["full", "slim"])
    );
}

#[test]
fn chart_values_contract() {
    let v = yaml(&chart().join("values.yaml"));
    assert_eq!(v["image"]["repository"], "ghcr.io/rj-s-org/mockingbird");
    assert_eq!(v["image"]["tag"], "");
    assert_eq!(v["image"]["flavor"], "full");
    assert_eq!(v["mode"], "allInOne");
    assert_eq!(v["config"], serde_json::json!({}));
    for store in ["storage", "state", "bus"] {
        for key in ["url", "existingSecret", "existingSecretKey"] {
            assert!(v[store].get(key).is_some(), "{store}.{key}");
        }
    }
    assert_eq!(v["llm"]["apiKeyKey"], "api-key");
    for block in ["allInOne", "control", "data"] {
        for key in [
            "replicas",
            "resources",
            "nodeSelector",
            "tolerations",
            "affinity",
            "podAnnotations",
            "topologySpreadConstraints",
        ] {
            assert!(v[block].get(key).is_some(), "{block}.{key}");
        }
    }
    assert_eq!(
        v["data"]["autoscaling"],
        serde_json::json!({"enabled": true, "minReplicas": 2, "maxReplicas": 10,
            "targetCPUUtilizationPercentage": 70})
    );
    assert_eq!(v["service"]["type"], "ClusterIP");
    assert_eq!(
        v["service"]["ports"],
        serde_json::json!({"http": 8080, "grpc": 50051, "admin": 9090})
    );
    assert_eq!(
        v["grpcIngress"]["annotations"]["nginx.ingress.kubernetes.io/backend-protocol"],
        "GRPC"
    );
    assert!(v["ingress"].is_object() && v["adminIngress"].is_object());
    assert_eq!(
        v["podDisruptionBudget"],
        serde_json::json!({"enabled": true, "minAvailable": 1})
    );
    assert_eq!(v["networkPolicy"]["enabled"], false);
    assert_eq!(v["serviceMonitor"]["enabled"], false);
    assert_eq!(v["serviceMonitor"]["interval"], "30s");
    assert_eq!(v["serviceAccount"]["create"], true);
    let sc = &v["securityContext"];
    assert_eq!(sc["runAsNonRoot"], true);
    assert_eq!(sc["runAsUser"], 65532);
    assert_eq!(sc["readOnlyRootFilesystem"], true);
    assert_eq!(sc["allowPrivilegeEscalation"], false);
    assert_eq!(sc["capabilities"]["drop"], serde_json::json!(["ALL"]));
    assert_eq!(sc["seccompProfile"]["type"], "RuntimeDefault");
}

/// The services of `docker-compose.yml` with their profiles (empty for the
/// default stack).
fn compose_services() -> (Value, BTreeMap<String, BTreeSet<String>>) {
    let doc = yaml(&compose_dir().join("docker-compose.yml"));
    let services = doc["services"]
        .as_object()
        .expect("services")
        .iter()
        .map(|(name, svc)| {
            let profiles = svc["profiles"]
                .as_array()
                .map(|a| a.iter().map(|p| p.as_str().unwrap().to_string()).collect())
                .unwrap_or_default();
            (name.clone(), profiles)
        })
        .collect();
    (doc, services)
}

#[test]
fn compose_profiles() {
    let (doc, services) = compose_services();
    let profile = |name: &str| -> Vec<&str> {
        services
            .get(name)
            .unwrap_or_else(|| panic!("service {name} is missing"))
            .iter()
            .map(String::as_str)
            .collect()
    };
    assert_eq!(profile("redis"), ["ha"]);
    assert_eq!(profile("kafka"), ["kafka"]);
    assert_eq!(profile("rabbitmq"), ["amqp"]);
    assert_eq!(profile("mosquitto"), ["mqtt"]);
    assert_eq!(profile("mockingbird-control"), ["ha"]);
    assert_eq!(profile("mockingbird-data"), ["ha"]);
    assert_eq!(doc["services"]["redis"]["image"], "redis:7-alpine");

    // The default stack (no profile) is exactly the P1-P6 one.
    let defaults: BTreeSet<&str> = services
        .iter()
        .filter(|(_, p)| p.is_empty())
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(
        defaults,
        BTreeSet::from(["postgres", "rauthy", "rauthy-bootstrap", "mockingbird"])
    );
    let mb = &doc["services"]["mockingbird"];
    assert_eq!(
        mb["command"],
        serde_json::json!([
            "serve",
            "-c",
            "/etc/mockingbird/mockingbird.yaml",
            "--host",
            "0.0.0.0",
            "--offline"
        ])
    );
    assert_eq!(
        mb["volumes"][0],
        "./mockingbird.yaml:/etc/mockingbird/mockingbird.yaml:ro"
    );
    assert_eq!(mb["network_mode"], "service:rauthy");

    // The HA nodes share mockingbird-ha.yaml and differ only in --role.
    for (service, role) in [
        ("mockingbird-control", "control"),
        ("mockingbird-data", "data"),
    ] {
        let svc = &doc["services"][service];
        let command: Vec<&str> = svc["command"]
            .as_array()
            .unwrap_or_else(|| panic!("{service}.command"))
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        let at = command
            .iter()
            .position(|a| *a == "--role")
            .unwrap_or_else(|| panic!("{service} passes --role"));
        assert_eq!(command.get(at + 1), Some(&role), "{service}");
        assert_eq!(
            command[..3],
            ["serve", "-c", "/etc/mockingbird/mockingbird.yaml"]
        );
        let volumes: Vec<&str> = svc["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            volumes.contains(&"./mockingbird-ha.yaml:/etc/mockingbird/mockingbird.yaml:ro"),
            "{service} mounts mockingbird-ha.yaml"
        );
    }
}

#[test]
fn compose_ha_config_is_valid() {
    let path = compose_dir().join("mockingbird-ha.yaml");
    let raw = yaml(&path);
    assert!(
        raw["server"].get("role").is_none(),
        "the role comes per service as --role"
    );
    let cfg = file_config::load(&path).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(
        cfg.storage.url.starts_with("postgres://"),
        "Postgres storage"
    );
    assert!(
        cfg.storage.effective_state_url().starts_with("redis://"),
        "Redis state"
    );
    assert!(
        cfg.bus
            .url
            .as_deref()
            .is_some_and(|u| u.starts_with("redis://")),
        "Redis bus"
    );
    // The nodes bind every interface, so the config must allow it.
    cfg.server.check_bind(&cfg.auth).unwrap();
}
