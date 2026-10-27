//! Builds a [`MockRuntime`] for a stored mock: its active contract, LLM
//! provider chain, engine settings and scenarios.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use arc_swap::ArcSwapOption;
use mb_engine::pii::Scrubber;
use mb_engine::{
    Engine, EngineConfig, EngineGuards, GenerationLimiter, ScenarioSpec, StreamConfig, TokenBudget,
};
use mb_ext::{Extensions, Tenancy};
use mb_import::smart::ExtractLlm;
use mb_llm::{FallbackProvider, HopSettings, OpenAiCompatible, OpenAiConfig, Provider};
use mb_protocols::events::EventDeps;
use mb_protocols::webhooks::deliver::{
    DispatchSettings, ResolvedTarget, SecretBytes, WebhookDispatcher,
};
use mb_protocols::webhooks::log::SharedDeliveryLog;
use mb_protocols::webhooks::sign::Signing;
use mb_protocols::{
    ChaosRuntime, ChaosSpec, DataPlaneAuth, DataPlaneAuthMode, DataPlaneKeys, MockRuntime,
    ProtocolSettings, RateLimiter, RequestRecorder, RuntimeLimits, ScenarioRuntime, Shutdown,
    UnknownIds,
};
use mb_state::StateStore;
use mb_store::{
    ChaosSettings, Consistency, ControlStore, DEFAULT_WORKSPACE, DataPlaneAuthConfig,
    DataPlaneAuthMode as StoredAuthMode, LlmProfile, Mock, Scenario,
    UnknownIds as StoredUnknownIds, WebhookSigning,
};

use crate::budget::StoreTokenBudget;
use crate::file_config::LimitsConfig;

/// Fallback hops followed from a mock's profile before the chain is rejected.
pub const MAX_FALLBACK_DEPTH: usize = 3;

/// The process-wide stop signal every runtime's open-ended streams end on.
static STOP: std::sync::LazyLock<tokio::sync::watch::Sender<bool>> =
    std::sync::LazyLock::new(|| tokio::sync::watch::channel(false).0);

/// Ends every open SSE stream and GraphQL subscription, so a graceful
/// shutdown is not held open by connected clients. Called once the process
/// is asked to stop.
pub fn stop_open_streams() {
    STOP.send_replace(true);
}

/// What the mock runtimes of a node run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServeMode {
    /// Everything (roles `all` and `data`).
    Full,
    /// Role `control`: the runtimes are built for manual publish only;
    /// their channel runtimes are
    /// [`PublishOnly`](mb_protocols::events::ChannelMode::PublishOnly).
    ControlOnly,
}

/// Everything shared by the runtimes of all mocks.
#[derive(Clone)]
pub struct RuntimeDeps {
    pub store: Arc<dyn ControlStore>,
    pub state: Arc<dyn StateStore>,
    pub ext: Extensions,
    pub recorder: Option<Arc<dyn RequestRecorder>>,
    /// Faker only: no LLM provider is built.
    pub offline: bool,
    /// Profile for mocks that name none. Shared with the config watcher,
    /// which swaps it when a reload changes `llm.default_profile`.
    pub default_profile: DefaultProfile,
    /// Verifies data-plane API keys for mocks in `api_key` mode; without it
    /// every key is rejected.
    pub data_plane_keys: Option<Arc<dyn DataPlaneKeys>>,
    /// Protocol knobs (the `protocols:` config section).
    pub protocols: ProtocolSettings,
    /// Stream limits and pacing handed to every engine.
    pub stream: StreamConfig,
    /// Brokers and the scheduler lease every channel runtime uses.
    pub events: EventDeps,
    /// Bounds the LLM generations in flight across every mock
    /// (`limits.max_concurrent_generations`).
    pub limiter: Arc<GenerationLimiter>,
    /// Scrubs PII from LLM output (`pii:`); `None` when disabled.
    pub scrubber: Option<Arc<Scrubber>>,
    /// The global `limits:`, for mocks that set none of their own.
    pub limits: LimitsConfig,
    /// Extra CA roots and the proxy of every LLM provider client
    /// (`outbound:`).
    pub http: mb_llm::HttpOptions,
    /// What the runtimes run (see [`ServeMode`]).
    pub mode: ServeMode,
    /// Every mock's recent webhook deliveries, shared with the Admin API.
    pub webhook_log: SharedDeliveryLog,
}

