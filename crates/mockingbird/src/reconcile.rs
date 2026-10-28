//! Applies a [`FileConfig`] to the control store: file-declared profiles,
//! mocks, contracts, scenarios, channel settings and event settings are
//! created, updated or deleted to match the file, and the workspaces they
//! name are created. API-owned resources and workspaces are never deleted.
//! A file mock's `chaos` and `limits` (and its scenarios' `chaos`) replace
//! whatever the API set, like every other file-managed field.
//!
//! Mocks and profiles are identified by `(workspace, name)`; reports label
//! them with [`mock_label`] (`name` in `default`, else `workspace/name`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, anyhow, bail};
use chrono::Utc;
use mb_mcm::{Bundle, Trigger};
use mb_state::StateStore;
use mb_store::{
    ChannelSettings, ContractStatus, ControlStore, DEFAULT_WORKSPACE, LlmProfile, Mock,
    MockEventSettings, NewContractVersion, NewMock, ResourceSource, Scenario, ScenarioRules,
    routes_collide, validate_header_name, validate_mock_name, validate_route_prefix,
    validate_workspace_name,
};
use sha2::{Digest, Sha256};

use crate::file_config::{FileConfig, FileMock, FileProfile};
use crate::registry::mock_label;

/// `(workspace, name)` of a mock or profile.
type Key = (String, String);

/// What a [`reconcile`] changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReconcileReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub deleted: Vec<String>,
    /// `(mock, version)` of every contract version added and activated.
    pub new_versions: Vec<(String, i64)>,
    /// Problems the change may cause but that did not stop it (also logged
    /// at warn level), e.g. API mocks left using a deleted file profile.
    pub warnings: Vec<String>,
}

impl ReconcileReport {
    pub fn is_empty(&self) -> bool {
        self.created.is_empty()
            && self.updated.is_empty()
            && self.deleted.is_empty()
            && self.new_versions.is_empty()
    }

    /// `"{added} added, {removed} removed, {changed} changed"`, counting
    /// mocks: `changed` is the distinct mocks updated or given a new
    /// contract version that were not created.
    pub fn summary(&self) -> String {
        let changed: std::collections::BTreeSet<&str> = self
            .updated
            .iter()
            .map(String::as_str)
            .chain(self.new_versions.iter().map(|(m, _)| m.as_str()))
            .filter(|m| !self.created.iter().any(|c| c == m))
            .collect();
        format!(
            "{} added, {} removed, {} changed",
            self.created.len(),
            self.deleted.len(),
            changed.len()
        )
    }
}

/// A file mock with its contract read and compiled.
struct Prepared<'a> {
    workspace: &'a str,
    name: &'a str,
    mock: &'a FileMock,
    prefix: String,
    source: Vec<u8>,
    digest: String,
    bundle: Bundle,
    /// The `channels:` block, validated: channel id → (enabled, triggers).
    channels: BTreeMap<String, FileChannelSettings>,
}

/// A validated `channels:` entry: `(enabled, triggers)`.
type FileChannelSettings = (bool, Option<Vec<Trigger>>);

