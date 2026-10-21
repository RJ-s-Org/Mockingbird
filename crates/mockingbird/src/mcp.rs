//! `mockingbird mcp`: a Model Context Protocol server that exposes every
//! Mockingbird capability to coding agents as tools — installing and
//! checking the setup, LLM onboarding, running a server, and the whole
//! Admin API — plus the skills as resources and prompts.
//!
//! Two transports: stdio (newline-delimited JSON-RPC, for agents that start
//! the process) and streamable HTTP (`POST /mcp`, for remote agents such as
//! ChatGPT). The server is stateless: every call goes to the Admin API at
//! `--admin-url`, or runs locally (doctor, init, provider tests, the
//! background server).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use clap::{Args, ValueEnum};
use reqwest::Method;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::agents::SKILLS;
use crate::client::{self, ClientError};
use crate::daemon;
use crate::onboard;
use crate::providers::{self, ProviderArgs};

/// Protocol versions this server speaks, newest first.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Largest body `call_mock` returns inline.
const MAX_INLINE_BODY: usize = 64 * 1024;

const INSTRUCTIONS: &str = "Mockingbird serves mock APIs (REST, GraphQL, gRPC, SOAP, SSE, WebSocket, Kafka/AMQP/MQTT) from their contracts, with LLM-generated, schema-valid responses. Start with `doctor`. To set up: `list_llm_providers`, `init_config` (or `configure_llm`), `test_llm`, `start_server`. To mock an API: `import_contract`, then `call_mock`; shape behaviour with `put_scenario`, `update_mock`, `seed_session`; inspect with `list_requests`. Never pass API keys as arguments: profiles hold references such as env:OPENROUTER_API_KEY that the user exports. The `mockingbird://skills/…` resources hold the full guides.";

#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    #[default]
    Stdio,
    Http,
}

#[derive(Args, Debug, Clone)]
pub struct McpArgs {
    #[arg(long, value_enum, default_value_t = Transport::Stdio)]
    pub transport: Transport,
    /// Listen address of the HTTP transport.
    #[arg(long, default_value = "127.0.0.1:7331")]
    pub listen: SocketAddr,
    /// Environment variable holding the bearer token HTTP clients must
    /// send; required to listen on a non-loopback address.
    #[arg(long, value_name = "NAME")]
    pub auth_token_env: Option<String>,
    /// Browser origins allowed to call the HTTP transport (requests with
    /// any other `Origin` are refused).
    #[arg(long = "allow-origin", value_name = "ORIGIN")]
    pub allow_origins: Vec<String>,
    /// Expose only tools that change nothing.
    #[arg(long)]
    pub read_only: bool,
    /// Over HTTP, also expose the tools that act on this machine (config
    /// files, local paths, provider tests with its environment, the
    /// background server). Always on for stdio.
    #[arg(long)]
    pub allow_local_tools: bool,
    /// Background server state (`server start`).
    #[arg(long, env = "MB_STATE_DIR", default_value = daemon::DEFAULT_STATE_DIR)]
    pub state_dir: PathBuf,
}

/// The tool handler: Admin API target and options.
pub struct McpServer {
    admin_url: String,
    token: Option<String>,
    workspace: String,
    read_only: bool,
    /// Whether tools that touch this machine are exposed (see
    /// [`LOCAL_TOOLS`]).
    local_tools: bool,
    state_dir: PathBuf,
}

/// Tools that read or write this machine's files or environment, or run
/// processes: hidden from HTTP clients unless `--allow-local-tools`, since a
/// remote caller could otherwise send a local secret to a host it picks.
const LOCAL_TOOLS: &[&str] = &[
    "doctor",
    "init_config",
    "configure_llm",
    "test_llm",
    "validate_contract",
    "start_server",
    "server_status",
    "server_logs",
    "stop_server",
];

/// Why a tool call failed: shown to the model as an error result.
struct ToolError(String);

impl From<anyhow::Error> for ToolError {
    fn from(e: anyhow::Error) -> Self {
        ToolError(format!("{e:#}"))
    }
}

impl From<ClientError> for ToolError {
    fn from(e: ClientError) -> Self {
        ToolError(match e {
            ClientError::Api { status, body } => {
                let detail = serde_json::from_str::<Value>(&body)
                    .ok()
                    .map(|v| v.to_string())
                    .unwrap_or(body);
                format!("Admin API answered {status}: {detail}")
            }
            ClientError::Usage(m) => m,
            ClientError::Incomplete { output, message } => format!("{message}: {output}"),
            ClientError::Other(e) if unreachable(&e) => format!(
                "{e:#} (is a server running? start one with the start_server tool, or set MB_ADMIN_URL)"
            ),
            ClientError::Other(e) => format!("{e:#}"),
        })
    }
}

/// Whether `e` is a failure to reach the Admin API at all.
fn unreachable(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<reqwest::Error>())
        .any(|r| r.is_connect())
}

type ToolResult = Result<Value, ToolError>;

/// One tool's definition.
struct Tool {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    schema: Value,
    read_only: bool,
    destructive: bool,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required, "additionalProperties": false })
}

