//! Admin API client subcommands: `mocks`, `import`, `sessions`,
//! `export-fixtures`, `seed-fixtures`, `channels`, `scenarios`, `profiles`,
//! `brokers` and `webhooks` (whose `sign` runs locally).
//!
//! Every mock- and workspace-scoped call goes to
//! `/api/v1/workspaces/<ws>/…` of the `--workspace` (`MB_WORKSPACE`)
//! workspace; `brokers list` stays global.
//!
//! Successful responses with a body are returned as JSON for the caller to
//! print on stdout; a non-2xx response becomes [`ClientError::Api`], whose
//! problem details the caller prints on stderr.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use clap::{Args, Subcommand};
use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};

/// Default `--admin-url` (and `MB_ADMIN_URL`).
pub const DEFAULT_ADMIN_URL: &str = "http://127.0.0.1:9090";

/// The client subcommands, flattened into the top-level command.
#[derive(Subcommand, Debug)]
pub enum ClientCommand {
    /// List, inspect, create and delete mocks.
    #[command(subcommand)]
    Mocks(MocksCommand),
    /// Import a contract, API documentation, a HAR recording or a Postman
    /// collection as a new version of a mock, creating the mock
    /// (`source = api`) if it does not exist. Documentation, HAR and
    /// Postman imports produce a draft version for review.
    Import(ImportArgs),
    /// List and reset the sessions of a mock.
    #[command(subcommand)]
    Sessions(SessionsCommand),
    /// Print a session's stored resources as JSON.
    ExportFixtures(SessionArgs),
    /// Replace a session's contents with the resources in a JSON file.
    SeedFixtures(SeedArgs),
    /// List, inspect, configure and publish on a mock's event channels.
    #[command(subcommand)]
    Channels(ChannelsCommand),
    /// List, inspect, write and delete a mock's scenarios.
    #[command(subcommand)]
    Scenarios(ScenariosCommand),
    /// List, inspect, write and delete the workspace's LLM profiles.
    #[command(subcommand)]
    Profiles(ProfilesCommand),
    /// List the configured message brokers.
    #[command(subcommand)]
    Brokers(BrokersCommand),
    /// Sign webhook bodies and list a mock's webhook deliveries.
    #[command(subcommand)]
    Webhooks(WebhooksCommand),
}

#[derive(Subcommand, Debug)]
pub enum WebhooksCommand {
    /// Print the signature header a mock would send for a body (for tests).
    /// Runs locally; the secret is never printed.
    Sign {
        /// env:NAME, file:/path or vault:path#key.
        #[arg(long)]
        secret_ref: String,
        /// The exact body bytes (`-` reads stdin).
        #[arg(long)]
        body: PathBuf,
        #[arg(long, value_enum, default_value = "hmac-sha256")]
        signing: SignArg,
        /// Unix seconds for Stripe's `t` [default: now].
        #[arg(long)]
        timestamp: Option<i64>,
    },
    /// List a mock's recent webhook deliveries, newest first.
    Deliveries {
        mock: String,
        /// 1-200.
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
}

/// A signing scheme `webhooks sign` produces.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignArg {
    Stripe,
    #[value(name = "hmac-sha256")]
    HmacSha256,
}

#[derive(Subcommand, Debug)]
pub enum ScenariosCommand {
    /// List a mock's scenarios.
    List { mock: String },
    /// Show one scenario.
    Get { mock: String, name: String },
    /// Create or replace a scenario (`source = api`).
    Put {
        mock: String,
        name: String,
        /// The scenario as JSON (`-` reads stdin); absent fields are cleared.
        #[arg(long)]
        file: PathBuf,
    },
    /// Delete a scenario.
    Delete { mock: String, name: String },
}

#[derive(Subcommand, Debug)]
pub enum ProfilesCommand {
    /// List the workspace's LLM profiles.
    List,
    /// Show one LLM profile.
    Get { name: String },
    /// Create or replace an LLM profile (`source = api`).
    Put {
        name: String,
        /// The profile as JSON (`-` reads stdin).
        #[arg(long)]
        file: PathBuf,
    },
    /// Delete an LLM profile.
    Delete { name: String },
}

// The doc comments are clap help text, not Markdown.
#[allow(rustdoc::invalid_html_tags)]
#[derive(Subcommand, Debug)]
pub enum ChannelsCommand {
    /// List a mock's channels with their effective settings.
    List { mock: String },
    /// Show one channel with its schemas.
    Get { mock: String, channel: String },
    /// Set a channel's settings (`source = api`).
    Set {
        mock: String,
        channel: String,
        /// Disable the channel.
        #[arg(long)]
        disable: bool,
        /// every:<duration> | cron:<expr> | state:<resource_type>[:upserted|deleted|any] | manual (repeatable)
        #[arg(long = "trigger")]
        triggers: Vec<String>,
        /// Use the contract's triggers (conflicts with --trigger)
        #[arg(long, conflicts_with = "triggers")]
        contract_triggers: bool,
    },
    /// Generate and publish messages on a channel the mock publishes.
    Publish {
        mock: String,
        channel: String,
        /// Messages to publish (1-100).
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// A JSON object handed to the message prompt.
        #[arg(long)]
        context: Option<String>,
    },
    /// Show the mock's broker; with a name, set it; with `-`, clear it
    /// (pick automatically).
    Broker { mock: String, name: Option<String> },
}