/// Makes the store match `cfg`. Deleting a file mock also resets its
/// session state in `state`.
///
/// The whole config is validated first (workspace names, names, route
/// prefixes and gRPC services across every workspace, data-plane auth
/// headers, profile references, secret references, contracts, scenario
/// operations and statuses, brokers, channel ids and triggers, and clashes with
/// API-owned resources, route prefixes and active gRPC services included);
/// any error
/// returns `Err` with nothing applied.
pub async fn reconcile(
    store: &dyn ControlStore,
    state: &dyn StateStore,
    cfg: &FileConfig,
) -> anyhow::Result<ReconcileReport> {
    let existing_workspaces: BTreeSet<String> = store
        .list_workspaces()
        .await
        .context("listing workspaces")?
        .into_iter()
        .map(|w| w.name)
        .collect();
    let existing_mocks: HashMap<Key, Mock> = store
        .list_all_mocks()
        .await
        .context("listing mocks")?
        .into_iter()
        .map(|m| ((m.workspace.clone(), m.name.clone()), m))
        .collect();
    let mut existing_profiles: HashMap<Key, LlmProfile> = HashMap::new();
    for ws in &existing_workspaces {
        for p in store
            .list_profiles(ws)
            .await
            .with_context(|| format!("listing the LLM profiles of workspace {ws:?}"))?
        {
            existing_profiles.insert((p.workspace.clone(), p.name.clone()), p);
        }
    }

    let prepared = validate(store, cfg, &existing_mocks, &existing_profiles).await?;

    let mut report = ReconcileReport::default();

    // Workspaces first, so every mock and profile has its workspace.
    let wanted: BTreeSet<&str> = cfg
        .mocks
        .values()
        .map(|m| m.workspace.as_str())
        .chain(cfg.llm.profiles.values().map(|p| p.workspace.as_str()))
        .collect();
    for ws in wanted {
        if !existing_workspaces.contains(ws) {
            store
                .create_workspace(ws, ws)
                .await
                .with_context(|| format!("creating workspace {ws:?}"))?;
        }
    }

    // Profiles next, so mocks never reference a missing one.
    for (name, fp) in &cfg.llm.profiles {
        let profile = to_profile(name, fp)?;
        if existing_profiles.get(&(fp.workspace.clone(), name.clone())) != Some(&profile) {
            store
                .put_profile(&profile)
                .await
                .with_context(|| format!("saving LLM profile {name:?}"))?;
        }
    }

    for p in &prepared {
        let key = (p.workspace.to_string(), p.name.to_string());
        apply_mock(store, p, existing_mocks.get(&key), &mut report).await?;
    }

    let declared_mocks: HashSet<Key> = cfg
        .mocks
        .iter()
        .map(|(name, m)| (m.workspace.clone(), name.clone()))
        .collect();
    for (key, mock) in &existing_mocks {
        if mock.source == ResourceSource::File && !declared_mocks.contains(key) {
            let (ws, name) = key;
            let label = mock_label(ws, name);
            // Its sessions go first, so a new mock of the same name starts
            // empty and a failed reset leaves the mock for the next reload.
            state
                .reset_mock(ws, name)
                .await
                .map_err(|e| anyhow!("{e}"))
                .with_context(|| format!("resetting the state of mock {label:?}"))?;
            store
                .delete_mock(ws, name)
                .await
                .with_context(|| format!("deleting mock {label:?}"))?;
            report.deleted.push(label);
        }
    }
    report.deleted.sort();

    let declared_profiles: HashSet<Key> = cfg
        .llm
        .profiles
        .iter()
        .map(|(name, p)| (p.workspace.clone(), name.clone()))
        .collect();
    let mut removed: Vec<&Key> = existing_profiles
        .iter()
        .filter(|(key, p)| p.source == ResourceSource::File && !declared_profiles.contains(*key))
        .map(|(key, _)| key)
        .collect();
    removed.sort();
    for (ws, name) in removed {
        store
            .delete_profile(ws, name)
            .await
            .with_context(|| format!("deleting LLM profile {:?}", mock_label(ws, name)))?;
        // Mocks the API owns are not in the file, so nothing above stopped
        // them from using a profile the file no longer declares.
        let mut users: Vec<&str> = existing_mocks
            .values()
            .filter(|m| m.source == ResourceSource::Api && m.workspace == *ws)
            .filter(|m| uses_profile(m, name, &existing_profiles))
            .map(|m| m.name.as_str())
            .collect();
        users.sort_unstable();
        if !users.is_empty() {
            let warning = format!(
                "LLM profile {:?} was removed from the config file but API mocks still use it: {}",
                mock_label(ws, name),
                users.join(", ")
            );
            tracing::warn!("{warning}");
            report.warnings.push(warning);
        }
    }
    Ok(report)
}