fn s(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

fn b(desc: &str) -> Value {
    json!({ "type": "boolean", "description": desc })
}

fn n(desc: &str) -> Value {
    json!({ "type": "integer", "description": desc })
}

fn o(desc: &str) -> Value {
    json!({ "type": "object", "description": desc })
}

fn provider_props() -> Map<String, Value> {
    let ids: Vec<&str> = providers::PRESETS.iter().map(|p| p.id).collect();
    let mut m = Map::new();
    m.insert(
        "provider".into(),
        json!({ "type": "string", "enum": ids, "description": "Provider preset (list_llm_providers)." }),
    );
    for (k, d) in [
        ("model", "Model; default: the preset's."),
        ("base_url", "Base URL; required for custom."),
        (
            "api_key_env",
            "Name of the environment variable holding the provider key (never the key itself).",
        ),
        (
            "account_id",
            "Cloudflare account ID (default: CLOUDFLARE_ACCOUNT_ID).",
        ),
        ("gateway_id", "Cloudflare AI Gateway ID (default: default)."),
        ("resource", "Azure OpenAI resource name."),
        (
            "timeout",
            "LLM timeout before the faker answers; default 30s.",
        ),
    ] {
        m.insert(k.into(), s(d));
    }
    m.insert("no_api_key".into(), b("Send no bearer key."));
    m.insert(
        "no_gateway_token".into(),
        b("Cloudflare AI Gateway without authentication: no cf-aig-authorization."),
    );
    m.insert(
        "json_schema".into(),
        b("Request JSON Schema structured output."),
    );
    m.insert(
        "headers".into(),
        json!({ "type": "object", "additionalProperties": { "type": "string" },
            "description": "Extra provider headers; embed secrets as ${env:NAME}." }),
    );
    m.insert(
        "extra_body".into(),
        json!({ "type": "object",
            "description": "JSON merged into every provider request body, e.g. OpenRouter routing {\"provider\": {\"order\": [\"atlas-cloud\"]}} or {\"reasoning\": {\"enabled\": false}}. Not for secrets." }),
    );
    m
}

fn with(mut base: Map<String, Value>, extra: Value) -> Value {
    if let Value::Object(e) = extra {
        base.extend(e);
    }
    Value::Object(base)
}

fn tools() -> Vec<Tool> {
    let mock = || s("Mock name.");
    let session_props = || json!({ "mock": s("Mock name."), "session": s("Session id.") });
    vec![
        Tool {
            name: "doctor",
            title: "Check the Mockingbird setup",
            description: "Report the installed version, the config file (profiles, mocks, missing secrets), which provider key variables are set, whether the Admin API answers, the background server, and a list of problems with fixes. Run this first.",
            schema: obj(
                json!({ "config": s("Config file; default ./mockingbird.yaml.") }),
                &[],
            ),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "list_llm_providers",
            title: "List LLM provider presets",
            description: "The LLM provider presets (OpenRouter, Google AI Studio, Cloudflare AI Gateway and Workers AI, OpenAI, Anthropic, DeepSeek, Groq, Mistral, Together, Azure OpenAI, Ollama, LM Studio, custom) with default model, key variable, whether it is set, and where to get a key.",
            schema: obj(json!({}), &[]),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "init_config",
            title: "Write a starter mockingbird.yaml",
            description: "Create mockingbird.yaml with persistent SQLite storage, an LLM profile from a provider preset (or offline: faker only), and one mock per contract path. Returns the file, the secrets the user must export, and next steps.",
            schema: obj(
                with(
                    provider_props(),
                    json!({
                        "offline": b("No LLM; faker only."),
                        "contracts": { "type": "array", "items": { "type": "string" }, "description": "Contract file paths; each becomes a mock named after the file." },
                        "output": s("File to write; default mockingbird.yaml."),
                        "profile_name": s("Profile name; default the preset id."),
                        "force": b("Overwrite an existing file."),
                        "port": { "type": "integer", "description": "Data-plane port (server.port); default 8080." },
                        "admin_port": { "type": "integer", "description": "Admin API port (server.admin_port); default 9090. Point MB_ADMIN_URL at it." },
                        "grpc_port": { "type": "integer", "description": "gRPC port (server.grpc_port); default 50051." },
                    }),
                ),
                &[],
            ),
            read_only: false,
            destructive: false,
        },
        Tool {
            name: "configure_llm",
            title: "Add an LLM profile",
            description: "Add or replace an LLM profile built from a provider preset, in a config file (target file, the default) or in the running server (target server). The key stays in the environment variable named by the preset or api_key_env.",
            schema: obj(
                with(
                    provider_props(),
                    json!({
                        "name": s("Profile name; default the preset id."),
                        "target": { "type": "string", "enum": ["file", "server"], "description": "Where to write it; default file." },
                        "config": s("Config file for target file; default mockingbird.yaml."),
                        "default": b("Make it the default profile."),
                        "fallback": s("Profile tried when this one fails."),
                    }),
                ),
                &[],
            ),
            read_only: false,
            destructive: false,
        },
        Tool {
            name: "test_llm",
            title: "Test an LLM profile",
            description: "Send one small JSON completion through a provider preset, or a profile of a config file, and report ok, latency, the reply, and a hint on failure. Secrets resolve from this process's environment.",
            schema: obj(
                with(
                    provider_props(),
                    json!({
                        "config": s("Config file whose profile to test; default ./mockingbird.yaml when no provider is given."),
                        "profile": s("Profile name in config; default its llm.default_profile."),
                    }),
                ),
                &[],
            ),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "validate_contract",
            title: "Validate a contract file",
            description: "Compile a contract (OpenAPI, Swagger, GraphQL, protobuf, WSDL, AsyncAPI, archive) locally and list its operations and channels. Needs no server.",
            schema: obj(json!({ "path": s("Contract file path.") }), &["path"]),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "start_server",
            title: "Start Mockingbird in the background",
            description: "Run `mockingbird serve` in the background (config watched for changes) and wait until it is ready. Returns pid and the data-plane, admin and gRPC URLs.",
            schema: obj(
                json!({
                    "config": s("Config file (preferred)."),
                    "contracts": { "type": "array", "items": { "type": "string" }, "description": "Contracts to serve without a config file." },
                    "offline": b("Faker only, no LLM."),
                    "port": n("Data-plane port; default 8080."),
                    "admin_port": n("Admin API port; default 9090."),
                    "grpc_port": n("gRPC port; default 50051."),
                }),
                &[],
            ),
            read_only: false,
            destructive: false,
        },
        Tool {
            name: "server_status",
            title: "Background server status",
            description: "Whether the background server runs, and its /readyz (ready, degraded reasons).",
            schema: obj(json!({}), &[]),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "server_logs",
            title: "Background server log",
            description: "The last lines of the background server's log.",
            schema: obj(json!({ "lines": n("Lines; default 100.") }), &[]),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "stop_server",
            title: "Stop the background server",
            description: "Stop the server started with start_server.",
            schema: obj(json!({}), &[]),
            read_only: false,
            destructive: true,
        },
        Tool {
            name: "list_mocks",
            title: "List mocks",
            description: "Every mock in the workspace with its route prefix, active contract version and source.",
            schema: obj(json!({}), &[]),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "get_mock",
            title: "Show a mock",
            description: "One mock's settings: prefix, persona, profile, chaos, limits, data-plane auth, active version.",
            schema: obj(json!({ "name": mock() }), &["name"]),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "create_mock",
            title: "Create a mock",
            description: "Create an empty mock; import_contract also creates one when missing.",
            schema: obj(
                json!({
                    "name": s("Mock name: ^[a-z0-9][a-z0-9-]{0,62}$."),
                    "route_prefix": s("Route prefix; default /<name>."),
                    "persona": s("Style instructions for generated data."),
                    "llm_profile": s("LLM profile; default the default profile."),
                }),
                &["name"],
            ),
            read_only: false,
            destructive: false,
        },
        Tool {
            name: "update_mock",
            title: "Update a mock",
            description: "PATCH a mock. Fields: route_prefix, persona, llm_profile, header_overrides, data_plane_auth {mode: none|api_key|passthrough_validate, header}, chaos {error_rate, latency_jitter {min_ms, max_ms}, drop_stream_rate} (null clears), limits. Only the given fields change.",
            schema: obj(
                json!({ "name": mock(), "patch": o("Fields to change.") }),
                &["name", "patch"],
            ),
            read_only: false,
            destructive: false,
        },
        Tool {
            name: "delete_mock",
            title: "Delete a mock",
            description: "Delete a mock with its contracts, scenarios and request log.",
            schema: obj(json!({ "name": mock() }), &["name"]),
            read_only: false,
            destructive: true,
        },
        Tool {
            name: "import_contract",
            title: "Import a contract into a mock",
            description: "Import a contract (OpenAPI, Swagger, GraphQL, .proto, WSDL, AsyncAPI, zip/tar.gz, a directory) as a new version of a mock, creating the mock if missing. API documentation (Markdown, HTML, PDF, text), HAR recordings and Postman collections become a draft: pass approve to accept it when there are no warnings. Give a local path or http(s) URL in source, or the file text in content with a filename.",
            schema: obj(
                json!({
                    "mock": mock(),
                    "source": s("File path, directory, or http(s) URL."),
                    "content": s("The contract or document text, when there is no file."),
                    "content_base64": s("Binary content (zip, PDF, descriptor set), base64."),
                    "filename": s("Name for content, with its extension, e.g. api.yaml."),
                    "activate": b("Activate the new version (the first is always activated)."),
                    "approve": b("Approve a draft import that reported no warnings."),
                    "timeout": s("How long to wait for a draft import; default 10m."),
                }),
                &["mock"],
            ),
            read_only: false,
            destructive: false,
        },
        Tool {
            name: "list_contract_versions",
            title: "List contract versions",
            description: "A mock's contract versions with status (active, inactive, draft) and warnings.",
            schema: obj(json!({ "mock": mock() }), &["mock"]),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "activate_contract_version",
            title: "Activate a contract version",
            description: "Serve this version of the mock's contract (roll forward or back).",
            schema: obj(
                json!({ "mock": mock(), "version": n("Version number.") }),
                &["mock", "version"],
            ),
            read_only: false,
            destructive: false,
        },
        Tool {
            name: "list_scenarios",
            title: "List scenarios",
            description: "A mock's scenarios: named override sets selected per request with X-Mockingbird-Scenario.",
            schema: obj(json!({ "mock": mock() }), &["mock"]),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "put_scenario",
            title: "Create or replace a scenario",
            description: "A named set of overrides, selected per request with X-Mockingbird-Scenario. Fields: instructions, status (declared by the operations, or any 5xx), latency_ms, set (fields forced into the body), operations (ids; empty = all), stream_items, stream_item_interval_ms, chaos.",
            schema: obj(
                json!({ "mock": mock(), "name": s("Scenario name."), "scenario": o("The scenario.") }),
                &["mock", "name", "scenario"],
            ),
            read_only: false,
            destructive: false,
        },
        Tool {
            name: "delete_scenario",
            title: "Delete a scenario",
            description: "Delete a scenario of a mock.",
            schema: obj(
                json!({ "mock": mock(), "name": s("Scenario name.") }),
                &["mock", "name"],
            ),
            read_only: false,
            destructive: true,
        },
        Tool {
            name: "list_sessions",
            title: "List sessions",
            description: "A mock's live sessions, most recently used first.",
            schema: obj(json!({ "mock": mock() }), &["mock"]),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "export_session",
            title: "Export a session's state",
            description: "A session's stored resources as fixtures [{resource_type, id, data}].",
            schema: obj(session_props(), &["mock", "session"]),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "seed_session",
            title: "Seed a session with fixtures",
            description: "Replace a session's state with fixtures [{resource_type, id, data}].",
            schema: obj(
                with(
                    session_props().as_object().cloned().unwrap_or_default(),
                    json!({ "fixtures": { "type": "array", "items": { "type": "object" } } }),
                ),
                &["mock", "session", "fixtures"],
            ),
            read_only: false,
            destructive: true,
        },
        Tool {
            name: "reset_session",
            title: "Reset a session",
            description: "Delete a session's state.",
            schema: obj(session_props(), &["mock", "session"]),
            read_only: false,
            destructive: true,
        },
        Tool {
            name: "list_requests",
            title: "Read the request log",
            description: "Recorded calls to a mock, newest first, with redacted bodies, matched operation, status, source (llm, repaired, fallback) and latency.",
            schema: obj(
                json!({
                    "mock": mock(),
                    "limit": n("1-500; default 20."),
                    "session": s("Only this session."),
                    "operation": s("Only this operation id."),
                    "source": s("llm, repaired or fallback."),
                    "kind": s("http, graphql, grpc, soap or event."),
                }),
                &["mock"],
            ),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "call_mock",
            title: "Call a mock",
            description: "Send an HTTP request to the data plane (REST, GraphQL at <prefix>/graphql, SOAP) and return status, x-mockingbird-* headers and body. Override fields map to X-Mockingbird-* headers.",
            schema: obj(
                json!({
                    "method": s("HTTP method; default GET."),
                    "path": s("Path including the mock's prefix, e.g. /orders/orders/42."),
                    "body": { "description": "JSON body, or a string sent as is." },
                    "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                    "session": s("X-Mockingbird-Session."),
                    "status": n("X-Mockingbird-Status: force a status."),
                    "latency": s("X-Mockingbird-Latency, e.g. 250ms."),
                    "scenario": s("X-Mockingbird-Scenario."),
                    "stream_items": n("X-Mockingbird-Stream-Items."),
                    "base_url": s("Data-plane URL; default the background server's or http://127.0.0.1:8080."),
                }),
                &["path"],
            ),
            read_only: false,
            destructive: false,
        },
        Tool {
            name: "list_channels",
            title: "List event channels",
            description: "A mock's event channels (AsyncAPI) with triggers and broker.",
            schema: obj(json!({ "mock": mock() }), &["mock"]),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "publish_event",
            title: "Publish events",
            description: "Generate and publish messages on a channel the mock publishes.",
            schema: obj(
                json!({
                    "mock": mock(),
                    "channel": s("Channel id."),
                    "count": n("1-100; default 1."),
                    "context": o("JSON handed to the message prompt."),
                }),
                &["mock", "channel"],
            ),
            read_only: false,
            destructive: false,
        },
        Tool {
            name: "list_llm_profiles",
            title: "List LLM profiles",
            description: "The workspace's LLM profiles (secret references only, never values).",
            schema: obj(json!({}), &[]),
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "put_llm_profile",
            title: "Create or replace an LLM profile",
            description: "Write an LLM profile through the Admin API: base_url, model, secret_ref (env:NAME), json_schema, temperature, timeout_ms, max_tokens, fallback_profile, headers. configure_llm builds one from a preset.",
            schema: obj(
                json!({ "name": s("Profile name."), "profile": o("The profile.") }),
                &["name", "profile"],
            ),
            read_only: false,
            destructive: false,
        },
        Tool {
            name: "delete_llm_profile",
            title: "Delete an LLM profile",
            description: "Delete an LLM profile; mocks using it are reported.",
            schema: obj(json!({ "name": s("Profile name.") }), &["name"]),
            read_only: false,
            destructive: true,
        },
        Tool {
            name: "admin_api",
            title: "Call the Admin API",
            description: "Any Admin API request, for what the other tools do not cover: workspaces, API keys, audit, drafts, imports, channel settings. path is under /api/v1, e.g. /workspaces/default/api-keys. The OpenAPI document is at <admin>/api/openapi.json.",
            schema: obj(
                json!({
                    "method": s("GET, POST, PUT, PATCH or DELETE."),
                    "path": s("Path under /api/v1, with any query."),
                    "body": { "description": "JSON body." },
                }),
                &["method", "path"],
            ),
            read_only: false,
            destructive: true,
        },
    ]
}

fn tool_json(t: &Tool) -> Value {
    json!({
        "name": t.name,
        "title": t.title,
        "description": t.description,
        "inputSchema": t.schema,
        "annotations": {
            "title": t.title,
            "readOnlyHint": t.read_only,
            "destructiveHint": t.destructive,
            "openWorldHint": false,
        },
    })
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError(format!("missing string argument {key:?}")))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(String::from)
}

