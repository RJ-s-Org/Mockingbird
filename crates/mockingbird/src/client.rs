//! Admin API client subcommands: `mocks`, `import`, `sessions`,
//! `export-fixtures` and `seed-fixtures`.
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
    /// Upload a contract as a new version of a mock, creating the mock
    /// (`source = api`) if it does not exist.
    Import(ImportArgs),
    /// List and reset the sessions of a mock.
    #[command(subcommand)]
    Sessions(SessionsCommand),
    /// Print a session's stored resources as JSON.
    ExportFixtures(SessionArgs),
    /// Replace a session's contents with the resources in a JSON file.
    SeedFixtures(SeedArgs),
}

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
    /// Contract file (OpenAPI, Swagger, AsyncAPI, GraphQL, .proto,
    /// descriptor set, WSDL, zip or tar.gz), or a directory, uploaded as a
    /// tar.gz.
    pub contract: PathBuf,
    /// The mock to import into.
    #[arg(long = "as", value_name = "MOCK")]
    pub mock: String,
    /// Activate the new version (the first version is always activated).
    #[arg(long)]
    pub activate: bool,
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

/// Runs `cmd` against the Admin API at `admin_url`. Returns the JSON to
/// print, or `None` when the API answered without a body.
pub async fn run(admin_url: &str, cmd: ClientCommand) -> Result<Option<Value>, ClientError> {
    let api = AdminClient::new(admin_url)?;
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
        ClientCommand::SeedFixtures(SeedArgs { target, file }) => {
            let text = read(&file)?;
            let fixtures: Value = serde_json::from_slice(&text)
                .with_context(|| format!("{} is not JSON", file.display()))?;
            api.send(
                Method::PUT,
                &["mocks", &target.mock, "sessions", &target.session],
                Some(Body::Json(fixtures)),
            )
            .await
        }
    }
}

async fn import(api: &AdminClient, args: ImportArgs) -> Result<Option<Value>, ClientError> {
    let source = mb_import::read_source(&args.contract)
        .with_context(|| format!("cannot read {}", args.contract.display()))?;
    match api.send(Method::GET, &["mocks", &args.mock], None).await {
        Ok(_) => {}
        Err(ClientError::Api { status, .. }) if status == StatusCode::NOT_FOUND => {
            api.send(
                Method::POST,
                &["mocks"],
                Some(Body::Json(json!({ "name": args.mock }))),
            )
            .await?;
        }
        Err(e) => return Err(e),
    }
    let mut url = api.url(&["mocks", &args.mock, "contracts"]);
    if args.activate {
        url.set_query(Some("activate=true"));
    }
    let content_type = contract_media_type(&args.contract, &source);
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

/// The upload media type for a contract, by extension: `.graphql`,
/// `.graphqls`, `.gql` (GraphQL), `.proto`, `.pb`/`.desc` (descriptor set),
/// `.wsdl`/`.xsd`/`.xml`, `.zip`, `.tgz`/`.tar.gz` and directories (packed
/// as tar.gz), `.asyncapi.yaml`/`.asyncapi.yml`/`.asyncapi.json`
/// (AsyncAPI); else `.json`/`.yaml`/`.yml`, else JSON when the document
/// starts with `{`, else YAML.
fn contract_media_type(path: &Path, source: &[u8]) -> &'static str {
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
}

impl AdminClient {
    fn new(admin_url: &str) -> anyhow::Result<Self> {
        let base =
            Url::parse(admin_url).with_context(|| format!("invalid admin URL {admin_url:?}"))?;
        if base.cannot_be_a_base() {
            return Err(anyhow!("invalid admin URL {admin_url:?}"));
        }
        Ok(Self {
            base,
            http: reqwest::Client::new(),
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

    async fn send(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<Body>,
    ) -> Result<Option<Value>, ClientError> {
        self.request(method, self.url(segments), body).await
    }

    async fn request(
        &self,
        method: Method,
        url: Url,
        body: Option<Body>,
    ) -> Result<Option<Value>, ClientError> {
        let mut req = self.http.request(method, url.clone());
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
        let resp = req
            .send()
            .await
            .with_context(|| format!("cannot reach the Admin API at {url}"))?;
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