/// Whether `mock`'s profile is `profile` or falls back to it (profiles of
/// the mock's workspace).
fn uses_profile(mock: &Mock, profile: &str, profiles: &HashMap<Key, LlmProfile>) -> bool {
    let mut seen = HashSet::new();
    let mut next = mock.llm_profile.as_deref();
    while let Some(name) = next {
        if name == profile {
            return true;
        }
        if !seen.insert(name) {
            return false;
        }
        next = profiles
            .get(&(mock.workspace.clone(), name.to_string()))
            .and_then(|p| p.fallback_profile.as_deref());
    }
    false
}

async fn validate<'a>(
    store: &dyn ControlStore,
    cfg: &'a FileConfig,
    existing_mocks: &HashMap<Key, Mock>,
    existing_profiles: &HashMap<Key, LlmProfile>,
) -> anyhow::Result<Vec<Prepared<'a>>> {
    for ws in cfg
        .mocks
        .values()
        .map(|m| &m.workspace)
        .chain(cfg.llm.profiles.values().map(|p| &p.workspace))
    {
        validate_workspace_name(ws).map_err(|e| anyhow!("{e}"))?;
    }

    // Profiles a file mock or profile in `ws` may use: the file's own in
    // `ws` and API-owned ones there.
    let known_profile = |ws: &str, name: &str| {
        cfg.llm
            .profiles
            .get(name)
            .is_some_and(|p| p.workspace == ws)
            || existing_profiles
                .get(&(ws.to_string(), name.to_string()))
                .is_some_and(|p| p.source == ResourceSource::Api)
    };

    for (name, p) in &cfg.llm.profiles {
        let label = mock_label(&p.workspace, name);
        if !valid_identifier(name) {
            bail!("LLM profile name {name:?} must be 1-128 characters of [A-Za-z0-9._-]");
        }
        if existing_profiles
            .get(&(p.workspace.clone(), name.clone()))
            .is_some_and(|e| e.source == ResourceSource::Api)
        {
            bail!("LLM profile {label:?} is managed through the API; rename it in the config file");
        }
        if let Some(reference) = &p.api_key
            && !valid_secret_ref(reference)
        {
            bail!(
                "LLM profile {label:?}: api_key must be a secret reference (env:NAME or file:/path), not a key"
            );
        }
        mb_store::validate_profile_headers(&p.headers)
            .map_err(|e| anyhow!("LLM profile {label:?}: headers: {e}"))?;
        mb_store::validate_extra_body(&p.extra_body)
            .map_err(|e| anyhow!("LLM profile {label:?}: {e}"))?;
        for value in p.headers.values() {
            for reference in mb_store::header_secret_refs(value)? {
                if !valid_secret_ref(reference) {
                    bail!(
                        "LLM profile {label:?}: headers: ${{{reference}}} must be a secret reference (env:NAME or file:/path)"
                    );
                }
            }
        }
        if let Some(fallback) = &p.fallback
            && !known_profile(&p.workspace, fallback)
        {
            bail!(
                "LLM profile {label:?}: fallback profile {fallback:?} does not exist in workspace {:?}",
                p.workspace
            );
        }
        to_profile(name, p)?;
    }
    if let Some(name) = &cfg.llm.default_profile
        && !known_profile(DEFAULT_WORKSPACE, name)
    {
        bail!("default LLM profile {name:?} does not exist");
    }

    // Routes (a prefix on a host set) are unique across every workspace.
    let mut routes: Vec<(String, Vec<String>, String)> = Vec::new();
    let mut prepared = Vec::with_capacity(cfg.mocks.len());
    for (name, mock) in &cfg.mocks {
        validate_mock_name(name).map_err(|e| anyhow!("{e}"))?;
        let ws = mock.workspace.as_str();
        let label = mock_label(ws, name);
        let existing = existing_mocks.get(&(ws.to_string(), name.clone()));
        if existing.is_some_and(|m| m.source == ResourceSource::Api) {
            bail!("mock {label:?} is managed through the API; rename it in the config file");
        }
        let prefix = mock.effective_prefix(name);
        validate_route_prefix(&prefix).map_err(|e| anyhow!("mock {label:?}: {e}"))?;
        if let Some((_, _, other)) = routes
            .iter()
            .find(|(p, hosts, _)| routes_collide(p, hosts, &prefix, &mock.hosts))
        {
            bail!("mocks {other:?} and {label:?} share the route prefix {prefix:?}");
        }
        routes.push((prefix.clone(), mock.hosts.clone(), label.clone()));
        if let Some(profile) = &mock.llm_profile
            && !known_profile(ws, profile)
        {
            bail!("mock {label:?}: LLM profile {profile:?} does not exist in workspace {ws:?}");
        }
        validate_header_name(&mock.data_plane_auth.header)
            .map_err(|e| anyhow!("mock {label:?}: data_plane_auth header: {e}"))?;

        // File reads and compilation block: keep them off the runtime.
        let path = mock.contract.clone();
        let (source, bundle) = tokio::task::spawn_blocking(move || {
            // A directory is packed as a deterministic tar.gz, so its
            // digest is stable across reconciles.
            let source = mb_import::read_source(&path)
                .with_context(|| format!("cannot read contract {}", path.display()))?;
            let bundle = mb_import::compile(&source)
                .with_context(|| format!("cannot compile contract {}", path.display()))?;
            anyhow::Ok((source, bundle))
        })
        .await
        .context("reading the contract did not finish")?
        .with_context(|| format!("mock {label:?}"))?;
        let operations: HashSet<&str> = bundle.operations.iter().map(|o| o.id.as_str()).collect();

        let api_scenarios: HashSet<String> = match existing {
            Some(m) => store
                .list_scenarios(m.id)
                .await
                .with_context(|| format!("listing the scenarios of mock {label:?}"))?
                .into_iter()
                .filter(|s| s.source == ResourceSource::Api)
                .map(|s| s.name)
                .collect(),
            None => HashSet::new(),
        };
        for (sname, s) in &mock.scenarios {
            if !valid_identifier(sname) {
                bail!(
                    "mock {label:?}: scenario name {sname:?} must be 1-128 characters of [A-Za-z0-9._-]"
                );
            }
            if api_scenarios.contains(sname) {
                bail!("mock {label:?}: scenario {sname:?} is managed through the API");
            }
            if let Some(status) = s.status
                && !(100..=599).contains(&status)
            {
                bail!("mock {label:?}, scenario {sname:?}: status {status} is not 100-599");
            }
            if let Some(set) = &s.set
                && !set.is_object()
            {
                bail!("mock {label:?}, scenario {sname:?}: set must be a mapping");
            }
            let ids = s.operation_ids();
            if let Some(op) = ids.iter().find(|op| !operations.contains(op.as_str())) {
                bail!(
                    "mock {label:?}, scenario {sname:?}: operation {op:?} is not in the contract"
                );
            }
            if let Some(status) = s.status
                && let Some(why) = mb_admin::undeclared_scenario_status(&bundle, &ids, status)
            {
                bail!("mock {label:?}, scenario {sname:?}: {why}");
            }
            if let Some(items) = s.stream_items
                && !(1..=10_000).contains(&items)
            {
                bail!("mock {label:?}, scenario {sname:?}: stream_items {items} is not 1-10000");
            }
            duration_ms(&label, sname, "latency", s.latency)?;
            duration_ms(
                &label,
                sname,
                "stream_item_interval",
                s.stream_item_interval,
            )?;
        }

        let channels = validate_channels(cfg, name, mock, &bundle)?;
        prepared.push(Prepared {
            workspace: ws,
            name,
            mock,
            prefix,
            digest: sha256_hex(&source),
            source,
            bundle,
            channels,
        });
    }
    // gRPC services are unique across every workspace (one shared
    // listener): among the file mocks, and against the active contracts of
    // the mocks the API owns.
    let mut grpc: BTreeMap<String, String> = BTreeMap::new();
    for p in &prepared {
        let label = mock_label(p.workspace, p.name);
        for service in p.bundle.grpc_services() {
            if let Some(other) = grpc.insert(service.clone(), label.clone()) {
                bail!("mocks {other:?} and {label:?} both serve the gRPC service {service:?}");
            }
        }
    }
    if !grpc.is_empty() {
        for api_mock in existing_mocks
            .values()
            .filter(|m| m.source == ResourceSource::Api)
        {
            let api_label = mock_label(&api_mock.workspace, &api_mock.name);
            let Some(active) = store
                .active_contract(api_mock.id)
                .await
                .with_context(|| format!("loading the active contract of mock {api_label:?}"))?
            else {
                continue;
            };
            for service in active.bundle.grpc_services() {
                if let Some(label) = grpc.get(&service) {
                    bail!(
                        "mock {label:?}: gRPC service {service:?} is already served by mock {api_label:?}, which is managed through the API"
                    );
                }
            }
        }
    }
    // A file mock may not take the prefix of a mock the API owns, in any
    // workspace.
    for api_mock in existing_mocks
        .values()
        .filter(|m| m.source == ResourceSource::Api)
    {
        if let Some((_, _, label)) = routes
            .iter()
            .find(|(p, hosts, _)| routes_collide(p, hosts, &api_mock.route_prefix, &api_mock.hosts))
        {
            bail!(
                "mock {label:?}: route prefix {:?} is already used by mock {:?}, which is managed through the API",
                api_mock.route_prefix,
                mock_label(&api_mock.workspace, &api_mock.name)
            );
        }
    }
    Ok(prepared)
}