#[derive(Subcommand, Debug)]
pub enum BrokersCommand {
    /// List the configured brokers.
    List,
}

// The doc comments are clap help text, not Markdown.
#[allow(rustdoc::invalid_html_tags)]
#[derive(Subcommand, Debug)]
pub enum MocksCommand {
    /// List every mock.
    List,
    /// Show one mock.
    Get { name: String },
    /// Create a mock.
    Create {
        name: String,
        /// Route prefix [default: /<name>].
        #[arg(long)]
        prefix: Option<String>,
        /// Persona (style instructions) for generated responses.
        #[arg(long)]
        persona: Option<String>,
        /// LLM profile [default: the default profile].
        #[arg(long)]
        profile: Option<String>,
    },
    /// Delete a mock with its contracts, scenarios and request log.
    Delete { name: String },
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    /// A contract file (OpenAPI, Swagger, AsyncAPI, GraphQL, .proto,
    /// descriptor set, WSDL, zip or tar.gz) or a directory (uploaded as a tar.gz); or API
    /// documentation (Markdown, HTML, PDF, text), a HAR recording or a
    /// Postman collection as a file or an http(s) URL, imported as a draft.
    /// A directory holding only .md, .markdown, .html, .htm and .txt files
    /// is imported as documentation, one source per file.
    #[arg(value_name = "FILE|URL")]
    pub source: String,
    /// More http(s) URLs imported into the same draft.
    #[arg(value_name = "URL")]
    pub more_urls: Vec<String>,
    /// The mock to import into.
    #[arg(long = "as", value_name = "MOCK")]
    pub mock: String,
    /// Activate the new version (the first contract version is always
    /// activated). For a draft, --approve already activates, and --activate
    /// is accepted only with --approve.
    #[arg(long)]
    pub activate: bool,
    /// Approve the draft when the import reported no warnings; the approved
    /// version is activated unless --no-activate.
    #[arg(long)]
    pub approve: bool,
    /// With --approve: approve without activating.
    #[arg(long, conflicts_with = "activate")]
    pub no_activate: bool,
    /// Merge into the mock's draft or active contract, or replace it
    /// [default: merge when one exists].
    #[arg(long, value_enum)]
    pub mode: Option<ImportMode>,
    /// How long to wait for a draft import to finish.
    #[arg(long, default_value = "10m")]
    pub timeout: humantime::Duration,
}

/// How a draft import treats the mock's existing contract.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportMode {
    /// Add new operations, replace those with the same method and path.
    Merge,
    /// The draft holds only what this import found.
    Replace,
}

impl ImportMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Replace => "replace",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum SessionsCommand {
    /// List a mock's live sessions, most recently touched first.
    List { mock: String },
    /// Delete a session's state.
    Reset { mock: String, session: String },
}

#[derive(Args, Debug)]
pub struct SessionArgs {
    #[arg(long)]
    pub mock: String,
    #[arg(long)]
    pub session: String,
}

#[derive(Args, Debug)]
pub struct SeedArgs {
    #[command(flatten)]
    pub target: SessionArgs,
    /// A JSON array of `{resource_type, id, data}`, as `export-fixtures` prints.
    pub file: PathBuf,
}

/// Why a client command failed.
#[derive(Debug)]
pub enum ClientError {
    /// The Admin API answered with a non-2xx status; `body` is its problem
    /// details document (or whatever it sent).
    Api { status: StatusCode, body: String },
    /// The command line is inconsistent (exit 2).
    Usage(String),
    /// The command ran but did not do everything asked: `output` is
    /// printed on stdout, `message` on stderr, and the exit code is 1.
    Incomplete { output: Value, message: String },
    /// The request could not be made, or a local input was invalid.
    Other(anyhow::Error),
}

impl From<anyhow::Error> for ClientError {
    fn from(e: anyhow::Error) -> Self {
        ClientError::Other(e)
    }
}

impl From<reqwest::Error> for ClientError {
    fn from(e: reqwest::Error) -> Self {
        ClientError::Other(anyhow!(e))
    }
}