fn opt_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn opt_port(args: &Value, key: &str) -> Result<Option<u16>, ToolError> {
    opt_u64(args, key)
        .map(|p| u16::try_from(p).map_err(|_| ToolError(format!("{key} is not a port"))))
        .transpose()
}

fn duration(args: &Value, key: &str) -> Result<Option<humantime::Duration>, ToolError> {
    opt_str(args, key)
        .map(|t| {
            t.parse::<humantime::Duration>()
                .map_err(|e| ToolError(format!("{key}: {e}")))
        })
        .transpose()
}

fn provider_args(args: &Value) -> Result<ProviderArgs, ToolError> {
    let headers = args
        .get("headers")
        .and_then(Value::as_object)
        .map(|h| {
            h.iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                .collect()
        })
        .unwrap_or_default();
    Ok(ProviderArgs {
        provider: opt_str(args, "provider"),
        model: opt_str(args, "model"),
        base_url: opt_str(args, "base_url"),
        api_key_env: opt_str(args, "api_key_env"),
        no_api_key: opt_bool(args, "no_api_key"),
        account_id: opt_str(args, "account_id"),
        gateway_id: opt_str(args, "gateway_id"),
        no_gateway_token: opt_bool(args, "no_gateway_token"),
        resource: opt_str(args, "resource"),
        headers,
        json_schema: args.get("json_schema").and_then(Value::as_bool),
        timeout: duration(args, "timeout")?,
        extra_body: args
            .get("extra_body")
            .filter(|v| !v.is_null())
            .map(Value::to_string),
    })
}

