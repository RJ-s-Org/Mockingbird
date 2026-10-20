//! Onboarding commands: `init` writes a starter `mockingbird.yaml`, and
//! `llm providers|configure|test` list the provider presets, add a profile
//! to a config file (or a running server) and check that it answers.
//!
//! Every command prints one JSON document on stdout, so agents and scripts
//! can read the outcome.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use clap::{Args, Subcommand};
use serde_json::{Map, Value, json};

use crate::file_config::{self, FileProfile};
use crate::providers::{self, PRESETS, ProviderArgs};

/// `mockingbird init`.
#[derive(Args, Debug, Clone)]
pub struct InitArgs {
    #[command(flatten)]
    pub provider: ProviderArgs,
    /// No LLM: serve with the built-in faker only.
    #[arg(long, conflicts_with = "provider")]
    pub offline: bool,
    /// Name of the LLM profile [default: the provider preset's id].
    #[arg(long)]
    pub profile_name: Option<String>,
    /// A contract to mock (repeatable); the mock is named after the file
    /// stem and mounted at `/<name>`.
    #[arg(long = "contract", value_name = "PATH")]
    pub contracts: Vec<PathBuf>,
    /// Control-plane storage [default: sqlite://./mockingbird.db].
    #[arg(long)]
    pub storage_url: Option<String>,
    /// File to write.
    #[arg(short, long, default_value = "mockingbird.yaml")]
    pub output: PathBuf,
    /// Overwrite an existing file.
    #[arg(long)]
    pub force: bool,
}

/// `mockingbird llm …`.
#[derive(Subcommand, Debug)]
pub enum LlmCommand {
    /// List the provider presets as JSON.
    Providers,
    /// Add (or replace) an LLM profile built from a preset in a config
    /// file, or with --apply in the running server's Admin API.
    Configure(ConfigureArgs),
    /// Send one small completion through a profile and report whether the
    /// provider answers with JSON, and how fast.
    Test(TestArgs),
}

#[derive(Args, Debug, Clone)]
pub struct ConfigureArgs {
    #[command(flatten)]
    pub provider: ProviderArgs,
    /// Profile name [default: the provider preset's id].
    #[arg(long)]
    pub name: Option<String>,
    /// Config file to update (created if missing).
    #[arg(short, long, default_value = "mockingbird.yaml")]
    pub config: PathBuf,
    /// Make it `llm.default_profile`.
    #[arg(long)]
    pub default: bool,
    /// Profile tried when this one's provider fails.
    #[arg(long)]
    pub fallback: Option<String>,
    /// Write the profile through the Admin API (`--admin-url`,
    /// `--workspace`) instead of the config file.
    #[arg(long)]
    pub apply: bool,
    /// Print the profile without writing anything.
    #[arg(long, conflicts_with = "apply")]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct TestArgs {
    #[command(flatten)]
    pub provider: ProviderArgs,
    /// Test this profile of a config file instead of a preset.
    #[arg(short, long, requires = "profile", conflicts_with = "provider")]
    pub config: Option<PathBuf>,
    /// The profile of --config to test.
    #[arg(long)]
    pub profile: Option<String>,
}

/// The preset catalogue, with which key variables are set here.
pub fn providers_json() -> Value {
    let list: Vec<Value> = PRESETS
        .iter()
        .map(|p| {
            let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
            let key_set = p
                .key_env
                .map(|e| std::env::var(e).is_ok_and(|v| !v.trim().is_empty()));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("key_env_set".into(), json!(key_set));
                obj.insert(
                    "headers".into(),
                    p.headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), json!(v)))
                        .collect::<Map<_, _>>()
                        .into(),
                );
            }
            v
        })
        .collect();
    json!({ "providers": list })
}

/// A profile as written to `mockingbird.yaml`: only the keys that differ
/// from the defaults.
fn profile_yaml_value(p: &FileProfile, fallback: Option<&str>) -> Value {
    let defaults = FileProfile::new("", "");
    let mut obj = Map::new();
    obj.insert("base_url".into(), json!(p.base_url));
    obj.insert("model".into(), json!(p.model));
    if let Some(key) = &p.api_key {
        obj.insert("api_key".into(), json!(key));
    }
    if p.json_schema {
        obj.insert("json_schema".into(), json!(true));
    }
    if p.timeout != defaults.timeout {
        obj.insert(
            "timeout".into(),
            json!(humantime::format_duration(p.timeout).to_string()),
        );
    }
    if let Some(f) = fallback.or(p.fallback.as_deref()) {
        obj.insert("fallback".into(), json!(f));
    }
    if !p.headers.is_empty() {
        obj.insert("headers".into(), json!(p.headers));
    }
    Value::Object(obj)
}