/// Runs `cmd` against workspace `workspace` of the Admin API at
/// `admin_url`, sending `Authorization: Bearer <token>` when `token` is
/// set. Returns the JSON to print, or `None` when the API answered without
/// a body.
pub async fn run(
    admin_url: &str,
    token: Option<&str>,
    workspace: &str,
    cmd: ClientCommand,
) -> Result<Option<Value>, ClientError> {
    if let ClientCommand::Webhooks(WebhooksCommand::Sign {
        secret_ref,
        body,
        signing,
        timestamp,
    }) = cmd
    {
        return sign_webhook(&secret_ref, &body, signing, timestamp)
            .await
            .map(Some)
            .map_err(ClientError::Other);
    }
    let api = AdminClient::new(admin_url, token, workspace)?;
    match cmd {
        ClientCommand::Mocks(MocksCommand::List) => api.send(Method::GET, &["mocks"], None).await,
        ClientCommand::Mocks(MocksCommand::Get { name }) => {
            api.send(Method::GET, &["mocks", &name], None).await
        }
        ClientCommand::Mocks(MocksCommand::Create {
            name,
            prefix,
            persona,
            profile,
        }) => {
            let mut body = json!({ "name": name });
            for (key, value) in [
                ("route_prefix", prefix),
                ("persona", persona),
                ("llm_profile", profile),
            ] {
                if let Some(v) = value {
                    body[key] = Value::String(v);
                }
            }
            api.send(Method::POST, &["mocks"], Some(Body::Json(body)))
                .await
        }
        ClientCommand::Mocks(MocksCommand::Delete { name }) => {
            api.send(Method::DELETE, &["mocks", &name], None).await
        }
        ClientCommand::Import(args) => import(&api, args).await,
        ClientCommand::Sessions(SessionsCommand::List { mock }) => {
            api.send(Method::GET, &["mocks", &mock, "sessions"], None)
                .await
        }
        ClientCommand::Sessions(SessionsCommand::Reset { mock, session }) => {
            api.send(
                Method::DELETE,
                &["mocks", &mock, "sessions", &session],
                None,
            )
            .await
        }
        ClientCommand::ExportFixtures(SessionArgs { mock, session }) => {
            api.send(Method::GET, &["mocks", &mock, "sessions", &session], None)
                .await
        }
        ClientCommand::Channels(cmd) => channels(&api, cmd).await,
        ClientCommand::Scenarios(cmd) => scenarios(&api, cmd).await,
        ClientCommand::Profiles(cmd) => profiles(&api, cmd).await,
        ClientCommand::Brokers(BrokersCommand::List) => {
            api.request(Method::GET, api.url(&["brokers"]), None).await
        }
        ClientCommand::Webhooks(WebhooksCommand::Deliveries { mock, limit }) => {
            let mut url = api.ws_url(&["mocks", &mock, "webhooks", "deliveries"]);
            url.query_pairs_mut()
                .append_pair("limit", &limit.to_string());
            api.request(Method::GET, url, None).await
        }
        ClientCommand::Webhooks(WebhooksCommand::Sign { .. }) => {
            Err(ClientError::Other(anyhow!("webhooks sign runs locally")))
        }
        ClientCommand::SeedFixtures(SeedArgs { target, file }) => {
            let fixtures = read_json(&file)?;
            api.send(
                Method::PUT,
                &["mocks", &target.mock, "sessions", &target.session],
                Some(Body::Json(fixtures)),
            )
            .await
        }
    }
}

/// Message of several sources that are not all URLs.
const SEVERAL_SOURCES_NEED_URLS: &str = "several sources must all be http(s) URLs";

/// Message of `--activate` given for a draft import without `--approve`.
const ACTIVATE_NEEDS_APPROVE: &str =
    "--activate needs --approve for document, HAR and Postman imports";

/// How often a draft import's job is polled.
const IMPORT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

async fn scenarios(api: &AdminClient, cmd: ScenariosCommand) -> Result<Option<Value>, ClientError> {
    match cmd {
        ScenariosCommand::List { mock } => {
            api.send(Method::GET, &["mocks", &mock, "scenarios"], None)
                .await
        }
        // There is no single-scenario GET: pick it out of the list.
        ScenariosCommand::Get { mock, name } => {
            let list = api
                .send(Method::GET, &["mocks", &mock, "scenarios"], None)
                .await?;
            list.as_ref()
                .and_then(Value::as_array)
                .and_then(|all| {
                    all.iter()
                        .find(|s| s.get("name").and_then(Value::as_str) == Some(name.as_str()))
                })
                .cloned()
                .map(Some)
                .ok_or_else(|| {
                    ClientError::Usage(format!("scenario {name} not found in mock {mock}"))
                })
        }
        ScenariosCommand::Put { mock, name, file } => {
            let body = read_json(&file)?;
            api.send(
                Method::PUT,
                &["mocks", &mock, "scenarios", &name],
                Some(Body::Json(body)),
            )
            .await
        }
        ScenariosCommand::Delete { mock, name } => {
            api.send(Method::DELETE, &["mocks", &mock, "scenarios", &name], None)
                .await
        }
    }
}