fn paths(args: &Value, key: &str) -> Vec<PathBuf> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

impl McpServer {
    pub fn new(admin_url: &str, token: Option<&str>, workspace: &str, args: &McpArgs) -> Self {
        Self {
            admin_url: admin_url.to_string(),
            token: token.map(String::from),
            workspace: workspace.to_string(),
            read_only: args.read_only,
            local_tools: args.transport == Transport::Stdio || args.allow_local_tools,
            state_dir: args.state_dir.clone(),
        }
    }

    fn tools(&self) -> Vec<Tool> {
        tools()
            .into_iter()
            .filter(|t| !self.read_only || t.read_only)
            .filter(|t| self.local_tools || !LOCAL_TOOLS.contains(&t.name))
            .collect()
    }

    /// One Admin API call scoped to the workspace.
    async fn ws(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> ToolResult {
        // Mutating requests must carry a JSON content type, even with no
        // body (the admin port's CSRF defence).
        let body = match (&method, body) {
            (&Method::GET | &Method::DELETE, b) => b,
            (_, None) => Some(json!({})),
            (_, b) => b,
        };
        let out = client::api(
            &self.admin_url,
            self.token.as_deref(),
            Some(&self.workspace),
            method,
            segments,
            query,
            body,
        )
        .await?;
        Ok(out.unwrap_or_else(|| json!({ "ok": true })))
    }

    fn state(&self) -> daemon::StateArgs {
        daemon::StateArgs {
            state_dir: self.state_dir.clone(),
        }
    }

    /// Runs tool `name`.
    async fn call(&self, name: &str, a: &Value) -> ToolResult {
        let Some(tool) = self.tools().into_iter().find(|t| t.name == name) else {
            return Err(ToolError(format!("unknown tool {name:?}")));
        };
        if self.read_only && !tool.read_only {
            return Err(ToolError("this MCP server is read-only".into()));
        }
        match name {
            "doctor" => {
                let args = daemon::DoctorArgs {
                    config: opt_str(a, "config").map(PathBuf::from),
                    state: self.state(),
                };
                Ok(daemon::doctor(&args, &self.admin_url).await)
            }
            "list_llm_providers" => Ok(onboard::providers_json()),
            "init_config" => {
                let args = onboard::InitArgs {
                    provider: provider_args(a)?,
                    offline: opt_bool(a, "offline"),
                    profile_name: opt_str(a, "profile_name"),
                    contracts: paths(a, "contracts"),
                    storage_url: None,
                    port: opt_port(a, "port")?,
                    admin_port: opt_port(a, "admin_port")?,
                    grpc_port: opt_port(a, "grpc_port")?,
                    output: opt_str(a, "output")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| "mockingbird.yaml".into()),
                    force: opt_bool(a, "force"),
                };
                Ok(onboard::init(&args)?)
            }
            "configure_llm" => {
                let args = onboard::ConfigureArgs {
                    provider: provider_args(a)?,
                    name: opt_str(a, "name"),
                    config: opt_str(a, "config")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| "mockingbird.yaml".into()),
                    default: opt_bool(a, "default"),
                    fallback: opt_str(a, "fallback"),
                    apply: opt_str(a, "target").as_deref() == Some("server"),
                    dry_run: false,
                };
                if args.apply {
                    let (name, body) = onboard::configure_api_body(&args)?;
                    self.ws(Method::PUT, &["llm-profiles", &name], &[], Some(body))
                        .await
                } else {
                    Ok(onboard::configure_file(&args)?)
                }
            }
            "test_llm" => {
                let args = onboard::TestArgs {
                    provider: provider_args(a)?,
                    config: opt_str(a, "config").map(PathBuf::from),
                    profile: opt_str(a, "profile"),
                };
                let profile = onboard::test_profile(&args)?;
                Ok(serde_json::to_value(providers::probe(&profile).await?)
                    .map_err(anyhow::Error::from)?)
            }
            "validate_contract" => Ok(onboard::contract_summary(&PathBuf::from(str_arg(
                a, "path",
            )?))?),
            "start_server" => {
                let args = daemon::StartArgs {
                    state: self.state(),
                    config: opt_str(a, "config").map(PathBuf::from),
                    host: None,
                    port: opt_port(a, "port")?,
                    admin_port: opt_port(a, "admin_port")?,
                    grpc_port: opt_port(a, "grpc_port")?,
                    offline: opt_bool(a, "offline"),
                    no_watch: false,
                    insecure_no_auth: false,
                    wait: std::time::Duration::from_secs(60).into(),
                    contracts: paths(a, "contracts"),
                };
                if args.config.is_none() && args.contracts.is_empty() {
                    return Err(ToolError("give config or contracts".into()));
                }
                if let Some(config) = &args.config
                    && !args.offline
                {
                    let missing = daemon::missing_default_secrets(config);
                    if !missing.is_empty() {
                        return Err(ToolError(format!(
                            "the default LLM profile needs {} but this MCP server process cannot see it, so every response would be faker output. Export it where your agent starts (or in your shell profile) and restart the agent, or pass offline: true to start without an LLM.",
                            missing.join(", ")
                        )));
                    }
                }
                Ok(daemon::start(&args).await?)
            }
            "server_status" => Ok(daemon::status(&self.state()).await?),
            "server_logs" => {
                let lines = opt_u64(a, "lines").unwrap_or(100);
                let text = daemon::logs(&self.state(), usize::try_from(lines).unwrap_or(100))?;
                Ok(json!({ "log": text }))
            }
            "stop_server" => Ok(daemon::stop(&self.state()).await?),
            "list_mocks" => self.ws(Method::GET, &["mocks"], &[], None).await,
            "get_mock" => {
                self.ws(Method::GET, &["mocks", str_arg(a, "name")?], &[], None)
                    .await
            }
            "create_mock" => {
                let mut body = json!({ "name": str_arg(a, "name")? });
                for key in ["route_prefix", "persona", "llm_profile"] {
                    if let Some(v) = opt_str(a, key) {
                        body[key] = json!(v);
                    }
                }
                self.ws(Method::POST, &["mocks"], &[], Some(body)).await
            }
            "update_mock" => {
                let patch = a.get("patch").cloned().unwrap_or_else(|| json!({}));
                self.ws(
                    Method::PATCH,
                    &["mocks", str_arg(a, "name")?],
                    &[],
                    Some(patch),
                )
                .await
            }
            "delete_mock" => {
                self.ws(Method::DELETE, &["mocks", str_arg(a, "name")?], &[], None)
                    .await
            }
            "import_contract" => self.import(a).await,
            "list_contract_versions" => {
                self.ws(
                    Method::GET,
                    &["mocks", str_arg(a, "mock")?, "contracts"],
                    &[],
                    None,
                )
                .await
            }
            "activate_contract_version" => {
                let version = opt_u64(a, "version")
                    .ok_or_else(|| ToolError("missing integer argument \"version\"".into()))?
                    .to_string();
                self.ws(
                    Method::POST,
                    &[
                        "mocks",
                        str_arg(a, "mock")?,
                        "contracts",
                        &version,
                        "activate",
                    ],
                    &[],
                    None,
                )
                .await
            }
            "list_scenarios" => {
                self.ws(
                    Method::GET,
                    &["mocks", str_arg(a, "mock")?, "scenarios"],
                    &[],
                    None,
                )
                .await
            }
            "put_scenario" => {
                self.ws(
                    Method::PUT,
                    &[
                        "mocks",
                        str_arg(a, "mock")?,
                        "scenarios",
                        str_arg(a, "name")?,
                    ],
                    &[],
                    a.get("scenario").cloned(),
                )
                .await
            }
            "delete_scenario" => {
                self.ws(
                    Method::DELETE,
                    &[
                        "mocks",
                        str_arg(a, "mock")?,
                        "scenarios",
                        str_arg(a, "name")?,
                    ],
                    &[],
                    None,
                )
                .await
            }
            "list_sessions" => {
                self.ws(
                    Method::GET,
                    &["mocks", str_arg(a, "mock")?, "sessions"],
                    &[],
                    None,
                )
                .await
            }
            "export_session" | "seed_session" | "reset_session" => {
                let segments = [
                    "mocks",
                    str_arg(a, "mock")?,
                    "sessions",
                    str_arg(a, "session")?,
                ];
                let (method, body) = match name {
                    "export_session" => (Method::GET, None),
                    "seed_session" => (
                        Method::PUT,
                        Some(a.get("fixtures").cloned().unwrap_or_else(|| json!([]))),
                    ),
                    _ => (Method::DELETE, None),
                };
                self.ws(method, &segments, &[], body).await
            }
            "list_requests" => {
                let mut query = vec![("limit", opt_u64(a, "limit").unwrap_or(20).to_string())];
                for key in ["session", "operation", "source", "kind"] {
                    if let Some(v) = opt_str(a, key) {
                        query.push((key, v));
                    }
                }
                self.ws(
                    Method::GET,
                    &["mocks", str_arg(a, "mock")?, "requests"],
                    &query,
                    None,
                )
                .await
            }
            "call_mock" => self.call_mock(a).await,
            "list_channels" => {
                self.ws(
                    Method::GET,
                    &["mocks", str_arg(a, "mock")?, "channels"],
                    &[],
                    None,
                )
                .await
            }
            "publish_event" => {
                let body = json!({
                    "count": opt_u64(a, "count").unwrap_or(1),
                    "context": a.get("context").cloned().unwrap_or(Value::Null),
                });
                self.ws(
                    Method::POST,
                    &[
                        "mocks",
                        str_arg(a, "mock")?,
                        "channels",
                        str_arg(a, "channel")?,
                        "publish",
                    ],
                    &[],
                    Some(body),
                )
                .await
            }
            "list_llm_profiles" => self.ws(Method::GET, &["llm-profiles"], &[], None).await,
            "put_llm_profile" => {
                self.ws(
                    Method::PUT,
                    &["llm-profiles", str_arg(a, "name")?],
                    &[],
                    a.get("profile").cloned(),
                )
                .await
            }
            "delete_llm_profile" => {
                self.ws(
                    Method::DELETE,
                    &["llm-profiles", str_arg(a, "name")?],
                    &[],
                    None,
                )
                .await
            }
            "admin_api" => {
                let method = str_arg(a, "method")?
                    .to_ascii_uppercase()
                    .parse::<Method>()
                    .map_err(|e| ToolError(format!("method: {e}")))?;
                let body = match (&method, a.get("body").cloned()) {
                    (&Method::GET | &Method::DELETE, b) => b,
                    (_, None) => Some(json!({})),
                    (_, b) => b,
                };
                let out = client::api_path(
                    &self.admin_url,
                    self.token.as_deref(),
                    method,
                    str_arg(a, "path")?,
                    body,
                )
                .await?;
                Ok(out.unwrap_or_else(|| json!({ "ok": true })))
            }
            other => Err(ToolError(format!("unknown tool {other:?}"))),
        }
    }

    async fn import(&self, a: &Value) -> ToolResult {
        let mock = str_arg(a, "mock")?.to_string();
        let mut scratch = None;
        let source = match (
            opt_str(a, "source"),
            opt_str(a, "content"),
            opt_str(a, "content_base64"),
        ) {
            (Some(source), None, None) => {
                let remote = ["http://", "https://"]
                    .iter()
                    .any(|p| source.to_ascii_lowercase().starts_with(p));
                if !self.local_tools && !remote {
                    return Err(ToolError(
                        "this server takes no local paths: pass an http(s) URL in source, or the text in content".into(),
                    ));
                }
                source
            }
            (None, text, b64) if text.is_some() || b64.is_some() => {
                let bytes = match (text, b64) {
                    (Some(t), None) => t.into_bytes(),
                    (None, Some(b)) => {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD
                            .decode(b.trim())
                            .map_err(|e| ToolError(format!("content_base64: {e}")))?
                    }
                    _ => return Err(ToolError("give content or content_base64, not both".into())),
                };
                let filename = opt_str(a, "filename").unwrap_or_else(|| "contract.yaml".into());
                let filename = std::path::Path::new(&filename)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .filter(|f| !f.is_empty() && *f != "..")
                    .ok_or_else(|| ToolError("filename must be a plain file name".into()))?
                    .to_string();
                let dir =
                    std::env::temp_dir().join(format!("mockingbird-mcp-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&dir).map_err(anyhow::Error::from)?;
                let path = dir.join(&filename);
                std::fs::write(&path, bytes).map_err(anyhow::Error::from)?;
                scratch = Some(dir);
                path.display().to_string()
            }
            _ => {
                return Err(ToolError(
                    "give exactly one of source, content or content_base64".into(),
                ));
            }
        };
        let cmd = client::ClientCommand::Import(client::ImportArgs {
            source,
            mock,
            activate: opt_bool(a, "activate"),
            approve: opt_bool(a, "approve"),
            timeout: duration(a, "timeout")?
                .unwrap_or_else(|| std::time::Duration::from_secs(600).into()),
        });
        let result =
            client::run(&self.admin_url, self.token.as_deref(), &self.workspace, cmd).await;
        if let Some(dir) = scratch {
            let _ = std::fs::remove_dir_all(dir);
        }
        Ok(result?.unwrap_or_else(|| json!({ "ok": true })))
    }

    /// The data-plane base URL: the argument, the background server's, or
    /// the default.
    fn data_plane(&self, a: &Value) -> String {
        opt_str(a, "base_url")
            .filter(|_| self.local_tools)
            .or_else(|| std::env::var("MB_DATA_URL").ok())
            .or_else(|| {
                std::fs::read(self.state_dir.join("server.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|v| v["data_plane"].as_str().map(String::from))
            })
            .unwrap_or_else(|| "http://127.0.0.1:8080".into())
    }

    async fn call_mock(&self, a: &Value) -> ToolResult {
        let method = opt_str(a, "method")
            .unwrap_or_else(|| "GET".into())
            .to_ascii_uppercase()
            .parse::<Method>()
            .map_err(|e| ToolError(format!("method: {e}")))?;
        let path = str_arg(a, "path")?;
        if !path.starts_with('/') {
            return Err(ToolError("path must start with /".into()));
        }
        let url = format!("{}{path}", self.data_plane(a).trim_end_matches('/'));
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(anyhow::Error::from)?;
        let mut req = http.request(method, &url);
        if let Some(h) = a.get("headers").and_then(Value::as_object) {
            for (k, v) in h {
                req = req.header(k, v.as_str().unwrap_or_default());
            }
        }
        for (key, header) in [
            ("session", "x-mockingbird-session"),
            ("latency", "x-mockingbird-latency"),
            ("scenario", "x-mockingbird-scenario"),
        ] {
            if let Some(v) = opt_str(a, key) {
                req = req.header(header, v);
            }
        }
        for (key, header) in [
            ("status", "x-mockingbird-status"),
            ("stream_items", "x-mockingbird-stream-items"),
        ] {
            if let Some(v) = opt_u64(a, key) {
                req = req.header(header, v.to_string());
            }
        }
        req = match a.get("body") {
            None | Some(Value::Null) => req,
            Some(Value::String(text)) => req.body(text.clone()),
            Some(v) => req.json(v),
        };
        let resp = req
            .send()
            .await
            .map_err(|e| ToolError(format!("cannot reach the data plane at {url}: {e}")))?;
        let status = resp.status().as_u16();
        let headers: Map<String, Value> = resp
            .headers()
            .iter()
            .filter(|(k, _)| {
                let k = k.as_str();
                k.starts_with("x-mockingbird") || k == "content-type" || k == "location"
            })
            .map(|(k, v)| (k.to_string(), json!(v.to_str().unwrap_or_default())))
            .collect();
        let bytes = resp.bytes().await.map_err(anyhow::Error::from)?;
        let truncated = bytes.len() > MAX_INLINE_BODY;
        let shown = &bytes[..bytes.len().min(MAX_INLINE_BODY)];
        let body = serde_json::from_slice::<Value>(shown)
            .unwrap_or_else(|_| json!(String::from_utf8_lossy(shown)));
        Ok(json!({
            "url": url,
            "status": status,
            "headers": headers,
            "body": body,
            "truncated": truncated,
        }))
    }

    /// Handles one JSON-RPC message; `None` for notifications and
    /// responses.
    pub async fn handle(&self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str);
        let Some(method) = method else {
            // A response to a request we never send, or garbage.
            return id.map(|id| rpc_error(id, -32600, "invalid request"));
        };
        let id = id?;
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({
                "tools": self.tools().iter().map(tool_json).collect::<Vec<_>>()
            })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                Ok(match self.call(name, &args).await {
                    Ok(value) => tool_result(value, false),
                    Err(ToolError(message)) => json!({
                        "content": [{ "type": "text", "text": message }],
                        "isError": true,
                    }),
                })
            }
            "resources/list" => Ok(json!({ "resources": resources() })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "resources/read" => {
                let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
                read_resource(uri)
            }
            "prompts/list" => Ok(json!({ "prompts": prompts() })),
            "prompts/get" => get_prompt(&params),
            "logging/setLevel" => Ok(json!({})),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => rpc_error(id, code, &message),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = requested
            .filter(|v| PROTOCOL_VERSIONS.contains(v))
            .unwrap_or(PROTOCOL_VERSIONS[0]);
        json!({
            "protocolVersion": version,
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "listChanged": false, "subscribe": false },
                "prompts": { "listChanged": false },
                "logging": {},
            },
            "serverInfo": {
                "name": "mockingbird",
                "title": "Mockingbird",
                "version": env!("CARGO_PKG_VERSION"),
                "websiteUrl": "https://github.com/rj-s-org/mockingbird",
            },
            "instructions": INSTRUCTIONS,
        })
    }
}