/// The profile used by mocks that name none: a shared, swappable cell, so
/// a config reload can change it for every holder at once.
#[derive(Clone, Default)]
pub struct DefaultProfile(Arc<ArcSwapOption<String>>);

impl DefaultProfile {
    pub fn new(name: Option<String>) -> Self {
        Self(Arc::new(ArcSwapOption::from(name.map(Arc::new))))
    }

    /// The current default profile name.
    pub fn get(&self) -> Option<String> {
        self.0.load().as_deref().cloned()
    }

    /// Replaces the default profile; returns whether it changed.
    pub fn set(&self, name: Option<String>) -> bool {
        let previous = self.0.swap(name.clone().map(Arc::new));
        previous.as_deref() != name.as_ref()
    }
}

impl From<Option<String>> for DefaultProfile {
    fn from(name: Option<String>) -> Self {
        Self::new(name)
    }
}

/// Lowest per-call timeout of an import's extraction calls; a profile's
/// shorter timeout is raised to it.
pub const IMPORT_MIN_TIMEOUT: Duration = Duration::from_secs(120);

/// Lowest per-call token limit of an import's extraction calls; a profile's
/// smaller limit is raised to it.
pub const IMPORT_MIN_MAX_TOKENS: u32 = 8192;

/// Highest temperature of an import's extraction calls.
const IMPORT_MAX_TEMPERATURE: f32 = 0.2;

/// The engine generating `mock`'s responses: its persona, the stream
/// settings, its guards (see [`engine_guards`]) and, unless offline or no
/// profile applies, the provider chain of its profile with the head
/// profile's timeout, temperature and token limit.
pub async fn build_engine(deps: &RuntimeDeps, mock: &Mock) -> anyhow::Result<Engine> {
    Ok(build_engine_with_budget(deps, mock).await?.0)
}

/// [`build_engine`], also returning the mock's token budget (when it has
/// one), for `/readyz`.
async fn build_engine_with_budget(
    deps: &RuntimeDeps,
    mock: &Mock,
) -> anyhow::Result<(Engine, Option<Arc<StoreTokenBudget>>)> {
    let engine = build_bare_engine(deps, mock).await?;
    let (guards, budget) = engine_guards(deps, mock);
    Ok((engine.with_guards(guards), budget))
}

/// The guards of `mock`'s engine: the shared generation limiter and PII
/// scrubber, a [`StoreTokenBudget`] when the mock has a
/// `daily_token_budget`, and the mock's response size cap, else the
/// global one. Also returns the budget.
pub fn engine_guards(
    deps: &RuntimeDeps,
    mock: &Mock,
) -> (EngineGuards, Option<Arc<StoreTokenBudget>>) {
    let budget = mock
        .limits
        .daily_token_budget
        .map(|limit| StoreTokenBudget::spawn(Arc::clone(&deps.store), mock, limit));
    let max_response_bytes = mock
        .limits
        .max_response_bytes
        .unwrap_or(deps.limits.max_response_bytes);
    let guards = EngineGuards {
        limiter: Some(Arc::clone(&deps.limiter)),
        budget: budget.clone().map(|b| b as Arc<dyn TokenBudget>),
        scrubber: deps.scrubber.clone(),
        max_response_bytes: Some(usize::try_from(max_response_bytes).unwrap_or(usize::MAX)),
    };
    (guards, budget)
}

/// `mock`'s rate limit and request size cap (its own, else the global
/// `limits.max_request_bytes`).
pub fn runtime_limits(deps: &RuntimeDeps, mock: &Mock) -> RuntimeLimits {
    let max_request_bytes = mock
        .limits
        .max_request_bytes
        .unwrap_or(deps.limits.max_request_bytes);
    RuntimeLimits {
        rate: mock
            .limits
            .rate_limit
            .as_ref()
            .map(|r| Arc::new(RateLimiter::new(r.requests_per_second, r.burst))),
        max_request_bytes: usize::try_from(max_request_bytes).unwrap_or(usize::MAX),
    }
}