async fn profiles(api: &AdminClient, cmd: ProfilesCommand) -> Result<Option<Value>, ClientError> {
    match cmd {
        ProfilesCommand::List => api.send(Method::GET, &["llm-profiles"], None).await,
        ProfilesCommand::Get { name } => {
            api.send(Method::GET, &["llm-profiles", &name], None).await
        }
        ProfilesCommand::Put { name, file } => {
            let body = read_json(&file)?;
            api.send(
                Method::PUT,
                &["llm-profiles", &name],
                Some(Body::Json(body)),
            )
            .await
        }
        ProfilesCommand::Delete { name } => {
            api.send(Method::DELETE, &["llm-profiles", &name], None)
                .await
        }
    }
}

async fn channels(api: &AdminClient, cmd: ChannelsCommand) -> Result<Option<Value>, ClientError> {
    match cmd {
        ChannelsCommand::List { mock } => {
            api.send(Method::GET, &["mocks", &mock, "channels"], None)
                .await
        }
        ChannelsCommand::Get { mock, channel } => {
            api.send(Method::GET, &["mocks", &mock, "channels", &channel], None)
                .await
        }
        ChannelsCommand::Set {
            mock,
            channel,
            disable,
            triggers,
            contract_triggers,
        } => {
            let triggers = if contract_triggers || triggers.is_empty() {
                Value::Null
            } else {
                Value::Array(
                    triggers
                        .iter()
                        .map(|t| parse_trigger_spec(t))
                        .collect::<Result<_, _>>()
                        .map_err(ClientError::Usage)?,
                )
            };
            let body = json!({ "enabled": !disable, "triggers": triggers });
            api.send(
                Method::PUT,
                &["mocks", &mock, "channels", &channel],
                Some(Body::Json(body)),
            )
            .await
        }
        ChannelsCommand::Publish {
            mock,
            channel,
            count,
            context,
        } => {
            let context = match context {
                Some(text) => serde_json::from_str::<Value>(&text)
                    .map_err(|e| ClientError::Usage(format!("--context is not JSON: {e}")))?,
                None => Value::Null,
            };
            api.send(
                Method::POST,
                &["mocks", &mock, "channels", &channel, "publish"],
                Some(Body::Json(json!({ "count": count, "context": context }))),
            )
            .await
        }
        ChannelsCommand::Broker { mock, name: None } => {
            api.send(Method::GET, &["mocks", &mock, "event-settings"], None)
                .await
        }
        ChannelsCommand::Broker {
            mock,
            name: Some(name),
        } => {
            let broker = if name == "-" {
                Value::Null
            } else {
                Value::String(name)
            };
            api.send(
                Method::PUT,
                &["mocks", &mock, "event-settings"],
                Some(Body::Json(json!({ "broker": broker }))),
            )
            .await
        }
    }
}

/// Parses a `--trigger` spec into MCM trigger JSON:
/// `every:<humantime>` → `{"schedule":{"every_ms":n}}`,
/// `cron:<expr>` → `{"cron":{"expr":…}}`,
/// `state:<type>[:<kind>]` → `{"on_state_change":{"resource_type":…,"kind":…}}`
/// (kind `upserted`, `deleted` or `any`, the default) and
/// `manual` → `"manual"`.
pub fn parse_trigger_spec(spec: &str) -> Result<Value, String> {
    let parsed = match spec.split_once(':') {
        None if spec == "manual" => Some(Value::String("manual".into())),
        None => None,
        Some(("every", d)) => humantime::parse_duration(d.trim())
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok())
            .map(|ms| json!({ "schedule": { "every_ms": ms } })),
        Some(("cron", expr)) if !expr.trim().is_empty() => {
            Some(json!({ "cron": { "expr": expr } }))
        }
        Some(("state", rest)) => {
            let (resource_type, kind) = rest.split_once(':').unwrap_or((rest, "any"));
            let known_kind = matches!(kind, "upserted" | "deleted" | "any");
            (!resource_type.is_empty() && known_kind).then(
                || json!({ "on_state_change": { "resource_type": resource_type, "kind": kind } }),
            )
        }
        Some(_) => None,
    };
    parsed.ok_or_else(|| {
        format!(
            "unknown trigger spec \"{spec}\" (every:<duration>|cron:<expr>|state:<type>[:kind]|manual)"
        )
    })
}

async fn import(api: &AdminClient, args: ImportArgs) -> Result<Option<Value>, ClientError> {
    if !args.more_urls.is_empty() {
        let urls: Vec<&String> = std::iter::once(&args.source)
            .chain(&args.more_urls)
            .collect();
        if !urls.iter().all(|u| is_url(u)) {
            return Err(ClientError::Usage(SEVERAL_SOURCES_NEED_URLS.into()));
        }
        let body = json!({ "urls": urls });
        return smart_import(api, &args, body).await;
    }
    if is_url(&args.source) {
        let body = json!({ "url": args.source });
        return smart_import(api, &args, body).await;
    }
    let path = PathBuf::from(&args.source);
    if path.is_dir() {
        let documents =
            docs_folder(&path).with_context(|| format!("cannot read {}", path.display()))?;
        if let Some(documents) = documents {
            return smart_import(api, &args, json!({ "documents": documents })).await;
        }
    } else {
        let bytes = read(&path)?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string);
        if let Some(kind) = mb_import::smart::detect_kind(name.as_deref(), &bytes) {
            let body = document_body(kind, name, bytes)
                .with_context(|| format!("cannot import {}", path.display()))?;
            return smart_import(api, &args, body).await;
        }
    }
    upload_contract(api, &args, &path).await
}