fn preset_id(args: &ProviderArgs) -> anyhow::Result<String> {
    args.provider
        .clone()
        .ok_or_else(|| anyhow!("--provider is required (see `mockingbird llm providers`)"))
}

/// Next steps for the secret references `profile` needs.
fn secret_steps(profile: &FileProfile, preset: &str) -> (Vec<Value>, Vec<String>) {
    let secrets = providers::secret_status(profile);
    let key_url = providers::preset(preset).ok().and_then(|p| p.key_url);
    let steps = secrets
        .iter()
        .filter(|s| s["set"] == false)
        .filter_map(|s| s["ref"].as_str())
        .map(|r| match r.strip_prefix("env:") {
            Some(name) => match key_url {
                Some(url) => format!("export {name}=<key from {url}>"),
                None => format!("export {name}=<key>"),
            },
            None => format!("create {}", r.trim_start_matches("file:")),
        })
        .collect();
    (secrets, steps)
}

/// `mockingbird init`.
pub fn init(args: &InitArgs) -> anyhow::Result<Value> {
    if args.output.exists() && !args.force {
        bail!(
            "{} already exists; pass --force to overwrite it, or `mockingbird llm configure` to add a profile",
            args.output.display()
        );
    }
    let mut doc = Map::new();
    doc.insert(
        "storage".into(),
        json!({ "url": args.storage_url.as_deref().unwrap_or("sqlite://./mockingbird.db") }),
    );
    let mut next = Vec::new();
    let mut secrets = Vec::new();
    let profile_name = if args.offline {
        None
    } else {
        let id = match &args.provider.provider {
            Some(id) => id.clone(),
            None => bail!(
                "choose an LLM provider with --provider (see `mockingbird llm providers`) or pass --offline"
            ),
        };
        let profile = args.provider.profile()?;
        let name = args.profile_name.clone().unwrap_or_else(|| id.clone());
        let (s, steps) = secret_steps(&profile, &id);
        secrets = s;
        next.extend(steps);
        doc.insert(
            "llm".into(),
            json!({
                "default_profile": name,
                "profiles": { name.clone(): profile_yaml_value(&profile, None) },
            }),
        );
        Some(name)
    };
    let base = args
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut mocks = Map::new();
    for contract in &args.contracts {
        let stem = contract
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("{} has no file name", contract.display()))?;
        let name = mock_name(stem);
        mb_store::validate_mock_name(&name)?;
        let path = relative_to(contract, &base);
        mocks.insert(name, json!({ "contract": path }));
    }
    if !mocks.is_empty() {
        doc.insert("mocks".into(), Value::Object(mocks));
    }
    let header = "# Mockingbird configuration, written by `mockingbird init`.\n\
                  # Every key: docs/configuration.md. Secrets are references (env:NAME), never values.\n";
    let text = format!(
        "{header}{}",
        serde_saphyr::to_string(&Value::Object(doc)).context("rendering YAML")?
    );
    std::fs::write(&args.output, &text)
        .with_context(|| format!("writing {}", args.output.display()))?;
    file_config::load(&args.output)
        .with_context(|| format!("{} does not load", args.output.display()))?;
    if args.contracts.is_empty() {
        next.push(format!(
            "add a mock: mocks.<name>.contract in {} (or `mockingbird import FILE --as NAME` once serving)",
            args.output.display()
        ));
    }
    if let Some(name) = &profile_name {
        next.push(format!(
            "check the provider: mockingbird llm test -c {} --profile {name}",
            args.output.display()
        ));
    }
    next.push(format!(
        "serve: mockingbird server start -c {}  (or in the foreground: mockingbird serve -c {} --watch)",
        args.output.display(),
        args.output.display()
    ));
    Ok(json!({
        "written": args.output,
        "profile": profile_name,
        "secrets": secrets,
        "config": text,
        "next_steps": next,
    }))
}

/// A mock name from a file stem: lowercase, other characters as `-`.
fn mock_name(stem: &str) -> String {
    let mut name: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    name = name.trim_matches('-').to_string();
    name.truncate(63);
    if name.is_empty() { "mock".into() } else { name }
}

/// `path` relative to `base` when it lies below it, else absolute.
fn relative_to(path: &Path, base: &Path) -> String {
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let (p, b) = (abs(path), abs(base));
    match p.strip_prefix(&b) {
        Ok(rel) => format!("./{}", rel.display()),
        Err(_) => p.display().to_string(),
    }
}

