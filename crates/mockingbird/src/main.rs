use std::net::SocketAddr;
use std::process::ExitCode;

use clap::Parser;
use mockingbird::config::{Cli, Command, LlmEnv, ServeArgs, build_app};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let result = match cli.cmd {
        Command::Serve(args) => serve(args).await,
        Command::Validate { contract } => validate(&contract),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(2)
        }
        Err(Failure::Other(e)) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

enum Failure {
    Usage(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Failure::Other(e)
    }
}

fn validate(path: &std::path::Path) -> Result<(), Failure> {
    use anyhow::Context;
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let bundle =
        mb_import::compile(&bytes).with_context(|| format!("cannot compile {}", path.display()))?;
    println!(
        "{} {} ({} operations)",
        bundle.title,
        bundle.version,
        bundle.operations.len()
    );
    for op in &bundle.operations {
        match &op.binding {
            mb_mcm::Binding::Http { method, path } => {
                println!(
                    "{:<24} {:<7} {}",
                    op.id,
                    format!("{method:?}").to_uppercase(),
                    path
                );
            }
            _ => println!("{}", op.id),
        }
    }
    Ok(())
}

async fn serve(args: ServeArgs) -> Result<(), Failure> {
    args.check_bind().map_err(Failure::Usage)?;
    let llm = LlmEnv::from_env();
    if !args.offline && llm.api_key.is_none() {
        return Err(Failure::Usage(
            "MB_LLM_API_KEY is not set (use --offline for faker-only mode)".into(),
        ));
    }
    let app = build_app(&args, Some(llm))?;
    let addr = SocketAddr::new(args.host, args.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind {addr}: {e}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