fn tool_result(value: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_default();
    let mut out = json!({ "content": [{ "type": "text", "text": text }], "isError": is_error });
    // structuredContent must be an object.
    if value.is_object() {
        out["structuredContent"] = value;
    }
    out
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn resources() -> Vec<Value> {
    let mut list: Vec<Value> = SKILLS
        .iter()
        .map(|s| {
            json!({
                "uri": format!("mockingbird://skills/{}", s.name),
                "name": s.name,
                "title": s.name,
                "description": s.description(),
                "mimeType": "text/markdown",
            })
        })
        .collect();
    list.push(json!({
        "uri": "mockingbird://providers",
        "name": "providers",
        "title": "LLM provider presets",
        "description": "The LLM provider presets as JSON.",
        "mimeType": "application/json",
    }));
    list
}

fn read_resource(uri: &str) -> Result<Value, (i64, String)> {
    let (text, mime) = if uri == "mockingbird://providers" {
        (
            serde_json::to_string_pretty(&onboard::providers_json()).unwrap_or_default(),
            "application/json",
        )
    } else if let Some(skill) = uri
        .strip_prefix("mockingbird://skills/")
        .and_then(|n| SKILLS.iter().find(|s| s.name == n))
    {
        (skill.text.to_string(), "text/markdown")
    } else {
        return Err((-32002, format!("resource not found: {uri}")));
    };
    Ok(json!({ "contents": [{ "uri": uri, "mimeType": mime, "text": text }] }))
}

/// Prompts: one per workflow, carrying the matching skill.
const PROMPTS: &[(&str, &str, &str)] = &[
    (
        "setup",
        "mockingbird-setup",
        "Install Mockingbird, pick and configure an LLM provider, and check it works.",
    ),
    (
        "mock-api",
        "mockingbird-mock-api",
        "Mock an API from a contract, documentation, HAR or Postman collection.",
    ),
    (
        "self-host",
        "mockingbird-deploy",
        "Run Mockingbird as a shared, persistent endpoint.",
    ),
    (
        "testing",
        "mockingbird-testing",
        "Use Mockingbird mocks in tests and CI.",
    ),
];

fn prompts() -> Vec<Value> {
    PROMPTS
        .iter()
        .map(|(name, _, desc)| {
            json!({
                "name": name,
                "title": desc,
                "description": desc,
                "arguments": [{ "name": "goal", "description": "What the user wants, in their words.", "required": false }],
            })
        })
        .collect()
}

fn get_prompt(params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let (_, skill, desc) = PROMPTS
        .iter()
        .find(|(n, _, _)| *n == name)
        .ok_or_else(|| (-32602, format!("unknown prompt {name:?}")))?;
    let guide = SKILLS
        .iter()
        .find(|s| s.name == *skill)
        .map(|s| s.body())
        .unwrap_or_default();
    let goal = params
        .pointer("/arguments/goal")
        .and_then(Value::as_str)
        .unwrap_or(
            "Follow the guide with sensible defaults; ask only for choices you cannot infer.",
        );
    Ok(json!({
        "description": desc,
        "messages": [{
            "role": "user",
            "content": { "type": "text", "text": format!("{guide}\n\n---\n\nGoal: {goal}\n\nUse the mockingbird MCP tools; start with `doctor`.") },
        }],
    }))
}

/// `mockingbird mcp`.
pub async fn run(
    args: McpArgs,
    admin_url: &str,
    token: Option<&str>,
    workspace: &str,
) -> anyhow::Result<()> {
    let server = Arc::new(McpServer::new(admin_url, token, workspace, &args));
    match args.transport {
        Transport::Stdio => stdio(server).await,
        Transport::Http => http(server, &args).await,
    }
}

async fn stdio(server: Arc<McpServer>) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::channel::<Value>(64);
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(msg) = rx.recv().await {
            let mut line = serde_json::to_vec(&msg).unwrap_or_default();
            line.push(b'\n');
            if out.write_all(&line).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => msg,
            Err(e) => {
                let _ = tx
                    .send(rpc_error(Value::Null, -32700, &format!("parse error: {e}")))
                    .await;
                continue;
            }
        };
        // Each request runs on its own task: a long import does not block
        // pings or other calls.
        let server = server.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Some(resp) = server.handle(msg).await {
                let _ = tx.send(resp).await;
            }
        });
    }
    drop(tx);
    // Let in-flight calls finish before the process exits.
    let _ = writer.await;
    Ok(())
}