/// The engine without guards.
async fn build_bare_engine(deps: &RuntimeDeps, mock: &Mock) -> anyhow::Result<Engine> {
    let mut config = EngineConfig {
        persona: mock.persona.clone(),
        stream: deps.stream.clone(),
        ..EngineConfig::default()
    };
    let provider = match profile_name(deps, mock)? {
        Some(name) if !deps.offline => {
            let workspace = profile_workspace(deps, mock, &name).await?;
            let chain = load_chain(deps, &workspace, &name).await?;
            let head = chain
                .first()
                .map(hop_settings)
                .ok_or_else(|| anyhow!("empty LLM profile chain"))?;
            config.timeout = head.timeout;
            config.temperature = head.temperature;
            config.max_tokens = head.max_tokens;
            Some(chain_provider(deps, &chain, hop_settings).await?)
        }
        _ => None,
    };
    Ok(Engine::new(provider, config))
}

/// The provider an import of `mock` extracts with, or `None` when the
/// server is offline or no profile applies. The profile is
/// `profile_override` (from the mock's workspace, else `default`) or the
/// one the mock is served with; each hop's settings are raised to the
/// import minimums ([`IMPORT_MIN_TIMEOUT`], [`IMPORT_MIN_MAX_TOKENS`]) and
/// its temperature capped at 0.2.
pub async fn import_provider(
    deps: &RuntimeDeps,
    mock: &Mock,
    profile_override: Option<&str>,
) -> anyhow::Result<Option<ExtractLlm>> {
    if deps.offline {
        return Ok(None);
    }
    let (workspace, name) = match profile_override {
        // The override is chosen through the provider policy like any
        // other profile; the policy may swap or refuse it.
        Some(name) => match choose_profile(deps, mock, Some(name))? {
            Some(name) => (own_or_default_workspace(deps, mock, &name).await?, name),
            None => return Ok(None),
        },
        None => match profile_name(deps, mock)? {
            Some(name) => (profile_workspace(deps, mock, &name).await?, name),
            None => return Ok(None),
        },
    };
    let chain = load_chain(deps, &workspace, &name).await?;
    let settings = chain
        .first()
        .map(import_hop)
        .ok_or_else(|| anyhow!("empty LLM profile chain"))?;
    let provider = chain_provider(deps, &chain, import_hop).await?;
    Ok(Some(ExtractLlm { provider, settings }))
}

/// The workspace profile `name` is loaded from for `mock` when it may come
/// from `default`: the mock's own when it has the profile, else `default`.
async fn own_or_default_workspace(
    deps: &RuntimeDeps,
    mock: &Mock,
    name: &str,
) -> anyhow::Result<String> {
    if mock.workspace == DEFAULT_WORKSPACE {
        return Ok(mock.workspace.clone());
    }
    let own = deps
        .store
        .get_profile(&mock.workspace, name)
        .await
        .with_context(|| format!("loading LLM profile {name:?}"))?;
    Ok(match own {
        Some(_) => mock.workspace.clone(),
        None => DEFAULT_WORKSPACE.to_string(),
    })
}

/// A profile's settings for import extraction calls.
fn import_hop(profile: &LlmProfile) -> HopSettings {
    let serving = hop_settings(profile);
    HopSettings {
        timeout: serving.timeout.max(IMPORT_MIN_TIMEOUT),
        temperature: serving.temperature.min(IMPORT_MAX_TEMPERATURE),
        max_tokens: serving.max_tokens.max(IMPORT_MIN_MAX_TOKENS),
    }
}

/// The runtime serving `mock`'s active contract, or `None` when it has none.
pub async fn build_runtime(deps: &RuntimeDeps, mock: &Mock) -> anyhow::Result<Option<MockRuntime>> {
    Ok(build_runtime_with_budget(deps, mock)
        .await?
        .map(|(rt, _)| rt))
}

