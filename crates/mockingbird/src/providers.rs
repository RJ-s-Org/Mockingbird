//! LLM provider presets for onboarding: OpenRouter, Google AI Studio,
//! Cloudflare AI Gateway and Workers AI, the major first-party APIs, local
//! runtimes, and `custom` for any other OpenAI-compatible endpoint (bring
//! your own key and base URL).
//!
//! A preset turns a few answers (model, key variable, Cloudflare account and
//! gateway) into an LLM profile: a [`FileProfile`] for `mockingbird.yaml` or
//! the JSON body of `PUT …/llm-profiles/<name>`. Keys are always secret
//! references (`env:NAME`); no raw key passes through here.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use clap::Args;
use mb_llm::{
    ChatMessage, ChatRequest, HttpOptions, OpenAiCompatible, OpenAiConfig, Provider,
    ResponseFormat, Role,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::file_config::FileProfile;

/// A value a preset's base URL or headers need (`{account_id}`).
#[derive(Serialize, Debug, Clone, Copy)]
pub struct Param {
    pub name: &'static str,
    /// Environment variable read when the flag is not given.
    pub env: Option<&'static str>,
    /// Used when neither the flag nor the variable is set.
    pub default: Option<&'static str>,
    pub help: &'static str,
}

/// One provider preset.
#[derive(Serialize, Debug, Clone, Copy)]
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    /// OpenAI-compatible base URL; `{param}` placeholders are filled from
    /// [`Preset::params`].
    pub base_url: &'static str,
    /// Model used when none is given; `None` when the model must be named
    /// (an Azure deployment, a custom endpoint).
    pub default_model: Option<&'static str>,
    /// Variable holding the provider key, sent as `Authorization: Bearer`.
    /// `None` for providers that take no bearer key.
    pub key_env: Option<&'static str>,
    /// Whether the provider rejects calls without a key.
    pub key_required: bool,
    pub json_schema: bool,
    /// Extra headers; `{param}` placeholders are filled like the base URL.
    pub headers: &'static [(&'static str, &'static str)],
    pub params: &'static [Param],
    /// Where to get a key.
    pub key_url: Option<&'static str>,
    pub notes: &'static str,
}

const CF_ACCOUNT: Param = Param {
    name: "account_id",
    env: Some("CLOUDFLARE_ACCOUNT_ID"),
    default: None,
    help: "Cloudflare account ID (dashboard sidebar, or `wrangler whoami`)",
};

