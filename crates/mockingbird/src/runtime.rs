//! Builds a [`MockRuntime`] for a stored mock: its active contract, LLM
//! provider chain, engine settings and scenarios.

use std::collections::{BTreeSet, HashMap, HashSet};
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
use crate::registry::mock_label;

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

impl std::fmt::Debug for RuntimeDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeDeps")
            .field("ext", &self.ext)
            .field("offline", &self.offline)
            .field("default_profile", &self.default_profile)
            .field("data_plane_keys", &self.data_plane_keys.is_some())
            .field("protocols", &self.protocols)
            .field("stream", &self.stream)
            .field("limiter", &self.limiter)
            .field("scrubber", &self.scrubber.is_some())
            .field("limits", &self.limits)
            .field("http", &self.http)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

/// The profile used by mocks that name none: a shared, swappable cell, so
/// a config reload can change it for every holder at once.
#[derive(Clone, Default)]
pub struct DefaultProfile(Arc<ArcSwapOption<String>>);

impl std::fmt::Debug for DefaultProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DefaultProfile")
            .field(&self.0.load_full())
            .finish()
    }
}

impl DefaultProfile {
    /// A default profile starting as `name`.
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
///
/// # Errors
///
/// Fails when the mock's profile chain cannot be built (see
/// [`build_provider`]) or its guards do not compile.
pub async fn build_engine(deps: &RuntimeDeps, mock: &Mock) -> anyhow::Result<Engine> {
    Ok(build_engine_with_budget(deps, mock, &ProfileUse::default())
        .await?
        .0)
}

/// The LLM profiles a mock runtime generates with: every profile of the
/// chains its engines were built from, the configured one's and those the
/// provider policy picked per call. Shared by the registry (to rebuild only
/// the mocks a profile change affects) and the runtime's [`PolicyEngines`],
/// which adds to it as calls build engines.
#[derive(Clone, Debug, Default)]
pub(crate) struct ProfileUse(Arc<std::sync::Mutex<BTreeSet<String>>>);

impl ProfileUse {
    /// Records the profiles of `chain`.
    fn record(&self, chain: &[LlmProfile]) {
        if let Ok(mut used) = self.0.lock() {
            used.extend(chain.iter().map(|p| p.name.clone()));
        }
    }