/// [`build_runtime`], also returning the mock's token budget (when it has
/// one), which lives as long as the runtime's engine.
pub async fn build_runtime_with_budget(
    deps: &RuntimeDeps,
    mock: &Mock,
) -> anyhow::Result<Option<(MockRuntime, Option<Arc<StoreTokenBudget>>)>> {
    let Some(contract) = deps
        .store
        .active_contract(mock.id)
        .await
        .with_context(|| format!("loading the active contract of mock {:?}", mock.name))?
    else {
        return Ok(None);
    };

    let (engine, budget) = build_engine_with_budget(deps, mock).await?;

    let scenarios = deps
        .store
        .list_scenarios(mock.id)
        .await
        .with_context(|| format!("loading the scenarios of mock {:?}", mock.name))?
        .into_iter()
        .map(|s| (s.name.clone(), scenario_runtime(s)))
        .collect::<HashMap<_, _>>();

    let mut rt = MockRuntime::new(
        mock.name.clone(),
        Arc::new(contract.bundle),
        Arc::new(engine),
        deps.state.clone(),
    );
    rt.workspace = mock.workspace.clone();
    rt.set_data_plane_auth(data_plane_auth(&mock.data_plane_auth));
    rt.data_plane_keys = deps.data_plane_keys.clone();
    rt.header_overrides = mock.header_overrides;
    rt.scenarios = scenarios;
    rt.ext = deps.ext.clone();
    rt.recorder = deps.recorder.clone();
    rt.protocols = deps.protocols.clone();
    rt.limits = runtime_limits(deps, mock);
    rt.chaos = mock
        .chaos
        .as_ref()
        .filter(|c| !c.is_noop())
        .map(chaos_runtime);
    rt.shutdown = Shutdown::new(STOP.subscribe());
    rt.consistency = mock.behavior.consistency == Consistency::On;
    rt.unknown_ids = match mock.behavior.unknown_ids {
        StoredUnknownIds::Generate => UnknownIds::Generate,
        StoredUnknownIds::NotFound => UnknownIds::NotFound,
    };
    rt.default_scenario = mock.behavior.default_scenario.clone();
    rt.webhooks = webhook_dispatcher(deps, mock).await?;
    Ok(Some((rt, budget)))
}

/// The runtime form of a mock's stored data-plane auth settings.
fn data_plane_auth(config: &DataPlaneAuthConfig) -> DataPlaneAuth {
    DataPlaneAuth {
        mode: match config.mode {
            StoredAuthMode::None => DataPlaneAuthMode::None,
            StoredAuthMode::ApiKey => DataPlaneAuthMode::ApiKey,
            StoredAuthMode::PassthroughValidate => DataPlaneAuthMode::PassthroughValidate,
        },
        header: config.header.clone(),
    }
}

/// The workspace `mock`'s profile `name` is loaded from: the mock's own
/// workspace, except that a default profile (the mock names none) missing
/// there is taken from the `default` workspace.
async fn profile_workspace(deps: &RuntimeDeps, mock: &Mock, name: &str) -> anyhow::Result<String> {
    if mock.llm_profile.is_some() {
        return Ok(mock.workspace.clone());
    }
    own_or_default_workspace(deps, mock, name).await
}

/// The provider for `profile_name` in the `default` workspace, wrapped in
/// [`FallbackProvider`]s along its `fallback_profile` chain, each hop with
/// its own profile's timeout, temperature and token limit. A cycle, a chain
/// longer than [`MAX_FALLBACK_DEPTH`] hops, a missing profile or an
/// unresolvable secret is an error.
pub async fn build_provider(
    deps: &RuntimeDeps,
    profile_name: &str,
) -> anyhow::Result<Arc<dyn Provider>> {
    let chain = load_chain(deps, DEFAULT_WORKSPACE, profile_name).await?;
    chain_provider(deps, &chain, hop_settings).await
}