#[derive(Clone)]
struct HttpState {
    server: Arc<McpServer>,
    token: Option<String>,
    origins: Arc<Vec<String>>,
}

/// Equal in time independent of where the inputs differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn http(server: Arc<McpServer>, args: &McpArgs) -> anyhow::Result<()> {
    let token = match &args.auth_token_env {
        Some(name) => Some(
            std::env::var(name)
                .ok()
                .filter(|t| t.len() >= 16)
                .ok_or_else(|| anyhow!("{name} must hold a token of at least 16 characters"))?,
        ),
        None => None,
    };
    if token.is_none() && !args.listen.ip().is_loopback() {
        bail!(
            "listening on {} needs --auth-token-env: anyone who reaches it could drive the Admin API",
            args.listen
        );
    }
    let state = HttpState {
        server,
        token,
        origins: Arc::new(args.allow_origins.clone()),
    };
    let app = Router::new()
        .route("/mcp", post(http_post).get(http_get).delete(http_get))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    eprintln!(
        "mockingbird MCP server on http://{}/mcp (Admin API {})",
        listener.local_addr()?,
        state.server.admin_url
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Origin and bearer checks shared by every method.
fn admit(st: &HttpState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok())
        && !st.origins.iter().any(|o| o == origin)
    {
        return Err(Box::new(
            (StatusCode::FORBIDDEN, "origin not allowed").into_response(),
        ));
    }
    if let Some(token) = &st.token {
        let given = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();
        if !same(given.as_bytes(), token.as_bytes()) {
            return Err(Box::new(
                (
                    StatusCode::UNAUTHORIZED,
                    [("www-authenticate", "Bearer")],
                    "missing or wrong bearer token",
                )
                    .into_response(),
            ));
        }
    }
    Ok(())
}

