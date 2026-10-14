use std::net::SocketAddr;
use std::process::ExitCode;

use clap::Parser;
use mockingbird::config::{
    Cli, Command, LlmEnv, ServeArgs, build_app, missing_env_api_key, resolve_config,
};
use mockingbird::telemetry;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // Kept alive until exit so buffered spans are flushed.
    let _telemetry = match telemetry::init("mockingbird") {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("warning: telemetry disabled: {e:#}");
            None
        }
    };
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
    let llm = LlmEnv::from_env();
    let cfg = resolve_config(&args, Some(&llm))?;
    cfg.server.check_bind().map_err(Failure::Usage)?;
    if missing_env_api_key(&cfg, args.offline) {
        return Err(Failure::Usage(
            "MB_LLM_API_KEY is not set (use --offline for faker-only mode)".into(),
        ));
    }
    let data_addr = SocketAddr::new(cfg.server.host, cfg.server.port);
    let admin_addr = SocketAddr::new(cfg.server.host, cfg.server.admin_port);
    let data_listener = tokio::net::TcpListener::bind(data_addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind {data_addr}: {e}"))?;
    let admin_listener = tokio::net::TcpListener::bind(admin_addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind {admin_addr}: {e}"))?;

    let app = build_app(&args, Some(llm)).await?;
    tracing::info!("data plane on http://{data_addr}, admin API on http://{admin_addr}");

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = stop_tx.send(true);
    });
    let stopped = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|stop| *stop).await;
    };
    let data = axum::serve(data_listener, app.data.clone())
        .with_graceful_shutdown(stopped(stop_rx.clone()));
    let admin =
        axum::serve(admin_listener, app.admin.clone()).with_graceful_shutdown(stopped(stop_rx));
    let (data, admin) = tokio::join!(data, admin);
    data.map_err(|e| anyhow::anyhow!("data plane: {e}"))?;
    admin.map_err(|e| anyhow::anyhow!("admin API: {e}"))?;
    drop(app);
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
