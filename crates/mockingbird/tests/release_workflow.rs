#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The release pipeline (`.github/workflows/release.yml`) and its version
//! gate (`scripts/check-version.sh`). The workflow runs only in CI, so this
//! parses it and checks the jobs, targets, scanning, signing, permissions and
//! action pins it must keep.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn workflow() -> Value {
    let path = repo().join(".github/workflows/release.yml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_saphyr::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn jobs(doc: &Value) -> &serde_json::Map<String, Value> {
    doc["jobs"].as_object().expect("jobs is a mapping")
}

/// Every step of every job, with the id of its job.
fn steps(doc: &Value) -> Vec<(String, Value)> {
    jobs(doc)
        .iter()
        .flat_map(|(id, job)| {
            job["steps"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(move |step| (id.clone(), step))
        })
        .collect()
}

fn uses(step: &Value) -> &str {
    step["uses"].as_str().unwrap_or("")
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[test]
fn release_jobs_present() {
    let doc = workflow();
    let ids: BTreeSet<&str> = jobs(&doc).keys().map(String::as_str).collect();
    for job in [
        "verify",
        "console",
        "binaries",
        "sbom",
        "images",
        "manifest",
        "chart",
        "publish-crate",
        "release",
    ] {
        assert!(ids.contains(job), "missing job {job}; have {ids:?}");
    }
}

#[test]
fn binary_targets_exact() {
    let doc = workflow();
    let binaries = &jobs(&doc)["binaries"];
    assert_eq!(binaries["strategy"]["fail-fast"], Value::Bool(false));
    let targets: BTreeSet<String> = binaries["strategy"]["matrix"]["include"]
        .as_array()
        .expect("binaries matrix include list")
        .iter()
        .map(|entry| scalar(&entry["target"]))
        .collect();
    let expected: BTreeSet<String> = [
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(targets, expected);
}

/// GitHub retired the macos-13 hosted image (2025-12-04); a matrix leg on it
/// never gets a runner, and `release` (which needs `binaries`) never runs.
/// The x86_64 macOS binary is cross-built on an Apple Silicon runner.
#[test]
fn binary_runners_live() {
    let doc = workflow();
    let retired = [
        "macos-13",
        "macos-12",
        "macos-11",
        "ubuntu-20.04",
        "windows-2019",
    ];
    for entry in jobs(&doc)["binaries"]["strategy"]["matrix"]["include"]
        .as_array()
        .expect("binaries matrix include list")
    {
        let runner = scalar(&entry["runner"]);
        let target = scalar(&entry["target"]);
        assert!(
            !retired.contains(&runner.as_str()),
            "{target} runs on retired image {runner}"
        );
        if target.ends_with("-apple-darwin") {
            assert!(
                matches!(runner.as_str(), "macos-14" | "macos-15"),
                "{target} must build on an Apple Silicon runner (macos-14/15), got {runner}"
            );
        }
    }
}

#[test]
fn images_scan_and_sign() {
    let doc = workflow();
    let all = steps(&doc);

    let trivy = all
        .iter()
        .filter(|(job, step)| {
            job == "images" && uses(step).starts_with("aquasecurity/trivy-action@")
        })
        .collect::<Vec<_>>();
    assert!(!trivy.is_empty(), "the images job must scan with trivy");
    for (_, step) in trivy {
        assert_eq!(scalar(&step["with"]["severity"]), "CRITICAL,HIGH");
        assert_eq!(scalar(&step["with"]["exit-code"]), "1");
    }

    assert!(
        all.iter()
            .any(|(job, step)| job == "manifest" && scalar(&step["run"]).contains("cosign sign")),
        "the manifest job must cosign sign the images"
    );
    for action in ["actions/attest-build-provenance@", "actions/attest-sbom@"] {
        assert!(
            all.iter().any(|(_, step)| uses(step).starts_with(action)),
            "{action} is not used"
        );
    }
}

#[test]
fn oidc_permission_scoped() {
    let doc = workflow();
    let top = doc["permissions"]
        .as_object()
        .expect("top-level permissions");
    assert_eq!(top.get("contents").map(scalar).as_deref(), Some("read"));
    assert!(
        !top.contains_key("id-token"),
        "id-token must not be granted to the whole workflow"
    );
    assert!(
        jobs(&doc)
            .values()
            .any(|job| scalar(&job["permissions"]["id-token"]) == "write"),
        "no job requests id-token: write"
    );
}

/// A ref is pinned when it is a version tag (`v3`, `v4.1.0`, `0.33.1`) or a
/// full commit SHA; branches like `main` or `master` are not.
fn pinned(reference: &str) -> bool {
    let version = reference.strip_prefix('v').unwrap_or(reference);
    let is_version = !version.is_empty()
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    let is_sha = reference.len() == 40 && reference.chars().all(|c| c.is_ascii_hexdigit());
    is_version || is_sha
}

#[test]
fn every_action_pinned() {
    let doc = workflow();
    let mut refs: Vec<String> = steps(&doc)
        .iter()
        .map(|(_, step)| uses(step).to_string())
        .filter(|u| !u.is_empty())
        .collect();
    refs.extend(
        jobs(&doc)
            .values()
            .filter_map(|job| job["uses"].as_str().map(String::from)),
    );
    assert!(!refs.is_empty());
    for action in refs {
        if action.starts_with("./") {
            continue;
        }
        let (_, reference) = action
            .split_once('@')
            .unwrap_or_else(|| panic!("{action} has no @ref"));
        assert!(pinned(reference), "{action} is not pinned to a tag");
    }
}

#[cfg(unix)]
#[test]
fn check_version_script() {
    let run = |tag: &str| {
        std::process::Command::new("bash")
            .arg(repo().join("scripts/check-version.sh"))
            .arg(tag)
            .current_dir(repo())
            .output()
            .unwrap()
    };
    let ok = run("v0.1.0-rc.1");
    assert_eq!(
        ok.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let wrong = run("v9.9.9");
    assert_eq!(
        wrong.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&wrong.stderr)
    );
}

/// `.github/workflows/ci.yml`, parsed.
fn ci_workflow() -> Value {
    let path = repo().join(".github/workflows/ci.yml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_saphyr::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn ci_gates_present() {
    let doc = ci_workflow();
    let ids: BTreeSet<&str> = jobs(&doc).keys().map(String::as_str).collect();
    for job in [
        "check",
        "protocol-features",
        "console",
        "deny",
        "image",
        "e2e",
        "helm",
        "semver",
        "msrv",
    ] {
        assert!(ids.contains(job), "ci.yml has no {job} job: {ids:?}");
    }
    let all = steps(&doc);
    let runs = |job: &str, needle: &str| {
        all.iter()
            .any(|(id, step)| id == job && scalar(&step["run"]).contains(needle))
    };

    let check = &jobs(&doc)["check"];
    assert!(
        scalar(&check["services"]["redis"]["image"]).starts_with("redis:7"),
        "the check job needs a Redis service"
    );
    assert_eq!(
        scalar(&check["env"]["MB_TEST_REDIS_URL"]),
        "redis://localhost:6379"
    );
    assert!(runs(
        "check",
        "cargo doc --workspace --no-deps --all-features"
    ));
    assert!(runs(
        "check",
        "cargo test -p mockingbird-custom-ext-example"
    ));

    assert!(runs("helm", "scripts/check-helm.sh"));
    assert!(runs("semver", "scripts/semver-check.sh"));
    assert!(
        all.iter().any(|(id, step)| id == "semver"
            && uses(step).starts_with("actions/checkout@")
            && scalar(&step["with"]["fetch-depth"]) == "0"),
        "the semver job needs the full history (fetch-depth: 0)"
    );
    assert!(runs(
        "msrv",
        "cargo +1.97 check --workspace --all-features --locked"
    ));

    let trivy: Vec<_> = all
        .iter()
        .filter(|(id, step)| id == "image" && uses(step).starts_with("aquasecurity/trivy-action@"))
        .collect();
    assert_eq!(trivy.len(), 1, "the image job scans the image it built");
    let variants: BTreeSet<String> = jobs(&doc)["image"]["strategy"]["matrix"]["include"]
        .as_array()
        .map(|rows| rows.iter().map(|row| scalar(&row["variant"])).collect())
        .unwrap_or_default();
    assert_eq!(
        variants,
        BTreeSet::from(["full".to_owned(), "slim".to_owned()]),
        "the image job builds and scans both images"
    );
    for (_, step) in trivy {
        assert_eq!(scalar(&step["with"]["severity"]), "CRITICAL,HIGH");
        assert_eq!(scalar(&step["with"]["exit-code"]), "1");
        assert_eq!(scalar(&step["with"]["ignore-unfixed"]), "true");
    }

    assert!(runs("e2e", "e2e-compose/flow.spec.ts"));
    assert!(runs("e2e", "--profile ha up -d --wait"));
    assert!(runs("e2e", "stop mockingbird-control"));
}

#[test]
fn ci_actions_pinned() {
    let doc = ci_workflow();
    for (job, step) in steps(&doc) {
        let action = uses(&step);
        if action.is_empty() {
            continue;
        }
        let (_, reference) = action
            .split_once('@')
            .unwrap_or_else(|| panic!("{job}: {action} has no @ref"));
        // dtolnay/rust-toolchain is pinned by its `toolchain` input instead.
        if action.starts_with("dtolnay/rust-toolchain@") {
            continue;
        }
        assert!(pinned(reference), "{job}: {action} is not pinned to a tag");
    }
}
