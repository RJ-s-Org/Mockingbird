//! `mockingbird examples list|show|init`: the API examples catalog.
//!
//! Every example in `examples/apis/<name>/` is embedded: its
//! `example.json` (the catalog entry), `mockingbird.yaml`, `README.md` and
//! contract files. A large upstream spec is not embedded; `init --full`
//! downloads it from the commit pinned in `example.json` and checks its
//! SHA-256 before pointing the config at it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// One embedded example: its directory name, catalog entry and files.
pub struct Example {
    pub name: &'static str,
    pub meta: &'static str,
    /// `(file name, contents)`, `mockingbird.yaml` first.
    pub files: &'static [(&'static str, &'static str)],
}

macro_rules! example {
    ($name:literal, [$($file:literal),* $(,)?]) => {
        Example {
            name: $name,
            meta: include_str!(concat!(env!("MB_EXAMPLES_DIR"), "/", $name, "/example.json")),
            files: &[
                ("mockingbird.yaml", include_str!(concat!(env!("MB_EXAMPLES_DIR"), "/", $name, "/mockingbird.yaml"))),
                ("README.md", include_str!(concat!(env!("MB_EXAMPLES_DIR"), "/", $name, "/README.md"))),
                $(($file, include_str!(concat!(env!("MB_EXAMPLES_DIR"), "/", $name, "/", $file))),)*
            ],
        }
    };
}

/// The catalog, in the order `examples list` prints it.
pub const EXAMPLES: &[Example] = &[
    example!("stripe", ["stripe.openapi.yaml"]),
    example!(
        "paypal",
        [
            "paypal-oauth.openapi.yaml",
            "paypal-orders.openapi.yaml",
            "paypal-payments.openapi.yaml"
        ]
    ),
    example!("square", ["square.openapi.yaml"]),
    example!("plaid", ["plaid.openapi.yaml"]),
    example!("github-rest", ["github-rest.openapi.yaml"]),
    example!("github-graphql", ["github.graphql"]),
    example!("shopify-admin-graphql", ["shopify-admin.graphql"]),
    example!(
        "twilio",
        ["twilio-lookups.openapi.yaml", "twilio.openapi.yaml"]
    ),
    example!("slack", ["slack.openapi.yaml"]),
    example!("sendgrid", ["sendgrid.openapi.yaml"]),
    example!("discord", ["discord.openapi.yaml"]),
    example!("hubspot", ["hubspot.openapi.yaml"]),
    example!("notion", ["notion.openapi.yaml"]),
    example!("jira", ["jira.openapi.yaml"]),
    example!("zendesk", ["zendesk.openapi.yaml"]),
    example!("auth0-management", ["auth0-management.openapi.yaml"]),
    example!(
        "spotify",
        ["spotify-accounts.openapi.yaml", "spotify.openapi.yaml"]
    ),
    example!("tmdb", ["tmdb.openapi.yaml"]),
    example!("openweathermap", ["openweathermap.openapi.yaml"]),
    example!("kafka-events", ["commerce-events.asyncapi.yaml"]),
    example!("grpc", ["otlp_trace_service.proto"]),
];

/// An example's `example.json`.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub name: String,
    pub title: String,
    pub category: String,
    /// `openapi`, `graphql`, `grpc`, `asyncapi` or `soap`.
    pub protocol: String,
    pub summary: String,
    /// Contract files, relative to the example's directory.
    pub contracts: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub scenarios: Vec<String>,
    /// The webhook signing scheme, when the example sends webhooks.
    #[serde(default)]
    pub webhooks: Option<String>,
    #[serde(default)]
    pub upstream: Option<Upstream>,
    /// The official SDK the README's snippet uses.
    #[serde(default)]
    pub sdk: Option<Sdk>,
    /// The request the CI smoke job sends (see scripts/smoke-examples.sh).
    pub smoke: Value,
}

