use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use assert_cmd::Command;
use mb_evals::bench::bench;
use mb_evals::corpus::{self, CorpusEntry};
use mb_evals::manifest;
use mb_evals::report::{
    ContractStats, EvalReport, RunOptions, Sample, TargetRef, Thresholds, run_contracts, run_evals,
};
use mb_llm::ScriptedProvider;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// Three unary operations whose success body is any object.
const PERMISSIVE: &str = r#"
openapi: 3.1.0
info: {title: Permissive, version: "1"}
paths:
  /a:
    get:
      operationId: getA
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {type: object, required: [id], properties: {id: {type: string}}}
  /b:
    get:
      operationId: getB
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {type: object, required: [id], properties: {id: {type: string}}}
  /c:
    post:
      operationId: postC
      requestBody:
        required: true
        content:
          application/json:
            schema: {type: object, properties: {name: {type: string}}}
      responses:
        "201":
          description: Created
          content:
            application/json:
              schema: {type: object, required: [id], properties: {id: {type: string}}}
"#;

fn sample_report() -> EvalReport {
    let mut per_contract = BTreeMap::new();
    per_contract.insert(
        "openapi/orders-3.1.yaml".to_string(),
        ContractStats {
            calls: 10,
            llm: 8,
            repaired: 1,
            fallback: 1,
            p95_ms: 1200,
        },
    );
    EvalReport {
        model: "test-model".into(),
        base_url: "https://llm.example".into(),
        started_at: chrono::Utc::now(),
        calls: 10,
        schema_valid_rate: 0.9,
        first_pass_rate: 0.8,
        repair_rate: 0.1,
        fallback_rate: 0.1,
        p50_ms: 400,
        p95_ms: 1200,
        prompt_tokens_per_call: 512.0,
        completion_tokens_per_call: 128.0,
        per_contract,
        per_api_type: BTreeMap::new(),
        per_domain: BTreeMap::new(),
    }
}

#[test]
fn corpus_loads_fixture_contracts() {
    let entries = corpus::load(&fixtures()).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        entries.len() >= 10,
        "only {} entries: {names:?}",
        entries.len()
    );
    assert!(names.contains(&"openapi/orders-3.1.yaml"), "{names:?}");
    assert!(
        names.contains(&"asyncapi/streetlights-kafka-3.0.yaml"),
        "{names:?}"
    );
    // A directory of several protos compiles as one contract.
    assert!(names.contains(&"proto/multi"), "{names:?}");
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "entries are sorted by name");
    let streetlights = entries
        .iter()
        .find(|e| e.name == "asyncapi/streetlights-kafka-3.0.yaml")
        .unwrap();
    assert!(!streetlights.bundle.channels.is_empty());
}

#[test]
fn corpus_skips_files_that_do_not_compile() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "not a contract").unwrap();
    std::fs::write(dir.path().join("api.yaml"), PERMISSIVE).unwrap();
    let entries = corpus::load(dir.path()).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["api.yaml"]);
}

#[tokio::test]
async fn run_evals_with_scripted_provider_counts_sources() {
    let entry = CorpusEntry {
        name: "permissive.yaml".into(),
        bundle: mb_import::compile(PERMISSIVE.as_bytes()).unwrap(),
    };
    // Call 1: valid (llm). Call 2: invalid, then repaired. Call 3: the
    // script is exhausted, so the engine falls back to the faker.
    let provider = Arc::new(ScriptedProvider::with_usage(vec![
        Ok((
            r#"{"id":"a1"}"#.into(),
            mb_llm::Usage {
                prompt_tokens: 100,
                completion_tokens: 10,
            },
        )),
        Ok((
            r#"{"nope":true}"#.into(),
            mb_llm::Usage {
                prompt_tokens: 100,
                completion_tokens: 10,
            },
        )),
        Ok((
            r#"{"id":"b1"}"#.into(),
            mb_llm::Usage {
                prompt_tokens: 100,
                completion_tokens: 10,
            },
        )),
    ]));
    let report = run_evals(&[entry], provider, "scripted", 10).await;

    assert_eq!(report.model, "scripted");
    assert_eq!(report.calls, 3);
    let stats = &report.per_contract["permissive.yaml"];
    assert_eq!(
        (stats.calls, stats.llm, stats.repaired, stats.fallback),
        (3, 1, 1, 1)
    );
    let third = 1.0 / 3.0;
    assert!((report.first_pass_rate - third).abs() < 1e-9);
    assert!((report.repair_rate - third).abs() < 1e-9);
    assert!((report.fallback_rate - third).abs() < 1e-9);
    assert!((report.schema_valid_rate - 2.0 * third).abs() < 1e-9);
    let sum = report.first_pass_rate + report.repair_rate + report.fallback_rate;
    assert!((sum - 1.0).abs() < 1e-9, "rates sum to {sum}");
    assert!((report.prompt_tokens_per_call - 100.0).abs() < 1e-9);
    assert!((report.completion_tokens_per_call - 10.0).abs() < 1e-9);
    assert!(report.p50_ms <= report.p95_ms);
}