/// Whether `source` is an http(s) URL rather than a path.
fn is_url(source: &str) -> bool {
    ["http://", "https://"].iter().any(|scheme| {
        source
            .get(..scheme.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(scheme))
    })
}

/// The `POST …/imports` body for a document of `kind`: base64 for a PDF,
/// text otherwise.
fn document_body(
    kind: mb_import::smart::ImportKind,
    filename: Option<String>,
    bytes: Vec<u8>,
) -> anyhow::Result<Value> {
    use base64::Engine as _;
    let mut body = json!({ "kind": kind.as_str() });
    if let Some(name) = filename {
        body["filename"] = Value::String(name);
    }
    if kind == mb_import::smart::ImportKind::Pdf {
        body["content_base64"] =
            Value::String(base64::engine::general_purpose::STANDARD.encode(&bytes));
    } else {
        let text = String::from_utf8(bytes)
            .map_err(|_| anyhow!("a {} document must be UTF-8 text", kind.as_str()))?;
        body["text"] = Value::String(text);
    }
    Ok(body)
}

/// Extensions of the files a documentation folder holds, with their kinds.
const DOC_EXTENSIONS: [(&str, &str); 5] = [
    ("md", "markdown"),
    ("markdown", "markdown"),
    ("html", "html"),
    ("htm", "html"),
    ("txt", "text"),
];

/// The documents of a folder holding only .md/.markdown/.html/.htm/.txt
/// files (recursive, hidden entries skipped, sorted by relative path), as
/// `{"filename", "kind", "text"}`; `None` when it holds anything else or
/// nothing.
fn docs_folder(dir: &Path) -> anyhow::Result<Option<Vec<Value>>> {
    let mut files: Vec<(String, &'static str, PathBuf)> = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Ok(None);
            };
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file() {
                return Ok(None);
            }
            let ext = name
                .rsplit_once('.')
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            let Some((_, kind)) = DOC_EXTENSIONS.iter().find(|(e, _)| *e == ext) else {
                return Ok(None);
            };
            let relative = path
                .strip_prefix(dir)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            files.push((relative, kind, path));
        }
    }
    if files.is_empty() {
        return Ok(None);
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut documents = Vec::with_capacity(files.len());
    for (filename, kind, path) in files {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("{filename} is not UTF-8 text"))?;
        documents.push(json!({ "filename": filename, "kind": kind, "text": text }));
    }
    Ok(Some(documents))
}

/// Uploads a contract file or directory to `…/contracts`.
async fn upload_contract(
    api: &AdminClient,
    args: &ImportArgs,
    path: &Path,
) -> Result<Option<Value>, ClientError> {
    let source =
        mb_import::read_source(path).with_context(|| format!("cannot read {}", path.display()))?;
    ensure_mock(api, &args.mock).await?;
    let mut url = api.ws_url(&["mocks", &args.mock, "contracts"]);
    if args.activate {
        url.set_query(Some("activate=true"));
    }
    let content_type = contract_media_type(path, &source);
    api.request(
        Method::POST,
        url,
        Some(Body::Bytes {
            bytes: source,
            content_type,
        }),
    )
    .await
}