async fn http_get(State(st): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(r) = admit(&st, &headers) {
        return *r;
    }
    // Stateless server: no server-initiated stream, no sessions to end.
    (StatusCode::METHOD_NOT_ALLOWED, [("allow", "POST")]).into_response()
}

async fn http_post(State(st): State<HttpState>, headers: HeaderMap, body: String) -> Response {
    if let Err(r) = admit(&st, &headers) {
        return *r;
    }
    let msg = match serde_json::from_str::<Value>(&body) {
        Ok(msg) => msg,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
            )
                .into_response();
        }
    };
    match st.server.handle(msg).await {
        Some(resp) => axum::Json(resp).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(read_only: bool) -> McpServer {
        McpServer {
            admin_url: "http://127.0.0.1:1".into(),
            token: None,
            workspace: "default".into(),
            read_only,
            local_tools: true,
            state_dir: std::env::temp_dir().join("mb-mcp-test-none"),
        }
    }

    #[tokio::test]
    async fn initialize_negotiates_the_version() {
        let s = server(false);
        let r = s
            .handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}))
            .await
            .unwrap_or_default();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        let r = s
            .handle(json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}))
            .await
            .unwrap_or_default();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert!(
            s.handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn tools_are_unique_and_described() {
        let all = tools();
        let mut names: Vec<_> = all.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len());
        for t in &all {
            assert_eq!(t.schema["type"], "object", "{}", t.name);
            assert!(t.description.len() > 20, "{}", t.name);
        }
        let ro = server(true);
        let r = ro
            .handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .await
            .unwrap_or_default();
        let listed = r["result"]["tools"].as_array().cloned().unwrap_or_default();
        assert!(
            listed
                .iter()
                .all(|t| t["annotations"]["readOnlyHint"] == true)
        );
        assert!(listed.iter().any(|t| t["name"] == "list_mocks"));
        let r = ro
            .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"delete_mock","arguments":{"name":"x"}}}))
            .await
            .unwrap_or_default();
        assert_eq!(r["result"]["isError"], true);
    }

    #[tokio::test]
    async fn remote_clients_get_no_local_tools() {
        let remote = McpServer {
            local_tools: false,
            ..server(false)
        };
        let names: Vec<&str> = remote.tools().iter().map(|t| t.name).collect();
        assert!(!names.contains(&"test_llm") && !names.contains(&"init_config"));
        assert!(names.contains(&"import_contract") && names.contains(&"list_mocks"));
        let r = remote
            .handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"import_contract","arguments":{"mock":"m","source":"/etc/passwd"}}}))
            .await
            .unwrap_or_default();
        assert_eq!(r["result"]["isError"], true);
        let r = remote
            .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"test_llm","arguments":{"provider":"openai"}}}))
            .await
            .unwrap_or_default();
        assert_eq!(r["result"]["isError"], true);
    }

    #[tokio::test]
    async fn unreachable_admin_api_is_a_tool_error() {
        let r = server(false)
            .handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_mocks","arguments":{}}}))
            .await
            .unwrap_or_default();
        assert_eq!(r["result"]["isError"], true);
        let text = r["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains("start_server"), "{text}");
    }

    #[tokio::test]
    async fn resources_and_prompts() {
        let s = server(false);
        let r = s
            .handle(json!({"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"mockingbird://skills/mockingbird-setup"}}))
            .await
            .unwrap_or_default();
        let text = r["result"]["contents"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains("OpenRouter"));
        let r = s
            .handle(json!({"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":"setup","arguments":{"goal":"use gemini"}}}))
            .await
            .unwrap_or_default();
        let text = r["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains("use gemini"));
        let r = s
            .handle(json!({"jsonrpc":"2.0","id":3,"method":"nope"}))
            .await
            .unwrap_or_default();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn server_hint_only_when_the_admin_api_is_unreachable() {
        let refused = reqwest::Client::new()
            .get("http://127.0.0.1:9/healthz")
            .send()
            .await
            .err()
            .map(ClientError::from);
        let Some(refused) = refused else {
            return;
        };
        assert!(ToolError::from(refused).0.contains("is a server running?"));
        let llm = ClientError::Other(anyhow::anyhow!("the LLM call timed out"));
        let text = ToolError::from(llm).0;
        assert!(!text.contains("server running"), "{text}");
    }
}