/// Checks a file mock's `broker` and `channels` against the configured
/// brokers and its compiled contract.
fn validate_channels(
    cfg: &FileConfig,
    name: &str,
    mock: &FileMock,
    bundle: &Bundle,
) -> anyhow::Result<BTreeMap<String, FileChannelSettings>> {
    if let Some(broker) = &mock.broker
        && !cfg.brokers.contains_key(broker)
    {
        bail!("mocks.{name}.broker: unknown broker \"{broker}\"");
    }
    let mut channels = BTreeMap::new();
    for (id, fc) in &mock.channels {
        let Some(channel) = bundle.channels.iter().find(|c| &c.id == id) else {
            bail!("mocks.{name}.channels.{id}: no such channel in the contract");
        };
        let triggers = fc
            .triggers
            .as_ref()
            .map(|ts| ts.iter().map(|t| t.to_trigger()).collect::<Vec<_>>());
        if let Some(triggers) = &triggers {
            mb_import::validate_triggers(triggers, channel.direction)
                .map_err(|e| anyhow!("mocks.{name}.channels.{id}.triggers: {e}"))?;
        }
        channels.insert(id.clone(), (fc.enabled, triggers));
    }
    Ok(channels)
}

async fn apply_mock(
    store: &dyn ControlStore,
    p: &Prepared<'_>,
    existing: Option<&Mock>,
    report: &mut ReconcileReport,
) -> anyhow::Result<()> {
    let label = mock_label(p.workspace, p.name);
    let name = label.as_str();
    let mut changed = false;
    let mock = match existing {
        None => {
            let mock = store
                .create_mock(
                    p.workspace,
                    NewMock {
                        name: p.name.to_string(),
                        route_prefix: Some(p.prefix.clone()),
                        persona: p.mock.persona.clone(),
                        llm_profile: p.mock.llm_profile.clone(),
                        header_overrides: p.mock.header_overrides,
                        source: ResourceSource::File,
                        chaos: p.mock.chaos.as_ref().map(|c| c.to_store()),
                        limits: p.mock.limits.to_store(),
                        data_plane_auth: p.mock.data_plane_auth.clone(),
                        behavior: behavior(p.mock),
                        hosts: p.mock.hosts.clone(),
                        webhooks: webhooks(p.mock),
                    },
                )
                .await
                .with_context(|| format!("creating mock {name:?}"))?;
            report.created.push(name.to_string());
            mock
        }
        Some(current) => {
            let wanted = Mock {
                route_prefix: p.prefix.clone(),
                persona: p.mock.persona.clone(),
                llm_profile: p.mock.llm_profile.clone(),
                header_overrides: p.mock.header_overrides,
                data_plane_auth: p.mock.data_plane_auth.clone(),
                chaos: p.mock.chaos.as_ref().map(|c| c.to_store()),
                limits: p.mock.limits.to_store(),
                behavior: behavior(p.mock),
                hosts: p.mock.hosts.clone(),
                webhooks: webhooks(p.mock),
                ..current.clone()
            };
            if wanted != *current {
                changed = true;
                store
                    .update_mock(&wanted)
                    .await
                    .with_context(|| format!("updating mock {name:?}"))?
            } else {
                current.clone()
            }
        }
    };

    let active = store
        .active_contract(mock.id)
        .await
        .with_context(|| format!("loading the active contract of mock {name:?}"))?;
    if active.map(|v| v.source_digest) != Some(p.digest.clone()) {
        let version = store
            .add_contract_version(
                mock.id,
                NewContractVersion {
                    source: p.source.clone(),
                    bundle: p.bundle.clone(),
                    status: ContractStatus::Approved,
                    created_by: Some("file".into()),
                },
            )
            .await
            .with_context(|| format!("adding a contract version to mock {name:?}"))?;
        store
            .activate_contract_version(mock.id, version.version)
            .await
            .with_context(|| {
                format!(
                    "activating contract version {} of mock {name:?}",
                    version.version
                )
            })?;
        report
            .new_versions
            .push((name.to_string(), version.version));
        changed = true;
    }

    let current: HashMap<String, Scenario> = store
        .list_scenarios(mock.id)
        .await
        .with_context(|| format!("listing the scenarios of mock {name:?}"))?
        .into_iter()
        .filter(|s| s.source == ResourceSource::File)
        .map(|s| (s.name.clone(), s))
        .collect();
    for (sname, s) in &p.mock.scenarios {
        let scenario = Scenario {
            mock_id: mock.id,
            name: sname.clone(),
            instructions: s.instructions.clone(),
            rules: ScenarioRules {
                status: s.status,
                latency_ms: duration_ms(name, sname, "latency", s.latency)?,
                set: s.set.clone(),
                operations: s.operation_ids(),
                stream_items: s.stream_items,
                stream_item_interval_ms: duration_ms(
                    name,
                    sname,
                    "stream_item_interval",
                    s.stream_item_interval,
                )?,
                chaos: s.chaos.as_ref().map(|c| c.to_store()),
                operation_instructions: s.operation_instructions(),
            },
            source: ResourceSource::File,
        };
        if current.get(sname) != Some(&scenario) {
            store
                .put_scenario(&scenario)
                .await
                .with_context(|| format!("saving scenario {sname:?} of mock {name:?}"))?;
            changed = true;
        }
    }
    for sname in current.keys() {
        if !p.mock.scenarios.contains_key(sname) {
            store
                .delete_scenario(mock.id, sname)
                .await
                .with_context(|| format!("deleting scenario {sname:?} of mock {name:?}"))?;
            changed = true;
        }
    }

    if apply_channels(store, p, &mock).await? {
        changed = true;
    }

    if changed && existing.is_some() {
        report.updated.push(name.to_string());
    }
    Ok(())
}