/// Creates mock `name` (`source = api`) unless it exists.
async fn ensure_mock(api: &AdminClient, name: &str) -> Result<(), ClientError> {
    match api.send(Method::GET, &["mocks", name], None).await {
        Ok(_) => Ok(()),
        Err(ClientError::Api { status, .. }) if status == StatusCode::NOT_FOUND => {
            api.send(
                Method::POST,
                &["mocks"],
                Some(Body::Json(json!({ "name": name }))),
            )
            .await?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Starts an import job with `body` (and `--mode`), waits for its draft
/// and summarises it; with `--approve`, approves a draft without warnings,
/// which also activates it unless `--no-activate`.
async fn smart_import(
    api: &AdminClient,
    args: &ImportArgs,
    mut body: Value,
) -> Result<Option<Value>, ClientError> {
    if args.activate && !args.approve {
        return Err(ClientError::Usage(ACTIVATE_NEEDS_APPROVE.into()));
    }
    if let Some(mode) = args.mode {
        body["mode"] = Value::String(mode.as_str().to_string());
    }
    ensure_mock(api, &args.mock).await?;
    let job = api
        .send(
            Method::POST,
            &["mocks", &args.mock, "imports"],
            Some(Body::Json(body)),
        )
        .await?
        .ok_or_else(|| anyhow!("the Admin API sent no import job"))?;
    let id = job_field(&job, "id")?;
    let job = wait_for_job(api, &args.mock, &id, args.timeout).await?;
    let version = job
        .get("result_version")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("import job {id} finished without a draft version"))?;
    let version_segment = version.to_string();
    let draft = api
        .send(
            Method::GET,
            &["mocks", &args.mock, "contracts", &version_segment, "draft"],
            None,
        )
        .await?
        .ok_or_else(|| anyhow!("the Admin API sent no draft"))?;
    let operations: Vec<Value> = draft
        .get("operations")
        .and_then(Value::as_array)
        .map(|ops| {
            ops.iter()
                .map(|op| {
                    let method = op.get("method").and_then(Value::as_str).unwrap_or("");
                    let path = op.get("path").and_then(Value::as_str).unwrap_or("");
                    Value::String(format!("{method} {path}"))
                })
                .collect()
        })
        .unwrap_or_default();
    let warnings: Vec<Value> = draft
        .pointer("/meta/warnings")
        .and_then(Value::as_array)
        .map(|ws| {
            ws.iter()
                .filter_map(|w| w.get("message").cloned())
                .collect()
        })
        .unwrap_or_default();
    let mut output = json!({
        "job": id,
        "version": version,
        "status": draft.get("status").cloned().unwrap_or(Value::Null),
        "operations": operations,
        "warnings": warnings,
        "approved": false,
    });
    if !args.approve {
        return Ok(Some(output));
    }
    if !warnings.is_empty() {
        let n = warnings.len();
        let noun = if n == 1 { "warning" } else { "warnings" };
        return Err(ClientError::Incomplete {
            output,
            message: format!("not approved: the draft has {n} {noun}; review it in the console"),
        });
    }
    let summary = api
        .send(
            Method::POST,
            &[
                "mocks",
                &args.mock,
                "contracts",
                &version_segment,
                "approve",
            ],
            Some(Body::Json(json!({ "activate": !args.no_activate }))),
        )
        .await?;
    output["approved"] = Value::Bool(true);
    if let Some(status) = summary.as_ref().and_then(|s| s.get("status")) {
        output["status"] = status.clone();
    }
    Ok(Some(output))
}

/// Polls import job `id` until it is `done`; `failed`, `canceled` or
/// still running after `timeout` is an error.
async fn wait_for_job(
    api: &AdminClient,
    mock: &str,
    id: &str,
    timeout: humantime::Duration,
) -> Result<Value, ClientError> {
    let deadline = tokio::time::Instant::now() + *timeout;
    loop {
        let job = api
            .send(Method::GET, &["mocks", mock, "imports", id], None)
            .await?
            .ok_or_else(|| anyhow!("the Admin API sent no import job"))?;
        match job_field(&job, "status")?.as_str() {
            "done" => return Ok(job),
            status @ ("failed" | "canceled") => {
                let error = job
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given");
                return Err(anyhow!("import {status}: {error}").into());
            }
            _ => {}
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(anyhow!(
                "import job {id} did not finish within {timeout}; it keeps running on the server"
            )
            .into());
        }
        tokio::time::sleep(IMPORT_POLL_INTERVAL.min(deadline - now)).await;
    }
}

/// A string field of an import job.
fn job_field(job: &Value, field: &str) -> anyhow::Result<String> {
    job.get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the import job has no {field}"))
}

/// The upload media type for a contract, by extension: `.graphql`,
/// `.graphqls`, `.gql` (GraphQL), `.proto`, `.pb`/`.desc` (descriptor set),
/// `.wsdl`/`.xsd`/`.xml`, `.zip`, `.tgz`/`.tar.gz` and directories (packed
/// as tar.gz), `.asyncapi.yaml`/`.asyncapi.yml`/`.asyncapi.json`
/// (AsyncAPI); else `.json`/`.yaml`/`.yml`, else JSON when the document
/// starts with `{`, else YAML.
pub(crate) fn contract_media_type(path: &Path, source: &[u8]) -> &'static str {
    if path.is_dir() {
        return "application/gzip";
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if name.ends_with(".tar.gz") {
        return "application/gzip";
    }
    if name.ends_with(".asyncapi.yaml") || name.ends_with(".asyncapi.yml") {
        return "application/vnd.aai.asyncapi+yaml";
    }
    if name.ends_with(".asyncapi.json") {
        return "application/vnd.aai.asyncapi+json";
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("graphql" | "graphqls" | "gql") => "application/graphql",
        Some("proto") => "text/x-protobuf",
        Some("pb" | "desc") => "application/x-protobuf",
        Some("wsdl" | "xsd" | "xml") => "application/xml",
        Some("zip") => "application/zip",
        Some("tgz") => "application/gzip",
        Some("json") => "application/json",
        Some("yaml" | "yml") => "application/yaml",
        _ if source.trim_ascii_start().starts_with(b"{") => "application/json",
        _ => "application/yaml",
    }
}

fn read(path: &Path) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))
}

