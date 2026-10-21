use std::process::ExitCode;

use clap::Parser;
use mockingbird::agents::{self, AgentCommand};
use mockingbird::client::{self, ClientCommand, ClientError};
use mockingbird::config::{Cli, Command};
use mockingbird::daemon::{self, ServerCommand};
use mockingbird::onboard::{self, LlmCommand};
use mockingbird::server::{self, ServeError};
use mockingbird::telemetry;
use mockingbird::{mcp, providers};

fn main() -> ExitCode {
    let cli = Cli::parse();
    if matches!(cli.cmd, Command::Mcp(_)) {
        // SAFETY: nothing else runs yet; the runtime starts below.
        unsafe { mockingbird::shell_env::import() };
    }
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(run(cli)),
        Err(e) => {
            eprintln!("error: starting the async runtime: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> ExitCode {
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
        Command::Init(args) => print_json(onboard::init(&args)),
        Command::Llm(cmd) => llm(&cli.admin_url, cli.token.as_deref(), &cli.workspace, cmd).await,
        Command::Server(cmd) => server_cmd(cmd).await,
        Command::Doctor(args) => print_json(Ok(daemon::doctor(&args, &cli.admin_url).await)),
        Command::Mcp(args) => mcp::run(args, &cli.admin_url, cli.token.as_deref(), &cli.workspace)
            .await
            .map_err(Failure::Other),
        Command::Agent(cmd) => print_json(match cmd {
            AgentCommand::Install(args) => agents::install(&args),
            AgentCommand::Print { client } => Ok(agents::print(client)),
            AgentCommand::Skills { dir } => agents::skills(&dir),
        }),
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

/// Prints `value` as pretty JSON.
fn print_json(value: anyhow::Result<serde_json::Value>) -> Result<(), Failure> {
    let text = serde_json::to_string_pretty(&value?).map_err(anyhow::Error::from)?;
    println!("{text}");
    Ok(())
}

async fn llm(
    admin_url: &str,
    token: Option<&str>,
    workspace: &str,
    cmd: LlmCommand,
) -> Result<(), Failure> {
    match cmd {
        LlmCommand::Providers => print_json(Ok(onboard::providers_json())),
        LlmCommand::Configure(args) if args.apply => {
            let (name, body) = onboard::configure_api_body(&args)?;
            let segments = ["llm-profiles", name.as_str()];
            match mockingbird::client::api(
                admin_url,
                token,
                Some(workspace),
                reqwest::Method::PUT,
                &segments,
                &[],
                Some(body),
            )
            .await
            {
                Ok(v) => print_json(Ok(v.unwrap_or_default())),
                Err(ClientError::Api { status, body }) => {
                    eprintln!("HTTP {status}: {body}");
                    Err(Failure::Silent)
                }
                Err(ClientError::Usage(m)) => Err(Failure::Usage(m)),
                Err(ClientError::Incomplete { message, .. }) => Err(Failure::Usage(message)),
                Err(ClientError::Other(e)) => Err(Failure::Other(e)),
            }
        }
        LlmCommand::Configure(args) => print_json(onboard::configure_file(&args)),
        LlmCommand::Test(args) => {
            let profile = onboard::test_profile(&args)?;
            let report = providers::probe(&profile).await?;
            let ok = report.ok;
            print_json(serde_json::to_value(report).map_err(anyhow::Error::from))?;
            if ok { Ok(()) } else { Err(Failure::Silent) }
        }
    }
}

async fn server_cmd(cmd: ServerCommand) -> Result<(), Failure> {
    match cmd {
        ServerCommand::Start(args) => print_json(daemon::start(&args).await),
        ServerCommand::Stop(args) => print_json(daemon::stop(&args).await),
        ServerCommand::Status(args) => print_json(daemon::status(&args).await),
        ServerCommand::Logs { state, lines } => {
            println!("{}", daemon::logs(&state, lines)?);
            Ok(())
        }
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
