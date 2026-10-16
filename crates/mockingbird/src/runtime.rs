//! Builds a [`MockRuntime`] for a stored mock: its active contract, LLM
//! provider chain, engine settings and scenarios.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use arc_swap::ArcSwapOption;
use mb_engine::{Engine, EngineConfig, ScenarioSpec, StreamConfig};
use mb_ext::{Extensions, Tenancy};
use mb_llm::{FallbackProvider, HopSettings, OpenAiCompatible, OpenAiConfig, Provider};
use mb_protocols::{
    DataPlaneAuth, DataPlaneAuthMode, DataPlaneKeys, MockRuntime, ProtocolSettings,
    RequestRecorder, ScenarioRuntime, Shutdown,
};
use mb_state::StateStore;
use mb_store::{
    ControlStore, DEFAULT_WORKSPACE, DataPlaneAuthConfig, DataPlaneAuthMode as StoredAuthMode,
    LlmProfile, Mock, Scenario,
};

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

/// The runtime serving `mock`'s active contract, or `None` when it has none.
pub async fn build_runtime(deps: &RuntimeDeps, mock: &Mock) -> anyhow::Result<Option<MockRuntime>> {
    let Some(contract) = deps
        .store
        .active_contract(mock.id)
        .await
        .with_context(|| format!("loading the active contract of mock {:?}", mock.name))?
    else {
        return Ok(None);
    };

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
            Some(chain_provider(deps, &chain).await?)
        }
        _ => None,
    };

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
        Arc::new(Engine::new(provider, config)),
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
    rt.shutdown = Shutdown::new(STOP.subscribe());
    Ok(Some(rt))
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
    if mock.llm_profile.is_some() || mock.workspace == DEFAULT_WORKSPACE {
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
    chain_provider(deps, &chain).await
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

/// The provider for a loaded chain: each profile falls back to the next.
async fn chain_provider(
    deps: &RuntimeDeps,
    chain: &[LlmProfile],
) -> anyhow::Result<Arc<dyn Provider>> {
    // Built from the last hop backwards: (provider, its profile's settings).
    let mut fallback: Option<(Arc<dyn Provider>, HopSettings)> = None;
    for profile in chain.iter().rev() {
        let this = openai_provider(deps, profile).await?;
        let settings = hop_settings(profile);
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
    let tenancy = Tenancy {
        tenant: "default".into(),
        workspace: mock.workspace.clone(),
    };
    let default_profile = deps.default_profile.get();
    let configured = mock.llm_profile.as_deref().or(default_profile.as_deref());
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
    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: profile.base_url.clone(),
        api_key,
        model: profile.model.clone(),
        json_schema: profile.json_schema,
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

fn scenario_runtime(s: Scenario) -> ScenarioRuntime {
    ScenarioRuntime {
        spec: ScenarioSpec {
            name: s.name,
            instructions: s.instructions,
            set: s.rules.set,
        },
        status: s.rules.status,
        latency: s.rules.latency_ms.map(Duration::from_millis),
        operations: s.rules.operations,
        stream_items: s.rules.stream_items.and_then(|n| usize::try_from(n).ok()),
        stream_item_interval: s.rules.stream_item_interval_ms.map(Duration::from_millis),
    }
}