/// Every preset, in the order onboarding offers them.
pub const PRESETS: &[Preset] = &[
    Preset {
        id: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        default_model: Some("deepseek/deepseek-v4.1-flash"),
        key_env: Some("OPENROUTER_API_KEY"),
        key_required: true,
        // JSON-object mode: some routes (DeepSeek on DigitalOcean) drop the
        // answer in JSON Schema mode and return only reasoning. The engine
        // unwraps the arrays models wrap in an object in this mode.
        json_schema: false,
        headers: &[
            ("http-referer", "https://github.com/rj-s-org/mockingbird"),
            ("x-title", "Mockingbird"),
        ],
        params: &[],
        key_url: Some("https://openrouter.ai/settings/keys"),
        notes: "One key for hundreds of models; name them as vendor/model (openai/gpt-4.1-mini, anthropic/claude-haiku-4.5, meta-llama/llama-3.3-70b-instruct). Models ending in :free cost nothing. Pin or skip upstream providers with --extra-body '{\"provider\": {\"order\": [...]}}'.",
    },
    Preset {
        id: "google-ai-studio",
        name: "Google AI Studio (Gemini API)",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        default_model: Some("gemini-2.5-flash"),
        key_env: Some("GEMINI_API_KEY"),
        key_required: true,
        json_schema: true,
        headers: &[],
        params: &[],
        key_url: Some("https://aistudio.google.com/apikey"),
        notes: "Gemini through its OpenAI-compatible endpoint. The free tier is enough to try Mockingbird.",
    },
    Preset {
        id: "cloudflare-ai-gateway",
        name: "Cloudflare AI Gateway",
        base_url: "https://gateway.ai.cloudflare.com/v1/{account_id}/{gateway_id}/compat",
        default_model: Some("workers-ai/@cf/meta/llama-3.3-70b-instruct-fp8-fast"),
        key_env: None,
        key_required: false,
        json_schema: false,
        headers: &[("cf-aig-authorization", "Bearer ${env:CF_AIG_API_KEY}")],
        params: &[
            CF_ACCOUNT,
            Param {
                name: "gateway_id",
                env: Some("CLOUDFLARE_GATEWAY_ID"),
                default: Some("default"),
                help: "AI Gateway ID; `default` is created on first use",
            },
        ],
        key_url: Some("https://dash.cloudflare.com/?to=/:account/ai/ai-gateway"),
        notes: "Routes through your gateway (logging, caching, rate limits). CF_AIG_API_KEY holds a Cloudflare API token with AI Gateway Run permission, sent as cf-aig-authorization. Name the model as provider/model (workers-ai/@cf/…, openai/…, google-ai-studio/…, anthropic/…). With keys stored in the gateway (BYOK) or Unified Billing nothing else is needed; to pass a provider key through, add --api-key-env.",
    },
    Preset {
        id: "cloudflare-workers-ai",
        name: "Cloudflare Workers AI (AI REST API)",
        base_url: "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1",
        default_model: Some("@cf/meta/llama-3.3-70b-instruct-fp8-fast"),
        key_env: Some("CLOUDFLARE_API_KEY"),
        key_required: true,
        json_schema: false,
        headers: &[],
        params: &[CF_ACCOUNT],
        key_url: Some("https://dash.cloudflare.com/profile/api-tokens"),
        notes: "Cloudflare's OpenAI-compatible REST API: Workers AI models (@cf/…) and, through Unified Billing, third-party models (openai/…, anthropic/…). CLOUDFLARE_API_KEY holds an API token with Workers AI permissions.",
    },
    Preset {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        default_model: Some("gpt-4.1-mini"),
        key_env: Some("OPENAI_API_KEY"),
        key_required: true,
        json_schema: true,
        headers: &[],
        params: &[],
        key_url: Some("https://platform.openai.com/api-keys"),
        notes: "Prefer non-reasoning models: Mockingbird sets temperature and max_tokens.",
    },
    Preset {
        id: "anthropic",
        name: "Anthropic (Claude)",
        base_url: "https://api.anthropic.com/v1",
        default_model: Some("claude-haiku-5-5"),
        key_env: Some("ANTHROPIC_API_KEY"),
        key_required: true,
        json_schema: false,
        headers: &[],
        params: &[],
        key_url: Some("https://console.anthropic.com/settings/keys"),
        notes: "Claude through Anthropic's OpenAI SDK compatibility endpoint.",
    },
    Preset {
        id: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com",
        default_model: Some("deepseek-chat"),
        key_env: Some("DEEPSEEK_API_KEY"),
        key_required: true,
        json_schema: false,
        headers: &[],
        params: &[],
        key_url: Some("https://platform.deepseek.com/api_keys"),
        notes: "The default of `MB_LLM_*` zero-config mode.",
    },
    Preset {
        id: "groq",
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        default_model: Some("llama-3.3-70b-versatile"),
        key_env: Some("GROQ_API_KEY"),
        key_required: true,
        json_schema: false,
        headers: &[],
        params: &[],
        key_url: Some("https://console.groq.com/keys"),
        notes: "Very low latency: responses arrive well inside the LLM timeout.",
    },
    Preset {
        id: "mistral",
        name: "Mistral",
        base_url: "https://api.mistral.ai/v1",
        default_model: Some("mistral-small-latest"),
        key_env: Some("MISTRAL_API_KEY"),
        key_required: true,
        json_schema: false,
        headers: &[],
        params: &[],
        key_url: Some("https://console.mistral.ai/api-keys"),
        notes: "",
    },
    Preset {
        id: "together",
        name: "Together AI",
        base_url: "https://api.together.xyz/v1",
        default_model: Some("meta-llama/Llama-3.3-70B-Instruct-Turbo"),
        key_env: Some("TOGETHER_API_KEY"),
        key_required: true,
        json_schema: false,
        headers: &[],
        params: &[],
        key_url: Some("https://api.together.ai/settings/api-keys"),
        notes: "",
    },
    Preset {
        id: "azure-openai",
        name: "Azure OpenAI",
        base_url: "https://{resource}.openai.azure.com/openai/v1",
        default_model: None,
        key_env: None,
        key_required: false,
        json_schema: true,
        headers: &[("api-key", "${env:AZURE_OPENAI_API_KEY}")],
        params: &[Param {
            name: "resource",
            env: Some("AZURE_OPENAI_RESOURCE"),
            default: None,
            help: "Azure OpenAI resource name (the <resource> of <resource>.openai.azure.com)",
        }],
        key_url: Some("https://portal.azure.com/"),
        notes: "The model is your deployment name. The key is sent as the api-key header.",
    },
    Preset {
        id: "ollama",
        name: "Ollama (local)",
        base_url: "http://127.0.0.1:11434/v1",
        default_model: Some("llama3.2"),
        key_env: None,
        key_required: false,
        json_schema: true,
        headers: &[],
        params: &[],
        key_url: None,
        notes: "No key. Pull the model first (`ollama pull llama3.2`) and raise the timeout (--timeout 30s) on slow machines.",
    },
    Preset {
        id: "lm-studio",
        name: "LM Studio (local)",
        base_url: "http://127.0.0.1:1234/v1",
        default_model: None,
        key_env: None,
        key_required: false,
        json_schema: true,
        headers: &[],
        params: &[],
        key_url: None,
        notes: "No key. Name the loaded model with --model.",
    },
    Preset {
        id: "custom",
        name: "Any OpenAI-compatible endpoint (bring your own key)",
        base_url: "",
        default_model: None,
        key_env: None,
        key_required: false,
        json_schema: false,
        headers: &[],
        params: &[],
        key_url: None,
        notes: "vLLM, LiteLLM, a corporate gateway or any other /chat/completions endpoint: give --base-url and --model, and --api-key-env for a bearer key or --header name='Bearer ${env:NAME}' for other auth.",
    },
];