/// `profile_name` and the profiles it falls back to, in order, all from
/// `workspace`; never empty.
async fn load_chain(
    deps: &RuntimeDeps,
    workspace: &str,
    profile_name: &str,
) -> anyhow::Result<Vec<LlmProfile>> {
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut next = Some(profile_name.to_string());
    while let Some(name) = next {
        if !seen.insert(name.clone()) {
            bail!(
                "LLM profile fallback cycle: {} -> {name}",
                chain_names(&chain)
            );
        }
        if chain.len() > MAX_FALLBACK_DEPTH {
            bail!(
                "LLM profile fallback chain from {profile_name:?} exceeds {MAX_FALLBACK_DEPTH} hops"
            );
        }
        let profile = load_profile(deps, workspace, &name).await?;
        next = profile.fallback_profile.clone().filter(|f| !f.is_empty());
        chain.push(profile);
    }
    if chain.is_empty() {
        bail!("empty LLM profile chain");
    }
    Ok(chain)
}

/// The provider for a loaded chain: each profile falls back to the next,
/// each hop with the settings `settings` gives its profile.
async fn chain_provider(
    deps: &RuntimeDeps,
    chain: &[LlmProfile],
    settings: fn(&LlmProfile) -> HopSettings,
) -> anyhow::Result<Arc<dyn Provider>> {
    // Built from the last hop backwards: (provider, its profile's settings).
    let mut fallback: Option<(Arc<dyn Provider>, HopSettings)> = None;
    for profile in chain.iter().rev() {
        let this = openai_provider(deps, profile).await?;
        let settings = settings(profile);
        let provider = match fallback {
            None => this,
            Some((secondary, secondary_settings)) => Arc::new(
                FallbackProvider::new(this, secondary)
                    .with_primary_settings(settings)
                    .with_secondary_settings(secondary_settings),
            ),
        };
        fallback = Some((provider, settings));
    }
    fallback
        .map(|(provider, _)| provider)
        .ok_or_else(|| anyhow!("empty LLM profile chain"))
}

/// A profile's per-request settings.
fn hop_settings(profile: &LlmProfile) -> HopSettings {
    HopSettings {
        timeout: Duration::from_millis(profile.timeout_ms),
        temperature: profile.temperature,
        max_tokens: profile.max_tokens,
    }
}

/// The profile the provider policy picks for `mock`, if any.
fn profile_name(deps: &RuntimeDeps, mock: &Mock) -> anyhow::Result<Option<String>> {
    let default_profile = deps.default_profile.get();
    let configured = mock.llm_profile.as_deref().or(default_profile.as_deref());
    choose_profile(deps, mock, configured)
}

/// The profile the provider policy picks for `mock` given `configured`.
fn choose_profile(
    deps: &RuntimeDeps,
    mock: &Mock,
    configured: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let tenancy = Tenancy::new("default", mock.workspace.clone());
    deps.ext
        .providers
        .choose(&tenancy, &mock.name, configured)
        .map_err(|r| {
            anyhow!(
                "provider policy rejected mock {:?}: {} ({})",
                mock.name,
                r.title,
                r.detail
            )
        })
}

async fn load_profile(
    deps: &RuntimeDeps,
    workspace: &str,
    name: &str,
) -> anyhow::Result<LlmProfile> {
    deps.store
        .get_profile(workspace, name)
        .await
        .with_context(|| format!("loading LLM profile {name:?} of workspace {workspace:?}"))?
        .ok_or_else(|| anyhow!("LLM profile {name:?} not found in workspace {workspace:?}"))
}

