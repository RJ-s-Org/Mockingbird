//! `mb-evals run` and `mb-evals bench`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use mb_evals::report::{EvalReport, Thresholds, run_evals};
use mb_evals::{bench, corpus};
use mb_llm::{OpenAiCompatible, OpenAiConfig};

/// Exit code of a usage error (clap's own code for bad arguments).
const EXIT_USAGE: u8 = 2;
/// Exit code of a run that broke a threshold.
const EXIT_THRESHOLD: u8 = 1;

/// The environment variables `run` reads.
const ENV_BASE_URL: &str = "MB_EVAL_LLM_BASE_URL";
const ENV_MODEL: &str = "MB_EVAL_LLM_MODEL";
const ENV_API_KEY: &str = "MB_EVAL_LLM_API_KEY";

#[derive(Parser, Debug)]
#[command(
    name = "mb-evals",
    about = "Mockingbird LLM evals and throughput bench"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Evaluate a real LLM provider against the contract corpus. Reads
    /// MB_EVAL_LLM_BASE_URL, MB_EVAL_LLM_MODEL and MB_EVAL_LLM_API_KEY.
    Run(RunArgs),
    /// Measure offline (faker-only) data-plane throughput for a contract.
    Bench(BenchArgs),
}

#[derive(clap::Args, Debug)]
struct RunArgs {
    /// Directory of contracts to evaluate.
    #[arg(long, default_value = "fixtures")]
    corpus: PathBuf,
    /// Directory `report.json` and `report.md` are written to.
    #[arg(long, default_value = "eval-report")]
    out: PathBuf,
    /// Calls per contract, at most.
    #[arg(long, default_value_t = 10)]
    max_ops_per_contract: usize,
    #[arg(long, default_value_t = Thresholds::default().min_schema_valid_rate)]
    min_schema_valid_rate: f64,
    #[arg(long, default_value_t = Thresholds::default().max_repair_rate)]
    max_repair_rate: f64,
    #[arg(long, default_value_t = Thresholds::default().max_fallback_rate)]
    max_fallback_rate: f64,
    #[arg(long, default_value_t = Thresholds::default().max_p95_ms)]
    max_p95_ms: u64,
}

#[derive(clap::Args, Debug)]
struct BenchArgs {
    /// The contract to serve.
    #[arg(long, default_value = "fixtures/openapi/orders-3.1.yaml")]
    contract: PathBuf,
    /// How long to drive load.
    #[arg(long, default_value = "10s")]
    duration: humantime::Duration,
    /// Concurrent client workers.
    #[arg(long, default_value_t = 32)]
    concurrency: usize,
    /// Where the JSON report is written.
    #[arg(long, default_value = "bench.json")]
    out: PathBuf,
}

/// The provider settings `run` takes from the environment.
struct EvalEnv {
    base_url: String,
    model: String,
    api_key: String,
}

impl EvalEnv {
    /// Every variable must be set and non-empty; `Err` names the missing
    /// ones.
    fn from_env() -> Result<Self, String> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let (base_url, model, api_key) = (get(ENV_BASE_URL), get(ENV_MODEL), get(ENV_API_KEY));
        match (base_url, model, api_key) {
            (Some(base_url), Some(model), Some(api_key)) => Ok(Self {
                base_url,
                model,
                api_key,
            }),
            (b, m, k) => {
                let missing: Vec<&str> = [(b, ENV_BASE_URL), (m, ENV_MODEL), (k, ENV_API_KEY)]
                    .into_iter()
                    .filter(|(v, _)| v.is_none())
                    .map(|(_, name)| name)
                    .collect();
                Err(format!("missing environment: {}", missing.join(", ")))
            }
        }
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,mb_evals=info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match cli.cmd {
        Cmd::Run(args) => {
            let env = match EvalEnv::from_env() {
                Ok(env) => env,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::from(EXIT_USAGE);
                }
            };
            runtime.block_on(run(args, env))
        }
        Cmd::Bench(args) => runtime
            .block_on(run_bench(args))
            .map(|()| ExitCode::SUCCESS),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: RunArgs, env: EvalEnv) -> anyhow::Result<ExitCode> {
    let entries = corpus::load(&args.corpus)?;
    eprintln!(
        "evaluating {} contracts with {} at {}",
        entries.len(),
        env.model,
        env.base_url
    );
    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: env.base_url.clone(),
        api_key: Some(env.api_key),
        model: env.model.clone(),
        // The server default (`MB_LLM_JSON_SCHEMA` unset): JSON mode, which
        // every OpenAI-compatible provider supports.
        json_schema: false,
        http: mb_llm::HttpOptions::default(),
    })
    .context("building the LLM client")?;
    let mut report = run_evals(
        &entries,
        Arc::new(provider),
        &env.model,
        args.max_ops_per_contract,
    )
    .await;
    report.base_url = env.base_url;
    write_reports(&args.out, &report)?;
    print!("{}", report.to_markdown());

    let thresholds = Thresholds {
        min_schema_valid_rate: args.min_schema_valid_rate,
        max_repair_rate: args.max_repair_rate,
        max_fallback_rate: args.max_fallback_rate,
        max_p95_ms: args.max_p95_ms,
    };
    let violations = report.violations(&thresholds);
    if violations.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    for v in &violations {
        eprintln!("threshold violated: {v}");
    }
    Ok(ExitCode::from(EXIT_THRESHOLD))
}

fn write_reports(out: &Path, report: &EvalReport) -> anyhow::Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
    let json = serde_json::to_string_pretty(report)?;
    let json_path = out.join("report.json");
    std::fs::write(&json_path, json + "\n")
        .with_context(|| format!("cannot write {}", json_path.display()))?;
    let md_path = out.join("report.md");
    std::fs::write(&md_path, report.to_markdown())
        .with_context(|| format!("cannot write {}", md_path.display()))?;
    Ok(())
}

async fn run_bench(args: BenchArgs) -> anyhow::Result<()> {
    let report = bench::bench(&args.contract, args.duration.into(), args.concurrency).await?;
    let json = serde_json::to_string_pretty(&report)?;
    println!("{json}");
    if let Some(dir) = args.out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::write(&args.out, json + "\n")
        .with_context(|| format!("cannot write {}", args.out.display()))?;
    Ok(())
}
