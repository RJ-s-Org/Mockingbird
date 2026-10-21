#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use assert_cmd::Command;
use mb_evals::bench::bench;
use mb_evals::corpus::{self, CorpusEntry};
use mb_evals::report::{ContractStats, EvalReport, Thresholds, run_evals};
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
    Command::cargo_bin("mb-evals")
        .unwrap()
        .args(["run", "--corpus"])
        .arg(fixtures())
        .env_remove("MB_EVAL_LLM_BASE_URL")
        .env_remove("MB_EVAL_LLM_MODEL")
        .env_remove("MB_EVAL_LLM_API_KEY")
        .assert()
        .code(2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_offline_smoke() {
    let report = bench(
        &fixtures().join("openapi/orders-3.1.yaml"),
        Duration::from_secs(1),
        4,
    )
    .await
    .unwrap();
    assert!(report.requests > 0, "{report:?}");
    assert_eq!(report.errors, 0, "{report:?}");
    assert_eq!(report.concurrency, 4);
    assert!(report.rps > 0.0);
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