#[tokio::test]
async fn run_evals_caps_operations_and_covers_publish_channels() {
    let bytes = std::fs::read(fixtures().join("asyncapi/streetlights-kafka-3.0.yaml")).unwrap();
    let entry = CorpusEntry {
        name: "streetlights".into(),
        bundle: mb_import::compile(&bytes).unwrap(),
    };
    let publish = entry
        .bundle
        .channels
        .iter()
        .filter(|c| c.direction == mb_mcm::Direction::Publish)
        .count();
    assert!(publish > 0);
    // An empty script: every call falls back.
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let report = run_evals(std::slice::from_ref(&entry), provider.clone(), "m", 10).await;
    assert_eq!(report.calls, publish.min(10) as u64);
    assert_eq!(report.fallback_rate, 1.0);

    let capped = run_evals(&[entry], provider, "m", 1).await;
    assert_eq!(capped.calls, 1);
}

#[test]
fn thresholds_report_violations() {
    let t = Thresholds::default();
    assert_eq!(t.min_schema_valid_rate, 0.95);
    assert_eq!(t.max_repair_rate, 0.25);
    assert_eq!(t.max_fallback_rate, 0.05);
    assert_eq!(t.max_p95_ms, 3000);

    let mut good = sample_report();
    good.schema_valid_rate = 0.99;
    good.repair_rate = 0.1;
    good.fallback_rate = 0.01;
    good.p95_ms = 2000;
    assert!(good.violations(&t).is_empty(), "{:?}", good.violations(&t));

    let mut bad = sample_report();
    bad.schema_valid_rate = 0.5;
    bad.repair_rate = 0.3;
    bad.fallback_rate = 0.5;
    bad.p95_ms = 4000;
    let v = bad.violations(&t);
    assert_eq!(v.len(), 4, "{v:?}");
    assert!(v.iter().any(|s| s.contains("schema_valid_rate")));
    assert!(v.iter().any(|s| s.contains("repair_rate")));
    assert!(v.iter().any(|s| s.contains("fallback_rate")));
    assert!(v.iter().any(|s| s.contains("p95_ms")));

    let mut empty = sample_report();
    empty.calls = 0;
    assert!(
        empty.violations(&t).iter().any(|s| s.contains("no calls")),
        "a run that made no calls is a violation"
    );
}

#[test]
fn markdown_has_summary_table() {
    let report = sample_report();
    let md = report.to_markdown();
    assert!(md.contains("| Metric | Value |"), "{md}");
    assert!(md.contains("| --- | --- |"), "{md}");
    assert!(md.contains("| Schema-valid rate | 90.0% |"), "{md}");
    assert!(md.contains("| p95 latency | 1200 ms |"), "{md}");
    assert!(md.contains("test-model"), "{md}");
    assert!(
        md.contains("| openapi/orders-3.1.yaml | 10 | 8 | 1 | 1 | 1200 |"),
        "{md}"
    );

    let json = serde_json::to_string(&report).unwrap();
    let back: EvalReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back, report);
}

