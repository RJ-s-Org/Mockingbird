//! `mockingbird-custom-ext`: `mockingbird serve` with the example's
//! extensions (tenants from `X-Tenant`, 100 calls per tenant).

use std::process::ExitCode;

use clap::Parser;
use mockingbird::config::{LlmEnv, ServeArgs};
use mockingbird::server::{self, ServeError};
use mockingbird_custom_ext_example::extensions;

/// Calls each tenant may make before it is answered 429.
const CALLS_PER_TENANT: u64 = 100;

/// Mockingbird with header-based tenants and a fixed per-tenant quota.
#[derive(Parser, Debug)]
#[command(name = "mockingbird-custom-ext", version)]
struct Cli {
    #[command(flatten)]
    serve: ServeArgs,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match server::run_with(
        cli.serve,
        Some(LlmEnv::from_env()),
        extensions(CALLS_PER_TENANT),
    )
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(ServeError::Usage(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}
