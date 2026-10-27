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
    /// Data-plane port (`server.port`) [default: 8080].
    #[arg(long)]
    pub port: Option<u16>,
    /// Admin API and console port (`server.admin_port`) [default: 9090].
    #[arg(long)]
    pub admin_port: Option<u16>,
    /// gRPC port (`server.grpc_port`) [default: 50051].
    #[arg(long)]
    pub grpc_port: Option<u16>,
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
    /// Test a profile of this config file instead of a preset [default:
    /// ./mockingbird.yaml when no --provider is given].
    #[arg(short, long, conflicts_with = "provider")]
    pub config: Option<PathBuf>,
    /// The profile of the config file to test [default: its
    /// `llm.default_profile`].
    #[arg(long, conflicts_with = "provider")]
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
    let mut server = Map::new();
    for (key, port) in [
        ("port", args.port),
        ("admin_port", args.admin_port),
        ("grpc_port", args.grpc_port),
    ] {
        if let Some(port) = port {
            server.insert(key.into(), json!(port));
        }
    }
    if !server.is_empty() {
        doc.insert("server".into(), Value::Object(server));
    }
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
    let mut make_default = false;
    let backup = rewrite_config(path, |root| {
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
        make_default = args.default || !llm.contains_key("default_profile");
        if make_default {
            llm.insert("default_profile".into(), json!(name));
        }
        Ok(())
    })?;
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

/// Rewrites the config at `path` (a new one when missing) with `edit`
/// applied to its root mapping. The leading comment block is kept; other
/// comments are lost in the rewrite, so the original is then kept as
/// `.yaml.bak` (returned). A result that does not load is rolled back.
/// Nothing is written when `edit` fails.
fn rewrite_config(
    path: &Path,
    edit: impl FnOnce(&mut Map<String, Value>) -> anyhow::Result<()>,
) -> anyhow::Result<Option<PathBuf>> {
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
    edit(root)?;
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
    Ok(backup)
}

/// The LLM profile of `config` to update: `profile` (which must exist),
/// else `llm.default_profile`, else the only profile.
pub fn resolve_profile(config: &Path, profile: Option<&str>) -> anyhow::Result<String> {
    let cfg = file_config::load(config)?;
    if let Some(name) = profile {
        if cfg.llm.profiles.contains_key(name) {
            return Ok(name.to_string());
        }
        bail!("{} has no LLM profile {name:?}", config.display());
    }
    if let Some(name) = cfg.llm.default_profile.filter(|n| cfg.llm.profiles.contains_key(n)) {
        return Ok(name);
    }
    if cfg.llm.profiles.len() == 1
        && let Some(name) = cfg.llm.profiles.keys().next()
    {
        return Ok(name.clone());
    }
    bail!(
        "{} has no LLM profile to update; create one with `mockingbird llm configure` (MCP configure_llm)",
        config.display()
    )
}

/// Points `llm.profiles.<profile>.api_key` of `config` at `reference` (a
/// secret reference, never a value). A missing profile writes nothing.
pub fn set_profile_api_key(config: &Path, profile: &str, reference: &str) -> anyhow::Result<Value> {
    let backup = rewrite_config(config, |root| {
        let entry = root
            .get_mut("llm")
            .and_then(|l| l.get_mut("profiles"))
            .and_then(|p| p.get_mut(profile))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow!("{} has no LLM profile {profile:?}", config.display()))?;
        entry.insert("api_key".into(), json!(reference));
        Ok(())
    })?;
    Ok(json!({ "config": config, "profile": profile, "backup": backup }))
}