#[test]
fn run_without_env_is_usage_error() {
    let assert = Command::cargo_bin("mb-evals")
        .unwrap()
        .args(["run", "--corpus"])
        .arg(fixtures())
        .env_remove("MB_EVAL_LLM_BASE_URL")
        .env_remove("MB_EVAL_LLM_MODEL")
        .env_remove("MB_EVAL_LLM_API_KEY")
        .assert()
        .code(2);
    // clap also exits 2 on any argument error, so the code alone does not
    // prove the missing-environment path ran: check its message too.
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains(
            "missing environment: MB_EVAL_LLM_BASE_URL, MB_EVAL_LLM_MODEL, MB_EVAL_LLM_API_KEY"
        ),
        "{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_offline_smoke() {
    let report = bench(
        &fixtures().join("openapi/orders-3.1.yaml"),
        // A smoke test needs only a handful of requests; the load loop runs
        // until this wall-clock window ends (real sockets, so no paused clock).
        Duration::from_millis(200),
        4,
    )
    .await
    .unwrap();
    assert!(report.requests > 0, "{report:?}");
    assert_eq!(report.errors, 0, "{report:?}");
    assert_eq!(report.concurrency, 4);
    assert!(report.rps > 0.0);
    // The load ran for the whole window, and rps is requests over that time.
    assert!(report.duration_s >= 0.2, "{report:?}");
    let rps = report.requests as f64 / report.duration_s;
    assert!((report.rps - rps).abs() < 1e-6, "{report:?}");
    assert!(report.p50_ms <= report.p95_ms && report.p95_ms <= report.p99_ms);
}

#[test]
fn evals_workflow_is_weekly_and_read_only() {
    let wf = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/evals.yml"),
    )
    .unwrap();
    assert!(wf.contains("cron: \"17 3 * * 1\""), "{wf}");
    assert!(wf.contains("workflow_dispatch"));
    assert!(!wf.contains("pull_request"));
    assert!(wf.contains("contents: read"));
    assert!(wf.contains("cargo run --release -p mb-evals -- run --out eval-report"));
    assert!(wf.contains("MB_EVAL_LLM_API_KEY: ${{ secrets.MB_EVAL_LLM_API_KEY }}"));
    assert!(wf.contains("if: always()"));
    assert!(wf.contains("GITHUB_STEP_SUMMARY"));
}

/// Two unary operations, one answering through a component `$ref`.
const WITH_REFS: &str = r##"
openapi: 3.1.0
info: {title: Refs, version: "1"}
paths:
  /pets/{petId}:
    get:
      operationId: getPet
      parameters:
        - {name: petId, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Pet"}
  /ping:
    get:
      operationId: ping
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {type: object, required: [ok], properties: {ok: {type: boolean}}}
components:
  schemas:
    Pet:
      type: object
      required: [id, owner]
      properties:
        id: {type: string}
        owner: {$ref: "#/components/schemas/Owner"}
    Owner:
      type: object
      required: [name]
      properties:
        name: {type: string}
"##;

fn write_contracts(dir: &Path) -> std::io::Result<()> {
    std::fs::write(dir.join("permissive.yaml"), PERMISSIVE)?;
    std::fs::write(dir.join("refs.yaml"), WITH_REFS)
}

#[test]
fn manifest_parse_validates_entries() {
    let ok = manifest::parse(
        r#"{"entries":[
            {"name":"rest/a","path":"a.yaml","api_type":"rest","domain":"generic"},
            {"name":"rest/b","path":"b.yaml","api_type":"rest","domain":"travel",
             "operations":["x"],"max_ops":2}
        ]}"#,
    )
    .unwrap();
    assert_eq!(ok.entries.len(), 2);
    assert_eq!(ok.entries[0].operations, None);
    assert_eq!(ok.entries[0].max_ops, None);
    assert_eq!(
        ok.entries[1].operations.as_deref(),
        Some(&["x".to_string()][..])
    );
    assert_eq!(ok.entries[1].max_ops, Some(2));

    let dup = manifest::parse(
        r#"{"entries":[
            {"name":"a","path":"a.yaml","api_type":"rest","domain":"generic"},
            {"name":"a","path":"b.yaml","api_type":"rest","domain":"generic"}
        ]}"#,
    );
    assert!(format!("{:#}", dup.unwrap_err()).contains("twice"));
    let missing = manifest::parse(r#"{"entries":[{"name":"a","path":"a.yaml","domain":"x"}]}"#);
    assert!(missing.is_err());
    let empty_type =
        manifest::parse(r#"{"entries":[{"name":"a","path":"a","api_type":"","domain":"x"}]}"#);
    assert!(empty_type.is_err());
}

#[test]
fn manifest_selects_named_operations_or_the_capped_default() {
    let dir = tempfile::tempdir().unwrap();
    write_contracts(dir.path()).unwrap();
    let path = dir.path().join("workloads.json");
    std::fs::write(
        &path,
        r#"{"entries":[
            {"name":"named","path":"permissive.yaml","api_type":"rest","domain":"generic",
             "operations":["postC","getA"]},
            {"name":"capped","path":"permissive.yaml","api_type":"rest","domain":"generic",
             "max_ops":2},
            {"name":"default","path":"refs.yaml","api_type":"rest","domain":"travel"}
        ]}"#,
    )
    .unwrap();
    let contracts = manifest::load(&path, dir.path(), 10).unwrap();
    let names: Vec<&str> = contracts.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["named", "capped", "default"]);

    let ids = |i: usize| -> Vec<String> {
        let c = &contracts[i];
        c.targets
            .iter()
            .map(|t| match *t {
                TargetRef::Operation(n) => c.bundle.operations[n].id.clone(),
                TargetRef::Channel(n) => c.bundle.channels[n].id.clone(),
            })
            .collect()
    };
    assert_eq!(
        ids(0),
        ["postC", "getA"],
        "exactly the named targets, in order"
    );
    assert_eq!(ids(1).len(), 2, "max_ops caps the default rule");
    assert_eq!(ids(2).len(), 2);
    assert_eq!(contracts[2].domain.as_deref(), Some("travel"));
    assert_eq!(contracts[2].api_type.as_deref(), Some("rest"));

    // The run default applies when the entry has no max_ops.
    let capped = manifest::load(&path, dir.path(), 1).unwrap();
    assert_eq!(capped[2].targets.len(), 1);
}