/// Makes a file mock's event settings and channel settings match its
/// `broker` and `channels`. Returns whether anything changed.
async fn apply_channels(
    store: &dyn ControlStore,
    p: &Prepared<'_>,
    mock: &Mock,
) -> anyhow::Result<bool> {
    let label = mock_label(p.workspace, p.name);
    let name = label.as_str();
    let mut changed = false;

    let events = store
        .get_event_settings(mock.id)
        .await
        .with_context(|| format!("loading the event settings of mock {name:?}"))?;
    let unchanged = match &events {
        Some(e) => e.broker == p.mock.broker && e.source == ResourceSource::File,
        // No row picks a broker automatically, as an absent `broker` does.
        None => p.mock.broker.is_none(),
    };
    if !unchanged {
        store
            .put_event_settings(&MockEventSettings {
                mock_id: mock.id,
                broker: p.mock.broker.clone(),
                source: ResourceSource::File,
                updated_at: Utc::now(),
            })
            .await
            .with_context(|| format!("saving the event settings of mock {name:?}"))?;
        changed = true;
    }

    let current: HashMap<String, ChannelSettings> = store
        .list_channel_settings(mock.id)
        .await
        .with_context(|| format!("listing the channel settings of mock {name:?}"))?
        .into_iter()
        .map(|s| (s.channel_id.clone(), s))
        .collect();
    for (id, (enabled, triggers)) in &p.channels {
        let same = current.get(id).is_some_and(|s| {
            s.enabled == *enabled && &s.triggers == triggers && s.source == ResourceSource::File
        });
        if same {
            continue;
        }
        store
            .put_channel_settings(&ChannelSettings {
                mock_id: mock.id,
                channel_id: id.clone(),
                enabled: *enabled,
                triggers: triggers.clone(),
                source: ResourceSource::File,
                updated_at: Utc::now(),
            })
            .await
            .with_context(|| format!("saving the settings of channel {id:?} of mock {name:?}"))?;
        changed = true;
    }
    for (id, s) in &current {
        if s.source == ResourceSource::File && !p.channels.contains_key(id) {
            store
                .delete_channel_settings(mock.id, id)
                .await
                .with_context(|| {
                    format!("deleting the settings of channel {id:?} of mock {name:?}")
                })?;
            changed = true;
        }
    }
    Ok(changed)
}