/// `mockingbird llm configure` without `--apply`: the profile, and the
/// config file's new text unless `dry_run`.
pub fn configure_file(args: &ConfigureArgs) -> anyhow::Result<Value> {
    let id = preset_id(&args.provider)?;
    let profile = args.provider.profile()?;
    let name = args.name.clone().unwrap_or_else(|| id.clone());
    if !mb_admin_identifier(&name) {
        bail!("profile names are 1-128 characters of [A-Za-z0-9._-]");
    }
    let (secrets, mut next) = secret_steps(&profile, &id);
    let entry = profile_yaml_value(&profile, args.fallback.as_deref());
    if args.dry_run {
        return Ok(json!({ "profile": name, "yaml": { name.clone(): entry }, "secrets": secrets }));
    }
    let path = &args.config;
    let existed = path.exists();
    let original = if existed {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    let mut doc: Value = if original.trim().is_empty() {
        json!({ "storage": { "url": "sqlite://./mockingbird.db" } })
    } else {
        serde_saphyr::from_str(&original)
            .with_context(|| format!("{} is not valid YAML", path.display()))?
    };
    let root = doc
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a YAML mapping", path.display()))?;
    let llm = root
        .entry("llm")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("llm: is not a mapping"))?;
    llm.entry("profiles")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("llm.profiles is not a mapping"))?
        .insert(name.clone(), entry);
    let make_default = args.default || !llm.contains_key("default_profile");
    if make_default {
        llm.insert("default_profile".into(), json!(name));
    }
    // The leading comment block is kept; other comments are lost in the
    // rewrite, so the original is then kept next to it.
    let header: String = original
        .lines()
        .take_while(|l| l.trim_start().starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    let text = format!(
        "{header}{}",
        serde_saphyr::to_string(&doc).context("rendering YAML")?
    );
    let backup = if existed && original[header.len()..].contains('#') {
        let backup = path.with_extension("yaml.bak");
        std::fs::write(&backup, &original)
            .with_context(|| format!("writing {}", backup.display()))?;
        Some(backup)
    } else {
        None
    };
    std::fs::write(path, &text).with_context(|| format!("writing {}", path.display()))?;
    if let Err(e) = file_config::load(path) {
        std::fs::write(path, &original).ok();
        return Err(e.context(format!(
            "the updated {} does not load; left unchanged",
            path.display()
        )));
    }
    next.push(format!(
        "check it: mockingbird llm test -c {} --profile {name}",
        path.display()
    ));
    next.push(
        "a server started with --watch picks the change up; otherwise restart it (mockingbird server stop && mockingbird server start -c …)".into(),
    );
    Ok(json!({
        "written": path,
        "profile": name,
        "default_profile": make_default,
        "backup": backup,
        "secrets": secrets,
        "next_steps": next,
    }))
}

fn mb_admin_identifier(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The profile `mockingbird llm test` checks.
pub fn test_profile(args: &TestArgs) -> anyhow::Result<FileProfile> {
    match (&args.config, &args.profile) {
        (Some(config), Some(name)) => {
            let cfg = file_config::load(config)?;
            cfg.llm
                .profiles
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow!("{} has no LLM profile {name:?}", config.display()))
        }
        _ => args.provider.profile(),
    }
}

/// `mockingbird validate` as JSON: the contract's title, version,
/// operations and channels.
pub fn contract_summary(path: &Path) -> anyhow::Result<Value> {
    let bytes =
        mb_import::read_source(path).with_context(|| format!("cannot read {}", path.display()))?;
    let bundle =
        mb_import::compile(&bytes).with_context(|| format!("cannot compile {}", path.display()))?;
    let operations: Vec<Value> = bundle
        .operations
        .iter()
        .map(|op| json!({ "id": op.id, "target": op.target(), "kind": op.kind.label() }))
        .collect();
    let channels: Vec<Value> = bundle
        .channels
        .iter()
        .map(|c| {
            let direction = match c.direction {
                mb_mcm::Direction::Publish => "publish",
                mb_mcm::Direction::Subscribe => "subscribe",
            };
            json!({
                "id": c.id,
                "direction": direction,
                "protocol": c.binding.protocol(),
                "address": c.binding.address(),
            })
        })
        .collect();
    Ok(json!({
        "title": bundle.title,
        "version": bundle.version,
        "operations": operations,
        "channels": channels,
    }))
}

/// The Admin API body for `mockingbird llm configure --apply`.
pub fn configure_api_body(args: &ConfigureArgs) -> anyhow::Result<(String, Value)> {
    let id = preset_id(&args.provider)?;
    let profile = args.provider.profile()?;
    let name = args.name.clone().unwrap_or(id);
    Ok((
        name,
        providers::api_body(&profile, args.fallback.as_deref()),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn mock_names_from_stems() {
        assert_eq!(mock_name("Orders API"), "orders-api");
        assert_eq!(mock_name("petstore.v3"), "petstore-v3");
        assert_eq!(mock_name("__"), "mock");
    }
}
