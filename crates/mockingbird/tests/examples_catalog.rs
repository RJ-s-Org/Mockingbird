#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The API examples catalog (`examples/apis`) and `mockingbird examples`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use mockingbird::examples::EXAMPLES;

fn apis_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/apis")
}

#[test]
fn every_example_directory_is_embedded_with_all_its_files() {
    let mut on_disk = BTreeSet::new();
    for entry in std::fs::read_dir(apis_dir()).unwrap() {
        let path = entry.unwrap().path();
        if !path.join("example.json").is_file() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let example = EXAMPLES
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("examples/apis/{name} is not in EXAMPLES (src/examples.rs)"));
        let embedded: BTreeSet<&str> = example.files.iter().map(|(n, _)| *n).collect();
        for file in std::fs::read_dir(&path).unwrap() {
            let file = file.unwrap();
            let file_name = file.file_name().to_string_lossy().into_owned();
            if file_name.starts_with('.') || file_name == "example.json" {
                continue;
            }
            assert!(
                embedded.contains(file_name.as_str()),
                "examples/apis/{name}/{file_name} is not embedded"
            );
        }
        on_disk.insert(name);
    }
    let listed: BTreeSet<String> = EXAMPLES.iter().map(|e| e.name.to_string()).collect();
    assert_eq!(listed, on_disk, "EXAMPLES and examples/apis differ");
}

#[test]
fn every_example_config_loads_and_its_contracts_compile() {
    for example in EXAMPLES {
        let meta = example.meta().unwrap();
        assert_eq!(meta.name, example.name);
        let dir = apis_dir().join(example.name);
        let config = mockingbird::file_config::load(&dir.join("mockingbird.yaml"))
            .unwrap_or_else(|e| panic!("{}: {e:#}", example.name));
        assert!(!config.mocks.is_empty(), "{}: no mocks", example.name);
        for (mock, cfg) in &config.mocks {
            let bytes = mb_import::read_source(&cfg.contract)
                .unwrap_or_else(|e| panic!("{}/{mock}: {e:#}", example.name));
            mb_import::compile_with_warnings(&bytes)
                .unwrap_or_else(|e| panic!("{}/{mock}: {e:#}", example.name));
            for host in &cfg.hosts {
                assert!(
                    meta.hosts.contains(host),
                    "{}: host {host} missing from example.json",
                    example.name
                );
            }
            for scenario in cfg.scenarios.keys() {
                assert!(
                    meta.scenarios.contains(scenario),
                    "{}: scenario {scenario} missing from example.json",
                    example.name
                );
            }
        }
        for contract in &meta.contracts {
            assert!(dir.join(contract).is_file(), "{}: {contract}", example.name);
        }
    }
}

#[test]
fn examples_list_show_and_init() {
    let out = Command::cargo_bin("mockingbird")
        .unwrap()
        .args(["examples", "list"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), EXAMPLES.len());

    let out = Command::cargo_bin("mockingbird")
        .unwrap()
        .args(["examples", "show", "stripe"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let show: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(show["example"]["name"], "stripe");
    assert!(show["config"].as_str().unwrap().contains("api.stripe.com"));

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("stripe");
    let init = || {
        Command::cargo_bin("mockingbird")
            .unwrap()
            .args(["examples", "init", "stripe", "--dir"])
            .arg(&target)
            .output()
            .unwrap()
    };
    let out = init();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for file in ["mockingbird.yaml", "README.md", "stripe.openapi.yaml"] {
        assert!(target.join(file).is_file(), "{file}");
    }
    // The written config loads from its new home.
    mockingbird::file_config::load(&target.join("mockingbird.yaml")).unwrap();
    // A second init refuses to overwrite.
    let out = init();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--force"));

    let out = Command::cargo_bin("mockingbird")
        .unwrap()
        .args(["examples", "show", "no-such-api"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("stripe"));
}