/// The preset called `id`.
///
/// # Errors
///
/// Fails, listing the presets, when none is called `id`.
pub fn preset(id: &str) -> anyhow::Result<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id).ok_or_else(|| {
        let ids: Vec<_> = PRESETS.iter().map(|p| p.id).collect();
        anyhow!("unknown provider {id:?}; one of: {}", ids.join(", "))
    })
}

/// The answers that turn a preset into a profile. Shared by `init`,
/// `llm configure`, `llm test` and the MCP tools.
#[derive(Args, Debug, Clone, Default)]
pub struct ProviderArgs {
    /// Provider preset (`mockingbird llm providers` lists them).
    #[arg(long = "provider", value_name = "PRESET")]
    pub provider: Option<String>,
    /// Model [default: the preset's].
    #[arg(long)]
    pub model: Option<String>,
    /// Base URL; required for `custom`, overrides the preset's otherwise.
    #[arg(long)]
    pub base_url: Option<String>,
    /// Environment variable holding the provider key, sent as a bearer
    /// token [default: the preset's, e.g. OPENROUTER_API_KEY].
    #[arg(long, value_name = "NAME")]
    pub api_key_env: Option<String>,
    /// Send no bearer key even if the preset has one.
    #[arg(long)]
    pub no_api_key: bool,
    /// Cloudflare account ID [env: CLOUDFLARE_ACCOUNT_ID].
    #[arg(long)]
    pub account_id: Option<String>,
    /// Cloudflare AI Gateway ID [env: CLOUDFLARE_GATEWAY_ID; default: default].
    #[arg(long)]
    pub gateway_id: Option<String>,
    /// Cloudflare AI Gateway with authentication off: send no
    /// `cf-aig-authorization` header.
    #[arg(long)]
    pub no_gateway_token: bool,
    /// Azure OpenAI resource name [env: AZURE_OPENAI_RESOURCE].
    #[arg(long)]
    pub resource: Option<String>,
    /// Extra provider header `name=value` (repeatable); embed secrets as
    /// `${env:NAME}`.
    #[arg(long = "header", value_name = "NAME=VALUE")]
    pub headers: Vec<String>,
    /// Request JSON Schema structured output [default: the preset's].
    #[arg(long)]
    pub json_schema: Option<bool>,
    /// Time before a call falls back to the faker [default: 60s].
    #[arg(long)]
    pub timeout: Option<humantime::Duration>,
    /// JSON object merged into every provider request body, e.g.
    /// OpenRouter routing: '{"provider": {"order": ["atlas-cloud"]}}'.
    #[arg(long, value_name = "JSON")]
    pub extra_body: Option<String>,
}