/// The mock's webhook dispatcher; `None` without targets. A secret that
/// does not resolve leaves the mock serving: that target's deliveries are
/// recorded failed, naming the reference.
pub(crate) async fn webhook_dispatcher(
    deps: &RuntimeDeps,
    mock: &Mock,
) -> anyhow::Result<Option<Arc<WebhookDispatcher>>> {
    if mock.webhooks.is_empty() {
        return Ok(None);
    }
    let mut targets = Vec::with_capacity(mock.webhooks.len());
    for t in &mock.webhooks {
        let secret = match &t.secret {
            None => Ok(None),
            Some(reference) => match deps.ext.secrets.resolve(reference).await {
                Ok(value) => Ok(Some(SecretBytes::new(value))),
                Err(_) => {
                    tracing::warn!(
                        mock = %mock.name,
                        reference = %reference,
                        "webhook secret reference could not be resolved; its deliveries will fail"
                    );
                    Err(format!(
                        "secret reference {reference} could not be resolved"
                    ))
                }
            },
        };
        targets.push(ResolvedTarget {
            url: t.url.clone(),
            events: t.events.clone(),
            signing: match t.effective_signing() {
                WebhookSigning::Stripe => Signing::Stripe,
                WebhookSigning::HmacSha256 => Signing::HmacSha256,
                WebhookSigning::None => Signing::None,
            },
            secret,
            delay: Duration::from_millis(t.delay_ms),
        });
    }
    let http = mb_llm::http_client(&deps.http, None)
        .map_err(|e| anyhow!("building the webhook client of mock {:?}: {e}", mock.name))?;
    Ok(Some(Arc::new(WebhookDispatcher::new(
        mock.workspace.clone(),
        mock.name.clone(),
        targets,
        http,
        deps.webhook_log.clone(),
        DispatchSettings::default(),
    ))))
}

async fn openai_provider(
    deps: &RuntimeDeps,
    profile: &LlmProfile,
) -> anyhow::Result<Arc<dyn Provider>> {
    let api_key = match &profile.secret_ref {
        Some(reference) => {
            Some(deps.ext.secrets.resolve(reference).await.with_context(|| {
                format!("resolving the secret of LLM profile {:?}", profile.name)
            })?)
        }
        None => None,
    };
    let mut headers = Vec::with_capacity(profile.headers.len());
    for (name, template) in &profile.headers {
        let mut value = template.clone();
        for reference in mb_store::header_secret_refs(template)? {
            let secret = deps.ext.secrets.resolve(reference).await.with_context(|| {
                format!(
                    "resolving header {name:?} of LLM profile {:?}",
                    profile.name
                )
            })?;
            value = value.replace(&format!("${{{reference}}}"), &secret);
        }
        headers.push((name.clone(), value));
    }
    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: profile.base_url.clone(),
        api_key,
        headers,
        extra_body: profile.extra_body.clone(),
        model: profile.model.clone(),
        json_schema: profile.json_schema,
        http: deps.http.clone(),
    })
    .with_context(|| format!("building the client for LLM profile {:?}", profile.name))?;
    Ok(Arc::new(provider))
}

fn chain_names(chain: &[LlmProfile]) -> String {
    chain
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join(" -> ")
}

/// The runtime form of stored chaos settings, seeded from `MB_CHAOS_SEED`
/// when it is set.
pub fn chaos_runtime(settings: &ChaosSettings) -> Arc<ChaosRuntime> {
    let spec = ChaosSpec {
        error_rate: settings.error_rate,
        latency_jitter: settings.latency_jitter.as_ref().map(|j| {
            (
                Duration::from_millis(j.min_ms),
                Duration::from_millis(j.max_ms),
            )
        }),
        drop_stream_rate: settings.drop_stream_rate,
    };
    Arc::new(ChaosRuntime::new(spec, ChaosRuntime::seed_from_env()))
}

/// A scenario's chaos replaces the mock's even when it injects nothing.
fn scenario_runtime(s: Scenario) -> ScenarioRuntime {
    ScenarioRuntime {
        spec: ScenarioSpec {
            name: s.name,
            instructions: s.instructions,
            set: s.rules.set,
            status: s.rules.status,
        },
        status: s.rules.status,
        latency: s.rules.latency_ms.map(Duration::from_millis),
        operations: s.rules.operations,
        stream_items: s.rules.stream_items.and_then(|n| usize::try_from(n).ok()),
        stream_item_interval: s.rules.stream_item_interval_ms.map(Duration::from_millis),
        chaos: s.rules.chaos.as_ref().map(chaos_runtime),
        per_operation: HashMap::new(),
    }
    .with_operation_instructions(s.rules.operation_instructions)
}
