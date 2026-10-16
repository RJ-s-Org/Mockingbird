use std::net::SocketAddr;
use std::process::ExitCode;

use clap::Parser;
use mockingbird::client::{self, ClientCommand, ClientError};
use mockingbird::config::{
    Cli, Command, LlmEnv, ServeArgs, bind_admin, bind_grpc, build_app, missing_env_api_key,
    resolve_config,
};
use mockingbird::telemetry;

/// How long open connections may delay shutdown after the stop signal.
const GRACEFUL_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
        Command::Openapi => openapi(),
        Command::Client(cmd) => client(&cli.admin_url, cli.token.as_deref(), cmd).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(2)
        }
        Err(Failure::Silent) => ExitCode::from(1),
        Err(Failure::Other(e)) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

enum Failure {
    Usage(String),
    /// Already reported on stderr; exit 1.
    Silent,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Failure::Other(e)
    }
}

/// Runs a client subcommand: JSON on stdout, problem details on stderr.
async fn client(admin_url: &str, token: Option<&str>, cmd: ClientCommand) -> Result<(), Failure> {
    match client::run(admin_url, token, cmd).await {
        Ok(Some(value)) => {
            let text = serde_json::to_string_pretty(&value).map_err(anyhow::Error::from)?;
            println!("{text}");
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(ClientError::Api { status, body }) => {
            let body = match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(problem) => serde_json::to_string_pretty(&problem).unwrap_or(body),
                Err(_) if body.trim().is_empty() => format!("HTTP {status}"),
                Err(_) => format!("HTTP {status}: {body}"),
            };
            eprintln!("{body}");
            Err(Failure::Silent)
        }
        Err(ClientError::Other(e)) => Err(Failure::Other(e)),
    }
}

/// Prints the Admin API's OpenAPI document.
fn openapi() -> Result<(), Failure> {
    let text = mb_admin::openapi_document()
        .to_pretty_json()
        .map_err(anyhow::Error::from)?;
    println!("{text}");
    Ok(())
}

fn validate(path: &std::path::Path) -> Result<(), Failure> {
    use anyhow::Context;
    let bytes =
        mb_import::read_source(path).with_context(|| format!("cannot read {}", path.display()))?;
    let bundle =
        mb_import::compile(&bytes).with_context(|| format!("cannot compile {}", path.display()))?;
    println!(
        "{} {} ({} operations)",
        bundle.title,
        bundle.version,
        bundle.operations.len()
    );
    for op in &bundle.operations {
        let streams = op.kind.streams_request() || op.kind.streams_response();
        if streams {
            println!("{:<32} {} [{}]", op.id, op.target(), op.kind.label());
        } else {
            println!("{:<32} {}", op.id, op.target());
        }
    }
    Ok(())
}

async fn serve(args: ServeArgs) -> Result<(), Failure> {
    let llm = LlmEnv::from_env();
    let cfg = resolve_config(&args, Some(&llm))?;
    cfg.server.check_bind(&cfg.auth).map_err(Failure::Usage)?;
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
    let admin_listener = bind_admin(admin_addr, cfg.server.admin_port_explicit).await?;
    let grpc_addr = SocketAddr::new(cfg.server.host, cfg.server.grpc_port);
    let grpc_listener = bind_grpc(grpc_addr, cfg.server.grpc_port_explicit).await?;

    let app = build_app(&args, Some(llm)).await?;
    match &admin_listener {
        Some(_) => {
            tracing::info!("data plane on http://{data_addr}, admin API on http://{admin_addr}")
        }
        None => tracing::info!("data plane on http://{data_addr}, no admin API"),
    }
    match &grpc_listener {
        Some(_) => tracing::info!("gRPC (h2c) on {grpc_addr}"),
        None => tracing::info!("no gRPC listener"),
    }

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutting down");
        mockingbird::runtime::stop_open_streams();
        let _ = stop_tx.send(true);
    });
    let stopped = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|stop| *stop).await;
    };
    let grace_rx = stop_rx.clone();
    let data = axum::serve(data_listener, app.data.clone())
        .with_graceful_shutdown(stopped(stop_rx.clone()));
    let admin_router = app.admin.clone();
    let admin_stop = stop_rx.clone();
    let admin = async move {
        match admin_listener {
            Some(listener) => {
                // The peer address keys the login-failure audit rate limit.
                axum::serve(
                    listener,
                    admin_router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(stopped(admin_stop))
                .await
            }
            None => Ok(()),
        }
    };
    // axum serves HTTP/2 prior knowledge (h2c), which is what gRPC uses.
    let grpc_router = app.grpc.clone();
    let grpc = async move {
        match grpc_listener {
            Some(listener) => {
                axum::serve(listener, grpc_router)
                    .with_graceful_shutdown(stopped(stop_rx))
                    .await
            }
            None => Ok(()),
        }
    };
    let servers = async { tokio::join!(data, admin, grpc) };
    // Connections still open this long after the stop signal (a gRPC
    // stream, a WebSocket) are abandoned, so the request log is still
    // flushed before the supervisor's kill.
    let grace = async {
        stopped(grace_rx).await;
        tokio::time::sleep(GRACEFUL_SHUTDOWN_TIMEOUT).await;
    };
    tokio::select! {
        (data, admin, grpc) = servers => {
            data.map_err(|e| anyhow::anyhow!("data plane: {e}"))?;
            admin.map_err(|e| anyhow::anyhow!("admin API: {e}"))?;
            grpc.map_err(|e| anyhow::anyhow!("gRPC: {e}"))?;
        }
        () = grace => tracing::warn!(
            "connections still open {GRACEFUL_SHUTDOWN_TIMEOUT:?} after the stop signal; closing them"
        ),
    }
    app.shutdown().await;
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