/// `{"header", "value"}` of the signature a mock sends for the bytes of
/// `body` (`-` reads stdin). Errors name the secret reference, never the
/// value.
async fn sign_webhook(
    secret_ref: &str,
    body: &Path,
    signing: SignArg,
    timestamp: Option<i64>,
) -> anyhow::Result<Value> {
    use mb_ext::SecretStore;
    use mb_protocols::webhooks::sign::{Signing, signature_header};

    let bytes = if body == Path::new("-") {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes)
            .context("cannot read stdin")?;
        bytes
    } else {
        read(body)?
    };
    let secret = mb_ext::DefaultSecrets::from_env()
        .resolve(secret_ref)
        .await
        .map_err(|_| anyhow!("secret reference {secret_ref} could not be resolved"))?;
    let timestamp = match timestamp {
        Some(t) => t,
        None => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            .context("the system clock is before 1970")?,
    };
    let signing = match signing {
        SignArg::Stripe => Signing::Stripe,
        SignArg::HmacSha256 => Signing::HmacSha256,
    };
    let (header, value) = signature_header(signing, secret.as_bytes(), timestamp, &bytes)
        .ok_or_else(|| anyhow!("no signature for this scheme"))?;
    Ok(json!({"header": header, "value": value}))
}

/// Reads a JSON document from `path`, or from stdin when `path` is `-`.
fn read_json(path: &Path) -> anyhow::Result<Value> {
    let (text, label) = if path == Path::new("-") {
        let mut text = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut text)
            .context("cannot read stdin")?;
        (text, "stdin".to_string())
    } else {
        (read(path)?, path.display().to_string())
    };
    serde_json::from_slice(&text).with_context(|| format!("{label} is not JSON"))
}

/// Sends one Admin API request: `segments` under the `workspace`
/// workspace (`/api/v1/workspaces/<ws>/…`), or under `/api/v1/` when
/// `workspace` is `None`, with `query` appended. Used by the MCP server.
pub async fn api(
    admin_url: &str,
    token: Option<&str>,
    workspace: Option<&str>,
    method: Method,
    segments: &[&str],
    query: &[(&str, String)],
    body: Option<Value>,
) -> Result<Option<Value>, ClientError> {
    let api = AdminClient::new(admin_url, token, workspace.unwrap_or_default())?;
    let mut url = match workspace {
        Some(_) => api.ws_url(segments),
        None => api.url(segments),
    };
    if !query.is_empty() {
        url.query_pairs_mut()
            .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
    }
    api.request(method, url, body.map(Body::Json)).await
}

/// The largest non-JSON body [`api_path`] returns.
const MAX_RAW_BODY: usize = 1 << 20;