/// Where the API's official, complete contract lives.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// The repository or page that publishes it.
    pub source: String,
    /// Its SPDX license id, or `proprietary`/`unknown`.
    pub license: String,
    /// The commit (or version) `url` is pinned to.
    #[serde(default)]
    pub commit: Option<String>,
    /// A download URL pinned to `commit`; absent when the license does not
    /// allow redistribution or there is no stable URL.
    #[serde(default)]
    pub url: Option<String>,
    /// SHA-256 of the file at `url`, checked by `init --full`.
    #[serde(default)]
    pub sha256: Option<String>,
    /// The embedded contract `init --full` points the config away from.
    #[serde(default)]
    pub replaces: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// The official SDK an example's README drives through the proxy.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Sdk {
    /// `python`, `node`, `go`, ...
    pub language: String,
    /// The package as its registry names it (`stripe`, `@octokit/rest`).
    pub package: String,
    /// Whether the snippet was run against the example through the proxy.
    pub verified: bool,
}

#[derive(Subcommand, Debug)]
pub enum ExamplesCommand {
    /// List the catalog as JSON.
    List {
        /// Only examples of this protocol (openapi, graphql, grpc, asyncapi, soap).
        #[arg(long)]
        protocol: Option<String>,
        /// Only examples in this category (payments, messaging, ...).
        #[arg(long)]
        category: Option<String>,
    },
    /// Print one example's catalog entry, config and README as JSON.
    Show { name: String },
    /// Write an example's config, contracts and README into a directory.
    Init(InitArgs),
}

#[derive(Args, Debug, Clone)]
pub struct InitArgs {
    /// The example's name (`mockingbird examples list`).
    pub name: String,
    /// Target directory, created if missing.
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
    /// Overwrite files that already exist.
    #[arg(long)]
    pub force: bool,
    /// Also download the API's complete upstream contract (pinned URL,
    /// SHA-256 checked) and point the config at it.
    #[arg(long)]
    pub full: bool,
}

impl Example {
    pub fn meta(&self) -> anyhow::Result<Meta> {
        serde_json::from_str(self.meta)
            .with_context(|| format!("examples/apis/{}/example.json is invalid", self.name))
    }

    fn file(&self, name: &str) -> Option<&'static str> {
        self.files.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
    }
}

/// The example called `name`.
pub fn find(name: &str) -> anyhow::Result<&'static Example> {
    EXAMPLES.iter().find(|e| e.name == name).ok_or_else(|| {
        let names: Vec<&str> = EXAMPLES.iter().map(|e| e.name).collect();
        anyhow!(
            "no example called {name:?}; the catalog has: {}",
            names.join(", ")
        )
    })
}

/// Runs an `examples` subcommand and returns its JSON output.
pub async fn run(cmd: ExamplesCommand) -> anyhow::Result<Value> {
    match cmd {
        ExamplesCommand::List { protocol, category } => {
            list(protocol.as_deref(), category.as_deref())
        }
        ExamplesCommand::Show { name } => show(&name),
        ExamplesCommand::Init(args) => init(&args).await,
    }
}

fn list(protocol: Option<&str>, category: Option<&str>) -> anyhow::Result<Value> {
    let mut out = Vec::new();
    for example in EXAMPLES {
        let meta = example.meta()?;
        if protocol.is_some_and(|p| !p.eq_ignore_ascii_case(&meta.protocol))
            || category.is_some_and(|c| !c.eq_ignore_ascii_case(&meta.category))
        {
            continue;
        }
        out.push(json!({
            "name": meta.name,
            "title": meta.title,
            "category": meta.category,
            "protocol": meta.protocol,
            "summary": meta.summary,
            "hosts": meta.hosts,
            "full_spec": meta.upstream.as_ref().is_some_and(|u| u.url.is_some() && u.replaces.is_some()),
        }));
    }
    Ok(Value::Array(out))
}

fn show(name: &str) -> anyhow::Result<Value> {
    let example = find(name)?;
    let meta = example.meta()?;
    Ok(json!({
        "example": meta,
        "files": example.files.iter().map(|(n, t)| json!({"name": n, "bytes": t.len()})).collect::<Vec<_>>(),
        "config": example.file("mockingbird.yaml"),
        "readme": example.file("README.md"),
    }))
}