fn mb_admin_identifier(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The profile `mockingbird llm test` checks.
pub fn test_profile(args: &TestArgs) -> anyhow::Result<FileProfile> {
    if args.provider.provider.is_some() {
        return args.provider.profile();
    }
    let config = match &args.config {
        Some(config) => config.clone(),
        None if Path::new("mockingbird.yaml").exists() => PathBuf::from("mockingbird.yaml"),
        None => bail!(
            "name a provider preset with --provider (see `mockingbird llm providers`), or run this next to a mockingbird.yaml (or pass --config)"
        ),
    };
    let cfg = file_config::load(&config)?;
    let name = match args.profile.clone().or(cfg.llm.default_profile.clone()) {
        Some(name) => name,
        None => bail!(
            "{} has no llm.default_profile; pass --profile",
            config.display()
        ),
    };
    cfg.llm
        .profiles
        .get(&name)
        .cloned()
        .ok_or_else(|| anyhow!("{} has no LLM profile {name:?}", config.display()))
}

/// How many operations and channels [`contract_summary`] lists.
pub const SUMMARY_OPERATIONS: usize = 20;

/// The first [`SUMMARY_OPERATIONS`] items, and how many were left out.
fn capped(items: Vec<Value>) -> (Vec<Value>, usize) {
    let left_out = items.len().saturating_sub(SUMMARY_OPERATIONS);
    (items.into_iter().take(SUMMARY_OPERATIONS).collect(), left_out)
}

/// `mockingbird validate` as JSON: the contract's title, version, counts
/// by protocol, and the first [`SUMMARY_OPERATIONS`] operations and
/// channels (`operations_truncated` / `channels_truncated` say how many
/// more there are).
pub fn contract_summary(path: &Path) -> anyhow::Result<Value> {
    let bytes =
        mb_import::read_source(path).with_context(|| format!("cannot read {}", path.display()))?;
    let bundle =
        mb_import::compile(&bytes).with_context(|| format!("cannot compile {}", path.display()))?;
    let mut counts = Map::new();
    for op in &bundle.operations {
        let protocol = serde_json::to_value(op.protocol)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "other".into());
        let n = counts.get(&protocol).and_then(Value::as_u64).unwrap_or(0);
        counts.insert(protocol, json!(n + 1));
    }
    counts.insert("channels".into(), json!(bundle.channels.len()));
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
    let (operations, operations_truncated) = capped(operations);
    let (channels, channels_truncated) = capped(channels);
    Ok(json!({
        "title": bundle.title,
        "version": bundle.version,
        "counts": counts,
        "operations": operations,
        "operations_truncated": operations_truncated,
        "channels": channels,
        "channels_truncated": channels_truncated,
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
    fn contract_summary_caps_the_operation_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut doc = String::from(
            "openapi: 3.0.3\ninfo: {title: Many, version: '1'}\npaths:\n",
        );
        for i in 0..25 {
            doc.push_str(&format!(
                "  /r{i}:\n    get:\n      operationId: r{i}\n      responses:\n        '200': {{description: ok}}\n"
            ));
        }
        let path = dir.path().join("many.yaml");
        std::fs::write(&path, doc).unwrap();
        let s = contract_summary(&path).unwrap();
        assert_eq!(
            s["operations"].as_array().unwrap().len(),
            SUMMARY_OPERATIONS
        );
        assert_eq!(s["operations_truncated"], 5);
        assert_eq!(s["counts"]["http"], 25);
        assert_eq!(s["counts"]["channels"], 0);
        assert_eq!(s["channels_truncated"], 0);
    }

    /// A config written by `init --provider openrouter` in a tempdir.
    fn openrouter_config(dir: &Path) -> PathBuf {
        let output = dir.join("mockingbird.yaml");
        init(&InitArgs {
            provider: ProviderArgs {
                provider: Some("openrouter".into()),
                ..ProviderArgs::default()
            },
            offline: false,
            profile_name: None,
            contracts: Vec::new(),
            storage_url: None,
            port: None,
            admin_port: None,
            grpc_port: None,
            output: output.clone(),
            force: false,
        })
        .unwrap();
        output
    }

    #[test]
    fn set_profile_api_key_points_the_profile_at_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = openrouter_config(dir.path());
        assert_eq!(resolve_profile(&cfg, None).unwrap(), "openrouter");
        assert_eq!(
            resolve_profile(&cfg, Some("openrouter")).unwrap(),
            "openrouter"
        );
        let out = set_profile_api_key(
            &cfg,
            "openrouter",
            "file:/home/u/.mockingbird/secrets/openrouter",
        )
        .unwrap();
        assert_eq!(out["profile"], "openrouter");
        let loaded = file_config::load(&cfg).unwrap();
        assert_eq!(
            loaded.llm.profiles["openrouter"].api_key.as_deref(),
            Some("file:/home/u/.mockingbird/secrets/openrouter")
        );
        assert!(resolve_profile(&cfg, Some("nope")).is_err());
        let before = std::fs::read_to_string(&cfg).unwrap();
        assert!(set_profile_api_key(&cfg, "nope", "file:/x").is_err());
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
        let empty = dir.path().join("offline.yaml");
        std::fs::write(&empty, "storage:\n  url: sqlite://./x.db\n").unwrap();
        let err = resolve_profile(&empty, None).unwrap_err().to_string();
        assert!(err.contains("no LLM profile"), "{err}");
    }

    #[test]
    fn mock_names_from_stems() {
        assert_eq!(mock_name("Orders API"), "orders-api");
        assert_eq!(mock_name("petstore.v3"), "petstore-v3");
        assert_eq!(mock_name("__"), "mock");
    }
}
