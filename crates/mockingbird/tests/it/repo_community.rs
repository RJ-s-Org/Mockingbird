//! Community and repository files: they exist, the changelog script works,
//! dependabot covers every ecosystem and the example gallery loads.
#![cfg(test)]

use std::path::Path;
use std::process::Command;

use crate::support::{read, repo};
use serde_json::Value;

#[test]
fn community_files_exist() {
    let files = [
        "SECURITY.md",
        "CONTRIBUTING.md",
        "CODE_OF_CONDUCT.md",
        "CHANGELOG.md",
        ".github/ISSUE_TEMPLATE/bug_report.yml",
        ".github/ISSUE_TEMPLATE/feature_request.yml",
        ".github/ISSUE_TEMPLATE/config.yml",
        ".github/pull_request_template.md",
        ".github/dependabot.yml",
        "examples/gallery/README.md",
        "examples/gallery/chaos/README.md",
        "scripts/changelog-section.sh",
    ];
    for file in files {
        let text = read(&repo().join(file));
        assert!(!text.trim().is_empty(), "{file} is empty");
    }
    let security = read(&repo().join("SECURITY.md"));
    assert!(security.contains("Report a vulnerability"));
}

#[test]
fn issue_templates_are_valid_yaml() {
    for file in [
        "bug_report.yml",
        "feature_request.yml",
        "contract.yml",
        "docs.yml",
        "config.yml",
    ] {
        let path = repo().join(".github/ISSUE_TEMPLATE").join(file);
        let doc: Value = serde_saphyr::from_str(&read(&path))
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(doc.is_object(), "{file} is not a mapping");
    }
}

fn changelog_section(version: &str) -> std::process::Output {
    Command::new("bash")
        .arg(repo().join("scripts/changelog-section.sh"))
        .arg(version)
        .current_dir(repo())
        .output()
        .unwrap()
}

#[test]
fn changelog_has_0_1_0_section() {
    let out = changelog_section("0.1.0-rc.1");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8(out.stdout).unwrap();
    assert!(
        !body.contains("## ["),
        "the section must stop at the next one"
    );
    for phase in ["P1", "P2", "P3", "P4", "P5", "P6", "P7"] {
        assert!(
            body.contains(phase),
            "0.1.0-rc.1 section does not mention {phase}"
        );
    }
}

#[test]
fn changelog_missing_version_exits_1() {
    let out = changelog_section("9.9.9");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn dependabot_covers_three_ecosystems_monthly() {
    let doc: Value = serde_saphyr::from_str(&read(&repo().join(".github/dependabot.yml"))).unwrap();
    assert_eq!(doc["version"], 2);
    let updates = doc["updates"].as_array().unwrap();
    let find = |eco: &str| {
        updates
            .iter()
            .find(|u| u["package-ecosystem"] == eco)
            .unwrap_or_else(|| panic!("no {eco} entry"))
    };
    // One grouped PR per ecosystem a month keeps CI runs down.
    for eco in ["cargo", "npm", "github-actions"] {
        assert_eq!(find(eco)["schedule"]["interval"], "monthly", "{eco}");
        let groups = find(eco)["groups"].as_object().unwrap();
        assert!(
            groups
                .values()
                .any(|g| g["patterns"].as_array().unwrap().iter().any(|p| *p == "*")),
            "{eco} updates are not grouped into one PR"
        );
    }
    assert_eq!(find("cargo")["directory"], "/");
    assert_eq!(find("npm")["directory"], "/crates/mb-console/web");
    assert_eq!(find("github-actions")["directory"], "/");
}

#[test]
fn gallery_configs_load() {
    let gallery = repo().join("examples/gallery");
    let readme = read(&gallery.join("README.md"));
    let mut loaded = Vec::new();
    for entry in std::fs::read_dir(&gallery).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            readme.contains(&format!(
                "mockingbird serve -c examples/gallery/{name}/mockingbird.yaml"
            )),
            "README has no serve line for {name}"
        );
        let path = dir.join("mockingbird.yaml");
        let cfg = mockingbird::file_config::load(&path)
            .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert!(!cfg.mocks.is_empty(), "{name} declares no mocks");
        mockingbird::file_config::broker_configs(&cfg)
            .unwrap_or_else(|e| panic!("{name}: brokers: {e:#}"));
        for (mock, m) in &cfg.mocks {
            assert!(
                m.contract.is_file(),
                "{name}/{mock}: contract {} does not exist",
                m.contract.display()
            );
        }
        assert_scenario_statuses_declared(&path);
        loaded.push(name);
    }
    loaded.sort();
    assert_eq!(
        loaded,
        ["chaos", "events", "graphql", "grpc", "rest", "soap"]
    );
}

/// Every scenario status in the config at `path` is one its operations
/// declare (or a 5xx), and every `set` fits a response it can select, as
/// `serve` and the Admin API require.
fn assert_scenario_statuses_declared(path: &Path) {
    let cfg = mockingbird::file_config::load(path)
        .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
    for (mock, m) in &cfg.mocks {
        let source = mb_import::read_source(&m.contract).unwrap();
        let bundle = mb_import::compile(&source)
            .unwrap_or_else(|e| panic!("{}: {mock}: {e:#}", path.display()));
        for (sname, sc) in &m.scenarios {
            if let Some(status) = sc.status
                && let Some(why) =
                    mb_admin::undeclared_scenario_status(&bundle, &sc.operation_ids(), status)
            {
                panic!("{}: mock {mock}, scenario {sname}: {why}", path.display());
            }
            if let Some(set) = &sc.set
                && let Some(why) =
                    mb_admin::invalid_scenario_set(&bundle, &sc.operation_ids(), sc.status, set)
            {
                panic!("{}: mock {mock}, scenario {sname}: {why}", path.display());
            }
        }
    }
}

#[test]
fn documented_example_scenarios_take_effect() {
    for example in ["mockingbird.yaml", "mockingbird.full.yaml"] {
        assert_scenario_statuses_declared(&repo().join("docs/examples").join(example));
    }
}

#[test]
fn every_example_config_scenario_takes_effect() {
    for dir in ["examples/apis", "examples/apps", "examples/gallery"] {
        for entry in std::fs::read_dir(repo().join(dir)).unwrap() {
            let config = entry.unwrap().path().join("mockingbird.yaml");
            if config.exists() {
                assert_scenario_statuses_declared(&config);
            }
        }
    }
}