#[test]
fn manifest_rejects_unknown_ids_and_missing_contracts() {
    let dir = tempfile::tempdir().unwrap();
    write_contracts(dir.path()).unwrap();
    let unknown = dir.path().join("unknown.json");
    std::fs::write(
        &unknown,
        r#"{"entries":[{"name":"x","path":"permissive.yaml","api_type":"rest",
            "domain":"generic","operations":["getA","nope"]}]}"#,
    )
    .unwrap();
    let err = format!(
        "{:#}",
        manifest::load(&unknown, dir.path(), 10).unwrap_err()
    );
    assert!(err.contains("nope"), "{err}");

    let missing = dir.path().join("missing.json");
    std::fs::write(
        &missing,
        r#"{"entries":[{"name":"x","path":"absent.yaml","api_type":"rest","domain":"generic"}]}"#,
    )
    .unwrap();
    assert!(manifest::load(&missing, dir.path(), 10).is_err());
}

#[test]
fn manifest_selects_publish_channels_by_id() {
    let fixtures = fixtures();
    let bundle = mb_import::compile(
        &std::fs::read(fixtures.join("asyncapi/streetlights-kafka-3.0.yaml")).unwrap(),
    )
    .unwrap();
    let publish = bundle
        .channels
        .iter()
        .find(|c| c.direction == mb_mcm::Direction::Publish)
        .unwrap()
        .id
        .clone();
    let entry = manifest::ManifestEntry {
        name: "streetlights".into(),
        path: "asyncapi/streetlights-kafka-3.0.yaml".into(),
        api_type: "asyncapi".into(),
        domain: "generic".into(),
        operations: Some(vec![publish]),
        max_ops: None,
    };
    let contract = manifest::compile_entry(entry, &fixtures, 10).unwrap();
    assert_eq!(contract.targets.len(), 1);
    assert!(matches!(contract.targets[0], TargetRef::Channel(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_contracts_repeats_samples_and_breaks_down() {
    let dir = tempfile::tempdir().unwrap();
    write_contracts(dir.path()).unwrap();
    let path = dir.path().join("workloads.json");
    std::fs::write(
        &path,
        r#"{"entries":[
            {"name":"pets","path":"refs.yaml","api_type":"rest","domain":"travel",
             "operations":["getPet"]},
            {"name":"perm","path":"permissive.yaml","api_type":"graphql","domain":"generic",
             "operations":["getA"]}
        ]}"#,
    )
    .unwrap();
    let contracts = manifest::load(&path, dir.path(), 10).unwrap();
    // Four calls: every answer below is valid for both schemas, so the
    // order the concurrent calls take them in does not matter.
    let answer = r#"{"id":"p1","owner":{"name":"Ada"}}"#;
    let provider = Arc::new(ScriptedProvider::with_usage(
        (0..4)
            .map(|_| {
                Ok((
                    answer.to_string(),
                    mb_llm::Usage {
                        prompt_tokens: 50,
                        completion_tokens: 5,
                    },
                ))
            })
            .collect(),
    ));
    let opts = RunOptions {
        repeat: 2,
        concurrency: 3,
        ..RunOptions::default()
    };
    let mut samples: Vec<Sample> = Vec::new();
    let report = run_contracts(
        &contracts,
        provider,
        "scripted",
        &opts,
        &mut |s: &Sample| {
            samples.push(s.clone());
            Ok(())
        },
    )
    .await
    .unwrap();

    assert_eq!(report.calls, 4);
    assert_eq!(report.first_pass_rate, 1.0);
    assert_eq!(report.per_contract["pets"].calls, 2);
    assert_eq!(report.per_contract["perm"].calls, 2);
    assert_eq!(report.per_api_type["rest"].calls, 2);
    assert_eq!(report.per_api_type["graphql"].calls, 2);
    assert_eq!(report.per_domain["travel"].llm, 2);
    assert_eq!(report.per_domain["generic"].llm, 2);
    assert!((report.prompt_tokens_per_call - 50.0).abs() < 1e-9);

    assert_eq!(samples.len(), 4);
    let mut reps: Vec<(String, usize)> = samples
        .iter()
        .map(|s| (s.contract.clone(), s.rep))
        .collect();
    reps.sort();
    assert_eq!(
        reps,
        [
            ("perm".to_string(), 0),
            ("perm".to_string(), 1),
            ("pets".to_string(), 0),
            ("pets".to_string(), 1)
        ]
    );
    let pet = samples.iter().find(|s| s.contract == "pets").unwrap();
    assert_eq!(pet.target, "getPet");
    assert_eq!(pet.target_kind, "operation");
    assert_eq!(pet.source, "llm");
    assert_eq!(pet.api_type.as_deref(), Some("rest"));
    assert_eq!(pet.domain.as_deref(), Some("travel"));
    assert_eq!(pet.body["owner"]["name"], "Ada");
    assert_eq!(pet.prompt_tokens, 50);
    assert!(pet.error.is_none());
    assert!(pet.request["path"]["petId"].is_string(), "{}", pet.request);
    // The validated schema is self-contained: every reference it makes,
    // transitively, is attached under `$defs`.
    let schema = serde_json::to_string(&pet.schema).unwrap();
    let defs = pet.schema["$defs"].as_object().unwrap();
    for name in ["Pet", "Owner"] {
        let key = defs
            .keys()
            .find(|k| k.ends_with(name))
            .unwrap_or_else(|| panic!("no {name} in {schema}"));
        assert!(schema.contains(&format!("#/$defs/{key}")), "{schema}");
    }

    let md = report.to_markdown();
    assert!(md.contains("## Per API type"), "{md}");
    assert!(md.contains("| graphql | 2 | 2 | 0 | 0 |"), "{md}");
    assert!(md.contains("## Per domain"), "{md}");
    let json = serde_json::to_string(&report).unwrap();
    assert!(json.contains("per_api_type"));
    let back: EvalReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back, report);
}

#[test]
fn corpus_report_omits_breakdowns() {
    let json = serde_json::to_string(&sample_report()).unwrap();
    assert!(!json.contains("per_api_type"), "{json}");
    assert!(!json.contains("per_domain"), "{json}");
    assert!(!sample_report().to_markdown().contains("Per API type"));
}

#[test]
fn run_rejects_manifest_with_corpus() {
    Command::cargo_bin("mb-evals")
        .unwrap()
        .args(["run", "--corpus", "fixtures", "--manifest", "m.json"])
        .assert()
        .code(2);
}