impl ProviderArgs {
    fn param(&self, p: &Param) -> anyhow::Result<String> {
        let given = match p.name {
            "account_id" => self.account_id.clone(),
            "gateway_id" => self.gateway_id.clone(),
            "resource" => self.resource.clone(),
            _ => None,
        };
        given
            .or_else(|| p.env.and_then(|e| std::env::var(e).ok()))
            .filter(|v| !v.trim().is_empty())
            .or_else(|| p.default.map(String::from))
            .ok_or_else(|| {
                let env = p.env.map(|e| format!(" or set {e}")).unwrap_or_default();
                anyhow!(
                    "the {} preset needs --{}{env}: {}",
                    self.provider.as_deref().unwrap_or("?"),
                    p.name.replace('_', "-"),
                    p.help
                )
            })
    }

    /// The profile these answers describe.
    ///
    /// # Errors
    ///
    /// Fails, naming the flag, when an answer the preset needs is missing or
    /// invalid.
    pub fn profile(&self) -> anyhow::Result<FileProfile> {
        let id = self
            .provider
            .as_deref()
            .ok_or_else(|| anyhow!("--provider is required"))?;
        let preset = preset(id)?;
        let mut params = BTreeMap::new();
        for p in preset.params {
            params.insert(p.name, self.param(p)?);
        }
        let fill = |template: &str| {
            params.iter().fold(template.to_string(), |s, (k, v)| {
                s.replace(&format!("{{{k}}}"), v)
            })
        };
        let base_url = match &self.base_url {
            Some(url) => url.clone(),
            None if preset.base_url.is_empty() => {
                bail!("the {id} preset needs --base-url")
            }
            None => fill(preset.base_url),
        };
        let model = self
            .model
            .clone()
            .or_else(|| preset.default_model.map(String::from))
            .ok_or_else(|| anyhow!("the {id} preset needs --model"))?;
        let mut profile = FileProfile::new(base_url, model);
        profile.json_schema = self.json_schema.unwrap_or(preset.json_schema);
        let key_env = self.api_key_env.as_deref().or(preset.key_env);
        if !self.no_api_key
            && let Some(name) = key_env
        {
            profile.api_key = Some(format!("env:{name}"));
        }
        for (name, value) in preset.headers {
            if self.no_gateway_token && *name == "cf-aig-authorization" {
                continue;
            }
            profile.headers.insert(name.to_string(), fill(value));
        }
        for h in &self.headers {
            let (name, value) = h
                .split_once('=')
                .ok_or_else(|| anyhow!("--header {h:?} is not name=value"))?;
            profile
                .headers
                .insert(name.trim().to_ascii_lowercase(), value.to_string());
        }
        mb_store::validate_profile_headers(&profile.headers)?;
        if let Some(t) = self.timeout {
            profile.timeout = t.into();
        }
        if let Some(raw) = &self.extra_body {
            let parsed: Value =
                serde_json::from_str(raw).map_err(|e| anyhow!("--extra-body is not JSON: {e}"))?;
            let Value::Object(body) = parsed else {
                bail!("--extra-body must be a JSON object");
            };
            mb_store::validate_extra_body(&body)?;
            profile.extra_body = body;
        }
        Ok(profile)
    }
}

/// The Admin API body (`PUT …/llm-profiles/<name>`) for `profile`.
pub fn api_body(profile: &FileProfile, fallback: Option<&str>) -> Value {
    json!({
        "base_url": profile.base_url,
        "model": profile.model,
        "secret_ref": profile.api_key,
        "json_schema": profile.json_schema,
        "temperature": profile.temperature,
        "timeout_ms": u64::try_from(profile.timeout.as_millis()).unwrap_or(u64::MAX),
        "max_tokens": profile.max_tokens,
        "fallback_profile": fallback.or(profile.fallback.as_deref()),
        "headers": profile.headers,
        "extra_body": profile.extra_body,
    })
}

/// The secret references `profile` needs, as variable names (for `env:`)
/// or paths (for `file:`), with whether each resolves here.
pub fn secret_status(profile: &FileProfile) -> Vec<Value> {
    let mut refs: Vec<String> = profile.api_key.iter().cloned().collect();
    for v in profile.headers.values() {
        if let Ok(found) = mb_store::header_secret_refs(v) {
            refs.extend(found.into_iter().map(String::from));
        }
    }
    refs.sort();
    refs.dedup();
    refs.into_iter()
        .map(|r| {
            let set = resolve(&r).is_ok();
            json!({ "ref": r, "set": set })
        })
        .collect()
}

/// Resolves an `env:` or `file:` reference locally.
fn resolve(reference: &str) -> anyhow::Result<String> {
    if let Some(name) = reference.strip_prefix("env:") {
        return std::env::var(name)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| anyhow!("environment variable {name} is not set"));
    }
    if let Some(path) = reference.strip_prefix("file:") {
        mb_ext::check_secret_file(std::path::Path::new(path))?;
        return std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .with_context(|| format!("reading {path}"));
    }
    bail!("{reference:?} is not an env: or file: reference")
}