async fn init(args: &InitArgs) -> anyhow::Result<Value> {
    let example = find(&args.name)?;
    let meta = example.meta()?;
    let upstream = if args.full {
        let upstream = meta
            .upstream
            .as_ref()
            .filter(|u| u.url.is_some() && u.replaces.is_some())
            .ok_or_else(|| {
                anyhow!(
                    "example {:?} has no downloadable upstream contract; it runs on its embedded contract",
                    example.name
                )
            })?;
        Some(upstream)
    } else {
        None
    };
    let full_name = upstream.and_then(|u| u.url.as_deref()).map(full_file_name);

    let mut planned: Vec<&str> = example.files.iter().map(|(n, _)| *n).collect();
    if let Some(name) = &full_name {
        planned.push(name);
    }
    if !args.force {
        let existing: Vec<String> = planned
            .iter()
            .filter(|n| args.dir.join(n).exists())
            .map(|n| args.dir.join(n).display().to_string())
            .collect();
        if !existing.is_empty() {
            bail!(
                "these files exist already (pass --force to overwrite): {}",
                existing.join(", ")
            );
        }
    }

    // Download first, so a failed download writes nothing.
    let full = match (upstream, &full_name) {
        (Some(u), Some(name)) => Some((name.clone(), download(u).await?, u)),
        _ => None,
    };

    std::fs::create_dir_all(&args.dir)
        .with_context(|| format!("cannot create {}", args.dir.display()))?;
    let mut written = Vec::new();
    let mut dropped = Vec::new();
    for (name, text) in example.files {
        let text = match (&full, *name) {
            (Some((full_name, bytes, u)), "mockingbird.yaml") => {
                let replaces = u.replaces.as_deref().unwrap_or_default();
                let (config, gone) = drop_unanswerable_scenarios(text, replaces, bytes)?;
                dropped = gone;
                point_at_full(&config, replaces, full_name)?
            }
            _ => (*text).to_string(),
        };
        write(&args.dir, name, text.as_bytes())?;
        written.push((*name).to_string());
    }
    if let Some((name, bytes, _)) = &full {
        write(&args.dir, name, bytes)?;
        written.push(name.clone());
    }

    let config = args.dir.join("mockingbird.yaml");
    Ok(json!({
        "example": meta.name,
        "dir": args.dir.display().to_string(),
        "files": written,
        "contract": full.as_ref().map(|(n, _, _)| n.clone()).or_else(|| meta.contracts.first().cloned()),
        "dropped_scenarios": dropped,
        "next": [
            format!("mockingbird serve -c {} --offline", config.display()),
            format!("see {} for a curl and an SDK snippet", args.dir.join("README.md").display()),
        ],
    }))
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let path = dir.join(name);
    std::fs::write(&path, bytes).with_context(|| format!("cannot write {}", path.display()))
}

/// The file an upstream contract is saved as: the URL's last path segment.
fn full_file_name(url: &str) -> String {
    let name = url
        .split(['?', '#'])
        .next()
        .and_then(|u| u.rsplit('/').next())
        .filter(|s| !s.is_empty() && !s.starts_with('.'))
        .unwrap_or("upstream-contract");
    format!("upstream.{name}")
}

/// `config` with its `contract: <from>` lines pointing at `to`.
fn point_at_full(config: &str, from: &str, to: &str) -> anyhow::Result<String> {
    let mut replaced = false;
    let mut out = String::with_capacity(config.len());
    for line in config.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("contract:")
            && rest.trim() == from
        {
            let indent = &line[..line.len() - trimmed.len()];
            let eol = if line.ends_with('\n') { "\n" } else { "" };
            out.push_str(&format!("{indent}contract: {to}{eol}"));
            replaced = true;
        } else {
            out.push_str(line);
        }
    }
    if !replaced {
        bail!("the example's mockingbird.yaml has no `contract: {from}` line to replace");
    }
    Ok(out)
}