    /// Whether an engine was built with profile `name`. A poisoned lock
    /// answers `true`, so the mock is rebuilt rather than kept stale.
    pub(crate) fn uses(&self, name: &str) -> bool {
        self.0
            .lock()
            .map(|used| used.contains(name))
            .unwrap_or(true)
    }
}

/// [`build_engine`], also returning the mock's token budget (when it has
/// one), for `/readyz`.
async fn build_engine_with_budget(
    deps: &RuntimeDeps,
    mock: &Mock,
    used: &ProfileUse,
) -> anyhow::Result<(Engine, Option<Arc<StoreTokenBudget>>)> {
    let engine = build_bare_engine(deps, mock, used).await?;
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

/// The engine of the mock's configured profile, without guards; its
/// profiles are recorded in `used`.
async fn build_bare_engine(
    deps: &RuntimeDeps,
    mock: &Mock,
    used: &ProfileUse,
) -> anyhow::Result<Engine> {
    let profile = configured_profile(deps, mock);
    let workspace = match &profile {
        Some(name) if !deps.offline => Some(profile_workspace(deps, mock, name).await?),
        _ => None,
    };
    bare_engine(deps, mock, profile.zip(workspace), used).await
}

/// `mock`'s engine generating with `profile` (its name and the workspace
/// it is loaded from), or with no LLM for `None` or offline; without
/// guards. The profiles of its chain are recorded in `used`.
async fn bare_engine(
    deps: &RuntimeDeps,
    mock: &Mock,
    profile: Option<(String, String)>,
    used: &ProfileUse,
) -> anyhow::Result<Engine> {
    let mut config = EngineConfig {
        persona: mock.persona.clone(),
        stream: deps.stream.clone(),
        output_budget: deps.limits.output_budget_tokens,
        ..EngineConfig::default()
    };
    let provider = match profile {
        Some((name, workspace)) if !deps.offline => {
            let chain = load_chain(deps, &workspace, &name).await?;
            used.record(&chain);
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
    let provider = crate::snapshot::wrap(&mock_label(&mock.workspace, &mock.name), provider);
    let engine = Engine::new(provider, config);
    Ok(match mock.behavior.faker_seed {
        Some(seed) => engine.with_faker_seed(seed),
        None => engine,
    })
}

/// The provider an import of `mock` extracts with, or `None` when the
/// server is offline or no profile applies. The profile is
/// `profile_override` (from the mock's workspace, else `default`) or the
/// one the mock is served with; each hop's settings are raised to the
/// import minimums ([`IMPORT_MIN_TIMEOUT`], [`IMPORT_MIN_MAX_TOKENS`]) and
/// its temperature capped at 0.2. The provider policy is asked as tenant
/// `default`; [`import_provider_for`] names the tenancy.
///
/// # Errors
///
/// As [`build_provider`], and the provider policy's rejection.
pub async fn import_provider(
    deps: &RuntimeDeps,
    mock: &Mock,
    profile_override: Option<&str>,
) -> anyhow::Result<Option<ExtractLlm>> {
    let tenancy = Tenancy::new(DEFAULT_TENANT, mock.workspace.clone());
    import_provider_for(deps, mock, profile_override, &tenancy).await
}

/// [`import_provider`] for an import billed to `tenancy`: the provider
/// policy chooses the profile for it.
///
/// # Errors
///
/// As [`build_provider`]; a rejection by the provider policy is an error
/// that downcasts to [`mb_ext::Rejection`].
pub async fn import_provider_for(
    deps: &RuntimeDeps,
    mock: &Mock,
    profile_override: Option<&str>,
    tenancy: &Tenancy,
) -> anyhow::Result<Option<ExtractLlm>> {
    if deps.offline {
        return Ok(None);
    }
    // The override is chosen through the provider policy like any other
    // profile; the policy may swap or refuse it.
    let (workspace, name) = match profile_override {
        Some(name) => match choose_profile(deps, mock, Some(name), tenancy)? {
            Some(name) => (own_or_default_workspace(deps, mock, &name).await?, name),
            None => return Ok(None),
        },
        None => match chosen_profile(deps, mock, tenancy).await? {
            Some((name, workspace)) => (workspace, name),
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

/// The engine previews of `mock` generate with for a call of `tenancy`:
/// [`build_engine`] when the provider policy keeps the configured profile,
/// else the same engine with the policy's choice.
///
/// # Errors
///
/// As [`build_engine`]; a rejection by the provider policy is an error that
/// downcasts to [`mb_ext::Rejection`].
pub async fn preview_engine_for(
    deps: &RuntimeDeps,
    mock: &Mock,
    tenancy: &Tenancy,
) -> anyhow::Result<Engine> {
    let configured = configured_profile(deps, mock);
    let chosen = choose_profile(deps, mock, configured.as_deref(), tenancy)?;
    if chosen == configured {
        return build_engine(deps, mock).await;
    }
    let engine = bare_engine(
        deps,
        mock,
        policy_profile(deps, mock, chosen.as_deref()).await?,
        &ProfileUse::default(),
    )
    .await?;
    let (guards, _) = engine_guards(deps, mock);
    Ok(engine.with_guards(guards))
}

/// `profile` with the workspace it is loaded from when the provider policy
/// chose it: the mock's own when it has the profile, else `default`.
/// `None` offline or for no profile.
async fn policy_profile(
    deps: &RuntimeDeps,
    mock: &Mock,
    profile: Option<&str>,
) -> anyhow::Result<Option<(String, String)>> {
    Ok(match profile {
        Some(name) if !deps.offline => Some((
            name.to_string(),
            own_or_default_workspace(deps, mock, name).await?,
        )),
        _ => None,
    })
}

/// The profile (with its workspace) the provider policy picks for a call
/// of `tenancy` on `mock`: loaded as the runtime loads the configured
/// profile when the policy keeps it, else as [`policy_profile`].
async fn chosen_profile(
    deps: &RuntimeDeps,
    mock: &Mock,
    tenancy: &Tenancy,
) -> anyhow::Result<Option<(String, String)>> {
    let configured = configured_profile(deps, mock);
    let chosen = choose_profile(deps, mock, configured.as_deref(), tenancy)?;
    match chosen {
        Some(name) if configured.as_deref() == Some(name.as_str()) => {
            let workspace = profile_workspace(deps, mock, &name).await?;
            Ok(Some((name, workspace)))
        }
        other => policy_profile(deps, mock, other.as_deref()).await,
    }
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
///
/// # Errors
///
/// As [`build_runtime_with_budget`].
pub async fn build_runtime(deps: &RuntimeDeps, mock: &Mock) -> anyhow::Result<Option<MockRuntime>> {
    Ok(build_runtime_with_budget(deps, mock)
        .await?
        .map(|(rt, _)| rt))
}

/// [`build_runtime`], also returning the mock's token budget (when it has
/// one), which lives as long as the runtime's engine.
///
/// # Errors
///
/// Fails when the active contract cannot be read or compiled, or the
/// engine cannot be built (see [`build_engine`]).
pub async fn build_runtime_with_budget(
    deps: &RuntimeDeps,
    mock: &Mock,
) -> anyhow::Result<Option<(MockRuntime, Option<Arc<StoreTokenBudget>>)>> {
    Ok(build_runtime_tracked(deps, mock)
        .await?
        .map(|(rt, budget, _)| (rt, budget)))
}

/// [`build_runtime_with_budget`], also returning the profiles the runtime
/// generates with, which grows as its calls build the engines the provider
/// policy picks.
pub(crate) async fn build_runtime_tracked(
    deps: &RuntimeDeps,
    mock: &Mock,
) -> anyhow::Result<Option<(MockRuntime, Option<Arc<StoreTokenBudget>>, ProfileUse)>> {
    let Some(contract) = deps
        .store
        .active_contract(mock.id)
        .await
        .with_context(|| format!("loading the active contract of mock {:?}", mock.name))?
    else {
        return Ok(None);
    };

    let used = ProfileUse::default();
    let (engine, budget) = build_engine_with_budget(deps, mock, &used).await?;

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
    rt.configured_profile = configured_profile(deps, mock);
    if !deps.offline {
        rt.profile_engines = Some(Arc::new(PolicyEngines {
            deps: deps.clone(),
            mock: mock.clone(),
            guards: rt.engine.guards().clone(),
            used: used.clone(),
        }));
    }
    rt.set_data_plane_auth(data_plane_auth(&mock.data_plane_auth));
    rt.data_plane_keys = deps.data_plane_keys.clone();
    rt.header_overrides = mock.header_overrides;
    rt.scenarios = scenarios;
    rt.ext = deps.ext.clone();
    rt.recorder = deps.recorder.clone();
    rt.protocols = deps.protocols.clone();
    if let Some(path) = &mock.behavior.graphql_path {
        rt.protocols.graphql_path = path.clone();
    }
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
    rt.base_paths = mb_import::base_paths(&contract.source);
    rt.webhooks = webhook_dispatcher(deps, mock).await?;
    Ok(Some((rt, budget, used)))
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
///
/// # Errors
///
/// Fails for a cycle, a chain longer than [`MAX_FALLBACK_DEPTH`] hops, a
/// missing profile or an unresolvable secret.
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

/// The profile `mock` is configured with: its own, else the workspace
/// default. The provider policy is asked about it per call.
fn configured_profile(deps: &RuntimeDeps, mock: &Mock) -> Option<String> {
    mock.llm_profile
        .clone()
        .or_else(|| deps.default_profile.get())
}

/// The tenant admin-side LLM work is attributed to when no tenancy was
/// resolved for it.
const DEFAULT_TENANT: &str = "default";

/// The profile the provider policy picks for a call of `tenancy` on `mock`
/// given `configured`. A rejection is an error that downcasts to
/// [`mb_ext::Rejection`].
fn choose_profile(
    deps: &RuntimeDeps,
    mock: &Mock,
    configured: Option<&str>,
    tenancy: &Tenancy,
) -> anyhow::Result<Option<String>> {
    deps.ext
        .providers
        .choose(tenancy, &mock.name, configured)
        .map_err(|r| {
            anyhow::Error::new(r).context(format!("provider policy rejected mock {:?}", mock.name))
        })
}

/// Builds the engines of the profiles `mock`'s provider policy picks
/// instead of the configured one ([`mb_protocols::ProfileEngines`]): the
/// same persona, stream settings and guards (the token budget included),
/// another provider chain. The profiles it builds with are recorded in
/// `used`, so a change of one rebuilds the runtime and drops its engine.
struct PolicyEngines {
    deps: RuntimeDeps,
    mock: Mock,
    guards: EngineGuards,
    used: ProfileUse,
}

impl std::fmt::Debug for PolicyEngines {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyEngines")
            .field("mock", &self.mock.name)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl mb_protocols::ProfileEngines for PolicyEngines {
    async fn engine(&self, profile: Option<&str>) -> Result<Arc<Engine>, String> {
        let built = async {
            let profile = policy_profile(&self.deps, &self.mock, profile).await?;
            bare_engine(&self.deps, &self.mock, profile, &self.used).await
        }
        .await
        .map_err(|e| format!("{e:#}"))?;
        Ok(Arc::new(built.with_guards(self.guards.clone())))
    }
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
    // A redirect is the receiver's answer: following it would turn the POST
    // into a GET (or re-send the signed body to another host).
    let http = deps
        .http
        .apply(reqwest::Client::builder())
        .and_then(|b| {
            b.redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| mb_llm::LlmError::Transport(e.to_string()))
        })
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
        headers: s.rules.headers.into_iter().collect(),
        per_operation: HashMap::new(),
    }
    .with_operation_instructions(s.rules.operation_instructions)
}