/// The outcome of [`probe`].
#[derive(Serialize, Debug)]
pub struct ProbeReport {
    pub ok: bool,
    pub base_url: String,
    pub model: String,
    pub latency_ms: u64,
    /// The model's reply, when it answered.
    pub reply: Option<String>,
    /// Whether the reply was the requested JSON.
    pub json_ok: bool,
    pub error: Option<String>,
    pub hint: Option<String>,
}

/// Sends `profile` one small JSON-mode completion, resolving its secret
/// references from this process's environment and files.
///
/// # Errors
///
/// Fails when a secret reference cannot be resolved or the provider client
/// cannot be built; a failed completion is reported in the [`ProbeReport`].
pub async fn probe(profile: &FileProfile) -> anyhow::Result<ProbeReport> {
    let api_key = profile.api_key.as_deref().map(resolve).transpose()?;
    let mut headers = Vec::new();
    for (name, template) in &profile.headers {
        let mut value = template.clone();
        for r in mb_store::header_secret_refs(template)? {
            value = value.replace(&format!("${{{r}}}"), &resolve(r)?);
        }
        headers.push((name.clone(), value));
    }
    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: profile.base_url.clone(),
        api_key,
        model: profile.model.clone(),
        json_schema: profile.json_schema,
        headers,
        extra_body: profile.extra_body.clone(),
        http: HttpOptions::default(),
    })?;
    let timeout = profile.timeout.max(Duration::from_secs(20));
    let request = ChatRequest {
        messages: vec![
            ChatMessage {
                role: Role::System,
                content: "You answer with one JSON object and nothing else.".into(),
            },
            ChatMessage {
                role: Role::User,
                content: r#"Reply with {"ok": true, "service": "mockingbird"}"#.into(),
            },
        ],
        response_format: if profile.json_schema {
            ResponseFormat::JsonSchema {
                name: "probe".into(),
                schema: json!({
                    "type": "object",
                    "properties": {"ok": {"type": "boolean"}, "service": {"type": "string"}},
                    "required": ["ok"]
                }),
            }
        } else {
            ResponseFormat::JsonObject
        },
        temperature: profile.temperature,
        max_tokens: 64,
        timeout,
    };
    let started = Instant::now();
    let result = tokio::time::timeout(timeout, provider.complete(request)).await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (reply, error) = match result {
        Ok(Ok(resp)) => (Some(resp.content), None),
        Ok(Err(e)) => (None, Some(e.to_string())),
        Err(_) => (None, Some(format!("no answer within {timeout:?}"))),
    };
    let json_ok = reply
        .as_deref()
        .map(strip_fences)
        .and_then(|r| serde_json::from_str::<Value>(r).ok())
        .is_some_and(|v| v.get("ok") == Some(&Value::Bool(true)));
    let hint = error.as_deref().map(hint_for).or_else(|| {
        (latency_ms > u64::try_from(profile.timeout.as_millis()).unwrap_or(u64::MAX)).then(|| {
            format!(
                "the reply took {latency_ms} ms, longer than the profile's {} timeout: raise it (--timeout) or pick a faster model",
                humantime::format_duration(profile.timeout)
            )
        })
    });
    Ok(ProbeReport {
        ok: error.is_none() && json_ok,
        base_url: profile.base_url.clone(),
        model: profile.model.clone(),
        latency_ms,
        reply,
        json_ok,
        error,
        hint,
    })
}

