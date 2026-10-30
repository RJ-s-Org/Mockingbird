//! LLM keys kept out of configs and agents: a private file store under
//! `~/.mockingbird/secrets` and a one-shot local page to type a key into.

pub mod page;
pub mod store;

use std::path::{Path, PathBuf};

use anyhow::bail;
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use page::{PageOptions, ProfileTarget};

#[derive(Subcommand, Debug)]
pub enum KeysCommand {
    /// Open a local page to type an LLM key into; it is stored in
    /// `~/.mockingbird/secrets/<name>` (mode 0600) and the profile is pointed
    /// at the file.
    Setup(SetupArgs),
}

#[derive(Args, Debug, Clone)]
pub struct SetupArgs {
    /// The key's name in the store [default: the profile's name].
    pub name: Option<String>,
    /// Config whose LLM profile should reference the stored key.
    #[arg(short = 'c', long, default_value = "mockingbird.yaml")]
    pub config: PathBuf,
    /// The profile to update [default: llm.default_profile, or the only one].
    #[arg(long)]
    pub profile: Option<String>,
    /// Print the page's URL without opening a browser.
    #[arg(long)]
    pub no_open: bool,
    /// How long the page waits for the key.
    #[arg(long, default_value = "10m")]
    pub timeout: humantime::Duration,
}

/// The page options for storing a key named `name` (default: the profile's
/// name) and updating `profile` of `config` when that config exists.
///
/// # Errors
///
/// Fails when no key name can be chosen or it is invalid, or the profile
/// cannot be resolved (see [`resolve_profile`](crate::onboard::resolve_profile)).
pub fn page_options(
    name: Option<&str>,
    config: &Path,
    profile: Option<&str>,
    timeout: std::time::Duration,
) -> anyhow::Result<PageOptions> {
    let target = if config.exists() {
        let profile = crate::onboard::resolve_profile(config, profile)?;
        Some(ProfileTarget {
            config: std::path::absolute(config)?,
            profile,
        })
    } else if profile.is_some() {
        bail!(
            "{} does not exist, so it has no profile to update",
            config.display()
        );
    } else {
        None
    };
    let name = match (name, &target) {
        (Some(n), _) => n.to_string(),
        (None, Some(t)) => t.profile.clone(),
        (None, None) => bail!("name the key: mockingbird keys setup <name>"),
    };
    if !store::valid_name(&name) {
        bail!("key names are 1-64 characters of [A-Za-z0-9._-], not starting with a dot");
    }
    Ok(PageOptions {
        name,
        store_dir: store::default_store_dir()?,
        profile: target,
        timeout,
    })
}

/// `mockingbird keys setup`: prints the URL on stderr, waits for the key,
/// and returns where it went (never the key).
///
/// # Errors
///
/// Fails when the page cannot start, no key is saved before the timeout,
/// or the key or the config file cannot be written.
pub async fn setup(args: &SetupArgs) -> anyhow::Result<Value> {
    let opts = page_options(
        args.name.as_deref(),
        &args.config,
        args.profile.as_deref(),
        args.timeout.into(),
    )?;
    let page = page::start(opts).await?;
    eprintln!("Open {} to enter the key", page.url);
    if !args.no_open && !page::open_browser(&page.url) {
        eprintln!("(could not open a browser; open the address above on this machine)");
    }
    let saved = page.wait().await?;
    Ok(json!({
        "saved": saved.name,
        "file": saved.file,
        "profile": saved.profile,
        "config": saved.config,
    }))
}
