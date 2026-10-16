use std::process::ExitCode;

use clap::Parser;
use mockingbird::client::{self, ClientCommand, ClientError};
use mockingbird::config::{Cli, Command};
use mockingbird::server::{self, ServeError};
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
        Command::Serve(args) => server::run(args).await.map_err(|e| match e {
            ServeError::Usage(msg) => Failure::Usage(msg),
            ServeError::Other(e) => Failure::Other(e),
        }),
        Command::Validate { contract } => validate(&contract),
        Command::Openapi => openapi(),
        Command::Client(cmd) => {
            client(&cli.admin_url, cli.token.as_deref(), &cli.workspace, cmd).await
        }
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
async fn client(
    admin_url: &str,
    token: Option<&str>,
    workspace: &str,
    cmd: ClientCommand,
) -> Result<(), Failure> {
    match client::run(admin_url, token, workspace, cmd).await {
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
        Err(ClientError::Usage(msg)) => Err(Failure::Usage(msg)),
        Err(ClientError::Incomplete { output, message }) => {
            let text = serde_json::to_string_pretty(&output).map_err(anyhow::Error::from)?;
            println!("{text}");
            eprintln!("{message}");
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
        "{} {} ({} operations, {} channels)",
        bundle.title,
        bundle.version,
        bundle.operations.len(),
        bundle.channels.len()
    );
    for op in &bundle.operations {
        let streams = op.kind.streams_request() || op.kind.streams_response();
        if streams {
            println!("{:<32} {} [{}]", op.id, op.target(), op.kind.label());
        } else {
            println!("{:<32} {}", op.id, op.target());
        }
    }
    for c in &bundle.channels {
        let direction = match c.direction {
            mb_mcm::Direction::Publish => "publish",
            mb_mcm::Direction::Subscribe => "subscribe",
        };
        println!(
            "{:<32} {} {} {}",
            c.id,
            direction,
            c.binding.protocol(),
            c.binding.address()
        );
    }
    Ok(())
}