/// Sends `method` to `<admin_url>/api/v1<path>` as given (`path` starts
/// with `/` and may carry a query). Used by the MCP server's generic
/// `admin_api` tool. A JSON (or empty) answer is returned as such; any
/// other body of up to 1 MiB as `{content_type, text}` when it is UTF-8,
/// else `{content_type, bytes, base64}`. An event stream is refused before
/// it is read (it would never end).
pub async fn api_path(
    admin_url: &str,
    token: Option<&str>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Option<Value>, ClientError> {
    if !path.starts_with('/') || path.contains("..") {
        return Err(ClientError::Usage(format!(
            "path {path:?} must start with / and stay under /api/v1"
        )));
    }
    let api = AdminClient::new(admin_url, token, "")?;
    let base = api.base.as_str().trim_end_matches('/');
    let url = Url::parse(&format!("{base}/api/v1{path}"))
        .map_err(|e| ClientError::Usage(format!("invalid path {path:?}: {e}")))?;
    let resp = api.send_request(method, url, body.map(Body::Json)).await?;
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let status = resp.status();
    if !status.is_success() || essence.is_empty() || is_json(&essence) {
        return AdminClient::parse(resp).await;
    }
    if essence == "text/event-stream" {
        return Err(ClientError::Usage(format!(
            "{path} answers with an event stream, which this call cannot read to its end; read recorded requests with list_requests or get_request instead"
        )));
    }
    let bytes = resp.bytes().await.map_err(anyhow::Error::from)?;
    if bytes.len() > MAX_RAW_BODY {
        return Err(ClientError::Usage(format!(
            "{path} answered {} bytes of {content_type}, more than the {MAX_RAW_BODY} this call returns",
            bytes.len()
        )));
    }
    Ok(Some(match std::str::from_utf8(&bytes) {
        Ok(text) => serde_json::json!({ "content_type": content_type, "text": text }),
        Err(_) => {
            use base64::Engine as _;
            serde_json::json!({
                "content_type": content_type,
                "bytes": bytes.len(),
                "base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            })
        }
    }))
}

/// Whether the media type `essence` (lowercase, without parameters) is
/// JSON: `application/json` or a `+json` suffix type.
fn is_json(essence: &str) -> bool {
    essence == "application/json" || essence.ends_with("+json")
}

enum Body {
    Json(Value),
    Bytes {
        bytes: Vec<u8>,
        content_type: &'static str,
    },
}

struct AdminClient {
    base: Url,
    http: reqwest::Client,
    token: Option<String>,
    workspace: String,
}

impl AdminClient {
    fn new(admin_url: &str, token: Option<&str>, workspace: &str) -> anyhow::Result<Self> {
        let base =
            Url::parse(admin_url).with_context(|| format!("invalid admin URL {admin_url:?}"))?;
        if base.cannot_be_a_base() {
            return Err(anyhow!("invalid admin URL {admin_url:?}"));
        }
        Ok(Self {
            base,
            http: reqwest::Client::new(),
            token: token.filter(|t| !t.is_empty()).map(str::to_string),
            workspace: workspace.to_string(),
        })
    }

    /// `<base>/api/v1/<segments…>`, each segment percent-encoded.
    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.set_query(None);
        url.set_fragment(None);
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(["api", "v1"]).extend(segments);
        }
        url
    }

    /// `<base>/api/v1/workspaces/<ws>/<segments…>`, each segment
    /// percent-encoded.
    fn ws_url(&self, segments: &[&str]) -> Url {
        let mut url = self.url(&["workspaces", &self.workspace]);
        if let Ok(mut path) = url.path_segments_mut() {
            path.extend(segments);
        }
        url
    }

    /// Sends a workspace-scoped request to [`Self::ws_url`].
    async fn send(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<Body>,
    ) -> Result<Option<Value>, ClientError> {
        self.request(method, self.ws_url(segments), body).await
    }

    async fn request(
        &self,
        method: Method,
        url: Url,
        body: Option<Body>,
    ) -> Result<Option<Value>, ClientError> {
        let resp = self.send_request(method, url, body).await?;
        Self::parse(resp).await
    }

    /// Sends the request and returns the response, whatever its status.
    async fn send_request(
        &self,
        method: Method,
        url: Url,
        body: Option<Body>,
    ) -> Result<reqwest::Response, ClientError> {
        let mut req = self.http.request(method, url.clone());
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        req = match body {
            Some(Body::Json(v)) => req.json(&v),
            Some(Body::Bytes {
                bytes,
                content_type,
            }) => req
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(bytes),
            None => req,
        };
        Ok(req
            .send()
            .await
            .with_context(|| format!("cannot reach the Admin API at {url}"))?)
    }

    /// An error for a non-2xx status, `None` for an empty body, else the
    /// body as JSON.
    async fn parse(resp: reqwest::Response) -> Result<Option<Value>, ClientError> {
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(ClientError::Api { status, body: text });
        }
        if text.trim().is_empty() {
            return Ok(None);
        }
        let value = serde_json::from_str(&text)
            .with_context(|| format!("the Admin API sent a non-JSON {status} response"))?;
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::contract_media_type;

    #[test]
    fn contract_media_types() {
        assert_eq!(
            contract_media_type(Path::new("a.JSON"), b"openapi: 3.0.0"),
            "application/json"
        );
        assert_eq!(
            contract_media_type(Path::new("a.yml"), b"{}"),
            "application/yaml"
        );
        assert_eq!(
            contract_media_type(Path::new("contract"), b"  \n{\"openapi\": \"3.0.0\"}"),
            "application/json"
        );
        assert_eq!(
            contract_media_type(Path::new("contract"), b"openapi: 3.0.0"),
            "application/yaml"
        );
        for (name, expected) in [
            ("s.graphql", "application/graphql"),
            ("s.GraphQLS", "application/graphql"),
            ("s.gql", "application/graphql"),
            ("a.proto", "text/x-protobuf"),
            ("a.pb", "application/x-protobuf"),
            ("a.desc", "application/x-protobuf"),
            ("a.wsdl", "application/xml"),
            ("a.xsd", "application/xml"),
            ("a.xml", "application/xml"),
            ("a.zip", "application/zip"),
            ("a.tgz", "application/gzip"),
            ("a.TAR.GZ", "application/gzip"),
            ("lights.asyncapi.yaml", "application/vnd.aai.asyncapi+yaml"),
            ("lights.AsyncAPI.yml", "application/vnd.aai.asyncapi+yaml"),
            ("lights.asyncapi.json", "application/vnd.aai.asyncapi+json"),
            ("asyncapi.yaml", "application/yaml"),
        ] {
            assert_eq!(
                contract_media_type(Path::new(name), b""),
                expected,
                "{name}"
            );
        }
        assert_eq!(
            contract_media_type(Path::new(env!("CARGO_MANIFEST_DIR")), b""),
            "application/gzip"
        );
    }
}