fn to_profile(name: &str, p: &FileProfile) -> anyhow::Result<LlmProfile> {
    Ok(LlmProfile {
        workspace: p.workspace.clone(),
        name: name.to_string(),
        base_url: p.base_url.clone(),
        model: p.model.clone(),
        secret_ref: p.api_key.clone(),
        json_schema: p.json_schema,
        temperature: p.temperature,
        timeout_ms: u64::try_from(p.timeout.as_millis())
            .map_err(|_| anyhow!("LLM profile {name:?}: timeout is too long"))?,
        max_tokens: p.max_tokens,
        fallback_profile: p.fallback.clone(),
        headers: p.headers.clone(),
        extra_body: p.extra_body.clone(),
        source: ResourceSource::File,
    })
}

/// The stored form of a file mock's consistency settings.
fn behavior(mock: &FileMock) -> mb_store::MockBehavior {
    mb_store::MockBehavior {
        consistency: mock.consistency,
        unknown_ids: mock.unknown_ids,
        default_scenario: mock.default_scenario.clone(),
        path_rewrite: mock.path_rewrite.clone(),
        graphql_path: mock.graphql_path.clone(),
    }
}

fn webhooks(mock: &FileMock) -> Vec<mb_store::WebhookTarget> {
    mock.webhooks
        .iter()
        .map(crate::file_config::FileWebhook::to_store)
        .collect()
}

fn duration_ms(
    mock: &str,
    scenario: &str,
    field: &str,
    duration: Option<std::time::Duration>,
) -> anyhow::Result<Option<u64>> {
    duration
        .map(|d| {
            u64::try_from(d.as_millis())
                .map_err(|_| anyhow!("mock {mock:?}, scenario {scenario:?}: {field} is too long"))
        })
        .transpose()
}

/// Scenario and profile names: 1–128 chars of `[A-Za-z0-9._-]`.
fn valid_identifier(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `env:NAME` or `file:/path` with a non-empty name or path.
pub(crate) fn valid_secret_ref(reference: &str) -> bool {
    reference
        .strip_prefix("env:")
        .or_else(|| reference.strip_prefix("file:"))
        .is_some_and(|rest| !rest.is_empty())
}

/// Hex sha256, as the store computes `source_digest`.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