/// `config` without the scenarios of mocks serving `replaces` that the
/// upstream contract (`upstream`) could never answer: an operation it lacks,
/// or a status none of the scenario's operations declares. The hand-written
/// contracts declare every status their scenarios use; upstream specs often
/// do not (Twilio's CreateMessage declares only 201). Returns the config and
/// what was dropped, with the reason.
fn drop_unanswerable_scenarios(
    config: &str,
    replaces: &str,
    upstream: &[u8],
) -> anyhow::Result<(String, Vec<Value>)> {
    let parsed: crate::file_config::FileConfig = serde_saphyr::from_str(config)
        .map_err(|e| anyhow!("the example's mockingbird.yaml: {e}"))?;
    let (bundle, _) = mb_import::compile_with_warnings(upstream)
        .map_err(|e| anyhow!("the upstream contract does not compile: {e}"))?;
    let mut drop: Vec<(String, String)> = Vec::new();
    let mut report = Vec::new();
    for (mock, cfg) in &parsed.mocks {
        if cfg.contract != Path::new(replaces) {
            continue;
        }
        for (name, scenario) in &cfg.scenarios {
            let ids = scenario.operation_ids();
            let why = if let Some(op) = ids.iter().find(|id| bundle.operation(id).is_none()) {
                Some(format!("operation {op:?} is not in the upstream contract"))
            } else {
                scenario
                    .status
                    .and_then(|status| mb_admin::undeclared_scenario_status(&bundle, &ids, status))
                    .or_else(|| {
                        scenario.set.as_ref().and_then(|set| {
                            mb_admin::invalid_scenario_set(&bundle, &ids, scenario.status, set)
                        })
                    })
            };
            if let Some(why) = why {
                report.push(json!({"mock": mock, "scenario": name, "reason": why}));
                drop.push((mock.clone(), name.clone()));
            }
        }
    }
    if drop.is_empty() {
        return Ok((config.to_string(), report));
    }
    let emptied: Vec<&str> = parsed
        .mocks
        .iter()
        .filter(|(mock, cfg)| {
            !cfg.scenarios.is_empty()
                && cfg
                    .scenarios
                    .keys()
                    .all(|s| drop.iter().any(|(m, n)| m == *mock && n == s))
        })
        .map(|(mock, _)| mock.as_str())
        .collect();
    Ok((remove_scenarios(config, &drop, &emptied), report))
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The key of a `key:` mapping line.
fn key_of(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.starts_with('#') {
        return None;
    }
    let (key, _) = trimmed.split_once(':')?;
    Some(key.trim().trim_matches(['"', '\'']))
}

/// `config` with the scenario blocks in `drop` (`(mock, scenario)`) removed,
/// the whole `scenarios:` block of the mocks in `emptied`, and any
/// `default_scenario:` naming a removed scenario. Works on the text so the
/// comments of the rest survive.
fn remove_scenarios(config: &str, drop: &[(String, String)], emptied: &[&str]) -> String {
    let mut out = String::with_capacity(config.len());
    let mut in_mocks = false;
    let mut mock_indent: Option<usize> = None;
    let mut mock = String::new();
    let mut scenarios_indent: Option<usize> = None;
    let mut scenario_indent: Option<usize> = None;
    // Lines indented deeper than this are skipped.
    let mut skip_deeper_than: Option<usize> = None;
    for line in config.split_inclusive('\n') {
        let blank = line.trim().is_empty() || line.trim_start().starts_with('#');
        let indent = indent_of(line);
        if let Some(limit) = skip_deeper_than {
            if line.trim().is_empty() || indent > limit {
                continue;
            }
            skip_deeper_than = None;
        }
        if blank {
            out.push_str(line);
            continue;
        }
        if indent == 0 {
            in_mocks = key_of(line) == Some("mocks");
            mock_indent = None;
            scenarios_indent = None;
            out.push_str(line);
            continue;
        }
        if in_mocks && mock_indent.is_none_or(|m| indent <= m) {
            mock_indent = Some(indent);
            mock = key_of(line).unwrap_or_default().to_string();
            scenarios_indent = None;
            out.push_str(line);
            continue;
        }
        if let Some(si) = scenarios_indent {
            if indent <= si {
                scenarios_indent = None;
            } else {
                let si2 = *scenario_indent.get_or_insert(indent);
                if indent == si2
                    && let Some(name) = key_of(line)
                    && drop.iter().any(|(m, n)| *m == mock && n == name)
                {
                    skip_deeper_than = Some(indent);
                    continue;
                }
                out.push_str(line);
                continue;
            }
        }
        if in_mocks && key_of(line) == Some("scenarios") {
            if emptied.contains(&mock.as_str()) {
                skip_deeper_than = Some(indent);
                continue;
            }
            scenarios_indent = Some(indent);
            scenario_indent = None;
        }
        if in_mocks
            && key_of(line) == Some("default_scenario")
            && let Some((_, value)) = line.split_once(':')
            && drop
                .iter()
                .any(|(m, n)| *m == mock && n == value.trim().trim_matches(['"', '\'']))
        {
            continue;
        }
        out.push_str(line);
    }
    out
}

async fn download(upstream: &Upstream) -> anyhow::Result<Vec<u8>> {
    let url = upstream
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("the upstream contract has no URL"))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .user_agent(concat!("mockingbird/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot download {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("downloading {url} answered HTTP {status}");
    }
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("cannot download {url}"))?
        .to_vec();
    if let Some(expected) = &upstream.sha256 {
        let actual = hex(&Sha256::digest(&bytes));
        if !actual.eq_ignore_ascii_case(expected) {
            bail!("{url} has SHA-256 {actual}, expected {expected}; refusing to use it");
        }
    }
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_parses_and_names_its_directory() {
        for example in EXAMPLES {
            let meta = example.meta().unwrap();
            assert_eq!(meta.name, example.name);
            for contract in &meta.contracts {
                assert!(
                    example.file(contract).is_some(),
                    "{}: contract {contract} is not embedded",
                    example.name
                );
            }
            if let Some(replaces) = meta.upstream.as_ref().and_then(|u| u.replaces.as_ref()) {
                assert!(
                    meta.contracts.contains(replaces),
                    "{}: replaces {replaces}",
                    example.name
                );
            }
        }
    }

    /// Any examples can be combined in one config: no two mocks of the
    /// catalog claim the same route prefix on the same host (GitHub's REST
    /// and GraphQL examples share api.github.com).
    #[test]
    fn examples_combine_without_route_collisions() {
        let mut mocks: Vec<(String, String, Vec<String>)> = Vec::new();
        for example in EXAMPLES {
            let text = example.file("mockingbird.yaml").unwrap();
            let cfg: crate::file_config::FileConfig = serde_saphyr::from_str(text).unwrap();
            for (name, mock) in &cfg.mocks {
                let prefix = mock
                    .route_prefix
                    .clone()
                    .unwrap_or_else(|| format!("/{name}"));
                for (other, other_prefix, other_hosts) in &mocks {
                    assert!(
                        !mb_store::routes_collide(&prefix, &mock.hosts, other_prefix, other_hosts),
                        "{}/{name} and {other} share route prefix {prefix}",
                        example.name
                    );
                }
                mocks.push((
                    format!("{}/{name}", example.name),
                    prefix,
                    mock.hosts.clone(),
                ));
            }
        }
    }

    #[test]
    fn full_file_names_come_from_the_url() {
        assert_eq!(
            full_file_name(
                "https://raw.githubusercontent.com/stripe/openapi/abc/openapi/spec3.json"
            ),
            "upstream.spec3.json"
        );
        assert_eq!(
            full_file_name("https://example.com/"),
            "upstream.upstream-contract"
        );
    }

    #[test]
    fn point_at_full_rewrites_only_the_matching_contract() {
        let config = "mocks:\n  a:\n    contract: a.yaml\n  b:\n    contract: b.yaml\n";
        let out = point_at_full(config, "a.yaml", "upstream.a.json").unwrap();
        assert_eq!(
            out,
            "mocks:\n  a:\n    contract: upstream.a.json\n  b:\n    contract: b.yaml\n"
        );
        assert!(point_at_full(config, "c.yaml", "x").is_err());
    }

    #[test]
    fn remove_scenarios_drops_blocks_and_keeps_the_rest() {
        let config = "\
proxy:
  enabled: true
mocks:
  a:
    contract: a.yaml
    default_scenario: declined
    scenarios:
      declined:
        status: 402
        set:
          error: {code: card_declined}
      # a comment
      outage:
        status: 503
    persona: x
  b:
    contract: b.yaml
    scenarios:
      declined:
        status: 400
";
        let drop = vec![
            ("a".to_string(), "declined".to_string()),
            ("b".to_string(), "declined".to_string()),
        ];
        let out = remove_scenarios(config, &drop, &["b"]);
        assert_eq!(
            out,
            "\
proxy:
  enabled: true
mocks:
  a:
    contract: a.yaml
    scenarios:
      # a comment
      outage:
        status: 503
    persona: x
  b:
    contract: b.yaml
"
        );
    }
}