fn strip_fences(s: &str) -> &str {
    let t = s.trim();
    t.strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .and_then(|r| r.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(t)
}

fn hint_for(error: &str) -> String {
    let e = error.to_ascii_lowercase();
    if e.contains("http 401") || e.contains("http 403") {
        "the provider refused the key: check the variable holds a valid key with access to this model".into()
    } else if e.contains("http 404") {
        "unknown model or base URL: check --model (and the account or gateway ID)".into()
    } else if e.contains("http 429") {
        "rate limited or out of credit at the provider".into()
    } else if e.contains("http 400") {
        "the provider rejected the request: the model may not support JSON mode or temperature; try --json-schema false or another model".into()
    } else if e.contains("transport") || e.contains("timed out") || e.contains("no answer") {
        "the endpoint did not answer: check the base URL, network access, and that a local runtime is running".into()
    } else {
        "see the error for details".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn key_store_files_readable_by_others_are_not_set() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let store = home.path().join(".mockingbird/secrets");
        std::fs::create_dir_all(&store).unwrap();
        let key = store.join("openrouter");
        std::fs::write(&key, "sk-test").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();
        let mut profile = args("openrouter").profile().unwrap();
        profile.api_key = Some(format!("file:{}", key.display()));
        assert_eq!(secret_status(&profile)[0]["set"], false);
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(secret_status(&profile)[0]["set"], true);
    }

    fn args(provider: &str) -> ProviderArgs {
        ProviderArgs {
            provider: Some(provider.into()),
            ..ProviderArgs::default()
        }
    }

    #[test]
    fn presets_are_unique_and_valid() {
        let mut ids: Vec<_> = PRESETS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), PRESETS.len());
        for p in PRESETS {
            let headers = p
                .headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            mb_store::validate_profile_headers(&headers)
                .unwrap_or_else(|e| panic!("{}: {e}", p.id));
            if let Some(env) = p.key_env {
                assert!(env.ends_with("_API_KEY"), "{}: {env}", p.id);
            }
        }
    }

    #[test]
    fn openrouter_profile() {
        let p = args("openrouter").profile().unwrap();
        assert_eq!(p.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(p.api_key.as_deref(), Some("env:OPENROUTER_API_KEY"));
        assert_eq!(p.headers["x-title"], "Mockingbird");
        // Field trials: JSON Schema output (JSON-object mode makes models
        // wrap top-level arrays) and a timeout a remote model can meet.
        // JSON Schema mode loses the answer on some OpenRouter routes.
        assert!(!p.json_schema);
        assert_eq!(p.model, "deepseek/deepseek-v4.1-flash");
        assert_eq!(p.timeout, std::time::Duration::from_secs(60));
        assert_eq!(p.max_tokens, 16_384);
    }

    #[test]
    fn extra_body_flag() {
        let p = ProviderArgs {
            extra_body: Some(r#"{"provider": {"order": ["atlas-cloud"]}}"#.into()),
            ..args("openrouter")
        }
        .profile()
        .unwrap();
        assert_eq!(p.extra_body["provider"]["order"][0], "atlas-cloud");
        for bad in [r#"{"model": "x"}"#, "[1]", "not json"] {
            let r = ProviderArgs {
                extra_body: Some(bad.into()),
                ..args("openrouter")
            }
            .profile();
            assert!(r.is_err(), "{bad}");
        }
    }

    #[test]
    fn cloudflare_gateway_profile() {
        let p = ProviderArgs {
            account_id: Some("acc".into()),
            ..args("cloudflare-ai-gateway")
        }
        .profile()
        .unwrap();
        assert_eq!(
            p.base_url,
            "https://gateway.ai.cloudflare.com/v1/acc/default/compat"
        );
        assert_eq!(p.api_key, None);
        assert_eq!(
            p.headers["cf-aig-authorization"],
            "Bearer ${env:CF_AIG_API_KEY}"
        );
        let byok = ProviderArgs {
            account_id: Some("acc".into()),
            gateway_id: Some("gw".into()),
            api_key_env: Some("OPENAI_API_KEY".into()),
            model: Some("openai/gpt-4.1-mini".into()),
            ..args("cloudflare-ai-gateway")
        }
        .profile()
        .unwrap();
        assert!(byok.base_url.contains("/acc/gw/compat"));
        assert_eq!(byok.api_key.as_deref(), Some("env:OPENAI_API_KEY"));
    }

    #[test]
    fn custom_needs_base_url_and_model() {
        assert!(args("custom").profile().is_err());
        let p = ProviderArgs {
            base_url: Some("http://vllm:8000/v1".into()),
            model: Some("qwen".into()),
            api_key_env: Some("VLLM_API_KEY".into()),
            headers: vec!["X-Tenant=mocks".into()],
            ..args("custom")
        }
        .profile()
        .unwrap();
        assert_eq!(p.api_key.as_deref(), Some("env:VLLM_API_KEY"));
        assert_eq!(p.headers["x-tenant"], "mocks");
        let raw = ProviderArgs {
            headers: vec!["x-api-key=sk-raw".into()],
            ..args("openai")
        };
        assert!(raw.profile().is_err(), "raw credential header");
    }

    #[test]
    fn unknown_preset() {
        assert!(args("nope").profile().is_err());
    }
}
